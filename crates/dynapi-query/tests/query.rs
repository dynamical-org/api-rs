use dynapi_contract::{validate_forecast, ValidationResult, Value};
use dynapi_query::*;
use std::collections::BTreeMap;

// A single point, one initialization, and two hourly values in source order.
fn forecast(run: i64, leads: Vec<i64>) -> (DatasetView, Collection) {
    let dimensions = ["init_time", "lead_time", "latitude", "longitude"].map(String::from);
    let view = DatasetView {
        dimensions: dimensions
            .iter()
            .cloned()
            .zip([1, leads.len(), 1, 1])
            .collect(),
        coordinates: BTreeMap::from([
            ("init_time".into(), Coordinate::Time(vec![run])),
            ("lead_time".into(), Coordinate::Duration(leads)),
            ("latitude".into(), Coordinate::Numeric(vec![0.0])),
            ("longitude".into(), Coordinate::Numeric(vec![0.0])),
        ]),
        variables: BTreeMap::from([(
            "temperature".into(),
            Variable {
                is_coordinate: false,
                dimensions: dimensions.to_vec(),
                chunks: Some(vec![Some(1); 4]),
                ..Variable::default()
            },
        )]),
        ..DatasetView::default()
    };
    let collection = Collection {
        id: "example-forecast".into(),
        kind: Kind::Forecast,
        version: None,
        catalog_url: None,
        variables: BTreeMap::from([(
            "temperature".into(),
            CatalogVariable {
                dimensions: dimensions.to_vec(),
                unit: Some("K".into()),
            },
        )]),
        point_query_reason: None,
    };
    (view, collection)
}

fn input<'a>(view: &'a DatasetView, collection: &'a Collection) -> QueryInput<'a> {
    QueryInput {
        dataset: view,
        collection,
        snapshot_id: "snapshot-test",
        query_id: None,
        latitude: 0.0,
        longitude: 0.0,
        variables: vec!["temperature".into()],
        init_times: InitTimeSelection::default(),
        max_lead_time_hours: None,
        start: None,
        end: None,
    }
}

#[derive(Default)]
struct MemoryReader {
    reads: usize,
    fail: Option<QueryError>,
}

impl DataReader for MemoryReader {
    fn read(
        &mut self,
        _: &DatasetView,
        name: &str,
        positions: &Positions,
    ) -> Result<Vec<Value>, QueryError> {
        self.reads += 1;
        if let Some(error) = &self.fail {
            return Err(error.clone());
        }
        assert_eq!(name, "temperature");
        assert_eq!(positions["init_time"], Selection::Index(0));
        assert_eq!(positions["latitude"], Selection::Index(0));
        assert_eq!(positions["longitude"], Selection::Index(0));
        let Selection::Indices(leads) = &positions["lead_time"] else {
            panic!("lead axis must be retained");
        };
        // Fixture values are 10 K at lead zero and 20 K at lead one.
        Ok(leads
            .iter()
            .map(|&index| Value::from([10_i64, 20][index]))
            .collect())
    }
}

#[test]
fn transport_errors_are_explicit_and_retryable_only_when_classified() {
    let (view, collection) = forecast(0, vec![0, HOUR]);
    let batch = prepare_batch(
        Kind::Forecast,
        &[input(&view, &collection), input(&view, &collection)],
        &mut QueryBudget::default(),
        true,
        64,
    )
    .unwrap();
    assert_eq!(batch.selections().len(), 2);
    for error in [
        QueryError::upstream_unavailable(),
        QueryError::internal("permanent storage failure"),
    ] {
        let mut reader = MemoryReader {
            fail: Some(error.clone()),
            ..MemoryReader::default()
        };
        assert_eq!(batch.load(&mut reader).unwrap_err(), error);
        assert_eq!(reader.reads, 1);
    }
    let timeout = "operation timeout (all attempts including retries) occurred after 5s";
    let transient = QueryError::from_storage_failure(true, timeout);
    assert_eq!((transient.status, transient.retry_after), (503, Some(5)));
    for error in [
        QueryError::from_storage_failure(false, timeout),
        QueryError::from_storage_failure(true, "HTTP 403 Forbidden"),
    ] {
        assert_eq!((error.status, error.retry_after), (500, None));
    }
}

