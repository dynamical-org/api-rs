//! Sink boundaries for telemetry integration. Failure records are unsampled; cheap successes do
//! not emit stdout api_request records (Python publishes only handler records).
use serde_json::Value;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Terminal,
    Analytics,
}
pub trait Telemetry: Send + Sync {
    fn emit(&self, stream: Stream, record: Value);
}
#[derive(Default)]
pub struct RecordingSink(pub Mutex<Vec<(Stream, Value)>>);
impl Telemetry for RecordingSink {
    fn emit(&self, stream: Stream, record: Value) {
        self.0.lock().unwrap().push((stream, record));
    }
}
pub struct StdoutSink;
impl Telemetry for StdoutSink {
    fn emit(&self, stream: Stream, record: Value) {
        if stream == Stream::Terminal {
            // Broken stdout must never replace the request's response.
            use std::io::Write;
            let _ = writeln!(std::io::stdout().lock(), "{record}");
        }
    }
}

#[cfg(feature = "harness")]
pub struct HarnessSink;
#[cfg(feature = "harness")]
impl Telemetry for HarnessSink {
    fn emit(&self, _: Stream, record: Value) {
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), "{record}");
    }
}
