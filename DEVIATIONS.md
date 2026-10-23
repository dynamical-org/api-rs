# Deliberate contract deviations

The reference Python service uses CPython 3.12, FastAPI 0.141.1 and Pydantic 2.13.4. It guides migration verification outside this repository and is not infallible. These entries document intentional differences rather than comparator exclusions. Public tests are Rust-owned and fixture-based, with explicit expectations, arithmetic and invariants; they do not use Python. Broader coverage gaps are listed in the Rust-owned test coverage tracking issue.

Dataset ensemble compatibility is covered in `crates/dynapi-dataset/tests/fixtures.rs`: `ensemble_cartesian_reads_follow_each_variable_source_order` checks reordered and duplicate selections in both forecast variable orders; `physical_ensemble_members_and_offsets_follow_the_recipe` checks decoded padded int16/typesize-4 and int8/uint8 member identities and offset values; `complete_ensemble_query_pairs_members_with_recipe_values` checks a complete two-time analysis response with each member's temperature and exact integer count. Expectations follow the construction recipes in `crates/dynapi-dataset/tests/fixtures/README.md`.

## D1 — Fixed JSON recursion guard

- Trigger: a JSON value deeper than 256 recursive child values; for example 258 nested arrays around `0`.
- Python: parsing/encoding depends on the interpreter recursion limit and the active call stack. The 258-array fixture reaches validation and returns 422 `model_attributes_type` with the nested input. Inputs beyond the interpreter's parsing limit return 400.
- Rust: stops parsing at value depth >256 and returns 400 `{"detail":"There was an error parsing the body"}`.
- Consequence: some deeply nested invalid requests return 400 instead of Python's 422 or 500. Documented valid ForecastBatch requests never approach this depth.
- Rationale: a fixed guard bounds recursive stack usage on native and WASM targets, independently of interpreter and runtime stack configuration.
- Tests: `excessive_depth_returns_the_explicit_parse_envelope` owns the 400 bytes at depths 258 and 10,000; `decoder_limits_are_recoverable` checks parser error kinds.

## D2 — Reject lone Unicode surrogates during parsing

- Trigger: an escaped lone surrogate (`"\ud800"`, or its UTF-16/32 equivalent), including a value subsequently overwritten by a duplicate key.
- Python: `json.loads` retains surrogate code points in `str`, decoding bytes with `surrogatepass`. An error response that echoes them normally fails UTF-8 encoding and becomes 500; an overwritten value disappears, so `{"queries":[],"discarded":"\ud800","discarded":0}` returns its ordinary 422. Raw UTF-8 surrogate bytes in `b'"\xed\xa0\x80"'` likewise give Python 500, while `b'{"queries":[],"x":"\xed\xa0\x80","x":0}'` gives Python 422. Surrogates removed by FastAPI's recursive `_sa`-prefix dictionary filtering can also disappear before error encoding. Success serialization with `ensure_ascii=True` can represent surviving surrogates.
- Rust: Rust strings contain Unicode scalar values. The parser rejects lone surrogates immediately and the validation entrypoint returns 500 `Internal Server Error`, even for overwritten values. Raw UTF-8 encodings of surrogates are invalid UTF-8 and return the standard 400 parse envelope.
- Consequence: the usual escaped-surrogate error 500 is reproduced, but discarded/filtered surrogate inputs and otherwise valid string fields can differ. The raw UTF-8 fixtures above give Python 500 / Rust 400 and Python 422 / Rust 400, respectively. A schema-valid `variables:["\ud800"]` reaches the Python handler but is rejected by Rust. The typed output/URL/repr interfaces accept Unicode scalar strings only.
- Rationale: avoids a second, surrogate-preserving string representation throughout request models, Unicode normalization and output APIs. No claim of arbitrary Python `str` parity is made.
- Tests: `surrogate_rejection_precedes_overwrite_and_handler_entry` owns escaped high/low surrogate rejection, overwritten/hidden fields, a schema-shaped variable and raw invalid UTF-8 envelopes; `decoder_limits_are_recoverable` checks parser rejection. UTF-16/32 surrogate coverage remains pending in the Rust-owned test coverage tracking issue (contract decoding).

## D3 — Preserve nonzero sub-minute datetime offsets

- Trigger: the typed datetime formatter receives a positive UTC offset smaller than one minute, for example `offset_seconds:1` or `16`.
- Python counterpart: the retained experimental expression `datetime.isoformat().replace("+00:00", "Z")` changes `2026-01-01T00:00:00+00:00:01` to `2026-01-01T00:00:00Z:01`.
- Rust: emits `2026-01-01T00:00:00+00:00:01`, preserving the nonzero offset. Only an exactly zero offset becomes `Z`; other offsets and naive datetimes match `datetime.isoformat()`.
- Consequence: the typed helper deliberately differs from the experimental expression on positive sub-minute offsets. Forecast request parsing does not accept offsets with seconds, and production `_format_datetime` converts to UTC before formatting, so this does not change a production response.
- Rationale: preserve valid ISO datetime offsets instead of replacing the prefix of a nonzero offset.
- Tests: `nonzero_subminute_offsets_are_preserved` checks zero and positive/negative one- and sixteen-second offsets; `timezone_seconds_and_microseconds_are_preserved` checks year 1, microseconds and a larger offset with seconds. Expected strings are explicit Rust literals.

## D4 — Wider typed integer-output domain

- Trigger: a caller constructs a typed `Value::Int` whose decimal magnitude exceeds 4,300 digits, for either sign; for example `BigInt::from(10u8).pow(4300)` or its negative.
- Python: CPython 3.12.3 with its default `sys.get_int_max_str_digits() == 4300` raises `ValueError` when `json.dumps` converts that integer to decimal. The minus sign does not count toward the limit. Both 4,299- and 4,300-digit magnitudes serialize successfully.
- Rust: `output::json` writes the complete arbitrary-precision integer, with no decimal-digit limit, in either JSON escaping mode.
- Consequence: the public typed formatter accepts a larger integer domain than the default Python writer. Accepted forecast requests cannot produce these values: raw JSON parsing and numeric-string validation already bound integer magnitudes. Forecast data integer dtypes are also below the limit.
- Rationale: preserve the arbitrary-precision typed writer without introducing a new output failure for caller-constructed integers.
- Tests: `typed_large_integers_preserve_sign_and_every_digit` constructs signed powers of ten at 4,299, 4,300, 4,301 and 5,000 digits and checks every decimal byte in both escaping modes, independently of parsing those expected strings. Request digit guards remain separate in `decoder_limits_are_recoverable`.

## D5 — Internal HTTP milestone stubs (temporary)

- Trigger: `POST /v1/analyses`; `GET /v1/forecasts/runs`, `/v1/data-products`, or `/v1/data-products/{dataProductId}`. The built-in GET/HEAD `/openapi.json`, `/docs`, `/docs/oauth2-redirect` and `/redoc` are also reserved stubs until the remaining-routes milestone.
- Python: validates and serves the complete API, catalog, data, schema and documentation responses.
- Rust: returns 501 `text/plain; charset=utf-8` with a temporary internal data-route stub message, after the implemented outer protections. These are internal test milestones, not proposed public API behavior. Analyses/run model validation and their data-dependent errors remain unfinished. Forecast POST and canonical point GET now serve materialized datasets. Data stubs exercise compute admission; discovery and docs stubs do not claim handler parity.
- Consequence: this server is not eligible for public traffic. The remaining data and documentation handlers remain unfinished; their stub responses do not establish handler parity.
- Rationale: land native HTTP protections independently of data handlers and keep the missing surface unmistakable. Remove these stubs as handlers land.
- Tests: `validation_precedes_compute_but_follows_rate_debit` checks validation/compute/rate admission order; `admitted_forecast_returns_data_with_cors_and_request_id` checks an admitted forecast's 200 status, JSON result, UUID4 request ID and simple-response CORS headers. The remaining stub routes retain their explicit milestone behavior. Full reserved-route/method coverage remains pending in the Rust-owned test coverage tracking issue (server response surface).

