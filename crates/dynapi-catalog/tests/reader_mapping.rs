#![cfg(feature = "reader")]
#[path = "support/fixtures.rs"]
mod fixtures;
use dynapi_catalog::Collection;
use dynapi_reader::StoreDescriptor;
fn collection(name: &str) -> Collection {
    Collection::from_stac(&serde_json::from_slice(&fixtures::fixture(name)).unwrap()).unwrap()
}
#[test]
fn pure_reader_mapping_s3_http_and_declared_containers() {
    let config = collection("synthetic-valid-s3.json")
        .reader_config()
        .unwrap();
    assert!(
        matches!(config.store,StoreDescriptor::AnonymousS3 {bucket,prefix,region} if bucket=="example-bucket" && prefix=="data/v1.icechunk" && region=="us-west-2")
    );
    assert!(config.containers.is_empty());
    let config = collection("synthetic-http.json").reader_config().unwrap();
    assert!(
        matches!(config.store,StoreDescriptor::Http {url} if url=="https://data.dynamical.org/data/v1.icechunk")
    );
    let config = collection("synthetic-http-container.json")
        .reader_config()
        .unwrap();
    assert_eq!(config.containers.len(), 1);
    assert_eq!(
        config.containers[0].url_prefix,
        "https://data.dynamical.org/chunks/"
    );
    assert!(config.containers[0].name.is_none());
    assert_eq!(
        collection("synthetic-s3-container.json")
            .reader_config()
            .unwrap_err()
            .to_string(),
        "Reader does not support non-HTTP virtual chunk containers"
    );
}
#[test]
fn duplicate_http_container_prefixes_authorize_once() {
    let container = serde_json::json!({"url_prefix":"https://data.dynamical.org/chunks/","credentials":{"type":"http"}});
    let other = serde_json::json!({"url_prefix":"https://data.dynamical.org/other/","credentials":{"type":"http"}});
    let collection =
        Collection::from_stac(&serde_json::json!({"id":"duplicates","assets":{"icechunk":{
            "href":"https://data.dynamical.org/repo/",
            "icechunk:virtual_chunk_containers":[container, other, container],
        }}}))
        .unwrap();
    // The recipe keeps the catalog's declarations; the reader gets Python's dict.
    assert_eq!(collection.virtual_chunk_containers().len(), 3);
    let prefixes: Vec<_> = collection
        .reader_config()
        .unwrap()
        .containers
        .into_iter()
        .map(|c| c.url_prefix)
        .collect();
    assert_eq!(
        prefixes,
        [
            "https://data.dynamical.org/chunks/",
            "https://data.dynamical.org/other/"
        ]
    );
}

#[test]
fn scalar_regions_map_to_explicit_reader_text() {
    for (region, expected) in [
        (serde_json::json!(1), "1"),
        (serde_json::json!(true), "True"),
        (serde_json::json!(1e-7), "1e-07"),
        (serde_json::json!("us-west-2"), "us-west-2"),
    ] {
        let value = serde_json::json!({"id":"example","assets":{"icechunk":{
            "href":"s3://example-bucket/prefix/",
            "xarray:storage_options":{"client_kwargs":{"region_name":region}}
        }}});
        let mapped = Collection::from_stac(&value)
            .unwrap()
            .reader_config()
            .unwrap();
        assert!(
            matches!(mapped.store, StoreDescriptor::AnonymousS3 {bucket, prefix, region}
            if bucket == "example-bucket" && prefix == "prefix" && region == expected)
        );
        assert!(mapped.containers.is_empty());
    }
}
