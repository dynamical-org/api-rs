//! Native runtime adapter. HTTP is injected behind `Fetcher`; parsing, cache and
//! refresh policy remain usable without a native runtime.
use crate::{
    CatalogError, Clock, Collection, MAX_DOCUMENT_BYTES, parse_collection_document, parse_document,
    refresh::{
        CatalogConfig, CatalogState, CatalogStatus, Flight, Snapshot, child_urls, request_target,
    },
};
use futures::{StreamExt, future::BoxFuture};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, watch};

#[derive(Clone)]
pub struct FetchContext {
    clock: Arc<dyn Clock>,
    deadline: f64,
    timeout_seconds: f64,
    cancelled: watch::Receiver<bool>,
}
impl FetchContext {
    pub fn remaining(&self) -> Result<f64, CatalogError> {
        let remaining = self.deadline - self.clock.now();
        if *self.cancelled.borrow() || remaining <= 0. {
            return Err(CatalogError::new(
                "STAC catalog load deadline exceeded or flight cancelled",
            ));
        }
        Ok(remaining)
    }
    pub fn request_timeout(&self) -> Result<f64, CatalogError> {
        Ok(self.timeout_seconds.min(10.).min(self.remaining()?))
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.cancelled.clone();
        if !*receiver.borrow() {
            let _ = receiver.changed().await;
        }
    }
}
#[derive(Clone, Debug)]
pub enum FetchError {
    Status(u16),
    Transport(&'static str),
    Catalog(CatalogError),
}
impl FetchError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Status(500..=599) | Self::Transport(_))
    }
    fn into_catalog(self) -> CatalogError {
        match self {
            Self::Status(_) => CatalogError::typed("STAC HTTP request failed", "HTTPStatusError"),
            Self::Transport(class) => CatalogError::typed("STAC transport failed", class),
            Self::Catalog(e) => e,
        }
    }
}
impl From<CatalogError> for FetchError {
    fn from(value: CatalogError) -> Self {
        Self::Catalog(value)
    }
}
/// Return raw, identity-encoded bytes from one HTTP attempt. Implementations must
/// honor context checks and the body bound; the service also validates the bound.
pub trait Fetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        context: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>>;
}
pub trait RuntimeHooks: Send + Sync {
    fn jitter(&self, low: f64, high: f64) -> f64;
    fn utc_now(&self) -> String;
    fn sleep(&self, seconds: f64) -> BoxFuture<'_, ()>;
}
pub struct NativeHooks;
impl RuntimeHooks for NativeHooks {
    fn jitter(&self, low: f64, high: f64) -> f64 {
        rand::random_range(low..=high)
    }
    fn utc_now(&self) -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
    }
    fn sleep(&self, seconds: f64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(Duration::from_secs_f64(seconds)))
    }
}
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

