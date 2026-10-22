use dynapi_contract::Value;
use dynapi_dataset::*;
use dynapi_query::*;
use serde_json::Value as Json;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}
fn manifest() -> Json {
    serde_json::from_slice(&std::fs::read(fixtures().join("manifest.json")).unwrap()).unwrap()
}
fn input(name: &str) -> Json {
    serde_json::from_slice(&std::fs::read(fixtures().join(name).join("input.json")).unwrap())
        .unwrap()
}
fn descriptor(name: &str) -> StoreDescriptor {
    StoreDescriptor::Local(
        fixtures()
            .join(name)
            .join("repository")
            .to_str()
            .unwrap()
            .into(),
    )
}
fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().into())
        .collect()
}
fn collection(row: &Json) -> Collection {
    let c = &row["collection"];
    Collection {
        id: c["id"].as_str().unwrap().into(),
        kind: if row["kind"] == "forecast" {
            Kind::Forecast
        } else {
            Kind::Analysis
        },
        version: Some(c["version"].as_str().unwrap().into()),
        catalog_url: Some(c["links"][0]["href"].as_str().unwrap().into()),
        variables: c["cube:variables"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(n, v)| {
                (
                    n.clone(),
                    CatalogVariable {
                        dimensions: strings(&v["dimensions"]),
                        unit: v["unit"].as_str().map(str::to_owned),
                    },
                )
            })
            .collect(),
        point_query_reason: None,
    }
}
fn date(j: &Json) -> Option<RequestDatetime> {
    j.as_str().map(|s| RequestDatetime::parse_str(s).unwrap())
}
fn inputs<'a>(case: &Json, dataset: &'a OpenedDataset, c: &'a Collection) -> Vec<QueryInput<'a>> {
    case["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| QueryInput {
            dataset: dataset.view(),
            collection: c,
            snapshot_id: dataset.snapshot_id(),
            query_id: q["queryId"].as_str().map(str::to_owned),
            latitude: q["location"]["latitude"].as_f64().unwrap(),
            longitude: q["location"]["longitude"].as_f64().unwrap(),
            variables: strings(&q["variables"]),
            init_times: InitTimeSelection {
                exact: if q["initTime"] == "latest" {
                    None
                } else {
                    date(&q["initTime"])
                },
                latest: q["initTime"] == "latest",
                after: date(&q["initTimesAfter"]),
                before: date(&q["initTimesBefore"]),
                ..InitTimeSelection::default()
            },
            max_lead_time_hours: q["maxLeadTimeHours"].as_i64().map(BigInt::from),
            start: date(&q["validTimeStart"]).or_else(|| date(&q["startTime"])),
            end: date(&q["validTimeEnd"]).or_else(|| date(&q["endTime"])),
        })
        .collect()
}
#[test]
fn input_integrity_and_size_bound() {
    use sha2::{Digest, Sha256};
    let manifest = manifest();
    let mut bytes = 0;
    for (name, hash) in manifest["sha256"].as_object().unwrap() {
        let raw = std::fs::read(fixtures().join(name)).unwrap();
        bytes += raw.len();
        assert_eq!(
            format!("{:x}", Sha256::digest(raw)),
            hash.as_str().unwrap(),
            "{name}"
        );
    }
    assert!(bytes < 2_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_repository_inputs_open_at_the_declared_snapshot() {
    for name in manifest()["fixture_names"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        let spec = input(name);
        let pinned = OpenedDataset::open(
            &descriptor(name),
            DatasetVersion::Snapshot(spec["snapshot_id"].as_str().unwrap()),
            &[],
        )
        .await
        .unwrap();
        let main = OpenedDataset::open(&descriptor(name), DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        assert_eq!(main.snapshot_id(), pinned.snapshot_id(), "{name}");
        assert!(pinned.metadata_bytes() < MAX_METADATA_BYTES);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe() {
    let dataset = OpenedDataset::open(&descriptor("forecast"), DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let view = dataset.view();
    for (name, size) in [
        ("init_time", 2),
        ("lead_time", 5),
        ("latitude", 4),
        ("longitude", 6),
    ] {
        assert_eq!(view.dimensions[name], size);
        assert!(view.variables[name].is_coordinate);
    }
    assert!(
        matches!(&view.coordinates["latitude"], Coordinate::Numeric(v) if v == &[44.,42.,40.,38.])
    );
    assert!(
        matches!(&view.coordinates["longitude"], Coordinate::Numeric(v) if v == &[0.,60.,120.,180.,240.,300.])
    );
    // 2026-07-30 is 20,664 days after the Unix epoch; runs are six hours apart.
    let epoch = 20_664_i64 * 86_400 * 1_000_000_000;
    assert!(
        matches!(&view.coordinates["init_time"], Coordinate::Time(v) if v == &[epoch, epoch + 6 * 3_600_000_000_000])
    );
    assert!(
        matches!(&view.coordinates["lead_time"], Coordinate::Duration(v) if v == &(0..5).map(|i| i*3_600_000_000_000).collect::<Vec<_>>())
    );
    assert_eq!(
        view.variables["temperature_2m"].attributes["units"],
        Value::from("degree_Celsius")
    );
    assert_eq!(
        view.variables["temperature_2m"].chunks,
        Some(vec![Some(1), Some(2), Some(2), Some(3)])
    );
    assert!(!view.variables["temperature_2m"].is_coordinate);
    assert!(!view.variables["lead_time"].attributes.contains_key("units"));
    assert_eq!(view.attributes["_FillValue"], Value::from(42_i64));
    run_with_permit(
        Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
        move |handle| {
            let positions = [
                ("init_time".into(), Selection::Index(1)),
                ("lead_time".into(), Selection::Indices(vec![4, 1, 4, 0])),
                ("latitude".into(), Selection::Index(1)),
                ("longitude".into(), Selection::Indices(vec![5, 0, 5])),
                ("ignored".into(), Selection::Index(42)),
            ]
            .into();
            let mut reader = dataset.reader(handle);
            let counts = reader.read(dataset.view(), "count", &positions)?;
            let mut expected_counts = Vec::new();
            let mut expected_floats = Vec::new();
            for lead in [4, 1, 4, 0] {
                for x in [5, 0, 5] {
                    let flat = ((5 + lead) * 4 + 1) * 6 + x;
                    expected_counts.push(Value::from(9_007_199_254_740_993_i64 + flat));
                    expected_floats.push(f64::from(flat as f32 / 10.0 + 10.0));
                }
            }
            assert_eq!(counts, expected_counts);
            let floats = reader.read(dataset.view(), "temperature_2m", &positions)?;
            assert_eq!(floats.len(), expected_floats.len());
            for (actual, expected) in floats.iter().zip(expected_floats) {
                assert!(matches!(actual, Value::Float(v) if v.to_bits()==expected.to_bits()));
            }
            // Static data omit run/lead dimensions and retain only the selected x order.
            assert_eq!(
                reader.read(dataset.view(), "static", &positions)?,
                vec![Value::Float(11.), Value::Float(6.), Value::Float(11.)]
            );
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn coordinate_membership_and_crs_reach_query_validation() {
    for (name, crs, promoted) in [
        ("geographic_xy", Some(SpatialCrs::Geographic), false),
        ("fallback_crs", Some(SpatialCrs::Geographic), false),
        ("missing_crs", Some(SpatialCrs::Missing), false),
        ("unreferenced_crs", None, false),
        ("coord_non_subset", Some(SpatialCrs::Geographic), false),
        ("global_aux_data", Some(SpatialCrs::Geographic), true),
        ("coord_self_ref", Some(SpatialCrs::Geographic), true),
        ("coord_attr_null", Some(SpatialCrs::Geographic), false),
        ("coord_attr_list", Some(SpatialCrs::Geographic), false),
    ] {
        let dataset = OpenedDataset::open(&descriptor(name), DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        let view = dataset.view();
        assert_eq!(view.spatial_crs, crs, "{name}");
        assert_eq!(
            view.variables["temperature_2m"].is_coordinate, promoted,
            "{name}"
        );
        assert_eq!(
            view.variables["spatial_ref"].is_coordinate,
            name != "unreferenced_crs",
            "{name}"
        );
        if name == "coord_non_subset" {
            assert_eq!(view.variables["spatial_ref"].dimensions, ["crs_axis"]);
        }
        if name == "coord_attr_null" {
            assert_eq!(view.attributes["coordinates"], Value::Null);
        }
        if name == "coord_attr_list" {
            assert_eq!(
                view.attributes["coordinates"],
                Value::Array(vec![Value::from("spatial_ref")])
            );
        }
        let spec = input(name);
        let mut c = collection(&spec);
        // Advertise the variable even after promotion: snapshot membership must reject it.
        c.variables.insert(
            "temperature_2m".into(),
            CatalogVariable {
                dimensions: view.variables["temperature_2m"].dimensions.clone(),
                unit: Some("K".into()),
            },
        );
        let mut requests = inputs(&spec["cases"][0], &dataset, &c);
        requests[0].variables = vec!["temperature_2m".into()];
        let result = prepare_batch(c.kind, &requests, &mut QueryBudget::default(), true, 64);
        if promoted {
            let error = result.unwrap_err();
            assert_eq!(error.status, 422);
            assert_eq!(
                error.detail,
                format!(
                    "Variable(s) listed in the catalog are absent from snapshot {}: temperature_2m",
                    dataset.snapshot_id()
                )
            );
        } else if matches!(name, "missing_crs" | "unreferenced_crs") {
            let error = result.unwrap_err();
            assert_eq!(error.status, 422);
            assert_eq!(
                error.detail,
                if name == "missing_crs" {
                    "Projected grid is missing its CRS"
                } else {
                    "Dataset has an unsupported spatial grid"
                }
            );
        } else {
            assert!(result.is_ok(), "{name}: {result:?}");
        }
    }
    let spec = input("projected");
    let dataset = OpenedDataset::open(
        &descriptor("projected"),
        DatasetVersion::Branch("main"),
        &[],
    )
    .await
    .unwrap();
    assert_eq!(dataset.view().spatial_crs, Some(SpatialCrs::Projected));
    let c = collection(&spec);
    assert_eq!(spec["cases"].as_array().unwrap().len(), 5);
    for case in spec["cases"].as_array().unwrap() {
        let requests = inputs(case, &dataset, &c);
        let error =
            prepare_batch(c.kind, &requests, &mut QueryBudget::default(), true, 64).unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.detail, "Projected-CRS grids are not yet supported");
    }
}

#[tokio::test]
async fn deployed_coordinates_keep_auxiliary_values_lazy_and_nested_arrays_outside_the_view() {
    let dataset = OpenedDataset::open(
        &descriptor("deployed_coordinates"),
        DatasetVersion::Branch("main"),
        &[],
    )
    .await
    .unwrap();
    let view = dataset.view();
    assert!(
        matches!(&view.coordinates["latitude"], Coordinate::Numeric(v) if v.len()==721 && v.iter().enumerate().all(|(i,x)| *x == 90. - i as f64/4.))
    );
    assert!(
        matches!(&view.coordinates["longitude"], Coordinate::Numeric(v) if v.len()==1440 && v.iter().enumerate().all(|(i,x)| *x == i as f64/4. - 180.))
    );
    assert_eq!(view.dimensions["init_time"], 7);
    assert_eq!(view.dimensions["lead_time"], 7);
    assert!(view.variables.contains_key("valid_time"));
    assert!(!view.coordinates.contains_key("valid_time"));
    assert!(view.variables.keys().all(|name| !name.contains('/')));
    assert_eq!(
        view.variables["valid_time"].chunks,
        Some(vec![Some(27_740), Some(209)])
    );
}

#[tokio::test]
async fn metadata_cap_includes_coordinates_and_attributes() {
    let descriptor = descriptor("forecast");
    let opened = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let measured = opened.metadata_bytes();
    assert!(measured < MAX_METADATA_BYTES);
    for cap in [0, 1, 100, measured - 1] {
        let error =
            OpenedDataset::open_bounded(&descriptor, DatasetVersion::Branch("main"), &[], cap)
                .await
                .err()
                .unwrap();
        assert!(
            matches!(error,OpenError::MetadataLimit{limit} if limit==cap),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("Opened dataset metadata exceeds {cap} bytes")
        );
    }
    let exact =
        OpenedDataset::open_bounded(&descriptor, DatasetVersion::Branch("main"), &[], measured)
            .await
            .unwrap();
    assert_eq!(exact.metadata_bytes(), measured);
}

#[tokio::test]
async fn unknown_snapshots_have_typed_errors_and_missing_branches_remain_reader_errors() {
    let descriptor = descriptor("forecast");
    for (id, message) in [
        ("not-a-snapshot", "Unknown snapshot 'not-a-snapshot'"),
        (
            "00000000000000000000",
            "Unknown snapshot '00000000000000000000'",
        ),
        ("main", "Unknown snapshot 'main'"),
        ("\"'☃", r#"Unknown snapshot '"\'☃'"#),
    ] {
        let error = OpenedDataset::open(&descriptor, DatasetVersion::Snapshot(id), &[])
            .await
            .err()
            .unwrap();
        assert!(matches!(error, OpenError::UnknownSnapshot(_)), "{error}");
        assert_eq!(error.to_string(), message);
    }
    let error = OpenedDataset::open(&descriptor, DatasetVersion::Branch("missing"), &[])
        .await
        .err()
        .unwrap();
    assert!(matches!(error, OpenError::Reader(_)));
    assert!(
        matches!(error, OpenError::Reader(ref e) if e.kind == dynapi_reader::ErrorKind::Snapshot)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinate_dtypes_fractional_durations_and_float_bits_follow_the_recipe() {
    for name in ["float32", "int64", "fractional"] {
        let dataset = OpenedDataset::open(&descriptor(name), DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        let view = dataset.view();
        match name {
            "float32" => assert!(
                matches!(&view.coordinates["latitude"], Coordinate::Float32(v) if v==&[44.,42.,40.,38.])
            ),
            "int64" => assert!(
                matches!(&view.coordinates["latitude"], Coordinate::Int64(v) if v==&[44,42,40,38])
            ),
            // The binary64 representation of 1.000001 seconds is below the
            // decimal value; successive s/ms/us/ns scaling and truncation yield
            // 1,000,000,999 ns. The quarter-second input is exactly representable.
            "fractional" => assert!(
                matches!(&view.coordinates["lead_time"], Coordinate::Duration(v) if v==&[0,250_000_000,1_000_000_999,3_600_000_000_000,i64::MIN])
            ),
            _ => unreachable!(),
        }
        run_with_permit(Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(), move |handle| {
            let positions = [("init_time".into(), Selection::Index(0)),("latitude".into(), Selection::Index(0)),("longitude".into(), Selection::Index(0))].into();
            let values = dataset.reader(handle).read(dataset.view(), "temperature_2m", &positions)?;
            assert_eq!(values.len(), 5);
            assert!(matches!(values[0],Value::Float(x) if x.is_nan()));
            for (value, expected) in values[1..].iter().zip([-0.0_f32,1e-5, f32::from_bits(1),3e38]) {
                assert!(matches!(value, Value::Float(x) if x.to_bits()==f64::from(expected).to_bits()));
            }
            Ok(())
        }).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_point_forecast_has_explicit_response_bytes() {
    let dataset = OpenedDataset::open(&descriptor("forecast"), DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let expected = r#"{"results":[{"queryId":"sample","dataProduct":{"id":"fixture-forecast","version":"1.2.3"},"point":{"requested":{"latitude":44.0,"longitude":0.0},"selected":{"latitude":44.0,"longitude":0.0,"grid":{"latitudeIndex":0,"longitudeIndex":0}},"method":"nearest"},"forecasts":[{"initTime":"2026-07-30T00:00:00Z","validTimes":["2026-07-30T00:00:00Z"],"leadTimeHours":[0.0],"links":{"canonical":"/v1/data/fixture-forecast/SNAPSHOT/points/0,0?variables=count&initTime=2026-07-30T00%3A00%3A00Z&maxLeadTimeHours=0"},"data":{"count":[9007199254740993]}}],"variables":{"count":{"unit":null}},"snapshotId":"SNAPSHOT","links":{"catalog":"https://example.invalid/forecast/collection.json"}}]}"#.replace("SNAPSHOT",dataset.snapshot_id());
    run_with_permit(
        Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
        move |handle| {
            let c = collection(&input("forecast"));
            let request = QueryInput {
                dataset: dataset.view(),
                collection: &c,
                snapshot_id: dataset.snapshot_id(),
                query_id: Some("sample".into()),
                latitude: 44.,
                longitude: 0.,
                variables: vec!["count".into()],
                init_times: InitTimeSelection {
                    exact: Some(RequestDatetime::parse_str("2026-07-30T00:00:00Z").unwrap()),
                    ..Default::default()
                },
                max_lead_time_hours: Some(BigInt::from(0)),
                start: None,
                end: None,
            };
            let batch = prepare_batch(
                Kind::Forecast,
                &[request],
                &mut QueryBudget::default(),
                true,
                64,
            )?;
            let response =
                batch.execute(&mut dataset.reader(handle), &ResponseMetadata::default())?;
            assert_eq!(response.body, expected.as_bytes());
            Ok(())
        },
    )
    .await
    .unwrap();
}
