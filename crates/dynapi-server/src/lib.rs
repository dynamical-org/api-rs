//! Native HTTP shell and bounded materialized point reads.
pub mod config;
mod data;
pub mod telemetry;
pub mod transport;

use bytes::Bytes;
use config::Config;
use dynapi_contract::{ValidationResult, validate_forecast};
use dynapi_policy::{
    Clock, ComputeGate, RateLimiter, cache_control, cf_worker, client_key, preflight,
    query_limited, simple_cors,
};
use futures_util::FutureExt;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::Incoming,
    header::{HeaderName, HeaderValue},
};
use serde_json::json;
use sha2::Sha256;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};
use subtle::ConstantTimeEq;
use telemetry::{Stream, Telemetry};

pub const BODY_LIMIT: usize = 16 * 1024 * 1024;
pub const INTERNAL_STUB: &str = "Internal stub: data route handlers are not implemented yet";
pub const ROBOTS: &[u8] = include_bytes!("../robots.txt");
include!(concat!(env!("OUT_DIR"), "/robots_metadata.rs"));
const RATE_DETAIL: &str =
    "Request rate limit exceeded; contact feedback@dynamical.org if you need higher limits";

pub struct Monotonic(Instant);
impl Default for Monotonic {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for Monotonic {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}
#[cfg(feature = "harness")]
pub struct FixedClock;
#[cfg(feature = "harness")]
impl Clock for FixedClock {
    fn now(&self) -> f64 {
        0.0
    }
}
type SharedClock = Arc<dyn Clock>;
pub type HttpResponse = Response<Full<Bytes>>;

#[derive(Debug)]
pub enum RequestError {
    BodyTimeout,
    BodyFraming(hyper::Error),
    BodyTooLarge,
}
impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BodyTimeout => f.write_str("request body deadline expired"),
            Self::BodyFraming(error) => write!(f, "invalid HTTP body framing: {error}"),
            Self::BodyTooLarge => f.write_str("request body exceeds limit"),
        }
    }
}
impl std::error::Error for RequestError {}

