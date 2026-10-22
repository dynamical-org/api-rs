use dynapi_policy::RateProfile;
use std::{collections::BTreeMap, env, time::Duration};

pub const MAX_HEADER_FIELDS: usize = 100;
/// Includes the request line and header delimiters.
pub const MAX_HEADER_BYTES: usize = 417_792;

#[derive(Clone, Debug)]
pub struct Config {
    pub origin_secret: Option<String>,
    pub release: Option<hyper::header::HeaderValue>,
    pub public_scheme: String,
    pub catalog_wait: Duration,
    pub drain_timeout: Duration,
    pub finalization_timeout: Duration,
    pub rate: RateProfile,
    pub query_rate: RateProfile,
    pub compute_slots: usize,
    pub max_connections: usize,
    pub header_read_timeout: Duration,
    pub body_read_timeout: Duration,
    pub keep_alive_timeout: Duration,
    pub busy_retry_after: u64,
    pub max_response_values: u64,
    pub max_read_values: u64,
    pub timing_sample_rate: f64,
    pub heartbeat_seconds: Option<f64>,
    pub dataset_cache_entries: usize,
    pub main_dataset_ttl_seconds: f64,
    pub stac_url: String,
    pub analytics_enabled: bool,
    pub error_reporting_enabled: bool,
    pub container: String,
    #[cfg(feature = "harness")]
    pub fixed_clock: bool,
    #[cfg(feature = "harness")]
    pub fixture_store_base: Option<String>,
}
impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_values(&env::vars().collect())
    }
    pub fn from_values(values: &BTreeMap<String, String>) -> Result<Self, String> {
        let get = |key: &str, default: &str| {
            values
                .get(key)
                .map(String::as_str)
                .unwrap_or(default)
                .to_owned()
        };
        let number = |key: &str, default: &str| {
            get(key, default)
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("Invalid {key}"))
        };
        let positive = |key: &str, default: &str| {
            number(key, default).and_then(|n| {
                if n > 0 {
                    Ok(n)
                } else {
                    Err(format!("{key} must be positive"))
                }
            })
        };
        let timeout = |key: &str, default: &str| {
            let duration = Duration::from_secs(positive(key, default)?);
            if std::time::Instant::now().checked_add(duration).is_none() {
                return Err(format!("{key} exceeds clock capacity"));
            }
            Ok(duration)
        };
        let origin_secret = values.get("DYNAMICAL_API_ORIGIN_SECRET").cloned();
        if origin_secret.as_deref() == Some("") {
            return Err("DYNAMICAL_API_ORIGIN_SECRET must not be empty".into());
        }
        if values
            .get("DYNAMICAL_API_REQUIRE_ORIGIN_AUTH")
            .is_some_and(|v| v == "1")
            && origin_secret.as_deref().is_none_or(|v| v.trim().is_empty())
        {
            return Err(
                "DYNAMICAL_API_REQUIRE_ORIGIN_AUTH requires a nonblank DYNAMICAL_API_ORIGIN_SECRET"
                    .into(),
            );
        }
        let release = values
            .get("DYNAMICAL_API_RELEASE")
            .map(|value| value.parse().map_err(|_| "Invalid DYNAMICAL_API_RELEASE"))
            .transpose()?;
        let public_scheme = get("DYNAMICAL_API_PUBLIC_SCHEME", "http");
        if !matches!(public_scheme.as_str(), "http" | "https") {
            return Err("DYNAMICAL_API_PUBLIC_SCHEME must be http or https".into());
        }
        let seconds = |key: &str, default: &str, allow_zero: bool| -> Result<Duration, String> {
            let value = get(key, default)
                .parse::<f64>()
                .map_err(|_| format!("Invalid {key}"))?;
            let duration =
                Duration::try_from_secs_f64(value).map_err(|_| format!("Invalid {key}"))?;
            if (!allow_zero && duration.is_zero())
                || std::time::Instant::now().checked_add(duration).is_none()
            {
                return Err(format!("Invalid {key}"));
            }
            Ok(duration)
        };
        let catalog_wait = seconds("DYNAMICAL_API_CATALOG_WAIT_SECONDS", "10", true)?;
        let drain_timeout = seconds("DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS", "30", false)?;
        let finalization_timeout =
            seconds("DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS", "15", false)?;
        let total = drain_timeout
            .checked_add(finalization_timeout)
            .ok_or("Shutdown budget exceeds clock capacity")?;
        if std::time::Instant::now().checked_add(total).is_none() {
            return Err("Shutdown budget exceeds clock capacity".into());
        }
        let clients = positive("DYNAMICAL_API_RATE_LIMIT_CLIENTS", "10000")? as usize;
        let heartbeat = get("DYNAMICAL_API_HEARTBEAT_SECONDS", "30")
            .parse::<f64>()
            .unwrap_or(30.0);
        let max_connections = positive("DYNAMICAL_API_MAX_CONNECTIONS", "1024")?;
        let max_connections = usize::try_from(max_connections)
            .ok()
            .filter(|n| *n <= tokio::sync::Semaphore::MAX_PERMITS)
            .ok_or("DYNAMICAL_API_MAX_CONNECTIONS exceeds semaphore capacity")?;
        let main_dataset_ttl_seconds = get("DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS", "30")
            .parse::<f64>()
            .map_err(|_| "Invalid DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS")?;
        if !main_dataset_ttl_seconds.is_finite() || main_dataset_ttl_seconds < 0. {
            return Err(
                "DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS must be finite and nonnegative".into(),
            );
        }
        Ok(Self {
            release,
            public_scheme,
            catalog_wait,
            drain_timeout,
            finalization_timeout,
            main_dataset_ttl_seconds,
            origin_secret,
            max_connections,
            header_read_timeout: timeout("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "10")?,
            body_read_timeout: timeout("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "30")?,
            keep_alive_timeout: timeout("DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS", "5")?,
            rate: RateProfile {
                requests_per_minute: positive(
                    "DYNAMICAL_API_RATE_LIMIT_REQUESTS_PER_MINUTE",
                    "1200",
                )?,
                burst: positive("DYNAMICAL_API_RATE_LIMIT_BURST", "120")?,
                max_clients: clients,
            },
            query_rate: RateProfile {
                requests_per_minute: positive(
                    "DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE",
                    "180",
                )?,
                burst: positive("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "30")?,
                max_clients: clients,
            },
            compute_slots: positive("DYNAMICAL_API_COMPUTE_SLOTS", "6")? as usize,
            busy_retry_after: positive("DYNAMICAL_API_BUSY_RETRY_AFTER_SECONDS", "5")?,
            max_response_values: number("DYNAMICAL_API_MAX_RESPONSE_VALUES", "100000")?,
            max_read_values: number("DYNAMICAL_API_MAX_READ_VALUES", "450000000")?,
            timing_sample_rate: get("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE", "0.1")
                .parse()
                .map_err(|_| "Invalid DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE")?,
            heartbeat_seconds: (heartbeat.is_finite() && heartbeat > 0.0).then_some(heartbeat),
            dataset_cache_entries: number("DYNAMICAL_API_DATASET_CACHE_ENTRIES", "32")? as usize,
            stac_url: get(
                "DYNAMICAL_API_STAC_URL",
                "https://stac.dynamical.org/catalog.json",
            ),
            analytics_enabled: values
                .get("POSTHOG_PROJECT_TOKEN")
                .is_some_and(|v| !v.is_empty()),
            error_reporting_enabled: values.get("SENTRY_DSN").is_some_and(|v| !v.is_empty()),
            container: get("MODAL_TASK_ID", "local"),
            #[cfg(feature = "harness")]
            fixture_store_base: values.get("DYNAPI_HARNESS_STORE_BASE_URL").cloned(),
            #[cfg(feature = "harness")]
            fixed_clock: values
                .get("DYNAPI_HARNESS_FIXED_CLOCK")
                .is_some_and(|v| v == "1"),
        })
    }
}
