use super::*;

// Exact WKT inputs from the public catalog's repositories; unrecognized
// WKT, including geographic-looking malformed WKT, remains Projected.
pub const GEOGRAPHIC_WKTS: &[&str] = &[
    r#"GEOGCS["Coordinate System imported from GRIB file",DATUM["unnamed",SPHEROID["Sphere",6371229,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST]]"#,
    r#"GEOGCS["unknown",DATUM["unknown",SPHEROID["unknown",6371229,0]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Longitude",EAST],AXIS["Latitude",NORTH]]"#,
    r#"GEOGCS["unknown",DATUM["unknown",SPHEROID["unknown",6367470,0]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Longitude",EAST],AXIS["Latitude",NORTH]]"#,
    r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","4326"]]"#,
    r#"GEOGCS["Coordinate System imported from GRIB file",DATUM["unnamed",SPHEROID["Spheroid imported from GRIB file",6378160,298.253916296469]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST]]"#,
];

pub(crate) fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(v) => *v,
        Json::String(v) => !v.is_empty(),
        Json::Array(v) => !v.is_empty(),
        Json::Object(v) => !v.is_empty(),
        Json::Number(v) => v.as_f64() != Some(0.0),
    }
}
fn invalid(message: &str) -> OpenError {
    OpenError::Metadata(message.into())
}

pub(crate) fn dimensions(meta: &Json) -> Result<Vec<String>, OpenError> {
    let shape = meta["shape"]
        .as_array()
        .ok_or_else(|| invalid("Missing array shape"))?;
    if shape.is_empty() && meta["dimension_names"].is_null() {
        return Ok(Vec::new());
    }
    let dims = meta["dimension_names"]
        .as_array()
        .ok_or_else(|| invalid("Missing Zarr dimension_names"))?;
    if dims.len() != shape.len() {
        return Err(invalid("Dimension names differ from array rank"));
    }
    let mut dimensions = Vec::with_capacity(dims.len());
    for dim in dims {
        dimensions.push(
            dim.as_str()
                .ok_or_else(|| invalid("Unnamed Zarr dimension"))?
                .to_owned(),
        );
    }
    Ok(dimensions)
}

pub(crate) fn cf_request(meta: &Json) -> Result<Option<cf_time::Request>, OpenError> {
    let attrs = &meta["attributes"];
    let units = attrs["units"].as_str().unwrap_or("");
    let temporal = units.contains("since")
        || attrs["dtype"]
            .as_str()
            .is_some_and(|dtype| dtype.starts_with("timedelta64["));
    if !temporal {
        return Ok(None);
    }
    let attributes = attrs
        .as_object()
        .ok_or_else(|| invalid("Expected attributes object"))?
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "units" | "calendar" | "dtype" | "_FillValue"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok(Some(cf_time::Request {
        revision: cf_time::REVISION.into(),
        attributes,
    }))
}

pub(crate) fn attributes(
    attrs: &Json,
    time: bool,
    variable: bool,
) -> Result<BTreeMap<String, Value>, OpenError> {
    let attrs = attrs
        .as_object()
        .ok_or_else(|| invalid("Expected attributes object"))?;
    attrs
        .iter()
        .filter(|(k, v)| {
            !(k.as_str() == "coordinates" && (variable || v.is_string()))
                && (!variable || k.as_str() != "_FillValue")
                && !(time
                    && (matches!(k.as_str(), "units" | "dtype")
                        || (k.as_str() == "calendar"
                            && attrs
                                .get("units")
                                .and_then(Json::as_str)
                                .is_some_and(|u| u.contains("since")))))
        })
        .map(|(k, v)| {
            Ok((
                k.clone(),
                dynapi_contract::json::parse(v.to_string().as_bytes())
                    .map_err(|_| invalid("Invalid attribute JSON"))?,
            ))
        })
        .collect()
}

pub(crate) fn variable(meta: &Json) -> Result<Variable, OpenError> {
    let dimensions = dimensions(meta)?;
    // zarr.Array.chunks reports inner chunks, not the outer shard shape.
    let codecs = meta["codecs"]
        .as_array()
        .ok_or_else(|| invalid("Missing codecs"))?;
    let shape = if codecs
        .first()
        .is_some_and(|c| c["name"] == "sharding_indexed")
    {
        &codecs[0]["configuration"]["chunk_shape"]
    } else {
        &meta["chunk_grid"]["configuration"]["chunk_shape"]
    };
    let shape = shape
        .as_array()
        .ok_or_else(|| invalid("Missing chunk shape"))?;
    let mut chunks = Vec::with_capacity(shape.len());
    for size in shape {
        chunks.push(Some(
            size.as_i64()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid chunk size"))?,
        ));
    }
    if chunks.len() != dimensions.len() {
        return Err(invalid("Chunk rank differs from dimension rank"));
    }
    let preferred_chunks = dimensions
        .iter()
        .cloned()
        .zip(chunks.iter().map(|v| v.unwrap()))
        .collect();
    Ok(Variable {
        is_coordinate: false,
        dimensions,
        attributes: attributes(&meta["attributes"], cf_request(meta)?.is_some(), true)?,
        chunks: Some(chunks),
        preferred_chunks,
    })
}

