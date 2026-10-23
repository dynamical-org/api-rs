#![cfg(feature = "harness")]
mod support;

use serde_json::{Value, json};
use support::{Reply, Server, input, payload};

fn path(kind: &str, variables: &str, suffix: &str) -> String {
    format!(
        "/v1/data/fixture-{kind}/{}/points/0,0?variables={variables}{suffix}",
        input(kind)["snapshot_id"].as_str().unwrap()
    )
}
fn forecast(variables: &str) -> String {
    path("forecast", variables, "&initTime=2026-07-30T00:00:00Z")
}
fn assert_error(reply: &Reply, status: u16, detail: &str) {
    assert_eq!(
        reply.status,
        status,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json(), json!({"detail":detail}));
    assert!(!reply.headers.contains_key("etag"));
    assert!(!reply.headers.contains_key("cache-control"));
}

#[tokio::test]
async fn canonical_variables_sort_trim_deduplicate_and_bound_after_normalization() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    for variables in [
        "temperature_2m,count,static,count",
        "%20static%20,%E2%80%83count%C2%A0,temperature_2m,,count",
        "count,static,temperature_2m",
    ] {
        let reply = server
            .request("GET", &forecast(variables), &Value::Null)
            .await;
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let result = reply.json();
        assert_eq!(
            result["variables"],
            json!({"count":{"unit":null},"static":{"unit":"m"},"temperature_2m":{"unit":"degree_Celsius"}})
        );
        assert_eq!(
            result["forecasts"][0]["links"]["canonical"],
            forecast("count%2Cstatic%2Ctemperature_2m").replace("00:00:00Z", "00%3A00%3A00Z")
        );
        let text = String::from_utf8(reply.body).unwrap();
        assert!(text.contains("\"variables\":{\"count\":{\"unit\":null},\"static\":{\"unit\":\"m\"},\"temperature_2m\":"));
    }
    // Bound the normalized unique set, not the number of CSV tokens.
    let reply = server
        .request("GET", &forecast(&vec!["count"; 26].join(",")), &Value::Null)
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["variables"], json!({"count":{"unit":null}}));
    for kind in ["forecast", "analysis"] {
        for n in [25, 26] {
            let names: Vec<_> = (0..n).rev().map(|i| format!("unknown-{i:02}")).collect();
            let suffix = if kind == "forecast" {
                "&initTime=2026-07-30T00:00:00Z"
            } else {
                ""
            };
            let reply = server
                .request("GET", &path(kind, &names.join(","), suffix), &Value::Null)
                .await;
            let detail = if n == 26 {
                "At most 25 variables may be requested".to_owned()
            } else {
                format!(
                    "Unknown variable(s): {}",
                    names.into_iter().rev().collect::<Vec<_>>().join(", ")
                )
            };
            assert_error(&reply, 422, &detail);
        }
        for blank in [",,%20,%E2%80%83", "%20", "%C2%A0"] {
            assert_error(
                &server
                    .request("GET", &path(kind, blank, ""), &Value::Null)
                    .await,
                422,
                "At least one variable is required",
            );
        }
    }
    // Body schema bounds raw list cardinality before handler normalization.
    for n in [25, 26] {
        let mut body = payload();
        body["queries"][0]["variables"] = json!(vec!["count"; n]);
        let reply = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(reply.status, if n == 25 { 200 } else { 422 });
        if n == 26 {
            assert_eq!(
                reply.json(),
                json!({"detail":[{"type":"too_long","loc":["body","queries",0,"variables"],"msg":"List should have at most 25 items after validation, not 26","input":vec!["count";26],"ctx":{"field_type":"List","max_length":25,"actual_length":26}}]})
            );
        }
    }
}

