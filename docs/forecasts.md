# Forecast, analysis and canonical point serving

`dynapi-server` serves forecast and analysis batches, forecast run listing, and
canonical point links for materialized rectilinear products. Deterministic and
ensemble data responses use the portable query engine's ordered JSON and SHA-256
ETags. Native HRRR Lambert conformal and HRDPS rotated-pole grids use the same
operation for geographic selection and canonical locations, preserving stored
y/x indices. Projected selection remains unsupported on WASM. Virtual GRIB and representation routing remain later work.

`POST /v1/analyses` accepts 1–10 queries, each with a product, location, 1–25
variables and required timezone-aware `startTime`/`endTime`. The UTC elapsed
window must be positive and at most 31 days. Selection includes start and
excludes end; result times preserve source order. Responses contain analysis
times, units, snapshot and canonical/catalog links, plus data or ensemble members.
Static variables keep their own dimension cardinality. The shared response/read
budgets apply atomically across a batch before loading, as for forecasts.

`GET /v1/forecasts/runs` requires `dataProductId`, accepts independent strict
`initTimesAfter`/`initTimesBefore` bounds, and returns newest-first unique init
times with the opened snapshot. Limit defaults to 20 and accepts 1–100. It reads
metadata coordinates without weather selection or data-query budget charges;
point-grid eligibility does not restrict discovery. Successful responses use
mutable caching and intentionally have no ETag, Server-Timing, dataset-cache or
work-unit response headers. Catalog/type checks precede timezone/order checks;
non-budget dataset-open failures retain the reference's generic 500 behavior.
Run listing opens stored coordinate metadata with all virtual containers disabled,
in a separate cache profile. Virtual forecast products can list runs when their
coordinates are stored in the repository; virtual coordinates fail closed without
fetching an external chunk. Virtual forecast point serving retains generic 500
until virtual GRIB support is implemented; its container authorization stays
separate from run discovery.

Eight materialized analysis products are readable: GFS, GEFS, HRRR, hourly CONUS
MRMS, both CHIRPS variants and both IMERG variants. Historical GEFS frames use
the admitted Blosc header typesize (D33). IMERG integer-epoch coordinates use
bounded staging within the unchanged 64 MiB metadata budget. The three virtual
analysis products await virtual GRIB support and return generic 500 `Internal
Server Error` before repository I/O, consistently with virtual forecast point
serving. Only a typed metadata-limit failure still exceeding the cap maps to 422
in the shared open-error mapper; unrelated metadata/storage failures keep their
existing classifications (D40).

The native catalog service warms at process startup and shares its refresh,
freshness and failure-backoff state across requests. Schema validation precedes
compute admission, then catalog lookup and caller-location checks finish before
dataset opens. Each worker retains that exact catalog generation, opens pinned
dataset views, selects points/runs/windows, and charges response values in input
order.
Only after every selection succeeds does it charge the aggregate read budget
and load data. The worker owns its compute permit until reads and encoding finish,
even if the HTTP client disconnects. Caller-location policy overrides response
cache headers after a gated product is identified.

| Environment variable | Default |
| --- | --- |
| `DYNAMICAL_API_STAC_URL` | `https://stac.dynamical.org/catalog.json` |
| `DYNAMICAL_API_COMPUTE_SLOTS` | `6` |
| `DYNAMICAL_API_BUSY_RETRY_AFTER_SECONDS` | `5` |
| `DYNAMICAL_API_FAIR_SHARE` | `1` (`0` disables) |
| `DYNAMICAL_API_FAIR_SHARE_BUSY_SLOTS` | compute slots minus two |
| `DYNAMICAL_API_FAIR_SHARE_CALLER_SLOTS` | `2` |
| `DYNAMICAL_API_FAIR_SHARE_RETRY_AFTER_SECONDS` | `5` |
| `DYNAMICAL_API_DATASET_CACHE_ENTRIES` | `32` |
| `DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS` | `30` |
| `DYNAMICAL_API_MAX_RESPONSE_VALUES` | `100000` |
| `DYNAMICAL_API_MAX_READ_VALUES` | `450000000` |

