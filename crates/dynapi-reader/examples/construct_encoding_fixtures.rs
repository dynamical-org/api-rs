//! Construct input frames only. Decoded expectations live in literal/arithmetic tests.
use serde_json::{Value, json};
use std::{borrow::Cow, path::Path};
use zarrs::array::{BytesToBytesCodecTraits, CodecOptions, codec::BloscCodec};
use zarrs::metadata_ext::codec::blosc::BloscCodecConfiguration;

fn frames(root: &Path, name: &str, raw: &[u8], width: usize) -> Value {
    std::fs::write(root.join(format!("{name}.raw")), raw).unwrap();
    let mut files = serde_json::Map::new();
    let mut settings = Vec::new();
    for shuffle in ["noshuffle", "shuffle", "bitshuffle"] {
        for level in 0..=9 {
            settings.push((format!("{shuffle}_{level}"), "zstd", shuffle, level));
        }
    }
    settings.push(("lz4".to_owned(), "lz4", "shuffle", 5));
    for (key, cname, shuffle, level) in settings {
        if cname == "lz4" && width != 4 {
            continue;
        }
        let config: BloscCodecConfiguration = serde_json::from_value(
            json!({"cname":cname,"clevel":level,"shuffle":shuffle,"typesize":width,"blocksize":0}),
        )
        .unwrap();
        let encoded = BloscCodec::new_with_configuration(&config)
            .unwrap()
            .encode(Cow::Borrowed(raw), &CodecOptions::default())
            .unwrap()
            .into_owned();
        if level > 0 {
            assert_eq!(
                encoded[2] & 2,
                0,
                "{name}/{key} must be genuinely compressed"
            );
        } else {
            assert_ne!(
                encoded[2] & 2,
                0,
                "{name}/{key} must use memcpy at level zero"
            );
        }
        assert!(encoded[3] as usize == width || encoded[3] == 1);
        let file = format!("{name}-{key}.blosc");
        std::fs::write(root.join(&file), encoded).unwrap();
        files.insert(key.clone(), json!(file.clone()));
        if cname == "zstd" && level == 3 {
            files.insert(shuffle.to_owned(), json!(file));
        }
    }
    Value::Object(files)
}
fn main() {
    let root = std::env::args().nth(1).expect("output directory argument");
    let root = Path::new(&root);
    std::fs::create_dir_all(root).unwrap();
    let mut arrays = Vec::new();
    for (dtype, width, min, max, fill) in [
        ("int8", 1, i8::MIN as i64, i8::MAX as i64, -11),
        ("uint8", 1, 0, u8::MAX as i64, 23),
        ("int16", 2, i16::MIN as i64, i16::MAX as i64, -11),
        ("uint16", 2, 0, u16::MAX as i64, 23),
        ("int32", 4, i32::MIN as i64, i32::MAX as i64, -11),
        ("uint32", 4, 0, u32::MAX as i64, 23),
        ("int64", 8, i64::MIN, i64::MAX, -11),
    ] {
        let values = [min, 0, max, fill, 7, 1, fill];
        let raw: Vec<u8> = (0..256)
            .flat_map(|i| {
                let value = values.get(i).copied().unwrap_or(fill);
                value.to_le_bytes()[..width].to_vec()
            })
            .collect();
        let encoded = frames(root, dtype, &raw, width);
        arrays.push(json!({"dtype":dtype,"width":width,"fill":fill,"values":values,"raw":format!("{dtype}.raw"),"frames":encoded}));
    }
    let mut matrix = Vec::new();
    for (name, base) in [("matrix-a", 0.25f32), ("matrix-b", 1000.5f32)] {
        let raw: Vec<u8> = (0..256)
            .flat_map(|i| (base + (i % 4) as f32).to_le_bytes())
            .collect();
        let encoded = frames(root, name, &raw, 4);
        matrix.push(json!({"name":name,"raw":format!("{name}.raw"),"frames":encoded}));
    }
    let manifest =
        json!({"recipe_version":1,"chunk_elements":256,"integers":arrays,"matrix":matrix});
    std::fs::write(
        root.join("inputs.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}
