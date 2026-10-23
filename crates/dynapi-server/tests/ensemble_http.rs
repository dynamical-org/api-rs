#![cfg(feature = "harness")]
#[path = "support/ensemble.rs"]
mod ensemble;
mod support;
use ensemble::{Members, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn payload(id: &str, mixed: bool) -> Value {
    let variables = if mixed {
        json!(["temperature", "wind", "count", "orography", "deterministic"])
    } else {
        json!(["deterministic", "orography"])
    };
    json!({"queries":[
        {"queryId":"earlier","dataProductId":id,"location":{"latitude":44,"longitude":-99},"variables":variables,"initTime":"2026-07-30T00:00:00Z","maxLeadTimeHours":6,"representation":"requested"},
        {"queryId":"later","dataProductId":id,"location":{"latitude":40,"longitude":-103},"variables":variables,"initTime":"2026-07-30T06:00:00Z","validTimeStart":"2026-07-30T12:00:00Z","validTimeEnd":"2026-07-31T00:00:00Z","maxLeadTimeHours":12,"representation":"requested"}
    ]})
}
fn etag(reply: &support::Reply) {
    assert_eq!(
        reply.headers["etag"],
        format!("\"{:x}\"", Sha256::digest(&reply.body))
    );
}

#[tokio::test]
async fn forecast_members_and_static_values_follow_physical_codec_recipes_over_http() {
    let profiles = [
        Members::Zstd,
        Members::Blosc("int16", &[7, -2, 7], 4),
        Members::Blosc("int8", &[-128, 7, 127], 1),
        Members::Blosc("uint8", &[255, 0, 7], 1),
        Members::Bytes("int32", &[-2147483648, 7, 2147483647]),
        Members::Bytes("uint16", &[65535, 0, 7]),
        Members::Bytes("uint32", &[4294967295, 0, 7]),
        Members::Bytes("int64", &[9007199254740993, -2, 7]),
    ];
    for profile in profiles {
        for (sharded, byte_header) in [(false, false), (true, false), (true, true)] {
            let fixture = fixture(profile, sharded, byte_header).await;
            let body = payload(&fixture.id, true);
            let post = fixture.server.request("POST", "/v1/forecasts", &body).await;
            assert_eq!(
                post.status,
                200,
                "dtype={} sharded={sharded} header1={byte_header}: {}",
                profile.dtype(),
                String::from_utf8_lossy(&post.body)
            );
            etag(&post);
            let response = post.json();
            assert_eq!(response["results"].as_array().unwrap().len(), 2);
            for (index, result) in response["results"].as_array().unwrap().iter().enumerate() {
                assert_eq!(
                    result["queryId"],
                    if index == 0 { "earlier" } else { "later" }
                );
                assert_eq!(result["snapshotId"], fixture.snapshot);
                assert_eq!(result["variables"]["orography"]["unit"], "m");
                assert_eq!(result["variables"]["temperature"]["unit"], "K");
                assert_eq!(result["variables"]["wind"]["unit"], "m s-1");
                assert_eq!(result["variables"]["count"]["unit"], Value::Null);
                assert_eq!(
                    result["point"]["selected"],
                    if index == 0 {
                        json!({"latitude":44.,"longitude":-99.,"grid":{"latitudeIndex":0,"longitudeIndex":1}})
                    } else {
                        json!({"latitude":40.,"longitude":-103.,"grid":{"latitudeIndex":1,"longitudeIndex":0}})
                    }
                );

                let forecast = &result["forecasts"][0];
                assert_eq!(result["forecasts"].as_array().unwrap().len(), 1);
                assert!(forecast.get("data").is_none());
                assert_eq!(
                    forecast["initTime"],
                    if index == 0 {
                        "2026-07-30T00:00:00Z"
                    } else {
                        "2026-07-30T06:00:00Z"
                    }
                );
                assert_eq!(
                    forecast["leadTimeHours"],
                    if index == 0 {
                        json!([0., 6.])
                    } else {
                        json!([6., 12.])
                    }
                );
                assert_eq!(
                    forecast["validTimes"],
                    if index == 0 {
                        json!(["2026-07-30T00:00:00Z", "2026-07-30T06:00:00Z"])
                    } else {
                        json!(["2026-07-30T12:00:00Z", "2026-07-30T18:00:00Z"])
                    }
                );
                let members = forecast["ensembleMembers"].as_array().unwrap();
                assert_eq!(members.len(), 3);
                for (position, &identity) in profile.ids().iter().enumerate() {
                    assert_eq!(members[position]["member"], identity);
                    assert!(
                        members[position]["member"].is_i64()
                            || members[position]["member"].is_u64()
                    );
                    let base = if index == 0 {
                        [1, 1001]
                    } else {
                        [11010, 12010]
                    };
                    let offset = 100 * position as i64;
                    let expected = json!({
                        "temperature":[(base[0]+offset) as f64+0.25,(base[1]+offset) as f64+0.25],
                        "wind":[(base[0]+offset) as f64+0.5,(base[1]+offset) as f64+0.5],
                        "count":[9_007_199_254_740_993i64+base[0]+offset,9_007_199_254_740_993i64+base[1]+offset],
                        "orography":[if index==0 {101.} else {110.}],
                        "deterministic":base.map(|v|v as f64)
                    });
                    assert_eq!(
                        members[position]["data"],
                        expected,
                        "dtype={} sharded={sharded} header1={byte_header}",
                        profile.dtype()
                    );
                }
                let get = fixture
                    .server
                    .request(
                        "GET",
                        forecast["links"]["canonical"].as_str().unwrap(),
                        &Value::Null,
                    )
                    .await;
                assert_eq!(get.status, 200, "{}", String::from_utf8_lossy(&get.body));
                assert_eq!(get.json()["forecasts"][0], *forecast);
                assert!(get.headers["cache-control"].ends_with("immutable"));
                etag(&get);
            }
            let again = fixture.server.request("POST", "/v1/forecasts", &body).await;
            assert_eq!(again.body, post.body);
            assert_eq!(again.headers["etag"], post.headers["etag"]);
        }
    }
}

#[tokio::test]
async fn member_independent_queries_use_data_without_inventing_a_member_axis() {
    let fixture = fixture(Members::Zstd, true, false).await;
    let post = fixture
        .server
        .request("POST", "/v1/forecasts", &payload(&fixture.id, false))
        .await;
    assert_eq!(post.status, 200, "{}", String::from_utf8_lossy(&post.body));
    let response = post.json();
    assert_eq!(response["results"].as_array().unwrap().len(), 2);
    for (index, result) in response["results"].as_array().unwrap().iter().enumerate() {
        let forecast = &result["forecasts"][0];
        assert!(forecast.get("ensembleMembers").is_none());
        assert_eq!(
            forecast["data"],
            if index == 0 {
                json!({"deterministic":[1.,1001.],"orography":[101.]})
            } else {
                json!({"deterministic":[11010.,12010.],"orography":[110.]})
            }
        );
        let get = fixture
            .server
            .request(
                "GET",
                forecast["links"]["canonical"].as_str().unwrap(),
                &Value::Null,
            )
            .await;
        assert_eq!(get.status, 200);
        assert_eq!(get.json()["forecasts"][0], *forecast);
    }
}

#[tokio::test]
async fn physical_string_members_remain_an_explicit_unsupported_encoding() {
    let fixture = fixture(Members::Strings, false, false).await;
    let reply = fixture
        .server
        .request("POST", "/v1/forecasts", &payload(&fixture.id, true))
        .await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.body, b"Internal Server Error");
    // Current catalog member axes are numeric; this fixture guards the existing
    // physical string-codec restriction rather than claiming new string support.
}

