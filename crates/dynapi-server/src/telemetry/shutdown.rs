//! Process exit also bounds blocked native writers and SDK cleanup.
use std::time::Duration;

pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
pub const SDK_SHUTDOWN_RESERVE: Duration = Duration::from_secs(4);

/// Stdout always drains; a configured Sentry SDK also needs its join reserve.
pub fn minimum_finalization_reserve(error_reporting: bool) -> Duration {
    SHUTDOWN_TIMEOUT
        + if cfg!(feature = "telemetry-delivery") && error_reporting {
            SDK_SHUTDOWN_RESERVE
        } else {
            Duration::ZERO
        }
}

/// A process-owned deadline also covers synchronous cleanup and runtime drops.
pub struct ProcessDeadline(
    pub(super) Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>,
);
impl Drop for ProcessDeadline {
    fn drop(&mut self) {
        if let Some((cancel, thread)) = self.0.take() {
            let _ = cancel.send(());
            let _ = thread.join();
        }
    }
}
pub(super) fn diagnostic(message: &[u8]) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WRITING: AtomicBool = AtomicBool::new(false);
    // Main cleanup and the process deadline can race. Never wait for another
    // diagnostic, or restore blocking flags while its nonblocking write runs.
    if WRITING.swap(true, Ordering::Acquire) {
        return;
    }
    #[cfg(unix)]
    unsafe {
        // Best effort only: a full stderr pipe must not delay process exit.
        let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
        if flags >= 0
            && libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
        {
            libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
            libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags);
        }
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(message);
    }
    WRITING.store(false, Ordering::Release);
}
fn exit_at_deadline(code: i32) -> ! {
    diagnostic(b"Shutdown deadline exceeded; pending responses and telemetry may be lost\n");
    #[cfg(unix)]
    unsafe {
        // Unlike exit(), _exit cannot wait for a C atexit hook or stdio lock.
        libc::_exit(code);
    }
    #[cfg(not(unix))]
    {
        std::process::exit(code);
    }
}
impl ProcessDeadline {
    pub(super) fn with_expiry(
        started: tokio::time::Instant,
        budget: Duration,
        code: i32,
        expired: impl Fn() + Send + 'static,
    ) -> Self {
        let deadline = (started + budget).into_std();
        let (cancel, cancelled) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            if matches!(
                cancelled
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                expired();
                // Captured Arcs may be the last owners during runtime teardown.
                // Do not run their allocator/destructor paths before _exit.
                std::mem::forget(expired);
                exit_at_deadline(code);
            }
        });
        Self(Some((cancel, thread)))
    }
}
