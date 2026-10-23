//! Optional native delivery. Request processing only attempts a bounded enqueue;
//! transport failures, backpressure and shutdown never expose sink credentials.
use super::{Stream, Telemetry};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const QUEUE_CAPACITY: usize = 256;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
use super::shutdown::SDK_SHUTDOWN_RESERVE;
pub use super::shutdown::SHUTDOWN_TIMEOUT;

type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>>;
trait Delivery: Send + Sync {
    fn send(&self, stream: Stream, record: Value) -> SendFuture<'_>;
    fn flush(&self, _budget: Duration) -> SendFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// No Debug implementation: these settings contain sink credentials.
struct Settings {
    posthog: Option<(String, reqwest::Url)>,
    sentry: Option<sentry::types::Dsn>,
}
impl Settings {
    fn from_values(values: &BTreeMap<String, String>) -> Result<Self, &'static str> {
        let posthog = values
            .get("POSTHOG_PROJECT_TOKEN")
            .filter(|v| !v.is_empty())
            .map(|token| {
                let host = values
                    .get("POSTHOG_HOST")
                    .map(String::as_str)
                    .unwrap_or("https://us.i.posthog.com");
                let url = reqwest::Url::parse(&format!("{}/capture/", host.trim_end_matches('/')))
                    .map_err(|_| "Invalid POSTHOG_HOST")?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err("Invalid POSTHOG_HOST");
                }
                Ok((token.clone(), url))
            })
            .transpose()?;
        let sentry = values
            .get("SENTRY_DSN")
            .filter(|v| !v.is_empty())
            .map(|dsn| dsn.parse().map_err(|_| "Invalid SENTRY_DSN"))
            .transpose()?;
        Ok(Self { posthog, sentry })
    }
}

#[derive(Default)]
pub struct DeliveryCounters {
    dropped: AtomicU64,
    failed: AtomicU64,
    pending: AtomicU64,
}
impl DeliveryCounters {
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}
fn increment(counter: &AtomicU64) {
    increment_by(counter, 1);
}
fn increment_by(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(amount))
    });
}
fn fits_record_limit(record: &Value) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("telemetry record exceeds limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_RECORD_BYTES), record).is_ok()
}

