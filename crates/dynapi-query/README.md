# dynapi-query

Pure forecast/analysis point-query engine over an already-opened `DatasetView`.
This crate is outside the production server path. It has no dependency on the
reader, HTTP framework, store, runtime, environment, or C2 routing implementation.

The server adapter supplies consistent dimension lengths, one-dimensional
coordinate arrays, variable dimensions/attributes, and inner chunk shapes.
Preserve decoded numeric dtypes: `Coordinate::Float32`, `Numeric` (float64),
and `Int64` match the reader's numeric buffers. Nearest selection performs
NumPy 2's float32 operations in float32, including each circular-distance step;
grid bounds use float64. Integer arrays promote to float64 for float queries.
`Coordinate::Time` and `Duration` contain CF-decoded nanoseconds, including
`i64::MIN` for NaT, matching `dynapi-reader::cf_time::decode`. Member coordinates
use contract `Value`s to preserve integer, float and string identities. Explicit
chunk tuples override preferred chunks; absent axes are charged at full size.
Run-selection/listing results use `DatetimeNanoseconds` (i128), separately from
raw i64 coordinates. Labels reproduce Python's correctly rounded integer-rational
floating seconds, then datetime microsecond rounding. `format_datetime` formats
those labels, including valid coordinate extrema.
`QueryInput`, `QueryOptions` and `InitTimeSelection` retain contract civil
`RequestDatetime` values; lead limits retain `BigInt`. Use
`QueryInput::from_forecast` to transfer a validated `dynapi-contract::ForecastQuery`
without narrowing. Canonical URLs format the original request dates in UTC and
the original integer limit. Selection separately normalizes dates to UTC and
wraps into NumPy's i64 nanosecond domain. Thus a 2600–2620 window compares as
2015–2035 while retaining 2600–2620 in its URL. Lead comparison reproduces NumPy's
float conversion, including Python's internal `OverflowError` when an integer
cannot convert to a finite float. UTC normalization outside years 1–9999 also
reproduces Python's internal overflow. Batch window options normalize to UTC
when each run's options are built, after point/run selection and before variable
validation. Canonical selectors retain their own later bound-conversion order.
Forecast run-plus-lead arithmetic is checked across the entire lead array before
bounds, selection masks, budget charges or reads. Existing NaT propagates;
finite overflow or a finite sum equal to the NaT sentinel raises `OverflowError`.
Lead-limit conversion precedes this addition. Analysis compares wrapped civil
bounds directly to decoded time coordinates; it performs no run-plus-lead sum.
An empty longitude axis raises the earlier internal `ValueError` from longitude
normalization; a nonempty all-NaN axis remains a 422 grid error.
For x/y grids, the adapter classifies `spatial_ref`: geographic grids use x/y as
longitude/latitude; missing CRS is a query error; projected CRS is explicitly
unsupported (500) until projected-grid support is implemented.

The server validates request schemas, resolves catalog products/snapshots, and
constructs `QueryInput`s. `prepare_batch` selects each query/run and charges
response values, then admits the whole batch's nominal read values. It cannot
read data. A rejected batch never reaches `DataReader`. Response charges and
read admission preserve Python's ordering; co-tile savings never discount the
read-budget charge. `PreparedBatch::load_plan()` exposes ordered selection
indices, grouping statistics, and grouped read estimates. Source identity is the
address of the same borrowed view: separately opened views do not group merely
because their metadata matches.
Spatial and temporal selection use the dimensions/coordinates retained by the
requested variable subset. A missing spatial dimension raises Python's internal
`ValueError` before temporal selection, budget charges or reads. The subset as a
whole must retain each spatial dimension; individual variables may omit axes
retained by another requested variable and still broadcast. Static-only subsets
retaining both spatial axes reproduce Python's internal temporal `KeyError`.
Batch initialization-run selection still precedes per-run subset selection,
matching Python's routing helpers.

`PreparedBatch::execute` accepts a `DataReader` implementation and returns exact
JSON bytes plus response headers. Reads return flattened, selected data in
variable dimension order; index selectors drop axes, index arrays retain axes,
order, and duplicates. Extra selectors for dimensions absent from a variable
must be ignored. Preserve integers as `Value::Int`; floating nonfinites become
null in data. Only known transient virtual-chunk diagnostics should use
`QueryError::from_storage_failure(true, message)`; other failures remain 500s.
Retryable errors carry `retry_after = Some(5)` and the exact UTF-8 error envelope.

The fallback executor reads each selection individually in planned group order
and restores request order. It does not claim physical co-tile savings: the plan
is available to a future vectorizing executor. No store opening occurs here.
The executor supplies measured `Server-Timing` text and dataset cache statuses
through `ResponseMetadata`; deterministic headers, SHA-256 ETag, byte length,
cache policy and work units are computed here. Error statuses and retry metadata
are for the HTTP adapter; ordinary exception envelopes use `QueryError::body`.
`QueryError::exception_class` records the corresponding Python internal exception
class for parity checks. The adapter returns generic 500 text for these failures;
internal diagnostic ordering/text is not part of its response contract.

For canonical GET handling, compose `point_from_indices`, `select_query`, read
admission (`budget.consume_read(selected.read_values())`), `load_query`,
`canonical_result`, and `encode_response`. These low-level functions deliberately
expose Python's selection/load boundary; callers must check the read budget
before loading. Set `ResponseMetadata::cache_policy` to `CachePolicy::Immutable`
for canonical GET (`public, max-age=31536000, immutable`). Batch responses use
the default `CachePolicy::Mutable` (`public, max-age=30, stale-while-revalidate=120`).
Raw query results and canonical/batch presentation preserve
Python object insertion order and use `dynapi-contract`'s JSON writer.

Run `cargo test --locked -p dynapi-query` from the repository root. Tests construct
small dataset/request inputs and own explicit values, links, errors and ordering
invariants; see [fixture policy](tests/fixtures/README.md) for current coverage.
They never execute Python or access a store/network. Maintainers verify migration
output against the reference Python service outside this repository.
