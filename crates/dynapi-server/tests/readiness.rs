#![cfg(all(unix, feature = "harness"))]
mod support;

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    net::SocketAddr,
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use support::FixtureHost;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

struct Process {
    child: Child,
    lines: mpsc::UnboundedReceiver<Value>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Process {
    fn spawn(host: &FixtureHost, settings: &[(&str, &str)], port: u16) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dynapi-server"));
        command
            .env_clear()
            .env("TOKIO_WORKER_THREADS", "2")
            .args(["--port", &port.to_string()])
            .env(
                "DYNAMICAL_API_STAC_URL",
                format!("{}/catalog.json", host.url),
            )
            .env("DYNAPI_HARNESS_STORE_BASE_URL", &host.url)
            .env("DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS", "2")
            .env("DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS", "0.3")
            .envs(settings.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str(&line) {
                    let _ = sender.send(value);
                }
            }
        });
        Self { child, lines }
    }
    async fn event(&mut self, event: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(value) = self.lines.recv().await {
                if value["event"] == event {
                    return value;
                }
            }
            panic!(
                "process exited without {event}: {:?}",
                self.child.try_wait()
            );
        })
        .await
        .unwrap()
    }
    async fn ready(&mut self) -> (SocketAddr, Value) {
        let event = self.event("api_ready").await;
        (event["address"].as_str().unwrap().parse().unwrap(), event)
    }
    fn signal(&self, signal: &str) {
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    async fn exited(&mut self, limit: Duration) -> ExitStatus {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server exceeded its process exit bound")
    }
}
async fn request(address: SocketAddr, path: &str, extra: &str, body: &str) -> TcpStream {
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("POST {path} HTTP/1.1\r\nHost: example.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}", body.len()).as_bytes()).await.unwrap();
    socket
}
async fn response(socket: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}
fn forecast() -> String {
    json!({"queries":[{"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":["temperature_2m"]}]}).to_string()
}

#[tokio::test]
async fn first_catalog_load_finishes_before_bind() {
    let host = FixtureHost::start().await;
    host.reads.hold_catalog.store(true, Ordering::SeqCst);
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut process = Process::spawn(&host, &[], address.port());
    tokio::time::timeout(
        Duration::from_secs(10),
        host.reads.catalog_started.notified(),
    )
    .await
    .unwrap();
    assert!(TcpStream::connect(address).await.is_err());
    host.reads.hold_catalog.store(false, Ordering::SeqCst);
    host.reads.release.add_permits(1);
    let (_, ready) = process.ready().await;
    assert_eq!(ready["catalogReady"], true);
    assert_eq!(ready["catalogWaitOutcome"], "ready");
    assert_eq!(ready["bindToCatalogReadyMs"], 0);
    process.signal("-TERM");
    assert!(process.exited(Duration::from_secs(3)).await.success());
}

#[tokio::test]
async fn failed_first_load_still_binds_and_serves_unloaded_catalog() {
    let host = FixtureHost::start().await;
    host.reads.fail_catalog.store(true, Ordering::SeqCst);
    let mut process = Process::spawn(&host, &[], 0);
    let (address, ready) = process.ready().await;
    assert_eq!(ready["catalogReady"], false);
    assert_eq!(ready["catalogWaitOutcome"], "failed");
    let mut socket = request(
        address,
        "/v1/forecasts",
        "Connection: close\r\n",
        &forecast(),
    )
    .await;
    assert!(response(&mut socket).await.starts_with(b"HTTP/1.1 503"));
    process.signal("-TERM");
    assert!(process.exited(Duration::from_secs(3)).await.success());
}

#[tokio::test]
async fn catalog_wait_timeout_binds_without_cancelling_flight_and_reports_late_readiness() {
    let host = FixtureHost::start().await;
    host.reads.hold_catalog.store(true, Ordering::SeqCst);
    let mut process = Process::spawn(&host, &[("DYNAMICAL_API_CATALOG_WAIT_SECONDS", "0.1")], 0);
    let (address, ready) = process.ready().await;
    assert_eq!(ready["catalogReady"], false);
    assert_eq!(ready["catalogWaitOutcome"], "timeout");
    assert!(ready["bindToCatalogReadyMs"].is_null());
    assert!(TcpStream::connect(address).await.is_ok());
    host.reads.hold_catalog.store(false, Ordering::SeqCst);
    host.reads.release.add_permits(1);
    let loaded = process.event("api_catalog_ready").await;
    assert!(loaded["bindToCatalogReadyMs"].as_f64().unwrap() >= 0.);
    assert_eq!(
        host.reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == "/catalog.json")
            .count(),
        1
    );
    process.signal("-TERM");
    assert!(process.exited(Duration::from_secs(3)).await.success());
}

