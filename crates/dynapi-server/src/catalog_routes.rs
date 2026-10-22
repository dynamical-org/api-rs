//! Catalog metadata presentation; refresh and storage policy live in dynapi-catalog.
use crate::{HttpResponse, detail, response, server_error};
use dynapi_catalog::{Collection, native::StacCatalog, refresh::Snapshot};
use std::io::{self, Write};

const MAX_CATALOG_RESPONSE_BYTES: usize = 1024 * 1024;
#[derive(Clone, Debug)]
pub(crate) struct ResponseTooLarge;

pub(crate) async fn list(catalog: &StacCatalog) -> HttpResponse {
    match catalog.snapshot().await {
        Ok(snapshot) => encode(Projection::List(&snapshot)),
        Err(error) => unavailable(&error.to_string()),
    }
}
pub(crate) async fn describe(catalog: &StacCatalog, id: &str) -> HttpResponse {
    match catalog.snapshot().await {
        Ok(snapshot) => match snapshot
            .collections
            .get(id)
            .filter(|product| visible(product))
        {
            Some(product) => encode(Projection::Detail(product)),
            None => response(
                404,
                dynapi_contract::output::product_not_found(id),
                Some("application/json"),
            ),
        },
        Err(error) => unavailable(&error.to_string()),
    }
}
fn unavailable(message: &str) -> HttpResponse {
    let mut response = detail(503, message);
    response
        .extensions_mut()
        .insert(crate::telemetry::ErrorKind::CatalogUnavailable);
    response
}
fn visible(product: &Collection) -> bool {
    matches!(product.kind(), "forecast" | "analysis")
}