pub struct NativeFetcher {
    client: reqwest::Client,
    phase_timeout: f64,
}
impl NativeFetcher {
    pub fn new() -> Result<Self, CatalogError> {
        Self::with_timeout(10.)
    }
    pub fn with_timeout(seconds: f64) -> Result<Self, CatalogError> {
        if !seconds.is_finite() || seconds <= 0. {
            return Err(CatalogError::new("Timeouts must be positive"));
        }
        let phase_timeout = seconds.min(10.);
        let client = reqwest::Client::builder()
            .user_agent("dynamical-api/0.1.0")
            // Feature unification must not enable decoding before our raw
            // encoding/body checks, even if another consumer enables codecs.
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .connect_timeout(Duration::from_secs_f64(phase_timeout))
            .read_timeout(Duration::from_secs_f64(phase_timeout))
            // Inspect Location before WHATWG parsing can rewrite its target.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                CatalogError::typed("Could not create STAC HTTP client", "RuntimeError")
            })?;
        Ok(Self {
            client,
            phase_timeout,
        })
    }
}
fn transport_error(error: reqwest::Error) -> FetchError {
    if error.is_redirect() {
        return FetchError::Catalog(CatalogError::typed(
            "STAC redirect limit exceeded",
            "TooManyRedirects",
        ));
    }
    if error.is_builder() {
        if error
            .url()
            .is_some_and(|url| !["http", "https"].contains(&url.scheme()))
        {
            return FetchError::Transport("UnsupportedProtocol");
        }
        return FetchError::Catalog(CatalogError::typed("Invalid STAC URL", "InvalidURL"));
    }
    FetchError::Transport(if error.is_timeout() && error.is_connect() {
        "ConnectTimeout"
    } else if error.is_timeout() {
        "ReadTimeout"
    } else if error.is_connect() {
        "ConnectError"
    } else if error.is_body() {
        "ReadError"
    } else {
        "RemoteProtocolError"
    })
}
impl Fetcher for NativeFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        context: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            // Per-phase limits follow Python's httpx timeout. A combined
            // connect+headers timeout would incorrectly reject two individually
            // legal phases. The request-wide timeout is only the load deadline.
            let timeout = context.timeout_seconds.min(10.);
            let client = if timeout < self.phase_timeout {
                Self::with_timeout(timeout)?.client
            } else {
                self.client.clone()
            };
            let mut current = crate::httpx_url::HttpxUrl::parse(url)?;
            let mut redirects = 0;
            let response = loop {
                let response = client
                    .get(current.request_url()?)
                    .header("Accept-Encoding", "identity")
                    .timeout(Duration::from_secs_f64(context.remaining()?))
                    .send()
                    .await
                    .map_err(transport_error)?;
                context.remaining()?;
                let location = response.headers().get(reqwest::header::LOCATION);
                if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
                    || location.is_none()
                {
                    break response;
                }
                // httpx sends the original request plus at most 20 redirects.
                if redirects == 20 {
                    return Err(CatalogError::typed(
                        "STAC redirect limit exceeded",
                        "TooManyRedirects",
                    )
                    .into());
                }
                // httpx Headers chooses UTF-8 if every header decodes, otherwise
                // Latin-1. Repeated Location fields are joined with comma-space.
                let utf8 = response
                    .headers()
                    .values()
                    .all(|value| std::str::from_utf8(value.as_bytes()).is_ok());
                let location = response
                    .headers()
                    .get_all(reqwest::header::LOCATION)
                    .iter()
                    .map(|value| {
                        if utf8 {
                            std::str::from_utf8(value.as_bytes()).unwrap().to_string()
                        } else {
                            value.as_bytes().iter().map(|b| char::from(*b)).collect()
                        }
                    })
                    .collect::<Vec<String>>()
                    .join(", ");
                current = current.redirect(&location).map_err(|error| {
                    if error.exception_type == "RemoteProtocolError" {
                        FetchError::Transport("RemoteProtocolError")
                    } else {
                        FetchError::Catalog(error)
                    }
                })?;
                redirects += 1;
            };
            if !response.status().is_success() {
                return Err(FetchError::Status(response.status().as_u16()));
            }
            let encodings: Vec<_> = response
                .headers()
                .get_all("content-encoding")
                .iter()
                .collect();
            let compressed = encodings.len() > 1
                || encodings.first().is_some_and(|header| {
                    header.to_str().map_or(true, |value| {
                        let value = value.trim();
                        !value.is_empty() && !value.eq_ignore_ascii_case("identity")
                    })
                });
            if compressed {
                return Err(CatalogError::new(
                    "STAC document was compressed despite identity request",
                )
                .into());
            }
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            loop {
                let chunk = tokio::time::timeout(
                    Duration::from_secs_f64(context.request_timeout()?),
                    stream.next(),
                )
                .await
                .map_err(|_| FetchError::Transport("ReadTimeout"))?;
                context.remaining()?;
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk.map_err(transport_error)?;
                if chunk.len() > MAX_DOCUMENT_BYTES - body.len() {
                    return Err(CatalogError::new("STAC document exceeds maximum body size").into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

struct FlightTask {
    flight: Flight,
    cancel: watch::Sender<bool>,
    done: watch::Sender<bool>,
}
struct Prepared {
    snapshot: Option<Arc<Snapshot>>,
    task: Option<Arc<FlightTask>>,
}
struct State {
    core: CatalogState<Arc<dyn Clock>>,
    task: Option<Arc<FlightTask>>,
}
struct Inner {
    url: String,
    config: CatalogConfig,
    clock: Arc<dyn Clock>,
    fetcher: Arc<dyn Fetcher>,
    hooks: Arc<dyn RuntimeHooks>,
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    workers: Arc<AtomicUsize>,
}
#[derive(Clone)]
pub struct StacCatalog {
    inner: Arc<Inner>,
}
impl StacCatalog {
    pub fn new(url: impl Into<String>, config: CatalogConfig) -> Result<Self, CatalogError> {
        Self::with_dependencies(
            url,
            config,
            Arc::new(MonotonicClock::default()),
            Arc::new(NativeFetcher::with_timeout(config.timeout_seconds)?),
            Arc::new(NativeHooks),
        )
    }
    pub fn with_dependencies(
        url: impl Into<String>,
        config: CatalogConfig,
        clock: Arc<dyn Clock>,
        fetcher: Arc<dyn Fetcher>,
        hooks: Arc<dyn RuntimeHooks>,
    ) -> Result<Self, CatalogError> {
        Ok(Self {
            inner: Arc::new(Inner {
                url: url.into(),
                config,
                clock: clock.clone(),
                fetcher,
                hooks,
                state: Mutex::new(State {
                    core: CatalogState::new(clock, config)?,
                    task: None,
                }),
                slots: Arc::new(Semaphore::new(12)),
                workers: Arc::new(AtomicUsize::new(0)),
            }),
        })
    }
    pub fn status(&self) -> CatalogStatus {
        self.inner
            .state
            .lock()
            .unwrap()
            .core
            .status(self.inner.workers.load(Ordering::SeqCst))
    }
    fn prepare(&self) -> Result<Prepared, CatalogError> {
        let (snapshot, task, start) = {
            let mut state = self.inner.state.lock().unwrap();
            let decision = state.core.decide()?;
            if decision.start {
                let (cancel, _) = watch::channel(false);
                let (done, _) = watch::channel(false);
                state.task = Some(Arc::new(FlightTask {
                    flight: decision.flight.unwrap(),
                    cancel,
                    done,
                }));
            }
            (decision.snapshot, state.task.clone(), decision.start)
        };
        if start {
            let task = task.as_ref().unwrap().clone();
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    let inner = self.inner.clone();
                    runtime.spawn(async move {
                        inner.refresh(task).await;
                    });
                }
                Err(_) => self.inner.finish(
                    &task,
                    Err(CatalogError::typed("No async runtime", "RuntimeError")),
                ),
            }
        }
        Ok(Prepared { snapshot, task })
    }
    /// Best-effort warmup shares exactly the same freshness/backoff/flight state.
    pub fn warm(&self) {
        let _ = self.prepare();
    }
    pub async fn snapshot(&self) -> Result<Arc<Snapshot>, CatalogError> {
        let Prepared { snapshot, task } = self.prepare()?;
        if let Some(snapshot) = snapshot {
            return Ok(snapshot);
        }
        let Some(task) = task else {
            let state = self.inner.state.lock().unwrap();
            return Err(CatalogError::new(
                state
                    .core
                    .last_error()
                    .unwrap_or("STAC catalog refresh is backing off"),
            ));
        };
        let mut done = task.done.subscribe();
        if !*done.borrow() {
            let _ = tokio::time::timeout(
                Duration::from_secs_f64(self.inner.config.cold_wait_seconds),
                done.changed(),
            )
            .await;
        }
        let state = self.inner.state.lock().unwrap();
        if state.core.is_closed() {
            return Err(CatalogError::new("STAC catalog is closed"));
        }
        state.core.snapshot().ok_or_else(|| {
            CatalogError::new(
                state
                    .core
                    .last_error()
                    .unwrap_or("Timed out waiting for STAC catalog"),
            )
        })
    }
    pub async fn list_collections(&self) -> Result<Vec<Collection>, CatalogError> {
        Ok(self
            .snapshot()
            .await?
            .collections
            .values()
            .cloned()
            .collect())
    }
    /// Missing IDs are distinct from catalog failures, just as Python's KeyError.
    pub async fn get_collection(&self, id: &str) -> Result<Option<Collection>, CatalogError> {
        Ok(self.snapshot().await?.collections.get(id).cloned())
    }
    pub fn close(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.core.close();
        if let Some(task) = state.task.take() {
            task.cancel.send_replace(true);
            task.done.send_replace(true);
        }
    }
}
struct WorkerCount(Arc<AtomicUsize>);
impl Drop for WorkerCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
enum Document {
    Root(serde_json::Value),
    Child(Option<Box<Collection>>),
}
impl Inner {
    fn context(&self, task: &FlightTask) -> FetchContext {
        FetchContext {
            clock: self.clock.clone(),
            deadline: task.flight.deadline,
            timeout_seconds: self.config.timeout_seconds,
            cancelled: task.cancel.subscribe(),
        }
    }
    fn submit(
        self: &Arc<Self>,
        url: String,
        task: Arc<FlightTask>,
        child: bool,
    ) -> tokio::task::JoinHandle<Result<Document, CatalogError>> {
        self.workers.fetch_add(1, Ordering::SeqCst);
        let count = WorkerCount(self.workers.clone());
        let inner = self.clone();
        tokio::spawn(async move {
            let _count = count;
            let context = inner.context(&task);
            let permit = tokio::select! {
                p=inner.slots.clone().acquire_owned()=>p.map_err(|_|CatalogError::new("STAC catalog is closed"))?,
                _=context.cancelled()=>return Err(CatalogError::new("STAC catalog load deadline exceeded or flight cancelled")),
            };
            context.remaining()?;
            // httpx would raise InvalidURL here too, before any attempt.
            request_target(&url)?;
            let mut bytes = None;
            for attempt in 0..3 {
                context.remaining()?;
                match inner.fetcher.fetch(&url, context.clone()).await {
                    Ok(body) => {
                        bytes = Some(body);
                        break;
                    }
                    Err(error) if attempt < 2 && error.retryable() => {
                        let seconds = inner.hooks.jitter(0.25, 1.).min(context.remaining()?);
                        inner.hooks.sleep(seconds).await;
                    }
                    Err(error) => return Err(error.into_catalog()),
                }
            }
            context.remaining()?;
            let body = bytes.expect("bounded retry loop returns a body or error");
            // Parsing can be CPU-heavy. Its worker retains the global slot even
            // if the refresh deadline retires this generation meanwhile.
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if child {
                    let collection = match parse_collection_document(&body)? {
                        Ok(collection) => Some(Box::new(collection)),
                        Err(error) => {
                            tracing::warn!(
                                "Skipping invalid STAC collection ({})",
                                error.exception_type
                            );
                            None
                        }
                    };
                    Ok(Document::Child(collection))
                } else {
                    Ok(Document::Root(parse_document(&body)?))
                }
            })
            .await
            .map_err(|_| CatalogError::typed("STAC parser worker failed", "RuntimeError"))?;
            context.remaining()?;
            result
        })
    }
    async fn await_worker(
        &self,
        handle: tokio::task::JoinHandle<Result<Document, CatalogError>>,
        task: &FlightTask,
    ) -> Result<Document, CatalogError> {
        let context = self.context(task);
        tokio::select! {
            result=tokio::time::timeout(Duration::from_secs_f64(context.remaining()?),handle)=>{
                result.map_err(|_|CatalogError::typed("STAC worker deadline exceeded","TimeoutError"))?
                    .map_err(|_|CatalogError::typed("STAC worker failed","RuntimeError"))?
            }
            _=context.cancelled()=>Err(CatalogError::new("STAC catalog load deadline exceeded or flight cancelled")),
        }
    }
    async fn refresh(self: Arc<Self>, task: Arc<FlightTask>) {
        let result = self.load(&task).await;
        self.finish(&task, result);
    }
    async fn load(
        self: &Arc<Self>,
        task: &Arc<FlightTask>,
    ) -> Result<Vec<Collection>, CatalogError> {
        let root = self
            .await_worker(self.submit(self.url.clone(), task.clone(), false), task)
            .await?;
        let Document::Root(root) = root else {
            unreachable!()
        };
        let urls = child_urls(&self.url, &root)?;
        let handles: Vec<_> = urls
            .into_iter()
            .map(|url| self.submit(url, task.clone(), true))
            .collect();
        let had_children = !handles.is_empty();
        let mut collections = vec![];
        for handle in handles {
            if let Document::Child(Some(collection)) = self.await_worker(handle, task).await? {
                collections.push(*collection);
            }
        }
        if had_children && collections.is_empty() {
            return Err(CatalogError::new("No STAC collection is valid"));
        }
        Ok(collections)
    }
    fn finish(&self, task: &FlightTask, result: Result<Vec<Collection>, CatalogError>) {
        // User-supplied wall-clock/RNG callbacks run outside the state lock.
        // Sample jitter only when Python would: a non-initial failed refresh.
        let loaded_at = if result.is_ok() {
            self.hooks.utc_now()
        } else {
            String::new()
        };
        let (result, was_failing) = {
            let mut state = self.state.lock().unwrap();
            let was_failing = state.core.status(0).consecutive_failures > 0;
            (
                result.and_then(|collections| {
                    state.core.publish(task.flight, collections, loaded_at)
                }),
                was_failing,
            )
        };
        match result {
            Err(error) => {
                let bounds = self
                    .state
                    .lock()
                    .unwrap()
                    .core
                    .failure_jitter_bounds(task.flight);
                let delay = bounds.map(|(low, high)| self.hooks.jitter(low, high));
                let warn = self
                    .state
                    .lock()
                    .unwrap()
                    .core
                    .fail(task.flight, &error, |_, _| delay.unwrap_or(5.));
                if warn {
                    tracing::warn!("STAC catalog refresh failed ({})", error.exception_type);
                }
            }
            Ok(()) if was_failing => tracing::info!("STAC catalog refresh recovered"),
            Ok(()) => {}
        }
        let mut state = self.state.lock().unwrap();
        task.cancel.send_replace(true);
        if state.task.as_ref().is_some_and(|f| f.flight == task.flight) {
            state.task = None;
        }
        task.done.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn internationalized_host_aborts_before_get(host: &str, resolved_host: &str) {
        for phase in ["root", "child", "redirect"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let destination = format!("http://{host}:{}/target.json", address.port());
            let calls = Arc::new(std::sync::Mutex::new(vec![]));
            let seen = calls.clone();
            let href = destination.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0; 8192];
                    let n = stream.read(&mut request).await.unwrap();
                    let target = String::from_utf8_lossy(&request[..n])
                        .split(' ')
                        .nth(1)
                        .unwrap()
                        .to_string();
                    seen.lock().unwrap().push(target.clone());
                    let response = if phase == "redirect" && target == "/root.json" {
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {href}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    } else {
                        let body = if phase == "child" && target == "/root.json" {
                            serde_json::json!({"links":[{"rel":"child","href":href}]}).to_string()
                        } else {
                            "{}".into()
                        };
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .resolve(resolved_host, address)
                .build()
                .unwrap();
            let url = if phase == "root" {
                destination
            } else {
                format!("http://{address}/root.json")
            };
            let catalog = StacCatalog::with_dependencies(
                url,
                CatalogConfig::default(),
                Arc::new(MonotonicClock::default()),
                Arc::new(NativeFetcher {
                    client,
                    phase_timeout: 10.,
                }),
                Arc::new(NativeHooks),
            )
            .unwrap();
            assert_eq!(
                catalog.list_collections().await.unwrap_err().to_string(),
                "STAC catalog refresh failed (InvalidURL)",
                "{host} {phase}"
            );
            let expected: &[&str] = if phase == "root" {
                &[]
            } else {
                &["/root.json"]
            };
            assert_eq!(*calls.lock().unwrap(), expected, "{host} {phase}");
            assert_eq!(catalog.status().consecutive_failures, 1);
            catalog.close();
            task.abort();
        }
    }

    #[tokio::test]
    async fn unicode_hosts_abort_roots_children_and_redirects_before_get() {
        internationalized_host_aborts_before_get("é.example.test", "xn--9ca.example.test").await;
    }

    #[tokio::test]
    async fn ace_hosts_abort_roots_children_and_redirects_before_get() {
        for host in ["xn--9ca.example.test", "sub.Xn--9ca.example.test"] {
            internationalized_host_aborts_before_get(host, &host.to_ascii_lowercase()).await;
        }
    }

    #[tokio::test]
    async fn scheme_without_host_redirect_does_not_inherit_the_request_port() {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_address = destination.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::Mutex::new(vec![]));
        let root_calls = calls.clone();
        let root_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let n = stream.read(&mut request).await.unwrap();
            root_calls.lock().unwrap().push(
                String::from_utf8_lossy(&request[..n])
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_string(),
            );
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http:/target.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let dest_calls = calls.clone();
        let dest_task = tokio::spawn(async move {
            let (mut stream, _) = destination.accept().await.unwrap();
            let mut request = [0; 8192];
            let n = stream.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..n])
                    .to_lowercase()
                    .contains("host: catalog.example.test\r\n")
            );
            dest_calls.lock().unwrap().push(
                String::from_utf8_lossy(&request[..n])
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_string(),
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });
        // A portless URL uses the DNS override's ephemeral port. An explicit
        // root port overrides it. Host headers and the independent oracle prove
        // that redirect construction selected HTTP's default port, without needing
        // privileged port 80 or silently skipping this wire test.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("catalog.example.test", destination_address)
            .build()
            .unwrap();
        let catalog = StacCatalog::with_dependencies(
            format!("http://catalog.example.test:{}/root.json", address.port()),
            CatalogConfig::default(),
            Arc::new(MonotonicClock::default()),
            Arc::new(NativeFetcher {
                client,
                phase_timeout: 10.,
            }),
            Arc::new(NativeHooks),
        )
        .unwrap();
        assert!(catalog.list_collections().await.unwrap().is_empty());
        assert_eq!(*calls.lock().unwrap(), ["/root.json", "/target.json"]);
        catalog.close();
        root_task.abort();
        dest_task.abort();
    }
}
