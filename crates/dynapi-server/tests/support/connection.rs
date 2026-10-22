use super::{Server, assert_disconnected, guard};
use dynapi_server::transport;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
};

pub struct Connection {
    pub socket: TcpStream,
    observer: Option<std::net::TcpStream>,
    enabled: watch::Sender<bool>,
    polls: Arc<AtomicUsize>,
    task: Option<JoinHandle<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Connection {
    pub async fn start(server: &Server) -> Self {
        let app = server.app.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = guard(TcpStream::connect(listener.local_addr().unwrap()))
            .await
            .unwrap();
        let (stream, peer) = guard(listener.accept()).await.unwrap();
        // A non-reading duplicate lets the test observe the receive queue. It
        // must be dropped after transport completion before checking client EOF.
        let observer = stream.into_std().unwrap();
        let stream = TcpStream::from_std(observer.try_clone().unwrap()).unwrap();
        let (enabled, mut changes) = watch::channel(true);
        let polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = polls.clone();
        let task = tokio::spawn(async move {
            let serving = transport::serve_connection(stream, app, peer.ip().to_string());
            tokio::pin!(serving);
            let serving = std::future::poll_fn(|cx| {
                let result = serving.as_mut().poll(cx);
                observed_polls.fetch_add(1, Ordering::SeqCst);
                result
            });
            tokio::pin!(serving);
            loop {
                if !*changes.borrow_and_update() {
                    if changes.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    changed = changes.changed() => if changed.is_err() { break; },
                    _ = &mut serving => break,
                }
            }
        });
        let connection = Self {
            socket,
            observer: Some(observer),
            enabled,
            polls,
            task: Some(task),
        };
        // Polling an empty connection arms its initial header deadline.
        guard(async {
            while connection.polls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        connection
    }

    pub async fn send_and_wait_for_read(&mut self, bytes: &[u8]) {
        // Current-thread tests cannot interleave transport polling with this
        // synchronous pause. The biased control branch keeps it paused if the
        // client write needs to yield. First observe queued bytes, then drain.
        self.enabled.send_replace(false);
        guard(self.socket.write_all(bytes)).await.unwrap();
        self.wait_for_queue(true).await;
        self.enabled.send_replace(true);
        self.wait_for_queue(false).await;
    }

    async fn wait_for_queue(&self, queued: bool) {
        let observer = self.observer.as_ref().unwrap();
        guard(async {
            loop {
                assert!(
                    !self.task.as_ref().unwrap().is_finished(),
                    "transport closed before read progress"
                );
                match observer.peek(&mut [0]) {
                    Ok(1) if queued => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if !queued {
                            break;
                        }
                    }
                    Ok(1) => {}
                    other => panic!("unexpected receive queue: {other:?}"),
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    pub async fn assert_open(&self) {
        let before = self.polls.load(Ordering::SeqCst);
        self.enabled.send_replace(true);
        guard(async {
            while self.polls.load(Ordering::SeqCst) == before {
                assert!(
                    !self.task.as_ref().unwrap().is_finished(),
                    "transport closed early"
                );
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            !self.task.as_ref().unwrap().is_finished(),
            "transport closed early"
        );
    }

    pub async fn expect_close(mut self) {
        // No virtual time passes in this guard: a later header/body/idle timer
        // cannot satisfy the configured deadline's assertion.
        guard(self.task.take().unwrap()).await.unwrap();
        drop(self.observer.take());
        assert_disconnected(&mut self.socket).await;
    }
}