pub struct DeliverySink {
    sender: Mutex<Option<mpsc::Sender<(Stream, Value)>>>,
    worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    pub counters: Arc<DeliveryCounters>,
    analytics: bool,
    errors: bool,
    backend: Option<Arc<dyn Delivery>>,
    stdout: Arc<super::StdoutSink>,
}
impl DeliverySink {
    pub fn capture_startup_error(&self) {
        if self.errors {
            self.emit(
                Stream::Error,
                super::error_record(super::ErrorKind::Startup, json!({})).unwrap(),
            );
        }
    }
    pub fn total_shutdown_budget(&self, budget: Duration) -> Duration {
        budget
            + if self.errors {
                SDK_SHUTDOWN_RESERVE
            } else {
                Duration::ZERO
            }
    }
    pub fn from_env() -> Result<Self, &'static str> {
        Self::from_values(&std::env::vars().collect())
    }
    pub fn from_env_with_stdout(stdout: Arc<super::StdoutSink>) -> Result<Self, &'static str> {
        Self::from_values_with_stdout(&std::env::vars().collect(), stdout)
    }
    pub fn from_values(values: &BTreeMap<String, String>) -> Result<Self, &'static str> {
        Self::from_values_with_stdout(values, Arc::new(super::StdoutSink::default()))
    }
    fn from_values_with_stdout(
        values: &BTreeMap<String, String>,
        stdout: Arc<super::StdoutSink>,
    ) -> Result<Self, &'static str> {
        let settings = Settings::from_values(values)?;
        let analytics = settings.posthog.is_some();
        let errors = settings.sentry.is_some();
        if !analytics && !errors {
            return Ok(Self {
                sender: Mutex::new(None),
                worker: tokio::sync::Mutex::new(None),
                counters: Arc::default(),
                analytics,
                errors,
                backend: None,
                stdout,
            });
        }
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .no_proxy();
        #[cfg(feature = "harness")]
        let builder = if values
            .get("DYNAPI_HARNESS_TELEMETRY_STALLED_DNS")
            .map(String::as_str)
            == Some("1")
        {
            builder.dns_resolver(Arc::new(StalledResolver))
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|_| "Cannot initialize telemetry HTTP client")?;
        let sentry = settings.sentry.map(|dsn| {
            let mut options = sentry::ClientOptions::new()
                .default_integrations(false)
                .environment("production")
                .traces_sample_rate(0.0);
            options.dsn = Some(dsn);
            options.transport = Some(Arc::new(HttpTransportFactory(client.clone())));
            Arc::new(sentry::Client::from_config(options))
        });
        Ok(Self::start_with_stdout(
            Arc::new(HttpDelivery {
                client,
                posthog: settings.posthog,
                sentry,
            }),
            QUEUE_CAPACITY,
            analytics,
            errors,
            stdout,
        ))
    }
    #[cfg(test)]
    fn start(backend: Arc<dyn Delivery>, capacity: usize, analytics: bool, errors: bool) -> Self {
        Self::start_with_stdout(
            backend,
            capacity,
            analytics,
            errors,
            Arc::new(super::StdoutSink::default()),
        )
    }
    fn start_with_stdout(
        backend: Arc<dyn Delivery>,
        capacity: usize,
        analytics: bool,
        errors: bool,
        stdout: Arc<super::StdoutSink>,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<(Stream, Value)>(capacity);
        let counters = Arc::new(DeliveryCounters::default());
        let worker_counters = counters.clone();
        let worker_backend = backend.clone();
        let worker = tokio::spawn(async move {
            while let Some((stream, record)) = receiver.recv().await {
                if !matches!(
                    tokio::time::timeout(REQUEST_TIMEOUT, worker_backend.send(stream, record))
                        .await,
                    Ok(Ok(()))
                ) {
                    increment(&worker_counters.failed);
                }
                worker_counters.pending.fetch_sub(1, Ordering::Relaxed);
            }
        });
        Self {
            sender: Mutex::new(Some(sender)),
            worker: tokio::sync::Mutex::new(Some(worker)),
            counters,
            analytics,
            errors,
            backend: Some(backend),
            stdout,
        }
    }
    /// Stop accepting records, drain within `budget`, then close the SDK using
    /// the same deadline plus a four-second reserve for its blocking enqueue/join.
    /// Returns false if SDK cleanup outlives the deadline. The executable must
    /// then exit without waiting for blocking runtime/SDK threads.
    pub async fn shutdown(&self, budget: Duration) -> bool {
        self.shutdown_started_at(tokio::time::Instant::now(), budget)
            .await
    }
    /// Include earlier shutdown work, such as startup error capture, in the
    /// same deadline rather than granting it an additional drain/flush budget.
    pub async fn shutdown_started_at(
        &self,
        started: tokio::time::Instant,
        budget: Duration,
    ) -> bool {
        let reserve = if self.errors {
            SDK_SHUTDOWN_RESERVE
        } else {
            Duration::ZERO
        };
        let drain_deadline = started.checked_add(budget).unwrap_or(started);
        let deadline = drain_deadline
            .checked_add(reserve)
            .unwrap_or(drain_deadline);
        self.shutdown_until(drain_deadline, deadline).await
    }
    /// Both deadlines belong to the process owner. The SDK reserve is already
    /// included in `deadline`; it must never extend the process budget.
    pub async fn shutdown_until(
        &self,
        drain_deadline: tokio::time::Instant,
        deadline: tokio::time::Instant,
    ) -> bool {
        let mut worker_guard = self.worker.lock().await;
        let Some(mut worker) = worker_guard.take() else {
            return true;
        };
        let reserve = if self.errors {
            SDK_SHUTDOWN_RESERVE
        } else {
            Duration::ZERO
        };
        let drain_deadline = drain_deadline.min(deadline);
        self.sender.lock().unwrap().take();
        match tokio::time::timeout_at(drain_deadline, &mut worker).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                increment(&self.counters.failed);
            }
            Err(_) => {
                worker.abort();
                let _ = worker.await;
            }
        }
        increment_by(
            &self.counters.dropped,
            self.counters.pending.swap(0, Ordering::Relaxed),
        );
        if let Some(backend) = &self.backend {
            let flush_budget = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .saturating_sub(reserve);
            match tokio::time::timeout_at(deadline, backend.flush(flush_budget)).await {
                Ok(Ok(())) => {}
                Ok(Err(())) => increment(&self.counters.failed),
                Err(_) => {
                    increment(&self.counters.failed);
                    return false;
                }
            }
        }
        true
    }
}
impl Drop for DeliverySink {
    fn drop(&mut self) {
        self.sender.get_mut().unwrap().take();
        if let Some(worker) = self.worker.get_mut().take() {
            worker.abort();
        }
    }
}
impl Telemetry for DeliverySink {
    fn emit(&self, stream: Stream, mut record: Value) {
        if stream == Stream::Terminal {
            if record["event"] == "api_container" && (self.analytics || self.errors) {
                record["telemetryDropped"] = self.counters.dropped().into();
                record["telemetryFailed"] = self.counters.failed().into();
            }
            self.stdout.emit(stream, record);
            return;
        }
        if (stream == Stream::Analytics && !self.analytics)
            || (stream == Stream::Error && !self.errors)
        {
            return;
        }
        if !fits_record_limit(&record) {
            increment(&self.counters.dropped);
            return;
        }
        let sender = self.sender.lock().unwrap();
        let Some(sender) = sender.as_ref() else {
            increment(&self.counters.dropped);
            return;
        };
        self.counters.pending.fetch_add(1, Ordering::Relaxed);
        if sender.try_send((stream, record)).is_err() {
            self.counters.pending.fetch_sub(1, Ordering::Relaxed);
            increment(&self.counters.dropped);
        }
    }
}

