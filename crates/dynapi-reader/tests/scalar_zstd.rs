//! Scalar input bytes follow RFC 8878; expected values are explicit int64 literals.
#[path = "support/encoding.rs"]
mod encoding;
use dynapi_reader::ErrorKind;
use encoding::{Fixture, metadata, raw_zstd};
use serde_json::{Value, json};

fn scalar(fill: i64) -> Value {
    metadata(
        "int64",
        &[],
        &[],
        &[],
        json!(fill),
        json!([
            {"name":"bytes","configuration":{"endian":"little"}},
            {"name":"zstd","configuration":{"level":0,"checksum":false}}
        ]),
        json!({}),
    )
}
fn non_single(value: i64, window: u8) -> Vec<u8> {
    // Four-byte FCS, non-single-segment descriptor, explicit window descriptor.
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x80, window, 8, 0, 0, 0, 0x41, 0, 0];
    frame.extend_from_slice(&value.to_le_bytes());
    frame
}

#[tokio::test]
async fn scalar_int64_raw_compressed_and_missing_fill_values_are_exact() {
    let fixture = Fixture::new().await;
    let values = [i64::MIN, -1, 0, 9_007_199_254_740_993, i64::MAX];
    for (index, value) in values.into_iter().enumerate() {
        for (label, frame) in [
            ("raw", raw_zstd(&value.to_le_bytes())),
            (
                "compressed",
                zstd::bulk::compress(&value.to_le_bytes(), 0).unwrap(),
            ),
            ("small_window", non_single(value, 0)),
        ] {
            fixture
                .array(
                    &format!("case_{index}_{label}/value"),
                    &scalar(17),
                    Some(frame),
                )
                .await;
        }
        fixture
            .array(&format!("fill_{index}/value"), &scalar(value), None)
            .await;
    }
    let reader = fixture.reader().await;
    for (index, value) in values.into_iter().enumerate() {
        for label in ["raw", "compressed", "small_window", "fill"] {
            let path = if label == "fill" {
                format!("fill_{index}/value")
            } else {
                format!("case_{index}_{label}/value")
            };
            let result = reader
                .read_dataset_selection(&path, &[], None)
                .await
                .unwrap();
            assert_eq!(result.dtype, "int64");
            assert_eq!(result.shape, Vec::<usize>::new());
            assert_eq!(result.bytes, value.to_le_bytes(), "{path}");
        }
    }
}

#[tokio::test]
async fn scalar_zstd_frame_matrix_rejects_unsupported_headers_sizes_and_corruption() {
    let fixture = Fixture::new().await;
    let valid = raw_zstd(&123i64.to_le_bytes());
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("short", vec![0; 5]),
        ("too_long", vec![0; 65]),
        ("wrong_output_size", raw_zstd(&[0; 7])),
        ("concatenated", [valid.clone(), valid.clone()].concat()),
        ("trailing", [valid.clone(), vec![0]].concat()),
        ("truncated", valid[..valid.len() - 1].to_vec()),
    ];
    for (name, index, value) in [
        ("magic", 0, 0),
        ("dictionary", 4, 0x21),
        ("checksum", 4, 0x24),
        ("reserved", 4, 0x28),
        ("unused", 4, 0x30),
        ("unknown_content_size", 4, 0),
        ("reserved_block", 6, 0x47),
    ] {
        let mut frame = valid.clone();
        frame[index] = value;
        cases.push((name, frame));
    }
    // Content size says eight, but the final raw block decodes seven bytes.
    let mut wrong_decoded = raw_zstd(&[0; 7]);
    wrong_decoded[5] = 8;
    cases.push(("decoded_length", wrong_decoded));
    for (name, frame) in &cases {
        fixture
            .array(&format!("{name}/value"), &scalar(17), Some(frame.clone()))
            .await;
    }
    let reader = fixture.reader().await;
    for (name, _) in cases {
        let error = reader
            .read_dataset_selection(&format!("{name}/value"), &[], None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode, "{name}: {error}");
    }
}

#[tokio::test]
async fn scalar_zstd_exact_encoded_limit_accepts_64_and_rejects_65_valid_frame_bytes() {
    let fixture = Fixture::new().await;
    // Empty non-final raw blocks add three bytes each without decoded output.
    let mut at_cap = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 8];
    at_cap.resize(6 + 18 * 3, 0);
    at_cap.extend_from_slice(&[0x43, 0, 0, 0]);
    let mut over_cap = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 8];
    over_cap.resize(6 + 16 * 3, 0);
    over_cap.extend_from_slice(&[0x41, 0, 0]);
    over_cap.extend_from_slice(&[0; 8]);
    assert_eq!(at_cap.len(), 64);
    assert_eq!(over_cap.len(), 65);
    // Both are valid Zstd inputs; the latter is outside this reader's input cap.
    assert_eq!(zstd::bulk::decompress(&at_cap, 8).unwrap(), [0; 8]);
    assert_eq!(zstd::bulk::decompress(&over_cap, 8).unwrap(), [0; 8]);
    fixture.array("at/value", &scalar(9), Some(at_cap)).await;
    fixture
        .array("over/value", &scalar(9), Some(over_cap))
        .await;
    let reader = fixture.reader().await;
    assert_eq!(
        reader
            .read_dataset_selection("at/value", &[], None)
            .await
            .unwrap()
            .bytes,
        [0; 8]
    );
    assert_eq!(
        reader
            .read_dataset_selection("over/value", &[], None)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Decode
    );
}

#[tokio::test]
async fn scalar_zstd_window_descriptor_cannot_bypass_the_one_kib_decode_bound() {
    let fixture = Fixture::new().await;
    for (name, window) in [("one_kib", 0), ("above_one_kib", 1), ("large", 0xa0)] {
        fixture
            .array(
                &format!("{name}/value"),
                &scalar(0),
                Some(non_single(42, window)),
            )
            .await;
    }
    let reader = fixture.reader().await;
    assert_eq!(
        reader
            .read_dataset_selection("one_kib/value", &[], None)
            .await
            .unwrap()
            .bytes,
        42i64.to_le_bytes()
    );
    for name in ["above_one_kib", "large"] {
        let error = reader
            .read_dataset_selection(&format!("{name}/value"), &[], None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode, "{name}: {error}");
    }
}
