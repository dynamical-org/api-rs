#![cfg(feature = "harness")]
#[path = "support/history_families.rs"]
mod history;
mod support;

use history::{Family, History, LAMBERT_A_LOCATION, LAMBERT_B_LOCATION};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use support::Reply;

const MUTABLE: &str = "public, max-age=30, stale-while-revalidate=120";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

fn ok(reply: &Reply) {
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
}
fn data_headers(reply: &Reply, cache_control: &str) {
    ok(reply);
    assert_eq!(reply.headers["cache-control"], cache_control);
    assert_eq!(
        reply.headers["etag"],
        format!("\"{:x}\"", Sha256::digest(&reply.body))
    );
    assert_eq!(
        reply.headers["content-length"],
        reply.body.len().to_string()
    );
    assert_eq!(reply.headers["content-type"], "application/json");
}
fn link(reply: &Reply, family: Family) -> String {
    ok(reply);
    let result = &reply.json()["results"][0];
    let links = if matches!(family, Family::Analysis) {
        &result["links"]
    } else {
        &result["forecasts"][0]["links"]
    };
    links["canonical"].as_str().unwrap().to_owned()
}
fn expected_link(family: Family, snapshot: &str, variables: &str) -> String {
    let (indices, bounds) = match family {
        Family::Analysis => (
            "0,0",
            "startTime=2026-07-30T01%3A00%3A00Z&endTime=2026-07-30T03%3A00%3A00Z",
        ),
        Family::Lambert => ("0,2", "initTime=2026-07-30T00%3A00%3A00Z"),
        _ => ("0,0", "initTime=2026-07-30T00%3A00%3A00Z"),
    };
    format!(
        "/v1/data/{}/{snapshot}/points/{indices}?variables={variables}&{bounds}",
        family.id()
    )
}

async fn begin(history: &History, query: &Value) -> (Reply, Reply, String) {
    let post = history
        .server
        .request("POST", history.family.route(), query)
        .await;
    data_headers(&post, MUTABLE);
    assert_eq!(post.json()["results"][0]["snapshotId"], history.a);
    let path = link(&post, history.family);
    assert_eq!(path, expected_link(history.family, &history.a, "count"));
    let get = history.server.request("GET", &path, &Value::Null).await;
    data_headers(&get, IMMUTABLE);
    assert_eq!(get.json()["snapshotId"], history.a);
    assert_eq!(get.headers["x-dynamical-dataset-cache"], "miss");
    (post, get, path)
}

async fn advance(history: &History, query: &Value, post_a: &Reply) -> (String, Reply, Reply) {
    let b = history.advance().await;
    assert_ne!(history.a, b);
    history.server.clock.0.store(29, Ordering::SeqCst);
    let cached = history
        .server
        .request("POST", history.family.route(), query)
        .await;
    assert_eq!(cached.body, post_a.body);
    assert_eq!(cached.headers["etag"], post_a.headers["etag"]);
    assert_eq!(cached.headers["x-dynamical-dataset-cache"], "hit");
    history.server.clock.0.store(30, Ordering::SeqCst);
    let post_b = history
        .server
        .request("POST", history.family.route(), query)
        .await;
    data_headers(&post_b, MUTABLE);
    assert_eq!(post_b.json()["results"][0]["snapshotId"], b);
    assert_ne!(post_b.headers["etag"], post_a.headers["etag"]);
    assert_eq!(post_b.headers["x-dynamical-dataset-cache"], "miss");
    let path = link(&post_b, history.family);
    assert_eq!(path, expected_link(history.family, &b, "count"));
    let get_b = history.server.request("GET", &path, &Value::Null).await;
    data_headers(&get_b, IMMUTABLE);
    assert_eq!(get_b.json()["snapshotId"], b);
    assert_eq!(get_b.headers["x-dynamical-dataset-cache"], "miss");
    // main B and pinned B now occupy both slots, so A must be opened again.
    (b, post_b, get_b)
}

