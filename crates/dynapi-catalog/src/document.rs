//! `json.loads(bytes)` acceptance through dynapi-contract's Python-compatible
//! parser, then conversion to `serde_json` values for collection parsing.
use crate::{CatalogError, MAX_DOCUMENT_BYTES};
use dynapi_contract::json::{self, ParseError};
use serde_json::{Map, Number, Value};
use std::borrow::Cow;

/// Keep CPython's float spelling even when serde_json's arbitrary_precision
/// feature is unified in by another workspace crate. Number::from_f64 uses
/// serde_json's own exponent layout in that build.
pub(crate) fn float_number(value: f64) -> Option<Number> {
    if !value.is_finite() {
        return None;
    }
    let mut text = String::new();
    dynapi_contract::output::float_repr(value, &mut text);
    text.parse().ok()
}

/// Fields the collection/root parsers read; see DEVIATIONS.md D11.
const READ_FIELDS: &[&str] = &[
    "id",
    "title",
    "description",
    "version",
    "license",
    "model_id",
    "model_name",
    "links",
    "cube:dimensions",
    "cube:variables",
];
/// Only one child of these objects is read.
const READ_CHILD: &[(&str, &str)] = &[("assets", "icechunk"), ("extent", "spatial")];
pub(crate) const UNSUPPORTED_NUMBER: &str =
    "STAC field holds a number outside the supported JSON number range";

