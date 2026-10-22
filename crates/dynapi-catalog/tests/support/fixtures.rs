use flate2::read::GzDecoder;
use std::{io::Read, path::Path};
pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stac")
        .join(format!("{name}.gz"));
    let mut body = vec![];
    GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_end(&mut body)
        .unwrap();
    body
}