async fn replay_a(history: &History, first: &Reply, path: &str) -> Reply {
    history.server.host.reads.paths.lock().unwrap().clear();
    let reopened = history.server.request("GET", path, &Value::Null).await;
    data_headers(&reopened, IMMUTABLE);
    assert_eq!(reopened.body, first.body);
    assert_eq!(reopened.headers["etag"], first.headers["etag"]);
    assert_eq!(reopened.headers["x-dynamical-dataset-cache"], "miss");
    assert!(
        history
            .server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p
                == &format!(
                    "/{}/repository/snapshots/{}",
                    history.family.id(),
                    history.a
                ))
    );
    let warm = history.server.request("GET", path, &Value::Null).await;
    data_headers(&warm, IMMUTABLE);
    assert_eq!(warm.body, first.body);
    assert_eq!(warm.headers["etag"], first.headers["etag"]);
    assert_eq!(warm.headers["x-dynamical-dataset-cache"], "hit");
    reopened
}

async fn old_schema_rejects_humidity(history: &History) {
    let path = expected_link(history.family, &history.a, "count%2Chumidity");
    let reply = history.server.request("GET", &path, &Value::Null).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":format!("Variable(s) listed in the catalog are absent from snapshot {}: humidity",history.a)})
    );
    for absent in ["etag", "cache-control", "retry-after"] {
        assert!(!reply.headers.contains_key(absent));
    }
}

#[tokio::test]
async fn analysis_history_preserves_half_open_windows_bytes_schema_and_latest_time() {
    let history = History::new(Family::Analysis).await;
    let query = history.family.request(&["count"]);
    let (post_a, get_a, path_a) = begin(&history, &query).await;
    let expected = format!(concat!(
        "{{\"dataProduct\":{{\"id\":\"fixture-history-analysis\",\"version\":\"1.2.3\"}},",
        "\"point\":{{\"selected\":{{\"latitude\":44.0,\"longitude\":0.0,\"grid\":{{\"latitudeIndex\":0,\"longitudeIndex\":0}}}},\"method\":\"nearest\"}},",
        "\"variables\":{{\"count\":{{\"unit\":null}}}},\"snapshotId\":\"{}\",",
        "\"analysisTimes\":[\"2026-07-30T01:00:00Z\"],",
        "\"links\":{{\"canonical\":\"{}\",\"catalog\":\"https://example.invalid/fixture-history-analysis/collection.json\"}},",
        "\"data\":{{\"count\":[1010]}}}}"
    ),history.a,path_a).into_bytes();
    assert_eq!(get_a.body, expected);
    assert_eq!(post_a.json()["results"][0]["data"], json!({"count":[1010]}));

    let (b, post_b, get_b) = advance(&history, &query, &post_a).await;
    let times_b = json!(["2026-07-30T01:00:00Z", "2026-07-30T02:00:00Z"]);
    for result in [&post_b.json()["results"][0], &get_b.json()] {
        assert_eq!(result["analysisTimes"], times_b);
        assert_eq!(result["data"], json!({"count":[2010,2020]}));
    }
    replay_a(&history, &get_a, &path_a).await;
    old_schema_rejects_humidity(&history).await;

    let mut new_query = history.family.request(&["count", "humidity"]);
    new_query["queries"][0]["endTime"] = json!("2026-07-30T02:00:00Z");
    let bounded = history
        .server
        .request("POST", "/v1/analyses", &new_query)
        .await;
    data_headers(&bounded, MUTABLE);
    let result = &bounded.json()["results"][0];
    assert_eq!(result["snapshotId"], b);
    assert_eq!(result["analysisTimes"], json!(["2026-07-30T01:00:00Z"]));
    assert_eq!(
        result["variables"],
        json!({"count":{"unit":null},"humidity":{"unit":"%"}})
    );
    assert_eq!(result["data"], json!({"count":[2010],"humidity":[51.]}));
    let bounded_get = history
        .server
        .request("GET", &link(&bounded, history.family), &Value::Null)
        .await;
    data_headers(&bounded_get, IMMUTABLE);
    assert_eq!(bounded_get.json()["data"], result["data"]);
    assert_eq!(bounded_get.json()["analysisTimes"], result["analysisTimes"]);

    // Appended time is absent from A, and both start and exclusive end survive
    // canonical replay rather than silently selecting the new main's axis.
    for (snapshot, status, data) in [(&history.a, 422, Value::Null), (&b, 200, json!([2020]))] {
        let path = expected_link(history.family, snapshot, "count").replace("T01%3A", "T02%3A");
        let reply = history.server.request("GET", &path, &Value::Null).await;
        assert_eq!(reply.status, status);
        if status == 422 {
            assert_eq!(
                reply.json(),
                json!({"detail":"No analysis times match the requested bounds"})
            );
        } else {
            data_headers(&reply, IMMUTABLE);
            assert_eq!(
                reply.json()["analysisTimes"],
                json!(["2026-07-30T02:00:00Z"])
            );
            assert_eq!(reply.json()["data"]["count"], data);
        }
    }
    for (snapshot, count, hour) in [(&history.a, 1010, "01"), (&b, 2020, "02")] {
        let path = format!(
            "/v1/data/{}/{snapshot}/points/0,0?variables=count",
            history.family.id()
        );
        let reply = history.server.request("GET", &path, &Value::Null).await;
        data_headers(&reply, IMMUTABLE);
        assert_eq!(
            reply.json()["analysisTimes"],
            json!([format!("2026-07-30T{hour}:00:00Z")])
        );
        assert_eq!(reply.json()["data"]["count"], json!([count]));
    }
}

