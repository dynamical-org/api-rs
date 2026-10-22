//! Deterministic catalog refresh state. The native service drives these decisions;
//! tests/other runtimes can drive them with an injected monotonic clock.
use crate::{CatalogError, Clock, Collection};
use indexmap::IndexMap;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct CatalogConfig {
    pub timeout_seconds: f64,
    pub cache_ttl_seconds: f64,
    pub cold_wait_seconds: f64,
    pub load_deadline_seconds: f64,
}
impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 10.,
            cache_ttl_seconds: 300.,
            cold_wait_seconds: 20.,
            load_deadline_seconds: 30.,
        }
    }
}
impl CatalogConfig {
    pub fn validate(&self) -> Result<(), CatalogError> {
        if !self.cache_ttl_seconds.is_finite() || self.cache_ttl_seconds < 0. {
            return Err(CatalogError::new("cache_ttl_seconds must not be negative"));
        }
        if [
            self.timeout_seconds,
            self.cold_wait_seconds,
            self.load_deadline_seconds,
        ]
        .iter()
        .any(|s| !s.is_finite() || *s <= 0.)
        {
            return Err(CatalogError::new("Timeouts must be positive"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub collections: IndexMap<String, Collection>,
    pub loaded_at: f64,
    pub loaded_at_utc: String,
    pub origin: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Flight {
    pub generation: u64,
    pub deadline: f64,
}
#[derive(Clone, Debug)]
pub struct LoadDecision {
    pub snapshot: Option<Arc<Snapshot>>,
    pub flight: Option<Flight>,
    pub start: bool,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub loaded_at: Option<String>,
    pub age_seconds: Option<f64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u64,
    pub refreshing: bool,
    pub outstanding_workers: usize,
}
pub struct CatalogState<C> {
    clock: C,
    config: CatalogConfig,
    snapshot: Option<Arc<Snapshot>>,
    flight: Option<Flight>,
    generation: u64,
    closed: bool,
    last_error: Option<String>,
    consecutive_failures: u64,
    retry_at: f64,
}
impl<C: Clock> CatalogState<C> {
    pub fn new(clock: C, config: CatalogConfig) -> Result<Self, CatalogError> {
        config.validate()?;
        Ok(Self {
            clock,
            config,
            snapshot: None,
            flight: None,
            generation: 0,
            closed: false,
            last_error: None,
            consecutive_failures: 0,
            retry_at: 0.,
        })
    }
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.clone()
    }
    pub fn retry_at(&self) -> f64 {
        self.retry_at
    }
    pub fn flight(&self) -> Option<Flight> {
        self.flight
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    pub fn fresh(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| self.clock.now() - s.loaded_at < self.config.cache_ttl_seconds)
    }
    pub fn decide(&mut self) -> Result<LoadDecision, CatalogError> {
        if self.closed {
            return Err(CatalogError::new("STAC catalog is closed"));
        }
        let mut start = false;
        if !self.fresh() && self.flight.is_none() && self.clock.now() >= self.retry_at {
            self.generation += 1;
            self.flight = Some(Flight {
                generation: self.generation,
                deadline: self.clock.now() + self.config.load_deadline_seconds,
            });
            start = true;
        }
        Ok(LoadDecision {
            snapshot: self.snapshot.clone(),
            flight: self.flight,
            start,
        })
    }
    pub fn remaining(&self, flight: Flight) -> Result<f64, CatalogError> {
        let remaining = flight.deadline - self.clock.now();
        if self.closed || self.flight != Some(flight) || remaining <= 0. {
            return Err(CatalogError::new(
                "STAC catalog load deadline exceeded or flight cancelled",
            ));
        }
        Ok(remaining)
    }
    pub fn publish(
        &mut self,
        flight: Flight,
        collections: Vec<Collection>,
        loaded_at_utc: String,
    ) -> Result<(), CatalogError> {
        self.remaining(flight)?;
        self.snapshot = Some(Arc::new(Snapshot {
            collections: collections.into_iter().map(|c| (c.id.clone(), c)).collect(),
            loaded_at: self.clock.now(),
            loaded_at_utc,
            origin: "live",
        }));
        self.consecutive_failures = 0;
        self.last_error = None;
        self.retry_at = 0.;
        self.flight = None;
        Ok(())
    }
    /// RNG is unused for an initial cold failure or a retired generation.
    pub fn failure_jitter_bounds(&self, flight: Flight) -> Option<(f64, f64)> {
        if self.closed
            || self.flight != Some(flight)
            || (self.snapshot.is_none() && self.consecutive_failures == 0)
        {
            return None;
        }
        let base = (30. * 2_f64.powi(self.consecutive_failures.min(4) as i32)).min(300.);
        Some((0.8 * base, base))
    }
    /// Return true only for the first failure, so a transport can log once per
    /// outage without putting URLs, response bodies or credentials into status.
    pub fn fail(
        &mut self,
        flight: Flight,
        error: &CatalogError,
        rng: impl FnOnce(f64, f64) -> f64,
    ) -> bool {
        if self.closed || self.flight != Some(flight) {
            return false;
        }
        self.last_error = Some(format!(
            "STAC catalog refresh failed ({})",
            error.exception_type
        ));
        let warn = self.consecutive_failures == 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let delay = if self.snapshot.is_none() && self.consecutive_failures == 1 {
            5.
        } else {
            let base = (30. * 2_f64.powi((self.consecutive_failures - 1).min(4) as i32)).min(300.);
            rng(0.8 * base, base)
        };
        self.retry_at = self.clock.now() + delay;
        self.flight = None;
        warn
    }
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.generation += 1;
            self.flight = None;
        }
    }
    pub fn status(&self, outstanding_workers: usize) -> CatalogStatus {
        CatalogStatus {
            loaded_at: self.snapshot.as_ref().map(|s| s.loaded_at_utc.clone()),
            age_seconds: self
                .snapshot
                .as_ref()
                .map(|s| (self.clock.now() - s.loaded_at).max(0.)),
            last_error: self.last_error.clone(),
            consecutive_failures: self.consecutive_failures,
            refreshing: self.flight.is_some() && !self.closed,
            outstanding_workers,
        }
    }
}

/// Python's child list: `[urljoin(root_url, link["href"]) for link in
/// root.get("links", ()) if link.get("rel") == "child"]`, with its errors in order.
/// Falsey hrefs resolve to the root URL itself, as `urljoin` returns its base.
pub fn child_urls(root_url: &str, root: &serde_json::Value) -> Result<Vec<String>, CatalogError> {
    use serde_json::Value;
    let links = match root.get("links") {
        None => return Ok(vec![]),
        Some(Value::Array(links)) => links,
        Some(Value::Object(map)) if map.is_empty() => return Ok(vec![]),
        Some(Value::String(text)) if text.is_empty() => return Ok(vec![]),
        Some(Value::Object(_) | Value::String(_)) => {
            return Err(CatalogError::typed(
                "'str' object has no attribute 'get'",
                "AttributeError",
            ));
        }
        Some(_) => return Err(CatalogError::typed("Invalid STAC links", "TypeError")),
    };
    let mut urls = vec![];
    for link in links {
        if !link.is_object() {
            return Err(CatalogError::typed("Invalid STAC link", "AttributeError"));
        }
        if link.get("rel").and_then(Value::as_str) != Some("child") {
            continue;
        }
        let href = link
            .get("href")
            .ok_or_else(|| CatalogError::typed("'href'", "KeyError"))?;
        let falsey = match href {
            Value::Null | Value::Bool(false) => true,
            Value::Number(n) => n.as_f64() == Some(0.),
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            Value::Bool(true) => false,
        };
        urls.push(match href {
            _ if falsey => root_url.to_string(),
            Value::String(href) => crate::pyurl::urljoin(root_url, href)?,
            _ => {
                return Err(CatalogError::typed(
                    "Cannot mix str and non-str arguments",
                    "TypeError",
                ));
            }
        });
    }
    Ok(urls)
}
/// Check the httpx-normalized destination and raw target against reqwest.
pub fn request_target(url: &str) -> Result<(), CatalogError> {
    request_url(url).map(|_| ())
}
/// Normalize with httpx semantics, construct reqwest's URL, then compare every
/// destination/target component before allowing a request (DEVIATIONS.md D11).
pub fn request_url(url: &str) -> Result<url::Url, CatalogError> {
    crate::httpx_url::HttpxUrl::parse(url)?.request_url()
}
/// Fetch/JSON errors arrive before this pure step and must abort the refresh.
/// Parsing errors skip children; an empty root remains a valid empty catalog.
pub fn parse_children(values: &[serde_json::Value]) -> Result<Vec<Collection>, CatalogError> {
    let collections: Vec<_> = values
        .iter()
        .filter_map(|v| Collection::from_stac(v).ok())
        .collect();
    if !values.is_empty() && collections.is_empty() {
        return Err(CatalogError::new("No STAC collection is valid"));
    }
    Ok(collections)
}
