//! Catalog metadata presentation; refresh and storage policy live in dynapi-catalog.
use crate::{HttpResponse, detail, response, server_error};
use dynapi_catalog::{Collection, native::StacCatalog};
use dynapi_contract::{Value, output};

pub(crate) async fn list(catalog: &StacCatalog) -> HttpResponse {
    match catalog.list_collections().await {
        Ok(products) => encode(Value::object([(
            "dataProducts",
            Value::Array(
                products
                    .iter()
                    .filter(|p| visible(p))
                    .map(summary)
                    .collect(),
            ),
        )])),
        Err(error) => detail(503, &error.to_string()),
    }
}

pub(crate) async fn describe(catalog: &StacCatalog, id: &str) -> HttpResponse {
    match catalog.get_collection(id).await {
        Ok(Some(product)) if visible(&product) => encode(product_detail(&product)),
        Ok(_) => response(404, output::product_not_found(id), Some("application/json")),
        Err(error) => detail(503, &error.to_string()),
    }
}

fn visible(product: &Collection) -> bool {
    matches!(product.kind(), "forecast" | "analysis")
}

fn optional(value: &Option<String>) -> Value {
    value.as_deref().map(Value::from).unwrap_or(Value::Null)
}

fn summary(product: &Collection) -> Value {
    let capability = match (product.point_query_reason(), product.kind()) {
        (None, "forecast") => Some("forecastTimeSeries"),
        (None, "analysis") => Some("analysisTimeSeries"),
        _ => None,
    };
    Value::object([
        ("id", Value::from(product.id.clone())),
        ("name", Value::from(product.title.clone())),
        ("description", Value::from(product.description.clone())),
        ("type", Value::from(product.kind())),
        ("modelId", optional(&product.model_id)),
        ("modelName", optional(&product.model_name)),
        ("version", Value::from(product.version.clone())),
        ("variableCount", Value::from(product.variables.len())),
        (
            "capabilities",
            Value::Array(capability.into_iter().map(Value::from).collect()),
        ),
        (
            "links",
            Value::object([
                (
                    "self",
                    Value::from(format!("/v1/data-products/{}", product.id)),
                ),
                ("catalog", optional(&product.catalog_url)),
            ]),
        ),
    ])
}

fn product_detail(product: &Collection) -> Value {
    let Value::Object(mut fields) = summary(product) else {
        unreachable!()
    };
    fields.extend([
        ("license".into(), Value::from(product.license.clone())),
        (
            "boundingBox".into(),
            product
                .bbox
                .map(|bbox| Value::Array(bbox.into_iter().map(Value::Float).collect()))
                .unwrap_or(Value::Null),
        ),
        (
            "dimensions".into(),
            Value::Object(
                product
                    .dimensions
                    .iter()
                    .map(|(k, v)| (k.clone(), metadata(v)))
                    .collect(),
            ),
        ),
        (
            "variables".into(),
            Value::Object(
                product
                    .variables
                    .iter()
                    .map(|(name, variable)| {
                        (
                            name.clone(),
                            Value::object([
                                (
                                    "dimensions",
                                    Value::Array(
                                        variable
                                            .dimensions
                                            .iter()
                                            .cloned()
                                            .map(Value::from)
                                            .collect(),
                                    ),
                                ),
                                ("unit", optional(&variable.unit)),
                                ("longName", optional(&variable.long_name)),
                                ("standardName", optional(&variable.standard_name)),
                            ]),
                        )
                    })
                    .collect(),
            ),
        ),
    ]);
    Value::Object(fields)
}

/// Preserve integer/float identity and insertion order from the catalog parser.
/// Bounding-box coercion can produce non-finite floats; the dict serializer
/// maps those to null, just like Pydantic's default JSON policy.
fn metadata(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(value) => {
            let text = value.to_string();
            if text.contains(['.', 'e', 'E']) {
                Value::Float(text.parse().unwrap_or(f64::NAN))
            } else {
                Value::Int(text.parse().expect("catalog integer"))
            }
        }
        serde_json::Value::String(value) => Value::from(value.clone()),
        serde_json::Value::Array(values) => Value::Array(values.iter().map(metadata).collect()),
        serde_json::Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(k, v)| (k.clone(), metadata(v)))
                .collect(),
        ),
    }
}

fn encode(value: Value) -> HttpResponse {
    match wire_value(value) {
        Some(value) => response(
            200,
            serde_json::to_vec(&value).unwrap(),
            Some("application/json"),
        ),
        None => server_error(),
    }
}

/// Dict responses use Pydantic's Ryū float layout rather than the Python JSON
/// layout used by explicit JSONResponse endpoints. Keep decimal 1e-5, unpadded
/// negative exponents, explicit positive exponent signs and signed zero.
fn wire_value(value: Value) -> Option<serde_json::Value> {
    use serde_json::Value as Json;
    Some(match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(value),
        Value::String(value) => Json::String(value),
        Value::Int(value) => Json::Number(value.to_string().parse().ok()?),
        Value::Float(value) => {
            if !value.is_finite() {
                return Some(Json::Null);
            }
            let mut buffer = ryu::Buffer::new();
            let raw = buffer.format_finite(value);
            let text = match raw.split_once('e') {
                Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                    format!("{mantissa}e+{exponent}")
                }
                _ => raw.to_owned(),
            };
            Json::Number(text.parse().ok()?)
        }
        Value::Array(values) => {
            Json::Array(values.into_iter().map(wire_value).collect::<Option<_>>()?)
        }
        Value::Object(values) => Json::Object(
            values
                .into_iter()
                .map(|(key, value)| Some((key, wire_value(value)?)))
                .collect::<Option<_>>()?,
        ),
    })
}
