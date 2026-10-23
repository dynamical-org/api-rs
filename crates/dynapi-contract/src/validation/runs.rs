use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct ForecastRunsQuery {
    pub data_product_id: String,
    pub init_times_after: Option<DateTime>,
    pub init_times_before: Option<DateTime>,
    pub limit: usize,
}
/// Route-level timezone/order checks intentionally follow product lookup.
pub fn validate_forecast_runs(params: &[(String, String)]) -> Result<ForecastRunsQuery, Response> {
    let mut validator = Validator { errors: vec![] };
    let get = |name: &str| {
        params
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| Value::from(value.clone()))
    };
    let loc = |name: &str| vec![Value::from("query"), Value::from(name)];
    let data_product_id = match get("dataProductId") {
        Some(value) => validator.string(
            &value,
            &loc("dataProductId"),
            Some(MIN_NONEMPTY_LENGTH),
            None,
        ),
        None => {
            validator.push(
                "missing",
                &loc("dataProductId"),
                "Field required",
                &Value::Null,
                None,
            );
            None
        }
    };
    let init_times_after =
        get("initTimesAfter").and_then(|value| validator.datetime(&value, &loc("initTimesAfter")));
    let init_times_before = get("initTimesBefore")
        .and_then(|value| validator.datetime(&value, &loc("initTimesBefore")));
    let limit = match get("limit") {
        Some(value) => validator
            .integer_bounded(
                &value,
                &loc("limit"),
                MIN_RUN_LIMIT as i64,
                Some(MAX_RUN_LIMIT as i64),
            )
            .and_then(|value| value.to_usize()),
        None => Some(DEFAULT_RUN_LIMIT),
    };
    if !validator.errors.is_empty() {
        return Err(Response::errors(validator.errors));
    }
    Ok(ForecastRunsQuery {
        data_product_id: data_product_id.unwrap(),
        init_times_after,
        init_times_before,
        limit: limit.unwrap(),
    })
}
