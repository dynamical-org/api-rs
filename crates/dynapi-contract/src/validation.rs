//! The documented FastAPI/Pydantic ForecastBatch request contract.
use crate::{
    json::{self, ParseError, Value},
    numeric::{float_as_int, str_as_float as parse_float, str_as_int as parse_integer},
    output,
};
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use speedate::{
    Date, DateTime, DateTimeConfig, MicrosecondsPrecisionOverflowBehavior, Time, TimeConfig,
};
use strum::EnumMessage;

// Shared with documentation so enforced bounds and literal values stay aligned.
pub const MAX_QUERIES: usize = 10;
pub const MAX_VARIABLES: usize = 25;
pub const MAX_QUERY_ID_LENGTH: usize = 128;
pub const MIN_NONEMPTY_LENGTH: usize = 1;
pub const LATITUDE_BOUND: f64 = 90.;
pub const LONGITUDE_BOUND: f64 = 180.;
pub const LATEST_INIT_TIME: &str = "latest";
pub const MIN_LEAD_TIME_HOURS: i64 = 0;

#[derive(Clone, Debug, PartialEq)]
pub struct ForecastBatch {
    pub queries: Vec<ForecastQuery>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum InitTime {
    Latest,
    Exact(DateTime),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Representation {
    #[default]
    Auto,
    Requested,
}
impl Representation {
    pub const ALL: [Self; 2] = [Self::Auto, Self::Requested];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Requested => "requested",
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ForecastQuery {
    pub query_id: Option<String>,
    pub data_product_id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub variables: Vec<String>,
    pub init_time: Option<InitTime>,
    pub init_times_after: Option<DateTime>,
    pub init_times_before: Option<DateTime>,
    pub valid_time_start: Option<DateTime>,
    pub valid_time_end: Option<DateTime>,
    pub max_lead_time_hours: Option<BigInt>,
    pub representation: Representation,
}
impl ForecastBatch {
    /// Presentation-independent model dump, with handler variable normalization applied.
    pub fn normalized(&self) -> Value {
        Value::object([(
            "queries",
            Value::Array(self.queries.iter().map(ForecastQuery::normalized).collect()),
        )])
    }
}
impl ForecastQuery {
    pub fn normalized(&self) -> Value {
        let dt = |v: Option<DateTime>| {
            v.map(|x| Value::from(output::datetime(&x)))
                .unwrap_or(Value::Null)
        };
        Value::object([
            (
                "queryId",
                self.query_id
                    .clone()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            ),
            ("dataProductId", Value::from(self.data_product_id.clone())),
            (
                "location",
                Value::object([
                    ("latitude", Value::Float(self.latitude)),
                    ("longitude", Value::Float(self.longitude)),
                ]),
            ),
            (
                "variables",
                Value::Array(
                    output::normalize_variables(&self.variables)
                        .into_iter()
                        .map(Value::from)
                        .collect(),
                ),
            ),
            (
                "initTime",
                match self.init_time {
                    Some(InitTime::Latest) => Value::from(LATEST_INIT_TIME),
                    Some(InitTime::Exact(d)) => Value::from(output::datetime(&d)),
                    None => Value::Null,
                },
            ),
            ("initTimesAfter", dt(self.init_times_after)),
            ("initTimesBefore", dt(self.init_times_before)),
            ("validTimeStart", dt(self.valid_time_start)),
            ("validTimeEnd", dt(self.valid_time_end)),
            (
                "maxLeadTimeHours",
                self.max_lead_time_hours
                    .clone()
                    .map(Value::Int)
                    .unwrap_or(Value::Null),
            ),
            ("representation", Value::from(self.representation.as_str())),
        ])
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum ValidationResult {
    Valid(ForecastBatch),
    Invalid(Response),
}
impl Response {
    fn server_error() -> Self {
        Self {
            status: 500,
            body: b"Internal Server Error".to_vec(),
        }
    }
    fn parsing_error() -> Self {
        Self {
            status: 400,
            body: output::detail("There was an error parsing the body"),
        }
    }
    fn errors(errors: Vec<Value>) -> Self {
        let mut value = Value::object([("detail", Value::Array(errors))]);
        filter_sqlalchemy_keys(&mut value);
        match output::json(&value, false) {
            Ok(body) => Self { status: 422, body },
            Err(_) => Self::server_error(),
        }
    }
}
// FastAPI jsonable_encoder's default sqlalchemy_safe=True applies to every
// dictionary in the error tree, but does not remove strings from loc arrays.
fn filter_sqlalchemy_keys(value: &mut Value) {
    match value {
        Value::Object(pairs) => {
            pairs.retain(|(key, _)| !key.starts_with("_sa"));
            for (_, value) in pairs {
                filter_sqlalchemy_keys(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                filter_sqlalchemy_keys(value);
            }
        }
        _ => {}
    }
}
fn error(kind: &str, loc: &[Value], msg: &str, input: &Value, ctx: Option<Value>) -> Value {
    let mut pairs = vec![
        ("type".into(), Value::from(kind)),
        ("loc".into(), Value::Array(loc.to_vec())),
        ("msg".into(), Value::from(msg)),
        ("input".into(), input.clone()),
    ];
    if let Some(ctx) = ctx {
        pairs.push(("ctx".into(), ctx));
    }
    Value::Object(pairs)
}
fn path(loc: &[Value], part: impl Into<Value>) -> Vec<Value> {
    let mut loc = loc.to_vec();
    loc.push(part.into());
    loc
}
fn is_json(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim_matches(output::python_whitespace)
        .to_lowercase();
    // email.message.Message.get_content_type falls back to text/plain unless
    // the parameter-free media type contains exactly one slash.
    if media.bytes().filter(|b| *b == b'/').count() != 1 {
        return false;
    }
    media.split_once('/').is_some_and(|(main, sub)| {
        main == "application" && (sub == "json" || sub.ends_with("+json"))
    })
}

pub fn validate_forecast(body: &[u8], content_type: Option<&str>) -> ValidationResult {
    let invalid = |r| ValidationResult::Invalid(r);
    let root = vec![Value::from("body")];
    let input = if body.is_empty() {
        Value::Null
    } else if is_json(content_type) {
        match json::parse(body) {
            Ok(value) => value,
            Err(ParseError::Body) => return invalid(Response::parsing_error()),
            Err(ParseError::Surrogate) => return invalid(Response::server_error()),
            Err(ParseError::Json { position, message }) => {
                return invalid(Response::errors(vec![error(
                    "json_invalid",
                    &path(&root, position),
                    "JSON decode error",
                    &Value::Object(vec![]),
                    Some(Value::object([("error", Value::from(message))])),
                )]));
            }
        }
    } else {
        match std::str::from_utf8(body) {
            Ok(s) => Value::from(s),
            Err(_) => return invalid(Response::server_error()),
        }
    };
    if input == Value::Null {
        return invalid(Response::errors(vec![error(
            "missing",
            &root,
            "Field required",
            &input,
            None,
        )]));
    }
    let mut validator = Validator { errors: Vec::new() };
    let Some(pairs) = validator.model(&input, &root) else {
        return invalid(Response::errors(validator.errors));
    };
    let mut queries = Vec::new();
    let loc = path(&root, "queries");
    if let Some(raw) = input.get("queries") {
        if let Value::Array(values) = raw {
            if values.len() > MAX_QUERIES {
                validator.length(raw, &loc, MAX_QUERIES, true, values.len());
            } else {
                let before = validator.errors.len();
                for (i, value) in values.iter().enumerate() {
                    if let Some(q) = validator.query(value, &path(&loc, i)) {
                        queries.push(q);
                    }
                }
                if values.is_empty() && validator.errors.len() == before {
                    validator.length(raw, &loc, MIN_NONEMPTY_LENGTH, false, 0);
                }
            }
        } else {
            validator.push("list_type", &loc, "Input should be a valid list", raw, None);
        }
    } else {
        validator.push("missing", &loc, "Field required", &input, None);
    }
    validator.extras(pairs, &["queries"], &root);
    if validator.errors.is_empty() {
        ValidationResult::Valid(ForecastBatch { queries })
    } else {
        invalid(Response::errors(validator.errors))
    }
}
struct Validator {
    errors: Vec<Value>,
}
impl Validator {
    fn push(&mut self, kind: &str, loc: &[Value], msg: &str, input: &Value, ctx: Option<Value>) {
        self.errors.push(error(kind, loc, msg, input, ctx));
    }
    fn model<'a>(&mut self, input: &'a Value, loc: &[Value]) -> Option<&'a Vec<(String, Value)>> {
        match input {
            Value::Object(pairs) => Some(pairs),
            _ => {
                self.push(
                    "model_attributes_type",
                    loc,
                    "Input should be a valid dictionary or object to extract fields from",
                    input,
                    None,
                );
                None
            }
        }
    }
    fn length(&mut self, input: &Value, loc: &[Value], limit: usize, max: bool, actual: usize) {
        let key = if max { "max_length" } else { "min_length" };
        self.push(
            if max { "too_long" } else { "too_short" },
            loc,
            &format!(
                "List should have at {} {limit} item{} after validation, not {actual}",
                if max { "most" } else { "least" },
                if limit == 1 { "" } else { "s" }
            ),
            input,
            Some(Value::object([
                ("field_type", Value::from("List")),
                (key, Value::from(limit)),
                ("actual_length", Value::from(actual)),
            ])),
        );
    }
    fn extras(&mut self, pairs: &[(String, Value)], known: &[&str], loc: &[Value]) {
        for (key, value) in pairs {
            if !known.contains(&key.as_str()) {
                self.push(
                    "extra_forbidden",
                    &path(loc, key.as_str()),
                    "Extra inputs are not permitted",
                    value,
                    None,
                );
            }
        }
    }
    fn field<'a>(
        &mut self,
        input: &'a Value,
        alias: &str,
        name: &str,
        required: bool,
        loc: &[Value],
    ) -> (Option<&'a Value>, Vec<Value>, String) {
        let key = if input.get(alias).is_some() {
            alias
        } else if input.get(name).is_some() {
            name
        } else {
            alias
        };
        let p = path(loc, key);
        let value = input.get(key);
        if value.is_none() && required {
            self.push("missing", &p, "Field required", input, None);
        }
        (value, p, key.to_owned())
    }
    fn string(
        &mut self,
        input: &Value,
        loc: &[Value],
        min: Option<usize>,
        max: Option<usize>,
    ) -> Option<String> {
        let Value::String(s) = input else {
            self.push(
                "string_type",
                loc,
                "Input should be a valid string",
                input,
                None,
            );
            return None;
        };
        let n = s.chars().count();
        if let Some(min) = min.filter(|v| n < *v) {
            self.push(
                "string_too_short",
                loc,
                &format!(
                    "String should have at least {min} character{}",
                    if min == 1 { "" } else { "s" }
                ),
                input,
                Some(Value::object([("min_length", min.into())])),
            );
            return None;
        }
        if let Some(max) = max.filter(|v| n > *v) {
            self.push(
                "string_too_long",
                loc,
                &format!("String should have at most {max} characters"),
                input,
                Some(Value::object([("max_length", max.into())])),
            );
            return None;
        }
        Some(s.clone())
    }
    fn float(&mut self, input: &Value, loc: &[Value], bound: f64) -> Option<f64> {
        let value = match input {
            Value::Float(n) => Some(*n),
            Value::Int(n) => n.to_f64().filter(|n| n.is_finite()),
            Value::Bool(b) => Some(u8::from(*b) as f64),
            Value::String(s) => parse_float(s),
            _ => None,
        };
        let Some(value) = value else {
            let string = matches!(input, Value::String(_));
            self.push(
                if string {
                    "float_parsing"
                } else {
                    "float_type"
                },
                loc,
                if string {
                    "Input should be a valid number, unable to parse string as a number"
                } else {
                    "Input should be a valid number"
                },
                input,
                None,
            );
            return None;
        };
        if value < -bound {
            self.push(
                "greater_than_equal",
                loc,
                &format!("Input should be greater than or equal to {}", -bound as i32),
                input,
                Some(Value::object([("ge", Value::Float(-bound))])),
            );
            return None;
        }
        if value.is_nan() || value > bound {
            self.push(
                "less_than_equal",
                loc,
                &format!("Input should be less than or equal to {}", bound as i32),
                input,
                Some(Value::object([("le", Value::Float(bound))])),
            );
            return None;
        }
        Some(value)
    }
    fn integer(&mut self, input: &Value, loc: &[Value]) -> Option<BigInt> {
        let (value, kind) = match input {
            Value::Int(n) => (Some(n.clone()), ""),
            Value::Bool(b) => (Some(u8::from(*b).into()), ""),
            Value::Float(n) => match float_as_int(*n) {
                Ok(n) => (Some(n), ""),
                Err(kind) => (None, kind),
            },
            Value::String(s) => match parse_integer(s) {
                Ok(n) => (Some(n), ""),
                Err(kind) => (None, kind),
            },
            _ => (None, "int_type"),
        };
        let Some(value) = value else {
            let msg = match kind {
                "finite_number" => "Input should be a finite number",
                "int_from_float" => {
                    "Input should be a valid integer, got a number with a fractional part"
                }
                "int_parsing" => {
                    "Input should be a valid integer, unable to parse string as an integer"
                }
                "int_parsing_size" => {
                    "Unable to parse input string as an integer, exceeded maximum size"
                }
                _ => "Input should be a valid integer",
            };
            self.push(kind, loc, msg, input, None);
            return None;
        };
        if value < BigInt::from(MIN_LEAD_TIME_HOURS) {
            self.push(
                "greater_than_equal",
                loc,
                "Input should be greater than or equal to 0",
                input,
                Some(Value::object([("ge", Value::from(0i64))])),
            );
            return None;
        }
        Some(value)
    }
    fn datetime(&mut self, input: &Value, loc: &[Value]) -> Option<DateTime> {
        let cfg = DateTimeConfig {
            time_config: TimeConfig {
                microseconds_precision_overflow_behavior:
                    MicrosecondsPrecisionOverflowBehavior::Truncate,
                unix_timestamp_offset: Some(0),
            },
            ..Default::default()
        };
        let parsed = match input {
            Value::String(s) => match DateTime::parse_bytes_with_config(s.as_bytes(), &cfg) {
                Ok(d) => return self.checked_datetime(d, input, loc),
                Err(_) => match Date::parse_bytes(s.as_bytes()) {
                    Ok(date) => {
                        return self.checked_datetime(
                            DateTime {
                                date,
                                time: Time {
                                    hour: 0,
                                    minute: 0,
                                    second: 0,
                                    microsecond: 0,
                                    tz_offset: None,
                                },
                            },
                            input,
                            loc,
                        )
                    }
                    Err(e) => Err((
                        "datetime_from_date_parsing",
                        e.get_documentation().unwrap_or_default().to_owned(),
                    )),
                },
            },
            Value::Int(n) => match n.to_i64() {
                Some(n) => DateTime::from_timestamp_with_config(n, 0, &cfg).map_err(|e| {
                    (
                        "datetime_parsing",
                        e.get_documentation().unwrap_or_default().to_owned(),
                    )
                }),
                None => match n.to_f64().filter(|n| n.is_finite()) {
                    Some(n) => DateTime::from_float_with_config(n, &cfg).map_err(|e| {
                        (
                            "datetime_parsing",
                            e.get_documentation().unwrap_or_default().to_owned(),
                        )
                    }),
                    None => {
                        self.push(
                            "datetime_type",
                            loc,
                            "Input should be a valid datetime",
                            input,
                            None,
                        );
                        return None;
                    }
                },
            },
            Value::Float(n) if n.is_nan() => {
                Err(("datetime_parsing", "NaN values not permitted".to_owned()))
            }
            Value::Float(n) => DateTime::from_float_with_config(*n, &cfg).map_err(|e| {
                (
                    "datetime_parsing",
                    e.get_documentation().unwrap_or_default().to_owned(),
                )
            }),
            _ => {
                self.push(
                    "datetime_type",
                    loc,
                    "Input should be a valid datetime",
                    input,
                    None,
                );
                return None;
            }
        };
        match parsed {
            Ok(d) => self.checked_datetime(d, input, loc),
            Err((kind, msg)) => {
                self.push(
                    kind,
                    loc,
                    &format!(
                        "Input should be a valid datetime{}",
                        if kind == "datetime_from_date_parsing" {
                            format!(" or date, {msg}")
                        } else {
                            format!(", {msg}")
                        }
                    ),
                    input,
                    Some(Value::object([("error", Value::from(msg))])),
                );
                None
            }
        }
    }
    fn checked_datetime(&mut self, d: DateTime, input: &Value, loc: &[Value]) -> Option<DateTime> {
        if d.date.year == 0 {
            self.push(
                "datetime_parsing",
                loc,
                "Input should be a valid datetime, year 0 is out of range",
                input,
                Some(Value::object([(
                    "error",
                    Value::from("year 0 is out of range"),
                )])),
            );
            None
        } else {
            Some(d)
        }
    }
    fn query(&mut self, input: &Value, loc: &[Value]) -> Option<ForecastQuery> {
        let pairs = self.model(input, loc)?;
        let start = self.errors.len();
        let mut used = Vec::new();
        let (raw, p, key) = self.field(input, "queryId", "query_id", false, loc);
        used.push(key);
        let query_id = raw
            .filter(|x| **x != Value::Null)
            .and_then(|v| self.string(v, &p, Some(MIN_NONEMPTY_LENGTH), Some(MAX_QUERY_ID_LENGTH)));
        let (raw, p, key) = self.field(input, "dataProductId", "data_product_id", true, loc);
        used.push(key);
        let product = raw.and_then(|v| self.string(v, &p, Some(MIN_NONEMPTY_LENGTH), None));
        let (raw, p, key) = self.field(input, "location", "location", true, loc);
        used.push(key);
        let mut latitude = None;
        let mut longitude = None;
        if let Some(raw) = raw {
            if let Some(pairs) = self.model(raw, &p) {
                let (v, l, _) = self.field(raw, "latitude", "latitude", true, &p);
                latitude = v.and_then(|v| self.float(v, &l, LATITUDE_BOUND));
                let (v, l, _) = self.field(raw, "longitude", "longitude", true, &p);
                longitude = v.and_then(|v| self.float(v, &l, LONGITUDE_BOUND));
                self.extras(pairs, &["latitude", "longitude"], &p);
            }
        }
        let (raw, p, key) = self.field(input, "variables", "variables", true, loc);
        used.push(key);
        let mut variables = Vec::new();
        if let Some(raw) = raw {
            match raw {
                Value::Array(values) => {
                    if values.len() > MAX_VARIABLES {
                        self.length(raw, &p, MAX_VARIABLES, true, values.len());
                    } else {
                        for (i, v) in values.iter().enumerate() {
                            if let Some(s) = self.string(v, &path(&p, i), None, None) {
                                variables.push(s);
                            }
                        }
                        if values.is_empty() {
                            self.length(raw, &p, MIN_NONEMPTY_LENGTH, false, 0);
                        }
                    }
                }
                _ => self.push("list_type", &p, "Input should be a valid list", raw, None),
            }
        }
        let (raw, p, key) = self.field(input, "initTime", "init_time", false, loc);
        used.push(key);
        let init_time = raw.filter(|x| **x != Value::Null).and_then(|v| {
            if v == &Value::from(LATEST_INIT_TIME) {
                Some(InitTime::Latest)
            } else {
                let d = self.datetime(v, &path(&p, "datetime"));
                if d.is_none() {
                    self.push(
                        "literal_error",
                        &path(&p, "literal['latest']"),
                        "Input should be 'latest'",
                        v,
                        Some(Value::object([("expected", Value::from("'latest'"))])),
                    );
                }
                d.map(InitTime::Exact)
            }
        });
        let mut times = [None; 4];
        for (i, (alias, name)) in [
            ("initTimesAfter", "init_times_after"),
            ("initTimesBefore", "init_times_before"),
            ("validTimeStart", "valid_time_start"),
            ("validTimeEnd", "valid_time_end"),
        ]
        .into_iter()
        .enumerate()
        {
            let (raw, p, key) = self.field(input, alias, name, false, loc);
            used.push(key);
            times[i] = raw
                .filter(|x| **x != Value::Null)
                .and_then(|v| self.datetime(v, &p));
        }
        let (raw, p, key) =
            self.field(input, "maxLeadTimeHours", "max_lead_time_hours", false, loc);
        used.push(key);
        let max_lead_time_hours = raw
            .filter(|x| **x != Value::Null)
            .and_then(|v| self.integer(v, &p));
        let (raw, p, key) = self.field(input, "representation", "representation", false, loc);
        used.push(key);
        let representation = match raw {
            None => Representation::default(),
            Some(Value::String(value)) if value == Representation::Auto.as_str() => {
                Representation::Auto
            }
            Some(Value::String(value)) if value == Representation::Requested.as_str() => {
                Representation::Requested
            }
            Some(value) => {
                self.push(
                    "literal_error",
                    &p,
                    "Input should be 'auto' or 'requested'",
                    value,
                    Some(Value::object([(
                        "expected",
                        Value::from("'auto' or 'requested'"),
                    )])),
                );
                Representation::Auto
            }
        };
        self.extras(
            pairs,
            &used.iter().map(String::as_str).collect::<Vec<_>>(),
            loc,
        );
        if self.errors.len() != start {
            return None;
        }
        let query = ForecastQuery {
            query_id,
            data_product_id: product?,
            latitude: latitude?,
            longitude: longitude?,
            variables,
            init_time,
            init_times_after: times[0],
            init_times_before: times[1],
            valid_time_start: times[2],
            valid_time_end: times[3],
            max_lead_time_hours,
            representation,
        };
        if let Some(msg) = model_error(&query) {
            self.push(
                "value_error",
                loc,
                &format!("Value error, {msg}"),
                input,
                Some(Value::object([("error", Value::Object(vec![]))])),
            );
            return None;
        }
        Some(query)
    }
}
fn model_error(query: &ForecastQuery) -> Option<String> {
    let range = query.init_times_after.is_some() || query.init_times_before.is_some();
    if query.init_time.is_some() && range {
        return Some("Provide either initTime or both initTimesAfter and initTimesBefore".into());
    }
    if range && (query.init_times_after.is_none() || query.init_times_before.is_none()) {
        return Some("initTimesAfter and initTimesBefore must be provided together".into());
    }
    let exact = match query.init_time {
        Some(InitTime::Exact(d)) => Some(d),
        _ => None,
    };
    for (name, value) in [
        ("initTime", exact),
        ("initTimesAfter", query.init_times_after),
        ("initTimesBefore", query.init_times_before),
        ("validTimeStart", query.valid_time_start),
        ("validTimeEnd", query.valid_time_end),
    ] {
        if value.is_some_and(|d| d.time.tz_offset.is_none()) {
            return Some(format!("{name} must include a timezone"));
        }
    }
    if let (Some(a), Some(b)) = (query.init_times_after, query.init_times_before) {
        if a >= b {
            return Some("initTimesAfter must be before initTimesBefore".into());
        }
    }
    if query.valid_time_start.is_some() != query.valid_time_end.is_some() {
        return Some("validTimeStart and validTimeEnd must be provided together".into());
    }
    if let (Some(a), Some(b)) = (query.valid_time_start, query.valid_time_end) {
        if a >= b {
            return Some("validTimeStart must be before validTimeEnd".into());
        }
    }
    None
}

/// Canonical query parameters use the same scalar validation as the body model.
#[derive(Clone, Debug)]
pub struct CanonicalQuery {
    pub variables: String,
    pub init_time: Option<DateTime>,
    pub max_lead_time_hours: Option<BigInt>,
    pub valid_time_start: Option<DateTime>,
    pub valid_time_end: Option<DateTime>,
    pub start_time: Option<DateTime>,
    pub end_time: Option<DateTime>,
}
pub fn validate_canonical(params: &[(String, String)]) -> Result<CanonicalQuery, Response> {
    let mut validator = Validator { errors: vec![] };
    let get = |name: &str| {
        params
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| Value::from(v.clone()))
    };
    let loc = |name: &str| vec![Value::from("query"), Value::from(name)];
    let variables = match get("variables") {
        None => {
            validator.push(
                "missing",
                &loc("variables"),
                "Field required",
                &Value::Null,
                None,
            );
            String::new()
        }
        Some(Value::String(value)) => {
            if value.is_empty() {
                validator.push(
                    "string_too_short",
                    &loc("variables"),
                    "String should have at least 1 character",
                    &Value::from(value.clone()),
                    Some(Value::object([("min_length", Value::from(1i64))])),
                );
            }
            value
        }
        _ => unreachable!(),
    };
    let init_time = get("initTime").and_then(|v| validator.datetime(&v, &loc("initTime")));
    let max_lead_time_hours =
        get("maxLeadTimeHours").and_then(|v| validator.integer(&v, &loc("maxLeadTimeHours")));
    let valid_time_start =
        get("validTimeStart").and_then(|v| validator.datetime(&v, &loc("validTimeStart")));
    let valid_time_end =
        get("validTimeEnd").and_then(|v| validator.datetime(&v, &loc("validTimeEnd")));
    let start_time = get("startTime").and_then(|v| validator.datetime(&v, &loc("startTime")));
    let end_time = get("endTime").and_then(|v| validator.datetime(&v, &loc("endTime")));
    if !validator.errors.is_empty() {
        return Err(Response::errors(validator.errors));
    }
    Ok(CanonicalQuery {
        variables,
        init_time,
        max_lead_time_hours,
        valid_time_start,
        valid_time_end,
        start_time,
        end_time,
    })
}

/// Canonical path grammar follows Python int(str), independently of Pydantic's
/// query/body integer coercion. Decimal fractions are never path integers.
pub fn parse_grid_integer(value: &str) -> Option<BigInt> {
    let value = value.trim_matches(char::is_whitespace);
    let (negative, digits) = if let Some(s) = value.strip_prefix('-') {
        (true, s)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    let mut ascii = Vec::new();
    let mut previous_digit = false;
    for c in digits.chars() {
        if c == '_' && previous_digit {
            previous_digit = false;
            continue;
        }
        let digit = decimal_digit(c)?;
        // CPython's default decimal conversion limit counts digits, not signs
        // or separators. Bound staging before constructing an arbitrary integer.
        if ascii.len() == 4300 {
            return None;
        }
        ascii.push(b'0' + digit);
        previous_digit = true;
    }
    if !previous_digit {
        return None;
    }
    let number = BigInt::parse_bytes(&ascii, 10)?;
    Some(if negative { -number } else { number })
}

fn decimal_digit(c: char) -> Option<u8> {
    // Unicode 15.0 UnicodeData.txt, General_Category=Nd, as used by CPython
    // 3.12. Each zero begins a consecutive ten-digit block; mathematical styles
    // have separate zeros. Fixed 68-entry table, no locale or Unicode dependency.
    // Source: https://www.unicode.org/Public/15.0.0/ucd/UnicodeData.txt
    const ZEROS: [u32; 68] = [
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0,
        0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60,
        0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
        0x1e950, 0x1fbf0,
    ];
    let n = c as u32;
    let i = ZEROS.partition_point(|zero| *zero <= n).checked_sub(1)?;
    let digit = n - ZEROS[i];
    (digit < 10).then_some(digit as u8)
}

#[cfg(test)]
mod grid_integer_tests {
    use super::*;

    #[test]
    fn grid_integer_has_bounded_decimal_grammar_separate_from_schema_coercion() {
        for (s, expected) in [("  +١_２\u{a0}", 12), ("-００", 0), ("𝟡", 9)] {
            assert_eq!(parse_grid_integer(s), Some(BigInt::from(expected)));
        }
        for s in [
            "0.0", "0.00", "_0", "0_", "0__0", "+_0", "²", "\u{1c}0", "", "+",
        ] {
            assert_eq!(parse_grid_integer(s), None, "{s:?}");
        }
        assert!(parse_grid_integer(&"0".repeat(4300)).is_some());
        assert!(parse_grid_integer(&"0".repeat(4301)).is_none());
        assert_eq!(parse_integer("0.00").unwrap(), BigInt::from(0));
    }
}