/// Both passes borrow the same immutable published generation. No Collection,
/// metadata string, or intermediate response tree is cloned per request.
#[derive(Clone, Copy)]
enum Projection<'a> {
    List(&'a Snapshot),
    Detail(&'a Collection),
}
impl Projection<'_> {
    fn write(self, output: &mut dyn Write) -> io::Result<()> {
        let mut json = Json { output };
        match self {
            Self::Detail(product) => json.product(product, true),
            Self::List(snapshot) => {
                json.raw(br#"{"dataProducts":["#)?;
                for (index, product) in snapshot
                    .collections
                    .values()
                    .filter(|product| visible(product))
                    .enumerate()
                {
                    if index != 0 {
                        json.raw(b",")?;
                    }
                    json.product(product, false)?;
                }
                json.raw(b"]}")
            }
        }
    }
}

struct Bounded<W> {
    output: W,
    written: usize,
    limit: usize,
    exceeded: bool,
}
impl<W> Bounded<W> {
    fn new(output: W, limit: usize) -> Self {
        Self {
            output,
            written: 0,
            limit,
            exceeded: false,
        }
    }
}
impl<W: Write> Write for Bounded<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.written {
            self.exceeded = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let written = self.output.write(bytes)?;
        self.written += written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn encode(projection: Projection<'_>) -> HttpResponse {
    encode_with_limit(projection, MAX_CATALOG_RESPONSE_BYTES)
}
fn encode_with_limit(projection: Projection<'_>, limit: usize) -> HttpResponse {
    // Count escaped UTF-8, punctuation and formatted numbers before reserving
    // any body storage. An aggregate list obeys the same budget as a detail.
    let mut count = Bounded::new(io::sink(), limit);
    if projection.write(&mut count).is_err() {
        return encoding_error(count.exceeded);
    }
    let mut body = Bounded::new(Vec::with_capacity(count.written), count.written);
    if projection.write(&mut body).is_err() {
        return encoding_error(body.exceeded);
    }
    response(200, body.output, Some("application/json"))
}
fn encoding_error(too_large: bool) -> HttpResponse {
    let mut result = server_error();
    if too_large {
        result.extensions_mut().insert(ResponseTooLarge);
    }
    result
}

struct Json<'a> {
    output: &'a mut dyn Write,
}
impl Json<'_> {
    fn raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)
    }
    fn string(&mut self, value: &str) -> io::Result<()> {
        serde_json::to_writer(&mut self.output, value).map_err(io::Error::other)
    }
    fn optional(&mut self, value: Option<&str>) -> io::Result<()> {
        match value {
            Some(value) => self.string(value),
            None => self.raw(b"null"),
        }
    }
    fn key(&mut self, name: &str, first: bool) -> io::Result<()> {
        if !first {
            self.raw(b",")?;
        }
        self.string(name)?;
        self.raw(b":")
    }
    fn product(&mut self, product: &Collection, detail: bool) -> io::Result<()> {
        self.raw(b"{")?;
        for (index, (name, value)) in [
            ("id", Some(product.id.as_str())),
            ("name", Some(product.title.as_str())),
            ("description", Some(product.description.as_str())),
            ("type", Some(product.kind())),
            ("modelId", product.model_id.as_deref()),
            ("modelName", product.model_name.as_deref()),
            ("version", Some(product.version.as_str())),
        ]
        .into_iter()
        .enumerate()
        {
            self.key(name, index == 0)?;
            self.optional(value)?;
        }
        self.key("variableCount", false)?;
        write!(self.output, "{}", product.variables.len())?;
        self.key("capabilities", false)?;
        self.raw(b"[")?;
        match (product.point_query_reason(), product.kind()) {
            (None, "forecast") => self.string("forecastTimeSeries")?,
            (None, "analysis") => self.string("analysisTimeSeries")?,
            _ => {}
        }
        self.raw(b"]")?;
        self.key("links", false)?;
        self.raw(br#"{"self":"/v1/data-products/"#)?;
        // Escape the ID as a string without allocating the concatenated URL.
        // The opening quote and prefix have already been written.
        let mut suffix = StripOpeningQuote {
            output: self.output,
            first: true,
        };
        serde_json::to_writer(&mut suffix, product.id.as_str()).map_err(io::Error::other)?;
        self.key("catalog", false)?;
        self.optional(product.catalog_url.as_deref())?;
        self.raw(b"}")?;
        if detail {
            self.key("license", false)?;
            self.string(&product.license)?;
            self.key("boundingBox", false)?;
            if let Some(bbox) = product.bbox {
                self.raw(b"[")?;
                for (index, value) in bbox.into_iter().enumerate() {
                    if index != 0 {
                        self.raw(b",")?;
                    }
                    self.float(value)?;
                }
                self.raw(b"]")?;
            } else {
                self.raw(b"null")?;
            }
            self.key("dimensions", false)?;
            self.raw(b"{")?;
            for (index, (name, value)) in product.dimensions.iter().enumerate() {
                self.key(name, index == 0)?;
                self.metadata(value)?;
            }
            self.raw(b"}")?;
            self.key("variables", false)?;
            self.raw(b"{")?;
            for (index, (name, variable)) in product.variables.iter().enumerate() {
                self.key(name, index == 0)?;
                self.raw(br#"{"dimensions":["#)?;
                for (index, dimension) in variable.dimensions.iter().enumerate() {
                    if index != 0 {
                        self.raw(b",")?;
                    }
                    self.string(dimension)?;
                }
                self.raw(b"]")?;
                for (name, value) in [
                    ("unit", variable.unit.as_deref()),
                    ("longName", variable.long_name.as_deref()),
                    ("standardName", variable.standard_name.as_deref()),
                ] {
                    self.key(name, false)?;
                    self.optional(value)?;
                }
                self.raw(b"}")?;
            }
            self.raw(b"}")?;
        }
        self.raw(b"}")
    }
    fn metadata(&mut self, value: &serde_json::Value) -> io::Result<()> {
        use serde_json::Value;
        match value {
            Value::Null => self.raw(b"null"),
            Value::Bool(value) => self.raw(if *value { b"true" } else { b"false" }),
            Value::String(value) => self.string(value),
            Value::Number(value) => {
                if value.is_f64() {
                    self.float(value.as_f64().unwrap())
                } else {
                    write!(self.output, "{value}")
                }
            }
            Value::Array(values) => {
                self.raw(b"[")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        self.raw(b",")?;
                    }
                    self.metadata(value)?;
                }
                self.raw(b"]")
            }
            Value::Object(values) => {
                self.raw(b"{")?;
                for (index, (name, value)) in values.iter().enumerate() {
                    self.key(name, index == 0)?;
                    self.metadata(value)?;
                }
                self.raw(b"}")
            }
        }
    }
    /// Preserve Pydantic's Ryū layout, explicit positive exponent sign and null
    /// for non-finite bounding-box coercions, without allocating number strings.
    fn float(&mut self, value: f64) -> io::Result<()> {
        if !value.is_finite() {
            return self.raw(b"null");
        }
        let mut buffer = ryu::Buffer::new();
        let raw = buffer.format_finite(value);
        match raw.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                self.raw(mantissa.as_bytes())?;
                self.raw(b"e+")?;
                self.raw(exponent.as_bytes())
            }
            _ => self.raw(raw.as_bytes()),
        }
    }
}