#[tokio::test]
async fn stored_ensemble_zstd_inputs_have_exact_forecast_values_over_http() {
    let server = support::Server::start(&[]).await;
    let body = json!({"queries":[{"dataProductId":"fixture-ensemble","location":{"latitude":42,"longitude":60},"variables":["temperature_2m","count","static"],"initTime":"2026-07-30T06:00:00Z","maxLeadTimeHours":2,"representation":"requested"}]});
    let post = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(post.status, 200, "{}", String::from_utf8_lossy(&post.body));
    let response = post.json();
    assert_eq!(response["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["results"][0]["forecasts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let forecast = &response["results"][0]["forecasts"][0];
    // Existing committed physical input: base index=(run*5+lead)*24+lat*6+lon;
    // this point has run1,lat1,lon1, hence indices127,151,175. Member expansion
    // precedes source-axis permutation and preserves these base values.
    assert_eq!(
        forecast["ensembleMembers"],
        json!([0,2,7].map(|id|json!({
        "member":id,"data":{
            "temperature_2m":[f64::from(22.7f32),f64::from(25.1f32),f64::from(27.5f32)],
            "count":[9_007_199_254_741_120i64,9_007_199_254_741_144i64,9_007_199_254_741_168i64],
            "static":[7.]
        }
    })))
    );
    assert_eq!(
        forecast["validTimes"],
        json!([
            "2026-07-30T06:00:00Z",
            "2026-07-30T07:00:00Z",
            "2026-07-30T08:00:00Z"
        ])
    );
    etag(&post);
    let get = server
        .request(
            "GET",
            forecast["links"]["canonical"].as_str().unwrap(),
            &Value::Null,
        )
        .await;
    assert_eq!(get.status, 200);
    assert_eq!(get.json()["forecasts"][0], *forecast);
    etag(&get);
}

#[tokio::test]
async fn member_filter_is_not_a_supported_forecast_request_field() {
    let server = support::Server::start(&[]).await;
    let mut body = payload("fixture-ensemble", true);
    body["queries"][0]["ensembleMembers"] = json!([7]);
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":[{"type":"extra_forbidden","loc":["body","queries",0,"ensembleMembers"],"msg":"Extra inputs are not permitted","input":[7]}]})
    );
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/repository/"))
    );
}
