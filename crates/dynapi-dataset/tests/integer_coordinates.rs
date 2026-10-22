use dynapi_contract::Value;
use dynapi_dataset::*;
use dynapi_query::{Coordinate, DataReader, Selection};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use tokio::sync::Semaphore;
mod common;

#[tokio::test]
async fn integer_member_widths_are_exact_including_extrema_and_duplicates() {
    let mut cases: Vec<(&str, Vec<u8>, Vec<i64>)> = Vec::new();
    macro_rules! case {
        ($dtype:literal, $ty:ty, $values:expr) => {{
            let values: Vec<$ty> = $values;
            cases.push((
                $dtype,
                values.iter().flat_map(|v| v.to_le_bytes()).collect(),
                values.iter().map(|v| i64::from(*v)).collect(),
            ));
        }};
    }
    case!("int8", i8, vec![i8::MIN, 0, i8::MAX]);
    case!("int16", i16, vec![i16::MIN, 0, i16::MAX]);
    case!("int32", i32, vec![i32::MIN, 0, i32::MAX]);
    case!(
        "int64",
        i64,
        vec![i64::MIN, 9_007_199_254_740_993, i64::MAX]
    );
    case!("uint8", u8, vec![0, 7, u8::MAX]);
    case!("uint16", u16, vec![0, 7, u16::MAX]);
    case!("uint32", u32, vec![0, 7, u32::MAX]);
    for (dtype, bytes, expected) in cases {
        let temp = tempfile::tempdir().unwrap();
        let meta = if matches!(dtype, "int8" | "uint8") {
            let metadata: Json =
                serde_json::from_str(include_str!("fixtures/one-byte-integers.json")).unwrap();
            metadata[dtype].clone()
        } else {
            common::axis("ensemble_member", dtype, 3, 3, json!({}))
        };
        common::repository(temp.path(), &[("ensemble_member", meta, Some(bytes))]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        let expected: Vec<_> = expected.into_iter().map(Value::from).collect();
        assert!(
            matches!(&dataset.view().coordinates["ensemble_member"], Coordinate::Members(v) if v==&expected),
            "{dtype}"
        );
        run_with_permit(
            Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
            move |handle| {
                let values = dataset.reader(handle).read(
                    dataset.view(),
                    "ensemble_member",
                    &[("ensemble_member".into(), Selection::Indices(vec![2, 0, 2]))].into(),
                )?;
                assert_eq!(
                    values,
                    vec![
                        expected[2].clone(),
                        expected[0].clone(),
                        expected[2].clone()
                    ]
                );
                Ok(())
            },
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn one_byte_endian_forms_keep_multibyte_and_original_api_admission_strict() {
    let metadata: Json =
        serde_json::from_str(include_str!("fixtures/one-byte-integers.json")).unwrap();
    for dtype in [
        "int8", "uint8", "int16", "uint16", "int32", "uint32", "int64", "float32", "float64",
    ] {
        for (codec, little, endian_free) in [
            (json!({"name":"bytes"}), false, true),
            (json!({"name":"bytes","configuration":{}}), false, true),
            (
                json!({"name":"bytes","configuration":{"endian":null}}),
                false,
                true,
            ),
            (
                json!({"name":"bytes","configuration":{"endian":"little"}}),
                true,
                false,
            ),
            (
                json!({"name":"bytes","configuration":{"endian":"big"}}),
                false,
                false,
            ),
            (
                json!({"name":"bytes","configuration":{"unexpected":null}}),
                false,
                false,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let one_byte = matches!(dtype, "int8" | "uint8");
            let mut meta = if one_byte {
                metadata[dtype].clone()
            } else {
                common::axis("ensemble_member", dtype, 3, 3, json!({}))
            };
            meta["dimension_names"] = json!(["data"]);
            meta["codecs"] = json!([codec]);
            common::repository(temp.path(), &[("data", meta, None)]).await;
            let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
            let session =
                dynapi_reader::ReadSession::open(&descriptor, DatasetVersion::Branch("main"), &[])
                    .await
                    .unwrap();
            let selectors = [dynapi_reader::Selector::Indices(vec![2, 0, 2])];
            let result = session
                .read_dataset_selection("data", &selectors, None)
                .await;
            assert_eq!(
                result.is_ok(),
                little || (one_byte && endian_free),
                "{dtype}: {codec}"
            );
            if let Err(error) = result {
                assert_eq!(error.kind, dynapi_reader::ErrorKind::Unsupported);
            }
            if one_byte {
                let error = dynapi_reader::read_selection(
                    &descriptor,
                    session.snapshot_id(),
                    "data",
                    &selectors,
                )
                .await
                .unwrap_err();
                assert_eq!(error.kind, dynapi_reader::ErrorKind::Unsupported);
                assert_eq!(error.message, "Only float32/float64/int64");
            }
        }
    }
}

#[tokio::test]
async fn unused_string_dimension_is_lazy_but_selected_string_axis_fails_cleanly() {
    for name in ["statistic", "ensemble_member"] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis(name, "string", 3, 3, json!({}));
        meta["fill_value"] = json!("");
        meta["codecs"] = json!([{"name":"vlen-utf8"}]);
        common::repository(temp.path(), &[(name, meta, None)]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let opened = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[]).await;
        if name == "statistic" {
            let dataset = opened.unwrap();
            assert_eq!(dataset.view().dimensions[name], 3);
            assert!(dataset.view().variables[name].is_coordinate);
            assert!(!dataset.view().coordinates.contains_key(name));
        } else {
            assert!(matches!(opened, Err(OpenError::Metadata(_))));
        }
    }
}

#[tokio::test]
async fn metadata_inventory_axis_types_exercise_admission_and_laziness() {
    let products: Json =
        serde_json::from_slice(include_bytes!("fixtures/live-coordinate-dtypes.json")).unwrap();
    let cases: std::collections::BTreeSet<_> = products
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|product| {
            product["coordinates"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, coord)| (name.clone(), coord["dtype"].as_str().unwrap().to_owned()))
        })
        .collect();
    for (name, dtype) in cases {
        let attrs = match name.as_str() {
            "init_time" | "time" => {
                json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"})
            }
            "lead_time" => {
                json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="})
            }
            _ => json!({}),
        };
        let mut meta = common::axis(&name, &dtype, 3, 3, attrs);
        if dtype == "string" {
            meta["fill_value"] = json!("");
            meta["codecs"] = json!([{"name":"vlen-utf8"}]);
        }
        let temp = tempfile::tempdir().unwrap();
        common::repository(temp.path(), &[(&name, meta, None)]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        assert!(dataset.view().variables[&name].is_coordinate);
        let coordinate = dataset.view().coordinates.get(&name);
        match (name.as_str(), dtype.as_str()) {
            ("statistic", "string") => assert!(coordinate.is_none(), "unused strings stay lazy"),
            ("init_time" | "time", "int64") => {
                assert!(matches!(coordinate, Some(Coordinate::Time(v)) if v==&[0,0,0]))
            }
            ("lead_time", "float64") => {
                assert!(matches!(coordinate, Some(Coordinate::Duration(v)) if v==&[i64::MIN;3]))
            }
            ("ensemble_member", "int16") => assert!(
                matches!(coordinate, Some(Coordinate::Members(v)) if v==&vec![Value::from(0_i64);3])
            ),
            ("latitude" | "longitude" | "y" | "x", "float64") => assert!(
                matches!(coordinate, Some(Coordinate::Numeric(v)) if v.len()==3 && v.iter().all(|x| x.is_nan()))
            ),
            other => panic!("add an explicit axis policy for {other:?}"),
        }
    }
}

#[tokio::test]
async fn virtual_indexes_are_bounded_and_never_replace_stored_invalid_coordinates() {
    for size in [3, 1_048_576] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis("data", "float64", size, 4096, json!({}));
        meta["dimension_names"] = json!(["y"]);
        common::repository(temp.path(), &[("data", meta, None)]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let opened = OpenedDataset::open_bounded(
            &descriptor,
            DatasetVersion::Branch("main"),
            &[],
            1_000_000,
        )
        .await;
        if size == 3 {
            let dataset = opened.unwrap();
            assert!(
                matches!(&dataset.view().coordinates["y"], Coordinate::Int64(v) if v==&[0,1,2])
            );
            assert!(!dataset.view().variables.contains_key("y"));
        } else {
            assert!(matches!(opened, Err(OpenError::MetadataLimit { .. })));
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let meta = common::axis("y", "float64", 3, 3, json!({}));
    common::repository(temp.path(), &[("y", meta, None)]).await;
    let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
    let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    assert!(
        matches!(&dataset.view().coordinates["y"], Coordinate::Numeric(v) if v.len()==3 && v.iter().all(|x|x.is_nan()))
    );
}
