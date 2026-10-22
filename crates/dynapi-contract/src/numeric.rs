//! Numeric coercion ported from pydantic-core 2.46.4 and jiter 0.14.0.
//! See LICENSE-numeric for the upstream MIT notices.
use num_bigint::BigInt;
use std::borrow::Cow;

fn strip_underscores(s: &str) -> Option<String> {
    // pydantic-core 2.46.4 src/input/shared.rs:60–70 does not require digits
    // beside underscores; their position is checked before removal.
    if s.starts_with('_') || s.ends_with('_') || !s.contains('_') || s.contains("__") {
        None
    } else {
        Some(s.replace('_', ""))
    }
}

pub(crate) fn str_as_int(s: &str) -> Result<BigInt, &'static str> {
    match strict_integer(s.as_bytes()) {
        Ok(int) => return Ok(int),
        Err("int_parsing_size") => return Err("int_parsing_size"),
        Err(_) => {}
    }
    // pydantic-core 2.46.4 src/input/shared.rs:77–97 preserves size errors
    // only on the first pass; every cleaned-retry failure is int_parsing.
    if let Some(cleaned) = clean_int_str(s) {
        strict_integer(cleaned.as_bytes()).map_err(|_| "int_parsing")
    } else {
        Err("int_parsing")
    }
}

pub(crate) fn str_as_float(s: &str) -> Option<f64> {
    // pydantic-core 2.46.4 src/input/shared.rs:101–108 retries the original
    // untrimmed string. This is Rust's float parser, not jiter's JSON decoder.
    match s.trim().parse() {
        Ok(float) => Some(float),
        Err(_) => strip_underscores(s).and_then(|stripped| stripped.parse().ok()),
    }
}

fn clean_int_str(mut s: &str) -> Option<Cow<'_, str>> {
    let len_before = s.len();

    s = s.trim();

    if let Some(suffix) = s.strip_prefix('+') {
        if suffix.starts_with('-') {
            return None;
        }
        s = suffix;
    }

    let mut is_negative = false;
    if let Some(suffix) = s.strip_prefix('-') {
        if suffix.starts_with('-') | suffix.starts_with('+') {
            return None;
        }

        is_negative = true;
        s = suffix;
    }

    // pydantic-core 2.46.4 src/input/shared.rs:141–154: zeros, then .0+, then
    // underscores. In particular, `0-_0` becomes `-_0`, then `-0`.
    s = strip_leading_zeros(s)?;

    if let Some(i) = s.find('.') {
        let decimal_part = &s[i + 1..];
        if !decimal_part.is_empty() && decimal_part.chars().all(|c| c == '0') {
            s = &s[..i];
        }
    }

    if let Some(str_stripped) = strip_underscores(s) {
        match is_negative {
            true => return Some(("-".to_string() + &str_stripped).into()),
            false => return Some(str_stripped.into()),
        }
    }

    if len_before == s.len() {
        return None;
    }

    match is_negative {
        true => Some(("-".to_string() + s).into()),
        false => Some(s.into()),
    }
}

fn strip_leading_zeros(s: &str) -> Option<&str> {
    let mut char_iter = s.char_indices();
    match char_iter.next() {
        Some((_, '0')) => (),
        Some((_, c)) if ('1'..='9').contains(&c) || c == '-' => return Some(s),
        _ => return None,
    }
    for (i, c) in char_iter {
        match c {
            '0' | '_' => (),
            '1'..='9' | '-' => return Some(&s[i..]),
            '.' => return Some(&s[(i - 1)..]),
            _ => return None,
        }
    }
    Some(&s[s.len() - 1..])
}

pub(crate) fn float_as_int(float: f64) -> Result<BigInt, &'static str> {
    if float.is_infinite() || float.is_nan() {
        Err("finite_number")
    } else if float % 1.0 != 0.0 {
        Err("int_from_float")
    } else if (i64::MIN as f64) < float && float < (i64::MAX as f64) {
        Ok((float as i64).into())
    } else {
        Err("int_parsing_size")
    }
}

fn strict_integer(data: &[u8]) -> Result<BigInt, &'static str> {
    let first = *data.first().ok_or("int_parsing")?;
    let (parsed, index) = IntParse::parse(data, 0, first)?;
    match parsed {
        IntParse::Int(int) if index == data.len() => Ok(int),
        _ => Err("int_parsing"),
    }
}

enum IntParse {
    Int(BigInt),
    Float,
}

impl IntParse {
    fn parse(data: &[u8], mut index: usize, first: u8) -> Result<(Self, usize), &'static str> {
        let start = index;
        let positive = match first {
            b'N' => return Ok((Self::Float, index)),
            b'-' => false,
            _ => true,
        };
        if !positive {
            index += 1;
        }
        let first2 = if positive {
            Some(&first)
        } else {
            data.get(index)
        };
        let first_value = match first2 {
            Some(b'0') => {
                index += 1;
                return match data.get(index) {
                    Some(b'.' | b'e' | b'E') => Ok((Self::Float, index)),
                    Some(digit) if digit.is_ascii_digit() => Err("int_parsing"),
                    _ => Ok((Self::Int(0.into()), index)),
                };
            }
            Some(b'I') => return Ok((Self::Float, index)),
            Some(digit) if (b'1'..=b'9').contains(digit) => (digit & 0x0f) as u64,
            _ => return Err("int_parsing"),
        };

        index += 1;
        let (chunk, new_index) = decode_int_chunk(data, index, first_value);
        let ongoing = match chunk {
            IntChunk::Ongoing(value) => value,
            IntChunk::Done(value) => {
                let mut value_i64 = value as i64;
                if !positive {
                    value_i64 = -value_i64;
                }
                return Ok((Self::Int(value_i64.into()), new_index));
            }
            IntChunk::Float => return Ok((Self::Float, new_index)),
        };

        // jiter 0.14.0 src/number_decoder.rs:238–295, using its portable
        // 18-digit fallback on every target. The sign counts toward 4300,
        // and the chunk's size is checked before its Float/Done variant.
        let mut big_value: BigInt = ongoing.into();
        index = new_index;
        loop {
            let (chunk, new_index) = decode_int_chunk(data, index, 0);
            if (new_index - start) > 4300 {
                return Err("int_parsing_size");
            }
            match chunk {
                IntChunk::Ongoing(value) => {
                    big_value *= 10u64.pow(18);
                    big_value += value;
                    index = new_index;
                }
                IntChunk::Done(value) => {
                    big_value *= 10u64.pow((new_index - index) as u32);
                    big_value += value;
                    if !positive {
                        big_value = -big_value;
                    }
                    return Ok((Self::Int(big_value), new_index));
                }
                IntChunk::Float => return Ok((Self::Float, new_index)),
            }
        }
    }
}

enum IntChunk {
    Ongoing(u64),
    Done(u64),
    Float,
}

fn decode_int_chunk(data: &[u8], mut index: usize, mut value: u64) -> (IntChunk, usize) {
    for _ in 0..18 {
        if let Some(digit) = data.get(index) {
            if digit.is_ascii_digit() {
                value = value.wrapping_mul(10).wrapping_add((digit & 0x0f) as u64);
                index += 1;
                continue;
            } else if matches!(digit, b'.' | b'e' | b'E') {
                return (IntChunk::Float, index);
            }
        }
        return (IntChunk::Done(value), index);
    }
    (IntChunk::Ongoing(value), index)
}
