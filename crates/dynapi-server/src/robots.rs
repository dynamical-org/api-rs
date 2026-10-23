//! Byte ranges for the one embedded robots asset (Starlette 1.3.1 contract).
use crate::{
    HttpResponse, ROBOTS, ROBOTS_ETAG, ROBOTS_LAST_MODIFIED, add_headers, latin1, response,
};
use dynapi_contract::validation::parse_grid_integer;
use dynapi_policy::{Headers, python_strip};
use hyper::HeaderMap;
use std::{fmt::Write, ops::Range};

enum RangeError {
    Malformed(&'static str),
    Unsatisfiable,
}

// All arithmetic is relative to a tiny asset. Saturating huge decimal operands
// preserves their comparison to its length without narrowing or overflowing.
fn integer(value: &str) -> Option<i64> {
    let decimal = parse_grid_integer(python_strip(value))?.to_string();
    Some(decimal.parse().unwrap_or_else(|_| {
        if decimal.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    }))
}

fn parse(value: &str) -> Result<Vec<Range<usize>>, RangeError> {
    let Some((unit, parts)) = value.split_once('=') else {
        return Err(RangeError::Malformed("Malformed range header."));
    };
    if !python_strip(unit).eq_ignore_ascii_case("bytes") {
        return Err(RangeError::Malformed("Only support bytes range"));
    }
    let size = ROBOTS.len() as i64;
    let mut ranges = Vec::new();
    for part in parts.split(',') {
        let Some((first, last)) = python_strip(part).split_once('-') else {
            continue;
        };
        let (first, last) = (python_strip(first), python_strip(last));
        let start = if first.is_empty() {
            let Some(suffix) = integer(last) else {
                continue;
            };
            size.saturating_sub(suffix).max(0)
        } else {
            let Some(start) = integer(first) else {
                continue;
            };
            start
        };
        let end = if !first.is_empty() && !last.is_empty() {
            let Some(end) = integer(last) else { continue };
            if end < size {
                end.saturating_add(1)
            } else {
                size
            }
        } else {
            size
        };
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(RangeError::Malformed(
            "Range header: range must be requested",
        ));
    }
    // This precedence is observable when one member is outside the asset and
    // another has its endpoints reversed. Empty slices are accepted by source.
    if ranges.iter().any(|(start, _)| !(0..size).contains(start)) {
        return Err(RangeError::Unsatisfiable);
    }
    if ranges.iter().any(|(start, end)| start > end) {
        return Err(RangeError::Malformed(
            "Range header: start must be less than end",
        ));
    }
    ranges.sort_unstable();
    let mut merged: Vec<Range<usize>> = Vec::new();
    for (start, end) in ranges {
        let range = start as usize..end as usize;
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Ok(merged)
}

fn boundary() -> String {
    let random = uuid::Uuid::new_v4();
    let mut text = String::with_capacity(26);
    // Skip the UUID's version/variant bytes to retain 13 fully random bytes,
    // matching token_hex(13), without adding another random-number dependency.
    for (_, byte) in random
        .as_bytes()
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 6 && *i != 8)
        .take(13)
    {
        write!(text, "{byte:02x}").unwrap();
    }
    text
}

pub(super) fn serve(headers: &HeaderMap, metadata: &Headers) -> HttpResponse {
    let range = headers.get("range").map(|v| latin1(v.as_bytes()));
    let if_range = headers.get("if-range").map(|v| latin1(v.as_bytes()));
    let use_range = if_range
        .as_deref()
        .is_none_or(|v| v == ROBOTS_ETAG || v == ROBOTS_LAST_MODIFIED);
    let mut result = if let Some(value) = range.filter(|_| use_range) {
        let ranges = match parse(&value) {
            Ok(ranges) => ranges,
            Err(RangeError::Malformed(text)) => {
                return response(
                    400,
                    text.as_bytes().to_vec(),
                    Some("text/plain; charset=utf-8"),
                );
            }
            Err(RangeError::Unsatisfiable) => {
                let mut error = response(416, vec![], Some("text/plain; charset=utf-8"));
                error.headers_mut().insert(
                    "content-range",
                    format!("bytes */{}", ROBOTS.len()).parse().unwrap(),
                );
                return error;
            }
        };
        if ranges.len() == 1 {
            let range = &ranges[0];
            let mut selected = response(
                206,
                ROBOTS[range.clone()].to_vec(),
                Some("text/plain; charset=utf-8"),
            );
            selected.headers_mut().insert(
                "content-range",
                format!(
                    "bytes {}-{}/{}",
                    range.start,
                    range.end as i64 - 1,
                    ROBOTS.len()
                )
                .parse()
                .unwrap(),
            );
            selected
        } else {
            let boundary = boundary();
            let mut body = Vec::new();
            for range in ranges {
                let header = format!(
                    "--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                    range.start,
                    range.end as i64 - 1,
                    ROBOTS.len()
                );
                body.extend_from_slice(header.as_bytes());
                body.extend_from_slice(&ROBOTS[range]);
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(format!("--{boundary}--").as_bytes());
            response(
                206,
                body,
                Some(&format!("multipart/byteranges; boundary={boundary}")),
            )
        }
    } else {
        response(200, ROBOTS.to_vec(), Some("text/plain; charset=utf-8"))
    };
    add_headers(&mut result, metadata.clone());
    result
        .headers_mut()
        .insert("cache-control", "public, max-age=300".parse().unwrap());
    result
}