#[test]
fn separate_views_do_not_share_load_groups() {
    let (a, collection) = forecast(0, vec![0, HOUR]);
    let b = a.clone();
    let batch = prepare_batch(
        Kind::Forecast,
        &[input(&a, &collection), input(&b, &collection)],
        &mut QueryBudget::default(),
        true,
        64,
    )
    .unwrap();
    assert_eq!(batch.load_plan().groups, vec![vec![0], vec![1]]);
}

#[test]
fn validated_request_domain_reaches_execution_and_canonical_encoding() {
    // 2016-01-01 is 46 Gregorian years after 1970, including eleven leap days.
    let run = (46 * 365 + 11) * 86_400_000_000_000_i64;
    let (view, collection) = forecast(run, vec![0, HOUR]);
    let request = br#"{"queries":[{"dataProductId":"example-forecast","location":{"latitude":0,"longitude":0},"variables":["temperature"],"maxLeadTimeHours":1000000000000000000000000000000,"validTimeStart":"2600-01-01T00:00:00Z","validTimeEnd":"2620-01-01T00:00:00Z"}]}"#;
    let ValidationResult::Valid(validated) = validate_forecast(request, Some("application/json"))
    else {
        panic!("fixture request must validate");
    };
    let input =
        QueryInput::from_forecast(&view, &collection, "snapshot-test", &validated.queries[0]);
    assert_eq!(input.start.unwrap().date.year, 2600);
    assert_eq!(input.end.unwrap().date.year, 2620);
    assert_eq!(input.max_lead_time_hours, Some(BigInt::from(10).pow(30)));
    let batch = prepare_batch(
        Kind::Forecast,
        &[input],
        &mut QueryBudget::default(),
        true,
        64,
    )
    .unwrap();
    assert_eq!(batch.selections().len(), 1);
    let selected = &batch.selections()[0];
    let canonical = "/v1/data/example-forecast/snapshot-test/points/0,0?variables=temperature&initTime=2016-01-01T00%3A00%3A00Z&maxLeadTimeHours=1000000000000000000000000000000&validTimeStart=2600-01-01T00%3A00%3A00Z&validTimeEnd=2620-01-01T00%3A00%3A00Z";
    assert_eq!(
        canonical_path(
            &collection,
            "snapshot-test",
            selected.point(),
            selected.options()
        ),
        canonical
    );
    let mut reader = MemoryReader::default();
    let response = batch
        .execute(&mut reader, &ResponseMetadata::default())
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    let result = &body["results"][0];
    assert_eq!(result["snapshotId"], "snapshot-test");
    assert_eq!(
        result["forecasts"],
        serde_json::json!([{
            "initTime": "2016-01-01T00:00:00Z",
            "validTimes": ["2016-01-01T00:00:00Z", "2016-01-01T01:00:00Z"],
            "leadTimeHours": [0.0, 1.0],
            "links": {"canonical": canonical},
            "data": {"temperature": [10, 20]}
        }])
    );
    assert_eq!(reader.reads, 1);

    // UTC normalization of a schema-valid civil date can fail before any budget/read.
    let mut input =
        QueryInput::from_forecast(&view, &collection, "snapshot-test", &validated.queries[0]);
    input.start = Some(RequestDatetime::parse_str("0001-01-01T00:00:00+01:00").unwrap());
    let mut budget = QueryBudget::default();
    let error = prepare_batch(Kind::Forecast, &[input], &mut budget, true, 64).unwrap_err();
    assert_eq!(
        (error.status, error.exception_class, error.detail.as_str()),
        (500, Some("OverflowError"), "date value out of range")
    );
    assert_eq!((budget.used_values, budget.used_read_values), (0, 0));
}

