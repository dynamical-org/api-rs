# Opened dataset adapter

Native store adapter between `dynapi-reader` and `dynapi-query`. It opens one
repository and one readonly session, resolves a branch once, and keeps all reads
on that immutable snapshot. No HTTP, catalog resolution, cache or TTL lives here.

`OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &containers)`
returns an `Arc<OpenedDataset>`. An explicit `DatasetVersion::Snapshot(id)` checks
that exact immutable ID; malformed/absent IDs produce `OpenError::UnknownSnapshot`
with Python's `Unknown snapshot <repr>` message. Missing branches retain Icechunk's
primary `ref not found` diagnostic and remain internal failures. Storage failures
are never misreported as unknown snapshots.

The view retains all variable metadata, dimensions, decoded attributes, inner
chunks and preferred chunks. Only dimension coordinates used by the query engine are read eagerly, with
float32, float64, CF time/duration and ensemble identities preserved. Signed
int8/16/32/64 and unsigned int8/16/32 widen exactly to contract integers (ordinary
integer coordinates use Int64). Dataset-mode integer Blosc shuffle widths may
differ from the element width, as in the live int16 members with typesize 4;
payload headers must still match the admitted codec metadata. One-byte integers
accept Python Zarr's bytes codec with omitted or null endian; multibyte types
still require explicit little endian. Unused axes,
including REFS string `statistic`, and auxiliary coordinates such as `valid_time`
keep lazy data. Existing references in variable `coordinates` attributes promote
coordinates at dataset scope without a dimension-subset condition. All arrays
retain metadata; `Variable::is_coordinate` excludes coordinates from data queries.
Missing same-name spatial coordinate arrays get a charged virtual integer range;
stored invalid coordinate values never fall back to a virtual index.
CF-consumed `coordinates`, `_FillValue`, and temporal units/calendar/dtype move out
of variable attributes as they do in default xarray decoding. Dataset attributes
retain non-string global `coordinates` values and their ordinary JSON numeric identities, including large integers, under
serde_json's unified `arbitrary_precision` and `preserve_order` features.

The shared reader session admits ordinary descriptive coordinate metadata without
relaxing the original reader entrypoints' pinned-coordinate attestation. Numeric
codec, payload, chunk, selector and output bounds are unchanged. Unsupported CF
value transforms and codecs return errors before invoking their decoder. The
supported time encodings are the reader's attested epoch seconds and float seconds
with the timedelta dtype hint; unfamiliar encodings fail explicitly.

For x/y grids, CRS classification follows Python's `crs_wkt`/`spatial_ref` precedence
on coordinates. Exact HRRR Lambert conformal and HRDPS rotated-pole operations
are selected before the five geographic identity WKT inputs. Other nonempty
values remain unsupported projected CRSs; missing CRS remains distinct. Both
forecast selection and canonical reconstruction use the same native operation
and stored y/x indices. Projected axes require float64 storage and decoded
values, dimension-coordinate membership, and the validated axis contract in D31.
Supplied units must be metres for HRRR or `degrees` for HRDPS; missing units use
the exact WKT's native units. Projected selection fails closed on WASM (D30).

## Async/sync execution and admission

`dynapi-query::DataReader` is synchronous. Execute the complete request inside
`run_with_permit(owned_permit, move |handle| { ... })`, constructing and borrowing
`QueryInput`/`PreparedBatch` inside that closure. Obtain each dataset reader with
`opened.reader(handle.clone())`, and call the ordinary `batch.execute(...)`.
The closure owns `Arc`s of opened datasets and collections. Each admitted job runs on a dedicated OS thread, bounded by the caller's compute
slots, and its explicit runtime handle drives asynchronous store reads. These
threads leave Tokio's blocking pool available for local storage and manifest work;
no blocking-pool headroom assumption is required. Worker panics (including runtime
shutdown) are contained and returned as internal errors.
Do not call the synchronous reader directly from an async task.

