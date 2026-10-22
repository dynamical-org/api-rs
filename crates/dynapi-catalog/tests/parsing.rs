//! Explicit catalog input and fail-closed policies; no external-service answers.
use dynapi_catalog::{
    Collection, MAX_DOCUMENT_BYTES, on_dynamical_https_host, parse_collection_document,
    parse_document,
    refresh::{child_urls, request_url},
};
use serde_json::json;
#[path = "support/fixtures.rs"]
mod fixtures;

fn document() -> serde_json::Value {
    json!({"id":"example","assets":{"icechunk":{"href":"https://data.dynamical.org/example/"}}})
}

#[test]
fn bbox_non_finite_strings_parse_like_python_float() {
    let mut value = document();
    value["extent"] = json!({"spatial":{"bbox":[["NaN","Infinity","-Infinity","0"]]}});
    let bbox = Collection::from_stac(&value).unwrap().bbox.unwrap();
    assert!(bbox[0].is_nan());
    assert_eq!(bbox[1..], [f64::INFINITY, f64::NEG_INFINITY, 0.]);
}

#[test]
fn bbox_and_repository_numbers_keep_python_tokens_in_catalog_only_builds() {
    let document = parse_document(br#"{"id":"example","title":1e-7,"assets":{"icechunk":{"href":"https://data.dynamical.org/example/"}},"extent":{"spatial":{"bbox":[[1e-7,0,1,2]]}},"cube:dimensions":{"time":{"step":1e-7}}}"#).unwrap();
    let collection = Collection::from_stac(&document).unwrap();
    let output = serde_json::to_string(&collection).unwrap();
    assert!(output.contains("\"bbox\":[1e-07,"), "{output}");
    assert!(output.contains("\"step\":1e-07"), "{output}");
    let output = serde_json::to_string(&collection.repository_config).unwrap();
    // Raw token assertions catch serde_json feature-unification spelling changes.
    assert!(output.contains("\"name\":1e-07"), "{output}");
}

#[test]
fn unsupported_field_types_are_explicit_errors() {
    for field in [
        "id",
        "title",
        "description",
        "version",
        "license",
        "model_id",
        "model_name",
    ] {
        let mut value = document();
        value[field] = json!({"unsupported":"object"});
        let error = Collection::from_stac(&value).unwrap_err();
        assert_eq!(error.exception_type, "TypeError", "{field}");
        assert_eq!(
            error.message,
            format!("STAC field '{field}' has unsupported type 'dict'")
        );
    }
    let mut value = document();
    value["cube:variables"] = json!({"temperature":{"type":"data","dimensions":["x",1]}});
    assert_eq!(
        Collection::from_stac(&value).unwrap_err().exception_type,
        "TypeError"
    );
    value["cube:variables"] = json!({"temperature":{"type":"data","dimensions":"xy"}});
    assert_eq!(
        Collection::from_stac(&value).unwrap().variables["temperature"].dimensions,
        ["x", "y"]
    );
    value["cube:dimensions"] = json!([["time", {}]]);
    assert_eq!(
        Collection::from_stac(&value).unwrap_err().exception_type,
        "TypeError"
    );
    value["cube:dimensions"] = json!("x");
    assert_eq!(
        Collection::from_stac(&value).unwrap_err().message,
        "dictionary update sequence element #0 has length 1; 2 is required"
    );
}

#[test]
fn bbox_ascii_number_policy_rejects_unsupported_coercions() {
    for text in ["1_000", "١"] {
        let mut value = document();
        value["extent"] = json!({"spatial":{"bbox":[[text,"0","1","2"]]}});
        let error = Collection::from_stac(&value).unwrap_err();
        assert_eq!(error.exception_type, "ValueError");
        assert_eq!(
            error.message,
            format!("could not convert string to float: '{text}'")
        );
    }
    let mut value = document();
    value["extent"] = json!({"spatial":{"bbox":[["\u{2003}-1.25\u{a0}","0","1e2","2"]]}});
    assert_eq!(
        Collection::from_stac(&value).unwrap().bbox,
        Some([-1.25, 0.0, 100.0, 2.0])
    );
}

#[test]
fn read_number_bounds_distinguish_root_abort_and_child_skip() {
    for literal in [
        "NaN",
        "Infinity",
        "1e400",
        "18446744073709551616",
        "-9223372036854775809",
    ] {
        let body = format!(
            "{{\"id\":{literal},\"assets\":{{\"icechunk\":{{\"href\":\"https://data.dynamical.org/example/\"}}}}}}"
        );
        assert_eq!(
            parse_document(body.as_bytes()).unwrap_err().exception_type,
            "ValueError"
        );
        let error = parse_collection_document(body.as_bytes())
            .unwrap()
            .unwrap_err();
        assert_eq!(error.exception_type, "ValueError");
        let body = format!(
            "{{\"id\":\"example\",\"unused\":{literal},\"assets\":{{\"icechunk\":{{\"href\":\"https://data.dynamical.org/example/\"}}}}}}"
        );
        assert!(parse_collection_document(body.as_bytes()).unwrap().is_ok());
    }
}

#[test]
fn parser_depth_surrogate_shape_and_body_limits_are_owned() {
    let deep = format!("{{\"unused\":{}0{}}}", "[".repeat(258), "]".repeat(258));
    assert_eq!(
        parse_document(deep.as_bytes()).unwrap_err().exception_type,
        "RecursionError"
    );
    assert_eq!(
        parse_document(br#"{"unused":"\ud800"}"#)
            .unwrap_err()
            .exception_type,
        "UnicodeDecodeError"
    );
    assert_eq!(
        parse_document(b"[]").unwrap_err().message,
        "Expected a STAC JSON object"
    );
    assert_eq!(
        parse_document(b"{").unwrap_err().exception_type,
        "JSONDecodeError"
    );
    let too_large = vec![b' '; MAX_DOCUMENT_BYTES + 1];
    assert_eq!(
        parse_document(&too_large).unwrap_err().message,
        "STAC document exceeds maximum body size"
    );
}

#[test]
fn repository_host_policy_uses_original_spelling() {
    for (url, allowed) in [
        ("https://data.dynamical.org/repo/", true),
        ("https://data.dynamical.org:443/repo/", false),
        ("https://DATA.dynamical.org/repo/", false),
        ("https://data.dynamical.org.example.test/repo/", false),
        ("https://user@data.dynamical.org/repo/", false),
        ("http://data.dynamical.org/repo/", false),
    ] {
        assert_eq!(on_dynamical_https_host(url), allowed, "{url}");
    }
}

#[test]
fn request_targets_have_explicit_normalization_and_rejection_rules() {
    for (url, target) in [
        ("http://EXAMPLE.test/a/../child.json?", "/child.json?"),
        ("http://example.test/child name.json", "/child%20name.json"),
        (
            "http://example.test/a/./child.json#fragment",
            "/a/child.json",
        ),
    ] {
        let parsed = request_url(url).unwrap();
        assert_eq!(parsed.host_str(), Some("example.test"));
        let actual = match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().into(),
        };
        assert_eq!(actual, target);
    }
    for url in [
        "http://example.test/a\\b",
        "http://example.test/a/%2e%2e/b",
        "http://example.test/a?x='y'",
        "http://127.1/a",
        "http://é.test/a",
        "http://xn--9ca.test/a",
        "http://example.test:65536/a",
    ] {
        assert_eq!(
            request_url(url).unwrap_err().exception_type,
            "InvalidURL",
            "{url}"
        );
    }
    assert_eq!(
        child_urls(
            "https://example.test/root.json",
            &json!({"links":[{"rel":"child","href":"child.json"},{"rel":"child","href":null}]})
        )
        .unwrap(),
        [
            "https://example.test/child.json",
            "https://example.test/root.json"
        ]
    );
}

#[test]
fn synthetic_collection_fields_and_order_come_from_declared_inputs() {
    let value = parse_document(&fixtures::fixture("synthetic-valid-s3.json")).unwrap();
    let collection = Collection::from_stac(&value).unwrap();
    assert_eq!(collection.id, "example-product");
    assert_eq!(collection.title, "example-product");
    assert_eq!(collection.version, "unknown");
    assert_eq!(collection.license, "proprietary");
    assert_eq!(collection.kind(), "analysis");
    assert_eq!(collection.point_query_reason(), None);
    assert_eq!(
        collection
            .dimensions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["latitude", "longitude", "time"]
    );
    assert_eq!(
        collection
            .variables
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["temperature"]
    );
    let temperature = &collection.variables["temperature"];
    assert_eq!(temperature.dimensions, ["time", "latitude", "longitude"]);
    assert_eq!(temperature.unit.as_deref(), Some("K"));
    assert_eq!(temperature.long_name.as_deref(), Some("Temperature"));
    assert_eq!(
        temperature.standard_name.as_deref(),
        Some("air_temperature")
    );
}

#[test]
fn synthetic_security_inputs_are_rejected_without_fetching() {
    for name in [
        "synthetic-credential-material.json",
        "synthetic-signed-container.json",
        "synthetic-false-anonymous.json",
        "synthetic-external.json",
        "synthetic-external-container.json",
        "synthetic-lookalike.json",
        "synthetic-userinfo.json",
        "synthetic-port.json",
        "synthetic-uppercase.json",
        "synthetic-missing-region.json",
        "synthetic-empty-region.json",
    ] {
        assert!(
            parse_collection_document(&fixtures::fixture(name))
                .unwrap()
                .is_err(),
            "{name}"
        );
    }
    assert_eq!(
        parse_document(&fixtures::fixture("synthetic-array.json"))
            .unwrap_err()
            .message,
        "Expected a STAC JSON object"
    );
    assert_eq!(
        parse_document(&fixtures::fixture("synthetic-bad-json.json"))
            .unwrap_err()
            .exception_type,
        "JSONDecodeError"
    );
}
