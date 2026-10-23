//! Stored CF input values are committed; ns expectations use explicit integer/IEEE arithmetic.
#[path = "support/encoding.rs"]
mod encoding;
use dynapi_reader::{
    ErrorKind, Selector, StoreDescriptor,
    cf_time::{REVISION, Request},
    read_cf_selection_traced,
};
use encoding::{Fixture, bytes_codec, input, metadata};
use serde_json::{Value, json};

fn attrs() -> Value {
    json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="})
}
async fn fixture() -> Fixture {
    let fixture = Fixture::new().await;
    let inputs: Value = serde_json::from_slice(&input("duration-inputs.json")).unwrap();
    for (name, values) in inputs.as_object().unwrap() {
        let values = values.as_array().unwrap();
        for dimension in [true, false] {
            let (path, dim) = if dimension {
                (format!("dim_{name}/lead_time"), "lead_time")
            } else {
                (format!("aux_{name}/duration"), "time")
            };
            fixture
                .array(
                    &path,
                    &metadata(
                        "float64",
                        &[values.len()],
                        &[1],
                        &[dim],
                        json!("NaN"),
                        json!([bytes_codec()]),
                        attrs(),
                    ),
                    None,
                )
                .await;
            for (index, value) in values.iter().enumerate() {
                if let Some(seconds) = value.as_f64() {
                    fixture
                        .store
                        .set(
                            &format!("{path}/c/{index}"),
                            seconds.to_le_bytes().to_vec().into(),
                        )
                        .await
                        .unwrap();
                }
            }
            // The original traced CF API takes caller-owned CF attributes;
            // generic stored names avoid its separately pinned coordinate profiles.
            let trace_path = format!(
                "trace_{}_{name}/value",
                if dimension { "dim" } else { "aux" }
            );
            fixture
                .array(
                    &trace_path,
                    &metadata(
                        "float64",
                        &[values.len()],
                        &[1],
                        &[if dimension { "value" } else { "time" }],
                        json!("NaN"),
                        json!([bytes_codec()]),
                        json!({}),
                    ),
                    None,
                )
                .await;
            for (index, value) in values.iter().enumerate() {
                if let Some(seconds) = value.as_f64() {
                    fixture
                        .store
                        .set(
                            &format!("{trace_path}/c/{index}"),
                            seconds.to_le_bytes().to_vec().into(),
                        )
                        .await
                        .unwrap();
                }
            }
        }
    }
    fixture
}