struct HttpDelivery {
    client: reqwest::Client,
    posthog: Option<(String, reqwest::Url)>,
    sentry: Option<Arc<sentry::Client>>,
}
#[cfg(feature = "harness")]
struct StalledResolver;
#[cfg(feature = "harness")]
impl reqwest::dns::Resolve for StalledResolver {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async {
            tokio::task::spawn_blocking(|| {
                eprintln!("Telemetry test resolver stalled");
                loop {
                    std::thread::park();
                }
            })
            .await?;
            Err(std::io::Error::other("stalled resolver returned").into())
        })
    }
}
struct HttpTransportFactory(reqwest::Client);
impl sentry::TransportFactory for HttpTransportFactory {
    fn create_transport_with_options(
        &self,
        options: sentry::TransportOptions,
    ) -> Arc<dyn sentry::Transport> {
        Arc::new(
            sentry::transports::ReqwestHttpTransportOptions::from(options)
                .with_client(self.0.clone())
                .build(),
        )
    }
}
impl Delivery for HttpDelivery {
    fn send(&self, stream: Stream, record: Value) -> SendFuture<'_> {
        Box::pin(async move {
            match stream {
                Stream::Analytics => {
                    let Some((token, url)) = &self.posthog else {
                        return Ok(());
                    };
                    let Some(payload) = posthog_payload(token, &record) else {
                        return Err(());
                    };
                    let response = self
                        .client
                        .post(url.clone())
                        .json(&payload)
                        .send()
                        .await
                        .map_err(|_| ())?;
                    if !response.status().is_success() {
                        return Err(());
                    }
                }
                Stream::Error => {
                    if let (Some(client), Some(event)) = (&self.sentry, sentry_event(&record)) {
                        client.capture_event(event, None);
                    }
                }
                Stream::Terminal => {}
            }
            Ok(())
        })
    }
    fn flush(&self, budget: Duration) -> SendFuture<'_> {
        let client = self.sentry.clone();
        Box::pin(async move {
            if let Some(client) = client {
                tokio::task::spawn_blocking(move || client.close(Some(budget)))
                    .await
                    .map_err(|_| ())?
                    .then_some(())
                    .ok_or(())?;
            }
            Ok(())
        })
    }
}

