use crate::selection::{format_request_datetime, variable_size};
use crate::*;
use dynapi_contract::output::{json, urlencode, JsonError};
use sha2::{Digest, Sha256};

pub const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
pub const MUTABLE_CACHE_CONTROL: &str = "public, max-age=30, stale-while-revalidate=120";
fn optional(value: &Option<String>) -> Value {
    value.clone().map(Value::from).unwrap_or(Value::Null)
}
pub(crate) fn times(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::from).collect())
}
pub fn canonical_path(
    collection: &Collection,
    snapshot: &str,
    point: &SelectedPoint,
    options: &QueryOptions,
) -> String {
    let mut params = vec![("variables".into(), options.variables.join(","))];
    if let (Kind::Forecast, Some(run)) = (collection.kind, options.run) {
        params.push(("initTime".into(), format_request_datetime(run)));
        if let Some(max) = &options.max_lead_time_hours {
            params.push(("maxLeadTimeHours".into(), max.to_string()));
        }
        if let Some(start) = options.start {
            params.push(("validTimeStart".into(), format_request_datetime(start)));
        }
        if let Some(end) = options.end {
            params.push(("validTimeEnd".into(), format_request_datetime(end)));
        }
    } else if collection.kind == Kind::Analysis {
        if let Some(start) = options.start {
            params.push(("startTime".into(), format_request_datetime(start)));
        }
        if let Some(end) = options.end {
            params.push(("endTime".into(), format_request_datetime(end)));
        }
    }
    format!(
        "/v1/data/{}/{snapshot}/points/{},{}?{}",
        collection.id,
        point.indexers[0].1,
        point.indexers[1].1,
        urlencode(&params)
    )
}
fn data_value(value: Value) -> Value {
    match value {
        Value::Float(x) => Value::data_float(x),
        x => x,
    }
}
fn data(
    query: &PreparedQuery<'_>,
    loaded: &[(String, Vec<Value>)],
    member: Option<usize>,
) -> Result<Value, QueryError> {
    let values = loaded
        .iter()
        .map(|(name, values)| {
            let var = &query.source.variables[name];
            let remaining: Vec<_> = var
                .dimensions
                .iter()
                .filter(|d| !matches!(query.positions.get(*d), Some(Selection::Index(_))))
                .collect();
            let values = if let Some(member) = member {
                if let Some(axis) = remaining
                    .iter()
                    .position(|d| d.as_str() == "ensemble_member")
                {
                    let stride: usize = remaining[axis + 1..]
                        .iter()
                        .map(|d| match query.positions.get(*d) {
                            Some(Selection::Indices(xs)) => xs.len(),
                            _ => query.source.dimensions[*d],
                        })
                        .product();
                    let count = query.source.dimensions["ensemble_member"];
                    values
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| (i / stride) % count == member)
                        .map(|(_, v)| data_value(v.clone()))
                        .collect()
                } else {
                    values.iter().cloned().map(data_value).collect()
                }
            } else {
                values.iter().cloned().map(data_value).collect()
            };
            let time_axes = remaining
                .iter()
                .filter(|d| d.as_str() != "ensemble_member")
                .count();
            if time_axes > 1 {
                return Err(QueryError::exception(
                    "TypeError",
                    "Requested variables have unsupported multidimensional response values",
                ));
            }
            Ok((name.clone(), Value::Array(values)))
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    Ok(Value::Object(values))
}
pub fn load_query(
    query: &PreparedQuery<'_>,
    reader: &mut impl DataReader,
) -> Result<Value, QueryError> {
    load_query_observed(query, reader, &mut crate::batch::NoopObserver)
}
pub fn load_query_observed(
    query: &PreparedQuery<'_>,
    reader: &mut impl DataReader,
    observer: &mut impl crate::batch::PhaseObserver,
) -> Result<Value, QueryError> {
    observer.start("zarr_load");
    let mut loaded = Vec::new();
    for name in &query.options.variables {
        let values = reader.read(query.source, name, &query.positions)?;
        if values.len() as u64
            != variable_size(
                query.source,
                &query.source.variables[name],
                &query.positions,
            )?
        {
            return Err(QueryError::internal(
                "Reader returned an incorrect selected value count",
            ));
        }
        loaded.push((name.clone(), values));
    }
    observer.finish("zarr_load");
    observer.start("response_build");
    let units = Value::Object(
        query
            .options
            .variables
            .iter()
            .map(|name| {
                let unit = query.collection.variables[name]
                    .unit
                    .as_ref()
                    .filter(|u| !u.is_empty())
                    .map(|u| Value::from(u.clone()))
                    .or_else(|| {
                        query.source.variables[name]
                            .attributes
                            .get("units")
                            .cloned()
                    })
                    .unwrap_or(Value::Null);
                (name.clone(), unit)
            })
            .collect(),
    );
    let grid = Value::Object(
        query
            .point
            .indexers
            .iter()
            .map(|(d, i)| (format!("{d}_index"), Value::from(*i)))
            .collect(),
    );
    let mut pairs = vec![
        (
            "source".into(),
            Value::object([
                ("collection", query.collection.id.clone().into()),
                ("dataset_version", optional(&query.collection.version)),
                ("snapshot", query.snapshot_id.clone().into()),
                (
                    "run",
                    query
                        .options
                        .run
                        .map(|r| Value::from(format_request_datetime(r)))
                        .unwrap_or(Value::Null),
                ),
                ("run_selection", optional(&query.options.run_selection)),
            ]),
        ),
        (
            "point".into(),
            Value::object([
                ("latitude", Value::Float(query.point.latitude)),
                ("longitude", Value::Float(query.point.longitude)),
                ("grid", grid),
                ("method", "nearest".into()),
            ]),
        ),
        ("time".into(), times(&query.times)),
        (
            "lead_time_hours".into(),
            query
                .lead_hours
                .as_ref()
                .map(|v| Value::Array(v.iter().map(|x| Value::Float(*x)).collect()))
                .unwrap_or(Value::Null),
        ),
        ("units".into(), units),
        (
            "links".into(),
            Value::object([
                (
                    "canonical",
                    canonical_path(
                        query.collection,
                        &query.snapshot_id,
                        &query.point,
                        &query.options,
                    )
                    .into(),
                ),
                ("catalog", optional(&query.collection.catalog_url)),
            ]),
        ),
    ];
    let ensemble = query.options.variables.iter().any(|v| {
        query.source.variables[v]
            .dimensions
            .iter()
            .any(|d| d == "ensemble_member")
    });
    if ensemble {
        let members = if let Some(Coordinate::Members(ids)) =
            query.source.coordinates.get("ensemble_member")
        {
            ids.clone()
        } else if let Some(Coordinate::Numeric(ids)) =
            query.source.coordinates.get("ensemble_member")
        {
            ids.iter().map(|x| Value::Float(*x)).collect()
        } else if let Some(Coordinate::Float32(ids)) =
            query.source.coordinates.get("ensemble_member")
        {
            ids.iter().map(|x| Value::Float(f64::from(*x))).collect()
        } else if let Some(Coordinate::Int64(ids)) = query.source.coordinates.get("ensemble_member")
        {
            ids.iter().map(|x| Value::from(*x)).collect()
        } else {
            (0..query.source.dimensions["ensemble_member"])
                .map(Value::from)
                .collect()
        };
        let values = members
            .into_iter()
            .enumerate()
            .map(|(i, id)| {
                Ok(Value::object([
                    ("member", id),
                    ("data", data(query, &loaded, Some(i))?),
                ]))
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        pairs.push(("ensemble_members".into(), Value::Array(values)));
    } else {
        pairs.push(("data".into(), data(query, &loaded, None)?));
    }
    observer.finish("response_build");
    Ok(Value::Object(pairs))
}
pub(crate) fn field(raw: &Value, key: &str) -> Value {
    raw.get(key)
        .expect("internally built response field")
        .clone()
}
pub(crate) fn forecast(raw: &Value) -> Value {
    let source = raw.get("source").unwrap();
    let mut pairs = vec![
        ("initTime".into(), field(source, "run")),
        ("validTimes".into(), field(raw, "time")),
        ("leadTimeHours".into(), field(raw, "lead_time_hours")),
        (
            "links".into(),
            Value::object([("canonical", field(raw.get("links").unwrap(), "canonical"))]),
        ),
    ];
    append_data(&mut pairs, raw);
    Value::Object(pairs)
}
pub(crate) fn append_data(pairs: &mut Vec<(String, Value)>, raw: &Value) {
    if let Some(members) = raw.get("ensemble_members") {
        pairs.push(("ensembleMembers".into(), members.clone()));
    } else {
        pairs.push(("data".into(), field(raw, "data")));
    }
}
pub(crate) fn variable_units(raw: &Value) -> Value {
    let Value::Object(units) = raw.get("units").unwrap() else {
        unreachable!()
    };
    Value::Object(
        units
            .iter()
            .map(|(name, unit)| (name.clone(), Value::object([("unit", unit.clone())])))
            .collect(),
    )
}
pub(crate) fn product(collection: &Collection) -> Value {
    Value::object([
        ("id", collection.id.clone().into()),
        ("version", optional(&collection.version)),
    ])
}
pub(crate) fn catalog(collection: &Collection) -> Value {
    Value::object([("catalog", optional(&collection.catalog_url))])
}
pub(crate) fn point_body(point: &SelectedPoint, requested: Option<(f64, f64)>) -> Value {
    let mut pairs = Vec::new();
    if let Some((lat, lon)) = requested {
        pairs.push((
            "requested".into(),
            Value::object([
                ("latitude", Value::Float(lat)),
                ("longitude", Value::Float(lon)),
            ]),
        ));
    }
    pairs.push((
        "selected".into(),
        Value::object([
            ("latitude", Value::Float(point.latitude)),
            ("longitude", Value::Float(point.longitude)),
            (
                "grid",
                Value::Object(
                    point
                        .indexers
                        .iter()
                        .map(|(d, i)| (format!("{d}Index"), Value::from(*i)))
                        .collect(),
                ),
            ),
        ]),
    ));
    pairs.push(("method".into(), "nearest".into()));
    Value::Object(pairs)
}
/// Canonical GET presentation (no requested coordinate), preserving presenters.py order.
pub fn canonical_result(query: &PreparedQuery<'_>, raw: &Value) -> Value {
    let mut pairs = vec![
        ("dataProduct".into(), product(query.collection)),
        ("point".into(), point_body(&query.point, None)),
        ("variables".into(), variable_units(raw)),
        ("snapshotId".into(), query.snapshot_id.clone().into()),
    ];
    if query.collection.kind == Kind::Forecast {
        pairs.push(("forecasts".into(), Value::Array(vec![forecast(raw)])));
        pairs.push(("links".into(), catalog(query.collection)));
    } else {
        pairs.push(("analysisTimes".into(), field(raw, "time")));
        pairs.push(("links".into(), field(raw, "links")));
        append_data(&mut pairs, raw);
    }
    Value::Object(pairs)
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CachePolicy {
    #[default]
    Mutable,
    Immutable,
}
impl CachePolicy {
    fn header(self) -> &'static str {
        match self {
            Self::Mutable => MUTABLE_CACHE_CONTROL,
            Self::Immutable => IMMUTABLE_CACHE_CONTROL,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct ResponseMetadata {
    /// The executor supplies measured phases in insertion order, already formatted
    /// to milliseconds with three decimals, as ServerTimings.header() does.
    pub server_timing: String,
    pub dataset_cache: Vec<String>,
    pub cache_policy: CachePolicy,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryResponse {
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}
/// Compact JSON uses ensure_ascii=True, as routes._json_response does. Error
/// envelopes instead use the contract's UTF-8 HTTP exception writer.
pub fn encode_response(
    body: &Value,
    work_units: u64,
    metadata: &ResponseMetadata,
) -> Result<QueryResponse, QueryError> {
    let encoded = json(body, true).map_err(|error| match error {
        JsonError::NonFinite => {
            fn first_nonfinite(value: &Value) -> Option<f64> {
                match value {
                    Value::Float(x) if !x.is_finite() => Some(*x),
                    Value::Array(xs) => xs.iter().find_map(first_nonfinite),
                    Value::Object(xs) => xs.iter().find_map(|(_, v)| first_nonfinite(v)),
                    _ => None,
                }
            }
            let mut repr = String::new();
            dynapi_contract::output::float_repr(first_nonfinite(body).unwrap(), &mut repr);
            QueryError::exception(
                "ValueError",
                format!("Out of range float values are not JSON compliant: {repr}"),
            )
        }
        JsonError::DuplicateKey => QueryError::internal("Duplicate key in response object"),
    })?;
    let etag = format!("\"{:x}\"", Sha256::digest(&encoded));
    let mut caches = metadata.dataset_cache.clone();
    caches.sort();
    caches.dedup();
    let cache = if caches.is_empty() {
        "unknown".to_string()
    } else {
        caches.join("|")
    };
    let headers = vec![
        (
            "cache-control".into(),
            metadata.cache_policy.header().into(),
        ),
        ("etag".into(), etag),
        ("server-timing".into(), metadata.server_timing.clone()),
        ("x-dynamical-dataset-cache".into(), cache),
        ("x-dynamical-work-units".into(), work_units.to_string()),
        ("content-length".into(), encoded.len().to_string()),
        ("content-type".into(), "application/json".into()),
    ];
    Ok(QueryResponse {
        body: encoded,
        headers,
    })
}