/// Join a fixed JSON-string prefix with serde's escaped ID bytes, without an
/// owned combined string. serde writes the opening quote in its own chunk.
struct StripOpeningQuote<'a> {
    output: &'a mut dyn Write,
    first: bool,
}
impl Write for StripOpeningQuote<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.first && !bytes.is_empty() {
            debug_assert_eq!(bytes[0], b'"');
            self.output.write_all(&bytes[1..])?;
            self.first = false;
        } else {
            self.output.write_all(bytes)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    fn snapshot() -> Snapshot {
        let fixture =
            dynapi_catalog::parse_document(include_bytes!("../tests/fixtures/catalog.json"))
                .unwrap();
        Snapshot {
            collections: fixture["collections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| {
                    let product = Collection::from_stac(value).unwrap();
                    (product.id.clone(), product)
                })
                .collect(),
            loaded_at: 0.,
            loaded_at_utc: "2026-01-01T00:00:00Z".into(),
            origin: "live",
        }
    }

    #[tokio::test]
    async fn response_budget_accepts_exact_limit_and_rejects_one_extra_encoded_byte() {
        let mut snapshot = snapshot();
        let product = snapshot.collections.get_mut("z-example-forecast").unwrap();
        product.description.clear();
        let overhead = encode(Projection::Detail(product))
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len();
        product.description = "x".repeat(MAX_CATALOG_RESPONSE_BYTES - overhead);
        let response = encode(Projection::Detail(product));
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), MAX_CATALOG_RESPONSE_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value["description"].as_str().unwrap().len(),
            MAX_CATALOG_RESPONSE_BYTES - overhead
        );
        product.description.push('x');
        let response = encode(Projection::Detail(product));
        assert_eq!(response.status(), 500);
        assert!(response.extensions().get::<ResponseTooLarge>().is_some());
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"Internal Server Error".as_slice()
        );
    }

    #[tokio::test]
    async fn preflight_counts_json_escaping_and_borrowed_link_id_bytes() {
        let mut snapshot = snapshot();
        let product = snapshot.collections.get_mut("z-example-forecast").unwrap();
        product.id = "example-\"\\\n☀".into();
        product.description = "\0é\\\"\n".into();
        let bytes = encode(Projection::Detail(product))
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["description"], "\0é\\\"\n");
        assert_eq!(value["links"]["self"], "/v1/data-products/example-\"\\\n☀");
        for limit in [bytes.len(), bytes.len() + 1] {
            let response = encode_with_limit(Projection::Detail(product), limit);
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                bytes
            );
        }
        assert_eq!(
            encode_with_limit(Projection::Detail(product), bytes.len() - 1).status(),
            500
        );
        product.description = "\0".repeat(MAX_CATALOG_RESPONSE_BYTES / 2);
        assert_eq!(encode(Projection::Detail(product)).status(), 500);
    }

    #[test]
    fn aggregate_list_is_bounded_and_hidden_metadata_is_not_projected() {
        let mut snapshot = snapshot();
        for product in snapshot
            .collections
            .values_mut()
            .filter(|product| visible(product))
        {
            product.description = "x".repeat(MAX_CATALOG_RESPONSE_BYTES / 2);
            assert_eq!(encode(Projection::Detail(product)).status(), 200);
        }
        let response = encode(Projection::List(&snapshot));
        assert_eq!(response.status(), 500);
        assert!(response.extensions().get::<ResponseTooLarge>().is_some());
        for product in snapshot
            .collections
            .values_mut()
            .filter(|product| visible(product))
        {
            product.description.clear();
        }
        snapshot
            .collections
            .get_mut("example-hidden")
            .unwrap()
            .description = "x".repeat(MAX_CATALOG_RESPONSE_BYTES * 2);
        assert_eq!(encode(Projection::List(&snapshot)).status(), 200);
    }
}
