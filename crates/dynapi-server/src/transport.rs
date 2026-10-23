//! Hyper owns HTTP/1 parsing and framing directly on the socket. A connection
//! permit and header/body/idle deadlines bound pending network work.
use crate::App;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{io, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, sleep_until},
};

fn http1_builder(header_timeout: std::time::Duration) -> http1::Builder {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_timeout)
        .max_headers(crate::config::MAX_HEADER_FIELDS)
        .max_header_size(crate::config::MAX_HEADER_BYTES)
        .max_buf_size(crate::config::MAX_HEADER_BYTES);
    builder
}

pub async fn run(listener: TcpListener, app: Arc<App>) -> io::Result<()> {
    Server::new(listener, app).accept().await
}

/// Owns connection tasks independently of the cancellable accept future.
pub struct Server {
    listener: Option<TcpListener>,
    app: Arc<App>,
    permits: Arc<Semaphore>,
    connections: JoinSet<()>,
    stopping: watch::Sender<bool>,
}
impl Server {
    pub fn new(listener: TcpListener, app: Arc<App>) -> Self {
        Self {
            listener: Some(listener),
            permits: Arc::new(Semaphore::new(app.config.max_connections)),
            app,
            connections: JoinSet::new(),
            stopping: watch::channel(false).0,
        }
    }
    pub async fn accept(&mut self) -> io::Result<()> {
        let Some(listener) = self.listener.as_ref() else {
            return Ok(());
        };
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, address) = accepted?;
                    // Refuse excess sockets immediately: no waiting tasks or userspace queue.
                    let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let app = self.app.clone();
                    let stopping = self.stopping.subscribe();
                    self.connections.spawn(async move {
                        let _permit = permit;
                        serve_connection_until(stream, app, address.ip().to_string(), stopping).await;
                    });
                }
                Some(_) = self.connections.join_next(), if !self.connections.is_empty() => {}
            }
        }
    }
    pub fn stop_accepting(&mut self) {
        drop(self.listener.take());
        self.stopping.send_replace(true);
    }
    /// The absolute deadline includes time already spent stopping acceptance.
    /// Detached compute workers retain their permits until they actually end.
    pub async fn drain_until(&mut self, deadline: Instant) -> bool {
        self.stop_accepting();
        let drained = tokio::time::timeout_at(deadline, async {
            while !self.connections.is_empty() || self.app.gate.snapshot().in_use != 0 {
                tokio::select! {
                    Some(_) = self.connections.join_next(), if !self.connections.is_empty() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {},
                }
            }
        })
        .await
        .is_ok();
        if !drained {
            // Cancels HTTP futures, never native workers or their compute permits.
            // Await request destructors/terminal records before telemetry flush.
            // The process watchdog also bounds a stuck cancellation destructor.
            self.connections.shutdown().await;
        }
        drained
    }
}

pub async fn serve_connection(stream: TcpStream, app: Arc<App>, peer: String) {
    let (_stopping, stopped) = watch::channel(false);
    serve_connection_until(stream, app, peer, stopped).await;
}