fn ints(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|b| i64::from_le_bytes(b.try_into().expect("8-byte chunk")))
        .collect()
}
pub(crate) fn coordinate(buffer: TypedBuffer, members: bool) -> Result<Coordinate, OpenError> {
    if members {
        return Ok(Coordinate::Members(values(buffer)?));
    }
    Ok(match buffer.dtype.as_str() {
        "float32" => Coordinate::Float32(
            buffer
                .bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().expect("4-byte chunk")))
                .collect(),
        ),
        "float64" => Coordinate::Numeric(
            buffer
                .bytes
                .chunks_exact(8)
                .map(|b| f64::from_le_bytes(b.try_into().expect("8-byte chunk")))
                .collect(),
        ),
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" => {
            Coordinate::Int64(integer_values(&buffer)?)
        }
        "datetime64[ns]" => Coordinate::Time(ints(&buffer.bytes)),
        "timedelta64[ns]" => Coordinate::Duration(ints(&buffer.bytes)),
        _ => return Err(invalid("Unsupported decoded coordinate dtype")),
    })
}
fn integer_values(buffer: &TypedBuffer) -> Result<Vec<i64>, OpenError> {
    macro_rules! decode {
        ($ty:ty) => {
            buffer
                .bytes
                .chunks_exact(std::mem::size_of::<$ty>())
                .map(|b| i64::from(<$ty>::from_le_bytes(b.try_into().expect("integer width"))))
                .collect()
        };
    }
    Ok(match buffer.dtype.as_str() {
        "int8" => decode!(i8),
        "int16" => decode!(i16),
        "int32" => decode!(i32),
        "int64" => ints(&buffer.bytes),
        "uint8" => decode!(u8),
        "uint16" => decode!(u16),
        "uint32" => decode!(u32),
        _ => return Err(invalid("Unsupported integer dtype")),
    })
}
pub(crate) fn values(buffer: TypedBuffer) -> Result<Vec<Value>, OpenError> {
    match buffer.dtype.as_str() {
        "float32" => Ok(buffer
            .bytes
            .chunks_exact(4)
            .map(|b| {
                Value::Float(f64::from(f32::from_le_bytes(
                    b.try_into().expect("4-byte chunk"),
                )))
            })
            .collect()),
        "float64" => Ok(buffer
            .bytes
            .chunks_exact(8)
            .map(|b| Value::Float(f64::from_le_bytes(b.try_into().expect("8-byte chunk"))))
            .collect()),
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" => {
            Ok(integer_values(&buffer)?
                .into_iter()
                .map(Value::from)
                .collect())
        }
        _ => Err(invalid("Unsupported query value dtype")),
    }
}

