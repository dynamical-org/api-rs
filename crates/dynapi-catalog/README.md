# dynapi-catalog

An independent port of the Python STAC catalog and dataset cache. It is not
wired into the production server. Its interfaces do not depend on dynapi-query.

- `parse_document` accepts what Python's `json.loads(bytes)` accepts, through
  dynapi-contract's parser: BOM and UTF-16/UTF-32 detection, NaN/Infinity,
  `1e400` and large integers, within the 16 MiB body limit.
- `Collection::from_stac` parses variables, dimensions/coordinate metadata,
  S3/HTTP locations and the catalog repository recipe. Insertion order is
  retained. Locations use a port of CPython 3.12 `urlparse`, with parameters
  only in the final path segment and bracket/NFKC authority checks. HTTP
  repository/container URLs use the Python anchored allowlist on the original
  spelling, before URL normalization. CatalogError text and exception classes
  match Python, including `repr` printability, for inputs within the STAC field
  contract in [D11](../../DEVIATIONS.md).
- `refresh::child_urls` is Python's `urljoin`. `refresh::request_target`
  rejects URLs that reqwest's WHATWG parser would send to a different host or
  path than httpx; the native service checks every URL before fetching it.
- `refresh::CatalogState` is a portable state machine with an injected
  `dynapi_policy::Clock`. Snapshots are shared immutable values; accessor clones
  cannot mutate the published snapshot. It models freshness, one flight,
  stale reads, negative caching, capped jittered backoff and generation fencing.
- `native::StacCatalog` drives that state through an injectable `Fetcher`.
  Defaults: 10 s request phases, 300 s catalog TTL, 20 s cold wait, 30 s load
  deadline and 12 worker slots shared across refresh generations. Requests send
  identity encoding, reject compressed documents, stream at most 16 MiB and
  retry transport/5xx failures twice. Cold timeouts leave the flight running;
  stale callers immediately receive the previous snapshot. Use `warm()` within
  a Tokio runtime and call `close()` on shutdown. `get_collection()` returns
  `Ok(None)` for an unknown ID, distinct from a catalog failure.
- `cache::DatasetCache<C, H>` is generic over an opaque handle `H`. The caller
  supplies a resolver returning a snapshot ID/session and an opener returning
  the handle. Main defaults to 30 s TTL, revalidates unchanged immutable
  snapshots, and never serves an expired handle after a failed resolution.
  Pinned entries have no TTL. Both share the same 32-entry LRU. Hits/revalidated
  entries move to the end; expired lookups do not. Zero capacity disables
  caching, and zero main TTL disables main reuse. Cache keys include product,
  location, optional pin and default/routing profile, matching Python.
  Closures run synchronously, outside the global cache lock and under only
  their per-key lock; async server wiring can drive them in `spawn_blocking`.
  `RepositoryCache<H>` provides the separate lifetime repository cache, also
  generic, with per-key coalescing. No real stores/datasets are opened here.
- With the `reader` feature, `Collection::reader_config()` purely creates the
  existing `dynapi_reader::StoreDescriptor` and HTTP container declarations.
  Python accepts anonymous containers for other storage types; their recipes
  remain intact, but mapping to the current HTTP-only container reader fails
  explicitly. Virtual containers are always explicit; no access is inferred.
  Repeated prefixes map to one declaration, as Python's authorization dict does.

Default features enable `native` and `reader`. Disable both for portable
parsing/cache/state code and `wasm32-unknown-unknown` checks. Neither the
Icechunk reader nor the native reqwest/Tokio adapter is claimed WASM-ready.
Native fetch errors expose only the Python exception class, never request URLs
or response bodies. Parsing errors skip individual JSON-object children; HTTP,
body and JSON-decoding failures abort refresh. D11 lists the inputs that fail
closed instead of matching Python. An empty root is valid; a root
with children but no valid collection fails. Last duplicate collection ID wins,
retaining first insertion position, as in Python.

Tests use explicit Rust-owned parser, URL and numeric policies, fake-clock
cache/refresh sequences, concurrency invariants and loopback HTTP targets.
[Fixture documentation](tests/fixtures/README.md) describes the public STAC and
synthetic input documents and offline URL map. No reference-service output dump
or runtime is part of the suite. Maintainers verify migration behavior against
the reference Python service outside this repository. Broader coverage gaps
remain tracked separately; these tests do not qualify server wiring.