## D6 — Bounded HTTP request bodies

- Trigger: a forecast body exceeds 16 MiB (16,777,216 bytes), for Content-Length or chunked requests.
- Python: has no application-level request body cap; it consumes the body before parsing/validation.
- Rust: counts received data bytes and responds 413 `{"detail":"Request body exceeds 16777216 bytes"}` before allocating/reading beyond the cap. Origin authentication, preflight and rate precedence are unchanged.
- Consequence: oversized requests can return 413 rather than Python's validation/error/data response. The response has X-Request-Id and the ordinary simple-response CORS policy.
- Rationale: make ingress memory finite for the native service. This limit requires reconsideration before a public cutover.
- Tests: `body_cap_is_explicit_and_carries_cors_and_request_id` checks the 16 MiB limit, 413 status, exact error body bytes, JSON content type, request-ID presence and wildcard CORS header for a Content-Length request one byte over the cap.

## D7 — HTTP field ordering and process-dependent method-set order

- Trigger: a response serialized by Hyper rather than uvicorn/h11; a 405 for the two-method root or robots route.
- Python: uvicorn adds Date and `server: uvicorn` first, then Starlette's headers. Method sets can produce either `GET, HEAD` or `HEAD, GET` in Allow depending on PYTHONHASHSEED.
- Rust: Hyper emits its header-map order, typically application fields first and Date last. It deliberately sends exactly `server: uvicorn` for HTTP compatibility, despite using Hyper. Parser rejections follow Hyper (D10). Multi-method Allow is `GET, HEAD`.
- Consequence: wire field order/case differs; meaningful field values remain part of the response contract. Date is validated as an HTTP date rather than fixed across clocks, and multi-method Allow is explicitly ordered as `GET, HEAD`.
- Rationale: retain the existing Server contract and meaningful header bytes without reimplementing Hyper's response encoder. Header order/case and valid changing dates carry no API meaning.
- Tests: `robots_get_head_and_packaged_metadata` checks HTTP Date, request IDs and the Server value without promising header order. `unhandled_and_serialization_errors_close_before_queued_requests` checks connection closure. Explicit 405 Allow coverage remains pending in the Rust-owned test coverage tracking issue (server response surface).

## D8 — Compiled robots metadata and limited file-response surface

- Trigger: a robots request after its source file has changed since compilation, or independently packaged builds with different source-file metadata.
- Python: FileResponse uses the installed file's current mtime/size for Last-Modified and an MD5 metadata ETag, and supports byte ranges.
- Rust: embeds the exact file bytes and the build input's metadata/ETag, so the standalone binary requires no Python checkout. Range handling for this asset follows the pinned Starlette 1.3.1 FileResponse: single/suffix/open-ended ranges, sorted/merged overlapping or adjacent ranges, multipart responses, malformed 400 and unsatisfiable 416 responses, exact If-Range validator equality and HEAD headers/body suppression. It preserves that source's accepted empty slices and integer grammar rather than silently imposing a different range policy.
- Consequence: bytes, cache policy, media type, length and metadata match when built from the same file. Metadata can differ across independently packaged artifacts; runtime changes to the source file do not affect the embedded asset. The earlier full-file Range difference is retired.
- Rationale: make the native artifact self-contained while retaining the file-response range contract for the embedded asset only. Multipart responses use fresh 26-hex-digit boundaries; their generated boundary bytes are not stable response content.
- Tests: `robots_get_head_and_packaged_metadata` owns the build-input metadata contract. `robots_single_range_has_exact_bytes_and_headers`, `robots_suffix_open_empty_and_merged_ranges_preserve_file_metadata`, `robots_invalid_ranges_have_exact_plain_text_errors_without_file_headers`, `robots_multipart_ranges_have_exact_framing_lengths_and_head_headers`, `robots_if_range_uses_exact_validators_and_bypasses_invalid_ranges_on_mismatch` and `robots_range_integer_grammar_keeps_huge_operands_and_digit_guard_explicit` in `crates/dynapi-server/tests/server.rs` own explicit ranges, literal error envelopes, byte slices, multipart framing, cache/metadata headers and HEAD behavior. No saved service answers are used.

## D9 — Startup configuration validation

- Trigger: invalid numeric configuration at startup.
- Python: reads some budgets and sample rates lazily during requests.
- Rust: parses its supported budgets, slots, rate limits, transport bounds, sample rate, heartbeat interval, dataset-cache entries and STAC URL at startup. Invalid bounds fail startup.
- Consequence: malformed numeric settings can fail earlier. Data-handler settings `DYNAMICAL_API_CHUNK_CACHE_BYTES`, `DYNAMICAL_API_GROUPED_READS` and `DYNAMICAL_API_MAX_GROUP_POINTS` are not implemented by this Config.
- Rationale: validate native resource bounds before accepting requests. The earlier milestone's deferred-emitter statements are superseded: native heartbeat, handler success/failure records and optional PostHog/Sentry delivery are implemented. D19 through D22 describe their error, resource, delivery and canonical analytics differences; D23 covers stdout backpressure.
- Tests: `configuration_defaults_and_empty_secret` owns startup validation. Telemetry integration and delivery tests cover the implemented event and shutdown behavior; see [docs/telemetry.md](docs/telemetry.md).

## D10 — Hyper HTTP/1 wire parsing and bounded native connections

- Decision: HTTP/1.1 wire parsing follows Hyper's standard parser and defaults, **not uvicorn/h11**. The custom compatibility parser, normalization and parser-error rewrite have been removed. Production requests pass through Cloudflare, Modal's front end and its in-container proxy, which parse and re-emit HTTP. Uvicorn is not in the production path: Python runs as a Modal `asgi_app`. Reproducing exotic h11 acceptance rules is not a client-visible contract.
- Inputs with potentially different handling: bare LF, obs-fold, duplicate or conflicting Content-Length, Transfer-Encoding variants (including TE with Content-Length), missing or duplicate Host, header-size/count limits, absolute-form targets and malformed chunks. There is no additional h11 validation. With the locked Hyper 1.12.0/http 1.5.0 stack, the tested outcomes are:

  | Input | Native outcome |
  | --- | --- |
  | Bare LF request headers | Accepted by Hyper; bytes are not normalized by this server |
  | Obs-fold header | Hyper 400, empty body |
  | Identical or numerically equal duplicate Content-Length | Accepted; `2` and `02` are equal |
  | Comma Content-Length or conflicting lengths | Hyper 400, empty body |
  | `gzip, chunked`, `identity, chunked`, repeated `chunked` | Accepted if Hyper recognizes final chunked framing |
  | Transfer-Encoding without final chunked | Hyper 400, empty body |
  | Transfer-Encoding with Content-Length | Hyper controls precedence and closes after the response; some validation is field-order dependent |
  | Missing or duplicate Host | Accepted by Hyper |
  | Headers | Explicit limits: 100 fields and 417,792 bytes including the request line; complete 70,000-byte padding accepted, over-limit head returns 431 regardless of read coalescing |
  | Absolute/authority/asterisk-form target | Application 400 `Invalid request target`, then close, instead of Python's 404 for absolute-form |
  | Raw URI fragment | `http::Uri` strips the fragment before dispatch; raw `/healthz#foo` routes as `/healthz` (200), while percent-encoded `%23` remains part of the decoded routing path (404) |
  | LF-only trailers | Incomplete standalone reproductions time out; appended CRLF pipeline bytes can complete Hyper's trailer parse and yield one 422 before idle/header closure |
  | Malformed chunk sizes/footers/trailers | Hyper's body-reader error propagates as a service error; the connection closes without an application response |

