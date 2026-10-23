# Server test policy

Loopback application and raw-socket tests own the supported route, middleware,
header, body, telemetry and connection behavior using explicit cases and
invariants. Packaged robots bytes/metadata and documented temporary limitations
are tested directly. No Python is needed or used by the test suite.

`server.rs` exercises parser/framing behavior, deadlines, capacity, cancellation,
middleware order and injectable telemetry. It also checks robots GET/HEAD and
full-body Range policy, the asset mtime/size metadata rule, root redirects and
health responses. UUID4 format/uniqueness and HTTP Date are invariants rather than
fixed clocks or saved values. The `harness` feature enables panic-injection cases.

The wider route/method and middleware/error matrices remain follow-up coverage.
`forecasts.rs` uses the committed synthetic Icechunk repositories from
`dynapi-dataset` through a loopback catalog and repository host. Assertions derive
from their numeric recipes, explicit validation cases and header/link invariants.
It covers nearest points, runs/windows, ensembles, value/read budgets, cache
TTL/LRU, canonical forecast/analysis reads, generic and retryable error mapping,
byte admission, representation literals and client cancellation during a read.
Transient error fixtures inject typed reader failures at the reader boundary;
a separate case exercises an actual upstream HTTP read failure. No stored
reference-service responses are used.

The `harness` feature also enables loopback store substitution for synthetic
catalog recipes and one-shot typed reader failure injection. These hooks are
omitted from ordinary builds. These tests do not qualify remaining handlers,
projected transforms, routing or external telemetry availability.

`telemetry.rs` and `fixtures/telemetry.json` own event conditions, keyed identities,
metadata bounds, nulls, request isolation and cancellation. Telemetry module tests
own sampling, heartbeat deltas/lifecycle and cumulative load observations. With
`telemetry-delivery`, tests also own bounded overflow, stalled-backend response
independence, payload allowlists and shutdown flush using a loopback collector.
No external telemetry service or credentials are needed.

These tests do not qualify unfinished data handlers or external sink availability.
Maintainers verify migration output against the reference Python service outside
the repository and record intentional differences in `DEVIATIONS.md`.

`catalog_documentation.rs` drives the native HTTP server with an injected STAC
fetcher and fictional, Rust-owned source documents in `fixtures/catalog.json`.
It covers catalog ordering, visibility, capabilities, nullable metadata, numeric
spelling, load failures/cold waits, method errors, HEAD framing, redirects,
authentication and CORS. It verifies successful catalog caching and the absence
of ETags/conditional responses on catalog and documentation routes.

The documentation runtime assets in `../docs/` are source templates adapted from
FastAPI (MIT); its OAuth redirect comes from Swagger UI (Apache-2.0). Their
licenses are in the crate root. They are not saved service responses or fixture
answers. Tests check usable links and OAuth flow invariants. `openapi.rs` owns the
complete ordered schema in Rust, reuses enforced request bounds and literals
from `dynapi-contract`, and caches its encoding once. Its unit tests check schema
references/required properties, validator aliases/defaults/boundaries, response
unions and retry headers. A two-way route inventory test requires every served
operation to be documented or explicitly excluded (infrastructure, documentation
and CORS); the pending execution operations are listed separately in that test.
Changes to public operations or models must update these definitions and tests.
Migration maintainers compare OpenAPI and HTML bytes over HTTP outside this repo.

