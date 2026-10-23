use crate::CatalogError;
use dynapi_contract::output::float_repr;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub dimensions: Vec<String>,
    pub unit: Option<String>,
    pub long_name: Option<String>,
    pub standard_name: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IcechunkLocation {
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IcechunkHttpLocation {
    pub url: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RepositoryLocation {
    S3(IcechunkLocation),
    Http(IcechunkHttpLocation),
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VirtualChunkContainer {
    pub url_prefix: String,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub title: String,
    pub description: String,
    pub version: String,
    pub license: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub catalog_url: Option<String>,
    #[serde(serialize_with = "serialize_bbox")]
    pub bbox: Option<[f64; 4]>,
    pub dimensions: IndexMap<String, Value>,
    pub variables: IndexMap<String, Variable>,
    pub icechunk: RepositoryLocation,
    pub repository_config: Value,
}
fn serialize_bbox<S: serde::Serializer>(
    bbox: &Option<[f64; 4]>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    bbox.map(|values| {
        values.map(|value| crate::document::float_number(value).map_or(Value::Null, Value::Number))
    })
    .serialize(serializer)
}
pub(crate) use dynapi_contract::output::python_repr;
/// Python `str()` of a JSON scalar. Containers are outside the STAC field
/// contract (DEVIATIONS.md D11): Python would use their `repr`.
fn py_str(value: &Value) -> Option<String> {
    Some(match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => i.to_string(),
            (_, Some(u)) => u.to_string(),
            _ => {
                let mut text = String::new();
                float_repr(n.as_f64().unwrap_or(f64::NAN), &mut text);
                text
            }
        },
        Value::Array(_) | Value::Object(_) => return None,
    })
}
fn unsupported(name: &str, value: &Value) -> CatalogError {
    CatalogError::typed(
        format!(
            "STAC field '{name}' has unsupported type '{}'",
            python_type(value)
        ),
        "TypeError",
    )
}
fn text(value: &Value, name: &str, default: &str) -> Result<String, CatalogError> {
    value.get(name).map_or(Ok(default.into()), |v| {
        py_str(v).ok_or_else(|| unsupported(name, v))
    })
}
/// Fields Python keeps raw: only strings and null are within the contract.
fn optional_text(value: &Value, name: &str) -> Result<Option<String>, CatalogError> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(unsupported(name, v)),
    }
}
fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
    }
}
fn attribute_error(value: &Value, method: &str) -> CatalogError {
    CatalogError::typed(
        format!(
            "'{}' object has no attribute '{method}'",
            python_type(value)
        ),
        "AttributeError",
    )
}
fn not_iterable(value: &Value) -> CatalogError {
    CatalogError::typed(
        format!("'{}' object is not iterable", python_type(value)),
        "TypeError",
    )
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
/// Python `float(text)` whitespace and ASCII decimal/inf/nan spellings.
/// Underscores and non-ASCII digits are outside the contract and are rejected.
fn py_float(text: &str) -> Result<f64, CatalogError> {
    let trimmed = text.trim();
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|_| trimmed.is_ascii() && !trimmed.contains('_'))
        .ok_or_else(|| {
            CatalogError::typed(
                format!("could not convert string to float: {}", python_repr(text)),
                "ValueError",
            )
        })
}
/// Matches Python's anchored regex on the original string, before URL normalization.
pub fn on_dynamical_https_host(value: &str) -> bool {
    let Some(tail) = value.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = tail.split_once('/') else {
        return false;
    };
    let host_ok = host == "dynamical.org"
        || host.strip_suffix(".dynamical.org").is_some_and(|labels| {
            labels.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            })
        });
    host_ok
        && !path
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}
/// `next((link.get("href") for link in links if link.get("rel") == "self"), None)`:
/// lazy, so elements after the first self link are never inspected.
fn self_link(value: &Value) -> Result<Option<String>, CatalogError> {
    let links = match value.get("links") {
        None => return Ok(None),
        Some(Value::Array(links)) => links,
        Some(Value::String(s)) if s.is_empty() => return Ok(None),
        Some(Value::Object(o)) if o.is_empty() => return Ok(None),
        // Iterating a string or object yields `str` items.
        Some(Value::String(_) | Value::Object(_)) => {
            return Err(attribute_error(&Value::String(String::new()), "get"));
        }
        Some(other) => return Err(not_iterable(other)),
    };
    for link in links {
        if !link.is_object() {
            return Err(attribute_error(link, "get"));
        }
        if link.get("rel").and_then(Value::as_str) == Some("self") {
            return optional_text(link, "href");
        }
    }
    Ok(None)
}
/// `dict(value.get("cube:dimensions", {}))`; non-empty sequences of pairs are
/// outside the contract.
fn cube_dimensions(value: &Value) -> Result<IndexMap<String, Value>, CatalogError> {
    match value.get("cube:dimensions") {
        None => Ok(IndexMap::new()),
        Some(Value::Object(o)) => Ok(o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Some(Value::String(s)) if s.is_empty() => Ok(IndexMap::new()),
        Some(Value::Array(a)) if a.is_empty() => Ok(IndexMap::new()),
        Some(Value::String(_)) => Err(CatalogError::typed(
            "dictionary update sequence element #0 has length 1; 2 is required",
            "ValueError",
        )),
        Some(v @ Value::Array(_)) => Err(unsupported("cube:dimensions", v)),
        Some(other) => Err(not_iterable(other)),
    }
}
impl Collection {
    pub fn from_stac(value: &Value) -> Result<Self, CatalogError> {
        let missing = || CatalogError::new("Collection is missing required Icechunk metadata");
        let raw_id = value.get("id").ok_or_else(missing)?;
        let id = py_str(raw_id).ok_or_else(|| unsupported("id", raw_id))?;
        let asset = value
            .get("assets")
            .and_then(|a| a.get("icechunk"))
            .filter(|a| a.is_object())
            .ok_or_else(missing)?;
        // A container's repr has no URL scheme, so it fails as an invalid URL.
        let raw_href = asset.get("href").ok_or_else(missing)?;
        let href = py_str(raw_href).unwrap_or_else(|| raw_href.to_string());
        let parsed = crate::pyurl::urlparse(&href, "")?;
        let (scheme, host, path) = (parsed.scheme.as_str(), &parsed.netloc, &parsed.path);
        let region = if scheme == "https" {
            None
        } else {
            Some(
                asset
                    .pointer("/xarray:storage_options/client_kwargs/region_name")
                    .ok_or_else(missing)?,
            )
        };
        if !["s3", "https"].contains(&scheme)
            || host.is_empty()
            || path.trim_matches('/').is_empty()
        {
            return Err(CatalogError::new(format!(
                "Collection {} has an invalid Icechunk URL",
                python_repr(&id)
            )));
        }
        let invalid_metadata = || {
            CatalogError::new(format!(
                "Collection {} has invalid Icechunk metadata",
                python_repr(&id)
            ))
        };
        let icechunk = match &region {
            None => RepositoryLocation::Http(IcechunkHttpLocation { url: href.clone() }),
            Some(raw) if !truthy(raw) => return Err(invalid_metadata()),
            Some(raw) => RepositoryLocation::S3(IcechunkLocation {
                bucket: host.clone(),
                prefix: path.trim_matches('/').into(),
                region: py_str(raw).ok_or_else(|| unsupported("region_name", raw))?,
            }),
        };
        let recipe = match &region {
            None => json!({"type":"http", "base_url":href.trim_end_matches('/')}),
            Some(raw) => {
                json!({"type":"s3", "bucket":host, "prefix":path.trim_start_matches('/'), "region":raw})
            }
        };
        let mut containers = Vec::new();
        if let Some(v) = asset
            .get("icechunk:virtual_chunk_containers")
            .filter(|v| !v.is_null())
        {
            let empty = json!({});
            for entry in v.as_array().ok_or_else(invalid_metadata)? {
                if !entry.is_object() {
                    return Err(attribute_error(entry, "get"));
                }
                let credentials = entry
                    .get("credentials")
                    .filter(|c| truthy(c))
                    .unwrap_or(&empty);
                if !credentials.is_object() {
                    return Err(attribute_error(credentials, "get"));
                }
                let kind = credentials
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_metadata)?;
                let schemes: &[&str] = match kind {
                    "s3" => &["s3://"],
                    "gcs" => &["gs://", "gcs://"],
                    "azure" => &["az://", "azure://", "abfs://"],
                    "tigris" => &["tigris://"],
                    "http" => &["https://"],
                    _ => return Err(invalid_metadata()),
                };
                let prefix = entry
                    .get("url_prefix")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_metadata)?;
                if !schemes.iter().any(|s| prefix.starts_with(s))
                    || credentials.as_object().is_none_or(|o| {
                        o.keys()
                            .any(|k| !["type", "anonymous"].contains(&k.as_str()))
                    })
                    || credentials.get("anonymous") == Some(&Value::Bool(false))
                    || (kind != "http" && !credentials.get("anonymous").is_some_and(truthy))
                {
                    return Err(invalid_metadata());
                }
                containers.push(VirtualChunkContainer {
                    url_prefix: prefix.into(),
                    kind: kind.into(),
                });
            }
        }
        if (scheme == "https" && !on_dynamical_https_host(&href))
            || containers
                .iter()
                .any(|c| c.kind == "http" && !on_dynamical_https_host(&c.url_prefix))
        {
            return Err(CatalogError::new(format!(
                "Collection {} reads HTTP outside dynamical.org",
                python_repr(&id)
            )));
        }
        let mut variables = IndexMap::new();
        if let Some(raw) = value.get("cube:variables") {
            let raw = raw
                .as_object()
                .ok_or_else(|| attribute_error(raw, "items"))?;
            for (name, metadata) in raw {
                if !metadata.is_object() {
                    return Err(attribute_error(metadata, "get"));
                }
                if metadata.get("type").and_then(Value::as_str) != Some("data") {
                    continue;
                }
                let dimensions = match metadata.get("dimensions") {
                    None => vec![],
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|d| d.as_str().map(str::to_owned))
                        .collect::<Option<_>>()
                        .ok_or_else(|| unsupported("dimensions", &metadata["dimensions"]))?,
                    Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
                    Some(v @ Value::Object(_)) => return Err(unsupported("dimensions", v)),
                    Some(other) => return Err(not_iterable(other)),
                };
                variables.insert(
                    name.clone(),
                    Variable {
                        name: name.clone(),
                        dimensions,
                        unit: optional_text(metadata, "unit")?,
                        long_name: optional_text(metadata, "long_name")?,
                        standard_name: optional_text(metadata, "standard_name")?,
                    },
                );
            }
        }
        let empty = json!({});
        let extent = value.get("extent").unwrap_or(&empty);
        if !extent.is_object() {
            return Err(attribute_error(extent, "get"));
        }
        let spatial = extent.get("spatial").unwrap_or(&empty);
        if !spatial.is_object() {
            return Err(attribute_error(spatial, "get"));
        }
        let raw_bbox = spatial.get("bbox");
        let bbox = match raw_bbox.filter(|v| truthy(v)) {
            None => None,
            Some(outer) => {
                let first = match outer {
                    Value::Array(a) => a[0].clone(),
                    Value::String(text) => Value::String(text.chars().next().unwrap().to_string()),
                    Value::Object(_) => return Err(CatalogError::typed("0", "KeyError")),
                    _ => {
                        return Err(CatalogError::typed(
                            format!("'{}' object is not subscriptable", python_type(outer)),
                            "TypeError",
                        ));
                    }
                };
                let items: Vec<Value> = match &first {
                    Value::Array(a) => a.clone(),
                    Value::String(text) => {
                        text.chars().map(|c| Value::String(c.to_string())).collect()
                    }
                    Value::Object(o) => o.keys().cloned().map(Value::String).collect(),
                    _ => return Err(not_iterable(&first)),
                };
                let numbers: Result<Vec<_>, _> = items
                    .iter()
                    .map(|v| match v {
                        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
                        Value::String(text) => py_float(text),
                        _ => v.as_f64().ok_or_else(|| {
                            CatalogError::typed(
                                format!(
                                    "float() argument must be a string or a real number, not '{}'",
                                    python_type(v)
                                ),
                                "TypeError",
                            )
                        }),
                    })
                    .collect();
                Some(numbers?.try_into().map_err(|_| {
                    CatalogError::new(format!(
                        "Collection {} has an invalid spatial extent",
                        python_repr(&id)
                    ))
                })?)
            }
        };
        let catalog_url = self_link(value)?;
        let title = text(value, "title", &id)?;
        let description = text(value, "description", "")?;
        let version = text(value, "version", "unknown")?;
        let license = text(value, "license", "proprietary")?;
        let model_id = optional_text(value, "model_id")?;
        let model_name = optional_text(value, "model_name")?;
        let dimensions = cube_dimensions(value)?;
        let repository_config = json!({"id":value["id"], "name":value.get("title").unwrap_or(&value["id"]), "description":value.get("description").unwrap_or(&Value::String(String::new())), "icechunk":recipe, "virtual_chunk_containers":containers});
        Ok(Self {
            title,
            description,
            version,
            license,
            model_id,
            model_name,
            catalog_url,
            bbox,
            dimensions,
            variables,
            icechunk,
            repository_config,
            id,
        })
    }
    pub fn kind(&self) -> &'static str {
        if self.dimensions.contains_key("init_time") && self.dimensions.contains_key("lead_time") {
            "forecast"
        } else if self.dimensions.contains_key("time") {
            "analysis"
        } else {
            "unknown"
        }
    }
    pub fn point_query_reason(&self) -> Option<&'static str> {
        let spatial: Vec<_> = ["latitude", "longitude", "x", "y"]
            .into_iter()
            .filter(|d| self.dimensions.contains_key(*d))
            .collect();
        if spatial != ["latitude", "longitude"] && spatial != ["x", "y"] {
            Some("Data product has an unsupported spatial grid")
        } else if self.kind() == "unknown" {
            Some("Data product has unsupported time dimensions")
        } else {
            None
        }
    }
    pub fn virtual_chunk_containers(&self) -> Vec<VirtualChunkContainer> {
        serde_json::from_value(self.repository_config["virtual_chunk_containers"].clone())
            .unwrap_or_default()
    }
    /// Pure mapping. Retain non-HTTP containers in the recipe, but reject them
    /// for the current reader rather than silently dropping access declarations.
    #[cfg(feature = "reader")]
    pub fn reader_config(&self) -> Result<ReaderConfig, CatalogError> {
        use dynapi_reader::{HttpVirtualChunkContainer, StoreDescriptor};
        let store = match &self.icechunk {
            RepositoryLocation::Http(h) => StoreDescriptor::Http {
                url: h.url.trim_end_matches('/').into(),
            },
            RepositoryLocation::S3(s) => StoreDescriptor::AnonymousS3 {
                bucket: s.bucket.clone(),
                prefix: s.prefix.clone(),
                region: s.region.clone(),
            },
        };
        // Python authorizes containers through a dict keyed by prefix, so a
        // repeated declaration is one authorization; the recipe keeps both.
        let mut containers: Vec<HttpVirtualChunkContainer> = vec![];
        for c in self.virtual_chunk_containers() {
            if c.kind != "http" {
                return Err(CatalogError::new(
                    "Reader does not support non-HTTP virtual chunk containers",
                ));
            }
            if !containers.iter().any(|d| d.url_prefix == c.url_prefix) {
                containers.push(HttpVirtualChunkContainer {
                    url_prefix: c.url_prefix,
                    name: None,
                });
            }
        }
        Ok(ReaderConfig { store, containers })
    }
}
#[cfg(feature = "reader")]
#[derive(Debug)]
pub struct ReaderConfig {
    pub store: dynapi_reader::StoreDescriptor,
    pub containers: Vec<dynapi_reader::HttpVirtualChunkContainer>,
}
