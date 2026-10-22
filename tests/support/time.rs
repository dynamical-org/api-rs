use std::{future::Future, task::Poll, time::Duration};

// This wall-clock limit is only a deadlock guard, with ample room for a loaded
// build host. Cooperative polling keeps paused Tokio time from auto-advancing
// while socket I/O or spawn_blocking parsers finish; tests advance behavior time.
pub async fn guard<F: Future>(future: F) -> F::Output {
    let started = std::time::Instant::now();
    tokio::pin!(future);
    loop {
        assert!(started.elapsed() < Duration::from_secs(30), "test stalled");
        if let Poll::Ready(value) =
            std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
        {
            return value;
        }
        // Defer the next poll so I/O and timer tasks get a scheduling turn too.
        tokio::task::yield_now().await;
    }
}
