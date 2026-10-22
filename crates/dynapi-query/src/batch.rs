use crate::response::{append_data, catalog, field, forecast, point_body, product, variable_units};
use crate::selection::checked_sum;
use crate::*;

/// The server resolves products and opens views before invoking this pure layer.
/// Request/schema validation and authorization remain the server's responsibility.
#[derive(Clone, Debug)]
pub struct QueryInput<'a> {
    pub dataset: &'a DatasetView,
    pub collection: &'a Collection,
    pub snapshot_id: &'a str,
    pub query_id: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub variables: Vec<String>,
    pub init_times: InitTimeSelection,
    pub max_lead_time_hours: Option<BigInt>,
    pub start: Option<RequestDatetime>,
    pub end: Option<RequestDatetime>,
}
impl<'a> QueryInput<'a> {
    /// Transfer an already validated contract request without narrowing its values.
    pub fn from_forecast(
        dataset: &'a DatasetView,
        collection: &'a Collection,
        snapshot_id: &'a str,
        query: &dynapi_contract::ForecastQuery,
    ) -> Self {
        use dynapi_contract::validation::InitTime;
        Self {
            dataset,
            collection,
            snapshot_id,
            query_id: query.query_id.clone(),
            latitude: query.latitude,
            longitude: query.longitude,
            variables: query.variables.clone(),
            init_times: InitTimeSelection {
                exact: match query.init_time {
                    Some(InitTime::Exact(date)) => Some(date),
                    _ => None,
                },
                latest: matches!(query.init_time, Some(InitTime::Latest)),
                after: query.init_times_after,
                before: query.init_times_before,
                ..InitTimeSelection::default()
            },
            max_lead_time_hours: query.max_lead_time_hours.clone(),
            start: query.valid_time_start,
            end: query.valid_time_end,
        }
    }
}
#[derive(Debug)]
struct PreparedResult {
    query_id: Option<String>,
    latitude: f64,
    longitude: f64,
    selections: std::ops::Range<usize>,
}
/// Constructed only after all selections and read-budget admission succeed.
/// The only high-level execution entry point accepts this admitted batch.
#[derive(Debug)]
pub struct PreparedBatch<'a> {
    queries: Vec<PreparedQuery<'a>>,
    results: Vec<PreparedResult>,
    plan: LoadPlan,
    work_units: u64,
    read_values: u64,
}
impl<'a> PreparedBatch<'a> {
    pub fn selections(&self) -> &[PreparedQuery<'a>] {
        &self.queries
    }
    pub fn load_plan(&self) -> &LoadPlan {
        &self.plan
    }
    pub fn work_units(&self) -> u64 {
        self.work_units
    }
    pub fn read_values(&self) -> u64 {
        self.read_values
    }
    pub fn load(&self, reader: &mut impl DataReader) -> Result<Value, QueryError> {
        self.load_observed(reader, &mut NoopObserver)
    }
    pub fn load_observed(
        &self,
        reader: &mut impl DataReader,
        observer: &mut impl PhaseObserver,
    ) -> Result<Value, QueryError> {
        // A deliberately simple executor. The complete physical co-tile plan is
        // exposed for a vectorizing server; this fallback reads per selection.
        let mut raws = vec![None; self.queries.len()];
        for members in &self.plan.groups {
            for index in members {
                raws[*index] = Some(crate::response::load_query_observed(
                    &self.queries[*index],
                    reader,
                    observer,
                )?);
            }
        }
        let mut results = Vec::new();
        for result in &self.results {
            let first = &self.queries[result.selections.start];
            let raw = raws[result.selections.start].as_ref().unwrap();
            let mut pairs = Vec::new();
            if let Some(id) = &result.query_id {
                pairs.push(("queryId".into(), Value::from(id.clone())));
            }
            pairs.push(("dataProduct".into(), product(first.collection)));
            pairs.push((
                "point".into(),
                point_body(&first.point, Some((result.latitude, result.longitude))),
            ));
            if first.collection.kind == Kind::Forecast {
                let forecasts = result
                    .selections
                    .clone()
                    .map(|i| forecast(raws[i].as_ref().unwrap()))
                    .collect();
                pairs.push(("forecasts".into(), Value::Array(forecasts)));
                let last = raws[result.selections.end - 1].as_ref().unwrap();
                pairs.push(("variables".into(), variable_units(last)));
                pairs.push(("snapshotId".into(), first.snapshot_id.clone().into()));
                pairs.push(("links".into(), catalog(first.collection)));
            } else {
                pairs.push(("analysisTimes".into(), field(raw, "time")));
                pairs.push(("variables".into(), variable_units(raw)));
                pairs.push(("snapshotId".into(), first.snapshot_id.clone().into()));
                pairs.push(("links".into(), field(raw, "links")));
                append_data(&mut pairs, raw);
            }
            results.push(Value::Object(pairs));
        }
        Ok(Value::object([("results", Value::Array(results))]))
    }
    pub fn execute(
        &self,
        reader: &mut impl DataReader,
        metadata: &ResponseMetadata,
    ) -> Result<QueryResponse, QueryError> {
        encode_response(&self.load(reader)?, self.work_units, metadata)
    }
}
/// Observer boundaries preserve phase insertion order without a native clock.
pub trait PhaseObserver {
    fn start(&mut self, _name: &'static str) {}
    fn finish(&mut self, _name: &'static str) {}
    fn read_values(&mut self, _values: u64) {}
}
pub struct NoopObserver;
impl PhaseObserver for NoopObserver {}

pub fn prepare_batch<'a>(
    kind: Kind,
    inputs: &[QueryInput<'a>],
    budget: &mut QueryBudget,
    grouped: bool,
    max_group_points: usize,
) -> Result<PreparedBatch<'a>, QueryError> {
    prepare_batch_observed(
        kind,
        inputs,
        budget,
        grouped,
        max_group_points,
        &mut NoopObserver,
    )
}
pub fn prepare_batch_observed<'a>(
    kind: Kind,
    inputs: &[QueryInput<'a>],
    budget: &mut QueryBudget,
    grouped: bool,
    max_group_points: usize,
    observer: &mut impl PhaseObserver,
) -> Result<PreparedBatch<'a>, QueryError> {
    let mut builder = BatchBuilder::new(kind);
    for input in inputs {
        builder.push(input, budget, observer)?;
    }
    builder.finish(budget, grouped, max_group_points)
}
/// Incremental selection keeps catalog/open/selection errors in request order.
/// No reads are admitted until every input has charged its response budget.
pub struct BatchBuilder<'a> {
    kind: Kind,
    queries: Vec<PreparedQuery<'a>>,
    results: Vec<PreparedResult>,
    bytes: MaterializationBudget,
}
impl<'a> BatchBuilder<'a> {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            queries: vec![],
            results: vec![],
            bytes: MaterializationBudget::default(),
        }
    }
    pub fn selections(&self) -> &[PreparedQuery<'a>] {
        &self.queries
    }
    /// Admit the server's owned contract-to-query copies before making them.
    pub fn push_forecast(
        &mut self,
        dataset: &'a DatasetView,
        collection: &'a Collection,
        snapshot_id: &'a str,
        query: &dynapi_contract::ForecastQuery,
        budget: &mut QueryBudget,
        observer: &mut impl PhaseObserver,
    ) -> Result<(), QueryError> {
        self.bytes
            .copy_labels(&query.variables, query.query_id.as_deref())?;
        self.push(
            &QueryInput::from_forecast(dataset, collection, snapshot_id, query),
            budget,
            observer,
        )
    }
    pub fn push(
        &mut self,
        input: &QueryInput<'a>,
        budget: &mut QueryBudget,
        observer: &mut impl PhaseObserver,
    ) -> Result<(), QueryError> {
        let Self {
            kind,
            queries,
            results,
            bytes,
        } = self;
        let kind = *kind;
        let collection = input.collection;
        if collection.kind != kind {
            return Err(QueryError::invalid(format!(
                "Data product {} is {}, not {}",
                dynapi_contract::output::python_repr(&collection.id),
                collection.kind.as_str(),
                kind.as_str()
            )));
        }
        if let Some(reason) = &collection.point_query_reason {
            return Err(QueryError::invalid(reason));
        }
        bytes.copy_labels(&input.variables, input.query_id.as_deref())?;
        let variables = dynapi_contract::output::normalize_variables(&input.variables);
        if variables.is_empty() {
            return Err(QueryError::invalid("At least one variable is required"));
        }
        observer.start("point_select");
        let point = select_nearest_point(input.dataset, input.latitude, input.longitude)?;
        observer.finish("point_select");
        observer.start("run_select");
        let start = queries.len();
        let runs = if kind == Kind::Forecast {
            let mut selection = input.init_times.clone();
            if selection.exact.is_none() && selection.after.is_none() && selection.before.is_none()
            {
                selection.latest = true;
            }
            select_forecast_init_times(input.dataset, &selection)?
                .into_iter()
                .map(Some)
                .collect()
        } else {
            vec![None]
        };
        observer.finish("run_select");
        // One normalized option list is shared by the run loop. Each retained
        // selection admits its own option clone before copying any label.
        let mut options = QueryOptions {
            variables,
            max_lead_time_hours: input.max_lead_time_hours.clone(),
            ..QueryOptions::default()
        };
        for run in runs {
            observer.start("query_select");
            options.run = run.map(crate::selection::datetime_from_label);
            options.run_selection = run.map(|_| "explicit".into());
            options.start = input
                .start
                .map(crate::selection::utc_datetime)
                .transpose()?;
            options.end = input.end.map(crate::selection::utc_datetime).transpose()?;
            queries.push(select_query_bounded_observed(
                input.dataset,
                collection,
                point.clone(),
                &options,
                input.snapshot_id,
                Some(budget),
                bytes,
                observer,
            )?);
            observer.finish("query_select");
        }
        results.push(PreparedResult {
            query_id: input.query_id.clone(),
            latitude: input.latitude,
            longitude: input.longitude,
            selections: start..queries.len(),
        });
        Ok(())
    }
    pub fn finish(
        self,
        budget: &mut QueryBudget,
        grouped: bool,
        max_group_points: usize,
    ) -> Result<PreparedBatch<'a>, QueryError> {
        let Self {
            queries, results, ..
        } = self;
        let read_values = checked_sum(queries.iter().map(|q| q.read_values))?;
        budget.consume_read(read_values)?;
        let plan = plan_load_groups(&queries, grouped, max_group_points)?;
        Ok(PreparedBatch {
            queries,
            results,
            plan,
            work_units: budget.used_values,
            read_values,
        })
    }
}
