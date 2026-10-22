use dynapi_contract::{
    json::{parse, ParseError, Value},
    output, validate_forecast, ValidationResult,
};

#[test]
fn float_layout_and_explicit_data_sanitization() {
    for (n, expected) in [
        (1e-5, "1e-05"),
        (1e-4, "0.0001"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e+16"),
        (-0.0, "-0.0"),
        (0.1f32 as f64, "0.10000000149011612"),
        (f64::from_bits(1), "5e-324"),
    ] {
        assert_eq!(
            output::json(&Value::Float(n), true).unwrap(),
            expected.as_bytes()
        );
    }
    assert_eq!(
        output::json(&Value::Float(f64::NAN), true),
        Err(output::JsonError::NonFinite)
    );
    assert_eq!(
        output::json(&Value::data_float(f64::INFINITY), true).unwrap(),
        b"null"
    );
}

#[test]
fn ordered_unicode_and_unique_keys() {
    let value = Value::object([
        ("z", Value::from("é😀\u{7f}\0\n\"\\/")),
        ("a", Value::Bool(true)),
    ]);
    assert_eq!(
        output::json(&value, true).unwrap(),
        br#"{"z":"\u00e9\ud83d\ude00\u007f\u0000\n\"\\/","a":true}"#
    );
    assert_eq!(
        output::json(&Value::from("é\u{7f}"), false).unwrap(),
        "\"é\u{7f}\"".as_bytes()
    );
    assert_eq!(
        output::json(
            &Value::object([("x", Value::Null), ("x", Value::Null)]),
            true
        ),
        Err(output::JsonError::DuplicateKey)
    );
}

#[test]
fn duplicate_keys_last_wins_without_moving_position() {
    let value = parse(br#"{"z":1,"a":2,"z":9007199254740993000000,"n":-0,"f":-0.0}"#).unwrap();
    assert_eq!(
        output::json(&value, false).unwrap(),
        br#"{"z":9007199254740993000000,"a":2,"n":0,"f":-0.0}"#
    );
}

#[test]
fn decoder_offsets_are_unicode_character_positions() {
    assert_eq!(
        parse("{\"é\": 1,}".as_bytes()),
        Err(ParseError::Json {
            position: 8,
            message: "Expecting property name enclosed in double quotes"
        })
    );
    assert_eq!(
        parse(br#"{"x":"\q"}"#),
        Err(ParseError::Json {
            position: 6,
            message: "Invalid \\escape"
        })
    );
    assert_eq!(parse(b"\xff"), Err(ParseError::Body));
}

#[test]
fn decoder_limits_are_recoverable() {
    let deep = format!("{}0{}", "[".repeat(258), "]".repeat(258));
    assert_eq!(parse(deep.as_bytes()), Err(ParseError::Body));
    assert_eq!(parse("9".repeat(4301).as_bytes()), Err(ParseError::Body));
    assert_eq!(parse(br#""\ud800""#), Err(ParseError::Surrogate));
}

#[test]
fn malformed_prefixes_do_not_panic() {
    let body=br#"{"queries":[{"dataProductId":"example","location":{"latitude":-0.0,"longitude":1e-5},"variables":["\ud83d\ude00"]}]}"#;
    for end in 0..=body.len() {
        let _ = validate_forecast(&body[..end], Some("application/json"));
    }
}

#[test]
fn unicode_whitespace_deduplication_and_codepoint_order() {
    let values = [
        "\u{1c}x\u{1f}",
        "\u{2003}é\u{a0}",
        "😀",
        "x",
        "",
        " ",
        "\u{85}z\u{3000}",
    ];
    assert_eq!(
        output::normalize_variables(&values.map(str::to_owned)),
        vec!["x", "z", "é", "😀"]
    );
}

#[test]
fn repr_quotes_controls_and_python_unicode_version() {
    assert_eq!(output::python_repr("it's"), "\"it's\"");
    assert_eq!(output::python_repr("it's \"x\""), "'it\\'s \"x\"'");
    assert_eq!(
        output::python_repr("café\n☃\u{a0}\u{e000}\u{1c89}"),
        "'café\\n☃\\xa0\\ue000\\u1c89'"
    );
    assert_eq!(
        output::product_not_found("it's"),
        br#"{"detail":"Unknown data product \"it's\""}"#
    );
}

#[test]
fn timezone_seconds_and_microseconds_are_preserved() {
    let d = dynapi_contract::DateTime {
        date: dynapi_contract::Date {
            year: 1,
            month: 12,
            day: 31,
        },
        time: dynapi_contract::Time {
            hour: 23,
            minute: 59,
            second: 59,
            microsecond: 1,
            tz_offset: Some(3661),
        },
    };
    assert_eq!(output::datetime(&d), "0001-12-31T23:59:59.000001+01:01:01");
}

#[test]
fn model_validator_waits_for_fields_and_has_query_location() {
    let ValidationResult::Invalid(response)=validate_forecast(br#"{"queries":[{"dataProductId":"example","location":{"latitude":0,"longitude":0},"variables":[1],"initTime":"2026-07-30"}]}"#,Some("application/json")) else {panic!("expected validation error")};
    assert_eq!(response.status, 422);
    let value = parse(&response.body).unwrap();
    let Value::Array(errors) = value.get("detail").unwrap() else {
        panic!("expected detail array")
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].get("type"), Some(&Value::from("string_type")));
}

#[test]
fn typed_large_integers_preserve_sign_and_every_digit() {
    for digits in [4299, 4300, 4301, 5000] {
        let magnitude = num_bigint::BigInt::from(10u8).pow(digits - 1);
        for negative in [false, true] {
            let integer = if negative {
                -&magnitude
            } else {
                magnitude.clone()
            };
            let decimal = format!(
                "{}1{}",
                if negative { "-" } else { "" },
                "0".repeat(digits as usize - 1)
            );
            for ensure_ascii in [true, false] {
                assert_eq!(
                    output::json(&Value::Int(integer.clone()), ensure_ascii).unwrap(),
                    decimal.as_bytes()
                );
            }
        }
    }
}

#[test]
fn excessive_depth_returns_the_explicit_parse_envelope() {
    for depth in [258, 10_000] {
        let body = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let ValidationResult::Invalid(response) =
            validate_forecast(body.as_bytes(), Some("application/json"))
        else {
            panic!("deep body must fail parsing");
        };
        assert_eq!(response.status, 400);
        assert_eq!(
            response.body,
            br#"{"detail":"There was an error parsing the body"}"#
        );
    }
}

#[test]
fn surrogate_rejection_precedes_overwrite_and_handler_entry() {
    for body in [
        br#"{"queries":[],"discarded":"\ud800","discarded":0}"#.as_slice(),
        br#"{"queries":[],"_sa_hidden":"\udfff"}"#,
        br#"{"queries":[{"dataProductId":"example","location":{"latitude":0,"longitude":0},"variables":["\ud800"]}]}"#,
    ] {
        assert_eq!(parse(body), Err(ParseError::Surrogate));
        let ValidationResult::Invalid(response) = validate_forecast(body, Some("application/json")) else {
            panic!("surrogate must never reach a handler");
        };
        assert_eq!(response.status, 500);
        assert_eq!(response.body, b"Internal Server Error");
    }
    let raw = b"{\"queries\":[],\"x\":\"\xed\xa0\x80\",\"x\":0}";
    assert_eq!(parse(raw), Err(ParseError::Body));
    let ValidationResult::Invalid(response) = validate_forecast(raw, Some("application/json"))
    else {
        panic!("invalid UTF-8 must fail parsing");
    };
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        br#"{"detail":"There was an error parsing the body"}"#
    );
}

#[test]
fn nonzero_subminute_offsets_are_preserved() {
    let mut date = dynapi_contract::DateTime::parse_str("2026-01-01T00:00:00Z").unwrap();
    for (seconds, expected) in [
        (0, "2026-01-01T00:00:00Z"),
        (1, "2026-01-01T00:00:00+00:00:01"),
        (16, "2026-01-01T00:00:00+00:00:16"),
        (-1, "2026-01-01T00:00:00-00:00:01"),
        (-16, "2026-01-01T00:00:00-00:00:16"),
    ] {
        date.time.tz_offset = Some(seconds);
        assert_eq!(output::datetime(&date), expected);
    }
}

#[test]
fn response_json_byte_bound_covers_escaped_text_and_numeric_extremes() {
    use dynapi_contract::{output, Value};
    let value = Value::object([
        ("\"\n\t", Value::from("a\u{0}\u{1f600}雪")),
        (
            "values",
            Value::Array(vec![
                Value::from(-9_223_372_036_854_775_808i64),
                Value::from(0i64),
                Value::Float(f64::MAX),
                Value::Float(f64::MIN_POSITIVE),
                Value::Bool(false),
                Value::Null,
            ]),
        ),
    ]);
    let bound = output::json_byte_bound(&value).unwrap();
    for ascii in [false, true] {
        assert!(output::json(&value, ascii).unwrap().len() <= bound);
    }
}

#[test]
fn representation_defaults_and_literal_errors_have_owned_ordered_shapes() {
    use dynapi_contract::{validate_forecast, ValidationResult, Value};
    let base = r#"{"dataProductId":"synthetic","location":{"latitude":0,"longitude":0},"variables":["temperature"]"#;
    for (suffix, expected) in [
        ("", "auto"),
        (r#", "representation":"auto""#, "auto"),
        (r#", "representation":"requested""#, "requested"),
    ] {
        let body = format!("{{\"queries\":[{base}{suffix}}}]}}");
        let ValidationResult::Valid(batch) =
            validate_forecast(body.as_bytes(), Some("application/json"))
        else {
            panic!("valid representation rejected")
        };
        let Value::Array(queries) = batch.normalized().get("queries").unwrap().clone() else {
            panic!("normalized queries missing")
        };
        assert_eq!(
            queries[0].get("representation"),
            Some(&Value::from(expected))
        );
    }
    for input in [r#""bad""#, "null"] {
        let body=format!("{{\"queries\":[{base},\"maxLeadTimeHours\":-1,\"representation\":{input},\"unknown\":true}}]}}");
        let ValidationResult::Invalid(invalid) =
            validate_forecast(body.as_bytes(), Some("application/json"))
        else {
            panic!("invalid literal accepted")
        };
        let parsed = dynapi_contract::json::parse(&invalid.body).unwrap();
        let Value::Array(errors) = parsed.get("detail").unwrap() else {
            panic!("missing detail")
        };
        assert_eq!(
            errors
                .iter()
                .map(|e| e.get("type").unwrap().clone())
                .collect::<Vec<_>>(),
            ["greater_than_equal", "literal_error", "extra_forbidden"].map(Value::from)
        );
        assert_eq!(
            errors[1].get("ctx").unwrap().get("expected"),
            Some(&Value::from("'auto' or 'requested'"))
        );
    }
}