Template provenance: [FastAPI documentation templates](https://github.com/fastapi/fastapi/blob/0.141.1/fastapi/openapi/docs.py)
and [Swagger UI OAuth redirect](https://github.com/swagger-api/swagger-ui/blob/v4.14.0/dist/oauth2-redirect.html).
The templates retain their response-significant whitespace; the documentation
asset directory disables Git's trailing-blank checks for those HTML files.
Catalog numeric tests distinguish the Pydantic/Ryū layout used by dict responses
from the Python JSON float layout used by explicit JSONResponse endpoints.
The fictional catalog also includes non-finite bounding-box strings, whose
coerced values serialize as null under that dict-response policy.

Catalog response projections borrow a published snapshot and use a bounded
counting pass before allocating one output buffer. `catalog_routes.rs` unit tests
exercise exact encoded-byte limits, escaping, aggregate lists and hidden products.
The current-thread HTTP allocation tests use `support/allocation.rs` to account
for every allocation on the request/runtime thread after fixture warmup. A 2 MiB
unused metadata field must not cause a large request allocation; oversized list
and detail responses must fail through the generic 500 path before materializing
large metadata, with the fixed operator error category. See D17 for the
intentional response limit and measured public catalog sizes.

`catalog_lifecycle.rs` injects a fictional fetcher shared by catalog and forecast
HTTP requests. It checks one startup root/child flight, a common warm snapshot,
failure backoff, stale serving and refresh publication, request cancellation and
final application shutdown. App owns catalog close; temporary DataService or
catalog handles do not. Forecast batches retain one immutable catalog generation.

`geo_gate.rs` and `geo_gate_tests.rs` own metadata-based target selection, trusted
viewer fields, local proxy request headers, allow/refusal/error mapping, canonical
privacy, unchanged direct products, gated stubs, catalog failure/refresh, expiry,
redirect/timeout/body bounds, cache capacity and coalesced detached tasks. The fake
proxy uses only loopback HTTP and obviously fictional credentials. No test calls
the production check endpoint.

## Historical canonical snapshot recipe

`historical_snapshots.rs` copies the committed `forecast/repository` input from
`dynapi-dataset/tests/fixtures` into a private temporary directory. The source
repository is never written. Tests use the Icechunk Rust API to commit A to main,
obtain A's canonical link from a forecast POST, then commit B to the same main.
These are real repository commits served through the loopback fixture host.

Both snapshots retain the input latitude `[44, 42, 40, 38]`, longitude
`[0, 60, 120, 180, 240, 300]` and geographic CRS. A removes the input temperature,
static and auxiliary valid-time arrays. It resets root group attributes to only
`{"coordinates":"spatial_ref"}` and count attributes to `{}`. It replaces
init/lead/count metadata and payloads with one uncompressed little-endian bytes
chunk per array:

- A: init times 2026-07-30 00:00 and 06:00 UTC (int64 epoch seconds), lead times
  `[0, 3600]` seconds (float64, attested CF duration attributes including encoded
  NaN `_FillValue`), count shape/chunk `[2, 2, 4, 6]`.
- B: append the 12:00 run and 7200-second lead, giving shape/chunk `[3, 3, 4, 6]`.
  Replace all count values and add float64 `humidity` with that same shape.
- With run index `r`, lead index `l` and flat spatial index `s = 6*y + x`, A's
  int64 count is `1000 + 100*r + 10*l + s`; B's is
  `2000 + 100*r + 10*l + s`. B's humidity is `50 + 10*r + l + s`, with unit `%`.
  At grid `(0, 0)`, A's 00:00 count is `[1000, 1010]`; B's 00:00 count is
  `[2000, 2010, 2020]`, and B's 12:00 count/humidity are
  `[2200, 2210, 2220]` / `[70, 71, 72]`.

The fixed catalog advertises count and humidity throughout, modeling a current
catalog that lists a variable absent from an older snapshot. Tests own A's full
canonical JSON bytes, independently hashed ETag and immutable headers. Fake-clock
steps 29/30 exercise main's TTL; a two-entry LRU holds A/B pinned handles together,
then evicts and reopens A while main remains B. Later revalidation must preserve
B and never expire pinned A. Snapshot errors, missing A humidity/run/lead bounds,
and B's added schema/time values have explicit assertions. No saved service
outputs, Python or external network are needed.

## Temporal and request breadth recipes

`temporal_breadth.rs` constructs two fresh temporary Icechunk repositories with
one latitude (44), one longitude (0), and one int64 `count` variable. No committed
repository is changed. The epoch is 2026-07-30T00:00:00Z. Epoch coordinates use
attested int64 seconds, with `i64::MIN` for NaT; duration coordinates use attested
float64 seconds with NaN fill and timedelta metadata.

- Forecast source runs in hours: `[2, NaT, 0, 1, 2]`. Leads in stored seconds:
  `[-1800, 0, 1800, 3600, 3600.000001, 7200, NaN]`. At source indices `(r,l)`,
  `count = 100*r + l`. Latest is hour 2 and exact duplicate selection takes the
  last source row (4). Run bounds are strict and preserve source order and
  duplicates. Lead caps are inclusive; valid-time windows are half-open.
- The stored float `3600.000001` becomes `3600s + 999ns` under the attested
  successive float64 resolution conversion and integer truncation. Its lead
  hours are `1 + 999/3_600_000_000_000`. Filtering retains the nanoseconds even
  when the displayed valid time rounds to one microsecond. Therefore it belongs
  to `[01:00:00, 01:00:00.000001)` and not to a window starting at the latter.
- Analysis source times: `[01:00, NaT, 00:00, 01:00, 02:00, 01:00:01]` on that
  date. At source index `t`, `count = 1000+t`. Windows retain source order and
  duplicates, exclude NaT and the upper bound, and include the lower bound.
  Canonical GET with no bounds selects source index 5, independently of the
  maximum timestamp at index 4. Successful POST selections replay through their
  generated immutable canonical links.

`request_breadth.rs` owns JSON media families and a cross product of UTF-8,
UTF-16 and UTF-32, BOM presence and byte order, using both BMP and supplementary
Unicode scalars. Both POST routes assert complete error envelopes without
catalog I/O. Schema tables own normalized coercions, required/null/alias cases,
ordered nested/union errors, datetime grammar and raw/string integer limits.
The mixed-batch and required-field HTTP cases hold all compute capacity to
prove validation precedes admission. D1–D4 remain explicit deviations; the
surrogate cases include the D2 early rejection policy.

`canonical_breadth.rs` owns CSV normalization and the unique-variable bound,
analysis windows (including canonical versus POST rules), once-decoded route
identifiers, and the reader-error table across both POST and both canonical
routes. Typed one-shot reader faults assert exact public responses and successful
recovery, without leaking fixture diagnostics. These assertions are independent
of any reference-service output and require no Python or external network.

`historical_families.rs` extends main-advance HTTP coverage to analyses, stable
and changed ensemble member axes, attested Lambert forecasts and run discovery.
Its private A/B construction, arithmetic, cache-expiry/eviction sequence and
independent expectations are specified in [support/history_families.md](support/history_families.md).
