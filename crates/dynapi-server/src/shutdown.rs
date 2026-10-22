//! One process-owned watchdog covers drain, finalization and runtime teardown.
use std::time::{Duration, Instant};

pub struct ProcessDeadline {
    cancel: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl ProcessDeadline {
    pub fn start(started: Instant, budget: Duration) -> Self {
        let deadline = started + budget;
        let (cancel, cancelled) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            if matches!(
                cancelled.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                // No logging, application locks, destructors or C atexit hooks.
                #[cfg(unix)]
                unsafe {
                    libc::_exit(1);
                }
                #[cfg(not(unix))]
                std::process::exit(1);
            }
        });
        Self {
            cancel,
            thread: Some(thread),
        }
    }
}
impl Drop for ProcessDeadline {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}
impl Signals {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        })
    }
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.terminate.recv() => {},
            _ = self.interrupt.recv() => {},
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}
