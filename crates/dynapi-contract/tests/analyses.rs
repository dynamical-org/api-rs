use dynapi_contract::{
    json::parse, validate_analysis, validate_forecast_runs, AnalysisBatch, Value,
};
fn query() -> &'static str {
    r#"{"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":[" count ","count"],"startTime":"2026-07-30T00:00:00Z","endTime":"2026-07-31T00:00:00Z"}"#
}
fn batch(query: &str) -> String {
    format!("{{\"queries\":[{query}]}}")
}
fn accepted(query: &str) -> AnalysisBatch {
    validate_analysis(batch(query).as_bytes(), Some("application/json")).unwrap()
}
fn errors(query: &str) -> Vec<Value> {
    let response =
        validate_analysis(batch(query).as_bytes(), Some("application/json")).unwrap_err();
    assert_eq!(response.status, 422);
    let value = parse(&response.body).unwrap();
    let Value::Array(errors) = value.get("detail").unwrap() else {
        panic!("validation errors")
    };
    errors.clone()
}
fn params(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
#[test]
fn analyses_have_typed_fields_aliases_and_preserve_variables_for_handler_order() {
    let body = accepted(query());
    let q = &body.queries[0];
    assert_eq!(q.data_product_id, "fixture-analysis");
    assert_eq!(q.query_id, None);
    assert_eq!(q.latitude, 44.);
    assert_eq!(q.variables, [" count ", "count"]);
    let snake = query()
        .replace("dataProductId", "data_product_id")
        .replace("startTime", "start_time")
        .replace("endTime", "end_time");
    assert_eq!(accepted(&snake), body);
    let labeled = query().replace(
        "{\"dataProductId",
        "{\"queryId\":\"point-A\",\"dataProductId",
    );
    assert_eq!(
        accepted(&labeled).queries[0].query_id.as_deref(),
        Some("point-A")
    );
}
#[test]
fn analysis_model_checks_timezone_order_and_elapsed_range_in_order() {
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
            "2026-07-30T00:00:00Z",
            "2026-08-30T00:00:00.000001Z",
            "Analysis queries may span at most 31 days",
        ),
    ] {
        let input = query()
            .replace("2026-07-30T00:00:00Z", start)
            .replace("2026-07-31T00:00:00Z", end);
        let e = errors(&input);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].get("type"), Some(&Value::from("value_error")));
        assert_eq!(
            e[0].get("msg"),
            Some(&Value::from(format!("Value error, {message}")))
        );
        assert_eq!(
            e[0].get("loc"),
            Some(&Value::Array(vec![
                "body".into(),
                "queries".into(),
                0i64.into()
            ]))
        );
    }
    let exact = query().replace("2026-07-31T00:00:00Z", "2026-08-30T00:00:00Z");
    accepted(&exact);
    let offset = exact.replace("2026-07-30T00:00:00Z", "2026-07-30T01:00:00+01:00");
    accepted(&offset);
    let micro = offset.replace("2026-08-30T00:00:00Z", "2026-08-30T00:00:00.000001Z");
    assert_eq!(errors(&micro).len(), 1);
}
#[test]
fn analysis_extras_and_field_errors_precede_model_validation() {
    let input = query()
        .replace("2026-07-30T00:00:00Z", "2026-07-30T00:00:00")
        .replace(
            "{\"dataProductId",
            "{\"representation\":\"auto\",\"dataProductId",
        );
    let e = errors(&input);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].get("type"), Some(&"extra_forbidden".into()));
    for (from, to) in [
        ("\"startTime\":\"2026-07-30T00:00:00Z\",", ""),
        ("\"variables\":[\" count \",\"count\"]", "\"variables\":[]"),
        ("\"latitude\":44", "\"latitude\":91"),
    ] {
        assert_eq!(errors(&query().replace(from, to)).len(), 1);
    }
    for body in ["{}", "{\"queries\":[]}", "{\"queries\":null}"] {
        assert!(validate_analysis(body.as_bytes(), Some("application/json")).is_err());
    }
    let many = format!("{{\"queries\":[{}]}}", [query(); 11].join(","));
    let response = validate_analysis(many.as_bytes(), Some("application/json")).unwrap_err();
    assert_eq!(response.status, 422);
}
#[test]
fn new_body_contract_reuses_media_decode_and_missing_policy() {
    assert_eq!(
        validate_analysis(b"\xff", Some("application/json"))
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(
        validate_analysis(br#"{"queries":["\ud800"]}"#, Some("application/json"))
            .unwrap_err()
            .status,
        500
    );
    assert_eq!(
        validate_analysis(batch(query()).as_bytes(), Some("text/plain"))
            .unwrap_err()
            .status,
        422
    );
    assert_eq!(
        validate_analysis(b"", Some("application/json"))
            .unwrap_err()
            .status,
        422
    );
    assert!(validate_analysis(
        batch(query()).as_bytes(),
        Some("Application/vnd.example+json; charset=utf-8")
    )
    .is_ok());
}
#[test]
fn runs_accept_last_query_value_extras_and_decimal_integer_coercion() {
    let q = validate_forecast_runs(&params(&[
        ("dataProductId", "fixture-forecast"),
        ("unused", "ignored"),
    ]))
    .unwrap();
    assert_eq!(q.limit, 20);
    let q = validate_forecast_runs(&params(&[
        ("dataProductId", "ignored"),
        ("dataProductId", "fixture-forecast"),
        ("limit", "100"),
        ("limit", "1.0"),
        ("initTimesAfter", "2026-07-30T00:00:00"),
    ]))
    .unwrap();
    assert_eq!(q.limit, 1);
    assert_eq!(q.data_product_id, "fixture-forecast");
    assert_eq!(q.init_times_after.unwrap().time.tz_offset, None);
    assert!(validate_forecast_runs(&params(&[
        ("dataProductId", "fixture-forecast"),
        ("initTimesAfter", "2026-07-31T00:00:00Z"),
        ("initTimesBefore", "2026-07-30T00:00:00Z")
    ]))
    .is_ok());
}
#[test]
fn runs_keep_typed_schema_errors_separate_from_route_time_validation() {
    for (value, kind) in [
        ("0", "greater_than_equal"),
        ("-1", "greater_than_equal"),
        ("101", "less_than_equal"),
        (
            "99999999999999999999999999999999999999999999999",
            "less_than_equal",
        ),
        ("1.5", "int_parsing"),
        ("true", "int_parsing"),
    ] {
        let response = validate_forecast_runs(&params(&[
            ("dataProductId", "fixture-forecast"),
            ("limit", value),
        ]))
        .unwrap_err();
        assert_eq!(response.status, 422);
        let detail = parse(&response.body).unwrap();
        let Value::Array(e) = detail.get("detail").unwrap() else {
            panic!("errors")
        };
        assert_eq!(e[0].get("type"), Some(&Value::from(kind)), "{value}");
        assert_eq!(
            e[0].get("loc"),
            Some(&Value::Array(vec!["query".into(), "limit".into()]))
        );
    }
    assert!(validate_forecast_runs(&params(&[("data_product_id", "ignored")])).is_err());
}

#[test]
fn analysis_variable_and_query_id_bounds_use_items_and_unicode_codepoints() {
    let variables = |count: usize| {
        query().replace(
            r#""variables":[" count ","count"]"#,
            &format!(r#""variables":[{}]"#, vec![r#""count""#; count].join(",")),
        )
    };
    assert_eq!(accepted(&variables(25)).queries[0].variables.len(), 25);
    assert_eq!(
        errors(&variables(26))[0].get("type"),
        Some(&"too_long".into())
    );
    let labeled = |count: usize| {
        query().replace(
            "{\"dataProductId",
            &format!("{{\"queryId\":\"{}\",\"dataProductId", "😀".repeat(count)),
        )
    };
    assert_eq!(
        accepted(&labeled(128)).queries[0]
            .query_id
            .as_ref()
            .unwrap()
            .chars()
            .count(),
        128
    );
    assert_eq!(
        errors(&labeled(129))[0].get("type"),
        Some(&"string_too_long".into())
    );
    assert_eq!(
        errors(&labeled(0))[0].get("type"),
        Some(&"string_too_short".into())
    );
}
