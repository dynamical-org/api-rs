//! Caller-location decisions for catalog-declared proxy reads.
use crate::{SharedClock, config::Config};
use dynapi_catalog::Collection;
use futures_util::FutureExt;
use hyper::{HeaderMap, header::HeaderValue};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

pub(crate) const NO_STORE: &str = "private, no-store";
pub(crate) const UNAVAILABLE: &str = "WeatherNext location check is unavailable; retry later";
const BLOCKED: &str = "WeatherNext data is not available from this service in your location.";
const URL: &str = "https://wn.dynamical.org/geo-check";
const TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_SECONDS: f64 = 300.;
const CACHE_ENTRIES: usize = 1024;
const CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IN_FLIGHT: usize = 16;
const MAX_DETAIL_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct Secret(HeaderValue);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Secret {
    pub(crate) fn parse(value: &str) -> Result<Option<Self>, String> {
        if value.is_empty() {
            return Ok(None);
        }
        let mut header =
            HeaderValue::from_str(value).map_err(|_| "Invalid DYNAMICAL_API_GEO_CHECK_SECRET")?;
        header.set_sensitive(true);
        Ok(Some(Self(header)))
    }
}

pub(crate) fn required(product: &Collection) -> bool {
    product.virtual_chunk_containers().iter().any(|container| {
        container.kind == "http"
            && reqwest::Url::parse(&container.url_prefix)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .as_deref()
                == Some("wn.dynamical.org")
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Location {
    country: Option<String>,
    region: Option<String>,
}
impl Location {
    fn from_headers(headers: &HeaderMap, trusted: bool) -> Result<Self, ()> {
        let read = |name| {
            if !trusted {
                return Ok(None);
            }
            headers
                .get(name)
                .filter(|v| !v.is_empty())
                .map(|v| v.to_str().map(str::to_owned))
                .transpose()
                .map_err(|_| ())
        };
        Ok(Self {
            country: read("x-dynamical-viewer-country")?,
            region: read("x-dynamical-viewer-region-code")?,
        })
    }
    fn bytes(&self) -> usize {
        self.country.as_ref().map_or(0, String::capacity)
            + self.region.as_ref().map_or(0, String::capacity)
    }
}
#[derive(Clone, Debug)]
struct Answer {
    allowed: bool,
    detail: String,
    expires: f64,
}
type Decision = Result<Answer, ()>;
#[derive(Default)]
struct State {
    answers: BTreeMap<Location, Answer>,
    flights: BTreeMap<Location, watch::Receiver<Option<Decision>>>,
}
#[derive(Clone)]
pub(crate) struct GeoGate(Arc<Inner>);
struct Inner {
    secret: Option<Secret>,
    client: reqwest::Client,
    url: String,
    clock: SharedClock,
    state: Mutex<State>,
}
impl GeoGate {
    pub(crate) fn new(config: &Config, clock: SharedClock) -> Result<Self, String> {
        #[cfg(feature = "harness")]
        let url = config.geo_check_url.as_deref().unwrap_or(URL);
        #[cfg(not(feature = "harness"))]
        let url = URL;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(TIMEOUT)
            .timeout(TIMEOUT)
            .read_timeout(TIMEOUT)
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .no_proxy()
            .build()
            .map_err(|_| "Cannot initialize location-check HTTP client")?;
        Ok(Self(Arc::new(Inner {
            secret: config.geo_check_secret.clone(),
            client,
            url: url.into(),
            clock,
            state: Mutex::new(State::default()),
        })))
    }
    pub(crate) async fn check(&self, headers: &HeaderMap, trusted: bool) -> Result<(), Failure> {
        let key = Location::from_headers(headers, trusted).map_err(|_| Failure::Unavailable)?;
        let answer = self.answer(key).await.map_err(|_| Failure::Unavailable)?;
        if answer.allowed {
            Ok(())
        } else {
            Err(Failure::Blocked(answer.detail))
        }
    }
    async fn answer(&self, key: Location) -> Decision {
        if self.0.secret.is_none() {
            return Err(());
        }
        let mut receiver = {
            let mut state = self.0.state.lock().unwrap();
            let now = self.0.clock.now();
            if let Some(answer) = state
                .answers
                .get(&key)
                .filter(|answer| answer.expires > now)
            {
                return Ok(answer.clone());
            }
            if let Some(receiver) = state.flights.get(&key) {
                receiver.clone()
            } else {
                if state.flights.len() >= MAX_IN_FLIGHT {
                    return Err(());
                }
                let (sender, receiver) = watch::channel(None);
                state.flights.insert(key.clone(), receiver.clone());
                let inner = self.0.clone();
                let deadline = tokio::time::Instant::now() + TIMEOUT;
                tokio::spawn(async move {
                    let answer = tokio::time::timeout_at(
                        deadline,
                        std::panic::AssertUnwindSafe(inner.ask(&key, now)).catch_unwind(),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(Err(()));
                    {
                        let mut state = inner.state.lock().unwrap();
                        if let Ok(answer) = &answer {
                            state
                                .answers
                                .retain(|_, value| value.expires > inner.clock.now());
                            let charge = |key: &Location, value: &Answer| {
                                key.bytes()
                                    + value.detail.capacity()
                                    + std::mem::size_of::<(Location, Answer)>()
                                    + 128
                            };
                            let bytes = charge(&key, answer);
                            let retained: usize =
                                state.answers.iter().map(|(k, a)| charge(k, a)).sum();
                            if state.answers.len() >= CACHE_ENTRIES
                                || retained.saturating_add(bytes) > CACHE_BYTES
                            {
                                state.answers.clear();
                            }
                            if bytes <= CACHE_BYTES {
                                state.answers.insert(key.clone(), answer.clone());
                            }
                        }
                        state.flights.remove(&key);
                    }
                    sender.send_replace(Some(answer));
                });
                receiver
            }
        };
        loop {
            if let Some(answer) = receiver.borrow().clone() {
                return answer;
            }
            receiver.changed().await.map_err(|_| ())?;
        }
    }
}
impl Inner {
    async fn ask(&self, key: &Location, started: f64) -> Decision {
        let secret = self.secret.as_ref().ok_or(())?;
        let mut request = self
            .client
            .get(&self.url)
            .header("x-geo-check-secret", secret.0.clone());
        if let Some(country) = &key.country {
            request = request.header("x-geo-check-country", country);
        }
        if let Some(region) = &key.region {
            request = request.header("x-geo-check-region-code", region);
        }
        let mut response = request.send().await.map_err(|_| ())?;
        let expires = started + CACHE_SECONDS;
        match response.status().as_u16() {
            204 => Ok(Answer {
                allowed: true,
                detail: String::new(),
                expires,
            }),
            451 => {
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_DETAIL_BYTES as u64)
                {
                    return Err(());
                }
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
                    if chunk.len() > MAX_DETAIL_BYTES.saturating_sub(body.len()) {
                        return Err(());
                    }
                    body.extend_from_slice(&chunk);
                }
                if body
                    .windows(secret.0.as_bytes().len())
                    .any(|value| value == secret.0.as_bytes())
                {
                    return Err(());
                }
                let decoded = String::from_utf8_lossy(&body);
                if decoded.len() > MAX_DETAIL_BYTES {
                    return Err(());
                }
                let detail = decoded.trim_matches(dynapi_contract::output::python_whitespace);
                Ok(Answer {
                    allowed: false,
                    detail: if detail.is_empty() {
                        BLOCKED.into()
                    } else {
                        detail.into()
                    },
                    expires,
                })
            }
            _ => Err(()),
        }
    }
}
pub(crate) enum Failure {
    Blocked(String),
    Unavailable,
}
impl Failure {
    pub(crate) fn response(self) -> crate::HttpResponse {
        let mut response = match self {
            Self::Blocked(detail) => crate::detail(451, &detail),
            Self::Unavailable => {
                let mut response = crate::detail(503, UNAVAILABLE);
                response
                    .headers_mut()
                    .insert("retry-after", HeaderValue::from_static("5"));
                response
            }
        };
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static(NO_STORE));
        response
    }
}

#[cfg(all(test, feature = "harness"))]
#[path = "geo_gate_tests.rs"]
mod tests;
