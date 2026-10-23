#![allow(dead_code)]
use dynapi_policy::Clock;
use dynapi_server::{App, config::Config, telemetry::RecordingSink, transport};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, Semaphore},
    task::{JoinHandle, JoinSet},
};

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../dynapi-dataset/tests/fixtures")
}
pub fn input(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(fixtures().join(name).join("input.json")).unwrap())
        .unwrap()
}
#[derive(Default)]
pub struct TestClock(pub AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> f64 {
        self.0.load(Ordering::SeqCst) as f64
    }
}
pub struct Reads {
    pub paths: Mutex<Vec<String>>,
    pub hold_chunks: AtomicBool,
    pub chunk_started: Notify,
    pub release: Semaphore,
    pub fail_chunks: AtomicBool,
    pub fail_catalog: AtomicBool,
    pub hold_catalog: AtomicBool,
    pub catalog_started: Notify,
    pub fail_snapshots: AtomicBool,
    pub hold_snapshots: AtomicBool,
    pub snapshot_started: Notify,
    pub version_bytes: AtomicUsize,
}
impl Default for Reads {
    fn default() -> Self {
        Self {
            paths: Mutex::new(vec![]),
            hold_chunks: AtomicBool::new(false),
            chunk_started: Notify::new(),
            release: Semaphore::new(0),
            fail_chunks: AtomicBool::new(false),
            fail_catalog: AtomicBool::new(false),
            hold_catalog: AtomicBool::new(false),
            catalog_started: Notify::new(),
            fail_snapshots: AtomicBool::new(false),
            hold_snapshots: AtomicBool::new(false),
            snapshot_started: Notify::new(),
            version_bytes: AtomicUsize::new(0),
        }
    }
}
pub struct FixtureHost {
    pub url: String,
    pub reads: Arc<Reads>,
    task: JoinHandle<()>,
}
impl Drop for FixtureHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl FixtureHost {
    pub async fn start() -> Self {
        Self::start_with_root(None).await
    }
    pub async fn start_with_root(root: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reads = Arc::new(Reads {
            release: Semaphore::new(0),
            ..Reads::default()
        });
        let state = reads.clone();
        let base = url.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => { let (socket, _) = accepted.unwrap(); let state = state.clone(); let base = base.clone(); let root = root.clone(); tasks.spawn(async move { let _ = serve_fixture(socket, &base, state, root.as_deref()).await; }); },
                    Some(result) = tasks.join_next() => { result.unwrap(); }
                }
            }
        });
        Self { url, reads, task }
    }
}
async fn serve_fixture(
    mut socket: TcpStream,
    _base: &str,
    reads: Arc<Reads>,
    root: Option<&Path>,
) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut buf = [0; 4096];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap();
    let path = first.split_whitespace().nth(1).unwrap();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
        .collect();
    reads.paths.lock().unwrap().push(path.into());
    if path == "/catalog.json" && reads.hold_catalog.load(Ordering::SeqCst) {
        reads.catalog_started.notify_one();
        reads.release.acquire().await.unwrap().forget();
    }
    if path == "/catalog.json" && reads.fail_catalog.load(Ordering::SeqCst) {
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    }
    if path.contains("/snapshots/") && reads.hold_snapshots.load(Ordering::SeqCst) {
        reads.snapshot_started.notify_one();
        reads.release.acquire().await.unwrap().forget();
    }
    if path.contains("/snapshots/") && reads.fail_snapshots.load(Ordering::SeqCst) {
        socket
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    }
    if path.contains("/chunks/") {
        if reads.hold_chunks.load(Ordering::SeqCst) {
            reads.chunk_started.notify_one();
            reads.release.acquire().await.unwrap().forget();
        }
        if reads.fail_chunks.load(Ordering::SeqCst) {
            socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            return Ok(());
        }
    }
    let body = if let Some(root) = root {
        if path.split('/').any(|s| s == "..") {
            None
        } else {
            std::fs::read(root.join(path.trim_start_matches('/'))).ok()
        }
    } else if path == "/catalog.json" {
        Some(json!({"links": (["forecast","ensemble","analysis","projected"].map(|n| json!({"rel":"child","href":format!("/{n}/collection.json")})))}).to_string().into_bytes())
    } else if let Some(name) = path
        .strip_prefix('/')
        .and_then(|p| p.strip_suffix("/collection.json"))
    {
        let mut collection = input(name)["collection"].clone();
        let bytes = reads.version_bytes.load(Ordering::SeqCst);
        if bytes > 0 {
            collection["version"] = json!("v".repeat(bytes));
        }
        Some(collection.to_string().into_bytes())
    } else {
        let relative = path.trim_start_matches('/').trim_start_matches("fixture-");
        if relative.split('/').any(|s| s == "..") {
            None
        } else {
            std::fs::read(fixtures().join(relative)).ok()
        }
    };
    let Some(body) = body else {
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };
    let length = body.len();
    let (status, body, extra) =
        if let Some(range) = headers.get("range").and_then(|v| v.strip_prefix("bytes=")) {
            let (start, end) = range.split_once('-').unwrap();
            let (start, end) = if start.is_empty() {
                (
                    length.saturating_sub(end.parse::<usize>().unwrap()),
                    length - 1,
                )
            } else {
                (
                    start.parse().unwrap(),
                    if end.is_empty() {
                        length - 1
                    } else {
                        end.parse::<usize>().unwrap().min(length - 1)
                    },
                )
            };
            (
                206,
                body[start..=end].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{length}\r\n"),
            )
        } else {
            (200, body, String::new())
        };
    let header = format!(
        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nAccept-Ranges: bytes\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await?;
    if !first.starts_with("HEAD ") {
        socket.write_all(&body).await?;
    }
    Ok(())
}
pub struct Server {
    pub address: std::net::SocketAddr,
    pub app: Arc<App>,
    pub sink: Arc<RecordingSink>,
    pub clock: Arc<TestClock>,
    pub host: FixtureHost,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(values: &[(&str, &str)]) -> Self {
        Self::start_with_catalog(values, None).await
    }
    pub async fn start_with_catalog(
        values: &[(&str, &str)],
        catalog: Option<dynapi_catalog::native::StacCatalog>,
    ) -> Self {
        Self::start_with_catalog_and_root(values, catalog, None).await
    }
    pub async fn start_with_catalog_and_root(
        values: &[(&str, &str)],
        catalog: Option<dynapi_catalog::native::StacCatalog>,
        root: Option<PathBuf>,
    ) -> Self {
        let host = FixtureHost::start_with_root(root).await;
        let mut env: BTreeMap<String, String> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.entry("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE".into())
            .or_insert_with(|| "1".into());
        env.insert(
            "DYNAMICAL_API_STAC_URL".into(),
            format!("{}/catalog.json", host.url),
        );
        env.insert("DYNAPI_HARNESS_STORE_BASE_URL".into(), host.url.clone());
        let config = Config::from_values(&env).unwrap();
        let sink = Arc::new(RecordingSink::default());
        let clock = Arc::new(TestClock::default());
        let app = Arc::new(
            match catalog {
                Some(catalog) => App::with_catalog(config, clock.clone(), sink.clone(), catalog),
                None => App::new(config, clock.clone(), sink.clone()),
            }
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = app.clone();
        let task = tokio::spawn(async move {
            transport::run(listener, serve).await.unwrap();
        });
        Self {
            address,
            app,
            sink,
            clock,
            host,
            task,
        }
    }
    pub async fn request(&self, method: &str, path: &str, body: &Value) -> Reply {
        let mut socket = self.socket(method, path, body).await;
        let mut raw = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            socket.read_to_end(&mut raw),
        )
        .await
        .unwrap()
        .unwrap();
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&raw[..end]).unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = head
            .split("\r\n")
            .skip(1)
            .map(|l| {
                let (k, v) = l.split_once(':').unwrap();
                (k.to_lowercase(), v.trim().to_owned())
            })
            .collect();
        Reply {
            status,
            headers,
            body: raw[end + 4..].to_vec(),
        }
    }
    pub async fn socket(&self, method: &str, path: &str, body: &Value) -> TcpStream {
        let mut socket = TcpStream::connect(self.address).await.unwrap();
        let body = if method == "POST" {
            body.to_string()
        } else {
            String::new()
        };
        let header = format!(
            "{method} {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket
    }
}
pub struct Reply {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}
pub fn payload() -> Value {
    json!({"queries":[{"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":["temperature_2m","count","static"],"initTime":"2026-07-30T00:00:00Z"}]})
}
