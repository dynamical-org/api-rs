use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisBatch {
    pub queries: Vec<AnalysisQuery>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisQuery {
    pub query_id: Option<String>,
    pub data_product_id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub variables: Vec<String>,
    pub start_time: DateTime,
    pub end_time: DateTime,
}
pub fn validate_analysis(
    body: &[u8],
    content_type: Option<&str>,
) -> Result<AnalysisBatch, Response> {
    validate_batch(body, content_type, Validator::analysis_query)
        .map(|queries| AnalysisBatch { queries })
}
impl Validator {
    fn analysis_query(&mut self, input: &Value, loc: &[Value]) -> Option<AnalysisQuery> {
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
        let (raw, p, key) = self.field(input, "startTime", "start_time", true, loc);
        used.push(key);
        let start_time = raw.and_then(|v| self.datetime(v, &p));
        let (raw, p, key) = self.field(input, "endTime", "end_time", true, loc);
        used.push(key);
        let end_time = raw.and_then(|v| self.datetime(v, &p));
        self.extras(
            pairs,
            &used.iter().map(String::as_str).collect::<Vec<_>>(),
            loc,
        );
        if self.errors.len() != start {
            return None;
        }
        let query = AnalysisQuery {
            query_id,
            data_product_id: product?,
            latitude: latitude?,
            longitude: longitude?,
            variables,
            start_time: start_time?,
            end_time: end_time?,
        };
        if let Some(message) = model_error(&query) {
            self.push(
                "value_error",
                loc,
                &format!("Value error, {message}"),
                input,
                Some(Value::object([("error", Value::Object(vec![]))])),
            );
            return None;
        }
        Some(query)
    }
}
fn model_error(query: &AnalysisQuery) -> Option<&'static str> {
    if query.start_time.time.tz_offset.is_none() {
        return Some("startTime must include a timezone");
    }
    if query.end_time.time.tz_offset.is_none() {
        return Some("endTime must include a timezone");
    }
    let micros = |date: DateTime| {
        i128::from(date.timestamp_tz()) * 1_000_000 + i128::from(date.time.microsecond)
    };
    let start = micros(query.start_time);
    let end = micros(query.end_time);
    if start >= end {
        return Some("startTime must be before endTime");
    }
    if end - start > i128::from(MAX_ANALYSIS_RANGE_SECONDS) * 1_000_000 {
        return Some("Analysis queries may span at most 31 days");
    }
    None
}