async fn serve_connection_until(
    stream: TcpStream,
    app: Arc<App>,
    peer: String,
    mut stopping: watch::Receiver<bool>,
) {
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let (idle, mut updates) = watch::channel(None);
    let header_timeout = app.config.header_read_timeout;
    let idle_timeout = app.config.keep_alive_timeout;
    let io = TokioIo::new(stream);
    let service = service_fn(move |request| {
        idle.send_replace(None);
        let idle = idle.clone();
        let app = app.clone();
        let peer = peer.clone();
        async move {
            let mut response = app.serve(request, Some(peer), local).await?;
            response
                .headers_mut()
                .insert("server", "uvicorn".parse().unwrap());
            idle.send_replace(Some(Instant::now() + idle_timeout));
            Ok::<_, crate::RequestError>(response)
        }
    });
    let builder = http1_builder(header_timeout);
    let connection = builder.serve_connection(io, service);
    tokio::pin!(connection);
    let mut draining = false;
    loop {
        if !draining && *stopping.borrow_and_update() {
            connection.as_mut().graceful_shutdown();
            draining = true;
        }
        let deadline = *updates.borrow_and_update();
        let expired = async {
            if let Some(deadline) = deadline {
                sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = expired => break,
            // Disable keep-alive before HTTP readiness can dispatch new work.
            _ = stopping.changed(), if !draining => {},
            _ = &mut connection => break,
            changed = updates.changed() => if changed.is_err() { break; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn queued_stop_prevents_idle_connection_from_dispatching_a_request() {
        let app = Arc::new(
            App::new(
                crate::config::Config::from_values(&Default::default()).unwrap(),
                Arc::new(crate::Monotonic::default()),
                Arc::new(crate::telemetry::RecordingSink::default()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (stop, stopped) = watch::channel(false);
        let connection = serve_connection_until(stream, app, "127.0.0.1".into(), stopped);
        tokio::pin!(connection);
        assert!(futures_util::poll!(&mut connection).is_pending());
        // Suspend inside select!, then queue both stop and socket readiness
        // before polling again. No executor scheduling race decides their order.
        stop.send_replace(true);
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), &mut connection)
            .await
            .unwrap();
        let mut response = Vec::new();
        // An unread request may cause a reset rather than an orderly EOF.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap();
        assert!(
            response.is_empty(),
            "request dispatched after stop: {}",
            String::from_utf8_lossy(&response)
        );
    }

    #[tokio::test]
    async fn queued_stop_finishes_in_flight_request_without_dispatching_pipeline() {
        let app = Arc::new(
            App::new(
                crate::config::Config::from_values(&Default::default()).unwrap(),
                Arc::new(crate::Monotonic::default()),
                Arc::new(crate::telemetry::RecordingSink::default()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (stop, stopped) = watch::channel(false);
        let connection = serve_connection_until(stream, app, "127.0.0.1".into(), stopped);
        tokio::pin!(connection);
        client
            .write_all(b"POST /v1/forecasts HTTP/1.1\r\nHost: example.test\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(futures_util::poll!(&mut connection).is_pending());
        // The handler awaits its last body byte. Complete it and queue another
        // request after stop, while keeping the connection future suspended.
        stop.send_replace(true);
        client
            .write_all(b"}GET /healthz HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), &mut connection)
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 422"), "{response}");
        assert_eq!(
            response.matches("HTTP/1.1 ").count(),
            1,
            "pipeline dispatched after stop: {response}"
        );
    }

    #[tokio::test]
    async fn drain_waits_for_detached_compute_without_connection_tasks() {
        let app = Arc::new(
            App::new(
                crate::config::Config::from_values(&Default::default()).unwrap(),
                Arc::new(crate::Monotonic::default()),
                Arc::new(crate::telemetry::RecordingSink::default()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut server = Server::new(listener, app.clone());
        let permit = app.gate.try_acquire().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let caller = tokio::spawn(dynapi_dataset::run_with_permit(permit, move |_| {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        }));
        ready.await.unwrap();
        caller.abort();
        assert_eq!(app.gate.snapshot().in_use, 1);
        let drain = server.drain_until(Instant::now() + std::time::Duration::from_secs(3));
        tokio::pin!(drain);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut drain)
                .await
                .is_err()
        );
        assert_eq!(app.gate.snapshot().in_use, 1);
        release.send(()).unwrap();
        assert!(drain.await);
        assert_eq!(app.gate.snapshot().in_use, 0);
    }

    #[tokio::test]
    async fn complete_oversized_headers_are_rejected_even_when_reads_are_coalesced() {
        let request = format!(
            "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\nX-Padding: {}\r\n\r\n",
            "x".repeat(430_000)
        );
        let (mut client, server) = tokio::io::duplex(request.len());
        let connection = tokio::spawn(async move {
            let service = service_fn(|_| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(
                    bytes::Bytes::new(),
                )))
            });
            let _ = http1_builder(std::time::Duration::from_secs(10))
                .serve_connection(TokioIo::new(server), service)
                .await;
        });
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        connection.await.unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 431"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }
}