The permit can be a Tokio `OwnedSemaphorePermit` or the service policy's
owned RAII permit. It moves into the compute job, which releases it after the
closure returns or unwinds. Dropping/aborting the caller's future leaves capacity
held until reads finish. Cancellation is not simulated by releasing a slot while
background work continues. The service owns admission status/retry headers.

Reader selectors follow each variable's source dimension order. Extra positions
are ignored; omitted positions select the full axis; scalar positions remove axes;
index lists preserve Cartesian order and duplicates. Integer values stay contract
`Value::Int`, floats retain nonfinites for the query engine to serialize as null.
Only typed Icechunk virtual-fetch errors with Python's known timeout/SlowDown
diagnostics map to retryable 503 with Retry-After 5. Other native failures retain their Python exception class where known (for example
`StorageError` or `InvalidInputError`); other reader failures stay internal.

## Byte bounds and verification

`MAX_METADATA_BYTES` is 64 MiB. `open_bounded` overrides it explicitly.
`metadata_bytes()` is a conservative deterministic byte charge, including staging,
not an RSS estimate. JSON is preflighted before its tree is allocated, and complete
coordinate/implicit-selector lengths are checked before their vectors are built.
Before any selector/group allocation, full-axis chunk cardinality must fit the
reader limit. Per eager coordinate of length N and G inner chunks, the charge is
N * (4 * stored_width + 128) + G * 512, plus member Value/BigInt storage and map
keys. This reserves simultaneous selector/group capacities and CF decode staging,
including the duration decoder's full-axis read. When that full read does not fit,
an attested int64 epoch dimension coordinate can use sequential batches of at most
262,144 selectors. Its checked reservation is `N * 8 + min(N, 262144) * 160 +
G * 512 + name.len() + 64`: exact retained i64 slots plus one reusable batch's
selector/CF staging and a conservative full-axis group allowance. Choose the plan
before charging or allocating. Planning is greedy per axis, in coordinate-name
order, against the remaining budget: a full read that fits keeps its original
charge. There is no global replanning of earlier axes if a later stored or implicit
axis exceeds the cap, even when batching an earlier axis could have made room.
Complete decoded axes remain eager and ordered; no regular spacing is inferred.
One 90-second deadline covers the entire axis.
Dropping its future releases the axis and in-flight selectors/buffers. Other
encodings keep the full-read plan; float-duration conversion still requires its
full-axis context. Existing output, chunk and selector cardinality bounds stay
independent. Cache and active-handle weights include the complete conservative
charge, even after transient batch staging is freed. Implicit indexes validate their
endpoint and allocate exactly N integer slots, charged at N * 8 bytes (plus
member Value/BigInt storage when applicable). Dimension, chunk and selector
vectors also reserve their exact element counts before fallible conversion.

The separate counting-allocator regression measures both 1,048,576
float64 duration axis and chunk-[1] variant at 34,000,000 bytes and 64 MiB, plus a
successful 256 MiB open. Its fixed 1 MiB margin covers the tiny fixture's upstream
snapshot/manifest/cache and runtime initialization, independent of axis length.
It also requires rejected opens to allocate less than that margin before returning.
Accepted implicit axes of 1,048,577 and 4,194,305 elements must retain exactly N
slots and keep retained/peak requested allocations below the charge and cap plus
a fixed 256 KiB upstream/runtime margin (less than 2.7% of the smaller 10 MB cap).
This is not a total-process cap: encoded/decompressed chunk scratch remains under
the reader's separate payload/chunk bounds, and upstream Icechunk snapshots,
manifests, transport and caches are excluded. D13 states these boundaries. A cache can use
this charge to limit a cache by bytes as well as entries.

Tests run offline in Rust with input fixtures and explicit Rust-owned
expectations. The [fixture recipes](../crates/dynapi-dataset/tests/fixtures/README.md)
describe stored shapes, values and codecs. Tests cover resource bounds, shared
sessions, cancellation, permits, integer admission, CRS policy and coordinate
membership. Verification against the reference service happens outside this
repository; broader coverage remains in the Rust-owned test coverage tracking
issue. No saved service responses are part of the test suite.
