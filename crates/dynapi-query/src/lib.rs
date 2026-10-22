//! Pure point-query planning over opened, CF-decoded views. No I/O or routing.
/// Original validated civil datetime, before NumPy nanosecond conversion.
pub use dynapi_contract::DateTime as RequestDatetime;
use dynapi_contract::Value;
pub use num_bigint::BigInt;
use std::collections::BTreeMap;
/// Nanoseconds since epoch (times) or nanoseconds (durations), as dynapi-reader returns.
pub type Nanoseconds = i64;
/// Python datetime labels, rounded to microseconds. Unlike raw NumPy coordinates,
/// these may lie just outside the i64 nanosecond domain.
pub type DatetimeNanoseconds = i128;
pub const NAT: Nanoseconds = i64::MIN;
pub const HOUR: Nanoseconds = 3_600_000_000_000;
#[derive(Clone, Debug, Default)]
pub struct DatasetView {
    pub dimensions: BTreeMap<String, usize>,
    pub coordinates: BTreeMap<String, Coordinate>,
    pub attributes: BTreeMap<String, Value>,
    pub variables: BTreeMap<String, Variable>,
    /// Adapter-classified spatial_ref for x/y grids. Never assume x/y are degrees.
    pub spatial_crs: Option<SpatialCrs>,
}
#[derive(Clone, Debug)]
pub enum Coordinate {
    /// Decoded float64 coordinates.
    Numeric(Vec<f64>),
    Float32(Vec<f32>),
    Int64(Vec<i64>),
    Time(Vec<Nanoseconds>),
    Duration(Vec<Nanoseconds>),
    Members(Vec<Value>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpatialCrs {
    Geographic,
    Projected,
    Missing,
}
#[derive(Clone, Debug, Default)]
pub struct Variable {
    /// Coordinate metadata remains visible, but cannot satisfy a data-variable query.
    pub is_coordinate: bool,
    pub dimensions: Vec<String>,
    pub attributes: BTreeMap<String, Value>,
    /// Explicit chunks take precedence, including unknown entries. Unknown axes
    /// are charged as their full original size, never their selected size.
    pub chunks: Option<Vec<Option<i64>>>,
    pub preferred_chunks: BTreeMap<String, i64>,
}
#[derive(Clone, Debug, Default)]
pub struct CatalogVariable {
    pub dimensions: Vec<String>,
    pub unit: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Collection {
    pub id: String,
    pub kind: Kind,
    pub version: Option<String>,
    pub catalog_url: Option<String>,
    pub variables: BTreeMap<String, CatalogVariable>,
    pub point_query_reason: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Forecast,
    Analysis,
    Unknown,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forecast => "forecast",
            Self::Analysis => "analysis",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedPoint {
    pub latitude: f64,
    pub longitude: f64,
    /// In Python insertion order: latitude/longitude or y/x.
    pub indexers: Vec<(String, usize)>,
}
#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    pub variables: Vec<String>,
    pub run: Option<RequestDatetime>,
    pub run_selection: Option<String>,
    pub max_lead_time_hours: Option<BigInt>,
    pub start: Option<RequestDatetime>,
    pub end: Option<RequestDatetime>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Selection {
    Index(usize),
    Indices(Vec<usize>),
}
pub type Positions = BTreeMap<String, Selection>;
/// Read flat values in source dimension order after selectors. Index removes its
/// axis; Indices retains order and duplicates. Preserve integer types as Int.
/// Floating nonfinites are permitted here and become null in response data.
pub trait DataReader {
    fn read(
        &mut self,
        view: &DatasetView,
        variable: &str,
        positions: &Positions,
    ) -> Result<Vec<Value>, QueryError>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    pub status: u16,
    pub detail: String,
    pub retry_after: Option<u32>,
    /// Python exception class for internal failures; never exposed as HTTP detail.
    pub exception_class: Option<&'static str>,
}
impl QueryError {
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self {
            status: 422,
            detail: detail.into(),
            retry_after: None,
            exception_class: None,
        }
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            status: 500,
            detail: detail.into(),
            retry_after: None,
            exception_class: None,
        }
    }
    pub(crate) fn exception(class: &'static str, detail: impl Into<String>) -> Self {
        Self {
            exception_class: Some(class),
            ..Self::internal(detail)
        }
    }
    pub fn upstream_unavailable() -> Self {
        Self {
            status: 503,
            detail: "Upstream data temporarily unavailable".into(),
            retry_after: Some(5),
            exception_class: None,
        }
    }
    /// Classify only Icechunk virtual-chunk transient diagnostics. Permanent
    /// 403/404s and unfamiliar failures remain internal errors, as in Python.
    pub fn from_storage_failure(virtual_chunk_fetch: bool, message: impl Into<String>) -> Self {
        let message = message.into();
        if virtual_chunk_fetch
            && (message
                .contains("operation timeout (all attempts including retries) occurred after ")
                || message.contains("Error { code: \"SlowDown\","))
        {
            Self::upstream_unavailable()
        } else {
            Self::exception("StorageError", message)
        }
    }
    pub fn body(&self) -> Vec<u8> {
        dynapi_contract::output::detail(&self.detail)
    }
}
impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl std::error::Error for QueryError {}
mod batch;
mod plan;
mod response;
mod selection;
pub use batch::{
    prepare_batch, prepare_batch_observed, BatchBuilder, PhaseObserver, PreparedBatch, QueryInput,
};
pub use plan::{co_tile_key, plan_load_groups, CoTileKey, LoadPlan};
pub use response::{
    canonical_path, canonical_result, encode_response, load_query, load_query_observed,
    CachePolicy, QueryResponse, ResponseMetadata, IMMUTABLE_CACHE_CONTROL, MUTABLE_CACHE_CONTROL,
};
pub use selection::{
    format_datetime, list_forecast_init_times, point_from_indices, select_forecast_init_times,
    select_nearest_point, select_query, InitTimeSelection, PreparedQuery, QueryBudget,
};
