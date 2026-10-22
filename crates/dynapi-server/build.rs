use md5::{Digest, Md5};
use std::{env, fs, path::PathBuf, time::UNIX_EPOCH};
fn main() {
    let path = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("robots.txt");
    println!("cargo:rerun-if-changed={}", path.display());
    let metadata = fs::metadata(path).expect("robots.txt metadata");
    let modified = metadata.modified().unwrap();
    let mut seconds = String::new();
    dynapi_contract::output::float_repr(
        modified.duration_since(UNIX_EPOCH).unwrap().as_secs_f64(),
        &mut seconds,
    );
    let etag = format!(
        "\"{:x}\"",
        Md5::digest(format!("{seconds}-{}", metadata.len()).as_bytes())
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("robots_metadata.rs"),
        format!(
            "const ROBOTS_LAST_MODIFIED: &str = {:?};\nconst ROBOTS_ETAG: &str = {etag:?};\n",
            httpdate::fmt_http_date(modified)
        ),
    )
    .unwrap();
}
