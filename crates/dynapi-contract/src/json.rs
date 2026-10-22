//! Lossless Python JSON number domain and insertion-ordered objects.
use num_bigint::BigInt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}
impl Value {
    pub fn object<const N: usize>(pairs: [(&str, Value); N]) -> Self {
        Self::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }
    pub fn get(&self, name: &str) -> Option<&Self> {
        if let Self::Object(pairs) = self {
            pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
        } else {
            None
        }
    }
    pub fn data_float(value: f64) -> Self {
        if value.is_finite() {
            Self::Float(value)
        } else {
            Self::Null
        }
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Self::Int(n.into())
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Self::Int(n.into())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    Json {
        position: usize,
        message: &'static str,
    },
    Body,
    Surrogate,
}

/// Decode the same UTF-8/16/32 byte encodings as json.loads(bytes).
fn decode(bytes: &[u8]) -> Result<String, ParseError> {
    let (width, little, skip) = if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        (4, false, 4)
    } else if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        (4, true, 4)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (2, false, 2)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (2, true, 2)
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        (1, false, 3)
    } else if bytes.len() >= 4 {
        // CPython 3.12 json.detect_encoding: the first two zero bytes
        // select UTF-32-BE regardless of the third and fourth bytes.
        if bytes[0] == 0 {
            (if bytes[1] == 0 { 4 } else { 2 }, false, 0)
        } else if bytes[1] == 0 {
            (if bytes[2] != 0 || bytes[3] != 0 { 2 } else { 4 }, true, 0)
        } else {
            (1, false, 0)
        }
    } else if bytes.len() == 2 && bytes[0] == 0 {
        (2, false, 0)
    } else if bytes.len() == 2 && bytes[1] == 0 {
        (2, true, 0)
    } else {
        (1, false, 0)
    };
    let bytes = &bytes[skip..];
    if width == 1 {
        return std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ParseError::Body);
    }
    if !bytes.len().is_multiple_of(width) {
        return Err(ParseError::Body);
    }
    if width == 2 {
        let units = bytes.chunks_exact(2).map(|x| {
            if little {
                u16::from_le_bytes([x[0], x[1]])
            } else {
                u16::from_be_bytes([x[0], x[1]])
            }
        });
        char::decode_utf16(units)
            .map(|x| x.map_err(|_| ParseError::Surrogate))
            .collect()
    } else {
        bytes
            .chunks_exact(4)
            .map(|x| {
                let n = if little {
                    u32::from_le_bytes(x.try_into().unwrap())
                } else {
                    u32::from_be_bytes(x.try_into().unwrap())
                };
                char::from_u32(n).ok_or(if (0xd800..=0xdfff).contains(&n) {
                    ParseError::Surrogate
                } else {
                    ParseError::Body
                })
            })
            .collect()
    }
}