Fair share uses the rate limiter's caller key across all four data routes. Once
busy slots are in use, a caller already holding the caller threshold receives
429 before the capacity 503. The decision and acquisition share one lock; the
existing compute permit retains both counts until physical work finishes,
including after disconnect once the dataset worker starts. Cancellation during
the preceding async catalog or caller-location check releases the slot and
caller count immediately; any detached location flight retains its separate
bounded lifetime ([D42](../DEVIATIONS.md#d42--cancellation-before-the-dataset-worker-starts)). Origin-authenticated requests without a nonempty
`CF-Connecting-IP` are unidentified and use capacity admission only. Without
origin authentication, the socket host identifies the caller.

With fewer than four compute slots, the default rule is inert unless either
threshold is explicitly set. Empty threshold settings count as unset. Thresholds
and retry delay must be positive; busy slots cannot exceed capacity. Caller slots
may exceed either busy slots or capacity. Disabling the rule still validates its
settings and tracks identified callers for heartbeat occupancy. The heartbeat's
`maxCallerSlots` is the current maximum; refusal counters are cumulative.

Cached metadata and key storage have a fixed 256 MiB budget. Unique active
metadata in one batch also has a 256 MiB admission budget; a currently opening
dataset has its independent 64 MiB adapter limit. Response selection,
presentation and encoding share a conservative 64 MiB charge. Selection reserves
bytes before copying options/labels and before growing selected time/index
vectors, cumulatively across queries and runs. Very large labels or selections
can therefore receive the byte-budget 422 before a later selection/value error;
ordinary requests keep the selection/response-value/read-value error order.
An additional allocation-free encoding preflight precedes serialization.
See D13–D15 in [DEVIATIONS.md](../DEVIATIONS.md)
for accounting, upstream allocation exclusions and physical loading differences.
The 16 MiB HTTP request-body cap and existing reader decode/scratch limits remain.

A main cache entry expires after its configured TTL and is reused only after
revalidating that the branch still resolves to the same immutable snapshot.
Pinned entries have no TTL; all entries share an LRU bounded by count and bytes.
`X-Dynamical-Dataset-Cache` reports sorted unique `hit`, `miss` or `revalidated`
statuses for the request. Canonical GET accepts immutable snapshot IDs and uses
immutable caching for products without a caller-location check. Invalid snapshot
IDs return 404 with instructions to use a response's `snapshotId`. Forecast POST
uses mutable caching and accepts
`representation` values `auto` and `requested`, with routing off for both.

Successes include `Server-Timing` phases in reference order,
`X-Dynamical-Work-Units`, ETag and Cache-Control. Query errors return JSON 422;
known transient virtual-chunk errors return 503 with `Retry-After: 5`.
Catalog failures return their public 503 detail, unknown products return 404,
and other failures return generic 500 text. Internal diagnostics stay out of
response bodies. Usage records include products, query count, work units,
cache status through the analytics sink. Terminal success records also include
response bytes and timings.

Offline tests require the `dynapi-server/harness` feature. Its store substitution
and typed reader fault hooks support synthetic fixtures; normal builds omit them.
No Python tooling or reference-service output is part of the test suite.

## Caller-location checks

A product with any `http` virtual chunk container whose hostname is
`wn.dynamical.org` requires a caller-location check. Product IDs do not select the
policy. Forecast and analysis POST, run listing and canonical GET acquire a
compute slot before catalog resolution and caller-location checks. Batches check
every known gated product before opening any dataset. Allowed requests then
serve data or list runs using the exact catalog generation checked.

Set `DYNAMICAL_API_GEO_CHECK_SECRET` to enable checks. The service sends it as
`X-Geo-Check-Secret` to `https://wn.dynamical.org/geo-check`. Viewer country and
region are forwarded only when both `X-Dynamical-Origin-Auth` and
`X-Dynamical-Viewer-Auth` match the configured origin secret. Empty fields and
untrusted viewer headers are omitted. Location never comes from query parameters
or client IP inference.

A 204 allows the request. A 451 returns the proxy's trimmed text in the JSON
`detail` field, with a fixed fallback for empty text. Missing configuration,
transport failures, redirects, other statuses or an oversized refusal return 503
with `Retry-After: 5`. The client never follows redirects, and a check, including
its body, has a five-second deadline. Refusal bodies are limited to 16 KiB.

204 and 451 decisions are cached for 300 seconds by country and region; failures
are never cached. The cache holds at most 1024 decisions and 4 MiB of charged key,
detail and entry storage. At most 16 distinct checks may run at once. Identical
checks share one task; disconnecting callers leave that bounded task running only
until its deadline. Capacity exhaustion fails closed with retryable 503.

Once a gated product is identified, every later response uses
`Cache-Control: private, no-store`, including canonical responses and errors.
Data responses retain ETags; run listing has no ETag. Earlier authentication,
rate and schema failures
retain their existing policy. See D36 through D39 in
[DEVIATIONS.md](../DEVIATIONS.md).