fn assert_members(result: &Value, ids: &[i64], second: bool, humidity: bool) {
    let forecast = &result["forecasts"][0];
    assert!(forecast.get("data").is_none());
    assert_eq!(forecast["initTime"], "2026-07-30T00:00:00Z");
    assert_eq!(
        forecast["validTimes"],
        if second {
            json!([
                "2026-07-30T00:00:00Z",
                "2026-07-30T01:00:00Z",
                "2026-07-30T02:00:00Z"
            ])
        } else {
            json!(["2026-07-30T00:00:00Z", "2026-07-30T01:00:00Z"])
        }
    );
    assert_eq!(
        forecast["leadTimeHours"],
        if second {
            json!([0., 1., 2.])
        } else {
            json!([0., 1.])
        }
    );
    let members = forecast["ensembleMembers"].as_array().unwrap();
    assert_eq!(members.len(), ids.len());
    for (ordinal, &id) in ids.iter().enumerate() {
        let base = if second { 2000 } else { 1000 } + 1000 * ordinal as i64;
        let values: Vec<_> = (0..if second { 3 } else { 2 })
            .map(|t| base + 10 * t)
            .collect();
        let expected = if humidity {
            json!({"member":id,"data":{"count":values,"humidity":[50.+ordinal as f64,51.+ordinal as f64,52.+ordinal as f64]}})
        } else {
            json!({"member":id,"data":{"count":values}})
        };
        assert_eq!(members[ordinal], expected);
    }
}

async fn ensemble_history(change_members: bool) {
    let history = History::new(Family::Ensemble { change_members }).await;
    let query = history.family.request(&["count"]);
    let (post_a, get_a, path_a) = begin(&history, &query).await;
    assert_members(&get_a.json(), &[7, -2, 7], false, false);
    assert_members(&post_a.json()["results"][0], &[7, -2, 7], false, false);
    // Same identity 7 occurs twice but has different ordinal-owned values.
    assert_eq!(
        get_a.json()["forecasts"][0]["ensembleMembers"][2]["data"]["count"],
        json!([3000, 3010])
    );
    let (b, post_b, get_b) = advance(&history, &query, &post_a).await;
    let ids: &[i64] = if change_members {
        &[-2, 11]
    } else {
        &[7, -2, 7]
    };
    assert_members(&post_b.json()["results"][0], ids, true, false);
    assert_members(&get_b.json(), ids, true, false);
    assert_eq!(
        post_b.json()["results"][0]["forecasts"][0],
        get_b.json()["forecasts"][0]
    );
    replay_a(&history, &get_a, &path_a).await;
    old_schema_rejects_humidity(&history).await;
    let added = history
        .server
        .request(
            "POST",
            "/v1/forecasts",
            &history.family.request(&["count", "humidity"]),
        )
        .await;
    data_headers(&added, MUTABLE);
    assert_eq!(added.json()["results"][0]["snapshotId"], b);
    assert_eq!(
        added.json()["results"][0]["variables"],
        json!({"count":{"unit":null},"humidity":{"unit":"%"}})
    );
    assert_members(&added.json()["results"][0], ids, true, true);
    let get = history
        .server
        .request("GET", &link(&added, history.family), &Value::Null)
        .await;
    data_headers(&get, IMMUTABLE);
    assert_members(&get.json(), ids, true, true);
}

