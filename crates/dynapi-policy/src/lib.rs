//! Portable API decisions. Time and synchronization boundaries are explicit;
//! this crate never reads an environment, performs I/O, or consults a wall clock.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub trait Clock: Send + Sync {
    fn now(&self) -> f64;
}
impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> f64 {
        (**self).now()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RateProfile {
    pub requests_per_minute: u64,
    pub burst: u64,
    pub max_clients: usize,
}
struct Bucket {
    tokens: f64,
    updated_at: f64,
    touched: u64,
}
pub struct RateLimiter<C> {
    clock: C,
    profile: RateProfile,
    buckets: BTreeMap<String, Bucket>,
    lru: BTreeMap<u64, String>,
    serial: u64,
}
impl<C: Clock> RateLimiter<C> {
    pub fn new(profile: RateProfile, clock: C) -> Result<Self, &'static str> {
        if profile.requests_per_minute == 0 || profile.burst == 0 || profile.max_clients == 0 {
            return Err("Rate limiter bounds must be positive");
        }
        Ok(Self {
            clock,
            profile,
            buckets: BTreeMap::new(),
            lru: BTreeMap::new(),
            serial: 0,
        })
    }
    /// Caller serializes checks. A rejected request still touches its bucket.
    pub fn check(&mut self, client: &str) -> Option<u64> {
        let now = self.clock.now();
        let rate = self.profile.requests_per_minute as f64 / 60.0;
        let mut bucket = self.buckets.remove(client).unwrap_or(Bucket {
            tokens: self.profile.burst as f64,
            updated_at: now,
            touched: 0,
        });
        self.lru.remove(&bucket.touched);
        bucket.tokens = (bucket.tokens + (now - bucket.updated_at).max(0.0) * rate)
            .min(self.profile.burst as f64);
        bucket.updated_at = now;
        self.serial += 1;
        bucket.touched = self.serial;
        self.lru.insert(self.serial, client.into());
        let retry = if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            Some((((1.0 - bucket.tokens) / rate).ceil() as u64).max(1))
        };
        self.buckets.insert(client.into(), bucket);
        while self.buckets.len() > self.profile.max_clients {
            if let Some((_, key)) = self.lru.pop_first() {
                self.buckets.remove(&key);
            }
        }
        retry
    }
    pub fn clients(&self) -> usize {
        self.buckets.len()
    }
}

struct Slots {
    serial: u64,
    held: BTreeMap<u64, f64>,
}
pub struct ComputeGate<C> {
    clock: C,
    capacity: usize,
    retry_after: u64,
    slots: Arc<Mutex<Slots>>,
}
#[derive(Debug, PartialEq)]
pub struct GateSnapshot {
    pub in_use: usize,
    pub capacity: usize,
    pub oldest_age_seconds: Option<f64>,
}
/// Ownership follows the work, including a worker whose caller has cancelled.
/// There is no public early-release method.
pub struct Permit {
    slots: Arc<Mutex<Slots>>,
    serial: u64,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.slots.lock().unwrap().held.remove(&self.serial);
    }
}
impl<C: Clock> ComputeGate<C> {
    pub fn new(capacity: usize, retry_after: u64, clock: C) -> Result<Self, &'static str> {
        if capacity == 0 || retry_after == 0 {
            return Err("Compute slots and busy retry delay must be positive");
        }
        Ok(Self {
            clock,
            capacity,
            retry_after,
            slots: Arc::new(Mutex::new(Slots {
                serial: 0,
                held: BTreeMap::new(),
            })),
        })
    }
    pub fn try_acquire(&self) -> Option<Permit> {
        let mut slots = self.slots.lock().unwrap();
        if slots.held.len() >= self.capacity {
            return None;
        }
        slots.serial += 1;
        let serial = slots.serial;
        slots.held.insert(serial, self.clock.now());
        Some(Permit {
            slots: self.slots.clone(),
            serial,
        })
    }
    pub fn retry_after(&self) -> u64 {
        self.retry_after
    }
    pub fn snapshot(&self) -> GateSnapshot {
        let slots = self.slots.lock().unwrap();
        GateSnapshot {
            in_use: slots.held.len(),
            capacity: self.capacity,
            oldest_age_seconds: slots
                .held
                .values()
                .copied()
                .reduce(f64::min)
                .map(|oldest| (self.clock.now() - oldest).max(0.0)),
        }
    }
}