fn unicode_error(message: String) -> CatalogError {
    CatalogError::typed(message, "UnicodeDecodeError")
}
fn surrogate_error() -> CatalogError {
    CatalogError::typed(
        "STAC JSON contains a lone Unicode surrogate",
        "UnicodeDecodeError",
    )
}
/// CPython's UTF-8 decoder message for the first invalid sequence.
fn utf8_error(bytes: &[u8], error: std::str::Utf8Error) -> CatalogError {
    let start = error.valid_up_to();
    // surrogatepass accepts encoded surrogates; Rust strings cannot hold them.
    if bytes[start] == 0xed
        && bytes
            .get(start + 1)
            .is_some_and(|b| (0xa0..=0xbf).contains(b))
        && bytes
            .get(start + 2)
            .is_some_and(|b| (0x80..=0xbf).contains(b))
    {
        return surrogate_error();
    }
    let (end, reason) = match error.error_len() {
        None => (bytes.len(), "unexpected end of data"),
        Some(1) if matches!(bytes[start], 0x80..=0xc1 | 0xf5..=0xff) => {
            (start + 1, "invalid start byte")
        }
        Some(n) => (start + n, "invalid continuation byte"),
    };
    unicode_error(if end - start == 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    })
}
/// `json.detect_encoding` and `bytes.decode(encoding, "surrogatepass")`.
fn decode(bytes: &[u8]) -> Result<Cow<'_, str>, CatalogError> {
    let (codec, width, little, skip) = if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        ("utf-32", 4, false, 4)
    } else if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        ("utf-32", 4, true, 4)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        ("utf-16", 2, false, 2)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        ("utf-16", 2, true, 2)
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        ("utf-8", 1, false, 3)
    } else if bytes.len() >= 4 && bytes[0] == 0 {
        if bytes[1] != 0 {
            ("utf-16-be", 2, false, 0)
        } else {
            ("utf-32-be", 4, false, 0)
        }
    } else if bytes.len() >= 4 && bytes[1] == 0 {
        if bytes[2] != 0 || bytes[3] != 0 {
            ("utf-16-le", 2, true, 0)
        } else {
            ("utf-32-le", 4, true, 0)
        }
    } else if bytes.len() == 2 && bytes[0] == 0 {
        ("utf-16-be", 2, false, 0)
    } else if bytes.len() == 2 && bytes[1] == 0 {
        ("utf-16-le", 2, true, 0)
    } else {
        ("utf-8", 1, false, 0)
    };
    let bytes = &bytes[skip..];
    if width == 1 {
        return std::str::from_utf8(bytes)
            .map(Cow::Borrowed)
            .map_err(|e| utf8_error(bytes, e));
    }
    let invalid = || unicode_error(format!("'{codec}' codec can't decode the STAC document"));
    if !bytes.len().is_multiple_of(width) {
        return Err(invalid());
    }
    let units = bytes.chunks_exact(width).map(|x| {
        let mut word = [0; 4];
        word[..width].copy_from_slice(x);
        match (width, little) {
            (2, true) => u16::from_le_bytes([x[0], x[1]]) as u32,
            (2, false) => u16::from_be_bytes([x[0], x[1]]) as u32,
            (_, true) => u32::from_le_bytes(word),
            (_, false) => u32::from_be_bytes(word),
        }
    });
    if width == 2 {
        char::decode_utf16(units.map(|u| u as u16))
            .map(|c| c.map_err(|_| surrogate_error()))
            .collect::<Result<String, _>>()
            .map(Cow::Owned)
    } else {
        units
            .map(|n| match char::from_u32(n) {
                Some(c) => Ok(c),
                None if (0xd800..=0xdfff).contains(&n) => Err(surrogate_error()),
                None => Err(invalid()),
            })
            .collect::<Result<String, _>>()
            .map(Cow::Owned)
    }
}
fn decode_error(text: &str, position: usize, message: &str) -> CatalogError {
    let before: Vec<char> = text.chars().take(position).collect();
    let line = before.iter().filter(|c| **c == '\n').count() + 1;
    let column = position - before.iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1) + 1;
    CatalogError::typed(
        format!("{message}: line {line} column {column} (char {position})"),
        "JSONDecodeError",
    )
}
/// After decoding, the contract parser rejects only nesting deeper than 256
/// values (Python allows far more) and integers over CPython's 4300 digits.
fn limit_error(text: &str) -> CatalogError {
    let (mut depth, mut deepest, mut in_string, mut escaped) = (0usize, 0, false, false);
    let mut chars = text.chars().peekable();
    let mut previous = ' ';
    let mut long_integer = None;
    while let Some(c) = chars.next() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '[' || c == '{' {
            depth += 1;
            deepest = deepest.max(depth);
        } else if c == ']' || c == '}' {
            depth = depth.saturating_sub(1);
        } else if c.is_ascii_digit() && !previous.is_ascii_alphanumeric() && previous != '.' {
            let mut digits = 1;
            while chars.next_if(char::is_ascii_digit).is_some() {
                digits += 1;
            }
            let float = matches!(chars.peek(), Some('.' | 'e' | 'E'));
            if digits > 4300 && !float && long_integer.is_none() {
                long_integer = Some(digits);
            }
            previous = '0';
            continue;
        }
        previous = c;
    }
    match long_integer {
        Some(digits) if deepest < 257 => CatalogError::typed(
            format!(
                "Exceeds the limit (4300 digits) for integer string conversion: value has \
                 {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
            ),
            "ValueError",
        ),
        _ => CatalogError::typed("STAC JSON nesting exceeds 256 levels", "RecursionError"),
    }
}
struct Converter {
    unsupported_read: bool,
}
impl Converter {
    fn value(&mut self, value: json::Value, read: bool) -> Value {
        match value {
            json::Value::Null => Value::Null,
            json::Value::Bool(b) => Value::Bool(b),
            json::Value::String(s) => Value::String(s),
            json::Value::Int(n) => {
                let number = i64::try_from(&n)
                    .map(Number::from)
                    .or_else(|_| u64::try_from(&n).map(Number::from));
                self.number(number.ok(), read)
            }
            json::Value::Float(x) => self.number(float_number(x), read),
            json::Value::Array(values) => {
                Value::Array(values.into_iter().map(|v| self.value(v, read)).collect())
            }
            json::Value::Object(pairs) => Value::Object(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k, self.value(v, read)))
                    .collect(),
            ),
        }
    }
    /// Unread fields keep a placeholder; read fields make the document unusable.
    fn number(&mut self, number: Option<Number>, read: bool) -> Value {
        number.map(Value::Number).unwrap_or_else(|| {
            self.unsupported_read |= read;
            Value::Null
        })
    }
    fn document(&mut self, value: json::Value) -> Value {
        let json::Value::Object(pairs) = value else {
            return self.value(value, true);
        };
        let mut map = Map::new();
        for (key, value) in pairs {
            let child = READ_CHILD.iter().find(|(k, _)| *k == key).map(|(_, c)| *c);
            let value = match (child, value) {
                (Some(read_child), json::Value::Object(pairs)) => Value::Object(
                    pairs
                        .into_iter()
                        .map(|(k, v)| {
                            let read = k == read_child;
                            (k, self.value(v, read))
                        })
                        .collect(),
                ),
                (_, value) => self.value(value, READ_FIELDS.contains(&key.as_str())),
            };
            map.insert(key, value);
        }
        Value::Object(map)
    }
}
/// Parse a document as Python's `json.loads(bytes)`. The flag reports a NaN,
/// infinity or out-of-range integer in a field the catalog reads.
pub(crate) fn parse(body: &[u8]) -> Result<(Value, bool), CatalogError> {
    if body.len() > MAX_DOCUMENT_BYTES {
        return Err(CatalogError::new("STAC document exceeds maximum body size"));
    }
    let text = decode(body)?;
    let value = match json::parse(body) {
        Ok(value) => value,
        Err(ParseError::Json { position, message }) => {
            return Err(decode_error(&text, position, message));
        }
        Err(ParseError::Surrogate) => return Err(surrogate_error()),
        Err(ParseError::Body) => return Err(limit_error(&text)),
    };
    if !matches!(value, json::Value::Object(_)) {
        return Err(CatalogError::new("Expected a STAC JSON object"));
    }
    let mut converter = Converter {
        unsupported_read: false,
    };
    let value = converter.document(value);
    Ok((value, converter.unsupported_read))
}
