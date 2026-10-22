use serde_json::{Value as Json, json};
use std::sync::Arc;

pub fn axis(name: &str, dtype: &str, size: usize, chunk: usize, attrs: Json) -> Json {
    json!({"zarr_format":3,"node_type":"array","shape":[size],"data_type":dtype,
        "dimension_names":[name],"chunk_grid":{"name":"regular","configuration":{"chunk_shape":[chunk]}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},
        "fill_value":if dtype.starts_with("float") {json!("NaN")} else {json!(0)},
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":attrs})
}

pub async fn repository(path: &std::path::Path, arrays: &[(&str, Json, Option<Vec<u8>>)]) {
    let storage = icechunk::new_local_filesystem_storage(path).await.unwrap();
    let repo = icechunk::Repository::create(None, storage, Default::default(), None, false)
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = icechunk::Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    store
        .set(
            "zarr.json",
            serde_json::to_vec(&json!({"zarr_format":3,"node_type":"group","attributes":{}}))
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    for (name, meta, data) in arrays {
        store
            .set(
                &format!("{name}/zarr.json"),
                serde_json::to_vec(meta).unwrap().into(),
            )
            .await
            .unwrap();
        if let Some(data) = data {
            store
                .set(&format!("{name}/c/0"), data.clone().into())
                .await
                .unwrap();
        }
    }
    store
        .session()
        .write()
        .await
        .commit("Synthetic regression fixture")
        .execute()
        .await
        .unwrap();
}