async fn collect_body(
    body: Incoming,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, RequestError> {
    tokio::time::timeout(timeout, async move {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(RequestError::BodyFraming)?;
            if let Ok(data) = frame.into_data() {
                if data.len() > BODY_LIMIT.saturating_sub(bytes.len()) {
                    return Err(RequestError::BodyTooLarge);
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| RequestError::BodyTimeout)?
}

pub struct App {
    pub config: Config,
    pub gate: ComputeGate<SharedClock>,
    rate: Mutex<RateLimiter<SharedClock>>,
    query_rate: Mutex<RateLimiter<SharedClock>>,
    sink: Arc<dyn Telemetry>,
    started: Instant,
    data: Arc<data::DataService>,
    robots_headers: Vec<(&'static str, String)>,
}
impl App {
    pub fn new(
        config: Config,
        clock: SharedClock,
        sink: Arc<dyn Telemetry>,
    ) -> Result<Self, String> {
        let robots_headers = vec![
            ("accept-ranges", "bytes".into()),
            ("last-modified", ROBOTS_LAST_MODIFIED.into()),
            ("etag", ROBOTS_ETAG.into()),
        ];
        let data = Arc::new(data::DataService::new(&config, clock.clone())?);
        Ok(Self {
            data,
            gate: ComputeGate::new(config.compute_slots, config.busy_retry_after, clock.clone())?,
            rate: Mutex::new(RateLimiter::new(config.rate, clock.clone())?),
            query_rate: Mutex::new(RateLimiter::new(config.query_rate, clock)?),
            config,
            sink,
            started: Instant::now(),
            robots_headers,
        })
    }
    pub async fn serve(
        self: Arc<Self>,
        request: Request<Incoming>,
        peer: String,
        local: SocketAddr,
    ) -> Result<HttpResponse, RequestError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let started = Instant::now();
        let headers = request.headers().clone();
        let method = request.method().as_str().to_owned();
        let path = percent_encoding::percent_decode_str(request.uri().path())
            .decode_utf8_lossy()
            .into_owned();
        let policy_path = middleware_url_path(&path);
        let query = request.uri().query().unwrap_or("").to_owned();
        let header = |name: &str| headers.get(name).map(|v| latin1(v.as_bytes()));
        let origin = header("origin");
        if let Some(cors) = preflight(
            &method,
            origin.is_some(),
            header("access-control-request-method").as_deref(),
            header("access-control-request-headers").as_deref(),
            headers.contains_key("access-control-request-private-network"),
        ) {
            let mut response = response(
                cors.status,
                cors.body.into_bytes(),
                Some("text/plain; charset=utf-8"),
            );
            add_headers(&mut response, cors.headers);
            response
                .headers_mut()
                .insert("x-request-id", id.parse().unwrap());
            return Ok(response);
        }
        let authenticated = self.config.origin_secret.as_ref().is_some_and(|secret| {
            // compare_digest(str, str) rejects non-ASCII headers as an unhandled error.
            header("x-dynamical-origin-auth")
                .is_some_and(|value| bool::from(value.as_bytes().ct_eq(secret.as_bytes())))
        });
        let non_ascii_auth = self
            .config
            .origin_secret
            .as_ref()
            .is_some_and(|s| !s.is_ascii())
            || self.config.origin_secret.is_some()
                && header("x-dynamical-origin-auth").is_some_and(|v| !v.is_ascii());
        let usage = Arc::new(Mutex::new(data::Usage::default()));
        let mut terminal = RequestGuard {
            usage: usage.clone(),
            app: self.clone(),
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            headers: headers.clone(),
            id: id.clone(),
            started,
            authenticated,
            active: !non_ascii_auth,
        };
        let mut analytics = true;
        let response = if non_ascii_auth {
            analytics = false;
            server_error()
        } else if self.config.origin_secret.is_some() && !authenticated {
            analytics = false;
            detail(403, "Forbidden")
        } else {
            let limited = query_limited(&policy_path) || policy_path.starts_with("/v1/data/");
            let retry = if limited {
                let key = client_key(
                    &peer,
                    authenticated,
                    header("cf-connecting-ip").as_deref(),
                    header("cf-worker").as_deref(),
                );
                if query_limited(&policy_path) {
                    self.query_rate.lock().unwrap().check(&key)
                } else {
                    self.rate.lock().unwrap().check(&key)
                }
            } else {
                None
            };
            if let Some(retry) = retry {
                let mut result = detail(429, RATE_DETAIL);
                result
                    .headers_mut()
                    .insert("retry-after", retry.to_string().parse().unwrap());
                result
            } else {
                std::panic::AssertUnwindSafe(self.route(
                    request,
                    &method,
                    &path,
                    local,
                    usage.clone(),
                ))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Ok(server_error()))?
            }
        };
        let mut response = response;
        let status = response.status().as_u16();
        usage.lock().unwrap().response_bytes = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()?.parse().ok());
        if status == 500 {
            // Hyper flushes this response and closes before dispatching a pipeline.
            response
                .headers_mut()
                .insert("connection", "close".parse().unwrap());
        }
        terminal.active = false;
        if !non_ascii_auth {
            self.record(
                &method,
                &path,
                &query,
                &headers,
                Some(status),
                false,
                authenticated,
                analytics,
                &id,
                started.elapsed().as_secs_f64() * 1000.0,
                &usage.lock().unwrap(),
            );
        }
        if let Some(cache) = cache_control(&method, &policy_path, status) {
            response
                .headers_mut()
                .insert("cache-control", cache.parse().unwrap());
        }
        add_headers(&mut response, simple_cors(origin.is_some(), status == 500));
        response
            .headers_mut()
            .insert("x-request-id", id.parse().unwrap());
        Ok(response)
    }
    async fn route(
        &self,
        request: Request<Incoming>,
        method: &str,
        path: &str,
        local: SocketAddr,
        usage: data::SharedUsage,
    ) -> Result<HttpResponse, RequestError> {
        // Hyper exposes scheme/authority, but not the original wire target.
        // Reject targets outside origin-form rather than routing their path alone.
        if request.uri().scheme().is_some()
            || request.uri().authority().is_some()
            || request.uri().path() == "*"
        {
            let mut response = detail(400, "Invalid request target");
            response
                .headers_mut()
                .insert("connection", "close".parse().unwrap());
            return Ok(response);
        }
        #[cfg(feature = "harness")]
        if request
            .headers()
            .get("x-dynapi-harness-panic")
            .is_some_and(|v| v == "1")
        {
            panic!("injected harness handler panic");
        }
        let Some(route) = find_route(path) else {
            let redirected = if path.ends_with('/') {
                path.trim_end_matches('/').to_owned()
            } else {
                format!("{path}/")
            };
            if path != "/" && find_route(&redirected).is_some() {
                let host = scope_authority(request.headers().get("host"), local);
                let query = request
                    .uri()
                    .query()
                    .filter(|q| !q.is_empty())
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                return Ok(redirect(
                    307,
                    &format!("http://{host}{}{query}", quote_location(&redirected)),
                ));
            }
            return Ok(detail(404, "Not Found"));
        };
        if method == "OPTIONS" && route.cors {
            return Ok(response(204, vec![], None));
        }
        if !route.methods.contains(&method) {
            let mut result = detail(405, "Method Not Allowed");
            result
                .headers_mut()
                .insert("allow", route.methods.join(", ").parse().unwrap());
            return Ok(result);
        }
        Ok(match route.template {
            "/" => redirect(301, "https://dynamical.org/api/"),
            "/healthz" => response(
                200,
                b"{\"status\":\"ok\"}".to_vec(),
                Some("application/json"),
            ),
            "/robots.txt" => {
                let mut result = response(200, ROBOTS.to_vec(), Some("text/plain; charset=utf-8"));
                add_headers(&mut result, self.robots_headers.clone());
                result
            }
            "/v1/forecasts" => {
                let content_type = request
                    .headers()
                    .get("content-type")
                    .map(|v| latin1(v.as_bytes()));
                let bytes =
                    match collect_body(request.into_body(), self.config.body_read_timeout).await {
                        Ok(bytes) => bytes,
                        Err(RequestError::BodyTooLarge) => {
                            return Ok(detail(413, "Request body exceeds 16777216 bytes"));
                        }
                        Err(error) => return Err(error),
                    };
                // Match FastAPI strict Content-Type behavior, including an absent header.
                match validate_forecast(&bytes, content_type.as_deref()) {
                    ValidationResult::Invalid(invalid) => response(
                        invalid.status,
                        invalid.body,
                        Some(if invalid.status == 500 {
                            "text/plain; charset=utf-8"
                        } else {
                            "application/json"
                        }),
                    ),
                    ValidationResult::Valid(payload) => {
                        self.data.forecasts(payload, &self.gate, usage).await
                    }
                }
            }
            "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}" => {
                let params: Vec<_> = request
                    .uri()
                    .query()
                    .unwrap_or("")
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|pair| {
                        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                        let decode = |v: &str| {
                            percent_encoding::percent_decode_str(&v.replace('+', " "))
                                .decode_utf8_lossy()
                                .into_owned()
                        };
                        (decode(key), decode(value))
                    })
                    .collect();
                match dynapi_contract::validation::validate_canonical(&params) {
                    Err(invalid) => response(
                        invalid.status,
                        invalid.body,
                        Some(if invalid.status == 500 {
                            "text/plain; charset=utf-8"
                        } else {
                            "application/json"
                        }),
                    ),
                    Ok(params) => {
                        let parts: Vec<_> = path.split('/').collect();
                        self.data
                            .canonical(
                                parts[3],
                                parts[4].into(),
                                parts[6],
                                params,
                                &self.gate,
                                usage,
                            )
                            .await
                    }
                }
            }
            _ => self.stub(),
        })
    }
    #[cfg(feature = "harness")]
    pub fn inject_read_failure(&self, error: dynapi_reader::ReaderError) {
        self.data.inject_read_failure(error);
    }
    pub fn warm_catalog(&self) {
        self.data.warm();
    }
    fn stub(&self) -> HttpResponse {
        let Some(_permit) = self.gate.try_acquire() else {
            let retry = self.gate.retry_after();
            let mut result = detail(503, &format!("Server busy; retry after {retry} seconds"));
            result
                .headers_mut()
                .insert("retry-after", retry.to_string().parse().unwrap());
            return result;
        };
        // Never consumed as a public result by the differential harness.
        response(
            501,
            INTERNAL_STUB.as_bytes().to_vec(),
            Some("text/plain; charset=utf-8"),
        )
    }
    fn emit(&self, stream: Stream, record: serde_json::Value) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.sink.emit(stream, record)
        }));
    }
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &hyper::HeaderMap,
        status: Option<u16>,
        cancelled: bool,
        authenticated: bool,
        analytics: bool,
        id: &str,
        duration_ms: f64,
        usage: &data::Usage,
    ) {
        let header = |name: &str| headers.get(name).map(|v| latin1(v.as_bytes()));
        let ua = header("user-agent").unwrap_or_default();
        let lowered = ua.to_lowercase();
        let kind = if lowered.starts_with("dynamical-") {
            "internal"
        } else if lowered.contains("uptime bot") || lowered.contains("sentry uptime") {
            "uptime_probe"
        } else {
            "unlabeled"
        };
        let caller = if authenticated {
            self.config.origin_secret.as_ref().and_then(|secret| {
                header("cf-connecting-ip")
                    .filter(|ip| !ip.is_empty())
                    .map(|ip| {
                        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
                        mac.update(ip.as_bytes());
                        format!("{:x}", mac.finalize().into_bytes())[..16].to_owned()
                    })
            })
        } else {
            None
        };
        let route = find_route(path);
        let matched = route.as_ref().filter(|r| {
            if !matches!(status, Some(403 | 429)) {
                return true;
            }
            // FastAPI 0.141 wraps include_router routes in _IncludedRouter.
            // telemetry.route_template's fallback cannot see their path before
            // routing. Direct application routes (including OPTIONS) are visible.
            let direct = matches!(
                r.template,
                "/" | "/robots.txt"
                    | "/healthz"
                    | "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}"
            ) || (method == "OPTIONS" && r.cors);
            direct && (r.methods.contains(&method) || (method == "OPTIONS" && r.cors))
        });
        if status == Some(200) && usage.work_units.is_some() {
            self.emit(Stream::Terminal, json!({
                "event": "api_request", "route": if route.as_ref().is_some_and(|r| r.template == "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}") { "/v1/data/:canonical" } else { matched.map(|r| r.template).unwrap_or("unrouted") },
                "containerAgeS": self.started.elapsed().as_secs(),
                "dataProductIds": usage.products, "queryCount": usage.query_count,
                "workUnits": usage.work_units, "responseBytes": usage.response_bytes,
                "datasetCache": usage.dataset_cache, "timingsMs": usage.timings
            }));
        }
        if cancelled || status.is_some_and(|s| s >= 400) {
            self.emit(Stream::Terminal, json!({
                "event": "api_failure", "route": matched.map(|r| r.template).unwrap_or("unrouted"),
                "method": method, "status": status, "cancelled": cancelled,
                "durationMs": (duration_ms * 1000.0).round_ties_even() / 1000.0,
                "requestId": id, "container": self.config.container,
                "containerAgeS": self.started.elapsed().as_secs(), "caller": caller,
                "clientKind": kind, "cfWorker": cf_worker(header("cf-worker").as_deref()),
                "dataProductIds": null, "queryCount": null, "readValues": null, "timingsMs": {}, "counters": {}
            }));
        }
        let path_product = route
            .as_ref()
            .filter(|r| {
                status != Some(429)
                    && (r.template == "/v1/data-products/{dataProductId}"
                        || r.template
                            == "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}")
            })
            .and_then(|_| path.split('/').nth(3))
            .map(str::to_owned);
        let product = path_product.or_else(|| {
            query.rsplit('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                let decode = |v: &str| {
                    percent_encoding::percent_decode_str(&v.replace('+', " "))
                        .decode_utf8_lossy()
                        .into_owned()
                };
                (decode(key) == "dataProductId").then(|| decode(value))
            })
        });
        let products = product
            .filter(|p| !p.is_empty())
            .into_iter()
            .collect::<Vec<_>>();
        let mut products = if usage.products.is_empty() {
            products
        } else {
            usage.products.clone()
        };
        products.sort();
        products.dedup();
        products.truncate(10);
        if self.config.analytics_enabled
            && analytics
            && middleware_url_path(path).starts_with("/v1/")
        {
            self.emit(Stream::Analytics, json!({"event": "api_request", "distinct_id": caller.as_deref().unwrap_or("anonymous"), "properties": {
                "$process_person_profile": false, "route": if status == Some(429) { "unmatched" } else { route.as_ref().map(|r| r.template).unwrap_or("unmatched") },
                "method": method, "status": status, "duration_ms": (duration_ms * 10.0).round_ties_even() / 10.0,
                "client_kind": kind, "user_agent": ua.chars().take(120).collect::<String>(),
                "country": header("cf-ipcountry"), "data_product_ids": products, "query_count": usage.query_count,
                "work_units": usage.work_units, "dataset_cache": usage.dataset_cache, "response_bytes": usage.response_bytes, "timings_ms": usage.timings
            }}));
        }
    }
}
// Starlette URL(scope=...) uses this Host grammar, not URI authority or IP
// validation: ^([a-z0-9.-]+|\[[a-f0-9]*:[a-f0-9.:]+\])(?::[0-9]+)?$ (IGNORECASE).
fn starlette_host(host: &[u8]) -> bool {
    let end = if host.starts_with(b"[") {
        let Some(end) = host.iter().position(|b| *b == b']') else {
            return false;
        };
        end + 1
    } else {
        host.iter().position(|b| *b == b':').unwrap_or(host.len())
    };
    let (name, port) = host.split_at(end);
    if !port.is_empty()
        && !(port.starts_with(b":") && port.len() > 1 && port[1..].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    if name.starts_with(b"[") {
        let literal = &name[1..name.len() - 1];
        let Some(colon) = literal.iter().position(|b| *b == b':') else {
            return false;
        };
        literal[..colon].iter().all(u8::is_ascii_hexdigit)
            && !literal[colon + 1..].is_empty()
            && literal[colon + 1..]
                .iter()
                .all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b':'))
    } else {
        !name.is_empty()
            && name
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    }
}

