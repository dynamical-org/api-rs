# dynamical.org data API

A Rust implementation of the [dynamical.org](https://dynamical.org) data API at
[api.dynamical.org](https://api.dynamical.org). This implementation is a work in
progress and is not yet serving traffic. The reference Python service continues
to serve the API while the Rust implementation is verified during migration.

## Crates

- `dynapi-catalog`: STAC parsing, refresh, store recipes and dataset-cache policy with portable and native adapters.
- `dynapi-contract`: portable request validation, ordered JSON, formatting and error envelopes.
- `dynapi-policy`: portable rate limits, compute permits, CORS, cache and timing rules.
- `dynapi-query`: portable point/run/time selection, budgets, load plans and response encoding over an opened dataset view.
- `dynapi-dataset`: native, bounded dataset metadata and CF coordinates with a shared snapshot session for query reads.
- `dynapi-reader`: native, snapshot-pinned Icechunk/Zarr numeric reads and explicit CF time decoding.
- `dynapi-read`: single-request JSON command-line adapter for exercising the reader.
- `dynapi-server`: native Hyper HTTP/1.1 server, middleware, admission and telemetry sink boundaries.

The server currently serves the root redirect, robots and health check, and
implements request IDs, CORS, origin authentication and rate limits. It serves
`POST /v1/forecasts` and canonical `GET /v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}`
for materialized rectilinear products, with bounded dataset caching, compute
admission and response encoding. Analyses POST, run listing, discovery and
documentation handlers remain internal 501 stubs. See [forecast serving](docs/forecasts.md)
for configuration, bounds and the fixture test contract. Optional analytics/error
delivery and stdout heartbeat behavior are
described in [docs/telemetry.md](docs/telemetry.md). Production connection/drain
qualification remains unfinished; see [DEVIATIONS.md](DEVIATIONS.md).

## Build and checks

Run from the repository root. Native builds need a C/C++ toolchain, CMake,
pkg-config and OpenSSL development headers.

```sh
cargo build --locked --release --workspace
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --features dynapi-server/harness -- -D warnings
cargo test --locked --workspace --features dynapi-server/harness
```

The `harness` feature enables test hooks, including panic injection, and belongs
only in test builds. An ordinary server build omits those hooks:

```sh
cargo run --locked -p dynapi-server -- --bind 127.0.0.1 --port 8000
```

An explicitly empty origin secret fails startup. Supported environment settings
are defined in `crates/dynapi-server/src/config.rs`; deferred settings are listed
in deviation D9.

## Testing

Tests are Rust-owned and fixture-based. No Python is needed or used by the test
suite. During migration, maintainers verify outputs against the reference Python
service outside this repository; intentional differences are recorded in
[DEVIATIONS.md](DEVIATIONS.md).

## Verification policy

During migration, maintainers check status, complete body bytes and meaningful
headers against the reference Python service outside this repository. That
service guides verification and may itself contain bugs. Committed tests use
local fixture inputs with explicit values, independently computed arithmetic,
invariants and error assertions. Intentional differences record their trigger, behavior,
consequence, rationale and regression test in [DEVIATIONS.md](DEVIATIONS.md).
Comparisons must not silently exclude mismatches.

## Reader contract and bounds

The provisional reader API opens a read-only session at an explicit snapshot;
it never falls back to a branch. It supports local, anonymous S3 and HTTP(S)
descriptors, stored float32/float64/int64 values, and a closed metadata/codec
allowlist. Ordered list selectors preserve duplicates and use Cartesian indexing.
CF time conversion is explicit and separate from stored-value reads.

Work is checked before allocation/decode: output is capped at 64 MiB, float inner
chunks at 16 MiB, int64 chunks at 64 MiB, shard indexes at 8 MiB, selector entries
at 1,048,576 and touched inner chunks at 65,536. Reads and decodes are sequential
with a 90-second reader deadline. Sparse sharded reads fetch selected payload
ranges and decode each touched inner chunk once. Trace counters describe logical
deliveries, not physical storage traffic or whole-process memory.

Virtual HTTP payloads require declared anonymous container prefixes and bounded
range responses without redirects. Icechunk's repository-object HTTP transport
still follows redirects and has no body memory cap; repository URLs and redirect
targets must be trusted. These prototype bounds are not production qualification.
The full contract, codec policy, CF tuples and transport limitations are in
[docs/reader.md](docs/reader.md).

## Opened datasets

`dynapi-dataset` opens a branch or explicit snapshot once and provides a bounded,
CF-decoded `dynapi-query::DatasetView` and shared-session `DataReader`.
`run_with_permit` keeps the compute permit until the worker finishes, including
when its caller is cancelled. The adapter adds no HTTP routes, catalog cache or
TTLs. The original reader entrypoints retain their stricter coordinate contract.
See [the adapter documentation](docs/dataset.md) for metadata accounting,
codec policy, typed storage errors and CRS limitations.

## Pins and portability

`rust-toolchain.toml` pins Rust 1.97.0. Retain `Cargo.lock` and use `--locked`.
Icechunk family crates are constrained to 2.2.2, zarrs to 0.23.0 and zarrs_icechunk
to 0.5.0; mixing Icechunk family versions breaks this stack. The adapter enables
additional storage/cloud dependencies transitively because Cargo features are
additive. Zarrs' ndarray feature works around unguarded types in its async code.

CI compile-checks `dynapi-contract`, `dynapi-policy` and `dynapi-query` for
`wasm32-unknown-unknown`, plus `dynapi-catalog` with default features disabled. Native storage, runtimes and C-backed codecs remain
portability blockers; these checks do not qualify the reader or server for WASM.

## ABI rule and CI

Release executables must import no GLIBC symbol above 2.36. Build in
`rust:1.97.0-bookworm`, run `ci/check-glibc.sh` against each exact release binary,
and smoke-run those same binaries in `debian:bookworm`. The runtime needs libc,
libgcc, libm and libstdc++. A build on a newer host can exceed the ABI ceiling.

[CI](.github/workflows/ci.yml) uses the pinned Bookworm image for formatting,
portable/native checks, release build, tests and Clippy. Build, test and Clippy
share the release profile and two compiler jobs. It caches the Cargo registry and
`target`, restores their ownership and readability after container execution,
then checks both executables' ABI and runtime. Actions are pinned to commit SHAs;
workflow permissions are read-only, with no secrets or deployment.

## License

[BSD-3-Clause](LICENSE). Third-party numeric parser notices are retained in
[LICENSE-numeric](crates/dynapi-contract/LICENSE-numeric). URL compatibility code retains
[LICENSE-httpx](crates/dynapi-catalog/LICENSE-httpx).