#[tokio::test]
async fn ensemble_history_retains_duplicate_member_order_with_an_unchanged_axis() {
    ensemble_history(false).await;
}
#[tokio::test]
async fn ensemble_history_pins_old_member_ids_and_cardinality_when_the_axis_changes() {
    ensemble_history(true).await;
}

fn assert_lambert_location(result: &Value, expected: (f64, f64)) {
    let selected = &result["point"]["selected"];
    assert_eq!(selected["grid"], json!({"yIndex":0,"xIndex":2}));
    assert!((selected["latitude"].as_f64().unwrap() - expected.0).abs() < 1e-10);
    assert!((selected["longitude"].as_f64().unwrap() - expected.1).abs() < 1e-10);
}

#[tokio::test]
async fn lambert_history_preserves_stored_indices_and_returned_location() {
    let history = History::new(Family::Lambert).await;
    let query = history.family.request(&["count"]);
    let (post_a, get_a, path_a) = begin(&history, &query).await;
    let selected = get_a.json()["point"]["selected"].clone();
    assert_eq!(selected["grid"], json!({"yIndex":0,"xIndex":2}));
    assert_eq!(
        get_a.json()["forecasts"][0]["data"],
        json!({"count":[1002,1012]})
    );
    assert_eq!(post_a.json()["results"][0]["point"]["selected"], selected);
    let (b, post_b, get_b) = advance(&history, &query, &post_a).await;
    // B moves both selected axes. The cold historical GET must reconstruct A's
    // non-origin geography, not use the current main's axes or swap y and x.
    let reopened_a = replay_a(&history, &get_a, &path_a).await;
    assert_lambert_location(&reopened_a.json(), LAMBERT_A_LOCATION);
    assert_eq!(
        reopened_a.json()["forecasts"][0]["links"]["canonical"],
        expected_link(history.family, &history.a, "count")
    );
    assert_eq!(
        reopened_a.json()["forecasts"][0]["data"],
        json!({"count":[1002,1012]})
    );
    for result in [&post_b.json()["results"][0], &get_b.json()] {
        assert_lambert_location(result, LAMBERT_B_LOCATION);
        assert_ne!(result["point"]["selected"], selected);
        assert_eq!(
            result["forecasts"][0]["validTimes"],
            json!([
                "2026-07-30T00:00:00Z",
                "2026-07-30T01:00:00Z",
                "2026-07-30T02:00:00Z"
            ])
        );
        assert_eq!(
            result["forecasts"][0]["data"],
            json!({"count":[2002,2012,2022]})
        );
    }
    old_schema_rejects_humidity(&history).await;
    let added = history
        .server
        .request(
            "POST",
            "/v1/forecasts",
            &history.family.request(&["count", "humidity"]),
        )
        .await;
    data_headers(&added, MUTABLE);
    let result = &added.json()["results"][0];
    assert_eq!(result["snapshotId"], b);
    assert_lambert_location(result, LAMBERT_B_LOCATION);
    assert_eq!(
        result["variables"],
        json!({"count":{"unit":null},"humidity":{"unit":"%"}})
    );
    assert_eq!(
        result["forecasts"][0]["data"],
        json!({"count":[2002,2012,2022],"humidity":[52.,53.,54.]})
    );
    let get = history
        .server
        .request("GET", &link(&added, history.family), &Value::Null)
        .await;
    data_headers(&get, IMMUTABLE);
    assert_lambert_location(&get.json(), LAMBERT_B_LOCATION);
    assert_eq!(get.json()["forecasts"][0], result["forecasts"][0]);
}