fn scope_authority(host: Option<&HeaderValue>, local: SocketAddr) -> String {
    if let Some(host) = host.filter(|host| starlette_host(host.as_bytes())) {
        latin1(host.as_bytes())
    } else if local.port() == 80 {
        local.ip().to_string()
    } else {
        // Starlette interpolates the scope's server host directly, including IPv6.
        format!("{}:{}", local.ip(), local.port())
    }
}

#[test]
fn scope_authority_elides_only_the_http_listener_default_port() {
    for (local, expected) in [
        ("127.0.0.2:80", "127.0.0.2"),
        ("127.0.0.2:443", "127.0.0.2:443"),
        ("[::1]:80", "::1"),
        ("[::1]:8080", "::1:8080"),
    ] {
        let local = local.parse().unwrap();
        assert_eq!(scope_authority(None, local), expected);
        assert_eq!(
            scope_authority(Some(&HeaderValue::from_static("example%2Etest")), local),
            expected
        );
        assert_eq!(
            scope_authority(Some(&HeaderValue::from_static("test:80")), local),
            "test:80"
        );
    }
}

/// Starlette builds an absolute URL using the decoded ASGI path and the Host
/// header (or listener address), then urllib.parse.urlsplit extracts its path.
/// The authority keeps leading path controls/spaces out of URL-level lstrip;
/// only tab/CR/LF removal and the first query/fragment separator affect the path.
/// Preserve the original decoded path separately for routing and path params.
fn middleware_url_path(path: &str) -> String {
    path.chars()
        .take_while(|c| !matches!(c, '?' | '#'))
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect()
}
// A cancelled async request claims its terminal event immediately. A future
// data worker must own its compute Permit independently of this request guard.
struct RequestGuard {
    usage: data::SharedUsage,
    app: Arc<App>,
    method: String,
    path: String,
    query: String,
    headers: hyper::HeaderMap,
    id: String,
    started: Instant,
    authenticated: bool,
    active: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.active {
            self.app.record(
                &self.method,
                &self.path,
                &self.query,
                &self.headers,
                None,
                true,
                self.authenticated,
                false,
                &self.id,
                self.started.elapsed().as_secs_f64() * 1000.0,
                &self.usage.lock().unwrap(),
            );
        }
    }
}
fn latin1(value: &[u8]) -> String {
    value.iter().map(|c| char::from(*c)).collect()
}
fn response(status: u16, bytes: Vec<u8>, media: Option<&str>) -> HttpResponse {
    let mut result = Response::builder().status(status);
    if status != 204 {
        result = result.header("content-length", bytes.len());
    }
    if let Some(media) = media {
        result = result.header("content-type", media);
    }
    result.body(Full::new(Bytes::from(bytes))).unwrap()
}
fn detail(status: u16, text: &str) -> HttpResponse {
    response(
        status,
        dynapi_contract::output::detail(text),
        Some("application/json"),
    )
}
fn server_error() -> HttpResponse {
    response(
        500,
        b"Internal Server Error".to_vec(),
        Some("text/plain; charset=utf-8"),
    )
}
fn add_headers(result: &mut HttpResponse, headers: dynapi_policy::Headers) {
    for (key, value) in headers {
        if let Ok(value) = HeaderValue::from_bytes(
            &value
                .chars()
                .filter_map(|c| u8::try_from(c as u32).ok())
                .collect::<Vec<_>>(),
        ) {
            result
                .headers_mut()
                .insert(HeaderName::from_static(key), value);
        }
    }
}
fn redirect(status: u16, location: &str) -> HttpResponse {
    let mut result = response(status, vec![], None);
    result
        .headers_mut()
        .insert("location", location.parse().unwrap());
    result
}
fn quote_location(path: &str) -> String {
    const SAFE: &str = ":/%#?=@[]!$&'()*+,;";
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || SAFE.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
struct Route {
    template: &'static str,
    methods: &'static [&'static str],
    cors: bool,
}
fn find_route(path: &str) -> Option<Route> {
    let route = |template, methods, cors| {
        Some(Route {
            template,
            methods,
            cors,
        })
    };
    // Starlette's static route regex ends with $, matching before one final LF.
    // Dynamic parameters below still use the full decoded ASGI path.
    match path.strip_suffix('\n').unwrap_or(path) {
        "/" => route("/", &["GET", "HEAD"], false),
        "/robots.txt" => route("/robots.txt", &["GET", "HEAD"], false),
        "/healthz" => route("/healthz", &["GET"], false),
        "/openapi.json" => route("/openapi.json", &["GET", "HEAD"], false),
        "/docs" => route("/docs", &["GET", "HEAD"], false),
        "/docs/oauth2-redirect" => route("/docs/oauth2-redirect", &["GET", "HEAD"], false),
        "/redoc" => route("/redoc", &["GET", "HEAD"], false),
        "/v1/forecasts" => route("/v1/forecasts", &["POST"], true),
        "/v1/analyses" => route("/v1/analyses", &["POST"], true),
        "/v1/forecasts/runs" => route("/v1/forecasts/runs", &["GET"], true),
        "/v1/data-products" => route("/v1/data-products", &["GET"], true),
        _ => {
            let parts = path.split('/').collect::<Vec<_>>();
            if parts.len() == 4 && parts[..3] == ["", "v1", "data-products"] && !parts[3].is_empty()
            {
                route("/v1/data-products/{dataProductId}", &["GET"], true)
            } else if parts.len() == 7
                && parts[..3] == ["", "v1", "data"]
                && parts[5] == "points"
                && [parts[3], parts[4], parts[6]].iter().all(|p| !p.is_empty())
            {
                route(
                    "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}",
                    &["GET"],
                    true,
                )
            } else {
                None
            }
        }
    }
}