- Routing choice: Hyper exposes parsed scheme/authority, so the server can reject non-origin forms with 400. It does **not** expose the original wire target or discarded fragment. Fragment normalization above is an explicit parser limitation; no raw-target pre-parser is retained. Accepted as a deviation: RFC 9110 §4.2.4 and RFC 3986 §3.5 keep fragments out of request targets, clients strip them before sending, and the production front ends sit in front of this server. A raw `#` therefore reaches it only from a non-conforming client, and the result is the same resource the client named. Origin-form percent-decoding, replacement of invalid UTF-8 and single-pass decoding remain unchanged and tested against Starlette.
- Parser responses: Hyper's header-parser 400/431 responses have an empty body, Content-Length and Date, without application Server or X-Request-Id. Body framing failures close without an HTTP response; they never become the application's JSON body-parse 400. Valid HTTP framing with undecodable JSON/body encoding still produces the ordinary application error. Unhandled/serialization/panic 500s retain application status/body/CORS/ID fields and flush once, then close before queued requests dispatch. Hyper receives an explicit `Connection: close`; Python closes after its 500 without that field. Tests assert this transport difference rather than claiming equal wire headers.
- Deadlines: all settings below are positive integer seconds/counts validated at startup, including clock/semaphore capacity. Hyper's timer enforces an absolute header deadline whenever it begins waiting for a head, including empty initial connections and the next head after a response. Incoming bytes do not restart it. A `tokio::time::timeout` wraps bounded collection of Hyper's `Incoming`, through all body frames and trailers; trickling bytes never extends it and no independent framing state can clear it early. Timeout/body-reader errors escape the handler, close the connection and retain one terminal cancellation record. If middleware/routing responds without consuming a body, Hyper drains immediately available bytes or closes rather than waiting without a body deadline.

  | Environment setting | Default | Meaning |
  | --- | --- | --- |
  | `DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS` | 10 | Overall wait for each complete header |
  | `DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS` | 30 | Overall wait for complete Hyper body collection |
  | `DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS` | 5 | Idle keep-alive expiry |
  | `DYNAMICAL_API_MAX_CONNECTIONS` | 1024 | Lifetime semaphore permits, including idle time |

- Idle/lifetime: a separate idle timer starts when the buffered application response is ready, slightly before transport completion, and stays armed until the next complete request dispatches. Raw incoming bytes do not cancel it, so a blocked response write or partial next head cannot lose its last active deadline. Partial next heads therefore expire at the earlier of idle expiry and Hyper's header deadline, even if bytes trickle in; this is deliberately stricter than uvicorn's cancellation of idle expiry on any incoming data. No custom I/O wrapper remains. Uvicorn's default idle interval is also five seconds, but it has no overall header/body budget or default connection cap. Above the cap, accepted sockets close immediately without a waiting task, HTTP 503 or rate debit. The OS backlog remains separate. Permits release on disconnect, error, timeout or task cancellation. The accept loop reaps tasks and aborts them when dropped.
- Proxy/shutdown: socket identity is used unless origin authentication enables CF metadata. X-Forwarded-* is ignored like the local reference Python service (`proxy_headers=False`). Real production proxy/graceful-drain/fault qualification remains later work; this does not qualify a public cutover.
- Tests: `hyper_owns_header_syntax_and_parser_error_responses`, `malformed_chunks_close_without_application_json_or_pipeline_dispatch`, `unhandled_and_serialization_errors_close_before_queued_requests`, `panic_closes_before_queued_requests`, `origin_form_decoding_is_preserved_and_non_origin_forms_are_rejected`, and the deadline/cap regressions in `crates/dynapi-server/tests/server.rs`. They cover empty connections, slow/trickled and partial next heads, fixed/chunked body waits, LF-only empty/nonempty and folded trailers, completed bodies, early middleware responses, keep-alive resets, pipelined errors and permit release with explicit cases and invariants.

## D11 — STAC catalog input contract and request targets

The STAC catalog is generated by dynamical-catalog. `dynapi-catalog` matches Python's `StacCatalog` and `Collection.from_stac` for the documents and links that generator produces. It fails closed on inputs where exact parity would mean reproducing Python's dynamic coercions or fighting the HTTP stack. Each item below skips the affected child unless it says the refresh aborts. An abort keeps the previous snapshot and enters backoff.

