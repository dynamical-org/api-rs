# Public contract coverage

Audited at Rust revision `f1b43f2` against reference Python service revision
`203ec99` (2026-10-23): routes and OpenAPI, middleware/error handlers, catalog
families, dataset encodings, representation routing and telemetry.

**covered** means the linked Rust-owned test asserts the stated behavior;
**deviation** links an intentional difference in [DEVIATIONS.md](../DEVIATIONS.md);
**gap** links an open issue for missing implementation or assertions;
**not applicable** means that contract axis does not apply to the route.
Component tests establish only their stated component behavior, not complete HTTP
or product coverage. A passing spot test does not certify every variable, snapshot
or codec combination. Test links identify source files and function names; names
and assertions were checked against the workspace test listing and source.

The [measured product sweep](https://github.com/dynamical-org/api/issues/7#issuecomment-6361528722)
is separate migration evidence, not a committed fixture or a cutover sign-off.
The service still has gaps and temporary 501 handlers. D16 narrows D5's older
stub inventory; D34 supersedes D12's unsupported status for the two attested
native projected grids. Tables below use that current scope.

## Static, health and documentation routes

Product, dtype and snapshot axes are **not applicable** to these responses.
"Other methods" below refers to the explicitly listed methods, not an exhaustive
HTTP-token sweep. CORS preflight is distinct from plain OPTIONS throughout.

| Route | Method / capability or error | Status | Evidence |
| --- | --- | --- | --- |
| `/` | GET, HEAD: 301 to `https://dynamical.org/api/`, empty body | covered | [server.rs::health_and_root_have_explicit_responses][server] |
| `/robots.txt` | GET, HEAD: embedded bytes, empty HEAD, length/type, ETag, Last-Modified, valid Date and distinct UUID4 IDs | covered | [server.rs::robots_get_head_and_packaged_metadata][server] |
| `/robots.txt` | Range and build-time file metadata differ from a runtime file response | deviation | [D8](../DEVIATIONS.md#d8--compiled-robots-metadata-and-limited-file-response-surface) |
| `/healthz` | GET: exact JSON body, type and length | covered | [server.rs::health_and_root_have_explicit_responses][server] |
| `/healthz` | HEAD/OPTIONS and root/robots unsupported-method status/body/Allow matrix | gap | [#10](https://github.com/dynamical-org/api/issues/10), server response-surface coverage; the GET/HEAD tests above do not assert these cases |
| `/healthz/` | GET: 307; omit empty query and preserve nonempty query bytes | covered | [server.rs::slash_redirect_omits_empty_query_and_preserves_nonempty_query][server] |
| `/openapi.json`, `/docs`, `/docs/oauth2-redirect`, `/redoc` | GET, HEAD: 200, media type, length, empty HEAD, no cache/ETag/304 | covered | [catalog_documentation.rs::documentation_get_head_options_and_redirect_contract][catalog-docs] |
| Same four documentation routes | OPTIONS, POST, PUT, PATCH, DELETE: 405 and Allow GET/HEAD; slash forms: 307 | covered | [catalog_documentation.rs::documentation_get_head_options_and_redirect_contract][catalog-docs] |
| `/docs`, `/docs/oauth2-redirect`, `/redoc` | Schema link, Swagger bundle, OAuth callbacks and ReDoc bundle/link | covered | [catalog_documentation.rs::documentation_links_and_oauth_flow_are_usable][catalog-docs]; these assert generated HTML, not browser/CDN availability |
| `/openapi.json` | Route/method inventory and explicit schema exclusions | covered | [openapi.rs::router_and_document_inventory_agree_both_ways_with_explicit_exclusions][openapi] |
| `/openapi.json` | Resolving schema references, required fields, aliases/defaults, request bounds and representation literals | covered | [openapi.rs::every_schema_reference_resolves_and_required_properties_exist][openapi]; [openapi.rs::forecast_property_aliases_defaults_and_required_fields_agree_with_validation][openapi]; [openapi.rs::shared_request_bounds_and_literals_are_enforced_at_the_schema_boundaries][openapi] |
| `/openapi.json` | Response unions, nullability, routing schema and Retry-After error schemas | covered | [openapi.rs::response_schemas_include_unions_nullability_routing_and_retry_headers][openapi]; schemas do not establish implementation of analyses, runs or automatic routing |
| Documentation routes | Origin-auth 403, CORS on rejection, preflight 200/400 without catalog fetch | covered | [catalog_documentation.rs::origin_guard_and_cors_preflight_cover_every_new_route][catalog-docs] |
| All routes | Header field ordering and deterministic two-method Allow ordering | deviation | [D7](../DEVIATIONS.md#d7--http-field-ordering-and-process-dependent-method-set-order) |

## Catalog discovery: `/v1/data-products` and `/v1/data-products/{dataProductId}`

| Method | Product/capability × error or middleware | Status | Evidence |
| --- | --- | --- | --- |
| GET list | Ordered visible products; forecast/analysis/unsupported-grid capabilities, nullable metadata, mutable cache policy; independent of compute admission | covered | [catalog_documentation.rs::catalog_listing_preserves_order_visibility_and_capabilities_without_compute_slots][catalog-docs] |
| GET detail | Variable metadata/order, bbox numeric spelling/nonfinites and nullable fields | covered | [catalog_documentation.rs::catalog_detail_includes_nullable_metadata_and_preserves_numeric_and_field_order][catalog-docs] |
| GET detail | Unknown/hidden/escaped identifiers: public 404 envelope and no cache/ETag/retry headers | covered | [catalog_documentation.rs::unknown_and_hidden_products_use_the_public_error_envelope][catalog-docs] |
| GET list/detail | Failed initial refresh or cold timeout: 503 detail, CORS, no cache/ETag/Retry-After | covered | [catalog_documentation.rs::catalog_load_failure_and_cold_timeout_are_503_without_cache_or_retry_after][catalog-docs] |
| HEAD, POST, PUT, DELETE, PATCH | 405 Allow GET, HEAD body suppression, no catalog load | covered | [catalog_documentation.rs::catalog_methods_and_redirects_do_not_load_the_catalog][catalog-docs] |
| OPTIONS | Plain 204 with empty body; Origin gets simple CORS | covered | [catalog_documentation.rs::catalog_methods_and_redirects_do_not_load_the_catalog][catalog-docs] |
| GET, HEAD, OPTIONS, POST on slash forms | 307 with preserved query, no catalog load/cache headers | covered | [catalog_documentation.rs::catalog_methods_and_redirects_do_not_load_the_catalog][catalog-docs] |
| GET list/detail and forecast POST | Startup warm/single-flight, failure backoff, stale generation serving and refresh publication shared across routes | covered | [catalog_lifecycle.rs::startup_catalog_and_forecast_share_one_root_child_flight_and_warm_snapshot][catalog-life]; [catalog_lifecycle.rs::startup_failure_backoff_and_refresh_publication_are_common_to_both_routes][catalog-life] |
| Catalog refresh component | Failed stale refresh preserves snapshot; backoff cap; returned objects are isolated | covered | [refresh.rs::stale_snapshot_is_preserved_backoff_caps_and_returned_values_are_isolated][refresh] |
| Catalog refresh component | Deadline/close/retired flights cannot publish | covered | [refresh.rs::deadline_close_and_retired_generation_cannot_publish][refresh] |
| Catalog input component | Restricted STAC input, URL/host/security and parser contract | deviation | [D11](../DEVIATIONS.md#d11--stac-catalog-input-contract-and-request-targets); broader skip/abort/coercion graph assertions remain [#10](https://github.com/dynamical-org/api/issues/10) |
| GET list/detail | Encoding over 1 MiB fails with bounded generic 500; unused metadata stays borrowed | deviation | [D17](../DEVIATIONS.md#d17--bounded-catalog-response-encoding) |

## Forecast batches: `POST /v1/forecasts`

| Product/capability | Error, middleware or response | Status | Evidence |
| --- | --- | --- | --- |
| Deterministic rectilinear | Nearest point, exact integer values above 2^53, units, static values, float edge values/null, canonical result parity and mutable cache | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| Latest, explicit and ranged initialization requests; lead/valid-time windows; ensembles | Successful fixture cases and canonical result equality; explicit initialization, two ranged runs and lead limit assertions | covered | [forecasts.rs::windows_runs_ensembles_and_representations_follow_fixture_recipes][forecasts]; this does not assert every selected time/value |
| Temporal selection breadth | Exact latest-run choice and expected time/value masks at all inclusive/exclusive, NaT, duplicate and fractional boundaries | gap | [#10](https://github.com/dynamical-org/api/issues/10); successful window requests and canonical equality alone do not prove each mask |
| Request schema | Shared batch/variable/location/label bounds and aliases/defaults | covered | [openapi.rs::shared_request_bounds_and_literals_are_enforced_at_the_schema_boundaries][openapi]; [openapi.rs::forecast_property_aliases_defaults_and_required_fields_agree_with_validation][openapi] (validation component) |
| Request decoding/schema breadth | Complete media-type/UTF-8/16/32, coercion, null/missing, digit/datetime and multi-error tables | gap | [#10](https://github.com/dynamical-org/api/issues/10), contract schema/decoding work packages |
| Deep JSON, surrogates and typed formatting extremes | Deliberately bounded parsing and typed output domains | deviation | [D1](../DEVIATIONS.md#d1--fixed-json-recursion-guard), [D2](../DEVIATIONS.md#d2--reject-lone-unicode-surrogates-during-parsing), [D3](../DEVIATIONS.md#d3--preserve-nonzero-sub-minute-datetime-offsets), [D4](../DEVIATIONS.md#d4--wider-typed-integer-output-domain) |
| Product/variable selection | Unknown product 404; analysis on forecast endpoint 422; blank variable list rejected before repository open | covered | [forecasts.rs::product_variable_snapshot_and_canonical_validation_errors_are_public_only][forecasts]; [forecasts.rs::blank_variables_are_rejected_before_any_store_open][forecasts] |
| Catalog failure | 503 without Retry-After; query/product context retained in failure analytics | covered | [forecasts.rs::catalog_not_loaded_is_503_and_failure_usage_retains_handler_context][forecasts] |
| Response/read-value budgets | 422; variable selection error before aggregate read budget; response budget before later product lookup | covered | [forecasts.rs::both_budgets_and_selection_errors_precede_loading][forecasts]; [forecasts.rs::batch_response_budget_precedes_read_budget_and_later_product_lookup][forecasts] |
| Admission order | Rate debit before validation; validation before compute; busy 503/Retry-After | covered | [server.rs::validation_precedes_compute_but_follows_rate_debit][server] |
| Request/metadata/response bytes | 16 MiB body cap; metadata/cache/materialization byte bounds | deviation | [D6](../DEVIATIONS.md#d6--bounded-http-request-bodies), [D13](../DEVIATIONS.md#d13--bounded-opened-dataset-metadata), [D14](../DEVIATIONS.md#d14--byte-bounded-dataset-retention-and-response-materialization) |
| Dataset reuse | Miss/hit/30-second revalidation; immutable pinned hit; disabled cache/zero TTL miss | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts]; [forecasts.rs::disabled_dataset_cache_and_zero_ttl_never_report_hits][forecasts] |
| Dataset retention | Main and pinned entries share LRU capacity; byte eviction and oversized entry non-retention | covered | [forecasts.rs::main_and_pinned_snapshots_share_the_configured_lru_capacity][forecasts]; [cache.rs::byte_weight_evicts_lru_and_never_retains_oversized_handles][cache] (cache component) |
| Response headers | Stable repeat body/ETag, cache status, Server-Timing phase order; analytics work units equal response header | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| Success middleware | JSON content type, UUID4 request ID, wildcard CORS and exact exposed-header list | covered | [server.rs::admitted_forecast_returns_data_with_cors_and_request_id][server] |
| Typed virtual storage failures | Known timeout/SlowDown → sanitized 503/Retry-After 5; other failures → generic 500; permit released | covered | [forecasts.rs::typed_storage_failures_reach_http_without_internal_diagnostics][forecasts] |
| Actual repository chunk failure | Generic 500 without upstream diagnostics | covered | [forecasts.rs::real_upstream_read_failure_returns_generic_500][forecasts] |
| Disconnected client | Compute permit retained until physical work finishes; next request busy, then recovery | covered | [forecasts.rs::disconnected_client_holds_compute_slot_until_the_read_finishes][forecasts] |
| Refresh during multi-query batch | One catalog generation retained across selections | deviation | [D18](../DEVIATIONS.md#d18--one-catalog-generation-per-forecast-batch) |
| Co-tiled points/variables | Per-selection physical reads; no vectorized read discount | deviation | [D15](../DEVIATIONS.md#d15--per-selection-loading-without-physical-co-tile-vectorization) |
| Independent chunks/shards | Bounded concurrency, shared shard-index fetch and adjacent range coalescing qualification | gap | [#15](https://github.com/dynamical-org/api/issues/15); passing response tests do not establish physical I/O efficiency |
| HEAD, GET, PUT, PATCH, DELETE; plain OPTIONS; slash forms | Complete data-route method/Allow/body/CORS/redirect table | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10); shared middleware tests below cover only their listed cases |

### Representation modes

| Route/method | Mode × capability | Status | Evidence |
| --- | --- | --- | --- |
| POST forecasts | Omitted → `auto`; `auto` and `requested` accepted; invalid/null literal error and multi-error order | covered | [contract.rs::representation_defaults_and_literal_errors_have_owned_ordered_shapes][contract]; [forecasts.rs::product_variable_snapshot_and_canonical_validation_errors_are_public_only][forecasts] |
| POST forecasts | Routing off: omitted/auto/requested requests serve fixture data and canonical equivalents | covered | [forecasts.rs::windows_runs_ensembles_and_representations_follow_fixture_recipes][forecasts] |
| POST forecasts | Shadow mode eligibility, sampling, candidate opens, counters and unchanged anchor response | gap | [#3](https://github.com/dynamical-org/api/issues/3); D15 records that routing remains off |
| POST forecasts | Auto substitution/direct/fallback, served product identity and routing provenance | gap | [#3](https://github.com/dynamical-org/api/issues/3); the documented response schema alone is not execution coverage |
| POST forecasts | Auto-mode requested opt-out; freshness/version/grid/selection/presence checks, read-budget and deadline reserves, circuit breaker, transient-only fallback | gap | [#3](https://github.com/dynamical-org/api/issues/3) |

## Canonical points: `GET /v1/data/{dataProductId}/{snapshotId}/points/{gridIndices}`

| Product/capability | Error, middleware or response | Status | Evidence |
| --- | --- | --- | --- |
| Forecast | Generated POST link resolves to equal forecast object; immutable cache, pinned cache hit and no run-select timing phase | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| Analysis | Pinned analysis start/end window returns the expected analysis time | covered | [forecasts.rs::analysis_canonical_and_projected_outside_use_owned_http_contracts][forecasts] |
| Forecast grid indices | Decimal signs, whitespace, underscores, Unicode decimal digits; reject decimal-point/malformed forms | covered | [forecasts.rs::canonical_grid_indices_use_decimal_integer_syntax][forecasts] |
| Query validation | Missing/empty variables, bad datetime, negative/unparseable lead limit: 422 kind/location | covered | [forecasts.rs::product_variable_snapshot_and_canonical_validation_errors_are_public_only][forecasts] |
| Handler validation | Missing/naive initTime, invalid/out-of-range grid indices; absent immutable snapshot: exact detail | covered | [forecasts.rs::product_variable_snapshot_and_canonical_validation_errors_are_public_only][forecasts] |
| Snapshot transport failure | Upstream snapshot read failure stays generic 500 instead of unknown-snapshot 404 | covered | [forecasts.rs::upstream_snapshot_failure_is_not_an_unknown_snapshot][forecasts] |
| Snapshot identifier component | Malformed/absent immutable ID vs missing branch error | covered | [fixtures.rs::unknown_snapshots_have_typed_errors_and_missing_branches_remain_reader_errors][dataset-fixtures] |
| Historical immutable snapshots | Serve a prior snapshot after main advances, with old values/metadata/links and immutable caching; missing historical arrays/variables | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10); current-snapshot canonical tests do not establish history |
| Canonical error breadth | Invalid branch-name URL, variable sorting/dedup/25-variable bound, analysis window validation, encoded identifiers and transient failure HTTP tables | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10) |
| Projected forecast | HRRR/HRDPS synthetic values/indices/location, independent SHA-256 ETag, repeat stability, immutable cache and out-of-grid 422 | covered | [projected.rs::projected_forecasts_and_canonical_get_share_cells_values_locations_and_etags][server-projected] |
| HEAD, POST, PUT, PATCH, DELETE; plain OPTIONS; slash forms | Complete canonical method/Allow/body/CORS/redirect assertions | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10) |

## Pending analyses and run listing

| Route | Method × capability or error | Status | Evidence |
| --- | --- | --- | --- |
| `/v1/analyses` | POST: all 11 analysis products, batch selection/data/headers and analysis validation | gap | [#4](https://github.com/dynamical-org/api/issues/4); [D16](../DEVIATIONS.md#d16--scope-of-the-remaining-http-milestone-stubs) records the 501 stub |
| `/v1/forecasts/runs` | GET: forecast product, sorted unique runs, snapshot, limit/default and after/before windows | gap | [#4](https://github.com/dynamical-org/api/issues/4); [D16](../DEVIATIONS.md#d16--scope-of-the-remaining-http-milestone-stubs) |
| `/v1/forecasts/runs` | GET: query/type/timezone/order/unknown-product/catalog errors and mutable run-cache headers | gap | [#4](https://github.com/dynamical-org/api/issues/4); middleware tests returning 501 are not handler coverage |
| Both routes | HEAD/unsupported methods, plain OPTIONS and slash redirect response tables | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10) |

## Data-route dtype, codec and spatial families

These are shared dependencies of forecast POST, canonical GET, analysis POST and
run listing. A covered reader/dataset cell does not fill a missing route handler.
The admission details and independent byte/work budgets are in
[reader.md](reader.md) and [dataset.md](dataset.md).

| Family × capability | Layer / bounded assertion | Status | Evidence |
| --- | --- | --- | --- |
| float32 weather values | Dataset: sharded Blosc/Zstd input, Cartesian source-order values, exact widening and NaN/signed-zero/subnormal/extreme bits | covered | [fixtures.rs::forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe][dataset-fixtures]; [fixtures.rs::coordinate_dtypes_fractional_durations_and_float_bits_follow_the_recipe][dataset-fixtures] |
| float64 values | Dataset: static spatial data with reordered/duplicate longitude selection and absent run/lead dimensions | covered | [fixtures.rs::forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe][dataset-fixtures] |
| int64 values | Dataset/HTTP: integers above 2^53 retained exactly in source order and responses | covered | [fixtures.rs::forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe][dataset-fixtures]; [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| int8/int16/int32/int64 and uint8/uint16/uint32 | Dataset: member identities at extrema, plain-byte reads preserve duplicates without precision loss | covered | [integer_coordinates.rs::integer_member_widths_are_exact_including_extrema_and_duplicates][integer-coords] |
| Narrow integer data variables | Full HTTP value/presentation table for every admitted integer width and codec | gap | [#10](https://github.com/dynamical-org/api/issues/10); member-coordinate component tests do not assert every weather-variable response |
| float32/float64/int64 geographic coordinates | Dataset: exact decoded coordinate type/value; fractional lead duration and NaT | covered | [fixtures.rs::coordinate_dtypes_fractional_durations_and_float_bits_follow_the_recipe][dataset-fixtures] |
| CF int64 epoch seconds | Reader: epoch zero, NaT sentinel, negative time and checked nanosecond overflow | covered | [cf_time.rs::epoch_zero_nat_and_checked_overflow][cf-time] |
| CF float64 durations | Dataset: fractional precision and NaT from explicit recipe | covered | [fixtures.rs::coordinate_dtypes_fractional_durations_and_float_bits_follow_the_recipe][dataset-fixtures]; broader full-decoding-unit/auxiliary-duration cases remain [#10](https://github.com/dynamical-org/api/issues/10) |
| Plain little-endian bytes; width-one omitted/null endian | Dataset/reader: admitted byte forms; big-endian and relaxed multibyte forms rejected | covered | [integer_coordinates.rs::one_byte_endian_forms_keep_multibyte_and_original_api_admission_strict][integer-coords] |
| Plain Blosc/Zstd; indexed sharded Blosc/Zstd with bytes/CRC32C index | Dataset: physical float32, float64, int64 fixture reads follow numeric recipes | covered | [fixtures.rs::forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe][dataset-fixtures] |
| Sharded bytes; both index locations; all Blosc shuffle/level and memcpy/frame-corruption combinations | Complete selected-read, sparse/missing/corrupt-index and codec cross-product assertions | gap | [#10](https://github.com/dynamical-org/api/issues/10); metadata admission and happy-path sharded fixture reads do not prove this entire family |
| WeatherNext float32 Blosc/LZ4 | Reader: compressed virtual payload, exact float bits, duplicate ordering, full/sparse range and decoded-unit charge | covered | [http.rs::weathernext_lz4_virtual_chunk][reader-http] |
| Leading float32 scale_offset + bytes + Blosc | Dataset reader: separate float32 division/addition, rounded parameters, duplicate selection, NaN fill and deployed offset/scale examples | covered | [catalog_encodings.rs::leading_scale_offset_decodes_in_float32_before_selection_and_preserves_fill][encodings]; [catalog_encodings.rs::scale_offset_rounds_the_float32_quotient_before_adding_offset][encodings]; [catalog_encodings.rs::deployed_scale_offset_values_and_frame_validation_are_exact][encodings] |
| Scalar int64 standalone Zstd | Exact stored scalar/fill values and standalone frame error matrix | gap | [#10](https://github.com/dynamical-org/api/issues/10); opening a repository containing lazy spatial_ref metadata does not assert its payload decode |
| Rank-one int16 standalone Zstd | Dataset: exact member values, padded edge chunks and bounded decoded/encoded frames | covered | [catalog_encodings.rs::int16_standalone_zstd_members_open_with_exact_values_and_edge_padding][encodings]; [catalog_encodings.rs::member_zstd_decoded_bound_rejects_one_additional_int16_at_open_and_read][encodings]; [catalog_encodings.rs::member_zstd_encoded_bound_accepts_256_and_rejects_257_before_fetch][encodings] |
| Other codecs/CF tuples, transpose, finite-fill scale/offset and required unsupported coordinates | Closed subset, lazy unsupported siblings and explicit rejection | deviation | [D33](../DEVIATIONS.md#d33--explicit-catalog-codec-subset-and-lazy-data-variables); unsupported selected variables are not successful coverage |
| Decode-unit limits | 56 MiB float/64 MiB int64 ceilings; separate output/index/metadata limits | deviation | [D32](../DEVIATIONS.md#d32--catalog-decoded-unit-ceiling) |
| GRIB + scale_offset; S3/GCS virtual chunk containers | Product eligibility, GRIB decoding exactly once and end-to-end reads | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| Ensemble source order | Dataset: variable-specific member axis placement, Cartesian reordered/duplicate selections | covered | [fixtures.rs::ensemble_cartesian_reads_follow_each_variable_source_order][dataset-fixtures] |
| Ensemble physical encodings | Dataset: int16 members with Blosc typesize 4, int8/uint8 members and member-position value offsets | covered | [fixtures.rs::physical_ensemble_members_and_offsets_follow_the_recipe][dataset-fixtures] |
| Ensemble presentation | Query over physical dataset: complete two-time analysis response with exact member IDs and values | covered | [fixtures.rs::complete_ensemble_query_pairs_members_with_recipe_values][dataset-fixtures]; analysis POST itself remains a gap |
| Ensemble HTTP breadth | Exact forecast member values/IDs and member-independent static-variable broadcasting across supported physical encodings | gap | [#3](https://github.com/dynamical-org/api/issues/3), [#10](https://github.com/dynamical-org/api/issues/10); array-shape and component recipe checks do not cover all HTTP combinations |
| Unused string statistic axis | Dataset: unused strings stay lazy; required string member coordinate fails open | covered | [integer_coordinates.rs::unused_string_dimension_is_lazy_but_selected_string_axis_fails_cleanly][integer-coords] |
| Geographic global x/y | Query: both hemispheres, longitude wrapping/seam and midpoint first-index ties | covered | [query.rs::geographic_xy_global_grid_normalizes_both_hemispheres_and_seam][query] |
| Native Lambert conformal/rotated pole | HTTP: both attested operations preserve stored indices, numeric values, geographic locations and canonical responses | covered | [projected.rs::projected_forecasts_and_canonical_get_share_cells_values_locations_and_etags][server-projected] |
| Projected metadata / malformed axes / WASM | Exact WKT dispatch and raw float64/unit guards; finite monotonic axes; projected selection unsupported on WASM | deviation | [D34](../DEVIATIONS.md#d34--attested-projected-operations-in-native-point-routes), [D31](../DEVIATIONS.md#d31--validate-projected-axes-before-selecting), [D30](../DEVIATIONS.md#d30--projected-selection-is-unsupported-on-wasm) |
| Nested pressure-level groups | Outside root point-query dimensions; no claim of pressure-level point support | not applicable | [D33](../DEVIATIONS.md#d33--explicit-catalog-codec-subset-and-lazy-data-variables); [reader contract](reader.md#catalog-units-and-staging-ledger) |

### Measured public products

The 2026-10-23 [#7 sweep](https://github.com/dynamical-org/api/issues/7#issuecomment-6361528722)
reports **12 of 25 forecast products** serving equal status, body bytes and ETag
for small requests at this Rust revision. Discovery/list and all 36 product detail
routes also matched, as did OpenAPI and Swagger UI. Those measurements do not
establish every variable, codec, time range or historical snapshot, nor the
remaining method/CORS/error combinations. Each row below keeps an explicit gap
for that product-level qualification; family-level fixture evidence is above.

| Product IDs / family | Measured execution | Status of remaining product coverage | Evidence / owner |
| --- | --- | --- | --- |
| `noaa-gfs-forecast`, `noaa-gefs-forecast-35-day` | Small forecast requests matched | gap | [#7](https://github.com/dynamical-org/api/issues/7), [#10](https://github.com/dynamical-org/api/issues/10): exhaustive variable/snapshot combinations |
| `ecmwf-aifs-single-forecast`, `ecmwf-aifs-ens-forecast` | Small forecast requests matched | gap | [#7](https://github.com/dynamical-org/api/issues/7), [#10](https://github.com/dynamical-org/api/issues/10) |
| `ecmwf-ifs-ens-forecast-15-day-0-25-degree`, `ecmwf-ifs-ens-forecast-46-day-daily-1-5-degree`, `ecmwf-ifs-ens-forecast-46-day-6-hourly-1-5-degree` | Small forecast requests matched | gap | [#7](https://github.com/dynamical-org/api/issues/7), [#10](https://github.com/dynamical-org/api/issues/10) |
| `dwd-icon-eu-forecast-5-day` | Small forecast request matched | gap | [#7](https://github.com/dynamical-org/api/issues/7), [#10](https://github.com/dynamical-org/api/issues/10) |
| `noaa-hrrr-forecast-48-hour`, `eccc-hrdps-forecast` | Projected forecast and canonical comparisons matched | gap | [#7](https://github.com/dynamical-org/api/issues/7), [#10](https://github.com/dynamical-org/api/issues/10): broader release-runtime/boundary/product coverage |
| `google-weathernext2-forecast-historical-virtual`, `google-weathernext2-forecast-operational-virtual` | Small forecast requests matched | gap | [#9](https://github.com/dynamical-org/api/issues/9): full HTTPS qualification; [#15](https://github.com/dynamical-org/api/issues/15): large member/lead deadline; [#13](https://github.com/dynamical-org/api/issues/13): caller-location gate |
| `noaa-gfs-forecast-virtual`; `noaa-gefs-forecast-10-day-0-25-degree-virtual`, `noaa-gefs-forecast-16-day-0-5-degree-virtual`, `noaa-gefs-forecast-35-day-0-5-degree-virtual` | Not served: virtual GRIB eligibility refused | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-hrrr-forecast-18-hour-virtual`, `noaa-hrrr-forecast-48-hour-virtual`; `noaa-rrfs-forecast-sub-hourly-virtual`, `noaa-rrfs-forecast-18-hour-virtual`, `noaa-rrfs-forecast-84-hour-virtual`, `noaa-rrfs-ens-forecast-virtual` | Not served: virtual GRIB eligibility refused | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-refs-forecast-hourly-virtual`, `noaa-refs-forecast-3-hourly-virtual`; `ecmwf-aifs-single-forecast-virtual` | Not served: virtual GRIB eligibility refused (13 forecast products total across the three rows) | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-gfs-analysis`, `noaa-gfs-analysis-virtual`; `noaa-gefs-analysis`, `noaa-gefs-analysis-0-25-degree-virtual` | Analysis POST remains 501 | gap | [#4](https://github.com/dynamical-org/api/issues/4); virtual GRIB dependency also [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-hrrr-analysis`, `noaa-hrrr-analysis-virtual`; `noaa-mrms-conus-analysis-hourly` | Analysis POST remains 501 | gap | [#4](https://github.com/dynamical-org/api/issues/4); native projected support does not complete analysis serving |
| `nasa-imerg-analysis-early`, `nasa-imerg-analysis-late`; `ucsb-chc-chirps-analysis-final`, `ucsb-chc-chirps-analysis-preliminary` | Analysis POST remains 501 (11 products total across these three rows) | gap | [#4](https://github.com/dynamical-org/api/issues/4); IMERG also exceeds the unchanged opened-metadata bound in [D13](../DEVIATIONS.md#d13--bounded-opened-dataset-metadata); no claim that all analysis products work through canonical GET |

## Shared data-route middleware and transport

| Route/method scope | Capability × error / precedence | Status | Evidence |
| --- | --- | --- | --- |
| OPTIONS preflight, including unknown paths | Origin + Access-Control-Request-Method bypass origin auth, rate and telemetry; plain OPTIONS does not | covered | [server.rs::preflight_bypasses_auth_rate_and_telemetry][server] |
| CORS policy component | Allowed preflight vs disallowed method/headers; simple response vs unhandled 500 exposed headers | covered | [policy.rs::cors_preflight_is_distinct_from_simple_and_500][policy] |
| Query paths and canonical prefix | Decoded middleware path determines query/data limiter without changing routing; encoded suffixes tested | covered | [server.rs::middleware_url_path_controls_rate_without_changing_routing][server] |
| Forecast POST / encoded path variants | Preflight → auth → rate precedence, including exhausted buckets | covered | [server.rs::middleware_url_path_preserves_preflight_and_origin_auth_precedence][server] |
| Forecast POST | Authenticated Cloudflare address/worker rate keys, first duplicate auth header wins | covered | [server.rs::origin_auth_controls_cf_rate_keys_and_first_duplicate_header_wins][server] |
| Rate-limiter component | Token refill/retry rounding/clock regression and bounded client LRU | covered | [policy.rs::token_refill_retry_and_clock_regression_match_python][policy]; [policy.rs::rejected_clients_refresh_lru_and_eviction_is_bounded][policy] |
| Redirecting paths | Configured scheme + validated Host; ignored forwarded headers; accepted-socket fallback | covered | [server.rs::public_redirect_scheme_uses_host_and_ignores_forwarded_headers][server]; [server.rs::slash_redirect_falls_back_to_accepted_socket_authority][server] |
| Application responses | Optional release marker / required origin auth / startup validation | deviation | [D27](../DEVIATIONS.md#d27--optional-deployment-identity-authentication-and-redirect-scheme), [D9](../DEVIATIONS.md#d9--startup-configuration-validation) |
| Data products using `wn.dynamical.org` | Authenticated viewer location, pre-compute gate, 451/503/retry, fail-closed cache and private no-store responses | gap | [#13](https://github.com/dynamical-org/api/issues/13); newer contract requirement, not present in the pinned reference revision |
| Native HTTP/1 | Hyper parser behavior, bounded connections/header/body/idle deadlines | deviation | [D10](../DEVIATIONS.md#d10--hyper-http1-wire-parsing-and-bounded-native-connections) |
| HTTPS repository / virtual payload reader | No redirects, exact prefix/range/length checks, response caps; trusted repository metadata expansion remains | deviation | [D24](../DEVIATIONS.md#d24--bounded-readonly-repository-http-transport); payload/component tests do not qualify an arbitrary remote repository |
| HTTP virtual payload reader | Declared-prefix success, duplicate values and exact requested byte range | covered | [http.rs::allowed_virtual_chunk_http_repository][reader-http] |
| HTTP virtual payload reader | Undeclared prefix, redirect, mismatched range and chunked overflow rejection | covered | [http.rs::undeclared_virtual_chunk_is_not_fetched][reader-http]; [http.rs::redirect_cross_origin_is_not_followed][reader-http]; [http.rs::inconsistent_content_range_is_rejected][reader-http]; [http.rs::oversized_chunked_body_is_stopped][reader-http] |
| Large WeatherNext ensemble | Full global-plane member/lead selection exceeds sequential reader deadline | deviation | [D25](../DEVIATIONS.md#d25--full-ensemble-global-plane-reads-exceed-the-reader-deadline); qualification remains [#9](https://github.com/dynamical-org/api/issues/9), [#15](https://github.com/dynamical-org/api/issues/15) |

## Telemetry conditions

These rows describe fixture events and sink behavior, not production event counts.
Network delivery requires the separate `telemetry-delivery` build feature.

| Route / condition | Expected observation | Status | Evidence |
| --- | --- | --- | --- |
| Static/health, preflight, origin failure, validation failure and unmatched routes | Terminal/analytics/error emission or suppression, route labels, null fields and status | covered | [telemetry.rs::telemetry_condition_fixture_preserves_nulls_routes_and_event_counts][telemetry] |
| Forecast success | Stdout products/query count/bytes/timings/work units; analytics usage subset excludes bytes/timings | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| Forecast error with success sampling disabled | Failure unsampled; analytic status; CORS on encoding failure | covered | [server.rs::failure_events_are_unsampled_and_analytics_conditions_match][server] |
| Canonical handler success/error | Count one query in terminal and analytics; pre-handler query-validation failure retains null query count | deviation | [D22](../DEVIATIONS.md#d22--count-canonical-queries-consistently-across-telemetry-streams) |
| Authenticated identity | Same HMAC caller across sinks; no secret/header/body/coordinates leakage in tested records | covered | [telemetry.rs::authenticated_identity_and_metadata_are_shared_and_payloads_stay_private][telemetry] |
| Rate/busy rejection, catalog server error | 429/admission 503 excluded from errors; catalog 503 captures CatalogUnavailable | covered | [telemetry.rs::rate_and_busy_rejections_are_not_reported_as_errors][telemetry]; [telemetry_data.rs::failed_loads_count_without_warming_and_catalog_busy_markers_are_distinct][telemetry-data] |
| Catalog GET failure / cold timeout | Exact HTTP detail and one terminal/analytics/error event per request | covered | [telemetry_catalog_docs.rs::catalog_metadata_failures_keep_the_catalog_error_category_and_http_contract][telemetry-catalog]; [telemetry_catalog_docs.rs::catalog_metadata_cold_waits_keep_the_catalog_error_category_and_http_contract][telemetry-catalog] |
| Documentation unsupported methods | 405, ordered Allow, one unrouted failure and no analytics | covered | [telemetry_catalog_docs.rs::rejected_documentation_methods_have_no_matched_telemetry_route][telemetry-catalog] |
| Unhandled panic / auth encoding failure | One scoped panic error; auth encoding failure only emits untagged error | covered | [telemetry.rs::panic_captures_once_with_request_scoped_tags][telemetry]; [telemetry.rs::authentication_encoding_failure_captures_an_untagged_error_only][telemetry] |
| Cancelled request | One cancelled terminal record; no analytics/error; admitted count released while detached compute remains occupied | covered | [telemetry.rs::cancelled_request_releases_admission_and_emits_no_analytics_or_error][telemetry]; [telemetry_data.rs::cancelled_data_request_does_not_publish_late_success_and_keeps_worker_metrics][telemetry-data] |
| Dataset open/load observations | Successful cold→warm counters, measured durations and aggregate heartbeat without variable/snapshot/coordinate leakage | covered | [telemetry_data.rs::dataset_observations_feed_success_failures_and_cumulative_heartbeats][telemetry-data] |
| Native errors / heartbeat resources | Static error categories/native stacks and nativeThreads replace Python-specific context | deviation | [D19](../DEVIATIONS.md#d19--native-error-categories-and-explicit-sdk-context), [D20](../DEVIATIONS.md#d20--native-heartbeat-resource-counters) |
| Optional analytics/error network delivery | Bounded queues/records, timeout/loss policy, explicit shutdown and SDK context differences | deviation | [D21](../DEVIATIONS.md#d21--optional-bounded-delivery-adapters); delivery-only tests require that feature and are separate from the harness-only test listing |
| Stdout backpressure | Bounded dropping preserves request availability; loss counters/diagnostic | deviation | [D23](../DEVIATIONS.md#d23--bounded-stdout-delivery-under-backpressure) |
| Routing/geo-gate telemetry | Shadow/auto counters/provenance and viewer-policy denial/error conditions | gap | [#3](https://github.com/dynamical-org/api/issues/3), [#13](https://github.com/dynamical-org/api/issues/13) |

## Readiness and drain

| Scope | Capability / failure | Status | Evidence |
| --- | --- | --- | --- |
| Process startup | First catalog load completes before bind; failed load still binds and forecast returns unloaded-catalog 503 | covered | [readiness.rs::first_catalog_load_finishes_before_bind][readiness]; [readiness.rs::failed_first_load_still_binds_and_serves_unloaded_catalog][readiness] |
| Process startup | Wait timeout binds without cancelling load, then reports late catalog readiness | covered | [readiness.rs::catalog_wait_timeout_binds_without_cancelling_flight_and_reports_late_readiness][readiness] |
| Startup wait policy | Bounded best-effort wait differs from immediate bind | deviation | [D26](../DEVIATIONS.md#d26--bounded-catalog-wait-before-binding) |
| SIGTERM/SIGINT | Stop acceptance, finish in-flight response, close idle keep-alive; heartbeat/readiness stop before drain | covered | [readiness.rs::sigterm_stops_accepting_then_completes_in_flight_request][readiness]; [readiness.rs::sigint_also_drains_and_closes_idle_keep_alive_connections][readiness]; [readiness.rs::sigterm_joins_heartbeat_and_readiness_before_request_drain][readiness] |
| Stuck handler/native work | Failed drain exits; detached worker retains permit through stuck cleanup; watchdog bounds process | covered | [readiness.rs::stuck_handler_cannot_hold_process_past_drain_and_finalization_bound][readiness]; [readiness.rs::detached_native_worker_keeps_permit_and_watchdog_bounds_stuck_cleanup][readiness] |
| Total process shutdown | Explicit request + finalization deadline and forced exit/loss semantics | deviation | [D28](../DEVIATIONS.md#d28--explicit-bounded-process-drain) |
| Deployment qualification | Actual platform grace, production connections/drain, reader memory and native numerical runtime | gap | [#7](https://github.com/dynamical-org/api/issues/7); subprocess fixtures and WASM compile checks do not establish deployment behavior |

## Keeping this matrix current

When a handler, product family or reference pin changes, update the affected rows
and measured product results together. Re-list tests with
`cargo-slot test --locked --workspace --features dynapi-server/harness -- --list`
and read the cited assertions; use a feature-enabled listing for delivery-only
tests. Keep gaps open until their specific assertions and migration evidence
exist. Recheck the newer caller-location contract and routing/co-tile/open-load
telemetry when adopting a later reference pin. Do not infer coverage from an
issue's closed state, a test name, an OpenAPI schema or a successful small request.

[server]: ../crates/dynapi-server/tests/server.rs
[contract]: ../crates/dynapi-contract/tests/contract.rs
[catalog-docs]: ../crates/dynapi-server/tests/catalog_documentation.rs
[catalog-life]: ../crates/dynapi-server/tests/catalog_lifecycle.rs
[openapi]: ../crates/dynapi-server/src/openapi.rs
[forecasts]: ../crates/dynapi-server/tests/forecasts.rs
[server-projected]: ../crates/dynapi-server/tests/projected.rs
[readiness]: ../crates/dynapi-server/tests/readiness.rs
[telemetry]: ../crates/dynapi-server/tests/telemetry.rs
[telemetry-data]: ../crates/dynapi-server/tests/telemetry_data.rs
[telemetry-catalog]: ../crates/dynapi-server/tests/telemetry_catalog_docs.rs
[dataset-fixtures]: ../crates/dynapi-dataset/tests/fixtures.rs
[integer-coords]: ../crates/dynapi-dataset/tests/integer_coordinates.rs
[encodings]: ../crates/dynapi-dataset/tests/catalog_encodings.rs
[reader-http]: ../crates/dynapi-reader/tests/http.rs
[cf-time]: ../crates/dynapi-reader/src/cf_time.rs
[query]: ../crates/dynapi-query/tests/query.rs
[policy]: ../crates/dynapi-policy/tests/policy.rs
[refresh]: ../crates/dynapi-catalog/tests/refresh.rs
[cache]: ../crates/dynapi-catalog/tests/cache.rs
