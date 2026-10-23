//! The input frames and raw numeric bytes are constructed by the committed Rust recipe.
#[path = "support/encoding.rs"]
mod encoding;
use dynapi_reader::{ErrorKind, Selector};
use encoding::*;
use serde_json::{Value, json};

struct Case {
    name: String,
    codecs: Value,
    a: Vec<u8>,
    b: Vec<u8>,
    blosc: bool,
    memcpy: bool,
}
fn codecs() -> Vec<Case> {
    assert_eq!(crc32c(b"123456789"), 0xe3069283);
    let inputs = inputs();
    let matrix = inputs["matrix"].as_array().unwrap();
    let raw_a = input(matrix[0]["raw"].as_str().unwrap());
    let raw_b = input(matrix[1]["raw"].as_str().unwrap());
    let mut cases = vec![Case {
        name: "bytes".into(),
        codecs: json!([bytes_codec()]),
        a: raw_a.clone(),
        b: raw_b.clone(),
        blosc: false,
        memcpy: false,
    }];
    for (shuffle, cname, levels) in [
        ("noshuffle", "zstd", (0..=9).collect::<Vec<u8>>()),
        ("shuffle", "zstd", (0..=9).collect()),
        ("bitshuffle", "zstd", (0..=9).collect()),
        ("shuffle", "lz4", vec![5]),
    ] {
        let key = if cname == "lz4" {
            "lz4".to_owned()
        } else {
            format!("{shuffle}_LEVEL")
        };
        for level in levels {
            let key = key.replace("LEVEL", &level.to_string());
            for is_memcpy in [false, true] {
                let a = if is_memcpy {
                    memcpy(&raw_a, 4)
                } else {
                    input(matrix[0]["frames"][&key].as_str().unwrap())
                };
                let b = if is_memcpy {
                    memcpy(&raw_b, 4)
                } else {
                    input(matrix[1]["frames"][&key].as_str().unwrap())
                };
                let actual_memcpy = a[2] & 2 != 0;
                cases.push(Case {
                    name: format!(
                        "{cname}_{shuffle}_{level}_{}",
                        if is_memcpy { "memcpy" } else { "encoded" }
                    ),
                    codecs: json!([bytes_codec(), blosc_codec(4, shuffle, level, cname)]),
                    a,
                    b,
                    blosc: true,
                    memcpy: actual_memcpy,
                });
            }
        }
    }
    cases
}
fn meta(case: &Case, layout: Option<bool>) -> Value {
    let codecs = if let Some(start) = layout {
        shard_codec(&[256], case.codecs.clone(), start)
    } else {
        case.codecs.clone()
    };
    metadata(
        "float32",
        &[2048],
        &[if layout.is_some() { 1024 } else { 256 }],
        &["sample"],
        json!(-7.25),
        codecs,
        json!({}),
    )
}
async fn put(
    fixture: &Fixture,
    path: &str,
    case: &Case,
    layout: Option<bool>,
    a: Vec<u8>,
    all_missing: bool,
) {
    if let Some(start) = layout {
        let payloads = if all_missing {
            vec![None, None, None, None]
        } else {
            vec![Some(a), None, Some(case.b.clone()), None]
        };
        fixture
            .array(path, &meta(case, layout), Some(shard(&payloads, start)))
            .await;
    } else {
        fixture
            .array(
                path,
                &meta(case, layout),
                if all_missing { None } else { Some(a) },
            )
            .await;
        if !all_missing {
            fixture
                .store
                .set(&format!("{path}/c/2"), case.b.clone().into())
                .await
                .unwrap();
        }
    }
}
fn expected(index: usize, missing: bool) -> f32 {
    if missing {
        -7.25
    } else if index < 256 {
        0.25 + (index % 4) as f32
    } else if (512..768).contains(&index) {
        1000.5 + (index % 4) as f32
    } else {
        -7.25
    }
}

#[tokio::test]
async fn every_codec_level_shuffle_frame_and_index_location_preserves_full_sparse_and_missing_reads()
 {
    let fixture = Fixture::new().await;
    let cases = codecs();
    let mut records = Vec::new();
    for case in &cases {
        for (layout_name, layout) in [("plain", None), ("start", Some(true)), ("end", Some(false))]
        {
            for missing in [false, true] {
                let path = format!(
                    "{}_{}_{}/value",
                    case.name,
                    layout_name,
                    if missing { "missing" } else { "stored" }
                );
                put(&fixture, &path, case, layout, case.a.clone(), missing).await;
                records.push((path, missing));
            }
        }
    }
    assert_eq!(records.len(), 63 * 3 * 2);
    let reader = fixture.reader().await;
    for (path, missing) in records {
        for indices in [vec![513, 0, 257, 513, 1039], (0..2048).collect(), vec![]] {
            let result = reader
                .read_dataset_selection(&path, &[Selector::Indices(indices.clone())], None)
                .await
                .unwrap_or_else(|error| panic!("{path}: {error}"));
            assert_eq!(result.dtype, "float32");
            assert_eq!(result.shape, [indices.len()]);
            let bytes: Vec<u8> = indices
                .iter()
                .flat_map(|i| expected(*i as usize, missing).to_le_bytes())
                .collect();
            assert_eq!(result.bytes, bytes, "{path} indices={}", indices.len());
        }
    }
}