/// Conservative, deterministic accounting. Fixed node overhead intentionally
/// exceeds BTree/JSON map links, and includes staging, not just final payloads.
pub(crate) struct Budget {
    used: usize,
    limit: usize,
    exceeded: bool,
}
impl Budget {
    pub fn new(limit: usize) -> Self {
        Self {
            used: 0,
            limit,
            exceeded: false,
        }
    }
    pub fn used(&self) -> usize {
        self.used
    }
    pub fn add(&mut self, bytes: usize) -> Result<(), OpenError> {
        if let Some(total) = self.used.checked_add(bytes).filter(|v| *v <= self.limit) {
            self.used = total;
        } else {
            self.exceeded = true;
            return Err(OpenError::MetadataLimit { limit: self.limit });
        }
        Ok(())
    }
    pub fn parse(&mut self, bytes: &[u8]) -> Result<Json, OpenError> {
        use serde::de::DeserializeSeed;
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        Charge(self).deserialize(&mut deserializer).map_err(|e| {
            if self.exceeded {
                OpenError::MetadataLimit { limit: self.limit }
            } else {
                invalid(&e.to_string())
            }
        })?;
        deserializer.end().map_err(|e| invalid(&e.to_string()))?;
        serde_json::from_slice(bytes).map_err(|e| invalid(&e.to_string()))
    }
    pub fn attrs(&mut self, attrs: &BTreeMap<String, Value>) -> Result<(), OpenError> {
        for (key, value) in attrs {
            self.add(key.len() + 64)?;
            self.value(value)?;
        }
        Ok(())
    }
    fn value(&mut self, value: &Value) -> Result<(), OpenError> {
        self.add(std::mem::size_of::<Value>() + 64)?;
        match value {
            Value::String(s) => self.add(s.len())?,
            Value::Int(n) => self.add(n.to_signed_bytes_le().len())?,
            Value::Array(xs) => {
                for x in xs {
                    self.value(x)?;
                }
            }
            Value::Object(xs) => {
                for (k, v) in xs {
                    self.add(k.len() + 64)?;
                    self.value(v)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn variable(&mut self, name: &str, var: &Variable) -> Result<(), OpenError> {
        self.add(name.len() + std::mem::size_of::<Variable>() + 64)?;
        for d in &var.dimensions {
            self.add(d.len() + std::mem::size_of::<String>())?;
        }
        if let Some(chunks) = &var.chunks {
            self.add(chunks.len() * std::mem::size_of::<Option<i64>>())?;
        }
        for name in var.preferred_chunks.keys() {
            self.add(name.len() + 96)?;
        }
        self.attrs(&var.attributes)
    }
}

// First pass charges the parsed tree before serde allocates it. It also reserves
// a second copy for decoded attribute/dimension staging. No JSON tree is built
// by this visitor, including under arbitrary_precision + preserve_order.
struct Charge<'a>(&'a mut Budget);
impl<'de> serde::de::DeserializeSeed<'de> for Charge<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        self.0
            .add(2 * (std::mem::size_of::<Json>() + 64))
            .map_err(serde::de::Error::custom)?;
        de.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for Charge<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded JSON metadata")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        self.0.add(40).map_err(E::custom)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        self.0.add(40).map_err(E::custom)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        self.0.add(48).map_err(E::custom)
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<(), E> {
        self.0.add(value.len().saturating_mul(2)).map_err(E::custom)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(Charge(self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        // Charge keys as nodes as well: conservative for JSON map capacities,
        // and for serde_json's arbitrary-precision number pseudo-map.
        while map.next_key_seed(Charge(self.0))?.is_some() {
            map.next_value_seed(Charge(self.0))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_preflight_bounds_allocations_and_preserves_number_identity() {
        let bytes = br#"{"small":1e-20,"integer":9007199254740993,"nested":[true,null,"value"]}"#;
        let mut budget = Budget::new(100_000);
        let value = budget.parse(bytes).unwrap();
        assert_eq!(value["integer"].as_i64(), Some(9_007_199_254_740_993));
        assert_eq!(value["small"].as_f64(), Some(1e-20));
        let measured = budget.used();
        assert!(matches!(
            Budget::new(measured - 1).parse(bytes),
            Err(OpenError::MetadataLimit { .. })
        ));
        assert!(Budget::new(measured).parse(bytes).is_ok());
        assert!(matches!(
            Budget::new(100_000).parse(b"{bad"),
            Err(OpenError::Metadata(_))
        ));
        let mut overflow = Budget::new(usize::MAX);
        overflow.add(usize::MAX).unwrap();
        assert!(overflow.add(1).is_err());
    }

    #[test]
    fn cf_attributes_are_consumed_only_in_the_correct_scope() {
        let attrs = serde_json::json!({"coordinates":"spatial_ref","_FillValue":42,"units":"seconds","calendar":"proleptic_gregorian","dtype":"timedelta64[us]","comment":"synthetic"});
        let root = attributes(&attrs, false, false).unwrap();
        assert_eq!(root["_FillValue"], Value::from(42i64));
        assert!(root.contains_key("units"));
        assert!(!root.contains_key("coordinates"));
        let temporal = attributes(&attrs, true, true).unwrap();
        assert_eq!(temporal.len(), 2);
        // Timedelta decoding does not consume a calendar attribute; unused
        // dimension coordinates must retain it even when their data stays lazy.
        assert_eq!(temporal["calendar"], Value::from("proleptic_gregorian"));
        assert_eq!(temporal["comment"], Value::from("synthetic"));
        // xarray's default timedelta decoder requires the dtype hint.
        assert!(
            cf_request(&serde_json::json!({"attributes":{"units":"seconds"}}))
                .unwrap()
                .is_none()
        );
    }
}