#[test]
fn valid_time_arithmetic_precedes_masks_budgets_and_reads() {
    for (run, lead) in [
        (1_000, i64::MAX),
        (-1_000, i64::MIN + 1),
        (-1_000, i64::MIN + 1_000),
    ] {
        // The first lead is valid. The second must fail even though the zero-hour
        // limit, end bound and zero budgets would otherwise discard/reject it.
        let (view, collection) = forecast(run, vec![0, lead]);
        let mut query = input(&view, &collection);
        query.max_lead_time_hours = Some(0.into());
        query.end = Some(RequestDatetime::parse_str("1970-01-01T00:00:00Z").unwrap());
        let mut budget = QueryBudget {
            max_values: 0,
            max_read_values: 0,
            ..QueryBudget::default()
        };
        let mut reader = MemoryReader::default();
        let result = prepare_batch(Kind::Forecast, &[query], &mut budget, true, 64)
            .and_then(|batch| batch.execute(&mut reader, &ResponseMetadata::default()));
        let error = result.unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.exception_class, Some("OverflowError"));
        assert_eq!(
            error.detail,
            "Overflow in datetime64 + timedelta64 addition"
        );
        assert_eq!(reader.reads, 0);
        assert_eq!((budget.used_values, budget.used_read_values), (0, 0));
    }
}

#[test]
fn lead_limit_conversion_precedes_addition_and_bound_conversion_follows_it() {
    // A 1-us run plus i64::MAX overflows, independently of civil bound handling.
    let (view, collection) = forecast(1_000, vec![i64::MAX]);
    let point = select_nearest_point(&view, 0.0, 0.0).unwrap();
    for (max_lead, expected) in [
        (
            Some(BigInt::from(10).pow(1_000)),
            "int too large to convert to float",
        ),
        (
            Some(0.into()),
            "Overflow in datetime64 + timedelta64 addition",
        ),
    ] {
        let mut budget = QueryBudget::default();
        let mut reader = MemoryReader::default();
        let options = QueryOptions {
            variables: vec!["temperature".into()],
            run: Some(RequestDatetime::parse_str("1970-01-01T00:00:00.000001Z").unwrap()),
            max_lead_time_hours: max_lead,
            start: Some(RequestDatetime::parse_str("0001-01-01T00:00:00+01:00").unwrap()),
            ..QueryOptions::default()
        };
        let error = select_query(
            &view,
            &collection,
            point.clone(),
            options,
            "snapshot-test",
            Some(&mut budget),
        )
        .and_then(|query| load_query(&query, &mut reader))
        .unwrap_err();
        assert_eq!(
            (error.status, error.exception_class),
            (500, Some("OverflowError"))
        );
        assert_eq!(error.detail, expected);
        assert_eq!(reader.reads, 0);
        assert_eq!((budget.used_values, budget.used_read_values), (0, 0));
    }
    // With finite addition, the invalid UTC bound becomes the first failure.
    let (view, collection) = forecast(0, vec![0]);
    let options = QueryOptions {
        variables: vec!["temperature".into()],
        run: Some(RequestDatetime::parse_str("1970-01-01T00:00:00Z").unwrap()),
        start: Some(RequestDatetime::parse_str("0001-01-01T00:00:00+01:00").unwrap()),
        ..QueryOptions::default()
    };
    let mut budget = QueryBudget::default();
    let mut reader = MemoryReader::default();
    let error = select_query(
        &view,
        &collection,
        select_nearest_point(&view, 0.0, 0.0).unwrap(),
        options,
        "snapshot-test",
        Some(&mut budget),
    )
    .and_then(|query| load_query(&query, &mut reader))
    .unwrap_err();
    assert_eq!(
        (error.status, error.exception_class),
        (500, Some("OverflowError"))
    );
    assert_eq!(error.detail, "date value out of range");
    assert_eq!(reader.reads, 0);
    assert_eq!((budget.used_values, budget.used_read_values), (0, 0));
}

