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
    let permits = Arc::new(Semaphore::new(app.config.max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, address) = accepted?;
                // Refuse excess sockets immediately: no waiting tasks or userspace queue.
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let app = app.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, app, address.ip().to_string()).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

pub async fn serve_connection(stream: TcpStream, app: Arc<App>, peer: String) {
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
            let mut response = app.serve(request, peer, local).await?;
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
    loop {
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
