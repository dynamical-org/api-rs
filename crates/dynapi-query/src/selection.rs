use crate::*;
use chrono::{DateTime, Utc};
use dynapi_contract::output::python_repr;
use num_traits::ToPrimitive;
use std::{borrow::Cow, collections::BTreeSet};

pub fn format_datetime(ns: DatetimeNanoseconds) -> String {
    let date = DateTime::<Utc>::from_timestamp(
        ns.div_euclid(1_000_000_000) as i64,
        ns.rem_euclid(1_000_000_000) as u32,
    )
    .expect("datetime within Python nanosecond-coordinate range");
    let mut out = date.format("%Y-%m-%dT%H:%M:%S").to_string();
    let micro = date.timestamp_subsec_micros();
    if micro != 0 {
        out.push_str(&format!(".{micro:06}"));
    }
    out.push('Z');
    out
}
fn civil_nanoseconds(date: RequestDatetime) -> i128 {
    i128::from(date.timestamp_tz()) * 1_000_000_000 + i128::from(date.time.microsecond) * 1_000
}
pub(crate) fn format_request_datetime(date: RequestDatetime) -> String {
    format_datetime(civil_nanoseconds(date))
}
pub(crate) fn datetime_from_label(ns: DatetimeNanoseconds) -> RequestDatetime {
    RequestDatetime::parse_str(&format_datetime(ns)).expect("decoded datetime label")
}
/// Python's routing helpers normalize options before validating variables.
/// Retain the full civil date here, independently of NumPy's ns conversion.
pub(crate) fn utc_datetime(date: RequestDatetime) -> Result<RequestDatetime, QueryError> {
    let seconds = date.timestamp_tz();
    if !(-62_135_596_800..=253_402_300_799).contains(&seconds) {
        return Err(QueryError::exception(
            "OverflowError",
            "date value out of range",
        ));
    }
    Ok(datetime_from_label(civil_nanoseconds(date)))
}
/// Casting civil dates to datetime64[ns] wraps modulo 2^64. Adding a finite
/// timedelta64[ns] instead raises on overflow, including the NaT sentinel.
fn numpy_datetime(date: RequestDatetime) -> Result<i64, QueryError> {
    Ok(civil_nanoseconds(utc_datetime(date)?) as i64)
}
fn valid_time(run: i64, lead: i64) -> Result<i64, QueryError> {
    if run == NAT || lead == NAT {
        return Ok(NAT);
    }
    run.checked_add(lead)
        .filter(|time| *time != NAT)
        .ok_or_else(|| {
            QueryError::exception(
                "OverflowError",
                "Overflow in datetime64 + timedelta64 addition",
            )
        })
}
/// Correctly rounded int/int true division, without first rounding the numerator.
/// Scaling the rational to a 53-bit significand fits in u128 for every i64 ns.
fn seconds_quotient(ns: i64) -> f64 {
    let n = u128::from(ns.unsigned_abs());
    if n == 0 {
        return 0.;
    }
    let denominator = 1_000_000_000u128;
    let mut exponent = (127 - n.leading_zeros()) as i32 - 29;
    let below = if exponent >= 0 {
        n < (denominator << exponent)
    } else {
        (n << -exponent) < denominator
    };
    if below {
        exponent -= 1;
    }
    let scaled = n << (52 - exponent);
    let mut significand = scaled / denominator;
    let remainder = scaled % denominator;
    if remainder * 2 > denominator || (remainder * 2 == denominator && significand % 2 == 1) {
        significand += 1;
    }
    // Addition also carries a rounded 2^53 significand into the next exponent.
    let bits = (((exponent + 1023) as u64) << 52) + (significand as u64 - (1u64 << 52));
    f64::from_bits(bits | (u64::from(ns < 0) << 63))
}
/// CPython datetime.fromtimestamp rounds fractional floating seconds to the
/// nearest microsecond (ties to even), separately from integer-rational division.
pub(crate) fn python_time(ns: i64) -> DatetimeNanoseconds {
    let seconds = seconds_quotient(ns);
    let integral = seconds.trunc() as i64;
    let micro = ((seconds - integral as f64) * 1e6).round_ties_even() as i64;
    integral as i128 * 1_000_000_000 + micro as i128 * 1_000
}
pub(crate) fn python_timestamp(ns: i64) -> String {
    format_datetime(python_time(ns))
}
pub(crate) fn coordinate<'a>(
    view: &'a DatasetView,
    name: &str,
) -> Result<&'a Coordinate, QueryError> {
    view.coordinates
        .get(name)
        .ok_or_else(|| QueryError::internal(format!("Dataset is missing its {name} coordinate")))
}
fn numeric<'a>(view: &'a DatasetView, name: &str) -> Result<Cow<'a, [f64]>, QueryError> {
    match coordinate(view, name)? {
        Coordinate::Numeric(xs) => Ok(Cow::Borrowed(xs)),
        Coordinate::Float32(xs) => Ok(Cow::Owned(xs.iter().map(|x| f64::from(*x)).collect())),
        Coordinate::Int64(xs) => Ok(Cow::Owned(xs.iter().map(|x| *x as f64).collect())),
        _ => Err(QueryError::internal(format!(
            "Expected numeric coordinate {name}"
        ))),
    }
}
pub(crate) fn temporal<'a>(view: &'a DatasetView, name: &str) -> Result<&'a [i64], QueryError> {
    match coordinate(view, name)? {
        Coordinate::Time(xs) | Coordinate::Duration(xs) => Ok(xs),
        _ => Err(QueryError::internal(format!(
            "Expected decoded time coordinate {name}"
        ))),
    }
}
fn spatial(view: &DatasetView) -> Result<(&str, &str), QueryError> {
    if view.dimensions.contains_key("latitude") && view.dimensions.contains_key("longitude") {
        return Ok(("latitude", "longitude"));
    }
    if view.dimensions.contains_key("y") && view.dimensions.contains_key("x") {
        match view.spatial_crs {
            Some(SpatialCrs::Geographic) => return Ok(("y", "x")),
            Some(SpatialCrs::Missing) => {
                return Err(QueryError::invalid("Projected grid is missing its CRS"))
            }
            Some(SpatialCrs::Projected) => {
                return Err(QueryError::internal(
                    "Projected-CRS grids are not yet supported",
                ))
            }
            None => {}
        }
    }
    Err(QueryError::invalid(
        "Dataset has an unsupported spatial grid",
    ))
}
fn finite_ordered(xs: &[f64]) -> Result<Vec<f64>, QueryError> {
    let mut xs: Vec<_> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return Err(QueryError::invalid(
            "Data product grid has no finite coordinates",
        ));
    }
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| *a == *b);
    Ok(xs)
}
fn step(xs: &[f64]) -> f64 {
    let mut steps: Vec<_> = xs.windows(2).map(|w| w[1] - w[0]).collect();
    steps.sort_by(f64::total_cmp);
    match steps.len() {
        0 => 0.,
        n if n % 2 == 1 => steps[n / 2],
        n => (steps[n / 2 - 1] + steps[n / 2]) / 2.,
    }
}
fn outside(value: f64, xs: &[f64], longitude: bool) -> Result<bool, QueryError> {
    let finite = finite_ordered(xs)?;
    let spacing = step(&finite);
    if longitude
        && xs.iter().filter(|v| v.is_finite()).count() > 1
        && finite[finite.len() - 1] - finite[0] + spacing >= 359.
    {
        return Ok(false);
    }
    Ok(value < finite[0] - spacing / 2. || value > finite[finite.len() - 1] + spacing / 2.)
}
fn modulo(value: f64) -> f64 {
    let rem = value % 360.;
    if rem < 0. {
        rem + 360.
    } else {
        rem
    }
}
// np.argmin chooses the first NaN, and the first index for ties.
fn argmin(xs: impl Iterator<Item = f64>) -> usize {
    let mut best = (0, f64::INFINITY);
    for (index, distance) in xs.enumerate() {
        if distance.is_nan() {
            return index;
        }
        if distance < best.1 {
            best = (index, distance);
        }
    }
    best.0
}
pub fn select_nearest_point(
    view: &DatasetView,
    latitude: f64,
    longitude: f64,
) -> Result<SelectedPoint, QueryError> {
    let (lat, lon) = spatial(view)?;
    let latitudes = numeric(view, lat)?;
    let longitudes = numeric(view, lon)?;
    if longitudes.is_empty() {
        return Err(QueryError::exception(
            "ValueError",
            "zero-size array to reduction operation fmin which has no identity",
        ));
    }
    let minimum = longitudes
        .iter()
        .copied()
        .filter(|v| !v.is_nan())
        .reduce(f64::min)
        .unwrap_or(f64::NAN);
    let normalized = if minimum >= 0. {
        modulo(longitude)
    } else {
        modulo(longitude + 180.) - 180.
    };
    if outside(latitude, &latitudes, false)? || outside(normalized, &longitudes, true)? {
        return Err(QueryError::invalid(
            "Location is outside the data product grid",
        ));
    }
    // NumPy 2 converts a Python float scalar to the array dtype before each
    // ufunc. Integer arrays instead promote to f64 when subtracting that float.
    let first = if let Coordinate::Float32(xs) = coordinate(view, lat)? {
        let latitude = latitude as f32;
        argmin(xs.iter().map(|x| f64::from((*x - latitude).abs())))
    } else {
        argmin(latitudes.iter().map(|x| (*x - latitude).abs()))
    };
    let second = if let Coordinate::Float32(xs) = coordinate(view, lon)? {
        let normalized = normalized as f32;
        argmin(xs.iter().map(|x| {
            let shifted = (*x - normalized) + 180f32;
            let rem = shifted % 360f32;
            let rem = if rem < 0. { rem + 360f32 } else { rem };
            f64::from((rem - 180f32).abs())
        }))
    } else {
        argmin(
            longitudes
                .iter()
                .map(|x| (modulo(*x - normalized + 180.) - 180.).abs()),
        )
    };
    Ok(SelectedPoint {
        latitude: latitudes[first],
        longitude: longitudes[second],
        indexers: vec![(lat.into(), first), (lon.into(), second)],
    })
}
pub fn point_from_indices(
    view: &DatasetView,
    first: i64,
    second: i64,
) -> Result<SelectedPoint, QueryError> {
    // Python validates indices before looking up the CRS on x/y grids.
    let (lat, lon) =
        if view.dimensions.contains_key("latitude") && view.dimensions.contains_key("longitude") {
            ("latitude", "longitude")
        } else if view.dimensions.contains_key("y") && view.dimensions.contains_key("x") {
            ("y", "x")
        } else {
            return Err(QueryError::invalid(
                "Dataset has an unsupported spatial grid",
            ));
        };
    for (name, index) in [(lat, first), (lon, second)] {
        if usize::try_from(index)
            .ok()
            .is_none_or(|index| index >= view.dimensions[name])
        {
            return Err(QueryError::invalid(format!(
                "{name}_index is outside the grid"
            )));
        }
    }
    if lat == "y" {
        spatial(view)?;
    }
    let latitude = numeric(view, lat)?
        .get(first as usize)
        .copied()
        .ok_or_else(|| QueryError::internal("Coordinate size mismatch"))?;
    let longitude = numeric(view, lon)?
        .get(second as usize)
        .copied()
        .ok_or_else(|| QueryError::internal("Coordinate size mismatch"))?;
    Ok(SelectedPoint {
        latitude,
        longitude,
        indexers: vec![(lat.into(), first as usize), (lon.into(), second as usize)],
    })
}
#[derive(Clone, Debug)]
pub struct InitTimeSelection {
    pub exact: Option<RequestDatetime>,
    pub after: Option<RequestDatetime>,
    pub before: Option<RequestDatetime>,
    pub latest: bool,
    pub max_runs: usize,
}
impl Default for InitTimeSelection {
    fn default() -> Self {
        Self {
            exact: None,
            after: None,
            before: None,
            latest: false,
            max_runs: 10,
        }
    }
}
impl InitTimeSelection {
    pub fn latest() -> Self {
        Self {
            latest: true,
            max_runs: 10,
            ..Self::default()
        }
    }
}
pub fn select_forecast_init_times(
    view: &DatasetView,
    selection: &InitTimeSelection,
) -> Result<Vec<DatetimeNanoseconds>, QueryError> {
    let times = temporal(view, "init_time")?;
    if let Some(exact) = selection.exact {
        let wrapped = numpy_datetime(exact)?;
        if !times.contains(&wrapped) || wrapped == NAT {
            return Err(QueryError::invalid(format!(
                "initTime {} is not present in this snapshot",
                format_request_datetime(exact)
            )));
        }
        return Ok(vec![python_time(wrapped)]);
    }
    if selection.latest {
        let latest = times
            .iter()
            .copied()
            .filter(|x| *x != NAT)
            .max()
            .ok_or_else(|| {
                QueryError::invalid("Forecast data product has no initialization times")
            })?;
        return Ok(vec![python_time(latest)]);
    }
    let (Some(after), Some(before)) = (selection.after, selection.before) else {
        return Err(QueryError::invalid(
            "Forecast queries require initTime or initialization-time bounds",
        ));
    };
    let after = numpy_datetime(after)?;
    let before = numpy_datetime(before)?;
    let times: Vec<_> = times
        .iter()
        .copied()
        .filter(|t| *t != NAT && after != NAT && before != NAT && *t > after && *t < before)
        .collect();
    if times.is_empty() {
        return Err(QueryError::invalid(
            "No forecast runs match the requested initialization-time bounds",
        ));
    }
    if times.len() > selection.max_runs {
        return Err(QueryError::invalid(format!(
            "Initialization-time bounds match {} runs; at most {} are allowed",
            times.len(),
            selection.max_runs
        )));
    }
    Ok(times.into_iter().map(python_time).collect())
}
pub fn list_forecast_init_times(
    view: &DatasetView,
    after: Option<RequestDatetime>,
    before: Option<RequestDatetime>,
    limit: usize,
) -> Result<Vec<DatetimeNanoseconds>, QueryError> {
    if !view.coordinates.contains_key("init_time") {
        return Err(QueryError::invalid(
            "Forecast data product is missing its init_time coordinate",
        ));
    }
    let after = after.map(numpy_datetime).transpose()?;
    let before = before.map(numpy_datetime).transpose()?;
    Ok(temporal(view, "init_time")?
        .iter()
        .copied()
        .filter(|t| {
            *t != NAT
                && after.is_none_or(|a| a != NAT && *t > a)
                && before.is_none_or(|b| b != NAT && *t < b)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .take(limit)
        .map(python_time)
        .collect())
}
#[derive(Clone, Debug)]
pub struct QueryBudget {
    pub max_values: u64,
    pub used_values: u64,
    pub max_read_values: u64,
    pub used_read_values: u64,
}
impl Default for QueryBudget {
    fn default() -> Self {
        Self {
            max_values: 100_000,
            used_values: 0,
            max_read_values: 450_000_000,
            used_read_values: 0,
        }
    }
}
impl QueryBudget {
    pub fn consume(&mut self, values: u64) -> Result<(), QueryError> {
        let total = self.used_values.checked_add(values);
        if total.is_none_or(|t| t > self.max_values) {
            return Err(QueryError::invalid(format!(
                "Query exceeds the {}-value response-value budget",
                self.max_values
            )));
        }
        self.used_values = total.unwrap();
        Ok(())
    }
    pub fn consume_read(&mut self, values: u64) -> Result<(), QueryError> {
        let total = self.used_read_values as u128 + values as u128;
        if total > self.max_read_values as u128 {
            return Err(QueryError::invalid(format!("Batch total {total} exceeds the {}-value read-value budget (DYNAMICAL_API_MAX_READ_VALUES); reduce variables, times, runs, or batch size",self.max_read_values)));
        }
        self.used_read_values = total as u64;
        Ok(())
    }
}
#[derive(Debug)]
pub struct PreparedQuery<'a> {
    pub(crate) source: &'a DatasetView,
    pub(crate) collection: &'a Collection,
    pub(crate) snapshot_id: String,
    pub(crate) point: SelectedPoint,
    pub(crate) options: QueryOptions,
    pub(crate) positions: Positions,
    pub(crate) times: Vec<String>,
    pub(crate) lead_hours: Option<Vec<f64>>,
    pub(crate) value_count: u64,
    pub(crate) read_values: u64,
}
impl<'a> PreparedQuery<'a> {
    pub fn source(&self) -> &'a DatasetView {
        self.source
    }
    pub fn positions(&self) -> &Positions {
        &self.positions
    }
    pub fn point(&self) -> &SelectedPoint {
        &self.point
    }
    pub fn options(&self) -> &QueryOptions {
        &self.options
    }
    pub fn read_values(&self) -> u64 {
        self.read_values
    }
    pub fn value_count(&self) -> u64 {
        self.value_count
    }
    /// Conservative charge for selected trees, temporary values and JSON encoding.
    /// Source coordinate axes remain shared metadata, not per-response copies.
    pub fn materialization_byte_bound(&self) -> Option<u128> {
        let text = |s: &str| (s.len() as u128).checked_mul(24)?.checked_add(256);
        let value = |v: &Value| {
            (dynapi_contract::output::json_byte_bound(v)? as u128)
                .checked_mul(8)?
                .checked_add(256)
        };
        let mut bytes = (self.value_count as u128)
            .checked_mul(512)?
            .checked_add(4096)?;
        for s in [&self.collection.id, &self.snapshot_id]
            .into_iter()
            .chain(self.collection.version.iter())
            .chain(self.collection.catalog_url.iter())
        {
            bytes = bytes.checked_add(text(s)?)?;
        }
        for name in &self.options.variables {
            bytes = bytes.checked_add(text(name)?)?;
            if let Some(unit) = &self.collection.variables[name].unit {
                bytes = bytes.checked_add(text(unit)?)?;
            } else if let Some(unit) = self.source.variables[name].attributes.get("units") {
                bytes = bytes.checked_add(value(unit)?)?;
            }
        }
        for time in &self.times {
            bytes = bytes.checked_add(text(time)?)?;
        }
        if self.options.variables.iter().any(|n| {
            self.source.variables[n]
                .dimensions
                .iter()
                .any(|d| d == "ensemble_member")
        }) {
            bytes = bytes.checked_add(
                (self.source.dimensions["ensemble_member"] as u128)
                    .checked_mul(512 + self.options.variables.len() as u128 * 256)?,
            )?;
            if let Some(Coordinate::Members(ids)) = self.source.coordinates.get("ensemble_member") {
                for id in ids {
                    bytes = bytes.checked_add(value(id)?)?;
                }
            }
        }
        Some(bytes)
    }
}
fn checked_mul(a: u64, b: u64) -> Result<u64, QueryError> {
    a.checked_mul(b)
        .ok_or_else(|| QueryError::internal("Dataset value count overflow"))
}
pub(crate) fn checked_sum(mut xs: impl Iterator<Item = u64>) -> Result<u64, QueryError> {
    xs.try_fold(0u64, |a, b| {
        a.checked_add(b)
            .ok_or_else(|| QueryError::internal("Dataset value count overflow"))
    })
}
pub(crate) fn variable_size(
    view: &DatasetView,
    var: &Variable,
    positions: &Positions,
) -> Result<u64, QueryError> {
    var.dimensions.iter().try_fold(1, |size, d| {
        let count = match positions.get(d) {
            Some(Selection::Index(_)) => 1,
            Some(Selection::Indices(xs)) => xs.len(),
            None => *view
                .dimensions
                .get(d)
                .ok_or_else(|| QueryError::internal("Variable dimension missing from view"))?,
        };
        checked_mul(size, count as u64)
    })
}
pub(crate) fn chunk(var: &Variable, axis: usize, dim: &str) -> Option<i64> {
    match &var.chunks {
        Some(chunks) => chunks.get(axis).copied().flatten(),
        None => var.preferred_chunks.get(dim).copied(),
    }
}
fn read_values(
    view: &DatasetView,
    variables: &[String],
    positions: &Positions,
) -> Result<u64, QueryError> {
    let counts = variables
        .iter()
        .map(|name| {
            let var = &view.variables[name];
            var.dimensions
                .iter()
                .enumerate()
                .try_fold(1, |values, (axis, dim)| {
                    let size = view.dimensions[dim] as u64;
                    let Some(chunk) = chunk(var, axis, dim) else {
                        return checked_mul(values, size);
                    };
                    if chunk <= 0 {
                        return Err(QueryError::invalid(format!(
                            "Invalid chunk size for {} dimension {}",
                            python_repr(name),
                            python_repr(dim)
                        )));
                    }
                    let chunk = chunk as u64;
                    let touched = match positions.get(dim) {
                        None => size.div_ceil(chunk),
                        Some(Selection::Index(_)) => 1,
                        Some(Selection::Indices(xs)) => xs
                            .iter()
                            .map(|x| *x as u64 / chunk)
                            .collect::<BTreeSet<_>>()
                            .len() as u64,
                    };
                    checked_mul(values, checked_mul(touched, chunk)?)
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    checked_sum(counts.into_iter())
}
pub fn select_query<'a>(
    view: &'a DatasetView,
    collection: &'a Collection,
    point: SelectedPoint,
    options: QueryOptions,
    snapshot_id: &str,
    budget: Option<&mut QueryBudget>,
) -> Result<PreparedQuery<'a>, QueryError> {
    let unknown: Vec<_> = options
        .variables
        .iter()
        .filter(|v| !collection.variables.contains_key(*v))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(QueryError::invalid(format!(
            "Unknown variable(s): {}",
            unknown.join(", ")
        )));
    }
    let missing: Vec<_> = options
        .variables
        .iter()
        .filter(|v| !view.variables.get(*v).is_some_and(|var| !var.is_coordinate))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let supported: BTreeSet<_> = point
            .indexers
            .iter()
            .map(|(d, _)| d.as_str())
            .chain(["init_time", "lead_time", "time", "ensemble_member"])
            .collect();
        let unsupported: Vec<_> = missing
            .iter()
            .filter_map(|name| {
                let dims: Vec<_> = collection.variables[name]
                    .dimensions
                    .iter()
                    .filter(|d| !supported.contains(d.as_str()))
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if dims.is_empty() {
                    None
                } else {
                    Some(format!("{name} ({})", dims.join(", ")))
                }
            })
            .collect();
        return Err(QueryError::invalid(if unsupported.is_empty() {
            format!(
                "Variable(s) listed in the catalog are absent from snapshot {snapshot_id}: {}",
                missing.join(", ")
            )
        } else {
            format!(
                "Requested variables require unsupported dimensions: {}",
                unsupported.join(", ")
            )
        }));
    }
    let projected_dimensions: BTreeSet<_> = options
        .variables
        .iter()
        .flat_map(|name| view.variables[name].dimensions.iter().map(String::as_str))
        .collect();
    let missing_spatial: Vec<_> = point
        .indexers
        .iter()
        .filter(|(dim, _)| !projected_dimensions.contains(dim.as_str()))
        .map(|(dim, _)| python_repr(dim))
        .collect();
    if !missing_spatial.is_empty() {
        return Err(QueryError::exception(
            "ValueError",
            format!(
                "Dimensions {{{}}} do not exist in the requested subset",
                missing_spatial.join(", ")
            ),
        ));
    }
    let mut positions: Positions = point
        .indexers
        .iter()
        .map(|(d, i)| (d.clone(), Selection::Index(*i)))
        .collect();
    for (dim, index) in &point.indexers {
        if view.dimensions.get(dim).is_none_or(|n| index >= n) {
            return Err(QueryError::internal(
                "Point index is outside the dataset view",
            ));
        }
    }
    let projected_time = |name: &str| {
        if !projected_dimensions.contains(name) {
            let retained = options
                .variables
                .iter()
                .map(String::as_str)
                .chain(
                    view.coordinates
                        .keys()
                        .map(String::as_str)
                        .filter(|name| projected_dimensions.contains(name)),
                )
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(QueryError::exception(
                "KeyError",
                format!(
                    "\"No variable named {}. Variables on the dataset include [{}]\"",
                    python_repr(name),
                    retained
                ),
            ));
        }
        temporal(view, name)
    };
    let (times, lead_hours) = match collection.kind {
        Kind::Forecast => {
            let run = options
                .run
                .ok_or_else(|| QueryError::invalid("Forecast queries require a run"))?;
            let requested_run = numpy_datetime(run)?;
            let init = projected_time("init_time")?;
            let index = init
                .iter()
                .rposition(|x| *x == requested_run && *x != NAT)
                .ok_or_else(|| {
                    QueryError::invalid(format!(
                        "Run {} is not present in this snapshot",
                        format_request_datetime(run)
                    ))
                })?;
            let leads = projected_time("lead_time")?;
            let max_lead = options
                .max_lead_time_hours
                .as_ref()
                .map(|max| {
                    max.to_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                        QueryError::exception("OverflowError", "int too large to convert to float")
                    })
                })
                .transpose()?;
            // NumPy evaluates every sum before converting/comparing bounds or
            // applying the lead mask, even when no values can be admitted.
            let valid_times = leads
                .iter()
                .map(|lead| valid_time(requested_run, *lead))
                .collect::<Result<Vec<_>, _>>()?;
            let start = options.start.map(numpy_datetime).transpose()?;
            let end = options.end.map(numpy_datetime).transpose()?;
            let mut indices = Vec::new();
            let mut times = Vec::new();
            let mut hours = Vec::new();
            for (i, (lead, time)) in leads.iter().copied().zip(valid_times).enumerate() {
                let hour = if lead == NAT {
                    f64::NAN
                } else {
                    lead as f64 / HOUR as f64
                };
                if max_lead.is_some_and(|max| hour > max || hour.is_nan())
                    || start.is_some_and(|start| time == NAT || start == NAT || time < start)
                    || end.is_some_and(|end| time == NAT || end == NAT || time >= end)
                {
                    continue;
                }
                indices.push(i);
                times.push(python_timestamp(time));
                hours.push(hour);
            }
            if indices.is_empty() {
                return Err(QueryError::invalid(
                    "No forecast times match the requested bounds",
                ));
            }
            positions.insert("init_time".into(), Selection::Index(index));
            positions.insert("lead_time".into(), Selection::Indices(indices));
            (times, Some(hours))
        }
        Kind::Analysis => {
            let times = projected_time("time")?;
            let start = options.start.map(numpy_datetime).transpose()?;
            let end = options.end.map(numpy_datetime).transpose()?;
            let indices: Vec<_> = if start.is_none() && end.is_none() {
                vec![times.len().checked_sub(1).ok_or_else(|| {
                    QueryError::exception("IndexError", "Analysis time coordinate is empty")
                })?]
            } else {
                times
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        start.is_none_or(|s| s != NAT && **t != NAT && **t >= s)
                            && end.is_none_or(|e| e != NAT && **t != NAT && **t < e)
                    })
                    .map(|(i, _)| i)
                    .collect()
            };
            if indices.is_empty() {
                return Err(QueryError::invalid(
                    "No analysis times match the requested bounds",
                ));
            }
            let selected = indices
                .iter()
                .map(|i| python_timestamp(times[*i]))
                .collect();
            positions.insert("time".into(), Selection::Indices(indices));
            (selected, None)
        }
        Kind::Unknown => {
            return Err(QueryError::invalid(
                "Collection has unsupported time dimensions",
            ))
        }
    };
    let remaining: BTreeSet<_> = options
        .variables
        .iter()
        .flat_map(|v| &view.variables[v].dimensions)
        .filter(|d| {
            !matches!(positions.get(*d), Some(Selection::Index(_)))
                && !["ensemble_member", "lead_time", "time"].contains(&d.as_str())
        })
        .cloned()
        .collect();
    if !remaining.is_empty() {
        return Err(QueryError::invalid(format!(
            "Requested variables require unsupported dimensions: {}",
            remaining.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let sizes = options
        .variables
        .iter()
        .map(|v| variable_size(view, &view.variables[v], &positions))
        .collect::<Result<Vec<_>, _>>()?;
    let value_count = checked_sum(sizes.into_iter())?;
    let read_values = read_values(view, &options.variables, &positions)?;
    if let Some(budget) = budget {
        budget.consume(value_count)?;
    }
    Ok(PreparedQuery {
        source: view,
        collection,
        snapshot_id: snapshot_id.into(),
        point,
        options,
        positions,
        times,
        lead_hours,
        value_count,
        read_values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_addition_propagates_existing_nat_but_rejects_finite_sentinel() {
        for (run, lead) in [(NAT, i64::MAX), (i64::MAX, NAT), (NAT, NAT)] {
            assert_eq!(valid_time(run, lead).unwrap(), NAT);
        }
        for (run, lead) in [(i64::MAX, 1), (NAT + 1, -2), (-1000, NAT + 1000)] {
            let error = valid_time(run, lead).unwrap_err();
            assert_eq!(error.status, 500);
            assert_eq!(error.exception_class, Some("OverflowError"));
        }
        assert_eq!(valid_time(1000, i64::MAX - 1000).unwrap(), i64::MAX);
        assert_eq!(valid_time(-1000, NAT + 1001).unwrap(), NAT + 1);
    }
}