#[tokio::test]
async fn canonical_analysis_window_rules_own_one_sided_equal_timezone_and_long_ranges() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    for (suffix, indices) in [
        ("", vec![4]),
        ("&startTime=2026-07-30T02:00:00Z", vec![2, 3, 4]),
        ("&endTime=2026-07-30T02:00:00Z", vec![0, 1]),
        (
            "&startTime=2026-07-30T02:00:00+01:00&endTime=2026-07-30T04:00:00+01:00",
            vec![1, 2],
        ),
        (
            "&startTime=2026-07-01T00:00:00Z&endTime=2026-09-01T00:00:00Z",
            vec![0, 1, 2, 3, 4],
        ),
    ] {
        let reply = server
            .request(
                "GET",
                &path("analysis", "count", &suffix.replace('+', "%2B")),
                &Value::Null,
            )
            .await;
        assert_eq!(
            reply.status,
            200,
            "{suffix}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(
            reply.json()["analysisTimes"],
            json!(
                indices
                    .iter()
                    .map(|t| format!("2026-07-30T{t:02}:00:00Z"))
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            reply.json()["data"]["count"],
            json!(
                indices
                    .iter()
                    .map(|t| 9_007_199_254_740_993i64 + t * 24)
                    .collect::<Vec<_>>()
            )
        );
    }
    for (suffix, detail) in [
        (
            "&startTime=2026-07-30T00:00:00",
            "startTime must include a timezone",
        ),
        (
            "&endTime=2026-07-30T00:00:00",
            "endTime must include a timezone",
        ),
        (
            "&startTime=2026-07-31T00:00:00Z&endTime=2026-07-30T00:00:00Z",
            "startTime must not be after endTime",
        ),
        (
            "&startTime=2026-07-30T00:00:00Z&endTime=2026-07-30T00:00:00Z",
            "No analysis times match the requested bounds",
        ),
        (
            "&startTime=2026-07-30T04:00:00.000001Z&endTime=2026-07-31T00:00:00Z",
            "No analysis times match the requested bounds",
        ),
    ] {
        assert_error(
            &server
                .request("GET", &path("analysis", "count", suffix), &Value::Null)
                .await,
            422,
            detail,
        );
    }
}

#[tokio::test]
async fn analysis_post_window_validation_has_exact_precedence_and_31_day_boundary() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    for (start, end, message) in [
        (
            "2026-07-30T00:00:00",
            "2026-07-29T00:00:00",
            "startTime must include a timezone",
        ),
        (
            "2026-07-30T00:00:00Z",
            "2026-07-29T00:00:00",
            "endTime must include a timezone",
        ),
        (
            "2026-07-30T00:00:00Z",
            "2026-07-30T00:00:00Z",
            "startTime must be before endTime",
        ),
        (
            "2026-07-31T00:00:00Z",
            "2026-07-30T00:00:00Z",
            "startTime must be before endTime",
        ),
        (
            "2026-07-30T00:00:00Z",
            "2026-08-30T00:00:00.000001Z",
            "Analysis queries may span at most 31 days",
        ),
    ] {
        let q = json!({"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":["count"],"startTime":start,"endTime":end});
        let reply = server
            .request("POST", "/v1/analyses", &json!({"queries":[q.clone()]}))
            .await;
        assert_eq!(reply.status, 422);
        assert_eq!(
            reply.json(),
            json!({"detail":[{"type":"value_error","loc":["body","queries",0],"msg":format!("Value error, {message}"),"input":q,"ctx":{"error":{}}}]})
        );
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    for end in ["2026-08-29T23:59:59.999999Z", "2026-08-30T00:00:00Z"] {
        let reply=server.request("POST","/v1/analyses",&json!({"queries":[{"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":["count"],"startTime":"2026-07-30T01:00:00+01:00","endTime":end}]})).await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.json()["results"][0]["data"]["count"],
            json!(
                (0..5)
                    .map(|t| 9_007_199_254_740_993i64 + t * 24)
                    .collect::<Vec<_>>()
            )
        );
    }
}