- **Field types (skip).** Python coerces non-STAC field types; Rust requires STAC types and skips the child with `TypeError` `STAC field '<name>' has unsupported type '<type>'`.
  - `id`, `title`, `description`, `version`, `license` and `xarray:storage_options.client_kwargs.region_name` take Python `str()` of strings, null, booleans and numbers exactly (floats use CPython `repr`). For a list or object, Python uses its `repr`; Rust skips. Non-string `href` values keep Python's outcome, an invalid Icechunk URL.
  - `model_id`, `model_name`, variable `unit`/`long_name`/`standard_name` and the first self link's `href` must be a string or null. Python keeps any raw value.
  - Variable `dimensions` must be a list of strings or a string (Python's `tuple()` of a string, its characters). Python also accepts objects (their keys) and lists with non-string elements. Non-iterables raise Python's exact `TypeError`.
  - `cube:dimensions` must be an object; `""` and `[]` give `{}`, and other scalars raise typed `dict()`-style errors. A non-empty list is skipped: Python's `dict()` would accept a list of key/value pairs.
  - Spatial bbox strings use Python's `float()` whitespace set and ASCII number grammar, including `nan` and `inf` spellings. Strings with underscores or non-ASCII digits skip with Python's `could not convert string to float` text, although Python accepts some.
- **Numbers (skip).** NaN, infinities and integers outside the 64-bit range are accepted anywhere the catalog does not read them. Inside a read field they skip the child with `ValueError`: `id`, `title`, `description`, `version`, `license`, `model_id`, `model_name`, `links`, `cube:dimensions`, `cube:variables`, `assets.icechunk` and `extent.spatial`. A root document is checked against the same field list; there the refresh aborts. Python keeps these values or converts them with `float()`. The finite/64-bit input bound remains explicit. Supported floats in parsed values, repository recipes and serialized bbox values use CPython's spelling, including exponent signs and padding, in catalog-only and workspace builds; the crate enables `arbitrary_precision` itself to retain those serialized tokens.
- **Nesting (abort).** Parsing uses dynapi-contract's Python-compatible JSON parser and its D1 bound: a value nested deeper than 256 levels aborts the refresh with `RecursionError`. CPython 3.12.3 `json.loads(bytes)` accepts up to 9,996 nested arrays inside a collection object. A fixed bound keeps parser stack use independent of thread and WASM stack sizes; generated STAC documents are a few levels deep. BOM, UTF-16/UTF-32 detection, NaN/Infinity, `1e400` and integers up to CPython's 4,300-digit limit are accepted as in Python.
- **Surrogates (abort).** Python decodes with `surrogatepass` and keeps lone surrogates, escaped or raw. Rust strings hold Unicode scalar values, so the refresh aborts with `UnicodeDecodeError`, as in D2. Other UTF-8 decode errors reproduce CPython's message. UTF-16/UTF-32 decode errors keep the exception class, but their text is approximate.
- **Request targets (abort).** Before every GET (root, child and redirect hop), Rust normalizes the URL with httpx 0.28.1 semantics, constructs a reqwest URL, and compares scheme, host bytes, normalized port and raw path/query. Any difference aborts with `InvalidURL` before that GET. Unrepresentable URLs also abort; ports must fit u16. This comparison replaces the spelling blacklist. Literal terminal `/.` and `/..`, interior dot segments, spaces, Unicode paths and empty queries are supported. Examples that fail closed include path backslashes, encoded dot segments, query apostrophes and rewritten hosts such as `127.1`. Fragment backslashes are allowed. Controls retain httpx's `InvalidURL` class/message; ASCII host normalization retains IP handling, lowercasing and character validation. Non-ASCII hosts and any ACE label (`xn--`, case-insensitive) abort with `InvalidURL` before their GET, in roots, children and redirects; Python would IDNA-encode internationalized hosts. Children use Python's `urllib.parse.urljoin`, including falsey hrefs resolving to the configured root. Redirects use httpx's construction: parse Location, fill a missing host without inheriting the request port, and join only relative URLs. When the request host is an IPv6 literal, a Location without a host (`http://:8080/x`, `http:/x`) aborts with `InvalidURL` before the redirected GET, although httpx would follow it. Absolute empty queries survive; `http:/target.json` selects port 80. Headers decode as UTF-8 if all values permit it, otherwise Latin-1, before URL quoting. The 20-redirect limit and Python's exception-dependent retry behavior remain.
- Tests: `unsupported_field_types_are_explicit_errors`, `bbox_ascii_number_policy_rejects_unsupported_coercions`, `read_number_bounds_distinguish_root_abort_and_child_skip`, `parser_depth_surrogate_shape_and_body_limits_are_owned`, `repository_host_policy_uses_original_spelling`, `request_targets_have_explicit_normalization_and_rejection_rules`, `bbox_and_repository_numbers_keep_python_tokens_in_catalog_only_builds`, `scalar_regions_map_to_explicit_reader_text`, `public_stac_graphs_refresh_offline_with_input_derived_membership`, `redirect_targets_obey_explicit_request_target_policy`, `normalized_port_state_must_match_even_when_the_effective_port_agrees`, and the existing catalog native/cache/state/loopback tests. Every expectation comes from explicit values, input metadata or invariants. Wider field/coercion/encoding/URL matrices and graph-order/failure scenarios remain pending in the catalog section of the Rust-owned test coverage tracking issue.

## D12 — Catalog-attested WKT classification without PROJ

- Trigger: an x/y dataset has a `spatial_ref` coordinate with a truthy `crs_wkt`, or otherwise a truthy `spatial_ref` attribute.
- Python: `pyproj.CRS.from_wkt(str(value)).is_geographic` accepts the full PROJ WKT language; unfamiliar valid geographic WKT is geographic, and malformed WKT raises a PROJ error.
- Rust: `dynapi-dataset` recognizes six exact geographic WKT input strings from the public catalog. The retained Lambert conformal WKT is projected. The six geographic inputs include the derived/rotated WKT used by HRDPS. Every other nonempty value fails closed as `Projected`, including malformed and differently formatted geographic WKT. A missing/false attribute is `Missing`; a variable that is not an xarray coordinate supplies no CRS. `crs_wkt` takes precedence over `spatial_ref` with Python truthiness.
- Consequence: the six supported geographic strings are accepted. A new geographic WKT requires explicit policy review before x/y point queries are accepted. Unknown WKT reaches the existing query engine's internal `Projected-CRS grids are not yet supported` error instead of being assumed to contain longitude/latitude. Projected point transformations remain unfinished.
- Rationale: keep the native adapter independent of PROJ/pyproj and make classification conservative and auditable. This classifier does not implement coordinate transformations.
- Tests: `classifier_uses_explicit_geographic_policy_and_fails_closed` owns a table of six geographic WKT inputs, the Lambert projected input, malformed/unknown inputs and formatting changes. `coordinate_membership_and_crs_reach_query_validation` owns referenced, fallback, missing and unreferenced CRS behavior plus five projected-query rejections. No service-produced classification or response answers are loaded.
- Superseded in part by D34 for the two attested projected operations.

## D13 — Bounded opened-dataset metadata

- Trigger: opening a dataset would exceed its metadata byte budget (64 MiB by default, configurable down or up explicitly through `open_bounded`).
- Python: `xr.open_zarr(chunks=None)` has no equivalent aggregate bound on eager coordinate arrays, attributes and metadata.
- Rust: rejects with `Opened dataset metadata exceeds <limit> bytes`. The deterministic conservative charge includes serialized metadata staging, parsed JSON nodes/strings, decoded attributes, dimensions/chunks, full-axis selectors, simultaneous selector-group capacities, CF raw/decoded staging and retained coordinate values. Before allocating full-axis selectors, reject excessive inner-chunk cardinality and reserve `N * (4 * stored_width + 128) + G * 512` for N elements and G chunks, plus member Value/BigInt storage and keys. Implicit queried dimension indexes validate their endpoint and allocate exactly the charged N integer slots (`N * 8`, plus member Value/BigInt storage). Other adapter-owned dimension/chunk/selector vectors reserve exact element counts before fallible conversion. A nonallocating JSON-tree preflight and coordinate/selector cardinality checks precede those allocations. `metadata_bytes()` reports this charge rather than allocator RSS. Auxiliary coordinates and dimension axes the query engine never selects remain lazy; their data is not loaded during open.
- Consequence: a large otherwise valid dataset may fail open. Existing reader output, padded-chunk and selector bounds remain in force independently. This bounds adapter-owned opened metadata and its selector/CF staging. Encoded/decompressed chunk scratch remains under the reader's separate payload/chunk bounds, rather than this charge. Icechunk's snapshot/manifest decompression, repository transport and internal caches are also excluded; those upstream allocation limitations remain documented in `docs/reader.md`.
- Rationale: make opened dataset memory finite by bytes. Cache entry count/TTL and HTTP mapping belong to the service adapter.
- Tests: `metadata_cap_includes_coordinates_and_attributes` checks tiny caps and the charged boundary. `coordinate_staging_peak_obeys_cap_before_selectors_or_groups` uses a counting global allocator for 1,048,576-element duration and chunk-[1] cases at 34,000,000 bytes, 64 MiB and 256 MiB. Peak requested allocation must fit the cap plus a fixed 1 MiB margin for the tiny fixture's excluded upstream/runtime allocations; rejected cases must allocate less than that margin. The same isolated test checks accepted implicit axes of 1,048,577 and 4,194,305 elements at 10 MB and 64 MiB: exact N-slot capacity, retained allocation and peak below the charge/cap plus a fixed 256 KiB upstream/runtime margin, independent of axis length and below 2.7% of 10 MB. Metadata preflight and oversized implicit selector regressions exercise rejection before tree/selector allocation.

- Large integer epochs: if the full-axis reservation does not fit, an exact attested int64 epoch dimension coordinate can use sequential batches of at most 262,144 selectors. The checked reservation is `N * 8 + min(N, 262144) * 160 + G * 512 + name.len() + 64`; complete exact eager axes, the 64 MiB cap, selector/chunk ceilings and cache/active byte accounting remain. One 90-second deadline covers all batches. Float durations and member encodings keep their existing full-read policy. `coordinate_staging_peak_obeys_cap_before_selectors_or_groups` additionally checks seven epoch sizes around batch boundaries through 1,048,576 at charge-minus-one/exact/default caps, exact retained capacity, and peak/retained allocation within the charge plus a fixed 1 MiB excluded-runtime/upstream margin. `batched_epochs_keep_nat_gaps_duplicates_source_order_and_analysis_bounds` and `invalid_epoch_in_a_later_batch_cannot_return_a_partial_dataset` own synthetic stored values and failures; private epoch tests own exact encoding/overflow admission, batching boundaries, virtual-clock deadline and cancellation.

## D14 — Byte-bounded dataset retention and response materialization

- Trigger: an opened dataset/cache entry, an active batch's metadata, or response presentation exceeds the service's byte charge.
- Python: bounds dataset retention by entry count and response/read work by value counts, with no equivalent aggregate metadata or response byte budget.
- Rust: retains at most 256 MiB across cached dataset metadata, key strings and cache-entry bookkeeping, in addition to `DYNAMICAL_API_DATASET_CACHE_ENTRIES` (default 32). Opaque handle weights come from `OpenedDataset::metadata_bytes()`. Main uses `DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS` (default 30); pinned IDs have no TTL. LRU eviction preserves request-owned Arcs. Oversized entries are usable without retention. A batch admits at most 256 MiB of unique active dataset metadata; the next currently opening dataset may temporarily add its separately bounded 64 MiB. Response selection/presentation/encoding has a conservative 64 MiB charge, including 512 bytes per selected value, selected time labels, member layouts and copied product/variable/unit strings. A second allocation-free JSON byte preflight precedes encoding. Exceeding active metadata or response bounds returns a 422 detail; opened-metadata failures retain the adapter's generic internal-error mapping.
- Selection admission: reserve the cumulative response charge before copying normalized labels or per-run options, then reserve 1,536 bytes before each selected timestamp/index/lead-hour vector insertion (including vector growth slack), and 512 bytes per response value before presentation/loading. Static product, variable, unit and member charges precede option copies. All queries and initialization runs share one budget. Forecast valid-time arithmetic is checked without allocating a full-axis temporary; initialization matches are counted before allocating the at-most-ten-run vector. Validated request storage and opened metadata have their separate bounds.
- Consequence: large batches or labels can fail despite fitting the value budgets. Byte admission deliberately precedes the allocations it protects, so an oversized label or selected-time sequence can return the byte-budget 422 before a later variable/dimension/response-value error; ordinary requests retain reference error precedence. The retained-cache cap is about several maximum-size opened datasets; ordinary smaller datasets still use the entry limit. Per-request active metadata and response work remain bounded after cache eviction. Reader decode/output scratch has its existing independent byte limits; upstream Icechunk manifests, transport/internal caches and whole-process RSS are not claimed by these charges (see D13 and `docs/reader.md`).
- Rationale: make service-owned memory finite while allowing concurrent request lifetimes independent of cache eviction. A 256 MiB cache leaves headroom for admitted reads and presentation rather than allowing 32 maximum-size metadata handles to retain 2 GiB.
- Tests: `byte_weight_evicts_lru_and_never_retains_oversized_handles`, `main_and_pinned_snapshots_share_the_configured_lru_capacity`, `response_byte_admission_rejects_large_labels_despite_tiny_value_count`, and `response_json_byte_bound_covers_escaped_text_and_numeric_extremes` use explicit byte weights, synthetic labels and encoder invariants. The isolated allocator test `multiple_runs_admit_label_copies_before_allocation` asserts peak new allocation below 4 MiB for ten-run requests with 8 MiB and 512 KiB variable names; the latter admits early runs and rejects the aggregate before all ten copies. It also checks selected forecast/analysis vectors against a smaller explicit charge. Native HTTP fixture tests cover cache hits, 30-second revalidation, disabled reuse and cancellation during data reads.

## D15 — Per-selection loading without physical co-tile vectorization

- Trigger: multiple point selections touch the same source chunks.
- Python: enables grouped vectorized loads by default, bounded by its grouping settings.
- Rust: uses `dynapi-query`'s existing per-selection executor, with grouping disabled in the server. Every selection and response-value charge precedes the aggregate nominal read-budget charge, and every data load follows that admission. The service does not claim a physical read discount or use grouping settings to change the budget.
- Consequence: ordered response bytes, work-unit charges and timing phase names are preserved, but repeated chunk reads and timing durations may differ. Representation routing remains off for both `auto` and `requested`; physical co-tile optimization is deferred.
- Rationale: integrate the admitted fallback executor without introducing a new storage execution strategy.
- Tests: `windows_runs_ensembles_and_representations_follow_fixture_recipes`, `batch_response_budget_precedes_read_budget_and_later_product_lookup` and `both_budgets_and_selection_errors_precede_loading` cover restored response order, representation compatibility and admission ordering.
## D16 — Scope of the remaining HTTP milestone stubs

This entry narrows D5: catalog summary/detail and OpenAPI/Swagger UI/OAuth
redirect/ReDoc routes now have native handlers. They no longer return the
internal 501 milestone response or consume compute slots. Execution operations
remain pending: analyses and forecast runs. Forecast and canonical point
queries have native handlers.
Their schemas are included now so the complete documentation contract remains
available while those handlers land.

Catalog and documentation responses intentionally have no ETag or conditional
304 behavior, matching the reference service. Successful catalog GETs carry
`public, max-age=300, stale-while-revalidate=600`; catalog errors, HEAD/405,
OPTIONS and redirects do not. Documentation GET/HEAD responses carry no cache
policy. Method/header field order retains D7, including `GET, HEAD` on docs 405s.
Catalog input restrictions remain D11. There are no new intended wire differences
for the catalog/documentation surface. Rust-owned fixture and loopback tests in
`catalog_documentation.rs` and schema invariant tests in `openapi.rs` cover this
surface; they do not qualify the pending execution handlers.

## D17 — Bounded catalog response encoding

- Trigger: a catalog list or detail projection would encode more than
  `MAX_CATALOG_RESPONSE_BYTES` (1,048,576 bytes, inclusive limit).
- Reference: materializes the complete response without an aggregate output cap.
- Rust: borrows one published catalog snapshot and projects response fields into
  a bounded counting sink before allocating any response tree or body buffer.
  It then allocates one exactly sized bounded body and encodes the same immutable
  projection. No collection, unused metadata or intermediate JSON tree is copied.
- Oversize behavior: the existing generic 500 response, `Internal Server Error`,
  with connection closure and no Cache-Control, ETag or Retry-After. A separate
  unsampled `api_error` record carries the fixed error category
  `catalog_response_too_large` and request ID; no limit or catalog data is exposed
  in the response. Catalog routes remain independent of compute admission.
- Live size check, 2026-10-22 17:35 UTC: the public endpoint
  `https://api.dynamical.org/v1/data-products` returned 36 products in 17,903 body
  bytes. All 36 detail responses were measured; the largest was 55,172 bytes
  (`noaa-rrfs-forecast-84-hour-virtual`), under 5.3% of the budget.
- Tests: exact-limit/one-byte-over, escaped UTF-8/JSON and self-link ID accounting,
  aggregate list limits and hidden metadata exclusion. Current-thread native HTTP
  tests use per-thread allocation tracking to prove that 2 MiB unused dimension
  strings cause no request-sized copy, and oversized list/detail projections fail
  with total request allocations below 256 KiB and no allocation above 128 KiB.

## D18 — One catalog generation per forecast batch

- Trigger: a background catalog refresh publishes between product lookups in a
  multi-query forecast request.
- Python: looks up each product separately and may observe different published
  generations within the batch.
- Rust: lazily retains one immutable catalog snapshot at the first product
  lookup and uses it for every query in that batch. Admission and initial lookup
  error precedence are unchanged. Separate requests can observe newer snapshots;
  catalog and execution routes share the same refresh and backoff state.
- Rationale: keep one batch's product metadata consistent while allowing refresh
  publication and other requests to proceed independently.
- Test: `forecast_batch_retains_one_catalog_generation_during_refresh` blocks the
  first dataset open, publishes a new generation, and verifies both batch results
  retain the first generation while subsequent catalog/forecast requests see the
  new one. Removing the snapshot pin makes this regression fail.

## D19 — Native error categories and explicit SDK context

- Trigger: an unexpected request/server failure or application startup failure is reported.
- Reference Python service: its framework and logging integrations capture Python exception types, messages and traceback frames, and attach scrubbed framework request context. A null caller tag can be present.
- Rust: reports bounded static error categories with native stacks captured at the reporting site. A caught panic's original unwound stack is unavailable. Request tags are copied explicitly per event; an absent caller remains null at the injectable sink and is omitted by the native SDK's string-valued tag map. No automatic request, panic, logging, tracing or context integration is installed. Request bodies, raw client addresses, headers, exception messages, panic payloads, frame locals, source text and absolute source paths are excluded entirely.
- Consequence: capture conditions and keyed correlation remain available, while exception/stack and SDK envelope metadata differ. Python exception messages and framework stack shapes are not reproduced. Startup events drain before exit. Expected client errors, cancellation and `ServerBusy` admission rejections do not become error events; catalog server failures do.
- Rationale: use the native SDK without allowing framework context or raw exception text to disclose caller payloads or credentials. A native stack still identifies the Rust reporting path.
- Tests: `telemetry_condition_fixture_preserves_nulls_routes_and_event_counts`, `authenticated_identity_and_metadata_are_shared_and_payloads_stay_private`, `authentication_encoding_failure_captures_an_untagged_error_only`, `panic_captures_once_with_request_scoped_tags`, `cancelled_request_releases_admission_and_emits_no_analytics_or_error`, `rate_and_busy_rejections_are_not_reported_as_errors`, `wire_payloads_exclude_unapproved_context_and_busy_errors`, and `startup_failure_is_reported_and_drained_before_process_exit`.

## D20 — Native heartbeat resource counters

- Trigger: a stdout `api_container` heartbeat is emitted.
- Reference Python service: emits `pythonThreads`, process CPU, admitted-request/compute counters and available cgroup throttling deltas.
- Rust: emits `nativeThreads` on Linux using the process thread count, alongside process CPU, admitted-request/compute counters and available cgroup throttling deltas. Unsupported measurements are omitted. Native threads include runtime and transport workers; there is no Python thread registry.
- Consequence: thread-count field and meaning differ. CPU deltas, compute occupancy, cumulative dataset observations and cgroup delta semantics retain their meaning. Task ownership is native: first beat follows one interval, and cancellation/drop/shutdown stop the task without retaining the application.
- Rationale: report a measured native runtime quantity rather than inventing a Python-equivalent count or zero for an unavailable source.
- Tests: `heartbeat_deltas_and_optional_measurements_are_explicit`, `heartbeat_waits_one_interval_stops_and_does_not_own_the_app`, `coldness_progresses_only_on_success_and_counts_failures_cumulatively`, and `snapshot_and_session_lru_caps_keep_the_last_eight_successful_loads`.

## D21 — Optional bounded delivery adapters

- Trigger: the native server is built with `telemetry-delivery` and a PostHog project token or Sentry DSN is configured.
- Reference Python service: uses its PostHog/Sentry SDK background transports and lifecycle shutdown.
- Rust: sends PostHog capture JSON over reqwest and uses the selective native Sentry SDK. Application enqueue is immediate and bounded to 256 records of at most 64 KiB each. Overflow/oversize events are dropped; counters saturate at `u64::MAX`. HTTP requests have a two-second timeout, do not follow redirects and do not inherit proxy settings. Failed PostHog sends are dropped without retry. The SDK's separate bounded envelope queue and rate-limit handling remain active. The telemetry phase shares a ten-second drain/flush budget and includes a four-second reserve for SDK enqueue/join: at most fourteen seconds with Sentry, ten without, all within the remaining shared process deadline (D28; 45 seconds total by default). Application draining stops before the reserve. Pending application events are counted as dropped when the drain budget expires; pending SDK envelopes may also be discarded on close. If SDK cleanup exceeds its telemetry deadline (including blocking DNS/NSS), the failure counter increments and a static stderr diagnostic notes possible SDK event loss. The executable shuts down its main runtime without waiting for blocking tasks and explicitly exits, terminating stalled SDK/resolver threads. Exact SDK losses are unavailable. Configured stdout heartbeats add application `telemetryDropped` and `telemetryFailed` counters; SDK-internal loss accounting is separate.
- Consequence: SDK-added analytics metadata, transport batching/retries and loss behavior differ. Core analytics event/properties, caller hash, nulls and capture conditions are preserved. No key/DSN means no network client, network worker or network activity; default builds omit delivery dependencies. Malformed sink configuration disables delivery using a static diagnostic, preserving stdout and request processing.
- Rationale: bound observability work and memory independently of endpoint availability, keep native delivery opt-in, and preserve the portable crate dependency boundary. Startup configuration differences remain documented in D9.
- Tests: `unconfigured_delivery_creates_no_worker_or_http_client`, `bounded_queue_drops_without_waiting_for_a_stalled_backend`, `oversized_events_are_dropped_before_the_queue`, `stalled_delivery_does_not_delay_an_http_response`, `loopback_wire_contract_and_shutdown_flush_both_destinations`, `saturated_sdk_and_stalled_endpoint_have_a_bounded_joined_shutdown`, `application_drain_and_sdk_flush_share_one_budget`, and `only_handler_success_records_are_sampled_and_all_fields_pass_through`.

## D22 — Count canonical queries consistently across telemetry streams

- Trigger: a canonical point GET enters its handler, on success or a handled query error.
- Reference: stdout records `queryCount: 1`, but analytics records `query_count: null`. Analytics reads `request.state.usage`, which the canonical handler never sets; its separate `request.state.query_count` is ignored.
- Rust: records one logical query in both streams. Requests rejected by validation before entering the handler retain a null query count.
- Rationale: preserve the known single-query count instead of copying an accidental omission from the reference analytics state.
- Tests: `canonical_analytics_counts_one_query_on_success_and_handler_error` covers success, a handler error and rejection before the handler, using synthetic committed fixtures.

## D23 — Bounded stdout delivery under backpressure

- Trigger: a full or stalled stdout pipe, an oversized terminal record, or shutdown before stdout draining completes.
- Reference Python service: synchronous flushed printing can block request completion indefinitely when the consumer stops reading.
- Rust: immediate enqueue into a 256-record queue, with one dedicated native writer and a 64 KiB serialized-record limit including the newline. Overflow and oversize records are dropped; write failures are counted. Stdout drains concurrently with network delivery under the shared telemetry drain deadline (at most ten seconds within the remaining process deadline; see D28 and [startup and shutdown](docs/readiness.md)), then pending records are counted as abandoned. Process exit ends a stalled writer; partially written in-flight records are possible.
- Observable difference: requests complete despite stdout backpressure, with bounded output loss. `telemetryDropped`/`telemetryFailed` heartbeats include stdout losses, even without network configuration once a loss occurs. A best-effort shutdown diagnostic reports stdout loss totals. Healthy terminal record fields and sampling rules are unchanged.
- Rationale: logging backpressure must not stall HTTP responses or process shutdown. No per-record blocking task or extra shutdown budget is introduced.
- Tests: `full_undrained_stdout_does_not_block_http_and_counts_loss_at_bounded_shutdown`, `overflow_size_limit_and_abandoned_records_are_counted_without_waiting_for_writer`, `heartbeat_exposes_stdout_losses_added_to_network_counters_and_drains`, and `stdout_and_network_shutdown_share_one_drain_deadline`.

## D24 — Bounded readonly repository HTTP transport

- Trigger: any reader HTTP(S) repository open or object read.
- The reference Python service uses Icechunk's native repository transport, which follows redirects and collects encoded objects without a body cap. Rust uses a private readonly Storage implementation: no redirects, exact status/range/length validation, bounded collection and no native fallback. The config and repo caps are 1 MiB, other metadata 64 MiB, stored chunks/ranges 72 MiB + 16 bytes. Oversized ranges fail before I/O; body errors fail rather than yielding fill. Repository URL userinfo/query/fragment and noncanonical object keys are rejected. Unsupported listing/writes fail explicitly.
- This intentionally rejects repositories/responses the reference might read. Header/request transport failures and 429/5xx have one retry; invalid response bodies are not retried. Conditional ETags and typed missing-object results are preserved. Metadata decompression, metadata-derived preallocation and caches inside Icechunk remain outside these encoded transport bounds, so repository contents must still be trusted. No whole-process memory bound or reader WASM support is claimed.
- Tests: Rust loopback fixtures exercise all repository object categories, redirects with zero destination requests, oversized lengths/chunked/unframed bodies, exact ranges, overflow/truncation, status retries, conditional reads, serialization and pre-I/O path/range rejection. Existing virtual container authorization tests remain in force.

## D25 — Full ensemble global-plane reads exceed the reader deadline

- Trigger: a direct stored-value `wind_u_10m` selection at one point across 64 members and 60 leads, at public WeatherNext 2 snapshots `FFKBFYQ1WAHSE16B92X0` (historical) and `ERK6V78QK32Q0Z29VXGG` (operational).
- Rust: both single selections return the internal Storage timeout at the existing 90-second deadline. The reference read-path comparison completed the historical selection after more than 90 seconds. A separate one-off Rust diagnostic completed that workload as 60 member-slice reads through one shared session, matching all output bits; this does not qualify a single full selection under the shipped deadline.
- Judgement: retain sequential decoding, per-chunk bounds and the reader deadline. Full ensemble latency qualification remains open. Coalescing does not avoid fetching and decoding the global planes. RSS was measured from actual processes; logical byte counts are not RSS or a general memory guarantee. Upstream metadata expansion still requires trusted repository contents (D24).
- Verification: network/RSS measurements are one-off development checks outside the test suite. Durable Rust tests remain offline. Full geographic coordinates and northern/southern point reads pass for both products. This change adds no server wiring or HTTP timeout contract.

## D26 — Bounded catalog wait before binding

The reference Python service launches a best-effort catalog load and binds
without waiting. Rust waits up to `DYNAMICAL_API_CATALOG_WAIT_SECONDS` (10 seconds
by default) for that first flight, then binds on success, failure or timeout.
Timeout leaves the shared load running. This improves initial readiness without
making a catalog outage prevent health/documentation serving. Set zero for an
immediate bind. Readiness logs report wait outcome and observed bind-to-ready
time. Rust subprocess tests in `tests/readiness.rs` cover all three outcomes and
a late successful load.

## D27 — Optional deployment identity, authentication and redirect scheme

`DYNAMICAL_API_RELEASE` adds an application response marker when configured,
including errors and CORS preflight. The marker is omitted from CORS exposure
and from Hyper parser errors. It is absent by default. The reference service at
the comparison revision does not add this marker. Tests in `tests/server.rs`
assert its scope and absence when unset.

`DYNAMICAL_API_REQUIRE_ORIGIN_AUTH=1` adds startup validation for a missing or
blank secret; local unauthenticated startup remains available by default.
`DYNAMICAL_API_PUBLIC_SCHEME=https` makes slash redirects use HTTPS while
preserving Host validation and ignoring forwarded headers. The default `http`
retains local reference behavior; HTTPS deployments must opt in. Fixed root
redirects and relative canonical links are unchanged. Configuration and
subprocess tests cover these deliberate deployment controls.

## D28 — Explicit bounded process drain

On SIGTERM/SIGINT Rust closes the listener, drains current requests and detached
compute permits, flushes telemetry, then closes the catalog and exits. The
configurable defaults allow 30 seconds for requests and 15 additional seconds for finalization. A
single independent watchdog enforces their sum through runtime/process teardown;
telemetry shares the absolute deadline, and startup validates its 10/14-second
minimum finalization reserve. Heartbeat/readiness work stops before drain.
Unix expiry uses lock-free `_exit(1)`. Compute permits are never released on
a caller's behalf. Forced exit may discard responses/telemetry and skip cleanup.
This explicit total bound includes stuck native work rather than relying on
runtime destruction or only an async timeout. See `docs/readiness.md` for the
telemetry integration order and the requirement to qualify the budget against
the deployment's actual shutdown grace. Rust subprocess tests verify completion,
connection refusal, stuck-handler expiry and a worker retaining its permit
through stuck synchronous cleanup.

## D29 — HRDPS rotated operation matches the reference

This is an alignment note, not an active contract deviation. Both the reference
Python service and `dynapi-projection` use the rotated-pole transform for HRDPS
geographic request selection and returned locations, including canonical point
reconstruction, while preserving stored grid indices/value representation.
HRDPS is a derived geographic CRS; its operation must be selected before any
identity-geographic shortcut. Dataset/query adapter and server wiring remain
separate work and must preserve the five identity CRS cases and D12's fail-closed
metadata policy. The library alone enables no service route.

## D30 — Projected selection is unsupported on WASM

- Trigger: construction of a projected selector on a WASM target.
- Rust: `ProjectedGrid::new` returns `UnsupportedTarget`. Compilable pure projection math is available for experiments, but neither projected family is qualified for WASM selection or serving.
- Evidence: exploratory WASM execution selected different cells at 272/1000 HRRR midpoint requests and 64/1000 HRDPS midpoint requests, and changed one HRRR edge admission. Tiny transform differences can select adjacent kilometre-scale grid cells and different weather values. This is an unsupported capability, not an accepted coordinate-accuracy relaxation. Node execution is not deployment-runtime qualification.
- Future gate: qualify the actual release selector and canonical reconstruction on the intended WASM deployment runtime, including adjacent-representable geographic boundary inputs, domain admission, indices, returned location bits/serialization and dataset metadata dispatch. Any change in cell/value/admission behavior needs an explicit contract decision before enablement; no epsilon may hide it.
- Native scope: release compiler and math runtime must be recorded for numerical comparisons. Results from one runtime do not establish universal bit-identical output, and signed zero must be checked explicitly.

## D31 — Validate projected axes before selecting

- Trigger: construction of the standalone projected-grid library with empty, nonfinite, nonmonotonic, overflowing, or out-of-range rotated axes.
- Reference: the previous selector sorts unique finite values for its bounds but scans the original arrays for argmin, potentially admitting nonmonotonic axes or selecting a NaN. It does not separately validate axis extents.
- Rust library: requires nonempty finite monotonic axes in either direction, with finite span and half-cell bounds. Rotated axes must be within longitude/latitude ranges. Duplicates are allowed and choose the first original array index on ties; singleton/constant axes have zero tolerance. Bounds use half the median spacing between unique coordinates and include their endpoints. No rounding epsilon changes admission or indices.
- Consequence: malformed axes can be rejected before selection. WKT attests only the operation; the adapter must still validate coordinate membership, dtype and units. All attested HRRR/HRDPS axes satisfy this contract. This library contract does not change existing service routes until adapter integration.
- Tests: `ascending_descending_and_duplicate_ties_choose_first_array_index`, `bounds_use_median_unique_spacing_and_are_inclusive`, `singleton_and_constant_axes_have_zero_tolerance`, and `invalid_axes_and_points_fail_before_selection` own explicit axis arrays and adjacent representable boundary values.


## D32 — Catalog decoded-unit ceiling

- The reference Python service has no equivalent 16 MiB per-float-chunk admission
  ceiling. Rust now admits up to **56 MiB (58,720,256 bytes)** per decoded float
  inner unit, covering all inventoried root arrays. IMERG analysis requires
  57,600,000 bytes, MRMS analysis 25,920,000 and HRRR analysis 17,496,000; the
  affected IFS forecast units require 17,756,160 and 18,213,120 bytes.
- The existing 64 MiB int64, 64 MiB output, 8 MiB index, selector/work and dataset
  metadata budgets stay independent. The largest new decode units are analysis
  arrays; this does not implement the analyses endpoint or increase IMERG's
  dataset metadata budget. Concurrent decode pools must budget for the largest
  admitted unit: the 64 MiB int64 ceiling, or
  `max(MAX_CHUNK_BYTES, MAX_INTEGER_CHUNK_BYTES)`. See `docs/reader.md` for the
  per-unit staging ledger.
- Tests: `float_chunk_boundary_and_in_place_scale_obey_unit_staging_charge` checks
  exact-limit decoding, allocation charge and one-float-over preflight rejection,
  with and without scale/offset. `decoded_unit_boundaries_and_zstd_window_are_fixed`
  checks exact-byte limits and the small native Zstd frame/window policy.

## D33 — Explicit catalog codec subset and lazy data variables

- The reference Python service supports more Zarr extension combinations. Rust
  adds leading float32 `scale_offset` only with NaN fill, numeric finite float32
  parameters, defaults scale 1/offset 0 and a nonzero converted scale. Division
  and addition run separately in float32. Integer/float64 transforms, string
  parameters, finite fills and transpose remain unsupported. These exclusions
  return typed Unsupported; oversized decoded units return typed SizeLimit.
- Dataset-mode rank-one int16 standalone Zstd admits only little-endian bytes,
  level 0/checksum false, <=128 decoded bytes and <=256 encoded bytes with an
  exact declared content size. Other standalone Zstd layouts retain typed
  rejection. No dependency or native-library versions change. Strict stored-value
  coordinate profiles remain separate from the dataset entry point; in particular
  a WeatherNext init_time tuple outside those profiles remains unsupported there.
- Data variables are already lazy: supported siblings remain readable when another
  variable has an unsupported codec. Required query coordinates are eager and
  may fail dataset open. With a gzip sibling, both engines open and read supported
  bytes data; the reference also reads gzip, while Rust rejects its selection.
  For a codec neither engine resolves, the reference can fail the whole open
  while Rust keeps supported siblings readable. This more lenient open is an
  intentional deviation. HRDPS's auxiliary expected_forecast_length CF tuple
  remains outside the existing exact CF-time tuples; projected point selection
  and nested pressure-level query dimensions are separate unsupported contracts.
- Tests use synthetic Rust-owned byte recipes, explicit decoded values and dtype
  precision invariants; no captured reference output is committed. See
  `catalog_encodings.rs` and `docs/reader.md` for codec and frame cases.

## D34 — Attested projected operations in native point routes

- Supersedes D12's historical classification/unsupported status for the two exact
  operation WKT inputs only. `SpatialCrs::Attested(ProjectionKind)` selects HRRR
  Lambert conformal or HRDPS rotation before the five geographic identities.
  Unknown WKT stays `Projected`, missing/false CRS stays `Missing`, coordinate
  membership and `crs_wkt` truthy precedence remain unchanged.
- POST forecasts and canonical GET share validated `ProjectedGrid` construction,
  preserve stored y/x indices, and return inverse-transformed geographic points.
  Out-of-grid/projection-domain requests return the existing 422 location detail.
  Query-layer index bounds are checked before CRS/grid validation; the raw-storage
  guard is an open-time metadata error that precedes request-level index checks.
- Beyond D31's axis requirements, projected coordinates require actual float64
  storage and decoded float64 dimension coordinates. Supplied HRRR units must be
  `m`, `metre`, `meter`, `metres`, or `meters`; HRDPS uses `degrees`. Absent unit
  attributes retain the attested WKT's native units. Other units/types fail closed
  as metadata errors (500), without conversion. The reference service does not
  impose these metadata guards; the attested catalog products satisfy them.
- D30 is unchanged: valid projected query/canonical paths fail closed on WASM.
  Rust-owned tests cover both operations, stored-value selection, canonical and
  ETag invariants, out-of-domain errors, dtype/units/membership, identity/CRS/index
  precedence and the unsupported-target path. No service captures supply expected
  public test results.

## D35 — Private bounded GRIB qualification rejects unqualified constants

- The unwired native GRIB module validates complete f64 data planes before calling
  pinned gribberish 1.5.0. It admits qualified 5.0/5.3 profiles within 32 MiB encoded
  and 56 MiB padded decoded limits. It deliberately rejects zero-bit 5.0 fields,
  empty bitmap streams, and zero-group 5.3 fields; zero descriptor octets are
  rejected as an unsupported width rather than a constant field. The reference
  codec can return wrong zeros or panic for these cases. A typed rejection is an
  intentional safe failure, not a claim of Python exception or HTTP status parity.
- Additional width, integer-intermediate, f32 table-alignment, scan, scaling and
  ignored-reference restrictions are explicit in [the qualification contract](docs/reader-grib.md).
  Valid but unqualified inputs remain unsupported; no general decoder replacement,
  CCSDS/image admission, public API, reader wiring or product enablement follows.
- The Lambert data path admits messages whose reference coordinate projector
  would fail or panic, because ordering does not construct that projector.
  Coordinate/CRS validity must be gated at integration before projected selection.
- Rust-owned constructed fixtures establish admitted numeric/missing/order/scaling
  behavior and constant rejections. Allocation tests include retained input through
  result conversion and distinguish peak live requests from cumulative allocations.
  They do not establish shared-pool, native/RSS or whole-process bounds.

## D36 — Container identity in repository and dataset cache keys

Cache keys include the sorted virtual-container type/prefix tuples as well as
product, repository location and profile (and snapshot for datasets). A container
recipe change, including proxy-backed to direct access, opens a new handle even
when product, location and snapshot are unchanged. Container key storage is
included in the dataset cache's byte charge. This matches the reference Python
service and prevents a caller-location check from being bypassed by a handle
opened under an older recipe. Reordering identical declarations keeps the same
identity. Rust-owned fixtures cover dataset/repository misses and byte bounds.

## D37 — Viewer trust and gate placement

Location is trusted only when both origin authentication and viewer authentication
match the configured origin secret, matching the reference Python service's
stronger trust rule. Forecast POST and canonical GET admit compute before catalog
resolution and caller-location checks, preserving busy-before-catalog/lookup
errors; all batch products are checked before any dataset open. The exact
immutable catalog generation checked is passed to the worker. Run listing and analyses remain stubs: their checks run first, allowed
requests retain 501, and refused/unavailable requests return 451/503.
The stubs now return 404 for unknown IDs, analyses reads/parses at most 16 MiB
before its stub, and hidden forecast products skip the check and return 404
without the gated privacy override, unlike the reference's refusal/unavailable
response for gated hidden products; no data is served in these cases.

## D38 — Bounded and coalesced location checks

The reference Python service has no refusal-body or byte-cache ceiling and does
not coalesce concurrent checks. Rust limits encoded and decoded refusal text to
16 KiB, cached decisions to 1024 entries and 4 MiB of charged owned key/detail
and entry storage, and distinct in-flight checks to 16. Identical requests share
one result task. Caller cancellation does not create replacement tasks: each
remains charged to that cap until completion, with a five-second whole-operation
deadline. Oversize, capacity, timeout and transport failures return retryable 503
and are never cached. Redirects are never followed. Only 204/451 are cached for
300 seconds from check start; expiry also uses the injected monotonic clock.

## D39 — Response privacy and secret handling

After identifying a gated product, all subsequent responses override cache policy
with `private, no-store`, including canonical responses and query/read failures.
Compute rejection precedes product identification on forecast POST and canonical
GET. Earlier schema/auth/rate errors retain their existing order and policy, matching the reference's identification boundary. Successful ETags
remain. The configured check secret has redacted Debug output, sensitive HTTP
header storage and static diagnostics. A refusal echoing the secret fails closed
to generic 503 rather than copying it into a public detail or telemetry.