/// Python str.strip includes these four controls beyond Unicode White_Space.
pub fn python_strip(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}
pub fn cf_worker(value: Option<&str>) -> Option<String> {
    let value = python_strip(value.unwrap_or(""));
    (!value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c)))
    .then(|| value.chars().take(100).collect())
}
pub fn client_key(
    peer: &str,
    authenticated: bool,
    connecting_ip: Option<&str>,
    worker: Option<&str>,
) -> String {
    if !authenticated {
        return peer.into();
    }
    let ip = connecting_ip.unwrap_or(peer);
    match cf_worker(worker) {
        Some(worker) => format!("{ip}|{worker}"),
        None => ip.into(),
    }
}
pub fn query_limited(path: &str) -> bool {
    matches!(
        path,
        "/v1/forecasts" | "/v1/forecasts/runs" | "/v1/analyses"
    )
}
pub const EXPOSE_HEADERS: &str = "ETag, Retry-After, X-Request-Id, Server-Timing, X-Dynamical-Dataset-Cache, X-Dynamical-Work-Units";
pub const ALLOW_HEADERS: &str =
    "Accept, Accept-Language, Content-Language, Content-Type, content-type";
pub type Headers = Vec<(&'static str, String)>;
#[derive(Debug, PartialEq)]
pub struct Preflight {
    pub status: u16,
    pub body: String,
    pub headers: Headers,
}
pub fn preflight(
    method: &str,
    origin: bool,
    requested_method: Option<&str>,
    requested_headers: Option<&str>,
    private_network: bool,
) -> Option<Preflight> {
    if method != "OPTIONS" || !origin {
        return None;
    }
    let requested_method = requested_method?;
    let mut failures = Vec::new();
    if !["GET", "HEAD", "POST", "OPTIONS"].contains(&requested_method) {
        failures.push("method");
    }
    if requested_headers.is_some_and(|headers| {
        headers.to_lowercase().split(',').any(|h| {
            ![
                "accept",
                "accept-language",
                "content-language",
                "content-type",
            ]
            .contains(&python_strip(h))
        })
    }) {
        failures.push("headers");
    }
    if private_network {
        failures.push("private-network");
    }
    Some(Preflight {
        status: if failures.is_empty() { 200 } else { 400 },
        body: if failures.is_empty() {
            "OK".into()
        } else {
            format!("Disallowed CORS {}", failures.join(", "))
        },
        headers: vec![
            ("access-control-allow-origin", "*".into()),
            (
                "access-control-allow-methods",
                "GET, HEAD, POST, OPTIONS".into(),
            ),
            ("access-control-max-age", "86400".into()),
            ("access-control-allow-headers", ALLOW_HEADERS.into()),
        ],
    })
}
pub fn simple_cors(origin: bool, server_error: bool) -> Headers {
    if !origin {
        return vec![];
    }
    vec![
        ("access-control-allow-origin", "*".into()),
        (
            "access-control-expose-headers",
            if server_error {
                "X-Request-Id"
            } else {
                EXPOSE_HEADERS
            }
            .into(),
        ),
    ]
}
pub fn cache_control(method: &str, path: &str, status: u16) -> Option<&'static str> {
    if path == "/robots.txt" && status == 200 && matches!(method, "GET" | "HEAD") {
        return Some("public, max-age=300");
    }
    if method != "GET" || status != 200 {
        return None;
    }
    if path == "/v1/forecasts/runs" {
        Some("public, max-age=30, stale-while-revalidate=120")
    } else if path == "/v1/data-products" || path.starts_with("/v1/data-products/") {
        Some("public, max-age=300, stale-while-revalidate=600")
    } else {
        None
    }
}
pub const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

#[derive(Default, Debug)]
pub struct ServerTimings {
    pub read_values: u64,
    pub durations_ms: Vec<(String, f64)>,
    pub descriptions: Vec<(String, BTreeSet<String>)>,
    pub counters: BTreeMap<String, u64>,
}
impl ServerTimings {
    pub fn add(&mut self, name: &str, duration_ms: f64) {
        if let Some((_, duration)) = self.durations_ms.iter_mut().find(|(key, _)| key == name) {
            *duration += duration_ms;
        } else {
            self.durations_ms.push((name.into(), duration_ms));
        }
    }
    pub fn describe(&mut self, name: &str, value: &str) {
        if let Some((_, values)) = self.descriptions.iter_mut().find(|(key, _)| key == name) {
            values.insert(value.into());
        } else {
            self.descriptions
                .push((name.into(), BTreeSet::from([value.into()])));
        }
    }
    pub fn description(&self, name: &str) -> Option<String> {
        self.descriptions
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, values)| values.iter().cloned().collect::<Vec<_>>().join("|"))
    }
    pub fn count(&mut self, name: &str, count: u64) {
        *self.counters.entry(name.into()).or_default() += count;
    }
    pub fn header(&self) -> String {
        let mut values = self
            .durations_ms
            .iter()
            .map(|(name, duration)| format!("{name};dur={duration:.3}"))
            .collect::<Vec<_>>();
        values.extend(
            self.descriptions
                .iter()
                .map(|(name, _)| format!("{name};desc=\"{}\"", self.description(name).unwrap())),
        );
        values.join(", ")
    }
}