fn posthog_payload(token: &str, record: &Value) -> Option<Value> {
    if record["event"] != "api_request" {
        return None;
    }
    let distinct = record["distinct_id"].as_str()?;
    if distinct != "anonymous"
        && !(distinct.len() == 16 && distinct.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let properties = record["properties"].as_object()?;
    let mut safe = serde_json::Map::new();
    for key in [
        "$process_person_profile",
        "route",
        "method",
        "status",
        "duration_ms",
        "client_kind",
        "user_agent",
        "country",
        "data_product_ids",
        "query_count",
        "work_units",
        "dataset_cache",
    ] {
        if let Some(value) = properties.get(key) {
            safe.insert(key.to_owned(), value.clone());
        }
    }
    Some(
        json!({"api_key": token, "event": "api_request", "distinct_id": distinct, "properties": safe}),
    )
}

fn sentry_event(record: &Value) -> Option<sentry::protocol::Event<'static>> {
    let kind = record["exception"]["type"].as_str()?;
    if ![
        "HandlerPanic",
        "InternalServerError",
        "HttpServerError",
        "CatalogUnavailable",
        "OriginAuthenticationError",
        "StartupError",
    ]
    .contains(&kind)
    {
        return None;
    }
    let mut event = sentry::protocol::Event {
        level: sentry::Level::Error,
        environment: Some("production".into()),
        ..Default::default()
    };
    event.exception = vec![sentry::protocol::Exception {
        ty: kind.to_owned(),
        ..Default::default()
    }]
    .into();
    for key in ["client_kind", "caller", "cf_worker"] {
        if let Some(value) = record["tags"][key].as_str() {
            let valid = match key {
                "caller" => value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                "client_kind" => ["internal", "unlabeled", "uptime_probe"].contains(&value),
                _ => dynapi_policy::cf_worker(Some(value)).as_deref() == Some(value),
            };
            if valid {
                event.tags.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    event.exception.values[0].stacktrace = record
        .get("stacktrace")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .map(scrub_stack);
    Some(event)
}

pub(crate) fn capture_stack() -> Option<Value> {
    // Capture at the request reporting site before enqueueing, never in the
    // delivery worker. Strip absolute paths, source text and frame locals.
    sentry::integrations::backtrace::current_stacktrace()
        .and_then(|stack| serde_json::to_value(scrub_stack(stack)).ok())
}
fn scrub_stack(mut stack: sentry::protocol::Stacktrace) -> sentry::protocol::Stacktrace {
    for frame in &mut stack.frames {
        frame.abs_path = None;
        frame.filename = frame
            .filename
            .as_ref()
            .and_then(|name| std::path::Path::new(name).file_name())
            .map(|name| name.to_string_lossy().into_owned());
        frame.vars.clear();
        frame.pre_context.clear();
        frame.post_context.clear();
        frame.context_line = None;
    }
    stack
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Stalled;
    impl Delivery for Stalled {
        fn send(&self, _: Stream, _: Value) -> SendFuture<'_> {
            Box::pin(std::future::pending())
        }
    }
    #[tokio::test(start_paused = true)]
    async fn absolute_sdk_deadline_does_not_restart_after_request_drain() {
        struct StalledFlush;
        impl Delivery for StalledFlush {
            fn send(&self, _: Stream, _: Value) -> SendFuture<'_> {
                Box::pin(async { Ok(()) })
            }
            fn flush(&self, _: Duration) -> SendFuture<'_> {
                Box::pin(std::future::pending())
            }
        }
        let sink = DeliverySink::start(Arc::new(StalledFlush), 1, false, true);
        let signal = tokio::time::Instant::now();
        let deadline = signal + Duration::from_secs(45);
        // Prior cleanup has consumed all but two seconds of the process budget,
        // less than even the SDK reserve. Cleanup cannot add four seconds.
        tokio::time::advance(Duration::from_secs(43)).await;
        assert!(
            !sink
                .shutdown_until(deadline - SDK_SHUTDOWN_RESERVE, deadline)
                .await
        );
        assert_eq!(tokio::time::Instant::now(), deadline);
        assert_eq!(sink.counters.failed(), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn stdout_and_network_shutdown_share_one_drain_deadline() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let stdout = Arc::new(super::super::StdoutSink::start(move |_| {
            let _ = entered.send(());
            let _ = blocked.recv_timeout(Duration::from_secs(2));
            Ok(())
        }));
        let sink =
            DeliverySink::start_with_stdout(Arc::new(Stalled), 1, true, false, stdout.clone());
        sink.emit(Stream::Terminal, json!({"event":"api_failure"}));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        sink.emit(Stream::Analytics, json!({}));
        let start = tokio::time::Instant::now();
        let budget = Duration::from_millis(50);
        let (drained, _) = tokio::join!(
            stdout.shutdown_started_at(start, budget),
            sink.shutdown_started_at(start, budget)
        );
        assert!(!drained);
        assert_eq!(tokio::time::Instant::now() - start, budget);
        assert_eq!(stdout.counters.dropped(), 1);
        assert_eq!(sink.counters.dropped(), 1);
        release.send(()).unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn bounded_queue_drops_without_waiting_for_a_stalled_backend() {
        let sink = DeliverySink::start(Arc::new(Stalled), 2, true, false);
        sink.emit(Stream::Analytics, json!({}));
        tokio::task::yield_now().await; // first event is in flight
        for _ in 0..5 {
            sink.emit(Stream::Analytics, json!({}));
        }
        assert_eq!(sink.counters.dropped(), 3);
        assert_eq!(sink.sender.lock().unwrap().as_ref().unwrap().capacity(), 0);
        sink.shutdown(Duration::from_millis(1)).await;
        assert_eq!(sink.counters.dropped(), 6);
        sink.emit(Stream::Analytics, json!({}));
        assert_eq!(sink.counters.dropped(), 7);
        sink.counters.dropped.store(u64::MAX, Ordering::Relaxed);
        sink.emit(Stream::Analytics, json!({}));
        assert_eq!(sink.counters.dropped(), u64::MAX);
    }
    #[tokio::test]
    async fn stalled_delivery_does_not_delay_an_http_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let sink = Arc::new(DeliverySink::start(Arc::new(Stalled), 1, true, false));
        sink.emit(Stream::Analytics, json!({}));
        tokio::task::yield_now().await;
        sink.emit(Stream::Analytics, json!({}));
        let config = crate::config::Config::from_values(&BTreeMap::from([(
            "POSTHOG_PROJECT_TOKEN".into(),
            "fake-project-token".into(),
        )]))
        .unwrap();
        let app = Arc::new(
            crate::App::new(config, Arc::new(crate::Monotonic::default()), sink.clone()).unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(crate::transport::run(listener, app));
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(b"POST /v1/forecasts HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 422"));
        assert_eq!(sink.counters.dropped(), 1);
        server.abort();
        let _ = server.await;
        sink.shutdown(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn loopback_wire_contract_and_shutdown_flush_both_destinations() {
        use http_body_util::BodyExt;
        use hyper::{Response, service::service_fn};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, mut received) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket,_) = accepted.unwrap();
                        let events = events.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                                let events = events.clone();
                                async move {
                                    let (parts,body) = request.into_parts();
                                    let body = body.collect().await.unwrap().to_bytes();
                                    events.send((parts.uri.path().to_owned(), parts.headers, body)).await.unwrap();
                                    Ok::<_,std::convert::Infallible>(Response::new(http_body_util::Full::new(bytes::Bytes::from_static(b"{}"))))
                                }
                            });
                            let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(socket),service).await;
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        let sink = DeliverySink::from_values(&BTreeMap::from([
            ("POSTHOG_PROJECT_TOKEN".into(), "fake-project-token".into()),
            ("POSTHOG_HOST".into(), format!("http://{address}")),
            ("SENTRY_DSN".into(), format!("http://public@{address}/1")),
        ]))
        .unwrap();
        sink.emit(Stream::Analytics,json!({"event":"api_request","distinct_id":"anonymous","properties":{"status":422,"$process_person_profile":false}}));
        let error =
            super::super::error_record(super::super::ErrorKind::Startup, json!({})).unwrap();
        sink.emit(Stream::Error, error);
        sink.shutdown(Duration::from_secs(5)).await;
        let mut paths = Vec::new();
        for _ in 0..2 {
            let (path, headers, body) =
                tokio::time::timeout(Duration::from_secs(1), received.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if path == "/capture/" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"api_key":"fake-project-token","event":"api_request","distinct_id":"anonymous","properties":{"status":422,"$process_person_profile":false}})
                );
            } else {
                assert_eq!(path, "/api/1/envelope/");
                assert!(headers.contains_key("x-sentry-auth"));
                let lines = std::str::from_utf8(&body)
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>();
                let event: Value = serde_json::from_str(lines[2]).unwrap();
                assert_eq!(event["exception"]["values"][0]["type"], "StartupError");
                assert_eq!(event["environment"], "production");
                assert!(event.get("request").is_none());
                assert!(event.get("user").is_none());
                assert!(event.get("transaction").is_none());
                assert!(!String::from_utf8_lossy(&body).contains("fake-project-token"));
            }
            paths.push(path);
        }
        paths.sort();
        assert_eq!(paths, ["/api/1/envelope/", "/capture/"]);
        assert_eq!(sink.counters.failed(), 0);
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn unconfigured_delivery_creates_no_worker_or_http_client() {
        let sink = DeliverySink::from_values(&BTreeMap::new()).unwrap();
        assert!(sink.sender.lock().unwrap().is_none());
        assert!(sink.worker.lock().await.is_none());
        for stream in [Stream::Analytics, Stream::Error] {
            sink.emit(stream, json!({}));
        }
        assert_eq!(sink.counters.dropped(), 0);
        assert_eq!(sink.counters.failed(), 0);
        sink.shutdown(SHUTDOWN_TIMEOUT).await;
        let invalid = BTreeMap::from([("SENTRY_DSN".into(), "fake-secret-invalid-dsn".into())]);
        assert_eq!(
            DeliverySink::from_values(&invalid).err(),
            Some("Invalid SENTRY_DSN")
        );
    }
    #[test]
    fn wire_payloads_exclude_unapproved_context_and_busy_errors() {
        let record = json!({"event":"api_request", "distinct_id":"anonymous", "properties":{"route":"/v1/forecasts", "status":500, "$process_person_profile":false, "body":"fake-payload", "ip":"192.0.2.1", "x-dynamical-viewer-auth":"fake-viewer-secret"}, "secret":"fake-secret"});
        assert_eq!(
            posthog_payload("fake-project-token", &record).unwrap(),
            json!({"api_key":"fake-project-token","event":"api_request","distinct_id":"anonymous", "properties":{"route":"/v1/forecasts","status":500,"$process_person_profile":false}})
        );
        assert!(sentry_event(&json!({"exception":{"type":"ServerBusy"}})).is_none());
        let event = sentry_event(&json!({"exception":{"type":"HandlerPanic","value":"fake-sensitive-message"},"request":{"body":"fake-payload","headers":{"x-dynamical-viewer-auth":"fake-viewer-secret"}},"tags":{"caller":"192.0.2.1","client_kind":"unlabeled","cf_worker":"example.test","secret":"fake-secret","x-dynamical-viewer-auth":"fake-viewer-secret"}})).unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        for forbidden in [
            "fake-sensitive-message",
            "fake-payload",
            "192.0.2.1",
            "fake-secret",
            "fake-viewer-secret",
            "x-dynamical-viewer-auth",
            "\"request\"",
            "\"vars\"",
            "/home/",
        ] {
            assert!(!encoded.contains(forbidden), "{forbidden}");
        }
        assert_eq!(
            event.tags.get("cf_worker").map(String::as_str),
            Some("example.test")
        );
        assert!(!event.tags.contains_key("caller"));
    }
    #[tokio::test]
    async fn oversized_events_are_dropped_before_the_queue() {
        let sink = DeliverySink::start(Arc::new(Stalled), 1, true, false);
        sink.emit(
            Stream::Analytics,
            json!({"oversized":"x".repeat(MAX_RECORD_BYTES)}),
        );
        assert_eq!(sink.counters.dropped(), 1);
        assert_eq!(sink.sender.lock().unwrap().as_ref().unwrap().capacity(), 1);
        sink.shutdown(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn saturated_sdk_and_stalled_endpoint_have_a_bounded_joined_shutdown() {
        // Hold each connection without a response to saturate both queues.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sink = DeliverySink::from_values(&BTreeMap::from([(
            "SENTRY_DSN".into(),
            format!("http://public@{address}/1"),
        )]))
        .unwrap();
        let event = json!({"exception":{"type":"StartupError"},"tags":{}});
        sink.emit(Stream::Error, event.clone());
        let (first, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let collector = tokio::spawn(async move {
            let mut sockets = vec![first];
            loop {
                sockets.push(listener.accept().await.unwrap().0);
            }
        });
        for _ in 0..(QUEUE_CAPACITY + 44) {
            sink.emit(Stream::Error, event.clone());
        }
        assert_eq!(sink.counters.dropped(), 44);
        // Every accepted event reaches the SDK while its first HTTP request is
        // stalled, necessarily overflowing its fixed 30-envelope queue.
        tokio::time::timeout(Duration::from_secs(1), async {
            while sink.counters.pending.load(Ordering::Relaxed) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let started = std::time::Instant::now();
        let budget = Duration::from_millis(50);
        sink.shutdown(budget).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed <= budget + SDK_SHUTDOWN_RESERVE + Duration::from_secs(1),
            "shutdown took {elapsed:?}"
        );
        assert_eq!(sink.counters.failed(), 1);
        assert!(sink.worker.lock().await.is_none());
        // SDK close/join was awaited; no detached flush remains.
        sink.shutdown(budget).await;
        collector.abort();
        let _ = collector.await;
    }

    #[tokio::test(start_paused = true)]
    async fn application_drain_and_sdk_flush_share_one_budget() {
        struct BudgetProbe(Mutex<Option<Duration>>);
        impl Delivery for BudgetProbe {
            fn send(&self, _: Stream, _: Value) -> SendFuture<'_> {
                Box::pin(std::future::pending())
            }
            fn flush(&self, budget: Duration) -> SendFuture<'_> {
                *self.0.lock().unwrap() = Some(budget);
                Box::pin(async { Ok(()) })
            }
        }
        let backend = Arc::new(BudgetProbe(Mutex::new(None)));
        let sink = DeliverySink::start(backend.clone(), 1, true, true);
        sink.emit(Stream::Analytics, json!({}));
        sink.shutdown(Duration::from_secs(1)).await;
        assert_eq!(*backend.0.lock().unwrap(), Some(Duration::ZERO));
        assert_eq!(sink.counters.dropped(), 1);
    }
}