fn mutations(case: &Case) -> Vec<(&'static str, Vec<u8>, bool)> {
    if !case.blosc {
        let mut changed = case.a.clone();
        changed[..4].copy_from_slice(&42f32.to_le_bytes());
        return vec![
            ("short_plain", case.a[..case.a.len() - 1].to_vec(), false),
            ("long_plain", [case.a.clone(), vec![0]].concat(), false),
            ("payload_change", changed, true),
        ];
    }
    let mut out = vec![("header_short", case.a[..15].to_vec(), false)];
    for (label, offset, value) in [
        ("version", 0, 1),
        ("reserved", 2, case.a[2] | 8),
        ("typesize", 3, 2),
        ("zero_block", 8, 0),
        ("nbytes", 4, case.a[4] ^ 1),
        ("cbytes", 12, case.a[12] ^ 1),
    ] {
        let mut frame = case.a.clone();
        frame[offset] = value;
        if label == "zero_block" {
            frame[8..12].fill(0);
        }
        out.push((label, frame, false));
    }
    // Compressor, compression-version and shuffle flags are irrelevant to memcpy.
    for (label, offset, value) in [
        ("compressor", 2, case.a[2] ^ 0x20),
        ("format_version", 1, 99),
        ("shuffle", 2, case.a[2] ^ 1),
    ] {
        let mut frame = case.a.clone();
        frame[offset] = value;
        out.push((label, frame, case.memcpy));
    }
    out.push(("truncated", case.a[..case.a.len() - 1].to_vec(), false));
    out.push(("trailing", [case.a.clone(), vec![0]].concat(), false));
    let mut damaged = case.a.clone();
    if case.memcpy {
        damaged[16..20].copy_from_slice(&42f32.to_le_bytes());
        out.push(("payload_change", damaged, true));
    } else {
        // Keep the valid offset table; make the first compressed stream length
        // exceed the frame, so corruption reaches the codec after header checks.
        let block = u32::from_le_bytes(damaged[16..20].try_into().unwrap()) as usize;
        damaged[block..block + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        out.push(("payload_corrupt", damaged, false));
    }
    out
}
#[tokio::test]
async fn every_codec_level_shuffle_and_layout_enforces_frame_header_policy_including_memcpy_bypasses()
 {
    let fixture = Fixture::new().await;
    let mut records = Vec::new();
    for case in codecs() {
        for (layout_name, layout) in [("plain", None), ("start", Some(true)), ("end", Some(false))]
        {
            for (mutation, frame, accepted) in mutations(&case) {
                let path = format!("{}_{}_{}/value", case.name, layout_name, mutation);
                put(&fixture, &path, &case, layout, frame, false).await;
                records.push((path, accepted, mutation == "payload_change"));
            }
        }
    }
    assert_eq!(records.len(), (62 * 13 + 3) * 3);
    let reader = fixture.reader().await;
    for (path, accepted, changed) in records {
        let result = reader
            .read_dataset_selection(&path, &[Selector::Indices(vec![1, 0, 1])], None)
            .await;
        if accepted {
            assert_eq!(
                result
                    .unwrap_or_else(|error| panic!("{path}: {error}"))
                    .bytes,
                [1.25f32, if changed { 42. } else { 0.25 }, 1.25]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
                "{path}"
            );
        } else {
            assert_eq!(result.unwrap_err().kind, ErrorKind::Decode, "{path}");
        }
    }
}

#[tokio::test]
async fn every_sharded_codec_level_shuffle_and_frame_rejects_corrupt_indexes_even_in_unselected_entries()
 {
    let fixture = Fixture::new().await;
    let mut paths = Vec::new();
    for case in codecs() {
        for start in [true, false] {
            for mutation in [
                "crc",
                "short",
                "overlap",
                "overflow",
                "half_missing",
                "outside",
                "unselected",
            ] {
                let mut payload = shard(
                    &[Some(case.a.clone()), None, Some(case.b.clone()), None],
                    start,
                );
                let at = if start { 0 } else { payload.len() - 68 };
                match mutation {
                    "crc" => payload[at + 67] ^= 1,
                    "short" => payload = vec![0; 67],
                    other => {
                        let entry = if other == "unselected" { 3 } else { 0 };
                        let (offset, length): (u64, u64) = match other {
                            "overlap" => (if start { 0 } else { at as u64 }, 16),
                            "overflow" => (u64::MAX - 1, 8),
                            "half_missing" => (u64::MAX, 0),
                            _ => (payload.len() as u64 + 1, 16),
                        };
                        payload[at + entry * 16..at + entry * 16 + 8]
                            .copy_from_slice(&offset.to_le_bytes());
                        payload[at + entry * 16 + 8..at + entry * 16 + 16]
                            .copy_from_slice(&length.to_le_bytes());
                        let crc = crc32c(&payload[at..at + 64]);
                        payload[at + 64..at + 68].copy_from_slice(&crc.to_le_bytes());
                    }
                }
                let path = format!("{}_{}_{}/value", case.name, start, mutation);
                fixture
                    .array(&path, &meta(&case, Some(start)), Some(payload))
                    .await;
                paths.push(path);
            }
        }
    }
    assert_eq!(paths.len(), 63 * 2 * 7);
    let reader = fixture.reader().await;
    for path in paths {
        let error = reader
            .read_dataset_selection(&path, &[Selector::Index(0)], None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode, "{path}: {error}");
    }
}
