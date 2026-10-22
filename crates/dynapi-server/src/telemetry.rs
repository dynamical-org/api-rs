//! Injectable telemetry. Only handler success records are sampled; failures and
//! heartbeats are unsampled. Delivery is an optional native adapter.
#[cfg(feature = "telemetry-delivery")]
pub mod delivery;
pub mod heartbeat;
pub mod loads;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Terminal,
    Analytics,
    Error,
}
pub trait Telemetry: Send + Sync {
    fn emit(&self, stream: Stream, record: Value);
}

/// No caller-provided messages or panic payloads may enter an error record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    HandlerPanic,
    InternalServerError,
    HttpServerError,
    CatalogUnavailable,
    OriginAuthentication,
    Startup,
    ServerBusy,
}
impl ErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::HandlerPanic => "HandlerPanic",
            Self::InternalServerError => "InternalServerError",
            Self::HttpServerError => "HttpServerError",
            Self::CatalogUnavailable => "CatalogUnavailable",
            Self::OriginAuthentication => "OriginAuthenticationError",
            Self::Startup => "StartupError",
            Self::ServerBusy => "ServerBusy",
        }
    }
}

pub(crate) fn best_effort<T>(operation: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).ok()
}

pub fn client_kind(user_agent: &str) -> &'static str {
    let lowered = user_agent.to_lowercase();
    if lowered.starts_with("dynamical-") {
        "internal"
    } else if lowered.contains("uptime bot") || lowered.contains("sentry uptime") {
        "uptime_probe"
    } else {
        "unlabeled"
    }
}

pub fn caller_id(key: Option<&str>, ip: Option<&str>) -> Option<String> {
    let (key, ip) = (key?, ip.filter(|ip| !ip.is_empty())?);
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(ip.as_bytes());
    Some(format!("{:x}", mac.finalize().into_bytes())[..16].to_owned())
}

/// Request tags contain only bounded metadata and an authenticated keyed identity.
pub fn request_tags(headers: &hyper::HeaderMap, key: Option<&str>) -> Value {
    let header = |name: &str| headers.get(name).map(|v| crate::latin1(v.as_bytes()));
    let mut tags = serde_json::json!({
        "client_kind": client_kind(&header("user-agent").unwrap_or_default()),
        "caller": caller_id(key, header("cf-connecting-ip").as_deref()),
    });
    if let Some(worker) = dynapi_policy::cf_worker(header("cf-worker").as_deref()) {
        tags["cf_worker"] = worker.into();
    }
    tags
}

pub fn error_record(kind: ErrorKind, tags: Value) -> Option<Value> {
    if kind == ErrorKind::ServerBusy {
        return None;
    }
    let record = serde_json::json!({
        "event": "api_error", "exception": {"type": kind.name()},
        "level": "error", "environment": "production", "tags": tags,
    });
    #[cfg(feature = "telemetry-delivery")]
    let record = {
        let mut record = record;
        if let Some(stack) = delivery::capture_stack() {
            record["stacktrace"] = stack;
        }
        record
    };
    Some(record)
}

fn sampled(rate: f64, draw: impl FnOnce() -> f64) -> bool {
    !(rate <= 0.0 || (rate < 1.0 && draw() >= rate))
}

pub(crate) fn should_sample(rate: f64) -> bool {
    sampled(rate, || {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        // The last eight bytes avoid the UUID version and variant bits.
        let bits = u64::from_be_bytes(bytes[8..].try_into().unwrap()) & ((1_u64 << 62) - 1);
        (bits >> 9) as f64 / (1_u64 << 53) as f64
    })
}

