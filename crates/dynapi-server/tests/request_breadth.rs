#![cfg(feature = "harness")]
//! Explicit schema/decoder tables; no captured service answers are used.
mod support;

use dynapi_contract::{ForecastBatch, ValidationResult, validate_forecast};
use serde_json::{Value, json};
use support::{Reply, Server, payload};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn raw(server: &Server, path: &str, media: Option<&str>, body: &[u8]) -> Reply {
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    let media = media
        .map(|m| format!("Content-Type: {m}\r\n"))
        .unwrap_or_default();
    socket.write_all(format!("POST {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Length: {}\r\n{media}\r\n",body.len()).as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        socket.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    Reply {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        headers: head
            .split("\r\n")
            .skip(1)
            .map(|line| {
                let (k, v) = line.split_once(':').unwrap();
                (k.to_lowercase(), v.trim().to_owned())
            })
            .collect(),
        body: bytes[end + 4..].to_vec(),
    }
}

fn encode(text: &str, width: usize, little: bool, bom: bool) -> Vec<u8> {
    let mut bytes = match (width, little, bom) {
        (1, _, true) => vec![0xef, 0xbb, 0xbf],
        (2, true, true) => vec![0xff, 0xfe],
        (2, false, true) => vec![0xfe, 0xff],
        (4, true, true) => vec![0xff, 0xfe, 0, 0],
        (4, false, true) => vec![0, 0, 0xfe, 0xff],
        _ => vec![],
    };
    match width {
        1 => bytes.extend_from_slice(text.as_bytes()),
        2 => {
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
        }
        4 => {
            for c in text.chars() {
                bytes.extend_from_slice(&if little {
                    (c as u32).to_le_bytes()
                } else {
                    (c as u32).to_be_bytes()
                });
            }
        }
        _ => unreachable!(),
    }
    bytes
}

fn error(kind: &str, loc: Value, msg: &str, input: Value) -> Value {
    json!({"type":kind,"loc":loc,"msg":msg,"input":input})
}
fn context(mut error: Value, ctx: Value) -> Value {
    error["ctx"] = ctx;
    error
}
fn accepted(query: Value) -> ForecastBatch {
    match validate_forecast(
        json!({"queries":[query]}).to_string().as_bytes(),
        Some("application/json"),
    ) {
        ValidationResult::Valid(batch) => batch,
        ValidationResult::Invalid(r) => {
            panic!("{}: {}", r.status, String::from_utf8_lossy(&r.body))
        }
    }
}
fn errors(body: &str) -> Value {
    let ValidationResult::Invalid(reply) =
        validate_forecast(body.as_bytes(), Some("application/json"))
    else {
        panic!("expected rejection: {body}")
    };
    assert_eq!(
        reply.status,
        422,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    serde_json::from_slice::<Value>(&reply.body).unwrap()["detail"].clone()
}
fn query() -> Value {
    payload()["queries"][0].clone()
}
fn field_error(field: &str, value: Value) -> Value {
    let mut q = query();
    q[field] = value;
    errors(&json!({"queries":[q]}).to_string())
}

#[tokio::test]
async fn media_and_utf_encoding_cross_product_has_exact_http_validation_envelopes() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    // Non-ASCII BMP and supplementary scalars distinguish UTF-16 pairs from UTF-32.
    let text = "{\"unexpected\":\"café 🌦\"}";
    let expected = json!({"detail":[
        error("missing",json!(["body","queries"]),"Field required",json!({"unexpected":"café 🌦"})),
        error("extra_forbidden",json!(["body","unexpected"]),"Extra inputs are not permitted",json!("café 🌦"))]});
    for path in ["/v1/forecasts", "/v1/analyses"] {
        for media in [
            "application/json",
            "Application/JSON; charset=iso-8859-1",
            "application/problem+json",
            "APPLICATION/vnd.example+JSON; x=y",
            "application/+json",
            "application/json; charset=utf-16",
        ] {
            for (width, little) in [(1, false), (2, false), (2, true), (4, false), (4, true)] {
                for bom in [false, true] {
                    let reply = raw(
                        &server,
                        path,
                        Some(media),
                        &encode(text, width, little, bom),
                    )
                    .await;
                    assert_eq!(reply.status, 422, "{path} {media} {width}/{little}/{bom}");
                    assert_eq!(
                        reply.json(),
                        expected,
                        "{path} {media} {width}/{little}/{bom}"
                    );
                    assert!(!reply.headers.contains_key("retry-after"));
                }
            }
        }
        for media in [
            None,
            Some(""),
            Some("text/plain"),
            Some("text/json"),
            Some("application/xml"),
            Some("application/json/extra"),
            Some("application/json, text/plain"),
            Some("json"),
        ] {
            let reply = raw(&server, path, media, text.as_bytes()).await;
            assert_eq!(reply.status, 422, "{media:?}");
            assert_eq!(
                reply.json(),
                json!({"detail":[error("model_attributes_type",json!(["body"]),"Input should be a valid dictionary or object to extract fields from",json!(text))]})
            );
            // Non-JSON bytes cannot be echoed through the UTF-8 error encoder.
            let reply = raw(&server, path, media, b"\xff").await;
            assert_eq!(reply.status, 500);
            assert_eq!(reply.body, b"Internal Server Error");
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn malformed_encoding_json_positions_and_empty_bodies_have_owned_http_errors() {
    let server = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "10000"),
    ])
    .await;
    for path in ["/v1/forecasts", "/v1/analyses"] {
        for bytes in [
            vec![0xff],
            vec![0xc0, 0xaf],
            vec![0xef, 0xbb],
            vec![0xff, 0xfe, b'{'],
            vec![0, 0, 0xfe, 0xff, 0, 0x11, 0, 0],
            vec![0xff, 0xfe, 0, 0, b'{', 0, 0],
        ] {
            let reply = raw(&server, path, Some("application/json"), &bytes).await;
            assert_eq!(reply.status, 400, "{bytes:?}");
            assert_eq!(
                reply.json(),
                json!({"detail":"There was an error parsing the body"})
            );
        }
        for bytes in [b"".as_slice(), b"null"] {
            let reply = raw(&server, path, Some("application/json"), bytes).await;
            assert_eq!(reply.status, 422);
            assert_eq!(
                reply.json(),
                json!({"detail":[error("missing",json!(["body"]),"Field required",Value::Null)]})
            );
        }
        for (text, pos, msg) in [
            (
                "{\"é\":1,}",
                7,
                "Expecting property name enclosed in double quotes",
            ),
            ("[1,]", 3, "Expecting value"),
            ("{\"x\":\"\\q\"}", 6, "Invalid \\escape"),
            ("{}x", 2, "Extra data"),
        ] {
            for (width, little, bom) in [(1, false, false), (2, true, true), (4, false, false)] {
                let reply = raw(
                    &server,
                    path,
                    Some("application/json"),
                    &encode(text, width, little, bom),
                )
                .await;
                assert_eq!(reply.status, 422);
                assert_eq!(
                    reply.json(),
                    json!({"detail":[context(error("json_invalid",json!(["body",pos]),"JSON decode error",json!({})),json!({"error":msg}))]}),
                    "{text} {width}"
                );
            }
        }
        // D2: raw UTF surrogate encodings reject before ordinary validation.
        for (width, little) in [(2, false), (2, true), (4, false), (4, true)] {
            let mut bytes = encode("\"", width, little, true);
            bytes.extend_from_slice(&match (width, little) {
                (2, true) => vec![0, 0xd8],
                (2, false) => vec![0xd8, 0],
                (4, true) => vec![0, 0xd8, 0, 0],
                _ => vec![0, 0, 0xd8, 0],
            });
            bytes.extend(encode("\"", width, little, false));
            let reply = raw(&server, path, Some("application/json"), &bytes).await;
            assert_eq!(reply.status, 500);
            assert_eq!(reply.body, b"Internal Server Error");
            // D2 also rejects bare surrogate code points before JSON syntax
            // validation; the reference can instead emit a syntax-only 422.
            let mut unquoted = encode("", width, little, true);
            let bom_len = unquoted.len();
            unquoted.extend_from_slice(&bytes[bom_len + width..bytes.len() - width]);
            let reply = raw(&server, path, Some("application/json"), &unquoted).await;
            assert_eq!(reply.status, 500);
            assert_eq!(reply.body, b"Internal Server Error");
        }
    }
}

#[test]
fn numeric_coercion_tables_assert_normalized_values_and_full_error_fields() {
    for (input, expected) in [
        (json!(true), 1_f64),
        (json!(false), 0.),
        (json!(" +4.4e1 "), 44.),
        (json!("4_4"), 44.),
        (json!("-0"), -0.),
        (json!(44), 44.),
    ] {
        let mut q = query();
        q["location"]["latitude"] = input;
        assert_eq!(
            accepted(q).queries[0].latitude.to_bits(),
            expected.to_bits()
        );
    }
    for (input, kind, msg) in [
        (Value::Null, "float_type", "Input should be a valid number"),
        (json!([]), "float_type", "Input should be a valid number"),
        (
            json!("forty"),
            "float_parsing",
            "Input should be a valid number, unable to parse string as a number",
        ),
    ] {
        let mut q = query();
        q["location"]["latitude"] = input.clone();
        assert_eq!(
            errors(&json!({"queries":[q]}).to_string()),
            json!([error(
                kind,
                json!(["body", "queries", 0, "location", "latitude"]),
                msg,
                input
            )])
        );
    }
    for (input, expected) in [
        (json!(true), 1),
        (json!(false), 0),
        (json!(2.0), 2),
        (json!(" +002.00 "), 2),
        (json!("1_0"), 10),
        (json!("0-_0"), 0),
        (json!("-0"), 0),
    ] {
        let mut q = query();
        q["maxLeadTimeHours"] = input;
        assert_eq!(
            accepted(q).queries[0]
                .max_lead_time_hours
                .as_ref()
                .unwrap()
                .to_string(),
            expected.to_string()
        );
    }
    for (input, kind, msg) in [
        (
            json!(1.5),
            "int_from_float",
            "Input should be a valid integer, got a number with a fractional part",
        ),
        (
            json!("1e1"),
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
        ),
        (
            json!("_1"),
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
        ),
        (
            json!("1__0"),
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
        ),
        (
            json!("١"),
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
        ),
        (json!([]), "int_type", "Input should be a valid integer"),
    ] {
        assert_eq!(
            field_error("maxLeadTimeHours", input.clone()),
            json!([error(
                kind,
                json!(["body", "queries", 0, "maxLeadTimeHours"]),
                msg,
                input
            )])
        );
    }
    assert_eq!(
        field_error("maxLeadTimeHours", json!(-1)),
        json!([context(
            error(
                "greater_than_equal",
                json!(["body", "queries", 0, "maxLeadTimeHours"]),
                "Input should be greater than or equal to 0",
                json!(-1)
            ),
            json!({"ge":0})
        )])
    );
}

#[test]
fn missing_null_alias_defaults_and_field_error_suppression_are_distinct() {
    let mut q = query();
    for name in ["queryId", "initTime", "maxLeadTimeHours"] {
        q.as_object_mut().unwrap().remove(name);
    }
    let defaults = accepted(q.clone());
    for name in [
        "queryId",
        "initTime",
        "initTimesAfter",
        "initTimesBefore",
        "validTimeStart",
        "validTimeEnd",
        "maxLeadTimeHours",
    ] {
        q[name] = Value::Null;
    }
    assert_eq!(accepted(q), defaults);
    let mut snake = query();
    for (alias, name) in [
        ("dataProductId", "data_product_id"),
        ("initTime", "init_time"),
    ] {
        let value = snake.as_object_mut().unwrap().remove(alias).unwrap();
        snake[name] = value;
    }
    assert_eq!(accepted(snake), accepted(query()));
    for (field, kind, msg) in [
        (
            "dataProductId",
            "string_type",
            "Input should be a valid string",
        ),
        (
            "location",
            "model_attributes_type",
            "Input should be a valid dictionary or object to extract fields from",
        ),
        ("variables", "list_type", "Input should be a valid list"),
    ] {
        let mut q = query();
        q.as_object_mut().unwrap().remove(field);
        assert_eq!(
            errors(&json!({"queries":[q.clone()]}).to_string()),
            json!([error(
                "missing",
                json!(["body", "queries", 0, field]),
                "Field required",
                q.clone()
            )])
        );
        q[field] = Value::Null;
        assert_eq!(
            errors(&json!({"queries":[q]}).to_string()),
            json!([error(
                kind,
                json!(["body", "queries", 0, field]),
                msg,
                Value::Null
            )])
        );
    }
    let mut q = query();
    q["initTime"] = json!("2026-07-30T00:00:00");
    q["location"]["latitude"] = json!("bad");
    let e = errors(&json!({"queries":[q]}).to_string());
    assert_eq!(
        e,
        json!([error(
            "float_parsing",
            json!(["body", "queries", 0, "location", "latitude"]),
            "Input should be a valid number, unable to parse string as a number",
            json!("bad")
        )])
    );
}

#[test]
fn multi_error_order_is_schema_then_extras_then_batch_with_union_branches() {
    // Deliberately reverse source order: field errors follow schema order.
    let q = json!({"extraB":1,"variables":[3,null],"location":{"extra":0,"longitude":181,"latitude":"bad"},"dataProductId":9,"queryId":"","initTime":false,"maxLeadTimeHours":1.5,"representation":null,"extraA":2});
    let expected = json!([
        context(
            error(
                "string_too_short",
                json!(["body", "queries", 0, "queryId"]),
                "String should have at least 1 character",
                json!("")
            ),
            json!({"min_length":1})
        ),
        error(
            "string_type",
            json!(["body", "queries", 0, "dataProductId"]),
            "Input should be a valid string",
            json!(9)
        ),
        error(
            "float_parsing",
            json!(["body", "queries", 0, "location", "latitude"]),
            "Input should be a valid number, unable to parse string as a number",
            json!("bad")
        ),
        context(
            error(
                "less_than_equal",
                json!(["body", "queries", 0, "location", "longitude"]),
                "Input should be less than or equal to 180",
                json!(181)
            ),
            json!({"le":180.})
        ),
        error(
            "extra_forbidden",
            json!(["body", "queries", 0, "location", "extra"]),
            "Extra inputs are not permitted",
            json!(0)
        ),
        error(
            "string_type",
            json!(["body", "queries", 0, "variables", 0]),
            "Input should be a valid string",
            json!(3)
        ),
        error(
            "string_type",
            json!(["body", "queries", 0, "variables", 1]),
            "Input should be a valid string",
            Value::Null
        ),
        error(
            "datetime_type",
            json!(["body", "queries", 0, "initTime", "datetime"]),
            "Input should be a valid datetime",
            json!(false)
        ),
        context(
            error(
                "literal_error",
                json!(["body", "queries", 0, "initTime", "literal['latest']"]),
                "Input should be 'latest'",
                json!(false)
            ),
            json!({"expected":"'latest'"})
        ),
        error(
            "int_from_float",
            json!(["body", "queries", 0, "maxLeadTimeHours"]),
            "Input should be a valid integer, got a number with a fractional part",
            json!(1.5)
        ),
        context(
            error(
                "literal_error",
                json!(["body", "queries", 0, "representation"]),
                "Input should be 'auto' or 'requested'",
                Value::Null
            ),
            json!({"expected":"'auto' or 'requested'"})
        ),
        error(
            "extra_forbidden",
            json!(["body", "queries", 0, "extraB"]),
            "Extra inputs are not permitted",
            json!(1)
        ),
        error(
            "extra_forbidden",
            json!(["body", "queries", 0, "extraA"]),
            "Extra inputs are not permitted",
            json!(2)
        ),
        error(
            "model_attributes_type",
            json!(["body", "queries", 1]),
            "Input should be a valid dictionary or object to extract fields from",
            Value::Null
        ),
        error(
            "extra_forbidden",
            json!(["body", "extra"]),
            "Extra inputs are not permitted",
            json!(true)
        )
    ]);
    assert_eq!(
        errors(&json!({"extra":true,"queries":[q,null]}).to_string()),
        expected
    );
}

#[test]
fn datetime_grammar_owns_normalized_precision_offsets_and_epoch_units() {
    for (input, expected) in [
        (json!("2026-07-30t00:00:00z"), "2026-07-30T00:00:00Z"),
        (
            json!("2026-07-30 00:00:00+0130"),
            "2026-07-30T00:00:00+01:30",
        ),
        (
            json!("2026-07-30T00:00:00.123456789Z"),
            "2026-07-30T00:00:00.123456Z",
        ),
        (json!("0001-01-01T00:00:00Z"), "0001-01-01T00:00:00Z"),
        (
            json!("9999-12-31T23:59:59.999999-00:01"),
            "9999-12-31T23:59:59.999999-00:01",
        ),
        (json!(0), "1970-01-01T00:00:00Z"),
        // Numeric datetime coercion retains the parser's negative-fraction rule.
        (json!(-1.25), "1969-12-31T23:59:58.250000Z"),
        (json!(1_785_369_600_000i64), "2026-07-30T00:00:00Z"),
        (json!("1785369600"), "2026-07-30T00:00:00Z"),
    ] {
        let mut q = query();
        q["initTime"] = input.clone();
        let normalized = accepted(q).normalized();
        let value: Value =
            serde_json::from_slice(&dynapi_contract::output::json(&normalized, false).unwrap())
                .unwrap();
        assert_eq!(value["queries"][0]["initTime"], expected, "{input}");
    }
    for input in ["2026-07-30", "2026-07-30T00:00:00"] {
        let mut q = query();
        q["initTime"] = json!(input);
        assert_eq!(
            errors(&json!({"queries":[q.clone()]}).to_string()),
            json!([context(
                error(
                    "value_error",
                    json!(["body", "queries", 0]),
                    "Value error, initTime must include a timezone",
                    q
                ),
                json!({"error":{}})
            )])
        );
    }
    for (input, kind, reason) in [
        (
            "0000-01-01T00:00:00Z",
            "datetime_parsing",
            "year 0 is out of range",
        ),
        (
            "2026-02-30T00:00:00Z",
            "datetime_from_date_parsing",
            "day value is outside expected range",
        ),
        (
            "2026-07-30T00:00:00+00:00:01",
            "datetime_from_date_parsing",
            "unexpected extra characters at the end of the input",
        ),
        (
            "2026-07-30T24:00:00Z",
            "datetime_from_date_parsing",
            "unexpected extra characters at the end of the input",
        ),
        (
            "2026-07-30T00:00:60Z",
            "datetime_from_date_parsing",
            "unexpected extra characters at the end of the input",
        ),
    ] {
        let message = format!(
            "Input should be a valid datetime{}, {reason}",
            if kind == "datetime_from_date_parsing" {
                " or date"
            } else {
                ""
            }
        );
        assert_eq!(
            field_error("initTime", json!(input)),
            json!([
                context(
                    error(
                        kind,
                        json!(["body", "queries", 0, "initTime", "datetime"]),
                        &message,
                        json!(input)
                    ),
                    json!({"error":reason})
                ),
                context(
                    error(
                        "literal_error",
                        json!(["body", "queries", 0, "initTime", "literal['latest']"]),
                        "Input should be 'latest'",
                        json!(input)
                    ),
                    json!({"expected":"'latest'"})
                )
            ])
        );
    }
}

#[test]
fn digit_limits_distinguish_raw_json_strings_signs_zeros_and_cleaned_retries() {
    let prefix = "{\"queries\":[{\"dataProductId\":\"fixture-forecast\",\"location\":{\"latitude\":44,\"longitude\":0},\"variables\":[\"count\"],\"maxLeadTimeHours\":";
    for digits in [4299, 4300, 4301] {
        let number = format!("1{}", "0".repeat(digits - 1));
        let body = format!("{prefix}{number}}}]}}");
        match validate_forecast(body.as_bytes(), Some("application/json")) {
            ValidationResult::Valid(batch) => {
                assert!(digits <= 4300);
                assert_eq!(
                    batch.queries[0]
                        .max_lead_time_hours
                        .as_ref()
                        .unwrap()
                        .to_string(),
                    number
                );
            }
            ValidationResult::Invalid(r) => {
                assert_eq!(digits, 4301);
                assert_eq!(r.status, 400);
                assert_eq!(
                    r.body,
                    br#"{"detail":"There was an error parsing the body"}"#
                );
            }
        }
        let input = json!(number);
        if digits <= 4300 {
            let mut q = query();
            q["maxLeadTimeHours"] = input;
            assert_eq!(
                accepted(q).queries[0]
                    .max_lead_time_hours
                    .as_ref()
                    .unwrap()
                    .to_string(),
                number
            );
        } else {
            assert_eq!(
                field_error("maxLeadTimeHours", input.clone()),
                json!([error(
                    "int_parsing_size",
                    json!(["body", "queries", 0, "maxLeadTimeHours"]),
                    "Unable to parse input string as an integer, exceeded maximum size",
                    input
                )])
            );
        }
    }
    for input in [
        format!("+{}", "1".repeat(4301)),
        format!(" {} ", "1".repeat(4301)),
        format!("1_{}", "0".repeat(4300)),
    ] {
        let e = field_error("maxLeadTimeHours", json!(input));
        assert_eq!(e[0]["type"], "int_parsing");
    }
    // The string cleaner strips leading zeros; raw JSON never accepts that spelling.
    let mut q = query();
    q["maxLeadTimeHours"] = json!("0".repeat(5000));
    assert_eq!(
        accepted(q).queries[0]
            .max_lead_time_hours
            .as_ref()
            .unwrap()
            .to_string(),
        "0"
    );
}

#[tokio::test]
async fn required_null_and_alias_error_tables_precede_compute_for_both_body_models() {
    let server = Server::start(&[("DYNAMICAL_API_COMPUTE_SLOTS", "1")]).await;
    let _held = server.app.gate.try_acquire().unwrap();
    for path in ["/v1/forecasts", "/v1/analyses"] {
        let mut good = query();
        if path.ends_with("analyses") {
            good.as_object_mut().unwrap().remove("initTime");
            good["dataProductId"] = json!("fixture-analysis");
            good["startTime"] = json!("2026-07-30T00:00:00Z");
            good["endTime"] = json!("2026-07-31T00:00:00Z");
        }
        let fields = if path.ends_with("analyses") {
            vec![
                "dataProductId",
                "location",
                "variables",
                "startTime",
                "endTime",
            ]
        } else {
            vec!["dataProductId", "location", "variables"]
        };
        for field in fields {
            let mut q = good.clone();
            q.as_object_mut().unwrap().remove(field);
            let reply = server
                .request("POST", path, &json!({"queries":[q.clone()]}))
                .await;
            assert_eq!(reply.status, 422);
            assert_eq!(
                reply.json(),
                json!({"detail":[error("missing",json!(["body","queries",0,field]),"Field required",q.clone())]})
            );
            q[field] = Value::Null;
            let (kind, msg) = match field {
                "dataProductId" => ("string_type", "Input should be a valid string"),
                "location" => (
                    "model_attributes_type",
                    "Input should be a valid dictionary or object to extract fields from",
                ),
                "variables" => ("list_type", "Input should be a valid list"),
                _ => ("datetime_type", "Input should be a valid datetime"),
            };
            let reply = server.request("POST", path, &json!({"queries":[q]})).await;
            assert_eq!(reply.status, 422);
            assert_eq!(
                reply.json(),
                json!({"detail":[error(kind,json!(["body","queries",0,field]),msg,Value::Null)]})
            );
        }
        let mut dual = good.clone();
        dual["data_product_id"] = json!("ignored-alias");
        let reply = server
            .request("POST", path, &json!({"queries":[dual]}))
            .await;
        assert_eq!(reply.status, 422);
        assert_eq!(
            reply.json(),
            json!({"detail":[error("extra_forbidden",json!(["body","queries",0,"data_product_id"]),"Extra inputs are not permitted",json!("ignored-alias"))]})
        );
        let mut invalid = good.clone();
        invalid["variables"] = json!([false]);
        let reply = server
            .request("POST", path, &json!({"queries":[good,invalid]}))
            .await;
        assert_eq!(reply.status, 422);
        assert_eq!(
            reply.json(),
            json!({"detail":[error("string_type",json!(["body","queries",1,"variables",0]),"Input should be a valid string",json!(false))]})
        );
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert_eq!(server.app.gate.snapshot().in_use, 1);
}
