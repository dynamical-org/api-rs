//! Ordered compact JSON, Python float repr, URLs, datetimes, and error messages.
use crate::json::Value;
use std::fmt::Write;

// Ryū supplies the shortest significant digits; apply CPython's repr layout.
pub fn float_repr(x: f64, out: &mut String) {
    if x.is_nan() {
        out.push_str("nan");
        return;
    }
    if x.is_infinite() {
        out.push_str(if x.is_sign_negative() { "-inf" } else { "inf" });
        return;
    }
    if x == 0.0 {
        out.push_str(if x.is_sign_negative() { "-0.0" } else { "0.0" });
        return;
    }
    if x.is_sign_negative() {
        out.push('-');
    }
    let mut b = ryu::Buffer::new();
    let raw = b.format_finite(x.abs());
    let (mantissa, power) = match raw.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap()),
        None => (raw, 0),
    };
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let mut digits = [0u8; 24];
    let mut len = 0usize;
    for c in mantissa.bytes() {
        if c != b'.' {
            digits[len] = c;
            len += 1;
        }
    }
    let start = digits[..len].iter().position(|c| *c != b'0').unwrap();
    let exp = power + point - start as i32 - 1;
    while len > start + 1 && digits[len - 1] == b'0' {
        len -= 1;
    }
    let d = std::str::from_utf8(&digits[start..len]).unwrap();
    if !(-4..16).contains(&exp) {
        out.push(d.as_bytes()[0] as char);
        if d.len() > 1 {
            out.push('.');
            out.push_str(&d[1..]);
        }
        write!(
            out,
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        )
        .unwrap();
    } else if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(d);
    } else {
        let point = exp as usize + 1;
        if point >= d.len() {
            out.push_str(d);
            for _ in d.len()..point {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            out.push_str(&d[..point]);
            out.push('.');
            out.push_str(&d[point..]);
        }
    }
}

/// Python quote_plus for ordered string pairs (UTF-8, safe="").
pub fn quote_plus(s: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    out
}
pub fn urlencode(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&quote_plus(k));
        out.push('=');
        out.push_str(&quote_plus(v));
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JsonError {
    NonFinite,
    DuplicateKey,
}

pub fn string(value: &str, ascii: bool, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < ' ' || (ascii && c >= '\u{7f}') => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    write!(out, "\\u{unit:04x}").unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Non-finite data values must use Value::data_float explicitly. All other floats reject them.
pub fn json(value: &Value, ensure_ascii: bool) -> Result<Vec<u8>, JsonError> {
    let mut out = String::new();
    write_json(value, ensure_ascii, &mut out)?;
    Ok(out.into_bytes())
}
fn write_json(value: &Value, ascii: bool, out: &mut String) -> Result<(), JsonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(x) => out.push_str(if *x { "true" } else { "false" }),
        Value::Int(x) => write!(out, "{x}").unwrap(),
        Value::Float(x) => {
            if !x.is_finite() {
                return Err(JsonError::NonFinite);
            }
            float_repr(*x, out);
        }
        Value::String(x) => string(x, ascii, out),
        Value::Array(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(x, ascii, out)?;
            }
            out.push(']');
        }
        Value::Object(xs) => {
            let mut seen = std::collections::BTreeSet::new();
            out.push('{');
            for (i, (key, x)) in xs.iter().enumerate() {
                if !seen.insert(key) {
                    return Err(JsonError::DuplicateKey);
                }
                if i > 0 {
                    out.push(',');
                }
                string(key, ascii, out);
                out.push(':');
                write_json(x, ascii, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Python datetime.isoformat(), using Z only for an exactly zero UTC offset.
/// Nonzero offsets, including positive sub-minute offsets, remain intact.
pub fn datetime(value: &speedate::DateTime) -> String {
    let d = value.date;
    let t = value.time;
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        d.year, d.month, d.day, t.hour, t.minute, t.second
    );
    if t.microsecond != 0 {
        write!(out, ".{:06}", t.microsecond).unwrap();
    }
    if let Some(offset) = t.tz_offset {
        if offset == 0 {
            out.push('Z');
        } else {
            let n = offset.unsigned_abs();
            write!(
                out,
                "{}{:02}:{:02}",
                if offset < 0 { '-' } else { '+' },
                n / 3600,
                n / 60 % 60
            )
            .unwrap();
            if n % 60 != 0 {
                write!(out, ":{:02}", n % 60).unwrap();
            }
        }
    }
    out
}

/// Python str.strip includes the four ASCII information separators, unlike Rust trim.
pub fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
pub fn normalize_variables(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|s| s.trim_matches(python_whitespace))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if crate::printable::UNPRINTABLE
                .binary_search_by(|(lo, hi)| {
                    let n = c as u32;
                    if n < *lo {
                        std::cmp::Ordering::Greater
                    } else if n > *hi {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .is_ok() =>
            {
                let n = c as u32;
                if n <= 0xff {
                    write!(out, "\\x{n:02x}").unwrap();
                } else if n <= 0xffff {
                    write!(out, "\\u{n:04x}").unwrap();
                } else {
                    write!(out, "\\U{n:08x}").unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

pub fn detail(message: &str) -> Vec<u8> {
    json(&Value::object([("detail", Value::from(message))]), false).unwrap()
}
pub fn product_not_found(id: &str) -> Vec<u8> {
    ErrorEnvelope::unknown_product(id).to_bytes()
}

/// The string-detail envelope shared by HTTP exception responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEnvelope {
    pub detail: String,
}
impl ErrorEnvelope {
    pub fn to_bytes(&self) -> Vec<u8> {
        detail(&self.detail)
    }
    pub fn unknown_product(id: &str) -> Self {
        Self {
            detail: format!("Unknown data product {}", python_repr(id)),
        }
    }
}