fn assert_runs(reply: &Reply, history: &History, snapshot: &str, hours: &[&str]) {
    ok(reply);
    assert_eq!(
        reply.json(),
        json!({"dataProductId":history.family.id(),"snapshotId":snapshot,
        "runs":hours.iter().map(|hour|json!({"initTime":format!("2026-07-30T{hour}:00:00Z")})).collect::<Vec<_>>()})
    );
    assert_eq!(reply.headers["cache-control"], MUTABLE);
    assert_eq!(reply.headers["content-type"], "application/json");
    assert_eq!(
        reply.headers["content-length"],
        reply.body.len().to_string()
    );
    for absent in [
        "etag",
        "server-timing",
        "x-dynamical-dataset-cache",
        "x-dynamical-work-units",
    ] {
        assert!(!reply.headers.contains_key(absent), "{absent}");
    }
}
#[tokio::test]
async fn run_discovery_refreshes_main_in_either_cache_profile_order() {
    // Metadata-only run discovery and full point queries have separate handles.
    // Refreshing either first must not leave a stale run list or stale point view.
    for runs_first in [true, false] {
        let history = History::new(Family::Forecast).await;
        let path = format!("/v1/forecasts/runs?dataProductId={}", history.family.id());
        let runs_a = history.server.request("GET", &path, &Value::Null).await;
        assert_runs(&runs_a, &history, &history.a, &["06", "00"]);
        let mut query = history.family.request(&["count"]);
        query["queries"][0]["initTime"] = json!("latest");
        let post_a = history
            .server
            .request("POST", "/v1/forecasts", &query)
            .await;
        data_headers(&post_a, MUTABLE);
        assert_eq!(post_a.json()["results"][0]["snapshotId"], history.a);
        assert_eq!(
            post_a.json()["results"][0]["forecasts"][0]["initTime"],
            "2026-07-30T06:00:00Z"
        );
        assert_eq!(
            post_a.json()["results"][0]["forecasts"][0]["data"],
            json!({"count":[1100,1110]})
        );
        let b = history.advance().await;
        history.server.clock.0.store(29, Ordering::SeqCst);
        let cached = history.server.request("GET", &path, &Value::Null).await;
        assert_runs(&cached, &history, &history.a, &["06", "00"]);
        assert_eq!(cached.body, runs_a.body);
        let cached_post = history
            .server
            .request("POST", "/v1/forecasts", &query)
            .await;
        assert_eq!(cached_post.body, post_a.body);
        history.server.clock.0.store(30, Ordering::SeqCst);
        let operations = if runs_first {
            ["GET", "POST"]
        } else {
            ["POST", "GET"]
        };
        for method in operations {
            if method == "GET" {
                let runs_b = history.server.request("GET", &path, &Value::Null).await;
                assert_runs(&runs_b, &history, &b, &["12", "06", "00"]);
                assert_ne!(runs_b.body, runs_a.body);
            } else {
                let post_b = history
                    .server
                    .request("POST", "/v1/forecasts", &query)
                    .await;
                data_headers(&post_b, MUTABLE);
                let result = &post_b.json()["results"][0];
                assert_eq!(result["snapshotId"], b);
                assert_eq!(result["forecasts"][0]["initTime"], "2026-07-30T12:00:00Z");
                assert_eq!(
                    result["forecasts"][0]["data"],
                    json!({"count":[2200,2210,2220]})
                );
            }
        }
        let filtered = history
            .server
            .request(
                "GET",
                &format!("{path}&initTimesAfter=2026-07-30T06%3A00%3A00Z&limit=1"),
                &Value::Null,
            )
            .await;
        assert_runs(&filtered, &history, &b, &["12"]);
        // After another TTL expiry the run list must still reflect B.
        history.server.clock.0.store(60, Ordering::SeqCst);
        assert_runs(
            &history.server.request("GET", &path, &Value::Null).await,
            &history,
            &b,
            &["12", "06", "00"],
        );
    }
}