#[tokio::test]
async fn sigterm_stops_accepting_then_completes_in_flight_request() {
    let host = FixtureHost::start().await;
    host.reads.hold_snapshots.store(true, Ordering::SeqCst);
    let mut process = Process::spawn(&host, &[], 0);
    let (address, _) = process.ready().await;
    // Keep-alive intentionally exercises Hyper's graceful close after this request.
    let mut socket = request(address, "/v1/forecasts", "", &forecast()).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        host.reads.snapshot_started.notified(),
    )
    .await
    .unwrap();
    process.signal("-TERM");
    process.event("api_shutdown").await;
    assert!(TcpStream::connect(address).await.is_err());
    assert!(process.child.try_wait().unwrap().is_none());
    host.reads.hold_snapshots.store(false, Ordering::SeqCst);
    host.reads.release.add_permits(32);
    let result = response(&mut socket).await;
    assert!(
        result.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&result)
    );
    let body_start = result
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    let body: Value = serde_json::from_slice(&result[body_start..]).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["dataProduct"]["id"], "fixture-forecast");
    assert!(process.exited(Duration::from_secs(3)).await.success());
}

#[tokio::test]
async fn sigint_also_drains_and_closes_idle_keep_alive_connections() {
    let host = FixtureHost::start().await;
    let mut process = Process::spawn(&host, &[], 0);
    let (address, _) = process.ready().await;
    let mut idle = TcpStream::connect(address).await.unwrap();
    idle.write_all(b"GET /healthz HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .await
        .unwrap();
    let mut header = [0; 4096];
    assert!(idle.read(&mut header).await.unwrap() > 0);
    process.signal("-INT");
    assert!(process.exited(Duration::from_secs(3)).await.success());
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn stuck_handler_cannot_hold_process_past_drain_and_finalization_bound() {
    let host = FixtureHost::start().await;
    let mut process = Process::spawn(&host, &[("DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS", "0.2")], 0);
    let (address, _) = process.ready().await;
    let _socket = request(
        address,
        "/healthz",
        "X-Dynapi-Harness-Stuck: handler\r\n",
        "",
    )
    .await;
    process.event("api_harness_stuck").await;
    let started = Instant::now();
    process.signal("-TERM");
    let failure = process.event("api_failure").await;
    assert_eq!(failure["cancelled"], true);
    let drained = process.event("api_shutdown_drained").await;
    assert_eq!(drained["drained"], false);
    let status = process.exited(Duration::from_secs(2)).await;
    assert!(!status.success());
    assert!(started.elapsed() >= Duration::from_millis(180));
}

#[tokio::test]
async fn stuck_native_worker_does_not_block_runtime_shutdown() {
    let host = FixtureHost::start().await;
    let mut process = Process::spawn(&host, &[("DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS", "0.2")], 0);
    let (address, _) = process.ready().await;
    let socket = request(
        address,
        "/healthz",
        "X-Dynapi-Harness-Stuck: worker\r\n",
        "",
    )
    .await;
    process.event("api_harness_stuck").await;
    drop(socket);
    process.signal("-TERM");
    let stopped = process.event("api_shutdown_drained").await;
    assert_eq!(stopped["drained"], false);
    assert_eq!(stopped["computeInUse"], 1);
    assert_eq!(process.exited(Duration::from_secs(2)).await.code(), Some(1));
}

#[tokio::test]
async fn detached_native_worker_keeps_permit_and_watchdog_bounds_stuck_cleanup() {
    let host = FixtureHost::start().await;
    let mut process = Process::spawn(
        &host,
        &[
            ("DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS", "0.2"),
            ("DYNAPI_HARNESS_STUCK_FINALIZATION", "1"),
        ],
        0,
    );
    let (address, _) = process.ready().await;
    let socket = request(
        address,
        "/healthz",
        "X-Dynapi-Harness-Stuck: worker\r\n",
        "",
    )
    .await;
    process.event("api_harness_stuck").await;
    drop(socket);
    let started = Instant::now();
    process.signal("-TERM");
    let stopped = process.event("api_shutdown_drained").await;
    assert_eq!(stopped["drained"], false);
    assert_eq!(stopped["computeInUse"], 1);
    let status = process.exited(Duration::from_secs(2)).await;
    assert_eq!(status.code(), Some(1));
    assert!(started.elapsed() >= Duration::from_millis(450));
}

#[tokio::test]
async fn required_origin_auth_refuses_startup_without_a_nonblank_secret() {
    let host = FixtureHost::start().await;
    for secret in [None, Some(""), Some(" \t")] {
        let mut settings = vec![("DYNAMICAL_API_REQUIRE_ORIGIN_AUTH", "1")];
        if let Some(secret) = secret {
            settings.push(("DYNAMICAL_API_ORIGIN_SECRET", secret));
        }
        let mut process = Process::spawn(&host, &settings, 0);
        assert!(!process.exited(Duration::from_secs(3)).await.success());
    }
}
