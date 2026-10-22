//! Materialized point reads with admitted workers and bounded cache/presentation.
use crate::{HttpResponse, SharedClock, config::Config, detail, response, server_error};
use dynapi_catalog::{
    Collection as Product,
    cache::{DatasetCache, DatasetKey, Profile, Resolved},
    native::StacCatalog,
    refresh::CatalogConfig,
};
use dynapi_contract::{
    ForecastBatch, Value,
    validation::{CanonicalQuery, parse_grid_integer},
};
use dynapi_dataset::{OpenError, OpenedDataset, run_with_permit};
use dynapi_policy::ComputeGate;
use dynapi_query::{
    self as query, CachePolicy, Collection, DataReader, DatasetView, Kind, PhaseObserver,
    Positions, QueryBudget, QueryError, QueryInput, QueryOptions, ResponseMetadata,
};
use dynapi_reader::{DatasetVersion, ReadSession};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Retained metadata is capped at 256 MiB, independently of the entry count.
/// Leaves headroom for compute; request-owned Arcs remain valid on eviction.
pub const DATASET_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Conservative response-tree, selection and encoding materialization charge.
pub const MATERIALIZATION_BYTES: u128 = 64 * 1024 * 1024;
#[derive(Clone, Default)]
pub(crate) struct Usage {
    pub products: Vec<String>,
    pub query_count: Option<usize>,
    pub work_units: Option<u64>,
    pub response_bytes: Option<usize>,
    pub dataset_cache: Option<String>,
    pub timings: BTreeMap<String, f64>,
}
pub(crate) type SharedUsage = Arc<Mutex<Usage>>;
#[derive(Default)]
struct Timings {
    durations: Vec<(&'static str, f64)>,
    starts: BTreeMap<&'static str, Instant>,
    caches: Vec<String>,
    usage: Option<SharedUsage>,
}
impl PhaseObserver for Timings {
    fn start(&mut self, name: &'static str) {
        self.starts.insert(name, Instant::now());
    }
    fn finish(&mut self, name: &'static str) {
        if let Some(started) = self.starts.remove(name) {
            let ms = started.elapsed().as_secs_f64() * 1000.;
            if let Some((_, total)) = self.durations.iter_mut().find(|(key, _)| *key == name) {
                *total += ms;
            } else {
                self.durations.push((name, ms));
            }
            if let Some(usage) = &self.usage {
                usage.lock().unwrap().timings = self
                    .durations
                    .iter()
                    .map(|(name, ms)| ((*name).into(), *ms))
                    .collect();
            }
        }
    }
}
impl Drop for Timings {
    fn drop(&mut self) {
        let pending: Vec<_> = self.starts.keys().copied().collect();
        for name in pending {
            self.finish(name);
        }
    }
}
impl Timings {
    fn new(usage: SharedUsage) -> Self {
        Self {
            usage: Some(usage),
            durations: Vec::new(),
            starts: BTreeMap::new(),
            caches: Vec::new(),
        }
    }
    fn cache(&mut self, value: dynapi_catalog::cache::CacheStatus) {
        self.caches.push(
            match value {
                dynapi_catalog::cache::CacheStatus::Hit => "hit",
                dynapi_catalog::cache::CacheStatus::Miss => "miss",
                dynapi_catalog::cache::CacheStatus::Revalidated => "revalidated",
                dynapi_catalog::cache::CacheStatus::Unknown => "unknown",
            }
            .into(),
        );
    }
    fn cache_header(&self) -> String {
        let mut caches = self.caches.clone();
        caches.sort();
        caches.dedup();
        caches.join("|")
    }
    fn header(&self) -> String {
        let mut parts: Vec<_> = self
            .durations
            .iter()
            .map(|(name, ms)| format!("{name};dur={ms:.3}"))
            .collect();
        parts.push(format!("dataset_cache;desc=\"{}\"", self.cache_header()));
        parts.join(", ")
    }
}
pub(crate) struct DataService {
    catalog: StacCatalog,
    cache: DatasetCache<SharedClock, Arc<OpenedDataset>>,
    config: Config,
    #[cfg(feature = "harness")]
    read_failure: Mutex<Option<dynapi_reader::ReaderError>>,
}
impl Drop for DataService {
    fn drop(&mut self) {
        self.catalog.close();
    }
}
impl DataService {
    pub fn new(config: &Config, clock: SharedClock) -> Result<Self, String> {
        let catalog = StacCatalog::new(&config.stac_url, CatalogConfig::default())
            .map_err(|e| e.to_string())?;
        let cache = DatasetCache::new(
            clock,
            config.main_dataset_ttl_seconds,
            config.dataset_cache_entries,
        )
        .map_err(|e| e.to_string())?
        .with_byte_limit(DATASET_CACHE_BYTES, |dataset: &Arc<OpenedDataset>| {
            dataset.metadata_bytes()
        });
        Ok(Self {
            catalog,
            cache,
            config: config.clone(),
            #[cfg(feature = "harness")]
            read_failure: Mutex::new(None),
        })
    }
    #[cfg(feature = "harness")]
    pub fn inject_read_failure(&self, error: dynapi_reader::ReaderError) {
        *self.read_failure.lock().unwrap() = Some(error);
    }
    pub fn warm(&self) {
        self.catalog.warm();
    }
    async fn product(&self, id: &str, forecast: bool) -> Result<Product, QueryError> {
        let product = self
            .catalog
            .get_collection(id)
            .await
            .map_err(|e| QueryError {
                status: 503,
                detail: e.to_string(),
                retry_after: None,
                exception_class: None,
            })?
            .filter(|p| !forecast || p.kind() != "unknown")
            .ok_or_else(|| QueryError {
                status: 404,
                detail: format!(
                    "Unknown data product {}",
                    dynapi_contract::output::python_repr(id)
                ),
                retry_after: None,
                exception_class: None,
            })?;
        if forecast && product.kind() != "forecast" {
            return Err(QueryError::invalid(format!(
                "Data product {} is {}, not forecast",
                dynapi_contract::output::python_repr(id),
                product.kind()
            )));
        }
        if let Some(reason) = product.point_query_reason() {
            return Err(QueryError::invalid(reason));
        }
        Ok(product)
    }
    fn budget(&self) -> QueryBudget {
        QueryBudget {
            max_values: self.config.max_response_values,
            max_read_values: self.config.max_read_values,
            ..QueryBudget::default()
        }
    }
    fn open(
        &self,
        product: &Product,
        snapshot: Option<&str>,
        handle: &tokio::runtime::Handle,
    ) -> Result<(Arc<OpenedDataset>, dynapi_catalog::cache::CacheStatus), QueryError> {
        let recipe = product
            .reader_config()
            .map_err(|e| QueryError::internal(e.to_string()))?;
        #[cfg(feature = "harness")]
        let recipe = if let Some(base) = &self.config.fixture_store_base {
            dynapi_catalog::ReaderConfig {
                store: dynapi_reader::StoreDescriptor::Http {
                    url: format!("{base}/{}/repository/", product.id),
                },
                containers: recipe.containers,
            }
        } else {
            recipe
        };
        let opened = self.cache.open(
            DatasetKey::new(product, snapshot, Profile::Default),
            || {
                let id = match snapshot {
                    Some(id) => id.to_owned(),
                    None => handle
                        .block_on(ReadSession::open(
                            &recipe.store,
                            DatasetVersion::Branch("main"),
                            &recipe.containers,
                        ))
                        .map_err(dynapi_dataset::map_reader_error)?
                        .snapshot_id()
                        .to_owned(),
                };
                Ok::<_, QueryError>(Resolved {
                    snapshot_id: id.clone(),
                    session: id,
                })
            },
            |id| {
                handle
                    .block_on(OpenedDataset::open(
                        &recipe.store,
                        DatasetVersion::Snapshot(&id),
                        &recipe.containers,
                    ))
                    .map_err(map_open)
            },
        )?;
        Ok((opened.dataset.as_ref().clone(), opened.cache_status))
    }
    pub async fn forecasts(
        self: &Arc<Self>,
        payload: ForecastBatch,
        gate: &ComputeGate<SharedClock>,
        usage: SharedUsage,
    ) -> HttpResponse {
        let started = Instant::now();
        {
            let mut u = usage.lock().unwrap();
            u.products = payload
                .queries
                .iter()
                .map(|q| q.data_product_id.clone())
                .collect();
            u.query_count = Some(payload.queries.len());
        }
        let mut timings = Timings::new(usage.clone());
        // A per-product caller policy can run here before admission or store I/O.
        let Some(permit) = gate.try_acquire() else {
            return busy(gate);
        };
        let service = self.clone();
        let result = run_with_permit(permit, move |handle| {
            let resolved: Vec<std::sync::OnceLock<(Arc<OpenedDataset>, Collection)>> =
                (0..payload.queries.len())
                    .map(|_| std::sync::OnceLock::new())
                    .collect();
            let mut budget = service.budget();
            let mut builder = query::BatchBuilder::new(Kind::Forecast);
            let mut opened = Vec::new();
            for (q, cell) in payload.queries.iter().zip(&resolved) {
                timings.start("catalog");
                let product = handle.block_on(service.product(&q.data_product_id, true))?;
                if dynapi_contract::output::normalize_variables(&q.variables).is_empty() {
                    return Err(QueryError::invalid("At least one variable is required"));
                }
                timings.finish("catalog");
                timings.start("dataset_open");
                let (dataset, status) = service.open(&product, None, &handle)?;
                timings.finish("dataset_open");
                timings.cache(status);
                let (dataset, c) = cell.get_or_init(|| (dataset, collection(&product)));
                builder.push(
                    &QueryInput::from_forecast(dataset.view(), c, dataset.snapshot_id(), q),
                    &mut budget,
                    &mut timings,
                )?;
                opened.push(dataset.clone());
                bound_selections(builder.selections(), &opened)?;
            }
            let batch = builder.finish(&mut budget, false, 1)?;
            bound_selections(batch.selections(), &opened)?;
            let mut reader = BatchReader {
                opened: &opened,
                handle,
                #[cfg(feature = "harness")]
                failure: service.read_failure.lock().unwrap().take(),
            };
            let body = batch.load_observed(&mut reader, &mut timings)?;
            finish(
                body,
                batch.work_units(),
                CachePolicy::Mutable,
                timings,
                started,
                &usage,
            )
        })
        .await;
        result.unwrap_or_else(query_error)
    }
    pub async fn canonical(
        self: &Arc<Self>,
        id: &str,
        snapshot: String,
        indices: &str,
        params: CanonicalQuery,
        gate: &ComputeGate<SharedClock>,
        usage: SharedUsage,
    ) -> HttpResponse {
        let started = Instant::now();
        {
            let mut u = usage.lock().unwrap();
            u.products = vec![id.into()];
            u.query_count = Some(1);
        }
        // A per-product caller policy can run here before admission or store I/O.
        let Some(permit) = gate.try_acquire() else {
            return busy(gate);
        };
        let mut timings = Timings::new(usage.clone());
        timings.start("catalog");
        let product = match self.product(id, false).await {
            Ok(p) => p,
            Err(e) => return query_error(e),
        };
        timings.finish("catalog");
        let variables = dynapi_contract::output::normalize_variables(
            &params
                .variables
                .split(',')
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
        if variables.is_empty() {
            return query_error(QueryError::invalid("At least one variable is required"));
        }
        if variables.len() > 25 {
            return query_error(QueryError::invalid("At most 25 variables may be requested"));
        }
        let Some((first, second)) = indices
            .split_once(',')
            .and_then(|(a, b)| Some((parse_grid_integer(a)?, parse_grid_integer(b)?)))
        else {
            return query_error(QueryError::invalid("Grid indices must be two integers"));
        };
        let c = collection(&product);
        let options = match options(c.kind, variables, params) {
            Ok(o) => o,
            Err(e) => return query_error(e),
        };
        let service = self.clone();
        let result = run_with_permit(permit, move |handle| {
            timings.start("dataset_open");
            let (ds, status) = service.open(&product, Some(&snapshot), &handle).map_err(|mut e| { if e.status == 404 { e.detail = format!("Unknown snapshot {} for data product {}; use the snapshotId from a forecasts or analyses response", dynapi_contract::output::python_repr(&snapshot), dynapi_contract::output::python_repr(&product.id)); } e })?;
            timings.finish("dataset_open");
            timings.cache(status);
            timings.start("point_select");
            // Every out-of-int64 integer is outside the bounded grid as well.
            let index = |n: query::BigInt| n.to_string().parse::<i64>().unwrap_or(-1);
            let point = query::point_from_indices(ds.view(), index(first), index(second))?;
            timings.finish("point_select");
            timings.start("query_select");
            let mut budget = service.budget();
            let selected = query::select_query(
                ds.view(),
                &c,
                point,
                options,
                ds.snapshot_id(),
                Some(&mut budget),
            )?;
            budget.consume_read(selected.read_values())?;
            timings.finish("query_select");
            bound_selections(std::slice::from_ref(&selected), std::slice::from_ref(&ds))?;
            let datasets = [ds.clone()];
            let mut reader = BatchReader { opened: &datasets, handle, #[cfg(feature = "harness")] failure: service.read_failure.lock().unwrap().take() };
            let raw = query::load_query_observed(&selected, &mut reader, &mut timings)?;
            let body = query::canonical_result(&selected, &raw);
            finish(
                body,
                budget.used_values,
                CachePolicy::Immutable,
                timings,
                started,
                &usage,
            )
        })
        .await;
        result.unwrap_or_else(query_error)
    }
}
fn collection(p: &Product) -> Collection {
    Collection {
        id: p.id.clone(),
        kind: match p.kind() {
            "forecast" => Kind::Forecast,
            "analysis" => Kind::Analysis,
            _ => Kind::Unknown,
        },
        version: Some(p.version.clone()),
        catalog_url: p.catalog_url.clone(),
        variables: p
            .variables
            .iter()
            .map(|(name, v)| {
                (
                    name.clone(),
                    query::CatalogVariable {
                        dimensions: v.dimensions.clone(),
                        unit: v.unit.clone(),
                    },
                )
            })
            .collect(),
        point_query_reason: p.point_query_reason().map(str::to_owned),
    }
}
fn map_open(error: OpenError) -> QueryError {
    match error {
        OpenError::UnknownSnapshot(_) => QueryError {
            status: 404,
            detail: error.to_string(),
            retry_after: None,
            exception_class: None,
        },
        OpenError::Reader(e) => dynapi_dataset::map_reader_error(e),
        _ => QueryError::internal(error.to_string()),
    }
}
fn options(
    kind: Kind,
    variables: Vec<String>,
    p: CanonicalQuery,
) -> Result<QueryOptions, QueryError> {
    let aware = |d: Option<query::RequestDatetime>, name| -> Result<_, QueryError> {
        d.map(|d| {
            if d.time.tz_offset.is_none() {
                Err(QueryError::invalid(format!(
                    "{name} must include a timezone"
                )))
            } else {
                Ok(d)
            }
        })
        .transpose()
    };
    let (run, start, end, max) = if kind == Kind::Forecast {
        let run = aware(p.init_time, "initTime")?
            .ok_or_else(|| QueryError::invalid("Canonical forecast queries require initTime"))?;
        (
            Some(run),
            aware(p.valid_time_start, "validTimeStart")?,
            aware(p.valid_time_end, "validTimeEnd")?,
            p.max_lead_time_hours,
        )
    } else {
        (
            None,
            aware(p.start_time, "startTime")?,
            aware(p.end_time, "endTime")?,
            None,
        )
    };
    if let (Some(start), Some(end)) = (start, end)
        && (start.timestamp_tz(), start.time.microsecond)
            > (end.timestamp_tz(), end.time.microsecond)
    {
        return Err(QueryError::invalid(if kind == Kind::Forecast {
            "validTimeStart must not be after validTimeEnd"
        } else {
            "startTime must not be after endTime"
        }));
    }
    Ok(QueryOptions {
        variables,
        run,
        run_selection: run.map(|_| "explicit".into()),
        max_lead_time_hours: max,
        start,
        end,
    })
}
struct BatchReader<'a> {
    opened: &'a [Arc<OpenedDataset>],
    handle: tokio::runtime::Handle,
    #[cfg(feature = "harness")]
    failure: Option<dynapi_reader::ReaderError>,
}
impl DataReader for BatchReader<'_> {
    fn read(
        &mut self,
        view: &DatasetView,
        variable: &str,
        positions: &Positions,
    ) -> Result<Vec<Value>, QueryError> {
        #[cfg(feature = "harness")]
        if let Some(error) = self.failure.take() {
            return Err(dynapi_dataset::map_reader_error(error));
        }
        self.opened
            .iter()
            .find(|ds| std::ptr::eq(ds.view(), view))
            .ok_or_else(|| QueryError::internal("Missing opened dataset"))?
            .reader(self.handle.clone())
            .read(view, variable, positions)
    }
}
fn bound_selections(
    selections: &[query::PreparedQuery<'_>],
    opened: &[Arc<OpenedDataset>],
) -> Result<(), QueryError> {
    // Request-owned metadata survives LRU eviction. Bound unique active handles
    // independently so opening many products cannot bypass the retained-cache cap.
    let mut seen = std::collections::BTreeSet::new();
    let metadata: u128 = opened
        .iter()
        .filter(|ds| seen.insert(Arc::as_ptr(ds) as usize))
        .map(|ds| ds.metadata_bytes() as u128)
        .sum();
    if metadata > DATASET_CACHE_BYTES as u128 {
        return Err(QueryError::invalid(
            "Query exceeds the 268435456-byte active metadata budget",
        ));
    }
    let bytes = selections
        .iter()
        .try_fold(0u128, |n, s| n.checked_add(s.materialization_byte_bound()?))
        .unwrap_or(u128::MAX);
    if bytes > MATERIALIZATION_BYTES {
        return Err(QueryError::invalid(
            "Query exceeds the 67108864-byte response materialization budget",
        ));
    }
    Ok(())
}
fn finish(
    body: Value,
    work: u64,
    cache_policy: CachePolicy,
    mut timings: Timings,
    started: Instant,
    usage: &SharedUsage,
) -> Result<HttpResponse, QueryError> {
    if dynapi_contract::output::json_byte_bound(&body)
        .is_none_or(|bytes| bytes as u128 > MATERIALIZATION_BYTES)
    {
        return Err(QueryError::invalid(
            "Query exceeds the 67108864-byte response materialization budget",
        ));
    }
    timings.start("json_encode");
    let mut encoded = query::encode_response(
        &body,
        work,
        &ResponseMetadata {
            cache_policy,
            dataset_cache: timings.caches.clone(),
            ..ResponseMetadata::default()
        },
    )?;
    if encoded.body.len() as u128 > MATERIALIZATION_BYTES {
        return Err(QueryError::invalid(
            "Query exceeds the 67108864-byte response materialization budget",
        ));
    }
    timings.finish("json_encode");
    timings
        .durations
        .push(("handler", started.elapsed().as_secs_f64() * 1000.));
    {
        let mut u = usage.lock().unwrap();
        u.work_units = Some(work);
        u.response_bytes = Some(encoded.body.len());
        u.dataset_cache = Some(timings.cache_header());
        u.timings = timings
            .durations
            .iter()
            .map(|(name, ms)| ((*name).into(), *ms))
            .collect();
    }
    for (key, value) in &mut encoded.headers {
        if key == "server-timing" {
            *value = timings.header();
        }
    }
    let mut result = response(200, encoded.body, None);
    for (key, value) in encoded.headers {
        result.headers_mut().insert(
            hyper::header::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    Ok(result)
}
pub(crate) fn query_error(error: QueryError) -> HttpResponse {
    if error.status == 500 {
        return server_error();
    }
    let mut result = detail(error.status, &error.detail);
    if let Some(retry) = error.retry_after {
        result
            .headers_mut()
            .insert("retry-after", retry.to_string().parse().unwrap());
    }
    result
}
fn busy(gate: &ComputeGate<SharedClock>) -> HttpResponse {
    let retry = gate.retry_after();
    let mut result = detail(503, &format!("Server busy; retry after {retry} seconds"));
    result
        .headers_mut()
        .insert("retry-after", retry.to_string().parse().unwrap());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_lookup_snapshot_errors_remain_internal() {
        let error = map_open(OpenError::Reader(dynapi_reader::ReaderError {
            kind: dynapi_reader::ErrorKind::Snapshot,
            message: "synthetic snapshot transport failure".into(),
            virtual_chunk_fetch: false,
            exception_class: None,
        }));
        assert_eq!(error.status, 500);
    }
}
