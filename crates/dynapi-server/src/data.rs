//! Materialized point reads with admitted workers and bounded cache/presentation.
use crate::telemetry::{
    self, ErrorKind,
    loads::{LoadClass, LoadIdentity, LoadObserver, NoopLoadObserver, OpenStatus},
};
use crate::{HttpResponse, SharedClock, config::Config, detail, response, server_error};
use dynapi_catalog::{
    Collection as Product,
    cache::{DatasetCache, DatasetKey, Profile, Resolved},
    native::StacCatalog,
    refresh::Snapshot,
};
use dynapi_contract::{
    AnalysisBatch, ForecastBatch, ForecastRunsQuery, Value,
    validation::{CanonicalQuery, parse_grid_integer},
};
use dynapi_dataset::{OpenError, OpenedDataset, run_with_permit};
use dynapi_policy::ComputeGate;
use dynapi_query::{
    self as query, CachePolicy, Collection, DataReader, DatasetView, Kind, PhaseObserver,
    Positions, QueryBudget, QueryError, QueryOptions, ResponseMetadata,
};
use dynapi_reader::{DatasetVersion, ReadSession};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

/// Retained metadata is capped at 256 MiB, independently of the entry count.
/// Leaves headroom for compute; request-owned Arcs remain valid on eviction.
pub const DATASET_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Conservative response-tree, selection and encoding materialization charge.
pub const MATERIALIZATION_BYTES: u128 = query::DEFAULT_MATERIALIZATION_BYTES;
#[derive(Clone, Default)]
pub(crate) struct Usage {
    pub pre_routing_rejection: bool,
    pub products: Vec<String>,
    pub geo_checked: bool,
    pub query_count: Option<usize>,
    pub work_units: Option<u64>,
    pub response_bytes: Option<usize>,
    pub dataset_cache: Option<String>,
    pub timings: BTreeMap<String, f64>,
    pub read_values: Option<u128>,
    pub counters: BTreeMap<String, u64>,
    pub error_kind: Option<ErrorKind>,
}
pub(crate) type SharedUsage = Arc<Mutex<Usage>>;
#[derive(Default)]
struct Timings {
    durations: Vec<(&'static str, f64)>,
    starts: BTreeMap<&'static str, Instant>,
    caches: Vec<String>,
    usage: Option<SharedUsage>,
    observer: Option<Arc<dyn LoadObserver>>,
    pending_loads: VecDeque<LoadWork>,
    active_load: Option<(LoadWork, LoadClass, Instant)>,
}
struct LoadWork {
    identity: LoadIdentity,
    read_values: u64,
    group: bool,
}
impl PhaseObserver for Timings {
    fn start(&mut self, name: &'static str) {
        self.starts.insert(name, Instant::now());
        if name == "zarr_load"
            && let Some(work) = self.pending_loads.pop_front()
        {
            let class = self
                .observer
                .as_ref()
                .and_then(|observer| telemetry::best_effort(|| observer.classify(&work.identity)))
                .unwrap_or(LoadClass::Warm);
            if work.group {
                self.count("readGroups", 1);
                self.count("groupedReadValues", work.read_values);
            }
            self.active_load = Some((work, class, Instant::now()));
        }
    }
    fn finish(&mut self, name: &'static str) {
        if name == "zarr_load" {
            self.finish_load(false);
        }
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
    fn read_values(&mut self, values: u64) {
        if let Some(usage) = &self.usage {
            *usage.lock().unwrap().read_values.get_or_insert(0) += u128::from(values);
        }
    }
}
impl Drop for Timings {
    fn drop(&mut self) {
        self.finish_load(true);
        let pending: Vec<_> = self.starts.keys().copied().collect();
        for name in pending {
            self.finish(name);
        }
    }
}
impl Timings {
    fn new(usage: SharedUsage, observer: Arc<dyn LoadObserver>) -> Self {
        usage.lock().unwrap().read_values = Some(0);
        Self {
            usage: Some(usage),
            observer: Some(observer),
            durations: Vec::new(),
            starts: BTreeMap::new(),
            caches: Vec::new(),
            pending_loads: VecDeque::new(),
            active_load: None,
        }
    }
    fn count(&mut self, key: &str, amount: u64) {
        if let Some(usage) = &self.usage {
            let mut usage = usage.lock().unwrap();
            let count = usage.counters.entry(key.to_owned()).or_default();
            *count = count.saturating_add(amount);
        }
    }
    fn finish_load(&mut self, failed: bool) {
        let Some((work, class, started)) = self.active_load.take() else {
            return;
        };
        let elapsed = elapsed_ms(started);
        if let Some(observer) = &self.observer {
            telemetry::best_effort(|| observer.loaded(&work.identity, class, elapsed, failed));
        }
        self.count(&format!("load:{}", class.name()), 1);
        self.count(&format!("loadMs:{}", class.name()), elapsed);
        if failed {
            self.count(&format!("loadFail:{}", class.name()), 1);
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
    observer: Arc<dyn LoadObserver>,
    sessions: Mutex<BTreeMap<usize, (Weak<OpenedDataset>, u64)>>,
    next_session: AtomicU64,
    #[cfg(feature = "harness")]
    read_failure: Mutex<Option<dynapi_reader::ReaderError>>,
}
impl DataService {
    pub fn new(config: &Config, clock: SharedClock, catalog: StacCatalog) -> Result<Self, String> {
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
            observer: Arc::new(NoopLoadObserver),
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
            #[cfg(feature = "harness")]
            read_failure: Mutex::new(None),
        })
    }
    pub fn with_load_observer(mut self, observer: Arc<dyn LoadObserver>) -> Self {
        self.observer = observer;
        self
    }
    fn load_work(
        &self,
        dataset: &Arc<OpenedDataset>,
        product: &str,
        selected: &query::PreparedQuery<'_>,
        group: bool,
    ) -> LoadWork {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, (weak, _)| weak.strong_count() > 0);
        let (_, session) = sessions
            .entry(Arc::as_ptr(dataset) as usize)
            .or_insert_with(|| {
                (
                    Arc::downgrade(dataset),
                    self.next_session.fetch_add(1, Ordering::Relaxed),
                )
            });
        LoadWork {
            identity: LoadIdentity {
                product: product.to_owned(),
                snapshot: dataset.snapshot_id().to_owned(),
                session: *session,
                variables: selected.options().variables.clone(),
            },
            read_values: selected.read_values(),
            group,
        }
    }
    #[cfg(feature = "harness")]
    pub fn inject_read_failure(&self, error: dynapi_reader::ReaderError) {
        *self.read_failure.lock().unwrap() = Some(error);
    }
    pub fn warm(&self) {
        self.catalog.warm();
    }
    pub(super) async fn snapshot(&self, usage: &SharedUsage) -> Result<Arc<Snapshot>, QueryError> {
        self.catalog.snapshot().await.map_err(|e| {
            usage.lock().unwrap().error_kind = Some(ErrorKind::CatalogUnavailable);
            QueryError {
                status: 503,
                detail: e.to_string(),
                retry_after: None,
                exception_class: None,
            }
        })
    }
    fn product_in(snapshot: &Snapshot, id: &str, forecast: bool) -> Result<Product, QueryError> {
        let product = snapshot
            .collections
            .get(id)
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
        Ok(product.clone())
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
        profile: Profile,
        handle: &tokio::runtime::Handle,
        timings: &mut Timings,
    ) -> Result<(Arc<OpenedDataset>, dynapi_catalog::cache::CacheStatus), QueryError> {
        let started = Instant::now();
        let result = self.open_unobserved(product, snapshot, profile, handle);
        let elapsed = elapsed_ms(started);
        let status = match &result {
            Ok((_, status)) => match status {
                dynapi_catalog::cache::CacheStatus::Hit => OpenStatus::Hit,
                dynapi_catalog::cache::CacheStatus::Miss => OpenStatus::Miss,
                dynapi_catalog::cache::CacheStatus::Revalidated => OpenStatus::Revalidated,
                dynapi_catalog::cache::CacheStatus::Unknown => OpenStatus::Unknown,
            },
            Err(_) => OpenStatus::Failed,
        };
        telemetry::best_effort(|| self.observer.opened(&product.id, status, elapsed));
        match status {
            OpenStatus::Failed => timings.count("openFail", 1),
            status => {
                let name = match status {
                    OpenStatus::Hit => "hit",
                    OpenStatus::Miss => "miss",
                    OpenStatus::Revalidated => "revalidated",
                    _ => "unknown",
                };
                timings.count(&format!("open:{name}"), 1);
                timings.count(&format!("openMs:{name}"), elapsed);
            }
        }
        result
    }
    fn open_recipe(
        product: &Product,
        snapshot: Option<&str>,
        profile: Profile,
    ) -> Result<(DatasetKey, dynapi_catalog::ReaderConfig), QueryError> {
        let recipe = if profile == Profile::Metadata {
            // Replace all persisted authorization with an empty set. Required
            // coordinates must be physically stored; virtual coordinates fail
            // closed without a virtual fetch. Never share this session with
            // point-serving handles, even when they use the same snapshot.
            dynapi_catalog::ReaderConfig {
                store: product.store_descriptor(),
                containers: Vec::new(),
            }
        } else {
            product
                .reader_config()
                .map_err(|e| QueryError::internal(e.to_string()))?
        };
        Ok((DatasetKey::new(product, snapshot, profile), recipe))
    }
    fn open_unobserved(
        &self,
        product: &Product,
        snapshot: Option<&str>,
        profile: Profile,
        handle: &tokio::runtime::Handle,
    ) -> Result<(Arc<OpenedDataset>, dynapi_catalog::cache::CacheStatus), QueryError> {
        let (key, recipe) = Self::open_recipe(product, snapshot, profile)?;
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
            key,
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
        catalog: Arc<Snapshot>,
        permit: dynapi_policy::Permit,
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
        let mut timings = Timings::new(usage.clone(), self.observer.clone());
        // Admission and caller-location policy completed before store I/O.
        let service = self.clone();
        let result = run_with_permit(permit, move |handle| {
            let resolved: Vec<std::sync::OnceLock<(Arc<OpenedDataset>, Collection)>> =
                (0..payload.queries.len())
                    .map(|_| std::sync::OnceLock::new())
                    .collect();
            let mut budget = service.budget();
            let mut builder = query::BatchBuilder::new(Kind::Forecast);
            let mut opened = Vec::new();
            // Keep the exact catalog generation checked before store I/O.
            let snapshot = catalog;
            for (q, cell) in payload.queries.iter().zip(&resolved) {
                timings.start("catalog");
                let product = Self::product_in(&snapshot, &q.data_product_id, true)?;
                if q.variables.iter().all(|v| {
                    v.trim_matches(dynapi_contract::output::python_whitespace)
                        .is_empty()
                }) {
                    return Err(QueryError::invalid("At least one variable is required"));
                }
                timings.finish("catalog");
                timings.start("dataset_open");
                let (dataset, status) =
                    service.open(&product, None, Profile::Default, &handle, &mut timings)?;
                timings.finish("dataset_open");
                timings.cache(status);
                let (dataset, c) = cell.get_or_init(|| (dataset, collection(&product)));
                builder.push_forecast(
                    dataset.view(),
                    c,
                    dataset.snapshot_id(),
                    q,
                    &mut budget,
                    &mut timings,
                )?;
                opened.push(dataset.clone());
                bound_selections(builder.selections(), &opened)?;
            }
            let batch = builder.finish(&mut budget, false, 1)?;
            bound_selections(batch.selections(), &opened)?;
            let plan = batch.load_plan();
            timings.count("selections", plan.selections as u64);
            timings.count("ungroupableSelections", plan.ungroupable_selections as u64);
            timings.count("coTiledQueries", plan.co_tiled_queries as u64);
            timings.pending_loads = plan
                .groups
                .iter()
                .flatten()
                .filter_map(|index| {
                    let selected = &batch.selections()[*index];
                    opened
                        .iter()
                        .enumerate()
                        .find(|(_, dataset)| std::ptr::eq(dataset.view(), selected.source()))
                        .map(|(position, dataset)| {
                            service.load_work(
                                dataset,
                                &payload.queries[position].data_product_id,
                                selected,
                                true,
                            )
                        })
                })
                .collect();
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
    pub async fn analyses(
        self: &Arc<Self>,
        payload: AnalysisBatch,
        catalog: Arc<Snapshot>,
        permit: dynapi_policy::Permit,
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
        let mut timings = Timings::new(usage.clone(), self.observer.clone());
        // Admission and caller-location policy completed before store I/O.
        let service = self.clone();
        let result = run_with_permit(permit, move |handle| {
            let resolved: Vec<std::sync::OnceLock<(Arc<OpenedDataset>, Collection)>> =
                (0..payload.queries.len())
                    .map(|_| std::sync::OnceLock::new())
                    .collect();
            let mut budget = service.budget();
            let mut builder = query::BatchBuilder::new(Kind::Analysis);
            let mut opened = Vec::new();
            let mut product_ids = Vec::with_capacity(payload.queries.len());
            // Keep the exact catalog generation whose recipes were checked.
            let snapshot = catalog;
            for (q, cell) in payload.queries.into_iter().zip(&resolved) {
                timings.start("catalog");
                let product = Self::visible_product(&snapshot, &q.data_product_id)?;
                require_kind(&product, "analysis")?;
                if let Some(reason) = product.point_query_reason() {
                    return Err(QueryError::invalid(reason));
                }
                if q.variables.iter().all(|v| {
                    v.trim_matches(dynapi_contract::output::python_whitespace)
                        .is_empty()
                }) {
                    return Err(QueryError::invalid("At least one variable is required"));
                }
                timings.finish("catalog");
                timings.start("dataset_open");
                let (dataset, status) =
                    service.open(&product, None, Profile::Default, &handle, &mut timings)?;
                timings.finish("dataset_open");
                timings.cache(status);
                let (dataset, c) = cell.get_or_init(|| (dataset, collection(&product)));
                product_ids.push(q.data_product_id);
                let input = query::QueryInput {
                    dataset: dataset.view(),
                    collection: c,
                    snapshot_id: dataset.snapshot_id(),
                    query_id: q.query_id,
                    latitude: q.latitude,
                    longitude: q.longitude,
                    variables: q.variables,
                    init_times: Default::default(),
                    max_lead_time_hours: None,
                    start: Some(q.start_time),
                    end: Some(q.end_time),
                };
                builder.push(&input, &mut budget, &mut timings)?;
                opened.push(dataset.clone());
                bound_selections(builder.selections(), &opened)?;
            }
            let batch = builder.finish(&mut budget, false, 1)?;
            bound_selections(batch.selections(), &opened)?;
            let plan = batch.load_plan();
            timings.count("selections", plan.selections as u64);
            timings.count("ungroupableSelections", plan.ungroupable_selections as u64);
            timings.count("coTiledQueries", plan.co_tiled_queries as u64);
            timings.pending_loads = plan
                .groups
                .iter()
                .flatten()
                .filter_map(|index| {
                    let selected = &batch.selections()[*index];
                    opened
                        .iter()
                        .enumerate()
                        .find(|(_, dataset)| std::ptr::eq(dataset.view(), selected.source()))
                        .map(|(position, dataset)| {
                            service.load_work(dataset, &product_ids[position], selected, true)
                        })
                })
                .collect();
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
    /// Run discovery reads only opened coordinates; it does not select weather values.
    pub async fn runs(
        self: &Arc<Self>,
        params: ForecastRunsQuery,
        catalog: Arc<Snapshot>,
        permit: dynapi_policy::Permit,
        usage: SharedUsage,
    ) -> HttpResponse {
        {
            let mut u = usage.lock().unwrap();
            u.products = vec![params.data_product_id.clone()];
            u.query_count = Some(1);
        }
        let service = self.clone();
        let result = run_with_permit(permit, move |handle| {
            let snapshot = catalog;
            let product = Self::visible_product(&snapshot, &params.data_product_id)?;
            require_kind(&product, "forecast")?;
            for (name, date) in [
                ("initTimesAfter", params.init_times_after),
                ("initTimesBefore", params.init_times_before),
            ] {
                if date.is_some_and(|date| date.time.tz_offset.is_none()) {
                    return Err(QueryError::invalid(format!(
                        "{name} must include a timezone"
                    )));
                }
            }
            if let (Some(after), Some(before)) = (params.init_times_after, params.init_times_before)
                && (after.timestamp_tz(), after.time.microsecond)
                    >= (before.timestamp_tz(), before.time.microsecond)
            {
                return Err(QueryError::invalid(
                    "initTimesAfter must be before initTimesBefore",
                ));
            }
            let mut timings = Timings::new(usage.clone(), service.observer.clone());
            let (dataset, _) = service
                .open(&product, None, Profile::Metadata, &handle, &mut timings)
                .map_err(|error| {
                    // The reference GET does not catch dataset-open failures as retryable.
                    // Preserve the deliberately typed metadata-budget 422 shared by data routes.
                    if error.status == 422 {
                        error
                    } else {
                        QueryError::internal(error.detail)
                    }
                })?;
            let runs = query::list_forecast_init_times(
                dataset.view(),
                params.init_times_after,
                params.init_times_before,
                params.limit,
            )?;
            let body = Value::object([
                ("dataProductId", Value::from(product.id)),
                ("snapshotId", Value::from(dataset.snapshot_id())),
                (
                    "runs",
                    Value::Array(
                        runs.into_iter()
                            .map(|run| {
                                Value::object([(
                                    "initTime",
                                    Value::from(query::format_datetime(run)),
                                )])
                            })
                            .collect(),
                    ),
                ),
            ]);
            let encoded = dynapi_contract::output::json(&body, false)
                .map_err(|_| QueryError::internal("Run response encoding failed"))?;
            Ok(response(200, encoded, Some("application/json")))
        })
        .await;
        result.unwrap_or_else(query_error)
    }
    fn visible_product(snapshot: &Snapshot, id: &str) -> Result<Product, QueryError> {
        snapshot
            .collections
            .get(id)
            .filter(|p| matches!(p.kind(), "forecast" | "analysis"))
            .cloned()
            .ok_or_else(|| QueryError {
                status: 404,
                detail: format!(
                    "Unknown data product {}",
                    dynapi_contract::output::python_repr(id)
                ),
                retry_after: None,
                exception_class: None,
            })
    }
    pub async fn canonical(
        self: &Arc<Self>,
        product: Product,
        snapshot: String,
        indices: &str,
        params: CanonicalQuery,
        permit: dynapi_policy::Permit,
        usage: SharedUsage,
    ) -> HttpResponse {
        let started = Instant::now();
        {
            let mut u = usage.lock().unwrap();
            u.products = vec![product.id.clone()];
            u.query_count = Some(1);
        }
        // Admission and caller-location policy completed before store I/O.
        let mut timings = Timings::new(usage.clone(), self.observer.clone());
        timings.start("catalog");
        if let Some(reason) = product.point_query_reason() {
            return query_error(QueryError::invalid(reason));
        }
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
            let (ds, status) = service.open(&product, Some(&snapshot), Profile::Default, &handle, &mut timings).map_err(|mut e| { if e.status == 404 { e.detail = format!("Unknown snapshot {} for data product {}; use the snapshotId from a forecasts or analyses response", dynapi_contract::output::python_repr(&snapshot), dynapi_contract::output::python_repr(&product.id)); } e })?;
            timings.finish("dataset_open");
            timings.cache(status);
            timings.start("point_select");
            // Every out-of-int64 integer is outside the bounded grid as well.
            let index = |n: query::BigInt| n.to_string().parse::<i64>().unwrap_or(-1);
            let point = query::point_from_indices(ds.view(), index(first), index(second))?;
            timings.finish("point_select");
            timings.start("query_select");
            let mut budget = service.budget();
            let selected = query::select_query_bounded_observed(
                ds.view(),
                &c,
                point,
                &options,
                ds.snapshot_id(),
                Some(&mut budget),
                &mut query::MaterializationBudget::default(),
                &mut timings,
            )?;
            budget.consume_read(selected.read_values())?;
            timings.finish("query_select");
            bound_selections(std::slice::from_ref(&selected), std::slice::from_ref(&ds))?;
            timings.pending_loads.push_back(service.load_work(&ds,&product.id,&selected,false));
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
fn require_kind(product: &Product, expected: &str) -> Result<(), QueryError> {
    if product.kind() != expected {
        return Err(QueryError::invalid(format!(
            "Data product {} is {}, not {expected}",
            dynapi_contract::output::python_repr(&product.id),
            product.kind()
        )));
    }
    Ok(())
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
        OpenError::MetadataLimit { limit } => {
            QueryError::invalid(format!("Opened dataset metadata exceeds {limit} bytes"))
        }
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
pub(crate) fn busy(gate: &ComputeGate<SharedClock>) -> HttpResponse {
    let retry = gate.retry_after();
    let mut result = detail(503, &format!("Server busy; retry after {retry} seconds"));
    result
        .headers_mut()
        .insert("retry-after", retry.to_string().parse().unwrap());
    result.extensions_mut().insert(ErrorKind::ServerBusy);
    result
}

fn elapsed_ms(started: Instant) -> u64 {
    (started.elapsed().as_secs_f64() * 1000.0).round_ties_even() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_temporary_data_service_does_not_close_the_shared_catalog() {
        let catalog = StacCatalog::new(
            "https://catalog.example.test/catalog.json",
            dynapi_catalog::refresh::CatalogConfig::default(),
        )
        .unwrap();
        let config = Config::from_values(&BTreeMap::new()).unwrap();
        drop(
            DataService::new(
                &config,
                Arc::new(crate::Monotonic::default()),
                catalog.clone(),
            )
            .unwrap(),
        );
        // Outside a runtime, an open catalog records the expected warm failure.
        // A prematurely closed catalog cannot begin that flight at all.
        catalog.warm();
        assert_eq!(catalog.status().consecutive_failures, 1);
        assert_eq!(
            catalog.status().last_error.as_deref(),
            Some("STAC catalog refresh failed (RuntimeError)")
        );
        catalog.close();
    }
    #[test]
    fn observer_failures_do_not_interrupt_a_load_or_lose_request_counters() {
        struct Broken;
        impl LoadObserver for Broken {
            fn classify(&self, _: &LoadIdentity) -> LoadClass {
                panic!("synthetic observer failure");
            }
            fn loaded(&self, _: &LoadIdentity, _: LoadClass, _: u64, _: bool) {
                panic!("synthetic observer failure");
            }
        }
        let usage = SharedUsage::default();
        let mut timings = Timings::new(usage.clone(), Arc::new(Broken));
        timings.pending_loads.push_back(LoadWork {
            identity: LoadIdentity {
                product: "example-forecast".into(),
                snapshot: "snapshot-test".into(),
                session: 1,
                variables: vec!["temperature".into()],
            },
            read_values: 2,
            group: true,
        });
        timings.start("zarr_load");
        timings.finish("zarr_load");
        assert!(timings.active_load.is_none());
        let usage = usage.lock().unwrap();
        assert_eq!(usage.counters["load:warm"], 1);
        assert_eq!(usage.counters["readGroups"], 1);
        assert_eq!(usage.counters["groupedReadValues"], 2);
    }

    fn recipe_product(repository: &str, kind: &str) -> Product {
        let prefix = if kind == "s3" {
            "s3://example-chunks/data/"
        } else {
            "https://chunks.dynamical.org/example/"
        };
        let mut value = serde_json::json!({"id":"example-forecast","assets":{"icechunk":{
            "href":repository,
            "icechunk:virtual_chunk_containers":[{"url_prefix":prefix,"credentials":{"type":kind,"anonymous":true}}]
        }}});
        if repository.starts_with("s3://") {
            value["assets"]["icechunk"]["xarray:storage_options"] =
                serde_json::json!({"client_kwargs":{"region_name":"us-west-2"}});
        }
        Product::from_stac(&value).unwrap()
    }

    #[test]
    fn metadata_open_recipe_removes_http_and_s3_virtual_authorization() {
        use dynapi_reader::StoreDescriptor;
        for repository in [
            "https://data.dynamical.org/example/repository/",
            "s3://example-bucket/data/",
        ] {
            for kind in ["http", "s3"] {
                let product = recipe_product(repository, kind);
                assert_eq!(product.virtual_chunk_containers().len(), 1);
                let (_, recipe) =
                    DataService::open_recipe(&product, None, Profile::Metadata).unwrap();
                assert!(
                    recipe.containers.is_empty(),
                    "metadata authorized {kind} containers"
                );
                // Full reads reject S3 authorization today. Clearing only the
                // declarations lets its repository mapping be compared anyway.
                let mut stored_only = product.clone();
                stored_only.repository_config["virtual_chunk_containers"] = serde_json::json!([]);
                let full = stored_only.reader_config().unwrap();
                match (recipe.store, full.store) {
                    (StoreDescriptor::Http { url }, StoreDescriptor::Http { url: full_url }) => {
                        assert_eq!(url, full_url);
                        assert_eq!(url, "https://data.dynamical.org/example/repository");
                    }
                    (
                        StoreDescriptor::AnonymousS3 {
                            bucket,
                            prefix,
                            region,
                        },
                        StoreDescriptor::AnonymousS3 {
                            bucket: full_bucket,
                            prefix: full_prefix,
                            region: full_region,
                        },
                    ) => {
                        assert_eq!(
                            (&bucket, &prefix, &region),
                            (&full_bucket, &full_prefix, &full_region)
                        );
                        assert_eq!(
                            (bucket.as_str(), prefix.as_str(), region.as_str()),
                            ("example-bucket", "data", "us-west-2")
                        );
                    }
                    stores => panic!("different repository mappings: {stores:?}"),
                }
            }
        }
    }

    #[test]
    fn metadata_and_full_open_recipe_keys_keep_cache_handles_separate() {
        use dynapi_catalog::cache::CacheStatus;
        let product = recipe_product("https://data.dynamical.org/example/repository/", "http");
        for snapshot in [None, Some("example-snapshot")] {
            for profiles in [
                [Profile::Default, Profile::Metadata],
                [Profile::Metadata, Profile::Default],
            ] {
                let clock: SharedClock = Arc::new(crate::Monotonic::default());
                let cache = DatasetCache::with_defaults(clock);
                let first_key = DataService::open_recipe(&product, snapshot, profiles[0])
                    .unwrap()
                    .0;
                let second_key = DataService::open_recipe(&product, snapshot, profiles[1])
                    .unwrap()
                    .0;
                assert_ne!(first_key, second_key);
                assert_ne!(first_key.repository_key(), second_key.repository_key());
                // Exercise the actual recipe keys, including both opening
                // orders: a metadata session must neither inherit nor remove
                // the full profile's authorization through cache reuse.
                for (index, profile) in profiles.into_iter().cycle().take(4).enumerate() {
                    let key = DataService::open_recipe(&product, snapshot, profile)
                        .unwrap()
                        .0;
                    let opened = cache
                        .open(
                            key,
                            || {
                                Ok::<_, QueryError>(Resolved {
                                    snapshot_id: "example-snapshot".into(),
                                    session: (),
                                })
                            },
                            |_| Ok(profile),
                        )
                        .unwrap();
                    assert_eq!(
                        opened.cache_status,
                        if index < 2 {
                            CacheStatus::Miss
                        } else {
                            CacheStatus::Hit
                        }
                    );
                    assert_eq!(*opened.dataset, profile);
                }
                assert_eq!(cache.len(), 2);
            }
        }
    }

    #[test]
    fn only_the_typed_metadata_budget_open_failure_becomes_422() {
        let limit = 64 * 1024 * 1024;
        let error = map_open(OpenError::MetadataLimit { limit });
        assert_eq!(error.status, 422);
        assert_eq!(
            error.detail,
            "Opened dataset metadata exceeds 67108864 bytes"
        );
        assert_eq!(error.retry_after, None);
        let error = map_open(OpenError::Metadata(
            "Opened dataset metadata exceeds 67108864 bytes".into(),
        ));
        assert_eq!(error.status, 500);
        for kind in [
            dynapi_reader::ErrorKind::Unsupported,
            dynapi_reader::ErrorKind::SizeLimit,
        ] {
            let error = map_open(OpenError::Reader(dynapi_reader::ReaderError {
                kind,
                message: "synthetic reader failure".into(),
                virtual_chunk_fetch: false,
                exception_class: None,
            }));
            assert_eq!(error.status, 500);
        }
    }
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