pub(crate) fn round_decimal(value: f64, precision: usize) -> f64 {
    // Fixed decimal formatting rounds the original binary value. Multiplying
    // first can erase a half-way difference (e.g. 0.15 * 10 rounds to 1.5).
    format!("{value:.precision$}").parse().unwrap_or(value)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_rounding_preserves_binary_halfway_direction_and_even_ties() {
        for (value, precision, expected) in [
            (0.15, 1, 0.1),
            (0.25, 1, 0.2),
            (0.35, 1, 0.3),
            (1.25, 1, 1.2),
            (1.35, 1, 1.4),
            (2.675, 2, 2.67),
            (1.0625, 3, 1.062),
            (1.1875, 3, 1.188),
        ] {
            assert_eq!(
                round_decimal(value, precision),
                expected,
                "{value} at {precision}"
            );
        }
        assert!(round_decimal(-0.0001, 3).is_sign_negative());
        assert_eq!(round_decimal(-0.0001, 3), 0.0);
    }

    #[test]
    fn sampling_boundaries_and_non_finite_rates_match_the_contract() {
        for rate in [0.0, -1.0, f64::NEG_INFINITY] {
            assert!(!sampled(rate, || panic!("disabled sampling must not draw")));
        }
        for rate in [1.0, 2.0, f64::INFINITY, f64::NAN] {
            assert!(sampled(rate, || panic!(
                "unconditional sampling must not draw"
            )));
        }
        assert!(sampled(0.1, || 0.099));
        assert!(!sampled(0.1, || 0.1));
        assert!(!sampled(0.1, || 0.999));
    }

    #[test]
    fn keyed_identity_matches_rfc4231_and_requires_a_key_and_nonempty_address() {
        // RFC 4231 test case 2, truncated to the public identity length.
        assert_eq!(
            caller_id(Some("Jefe"), Some("what do ya want for nothing?")).as_deref(),
            Some("5bdcc146bf60754e")
        );
        assert_eq!(caller_id(None, Some("192.0.2.1")), None);
        assert_eq!(caller_id(Some("fake-key"), None), None);
        assert_eq!(caller_id(Some("fake-key"), Some("")), None);
        assert_ne!(
            caller_id(Some("fake-key"), Some("192.0.2.1")),
            caller_id(Some("other-key"), Some("192.0.2.1"))
        );
    }

    #[test]
    fn client_classification_is_case_insensitive_and_internal_takes_precedence() {
        for (ua, expected) in [
            ("DYNAMICAL-test Sentry Uptime", "internal"),
            ("Better Uptime Bot", "uptime_probe"),
            ("SENTRY UPTIME", "uptime_probe"),
            ("example-client", "unlabeled"),
            ("", "unlabeled"),
        ] {
            assert_eq!(client_kind(ua), expected);
        }
    }

    #[test]
    fn busy_errors_are_filtered_and_error_records_exclude_payloads() {
        assert_eq!(
            error_record(ErrorKind::ServerBusy, serde_json::json!({})),
            None
        );
        let mut headers = hyper::HeaderMap::new();
        for (name, value) in [
            ("cf-connecting-ip", "192.0.2.1"),
            ("x-dynamical-origin-auth", "fake-secret"),
            ("x-dynamical-viewer-auth", "fake-viewer-secret"),
            ("authorization", "Bearer fake-token"),
            ("cookie", "session=fake"),
            ("cf-worker", " example.test "),
        ] {
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let record = error_record(
            ErrorKind::HandlerPanic,
            request_tags(&headers, Some("fake-key")),
        )
        .unwrap();
        assert_eq!(record["tags"]["cf_worker"], "example.test");
        let encoded = record.to_string();
        for forbidden in [
            "192.0.2.1",
            "fake-secret",
            "fake-viewer-secret",
            "x-dynamical-viewer-auth",
            "fake-token",
            "session",
            "request",
            "body",
            "latitude",
            "longitude",
        ] {
            assert!(!encoded.contains(forbidden), "{forbidden}: {encoded}");
        }
    }

    #[test]
    fn only_handler_success_records_are_sampled_and_all_fields_pass_through() {
        for (rate, expected) in [("0", 2), ("1", 3)] {
            let config = crate::config::Config::from_values(&std::collections::BTreeMap::from([(
                "DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE".into(),
                rate.into(),
            )]))
            .unwrap();
            let sink = std::sync::Arc::new(RecordingSink::default());
            let app = crate::App::new(
                config,
                std::sync::Arc::new(crate::Monotonic::default()),
                sink.clone(),
            )
            .unwrap();
            let success = serde_json::json!({"event":"api_request","route":"/v1/forecasts","dataProductIds":["example-forecast"],"queryCount":1,"workUnits":2,"responseBytes":3,"timingsMs":{"example":1.25}});
            app.emit(Stream::Terminal, success.clone());
            app.emit(Stream::Terminal, serde_json::json!({"event":"api_failure"}));
            app.emit(
                Stream::Terminal,
                serde_json::json!({"event":"api_container"}),
            );
            let records = sink.0.lock().unwrap();
            assert_eq!(records.len(), expected);
            if rate == "1" {
                assert_eq!(records[0].1, success);
            }
        }
    }
}
