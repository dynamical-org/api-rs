//! Explicit CF conversion for the M1b attested encodings, separate from stored reads.
use crate::{ErrorKind, MAX_OUTPUT_BYTES, ReaderError, TypedBuffer, err, unsupported};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const REVISION: &str = "m1b-cf-time-v1";
const NS_PER_SECOND: i64 = 1_000_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub revision: String,
    pub attributes: Map<String, Value>,
}

pub(crate) enum Encoding {
    EpochSeconds,
    DurationSeconds,
}

pub(crate) fn attested_encoding(
    dtype: &str,
    attrs: &Map<String, Value>,
) -> Result<Encoding, ReaderError> {
    const ALLOWED_ATTRIBUTES: &[&str] = &[
        "long_name",
        "standard_name",
        "statistics_approximate",
        "units",
        "calendar",
        "dtype",
        "_FillValue",
    ];
    if let Some(key) = attrs
        .keys()
        .find(|key| !ALLOWED_ATTRIBUTES.contains(&key.as_str()))
    {
        return Err(unsupported(format!("Unattested CF attribute {key}")));
    }
    if dtype == "int64"
        && attrs.get("units").and_then(Value::as_str) == Some("seconds since 1970-01-01")
        && attrs.get("calendar").and_then(Value::as_str) == Some("proleptic_gregorian")
        && !attrs.contains_key("dtype")
        && !attrs.contains_key("_FillValue")
    {
        Ok(Encoding::EpochSeconds)
    } else if dtype == "float64"
        && attrs.get("units").and_then(Value::as_str) == Some("seconds")
        && attrs.get("dtype").and_then(Value::as_str) == Some("timedelta64[us]")
        && attrs.get("_FillValue").and_then(Value::as_str) == Some("AAAAAAAA+H8=")
        && !attrs.contains_key("calendar")
    {
        Ok(Encoding::DurationSeconds)
    } else {
        Err(unsupported("Unattested CF time encoding"))
    }
}