#[tokio::test]
async fn full_duration_coordinates_keep_unselected_chunk_precision_for_scalar_list_and_empty_reads()
{
    let fixture = fixture().await;
    let reader = fixture.reader().await;
    let descriptor = StoreDescriptor::Local(fixture.path.to_str().unwrap().into());
    let request = Request {
        revision: REVISION.into(),
        attributes: attrs().as_object().unwrap().clone(),
    };
    // IEEE binary64 at this magnitude has a 1024ns ULP. Repeated s/ms/us/ns
    // promotion rounds the exact midpoint downward to an even significand (-512).
    const MIXED: i64 = 5_000_000_000_999_999_488;
    for (selection, expected, shape) in [
        (Selector::Index(0), vec![MIXED], vec![]),
        (Selector::Index(2), vec![i64::MIN], vec![]),
        (
            Selector::Indices(vec![0, 0, 2, 1]),
            vec![MIXED, MIXED, i64::MIN, 1],
            vec![4],
        ),
        (Selector::Indices(vec![2]), vec![i64::MIN], vec![1]),
        (Selector::Indices(vec![]), vec![], vec![0]),
    ] {
        let (buffer, trace) = read_cf_selection_traced(
            &descriptor,
            reader.snapshot_id(),
            "trace_dim_mixed/value",
            std::slice::from_ref(&selection),
            &request,
        )
        .await
        .unwrap();
        assert_eq!(buffer.dtype, "timedelta64[ns]");
        assert_eq!(buffer.shape, shape);
        assert_eq!(
            buffer.bytes,
            expected
                .iter()
                .flat_map(|n| n.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(trace.decoded_bytes, 24);
        assert_eq!(trace.output_bytes, expected.len() * 8);
        assert_eq!(trace.decoded_inner_chunks.len(), 3);
        let native = reader
            .read_dataset_selection("dim_mixed/lead_time", &[selection], Some(&request))
            .await
            .unwrap();
        assert_eq!(native.bytes, buffer.bytes);
        assert_eq!(native.shape, buffer.shape);
    }
}

#[tokio::test]
async fn auxiliary_durations_decode_only_the_selected_values_and_preserve_duplicate_order() {
    let fixture = fixture().await;
    let reader = fixture.reader().await;
    let descriptor = StoreDescriptor::Local(fixture.path.to_str().unwrap().into());
    let request = Request {
        revision: REVISION.into(),
        attributes: attrs().as_object().unwrap().clone(),
    };
    for (selection, expected, shape, decoded) in [
        (
            Selector::Index(0),
            vec![5_000_000_001_000_000_000i64],
            vec![],
            8,
        ),
        (Selector::Index(2), vec![i64::MIN], vec![], 8),
        (
            Selector::Indices(vec![0, 2, 0]),
            vec![
                5_000_000_001_000_000_000,
                i64::MIN,
                5_000_000_001_000_000_000,
            ],
            vec![3],
            16,
        ),
        (
            Selector::Indices(vec![0, 1, 0]),
            vec![5_000_000_000_999_999_488, 1, 5_000_000_000_999_999_488],
            vec![3],
            16,
        ),
        (Selector::Indices(vec![]), vec![], vec![0], 0),
    ] {
        let (buffer, trace) = read_cf_selection_traced(
            &descriptor,
            reader.snapshot_id(),
            "trace_aux_mixed/value",
            std::slice::from_ref(&selection),
            &request,
        )
        .await
        .unwrap();
        assert_eq!(buffer.dtype, "timedelta64[ns]");
        assert_eq!(buffer.shape, shape);
        assert_eq!(
            buffer.bytes,
            expected
                .iter()
                .flat_map(|n| n.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(trace.decoded_bytes, decoded);
        assert_eq!(trace.output_bytes, expected.len() * 8);
        let native = reader
            .read_dataset_selection("aux_mixed/duration", &[selection], Some(&request))
            .await
            .unwrap();
        assert_eq!(native.bytes, buffer.bytes);
        assert_eq!(native.shape, buffer.shape);
    }
}

#[tokio::test]
async fn fractional_nat_and_overflow_tables_respect_dimension_versus_auxiliary_decoding_units() {
    let fixture = fixture().await;
    let reader = fixture.reader().await;
    let request = Request {
        revision: REVISION.into(),
        attributes: attrs().as_object().unwrap().clone(),
    };
    for prefix in ["dim_fractional/lead_time", "aux_fractional/duration"] {
        let buffer = reader
            .read_dataset_selection(
                prefix,
                &[Selector::Indices(vec![6, 0, 1, 2, 3, 4, 5, 3])],
                Some(&request),
            )
            .await
            .unwrap();
        assert_eq!(buffer.dtype, "timedelta64[ns]");
        assert_eq!(buffer.shape, [8]);
        let expected = [
            i64::MIN,
            250_000_000,
            -250_000_000,
            100_000_000,
            1_000_000_999,
            1,
            -1,
            1_000_000_999,
        ];
        assert_eq!(
            buffer.bytes,
            expected
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }
    for selection in [Selector::Index(1), Selector::Indices(vec![])] {
        let error = reader
            .read_dataset_selection(
                "dim_overflow/lead_time",
                std::slice::from_ref(&selection),
                Some(&request),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode);
        assert_eq!(
            error.message,
            "CF time outside datetime64[ns]/timedelta64[ns] range"
        );
        let buffer = reader
            .read_dataset_selection(
                "aux_overflow/duration",
                std::slice::from_ref(&selection),
                Some(&request),
            )
            .await
            .unwrap();
        assert_eq!(buffer.dtype, "timedelta64[ns]");
        let expected = if matches!(selection, Selector::Index(_)) {
            1i64.to_le_bytes().to_vec()
        } else {
            vec![]
        };
        assert_eq!(buffer.bytes, expected);
    }
    for path in ["dim_all_nat/lead_time", "aux_all_nat/duration"] {
        let buffer = reader
            .read_dataset_selection(path, &[Selector::Indices(vec![2, 0, 2])], Some(&request))
            .await
            .unwrap();
        assert_eq!(
            buffer.bytes,
            [i64::MIN; 3]
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }
}