pub fn parse(bytes: &[u8]) -> Result<Value, ParseError> {
    let decoded = decode(bytes)?;
    let mut parser = Parser {
        input: decoded.chars().collect(),
        pos: 0,
    };
    let value = parser.value(0)?;
    parser.whitespace();
    if parser.pos != parser.input.len() {
        return parser.fail("Extra data");
    }
    Ok(value)
}
struct Parser {
    input: Vec<char>,
    pos: usize,
}
impl Parser {
    fn fail<T>(&self, message: &'static str) -> Result<T, ParseError> {
        Err(ParseError::Json {
            position: self.pos,
            message,
        })
    }
    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            self.pos += 1;
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > 256 {
            return Err(ParseError::Body);
        }
        self.whitespace();
        match self.peek() {
            Some('"') => self.string().map(Value::String),
            Some('{') => {
                self.pos += 1;
                self.whitespace();
                let mut pairs: Vec<(String, Value)> = Vec::new();
                if self.peek() == Some('}') {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                loop {
                    if self.peek() != Some('"') {
                        return self.fail("Expecting property name enclosed in double quotes");
                    }
                    let key = self.string()?;
                    self.whitespace();
                    if self.peek() != Some(':') {
                        return self.fail("Expecting ':' delimiter");
                    }
                    self.pos += 1;
                    let value = self.value(depth + 1)?;
                    if let Some((_, v)) = pairs.iter_mut().find(|(k, _)| *k == key) {
                        *v = value;
                    } else {
                        pairs.push((key, value));
                    }
                    self.whitespace();
                    match self.peek() {
                        Some('}') => {
                            self.pos += 1;
                            break;
                        }
                        Some(',') => {
                            self.pos += 1;
                            self.whitespace();
                        }
                        _ => return self.fail("Expecting ',' delimiter"),
                    }
                }
                Ok(Value::Object(pairs))
            }
            Some('[') => {
                self.pos += 1;
                self.whitespace();
                let mut values = Vec::new();
                if self.peek() == Some(']') {
                    self.pos += 1;
                    return Ok(Value::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.whitespace();
                    match self.peek() {
                        Some(']') => {
                            self.pos += 1;
                            break;
                        }
                        Some(',') => self.pos += 1,
                        _ => return self.fail("Expecting ',' delimiter"),
                    }
                }
                Ok(Value::Array(values))
            }
            Some('t') if self.consume("true") => Ok(Value::Bool(true)),
            Some('f') if self.consume("false") => Ok(Value::Bool(false)),
            Some('n') if self.consume("null") => Ok(Value::Null),
            Some('N') if self.consume("NaN") => Ok(Value::Float(f64::NAN)),
            Some('I') if self.consume("Infinity") => Ok(Value::Float(f64::INFINITY)),
            Some('-') if self.consume("-Infinity") => Ok(Value::Float(f64::NEG_INFINITY)),
            Some('-' | '0'..='9') => self.number(),
            _ => self.fail("Expecting value"),
        }
    }
    fn consume(&mut self, s: &str) -> bool {
        let chars: Vec<_> = s.chars().collect();
        if self.input[self.pos..].starts_with(&chars) {
            self.pos += chars.len();
            true
        } else {
            false
        }
    }
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        match self.peek() {
            Some('0') => self.pos += 1,
            Some('1'..='9') => {
                while matches!(self.peek(), Some('0'..='9')) {
                    self.pos += 1;
                }
            }
            _ => {
                self.pos = start;
                return self.fail("Expecting value");
            }
        }
        let mut float = false;
        if self.peek() == Some('.') && matches!(self.input.get(self.pos + 1), Some('0'..='9')) {
            float = true;
            self.pos += 1;
            while matches!(self.peek(), Some('0'..='9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let exponent = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some('0'..='9')) {
                float = true;
                while matches!(self.peek(), Some('0'..='9')) {
                    self.pos += 1;
                }
            } else {
                self.pos = exponent;
            }
        }
        let text: String = self.input[start..self.pos].iter().collect();
        if float {
            Ok(Value::Float(text.parse().map_err(|_| ParseError::Body)?))
        } else {
            if text.trim_start_matches('-').len() > 4300 {
                return Err(ParseError::Body);
            }
            Ok(Value::Int(text.parse().map_err(|_| ParseError::Body)?))
        }
    }
    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    self.pos = start;
                    return self.fail("Unterminated string starting at");
                }
                Some('"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some('"' | '\\' | '/') => {
                            out.push(self.peek().unwrap());
                            self.pos += 1;
                        }
                        Some('b') => {
                            out.push('\u{08}');
                            self.pos += 1;
                        }
                        Some('f') => {
                            out.push('\u{0c}');
                            self.pos += 1;
                        }
                        Some('n') => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        Some('r') => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        Some('t') => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        Some('u') => {
                            let first = self.hex()?;
                            let n = if (0xd800..=0xdbff).contains(&first) {
                                if self.peek() != Some('\\')
                                    || self.input.get(self.pos + 1) != Some(&'u')
                                {
                                    return Err(ParseError::Surrogate);
                                }
                                self.pos += 1;
                                let second = self.hex()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(ParseError::Surrogate);
                                }
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            } else {
                                first
                            };
                            out.push(char::from_u32(n).ok_or(ParseError::Surrogate)?);
                        }
                        None => {
                            self.pos = start;
                            return self.fail("Unterminated string starting at");
                        }
                        _ => {
                            self.pos -= 1;
                            return self.fail("Invalid \\escape");
                        }
                    }
                }
                Some(c) if c < ' ' => return self.fail("Invalid control character at"),
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }
    fn hex(&mut self) -> Result<u32, ParseError> {
        let at = self.pos;
        // CPython's scanner requires a character beyond all four hex digits,
        // even when the escape is complete but the closing quote is missing.
        if self.input.len() - at <= 5 {
            return self.fail("Invalid \\uXXXX escape");
        }
        self.pos += 1;
        let mut n = 0;
        for _ in 0..4 {
            if let Some(d) = self
                .peek()
                .and_then(|c| c.to_digit(16))
                .filter(|_| self.peek().unwrap().is_ascii())
            {
                n = n * 16 + d;
                self.pos += 1;
            } else {
                self.pos = at;
                return self.fail("Invalid \\uXXXX escape");
            }
        }
        Ok(n)
    }
}