/// Convert an already-read buffer with explicitly supplied, attested CF attributes.
/// The caller owns the association between buffer and metadata; no storage is opened.
/// Returns normalized datetime64[ns]/timedelta64[ns] little-endian integer bits.
pub fn decode(mut buffer: TypedBuffer, request: &Request) -> Result<TypedBuffer, ReaderError> {
    if request.revision != REVISION {
        return Err(unsupported("Unrecognized CF time revision"));
    }
    let encoding = attested_encoding(&buffer.dtype, &request.attributes)?;
    let count = if buffer.shape.contains(&0) {
        0
    } else {
        buffer
            .shape
            .iter()
            .try_fold(1usize, |n, size| n.checked_mul(*size))
            .ok_or_else(|| err(ErrorKind::SizeLimit, "CF shape overflow"))?
    };
    if count.checked_mul(8) != Some(buffer.bytes.len()) || buffer.bytes.len() > MAX_OUTPUT_BYTES {
        return Err(err(
            ErrorKind::Decode,
            "CF buffer length differs from shape or exceeds bound",
        ));
    }
    let overflow = || {
        err(
            ErrorKind::Decode,
            "CF time outside datetime64[ns]/timedelta64[ns] range",
        )
    };
    let values: Vec<i64> = match encoding {
        Encoding::EpochSeconds => buffer
            .bytes
            .chunks_exact(8)
            .map(|b| {
                let value = i64::from_le_bytes(b.try_into().map_err(|_| overflow())?);
                if value == i64::MIN {
                    Ok(i64::MIN)
                } else {
                    value
                        .checked_mul(NS_PER_SECOND)
                        .filter(|v| *v != i64::MIN)
                        .ok_or_else(overflow)
                }
            })
            .collect::<Result<_, _>>()?,
        Encoding::DurationSeconds => {
            let mut values: Vec<f64> = buffer
                .bytes
                .chunks_exact(8)
                .map(|b| f64::from_le_bytes(b.try_into().expect("8-byte chunk")))
                .collect();
            if values.iter().any(|v| v.is_infinite()) {
                return Err(overflow());
            }
            // Match xarray's successive s -> ms -> us -> ns multiplications and
            // truncation, rather than a single multiply that rounds differently.
            let mut multiplier = NS_PER_SECOND;
            while multiplier > 1 && values.iter().any(|v| !v.is_nan() && v.fract() != 0.0) {
                for value in &mut values {
                    *value *= 1000.0;
                }
                multiplier /= 1000;
            }
            values
                .into_iter()
                .map(|value| {
                    if value.is_nan() {
                        return Ok(i64::MIN);
                    }
                    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value)
                    {
                        return Err(overflow());
                    }
                    (value as i64)
                        .checked_mul(multiplier)
                        .filter(|v| *v != i64::MIN)
                        .ok_or_else(overflow)
                })
                .collect::<Result<_, _>>()?
        }
    };
    buffer.dtype = match encoding {
        Encoding::EpochSeconds => "datetime64[ns]",
        Encoding::DurationSeconds => "timedelta64[ns]",
    }
    .into();
    buffer.bytes = values.into_iter().flat_map(i64::to_le_bytes).collect();
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn epoch() -> Request {
        serde_json::from_value(json!({
            "revision": REVISION,
            "attributes": {"units":"seconds since 1970-01-01", "calendar":"proleptic_gregorian"}
        }))
        .unwrap()
    }

    #[test]
    fn buffer_shape_and_empty_bounds() {
        let make = |shape, bytes| TypedBuffer {
            dtype: "int64".into(),
            shape,
            bytes,
        };
        assert_eq!(
            decode(make(vec![usize::MAX, 2], vec![]), &epoch())
                .unwrap_err()
                .kind,
            ErrorKind::SizeLimit
        );
        assert_eq!(
            decode(make(vec![2], vec![0; 8]), &epoch())
                .unwrap_err()
                .kind,
            ErrorKind::Decode
        );
        // An empty axis gives an empty buffer without multiplying the other axes.
        let empty = decode(make(vec![usize::MAX, 0], vec![]), &epoch()).unwrap();
        assert_eq!(empty.shape, vec![usize::MAX, 0]);
        assert!(empty.bytes.is_empty());
    }

    #[test]
    fn epoch_conversion_is_independent_of_selection_context() {
        // Pin the reader contract used by the dataset's bounded epoch staging:
        // selecting before decode must preserve every bit, order and duplicate.
        let seconds = [i64::MIN, -9_223_372_036, -1, 0, 1, 9_223_372_036, 7];
        let make = |values: &[i64]| TypedBuffer {
            dtype: "int64".into(),
            shape: vec![values.len()],
            bytes: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        };
        let request = epoch();
        assert!(matches!(
            attested_encoding("int64", &request.attributes),
            Ok(Encoding::EpochSeconds)
        ));
        let full = decode(make(&seconds), &request).unwrap();
        for indices in [vec![], vec![0], vec![5, 2, 0, 5], vec![6, 4, 3, 1]] {
            let selected: Vec<_> = indices.iter().map(|&i| seconds[i]).collect();
            let decoded = decode(make(&selected), &request).unwrap();
            let expected: Vec<_> = indices
                .iter()
                .flat_map(|&i| full.bytes[i * 8..(i + 1) * 8].iter().copied())
                .collect();
            assert_eq!(decoded.dtype, "datetime64[ns]");
            assert_eq!(decoded.shape, vec![indices.len()]);
            assert_eq!(decoded.bytes, expected);
        }
    }

    #[test]
    fn epoch_zero_nat_and_checked_overflow() {
        let make = |values: &[i64]| TypedBuffer {
            dtype: "int64".into(),
            shape: vec![values.len()],
            bytes: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        };
        let result = decode(make(&[0, i64::MIN, -1, 9_223_372_036]), &epoch()).unwrap();
        assert_eq!(
            result.bytes,
            [0i64, i64::MIN, -1_000_000_000, 9_223_372_036_000_000_000]
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            decode(make(&[9_223_372_037]), &epoch()).unwrap_err().kind,
            ErrorKind::Decode
        );
    }
}
