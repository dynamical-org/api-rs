//! Rust-owned OpenAPI 3.1 definitions. Keep operation order, schema order and
//! numeric types explicit: they are part of the documentation wire contract.
//! Request bounds/literals come from the same definitions as validation.
use dynapi_contract::{Value as V, output, validation as contract};
use std::sync::OnceLock;

pub(crate) fn bytes() -> &'static [u8] {
    static DOCUMENT: OnceLock<Vec<u8>> = OnceLock::new();
    DOCUMENT.get_or_init(|| output::json(&document(), false).expect("valid OpenAPI document"))
}

fn object(pairs: impl IntoIterator<Item = (&'static str, V)>) -> V {
    V::Object(
        pairs
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
fn add(mut schema: V, pairs: impl IntoIterator<Item = (&'static str, V)>) -> V {
    let V::Object(fields) = &mut schema else {
        unreachable!()
    };
    fields.extend(pairs.into_iter().map(|(key, value)| (key.into(), value)));
    schema
}
fn strings(values: &[&str]) -> V {
    V::Array(values.iter().map(|value| V::from(*value)).collect())
}
fn kind(name: &str) -> V {
    V::object([("type", V::from(name))])
}
fn titled(schema: V, title: &str) -> V {
    add(schema, [("title", V::from(title))])
}
fn field_title(name: &str) -> String {
    let mut title = name.to_ascii_lowercase();
    if let Some(first) = title.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    title
}
fn field(name: &'static str, schema: V) -> (&'static str, V) {
    (name, titled(schema, &field_title(name)))
}
fn reference(name: &str) -> V {
    V::object([("$ref", V::from(format!("#/components/schemas/{name}")))])
}
fn union(schemas: impl IntoIterator<Item = V>) -> V {
    V::object([("anyOf", V::Array(schemas.into_iter().collect()))])
}
fn refs(names: &[&str]) -> V {
    union(names.iter().map(|name| reference(name)))
}
fn nullable(schema: V) -> V {
    union([schema, kind("null")])
}
fn array(items: V) -> V {
    V::object([("items", items), ("type", V::from("array"))])
}
fn dictionary(values: V) -> V {
    V::object([
        ("additionalProperties", values),
        ("type", V::from("object")),
    ])
}
fn datetime() -> V {
    add(kind("string"), [("format", V::from("date-time"))])
}
fn literal(value: &str) -> V {
    add(kind("string"), [("const", V::from(value))])
}
fn enumeration(values: &[&str]) -> V {
    add(kind("string"), [("enum", strings(values))])
}
fn nonempty() -> V {
    add(
        kind("string"),
        [("minLength", V::from(contract::MIN_NONEMPTY_LENGTH))],
    )
}
fn query_id() -> V {
    nullable(add(
        kind("string"),
        [
            ("maxLength", V::from(contract::MAX_QUERY_ID_LENGTH)),
            ("minLength", V::from(contract::MIN_NONEMPTY_LENGTH)),
        ],
    ))
}
fn bounded_array(items: V, max: usize) -> V {
    add(
        array(items),
        [
            ("maxItems", V::from(max)),
            ("minItems", V::from(contract::MIN_NONEMPTY_LENGTH)),
        ],
    )
}
fn model(name: &str, fields: Vec<(&'static str, V)>, required: &[&str]) -> V {
    V::object([
        ("properties", object(fields)),
        ("additionalProperties", V::Bool(false)),
        ("type", V::from("object")),
        ("required", strings(required)),
        ("title", V::from(name)),
    ])
}
fn series_data() -> V {
    dictionary(array(union([
        kind("number"),
        kind("integer"),
        kind("null"),
    ])))
}
fn query_fields() -> Vec<(&'static str, V)> {
    vec![
        field("queryId", query_id()),
        field("dataProductId", nonempty()),
        ("location", reference("Location")),
        field(
            "variables",
            bounded_array(kind("string"), contract::MAX_VARIABLES),
        ),
    ]
}
fn result_fields(forecast: bool) -> Vec<(&'static str, V)> {
    let mut fields = vec![
        field("queryId", query_id()),
        ("dataProduct", reference("DataProductReference")),
        ("point", reference("Point")),
    ];
    fields.push(if forecast {
        field(
            "forecasts",
            array(refs(&[
                "DeterministicForecastSeries",
                "EnsembleForecastSeries",
            ])),
        )
    } else {
        field("analysisTimes", array(datetime()))
    });
    fields.extend([
        field("variables", dictionary(reference("Variable"))),
        field("snapshotId", kind("string")),
        (
            "links",
            reference(if forecast {
                "CatalogLink"
            } else {
                "AnalysisLinks"
            }),
        ),
    ]);
    fields
}
fn schemas() -> V {
    let mut schemas = Vec::new();
    for (batch, query) in [
        ("AnalysisBatch", "AnalysisQuery"),
        ("ForecastBatch", "ForecastQuery"),
    ] {
        schemas.push((
            batch,
            model(
                batch,
                vec![field(
                    "queries",
                    bounded_array(reference(query), contract::MAX_QUERIES),
                )],
                &["queries"],
            ),
        ));
    }
    for (name, items) in [
        (
            "AnalysisBatchResponse",
            refs(&["DeterministicAnalysisResult", "EnsembleAnalysisResult"]),
        ),
        ("ForecastBatchResponse", reference("ForecastResult")),
    ] {
        schemas.push((
            name,
            model(name, vec![field("results", array(items))], &["results"]),
        ));
    }
    schemas.push((
        "AnalysisLinks",
        model(
            "AnalysisLinks",
            vec![
                field("canonical", kind("string")),
                field("catalog", kind("string")),
            ],
            &["canonical", "catalog"],
        ),
    ));
    let mut analysis = query_fields();
    analysis.extend([field("startTime", datetime()), field("endTime", datetime())]);
    schemas.push((
        "AnalysisQuery",
        model(
            "AnalysisQuery",
            analysis,
            &[
                "dataProductId",
                "location",
                "variables",
                "startTime",
                "endTime",
            ],
        ),
    ));
    for (name, property) in [
        ("CanonicalLink", "canonical"),
        ("CatalogLink", "catalog"),
        ("ErrorDetail", "detail"),
    ] {
        schemas.push((
            name,
            model(name, vec![field(property, kind("string"))], &[property]),
        ));
    }
    let coordinates = || {
        vec![
            field("latitude", kind("number")),
            field("longitude", kind("number")),
        ]
    };
    schemas.push((
        "Coordinate",
        model("Coordinate", coordinates(), &["latitude", "longitude"]),
    ));
    schemas.push((
        "DataProductReference",
        model(
            "DataProductReference",
            vec![
                field("id", kind("string")),
                field("version", nullable(kind("string"))),
            ],
            &["id", "version"],
        ),
    ));
    for (name, ensemble) in [
        ("DeterministicAnalysisResult", false),
        ("EnsembleAnalysisResult", true),
    ] {
        let mut fields = result_fields(false);
        let last = if ensemble { "ensembleMembers" } else { "data" };
        fields.push(field(
            last,
            if ensemble {
                array(reference("EnsembleMember"))
            } else {
                series_data()
            },
        ));
        schemas.push((
            name,
            model(
                name,
                fields,
                &[
                    "dataProduct",
                    "point",
                    "analysisTimes",
                    "variables",
                    "snapshotId",
                    "links",
                    last,
                ],
            ),
        ));
    }
    for (name, ensemble) in [
        ("DeterministicForecastSeries", false),
        ("EnsembleForecastSeries", true),
    ] {
        let last = if ensemble { "ensembleMembers" } else { "data" };
        let fields = vec![
            field("initTime", datetime()),
            field("validTimes", array(datetime())),
            field("leadTimeHours", array(kind("number"))),
            ("links", reference("CanonicalLink")),
            field(
                last,
                if ensemble {
                    array(reference("EnsembleMember"))
                } else {
                    series_data()
                },
            ),
        ];
        schemas.push((
            name,
            model(
                name,
                fields,
                &["initTime", "validTimes", "leadTimeHours", "links", last],
            ),
        ));
    }
    schemas.push((
        "EnsembleMember",
        model(
            "EnsembleMember",
            vec![
                field(
                    "member",
                    union([kind("string"), kind("integer"), kind("number")]),
                ),
                field("data", series_data()),
            ],
            &["member", "data"],
        ),
    ));
    let mut forecast = query_fields();
    forecast.push(field(
        "initTime",
        union([
            datetime(),
            literal(contract::LATEST_INIT_TIME),
            kind("null"),
        ]),
    ));
    for name in [
        "initTimesAfter",
        "initTimesBefore",
        "validTimeStart",
        "validTimeEnd",
    ] {
        forecast.push(field(name, nullable(datetime())));
    }
    forecast.push(field(
        "maxLeadTimeHours",
        nullable(add(
            kind("integer"),
            [("minimum", V::Float(contract::MIN_LEAD_TIME_HOURS as f64))],
        )),
    ));
    forecast.push((
        "representation",
        add(
            enumeration(&contract::Representation::ALL.map(contract::Representation::as_str)),
            [
                ("title", V::from("Representation")),
                (
                    "default",
                    V::from(contract::Representation::default().as_str()),
                ),
            ],
        ),
    ));
    schemas.push((
        "ForecastQuery",
        model(
            "ForecastQuery",
            forecast,
            &["dataProductId", "location", "variables"],
        ),
    ));
    let mut forecast_result = result_fields(true);
    forecast_result.push(("routing", add(nullable(reference("Routing")), [("description", V::from("Present only when the requested data product has a reviewed equivalent representation; dataProduct, snapshotId and links name the product served."))])));
    schemas.push((
        "ForecastResult",
        model(
            "ForecastResult",
            forecast_result,
            &[
                "dataProduct",
                "point",
                "forecasts",
                "variables",
                "snapshotId",
                "links",
            ],
        ),
    ));
    schemas.push((
        "HTTPValidationError",
        V::object([
            (
                "properties",
                object([field("detail", array(reference("ValidationError")))]),
            ),
            ("type", V::from("object")),
            ("title", V::from("HTTPValidationError")),
        ]),
    ));
    let location = [
        ("latitude", contract::LATITUDE_BOUND),
        ("longitude", contract::LONGITUDE_BOUND),
    ]
    .into_iter()
    .map(|(name, bound)| {
        field(
            name,
            add(
                kind("number"),
                [("maximum", V::Float(bound)), ("minimum", V::Float(-bound))],
            ),
        )
    })
    .collect();
    schemas.push((
        "Location",
        model("Location", location, &["latitude", "longitude"]),
    ));
    schemas.push((
        "Point",
        model(
            "Point",
            vec![
                ("requested", nullable(reference("Coordinate"))),
                ("selected", reference("SelectedPoint")),
                field("method", literal("nearest")),
            ],
            &["selected", "method"],
        ),
    ));
    schemas.push((
        "Routing",
        model(
            "Routing",
            vec![
                ("requestedDataProduct", reference("DataProductReference")),
                field("requestedSnapshotId", kind("string")),
                field(
                    "decision",
                    enumeration(&["substituted", "direct", "fallback"]),
                ),
                field("reason", kind("string")),
                field(
                    "fallbackReason",
                    nullable(enumeration(&["missing", "timeout", "throttle"])),
                ),
                ("coverage", reference("RoutingCoverage")),
                field("policyRevision", kind("string")),
            ],
            &[
                "requestedDataProduct",
                "requestedSnapshotId",
                "decision",
                "reason",
                "fallbackReason",
                "coverage",
                "policyRevision",
            ],
        ),
    ));
    schemas.push((
        "RoutingCoverage",
        model(
            "RoutingCoverage",
            vec![
                field("selectedValues", kind("integer")),
                field("finiteValues", kind("integer")),
            ],
            &["selectedValues", "finiteValues"],
        ),
    ));
    let mut selected = coordinates();
    selected.push(field("grid", dictionary(kind("integer"))));
    schemas.push((
        "SelectedPoint",
        model(
            "SelectedPoint",
            selected,
            &["latitude", "longitude", "grid"],
        ),
    ));
    schemas.push((
        "ValidationError",
        V::object([
            (
                "properties",
                object([
                    (
                        "loc",
                        titled(array(union([kind("string"), kind("integer")])), "Location"),
                    ),
                    ("msg", titled(kind("string"), "Message")),
                    ("type", titled(kind("string"), "Error Type")),
                    ("input", V::object([("title", V::from("Input"))])),
                    ("ctx", titled(kind("object"), "Context")),
                ]),
            ),
            ("type", V::from("object")),
            ("required", strings(&["loc", "msg", "type"])),
            ("title", V::from("ValidationError")),
        ]),
    ));
    schemas.push((
        "Variable",
        model(
            "Variable",
            vec![field("unit", nullable(kind("string")))],
            &["unit"],
        ),
    ));
    schemas.sort_by_key(|(name, _)| *name);
    object(schemas)
}

fn content(schema: V) -> V {
    V::object([("application/json", V::object([("schema", schema)]))])
}
fn success(schema: V) -> V {
    V::object([
        ("description", V::from("Successful Response")),
        ("content", content(schema)),
    ])
}
fn validation_error() -> V {
    V::object([
        ("description", V::from("Validation Error")),
        ("content", content(reference("HTTPValidationError"))),
    ])
}
fn retryable(description: &str, retry_description: &str, post: bool) -> V {
    V::object([
        ("description", V::from(description)),
        (
            "headers",
            V::object([(
                "Retry-After",
                V::object([
                    ("description", V::from(retry_description)),
                    (
                        "schema",
                        add(
                            kind("integer"),
                            [("minimum", if post { V::Float(1.) } else { V::from(1i64) })],
                        ),
                    ),
                ]),
            )]),
        ),
        ("content", content(reference("ErrorDetail"))),
    ])
}
fn responses(schema: V, errors: bool, data: bool, post: bool, validation: bool) -> V {
    let mut responses = vec![("200", success(schema))];
    if errors {
        responses.push((
            "429",
            retryable(crate::RATE_DETAIL, "Seconds to wait before retrying", post),
        ));
        responses.push(("503", retryable(
            if data { "Server busy, catalog unavailable, or upstream data read timed out or was throttled" } else { "Server busy or catalog unavailable" },
            if data { "Seconds to wait when server busy or upstream data timed out or was throttled; may be absent for catalog errors" } else { "Seconds to wait when server busy; may be absent for catalog errors" }, post)));
    }
    if validation {
        responses.push(("422", validation_error()));
    }
    object(responses)
}
fn parameter(name: &str, location: &str, required: bool, schema: V) -> V {
    V::object([
        ("name", V::from(name)),
        ("in", V::from(location)),
        ("required", V::Bool(required)),
        ("schema", titled(schema, &field_title(name))),
    ])
}
fn operation(
    summary: &str,
    id: &str,
    parameters: Vec<V>,
    request: Option<&str>,
    responses: V,
) -> V {
    let mut fields = vec![("summary", V::from(summary)), ("operationId", V::from(id))];
    if !parameters.is_empty() {
        fields.push(("parameters", V::Array(parameters)));
    }
    if let Some(request) = request {
        fields.push((
            "requestBody",
            V::object([
                ("content", content(reference(request))),
                ("required", V::Bool(true)),
            ]),
        ));
    }
    fields.push(("responses", responses));
    object(fields)
}
fn paths() -> V {
    let listing = titled(
        dictionary(V::Bool(true)),
        "Response List Data Products V1 Data Products Get",
    );
    let detail = titled(
        add(kind("object"), [("additionalProperties", V::Bool(true))]),
        "Response Describe Data Product V1 Data Products  Dataproductid  Get",
    );
    let runs = titled(
        add(kind("object"), [("additionalProperties", V::Bool(true))]),
        "Response List Forecast Runs V1 Forecasts Runs Get",
    );
    let run_parameters = vec![
        parameter("dataProductId", "query", true, nonempty()),
        parameter("initTimesAfter", "query", false, nullable(datetime())),
        parameter("initTimesBefore", "query", false, nullable(datetime())),
        parameter(
            "limit",
            "query",
            false,
            add(
                kind("integer"),
                [
                    ("maximum", V::from(100i64)),
                    ("minimum", V::from(1i64)),
                    ("default", V::from(20i64)),
                ],
            ),
        ),
    ];
    let mut canonical_parameters: Vec<_> = ["dataProductId", "snapshotId", "gridIndices"]
        .into_iter()
        .map(|name| parameter(name, "path", true, kind("string")))
        .collect();
    canonical_parameters.push(parameter("variables", "query", true, nonempty()));
    canonical_parameters.push(parameter("initTime", "query", false, nullable(datetime())));
    canonical_parameters.push(parameter(
        "maxLeadTimeHours",
        "query",
        false,
        nullable(add(
            kind("integer"),
            [("minimum", V::from(contract::MIN_LEAD_TIME_HOURS))],
        )),
    ));
    for name in ["validTimeStart", "validTimeEnd", "startTime", "endTime"] {
        canonical_parameters.push(parameter(name, "query", false, nullable(datetime())));
    }
    let canonical = titled(
        refs(&[
            "ForecastResult",
            "DeterministicAnalysisResult",
            "EnsembleAnalysisResult",
        ]),
        "Response Query Canonical Point V1 Data  Dataproductid   Snapshotid  Points  Gridindices  Get",
    );
    object([
        (
            "/v1/data-products",
            object([(
                "get",
                operation(
                    "List Data Products",
                    "list_data_products_v1_data_products_get",
                    vec![],
                    None,
                    responses(listing, false, false, false, false),
                ),
            )]),
        ),
        (
            "/v1/data-products/{dataProductId}",
            object([(
                "get",
                operation(
                    "Describe Data Product",
                    "describe_data_product_v1_data_products__dataProductId__get",
                    vec![parameter("dataProductId", "path", true, kind("string"))],
                    None,
                    responses(detail, false, false, false, true),
                ),
            )]),
        ),
        (
            "/v1/forecasts/runs",
            object([(
                "get",
                operation(
                    "List Forecast Runs",
                    "list_forecast_runs_v1_forecasts_runs_get",
                    run_parameters,
                    None,
                    responses(runs, true, false, false, true),
                ),
            )]),
        ),
        (
            "/v1/forecasts",
            object([(
                "post",
                operation(
                    "Query Forecasts",
                    "query_forecasts_v1_forecasts_post",
                    vec![],
                    Some("ForecastBatch"),
                    responses(reference("ForecastBatchResponse"), true, true, true, true),
                ),
            )]),
        ),
        (
            "/v1/analyses",
            object([(
                "post",
                operation(
                    "Query Analyses",
                    "query_analyses_v1_analyses_post",
                    vec![],
                    Some("AnalysisBatch"),
                    responses(reference("AnalysisBatchResponse"), true, true, true, true),
                ),
            )]),
        ),
        (
            "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}",
            object([(
                "get",
                operation(
                    "Query Canonical Point",
                    "query_canonical_point_v1_data__dataProductId___snapshotId__points__gridIndices__get",
                    canonical_parameters,
                    None,
                    responses(canonical, true, true, false, true),
                ),
            )]),
        ),
    ])
}
fn document() -> V {
    V::object([
        ("openapi", V::from("3.1.0")),
        (
            "info",
            V::object([
                ("title", V::from("Dynamical API")),
                (
                    "description",
                    V::from("Catalog-backed point access to dynamical.org weather data."),
                ),
                ("version", V::from("0.1.0")),
            ]),
        ),
        ("paths", paths()),
        ("components", V::object([("schemas", schemas())])),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynapi_contract::{ValidationResult, validate_forecast};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn spec() -> Value {
        serde_json::from_slice(bytes()).unwrap()
    }
    fn query() -> Value {
        json!({"dataProductId":"example-product", "location":{"latitude":0.0,"longitude":0.0}, "variables":["temperature"]})
    }
    fn validate(query: Value) -> ValidationResult {
        validate_forecast(
            &serde_json::to_vec(&json!({"queries":[query]})).unwrap(),
            Some("application/json"),
        )
    }
    fn accepted(query: Value) -> dynapi_contract::ForecastBatch {
        let ValidationResult::Valid(batch) = validate(query) else {
            panic!("expected valid fixture")
        };
        batch
    }
    fn rejected(query: Value) {
        assert!(matches!(validate(query), ValidationResult::Invalid(_)));
    }

    #[test]
    fn router_and_document_inventory_agree_both_ways_with_explicit_exclusions() {
        let document = spec();
        let documented: BTreeSet<_> = document["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, methods)| {
                methods
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|method| (path.clone(), method.to_ascii_uppercase()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let served: BTreeSet<_> = crate::ROUTES
            .iter()
            .flat_map(|route| {
                route
                    .methods
                    .iter()
                    .copied()
                    .chain(route.cors.then_some("OPTIONS"))
                    .map(|method| (route.template.to_owned(), method.to_owned()))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Infrastructure/docs and explicit CORS responders intentionally stay
        // outside the public data OpenAPI schema, matching its published scope.
        let mut excluded = BTreeSet::new();
        for (path, methods) in [
            ("/", &["GET", "HEAD"][..]),
            ("/robots.txt", &["GET", "HEAD"][..]),
            ("/healthz", &["GET"][..]),
            ("/openapi.json", &["GET", "HEAD"][..]),
            ("/docs", &["GET", "HEAD"][..]),
            ("/docs/oauth2-redirect", &["GET", "HEAD"][..]),
            ("/redoc", &["GET", "HEAD"][..]),
            ("/v1/data-products", &["OPTIONS"][..]),
            ("/v1/data-products/{dataProductId}", &["OPTIONS"][..]),
            ("/v1/forecasts/runs", &["OPTIONS"][..]),
            ("/v1/forecasts", &["OPTIONS"][..]),
            ("/v1/analyses", &["OPTIONS"][..]),
            (
                "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}",
                &["OPTIONS"][..],
            ),
        ] {
            for method in methods {
                excluded.insert((path.to_owned(), (*method).to_owned()));
            }
        }
        assert!(documented.is_disjoint(&excluded));
        assert_eq!(served, documented.union(&excluded).cloned().collect());
        // These operations are documented now and implemented by the execution
        // streams. Keep the pending surface explicit until their handlers land.
        for operation in [
            ("/v1/forecasts", "POST"),
            ("/v1/analyses", "POST"),
            ("/v1/forecasts/runs", "GET"),
            (
                "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}",
                "GET",
            ),
        ] {
            assert!(documented.contains(&(operation.0.to_owned(), operation.1.to_owned())));
        }
    }

    #[test]
    fn every_schema_reference_resolves_and_required_properties_exist() {
        fn visit(value: &Value, document: &Value) {
            match value {
                Value::Object(fields) => {
                    if let Some(reference) = fields.get("$ref") {
                        let path = reference.as_str().unwrap().strip_prefix('#').unwrap();
                        assert!(document.pointer(path).is_some(), "unresolved {reference}");
                    }
                    if let Some(required) = fields.get("required").and_then(Value::as_array) {
                        for name in required {
                            assert!(
                                fields["properties"].get(name.as_str().unwrap()).is_some(),
                                "unknown required {name}"
                            );
                        }
                    }
                    for child in fields.values() {
                        visit(child, document);
                    }
                }
                Value::Array(values) => {
                    for child in values {
                        visit(child, document);
                    }
                }
                _ => {}
            }
        }
        let document = spec();
        visit(&document, &document);
        let schemas = document["components"]["schemas"].as_object().unwrap();
        assert_eq!(
            schemas.keys().collect::<Vec<_>>(),
            schemas
                .keys()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
        for (name, schema) in schemas {
            assert_eq!(schema["title"], *name);
            if !["HTTPValidationError", "ValidationError"].contains(&name.as_str()) {
                assert_eq!(schema["additionalProperties"], false);
            }
        }
        assert!(std::ptr::eq(bytes().as_ptr(), bytes().as_ptr()));
        assert!(
            std::str::from_utf8(bytes())
                .unwrap()
                .starts_with("{\"openapi\":\"3.1.0\",\"info\":{\"title\":\"Dynamical API\",")
        );
    }

    #[test]
    fn forecast_property_aliases_defaults_and_required_fields_agree_with_validation() {
        let document = spec();
        let schema = &document["components"]["schemas"]["ForecastQuery"];
        let normalized = accepted(query()).queries[0].normalized();
        let V::Object(fields) = normalized else {
            unreachable!()
        };
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            fields.into_iter().map(|(name, _)| name).collect()
        );
        for required in schema["required"].as_array().unwrap() {
            let mut missing = query();
            missing
                .as_object_mut()
                .unwrap()
                .remove(required.as_str().unwrap());
            rejected(missing);
        }
        let batch = accepted(query());
        assert_eq!(
            batch.queries[0].representation.as_str(),
            schema["properties"]["representation"]["default"]
        );
        assert_eq!(batch.queries[0].init_time, None);
        assert_eq!(batch.queries[0].query_id, None);
        for (alias, snake, value) in [
            ("queryId", "query_id", json!("example-query")),
            (
                "dataProductId",
                "data_product_id",
                json!("example-other-product"),
            ),
            (
                "initTime",
                "init_time",
                schema["properties"]["initTime"]["anyOf"][1]["const"].clone(),
            ),
            ("maxLeadTimeHours", "max_lead_time_hours", json!(6)),
        ] {
            for key in [alias, snake] {
                let mut input = query();
                input.as_object_mut().unwrap().remove(alias);
                input[key] = value.clone();
                let batch = accepted(input);
                let bytes = output::json(&batch.queries[0].normalized(), false).unwrap();
                let normalized: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(normalized[alias], value, "{key}");
            }
        }
        for (after, before, snake_after, snake_before) in [
            (
                "initTimesAfter",
                "initTimesBefore",
                "init_times_after",
                "init_times_before",
            ),
            (
                "validTimeStart",
                "validTimeEnd",
                "valid_time_start",
                "valid_time_end",
            ),
        ] {
            for (left, right) in [(after, before), (snake_after, snake_before)] {
                let mut input = query();
                input[left] = json!("2026-01-01T00:00:00Z");
                input[right] = json!("2026-01-02T00:00:00Z");
                let normalized: Value = serde_json::from_slice(
                    &output::json(&accepted(input).queries[0].normalized(), false).unwrap(),
                )
                .unwrap();
                assert_eq!(normalized[after], "2026-01-01T00:00:00Z");
                assert_eq!(normalized[before], "2026-01-02T00:00:00Z");
            }
        }
        let mut extra = query();
        extra["unexpected"] = json!(true);
        rejected(extra);
    }

    #[test]
    fn shared_request_bounds_and_literals_are_enforced_at_the_schema_boundaries() {
        let document = spec();
        let schemas = &document["components"]["schemas"];
        let properties = &schemas["ForecastQuery"]["properties"];
        let max_queries = schemas["ForecastBatch"]["properties"]["queries"]["maxItems"]
            .as_u64()
            .unwrap() as usize;
        let min_queries = schemas["ForecastBatch"]["properties"]["queries"]["minItems"]
            .as_u64()
            .unwrap() as usize;
        for (count, valid) in [
            (min_queries - 1, false),
            (min_queries, true),
            (max_queries, true),
            (max_queries + 1, false),
        ] {
            let result = validate_forecast(
                &serde_json::to_vec(&json!({"queries":vec![query(); count]})).unwrap(),
                Some("application/json"),
            );
            assert_eq!(
                matches!(result, ValidationResult::Valid(_)),
                valid,
                "queries={count}"
            );
        }
        for name in ["AnalysisBatch", "ForecastBatch"] {
            assert_eq!(
                schemas[name]["properties"]["queries"]["maxItems"],
                max_queries
            );
        }
        let max_variables = properties["variables"]["maxItems"].as_u64().unwrap() as usize;
        let min_variables = properties["variables"]["minItems"].as_u64().unwrap() as usize;
        for (count, valid) in [
            (min_variables - 1, false),
            (min_variables, true),
            (max_variables, true),
            (max_variables + 1, false),
        ] {
            let mut input = query();
            input["variables"] = json!(vec!["temperature"; count]);
            assert_eq!(matches!(validate(input), ValidationResult::Valid(_)), valid);
        }
        for (name, upper_key, lower_key) in [
            ("queryId", "maxLength", "minLength"),
            ("dataProductId", "maxLength", "minLength"),
        ] {
            let constraints = if name == "queryId" {
                &properties[name]["anyOf"][0]
            } else {
                &properties[name]
            };
            let min = constraints[lower_key].as_u64().unwrap() as usize;
            for (count, valid) in [(min - 1, false), (min, true)] {
                let mut input = query();
                input[name] = json!("é".repeat(count));
                assert_eq!(matches!(validate(input), ValidationResult::Valid(_)), valid);
            }
            if let Some(max) = constraints[upper_key].as_u64() {
                for (count, valid) in [(max as usize, true), (max as usize + 1, false)] {
                    let mut input = query();
                    input[name] = json!("é".repeat(count));
                    assert_eq!(matches!(validate(input), ValidationResult::Valid(_)), valid);
                }
            }
        }
        for name in ["latitude", "longitude"] {
            let schema = &schemas["Location"]["properties"][name];
            for bound in ["minimum", "maximum"] {
                let value = schema[bound].as_f64().unwrap();
                let mut input = query();
                input["location"][name] = json!(value);
                accepted(input.clone());
                input["location"][name] = json!(value + value.signum());
                rejected(input);
            }
        }
        let minimum = properties["maxLeadTimeHours"]["anyOf"][0]["minimum"]
            .as_f64()
            .unwrap();
        let mut input = query();
        input["maxLeadTimeHours"] = json!(minimum);
        accepted(input.clone());
        input["maxLeadTimeHours"] = json!(minimum - 1.);
        rejected(input);
        for representation in properties["representation"]["enum"].as_array().unwrap() {
            let mut input = query();
            input["representation"] = representation.clone();
            assert_eq!(
                accepted(input).queries[0].representation.as_str(),
                representation
            );
        }
        let mut input = query();
        input["representation"] = json!("unsupported");
        rejected(input);
        let mut input = query();
        input["initTime"] = properties["initTime"]["anyOf"][1]["const"].clone();
        assert!(matches!(
            accepted(input).queries[0].init_time,
            Some(contract::InitTime::Latest)
        ));
    }

    #[test]
    fn response_schemas_include_unions_nullability_routing_and_retry_headers() {
        let document = spec();
        let schemas = &document["components"]["schemas"];
        assert_eq!(
            schemas["ForecastQuery"]["properties"]["representation"]["enum"],
            json!(["auto", "requested"])
        );
        assert_eq!(
            schemas["ForecastResult"]["properties"]["forecasts"]["items"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            schemas["AnalysisBatchResponse"]["properties"]["results"]["items"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            schemas["ForecastResult"]["properties"]["routing"]["anyOf"][0]["$ref"],
            "#/components/schemas/Routing"
        );
        assert!(
            !schemas["ForecastResult"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("routing"))
        );
        assert_eq!(
            schemas["Routing"]["properties"]["decision"]["enum"],
            json!(["substituted", "direct", "fallback"])
        );
        assert_eq!(
            schemas["Routing"]["properties"]["fallbackReason"]["anyOf"][0]["enum"],
            json!(["missing", "timeout", "throttle"])
        );
        assert_eq!(
            schemas["Variable"]["properties"]["unit"]["anyOf"][1]["type"],
            "null"
        );
        for (path, method) in [
            ("/v1/forecasts", "post"),
            ("/v1/analyses", "post"),
            ("/v1/forecasts/runs", "get"),
            (
                "/v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}",
                "get",
            ),
        ] {
            for status in ["429", "503"] {
                let response = &document["paths"][path][method]["responses"][status];
                assert_eq!(
                    response["content"]["application/json"]["schema"]["$ref"],
                    "#/components/schemas/ErrorDetail"
                );
                assert_eq!(
                    response["headers"]["Retry-After"]["schema"]["minimum"].as_f64(),
                    Some(1.)
                );
            }
        }
    }
}
