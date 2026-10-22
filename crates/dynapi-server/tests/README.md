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
projected transforms, routing or real telemetry delivery.
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