#[tokio::test]
async fn encoded_identifiers_are_decoded_once_and_schema_errors_still_precede_lookup() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    let base = forecast("count");
    let encoded = base
        .replace("fixture-forecast", "%66ixture%2Dforecast")
        .replace("/points/0,0", "/points/%30%2C%30");
    let normal = server.request("GET", &base, &Value::Null).await;
    assert_eq!(normal.status, 200);
    let reply = server.request("GET", &encoded, &Value::Null).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, normal.body);
    let snapshot = input("forecast")["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let encoded_snapshot = base.replace(
        &snapshot,
        &format!("%{:02X}{}", snapshot.as_bytes()[0], &snapshot[1..]),
    );
    assert_eq!(
        server
            .request("GET", &encoded_snapshot, &Value::Null)
            .await
            .body,
        normal.body
    );
    for (encoded, detail) in [
        (
            "missing%20product",
            "Unknown data product 'missing product'",
        ),
        (
            "missing%27product",
            "Unknown data product \"missing'product\"",
        ),
        ("%E9%9B%A8", "Unknown data product '雨'"),
        (
            "missing%252Fproduct",
            "Unknown data product 'missing%2Fproduct'",
        ),
    ] {
        let path = base.replace("fixture-forecast", encoded);
        assert_error(
            &server.request("GET", &path, &Value::Null).await,
            404,
            detail,
        );
        let invalid = server
            .request("GET", &format!("{path}&maxLeadTimeHours=-1"), &Value::Null)
            .await;
        assert_eq!(invalid.status, 422);
        assert_eq!(
            invalid.json()["detail"][0]["loc"],
            json!(["query", "maxLeadTimeHours"])
        );
    }
    assert_error(
        &server
            .request(
                "GET",
                &base.replace("fixture-forecast", "fixture%2Fforecast"),
                &Value::Null,
            )
            .await,
        404,
        "Not Found",
    );
    let invalid_snapshot = base.replace(&snapshot, "m%61in");
    assert_error(
        &server.request("GET", &invalid_snapshot, &Value::Null).await,
        404,
        "Unknown snapshot 'main' for data product 'fixture-forecast'; use the snapshotId from a forecasts or analyses response",
    );
}

#[tokio::test]
async fn transient_reader_failure_table_is_identical_on_both_post_and_canonical_routes() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    let analysis = json!({"queries":[{"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":["count"],"startTime":"2026-07-30T00:00:00Z","endTime":"2026-07-31T00:00:00Z"}]});
    for (method, path, body) in [
        ("POST", "/v1/forecasts".to_owned(), payload()),
        ("POST", "/v1/analyses".to_owned(), analysis),
        ("GET", forecast("count"), Value::Null),
        ("GET", path("analysis", "count", ""), Value::Null),
    ] {
        for (kind, virtual_chunk_fetch, message, status) in [
            (
                dynapi_reader::ErrorKind::Storage,
                true,
                "operation timeout (all attempts including retries) occurred after 90 seconds",
                503,
            ),
            (
                dynapi_reader::ErrorKind::Storage,
                true,
                "Error { code: \"SlowDown\", message: synthetic throttle }",
                503,
            ),
            (
                dynapi_reader::ErrorKind::Storage,
                false,
                "operation timeout (all attempts including retries) occurred after 90 seconds",
                500,
            ),
            (
                dynapi_reader::ErrorKind::Storage,
                true,
                "synthetic permanent upstream 403",
                500,
            ),
            (
                dynapi_reader::ErrorKind::Storage,
                true,
                "synthetic upstream 404",
                500,
            ),
            (
                dynapi_reader::ErrorKind::Decode,
                false,
                "synthetic invalid payload",
                500,
            ),
        ] {
            server.app.inject_read_failure(dynapi_reader::ReaderError {
                kind,
                message: message.into(),
                virtual_chunk_fetch,
                exception_class: Some("StorageError"),
                virtual_failure: None,
            });
            let reply = server.request(method, &path, &body).await;
            assert_eq!(
                reply.status,
                status,
                "{method} {path} {message}: {}",
                String::from_utf8_lossy(&reply.body)
            );
            assert!(!reply.headers.contains_key("etag"));
            assert!(!reply.headers.contains_key("cache-control"));
            assert!(!String::from_utf8_lossy(&reply.body).contains(message));
            if status == 503 {
                assert_eq!(
                    reply.body,
                    br#"{"detail":"Upstream data temporarily unavailable"}"#
                );
                assert_eq!(reply.headers["retry-after"], "5");
                assert_eq!(reply.headers["content-type"], "application/json");
            } else {
                assert_eq!(reply.body, b"Internal Server Error");
                assert!(!reply.headers.contains_key("retry-after"));
                assert_eq!(reply.headers["content-type"], "text/plain; charset=utf-8");
            }
            assert_eq!(server.app.gate.snapshot().in_use, 0);
            let recovered = server.request(method, &path, &body).await;
            assert_eq!(recovered.status, 200, "recovery after {message}");
        }
    }
}
