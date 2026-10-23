//! One bounded queue and one native writer; requests never write to a pipe.
use super::{Stream, Telemetry};
use serde_json::Value;
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::Duration,
};

const QUEUE_CAPACITY: usize = 256;
const MAX_RECORD_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct StdoutCounters {
    dropped: AtomicU64,
    failed: AtomicU64,
    pending: AtomicU64,
}
impl StdoutCounters {
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}
fn add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(amount))
    });
}

pub struct StdoutSink {
    sender: Mutex<Option<SyncSender<Vec<u8>>>>,
    finished: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    abandoned: Arc<AtomicBool>,
    pub counters: Arc<StdoutCounters>,
}
impl Default for StdoutSink {
    fn default() -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            // An owned, unbuffered descriptor avoids holding Rust's global
            // stdout lock, including during process-exit flushing.
            let mut writer = std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .map(std::fs::File::from);
            Self::start(move |bytes| match &mut writer {
                Ok(writer) => writer.write_all(bytes),
                Err(_) => Err(std::io::Error::other("stdout unavailable")),
            })
        }
        #[cfg(not(unix))]
        Self::start(|bytes| std::io::stdout().write_all(bytes))
    }
}
impl StdoutSink {
    pub fn process_deadline(
        &self,
        started: tokio::time::Instant,
        budget: Duration,
        code: i32,
    ) -> super::shutdown::ProcessDeadline {
        let counters = self.counters.clone();
        let abandoned = self.abandoned.clone();
        super::shutdown::ProcessDeadline::with_expiry(started, budget, code, move || {
            abandoned.store(true, Ordering::Release);
            add(
                &counters.dropped,
                counters.pending.swap(0, Ordering::Relaxed),
            );
            // Expiry must not format, allocate or acquire application locks.
        })
    }
    pub fn report_loss(&self) {
        report_loss(&self.counters);
    }
    pub(super) fn start(
        mut write: impl FnMut(&[u8]) -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
        let (finished, completion) = tokio::sync::oneshot::channel();
        let counters = Arc::new(StdoutCounters::default());
        let abandoned = Arc::new(AtomicBool::new(false));
        let worker_counters = counters.clone();
        let worker_abandoned = abandoned.clone();
        std::thread::spawn(move || {
            while let Ok(bytes) = receiver.recv() {
                if worker_abandoned.load(Ordering::Acquire) {
                    break;
                }
                if write(&bytes).is_err() {
                    add(&worker_counters.failed, 1);
                }
                // Shutdown may already have counted this in-flight record as
                // abandoned. Completing a late write must not underflow.
                let _ = worker_counters.pending.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |n| Some(n.saturating_sub(1)),
                );
            }
            drop(write);
            let _ = finished.send(());
        });
        Self {
            sender: Mutex::new(Some(sender)),
            finished: Mutex::new(Some(completion)),
            abandoned,
            counters,
        }
    }
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        add(
            &self.counters.dropped,
            self.counters.pending.swap(0, Ordering::Relaxed),
        );
    }
    /// Close the producer and drain under the same deadline as network delivery.
    /// A blocked OS write cannot be cancelled; detach it until process exit.
    pub async fn shutdown_started_at(
        &self,
        started: tokio::time::Instant,
        budget: Duration,
    ) -> bool {
        self.shutdown_until(started + budget).await
    }
    /// Use the executable's absolute deadline, including time already spent.
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> bool {
        self.sender.lock().unwrap().take();
        let finished = self.finished.lock().unwrap().take();
        let Some(finished) = finished else {
            return !self.abandoned.load(Ordering::Acquire);
        };
        if matches!(
            tokio::time::timeout_at(deadline, finished).await,
            Ok(Ok(()))
        ) {
            return true;
        }
        self.abandon();
        false
    }
}
fn report_loss(counters: &StdoutCounters) {
    let dropped = counters.dropped();
    let failed = counters.failed();
    if dropped != 0 || failed != 0 {
        super::shutdown::diagnostic(
            format!("Stdout telemetry lost records: dropped={dropped}, failed={failed}\n")
                .as_bytes(),
        );
    }
}
impl Drop for StdoutSink {
    fn drop(&mut self) {
        self.sender.get_mut().unwrap().take();
        self.abandon();
    }
}
impl Telemetry for StdoutSink {
    fn emit(&self, stream: Stream, mut record: Value) {
        if stream != Stream::Terminal {
            return;
        }
        let dropped = self.counters.dropped();
        let failed = self.counters.failed();
        if record["event"] == "api_container" && (dropped != 0 || failed != 0) {
            record["telemetryDropped"] = record["telemetryDropped"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(dropped)
                .into();
            record["telemetryFailed"] = record["telemetryFailed"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(failed)
                .into();
        }
        struct Record(Vec<u8>);
        impl Write for Record {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() >= MAX_RECORD_BYTES.saturating_sub(self.0.len()) {
                    return Err(std::io::Error::other("stdout record exceeds limit"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Record(Vec::new());
        if serde_json::to_writer(&mut bytes, &record).is_err() {
            add(&self.counters.dropped, 1);
            return;
        }
        bytes.0.push(b'\n');
        let sender = self.sender.lock().unwrap();
        let Some(sender) = sender.as_ref() else {
            add(&self.counters.dropped, 1);
            return;
        };
        self.counters.pending.fetch_add(1, Ordering::Relaxed);
        if sender.try_send(bytes.0).is_err() {
            self.counters.pending.fetch_sub(1, Ordering::Relaxed);
            add(&self.counters.dropped, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn overflow_size_limit_and_abandoned_records_are_counted_without_waiting_for_writer() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let sink = StdoutSink::start(move |_| {
            let _ = entered.send(());
            let _ = blocked.recv_timeout(Duration::from_secs(2));
            Ok(())
        });
        sink.emit(Stream::Terminal, json!({"event":"api_failure"}));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..QUEUE_CAPACITY + 3 {
            sink.emit(Stream::Terminal, json!({"event":"api_failure"}));
        }
        assert_eq!(sink.counters.dropped(), 3);
        sink.emit(
            Stream::Terminal,
            json!({"oversized":"x".repeat(MAX_RECORD_BYTES)}),
        );
        assert_eq!(sink.counters.dropped(), 4);
        assert!(
            !sink
                .shutdown_started_at(tokio::time::Instant::now(), Duration::from_millis(1))
                .await
        );
        assert_eq!(sink.counters.dropped(), QUEUE_CAPACITY as u64 + 5);
        assert_eq!(sink.counters.failed(), 0);
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn heartbeat_exposes_stdout_losses_added_to_network_counters_and_drains() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let captured = records.clone();
        let sink = StdoutSink::start(move |bytes| {
            if captured.lock().unwrap().is_empty() {
                captured.lock().unwrap().push(Value::Null);
                return Err(std::io::Error::other("broken output"));
            }
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(bytes).unwrap());
            Ok(())
        });
        sink.emit(Stream::Terminal, json!({"event":"api_failure"}));
        tokio::time::timeout(Duration::from_secs(1), async {
            while sink.counters.failed() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sink.emit(
            Stream::Terminal,
            json!({"oversized":"x".repeat(MAX_RECORD_BYTES)}),
        );
        sink.emit(
            Stream::Terminal,
            json!({"event":"api_container", "telemetryDropped":7, "telemetryFailed":2}),
        );
        assert!(
            sink.shutdown_started_at(tokio::time::Instant::now(), Duration::from_secs(1))
                .await
        );
        let records = records.lock().unwrap();
        assert_eq!(records[1]["telemetryDropped"], 8);
        assert_eq!(records[1]["telemetryFailed"], 3);
        sink.counters.dropped.store(u64::MAX, Ordering::Relaxed);
        sink.emit(Stream::Terminal, json!({}));
        assert_eq!(sink.counters.dropped(), u64::MAX);
    }
}
