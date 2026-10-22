use std::{future::Future, task::Poll, time::Duration};

// This wall-clock limit is only a deadlock guard, with ample room for a loaded
// build host. Self-waking keeps paused Tokio time from auto-advancing while
// socket I/O or spawn_blocking parsers finish; tests advance behavior time.
pub async fn guard<F: Future>(future: F) -> F::Output {
    let started = std::time::Instant::now();
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(started.elapsed() < Duration::from_secs(30), "test stalled");
        match future.as_mut().poll(cx) {
            Poll::Ready(value) => Poll::Ready(value),
            Poll::Pending => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    })
    .await
}