#[test]
fn geographic_xy_global_grid_normalizes_both_hemispheres_and_seam() {
    let view = DatasetView {
        dimensions: [("y".into(), 721), ("x".into(), 1440)].into(),
        coordinates: [
            (
                "y".into(),
                Coordinate::Numeric((0..721).map(|i| -90. + i as f64 / 4.).collect()),
            ),
            (
                "x".into(),
                Coordinate::Numeric((0..1440).map(|i| i as f64 / 4.).collect()),
            ),
        ]
        .into(),
        spatial_crs: Some(SpatialCrs::Geographic),
        ..DatasetView::default()
    };
    // Explicit quarter-degree grid expectations, including ties (first index wins).
    for (latitude, longitude, y, x, selected_lat, selected_lon) in [
        (43.2, -100.1, 533, 1040, 43.25, 260.),
        (-33.9, 151.2, 224, 605, -34., 151.25),
        (-90., -180., 0, 720, -90., 180.),
        (90., 180., 720, 720, 90., 180.),
        (0., -0.1, 360, 0, 0., 0.),
        (0., -0.25, 360, 1439, 0., 359.75),
        (0.125, 0.125, 360, 0, 0., 0.),
    ] {
        let point = select_nearest_point(&view, latitude, longitude).unwrap();
        assert_eq!(point.indexers, [("y".into(), y), ("x".into(), x)]);
        assert_eq!(
            (point.latitude, point.longitude),
            (selected_lat, selected_lon)
        );
    }
}

#[test]
fn run_listing_sorts_unique_full_precision_times_drops_nat_and_filters_strictly() {
    let (mut view, _) = forecast(0, vec![0]);
    view.coordinates.insert(
        "init_time".into(),
        Coordinate::Time(vec![HOUR, 1, NAT, 0, HOUR, 2]),
    );
    view.dimensions.insert("init_time".into(), 6);
    let runs = list_forecast_init_times(&view, None, None, 20).unwrap();
    // Distinct nanosecond labels remain distinct even when presentation rounds them.
    assert_eq!(runs, [i128::from(HOUR), 0, 0, 0]);
    assert_eq!(
        list_forecast_init_times(&view, None, None, 1).unwrap(),
        runs[..1]
    );
    let bound = RequestDatetime::parse_str("1970-01-01T00:00:00Z").unwrap();
    let before = RequestDatetime::parse_str("1970-01-01T01:00:00Z").unwrap();
    let filtered = list_forecast_init_times(&view, Some(bound), Some(before), 20).unwrap();
    assert_eq!(filtered, [0, 0]);
    view.coordinates.remove("init_time");
    assert_eq!(
        list_forecast_init_times(&view, None, None, 20)
            .unwrap_err()
            .detail,
        "Forecast data product is missing its init_time coordinate"
    );
}

#[test]
fn analysis_windows_preserve_unsorted_duplicate_source_times_and_exclude_end() {
    let (mut view, mut collection) = forecast(0, vec![0]);
    view.dimensions = [
        ("time".into(), 6),
        ("latitude".into(), 1),
        ("longitude".into(), 1),
    ]
    .into();
    view.coordinates.remove("init_time");
    view.coordinates.remove("lead_time");
    view.coordinates.insert(
        "time".into(),
        Coordinate::Time(vec![NAT, HOUR, 0, HOUR, 2 * HOUR, HOUR + 1]),
    );
    let dimensions = vec!["time".into(), "latitude".into(), "longitude".into()];
    let variable = view.variables.get_mut("temperature").unwrap();
    variable.dimensions = dimensions.clone();
    variable.chunks = Some(vec![Some(2), Some(1), Some(1)]);
    collection.kind = Kind::Analysis;
    collection
        .variables
        .get_mut("temperature")
        .unwrap()
        .dimensions = dimensions;
    let point = select_nearest_point(&view, 0., 0.).unwrap();
    let selected = select_query(
        &view,
        &collection,
        point,
        QueryOptions {
            variables: vec!["temperature".into()],
            start: Some(RequestDatetime::parse_str("1970-01-01T00:00:00Z").unwrap()),
            end: Some(RequestDatetime::parse_str("1970-01-01T02:00:00Z").unwrap()),
            ..QueryOptions::default()
        },
        "snapshot-test",
        None,
    )
    .unwrap();
    assert_eq!(
        selected.positions()["time"],
        Selection::Indices(vec![1, 2, 3, 5])
    );
    assert_eq!(selected.value_count(), 4);
}
