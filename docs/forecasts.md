# Forecast and canonical point serving

`dynapi-server` serves forecast batches and canonical point links for materialized
rectilinear products. Both deterministic and ensemble values use the portable
query engine's ordered JSON and SHA-256 ETags. Analysis data can be read through
canonical GET; `POST /v1/analyses` remains a stub. Projected transformations,
virtual GRIB, representation routing, discovery and run listing remain later work.

The native catalog service warms at process startup and shares its refresh,
freshness and failure-backoff state across requests. Schema validation precedes
compute admission. Each admitted worker resolves products, opens pinned dataset
views, selects points/runs/windows, and charges response values in input order.
Only after every selection succeeds does it charge the aggregate read budget
and load data. The worker owns its compute permit until reads and encoding finish,
even if the HTTP client disconnects. Product policy has an insertion point before
admission/store reads; successful response cache policy is set centrally by the
encoder, allowing later per-response policy overrides.

| Environment variable | Default |
| --- | --- |
| `DYNAMICAL_API_STAC_URL` | `https://stac.dynamical.org/catalog.json` |
| `DYNAMICAL_API_COMPUTE_SLOTS` | `6` |
| `DYNAMICAL_API_BUSY_RETRY_AFTER_SECONDS` | `5` |
| `DYNAMICAL_API_DATASET_CACHE_ENTRIES` | `32` |
| `DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS` | `30` |
| `DYNAMICAL_API_MAX_RESPONSE_VALUES` | `100000` |
| `DYNAMICAL_API_MAX_READ_VALUES` | `450000000` |

Cached metadata and key storage have a fixed 256 MiB budget. Unique active
metadata in one batch also has a 256 MiB admission budget; a currently opening
dataset has its independent 64 MiB adapter limit. Response selection,
presentation and encoding have a conservative 64 MiB budget, with an additional
allocation-free encoding preflight. See D13–D15 in [DEVIATIONS.md](../DEVIATIONS.md)
for accounting, upstream allocation exclusions and physical loading differences.
The 16 MiB HTTP request-body cap and existing reader decode/scratch limits remain.

A main cache entry expires after its configured TTL and is reused only after
revalidating that the branch still resolves to the same immutable snapshot.
Pinned entries have no TTL; all entries share an LRU bounded by count and bytes.
`X-Dynamical-Dataset-Cache` reports sorted unique `hit`, `miss` or `revalidated`
statuses for the request. Canonical GET accepts immutable snapshot IDs and uses
immutable caching. Invalid snapshot IDs return 404 with instructions to use a
response's `snapshotId`. Forecast POST uses mutable caching and accepts
`representation` values `auto` and `requested`, with routing off for both.

Successes include `Server-Timing` phases in reference order,
`X-Dynamical-Work-Units`, ETag and Cache-Control. Query errors return JSON 422;
known transient virtual-chunk errors return 503 with `Retry-After: 5`.
Catalog failures return their public 503 detail, unknown products return 404,
and other failures return generic 500 text. Internal diagnostics stay out of
response bodies. Usage records include products, query count, work units,
response bytes, cache status and timings through the existing sink boundaries.

Offline tests require the `dynapi-server/harness` feature. Its store substitution
and typed reader fault hooks support synthetic fixtures; normal builds omit them.
No Python tooling or reference-service output is part of the test suite.
