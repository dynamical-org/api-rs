# Public contract coverage

Route/product inventory audited at Rust revision `f1b43f2` against reference
Python service revision `203ec99` (2026-10-23): routes and OpenAPI, middleware/error handlers, catalog
families, dataset encodings, representation routing and telemetry.
The cited HTTP assertions also include follow-up tests added from `48f9abc`;
the measured product sweep retains its original revision pin. Caller-location,
integer-epoch staging and byte-sized Blosc frame assertions were refreshed against
Rust revision `05bf01e` (2026-10-23); this does not re-run the original product sweep.

**covered** means the linked Rust-owned test asserts the stated behavior;
**deviation** links an intentional difference in [DEVIATIONS.md](../DEVIATIONS.md);
**gap** links an open issue for missing implementation or assertions;
**not applicable** means that contract axis does not apply to the route.
Component tests establish only their stated component behavior, not complete HTTP
or product coverage. A passing spot test does not certify every variable, snapshot
or codec combination. Test links identify source files and function names; names
and assertions were checked against the workspace test listing and source.
**Cutover-pending** deviations still need an explicit implementation or acceptance
decision in their linked issue. Product/deployment qualification rows owned by
[#7](https://github.com/dynamical-org/api/issues/7) are tracked by that issue's
whole-contract acceptance criteria.

The [measured product sweep](https://github.com/dynamical-org/api/issues/7#issuecomment-6361528722)
is separate migration evidence, not a committed fixture or a cutover sign-off.
The service still has product and qualification gaps. D16's analysis/run stub
inventory is historical; the native handler assertions are listed below. D34
supersedes D12's unsupported status for the two attested native projected grids.
Analysis/run rows were extended from Rust-owned fixtures on 2026-10-23.

## Static, health and documentation routes

Product, dtype and snapshot axes are **not applicable** to these responses.
"Other methods" below refers to the explicitly listed methods, not an exhaustive
HTTP-token sweep. CORS preflight is distinct from plain OPTIONS throughout.
These tables describe origin behavior. Public reachability depends on the edge
route/method and header-policy alignment tracked in
[#33](https://github.com/dynamical-org/api/issues/33); an origin method-table gap
does not establish that the edge allows that request.

| Route | Method / capability or error | Status | Evidence |
| --- | --- | --- | --- |
| `/` | GET, HEAD: 301 to `https://dynamical.org/api/`, empty body | covered | [server.rs::health_and_root_have_explicit_responses][server] |
| `/robots.txt` | GET, HEAD: embedded bytes, empty HEAD, length/type, ETag, Last-Modified, valid Date and distinct UUID4 IDs | covered | [server.rs::robots_get_head_and_packaged_metadata][server] |
| `/robots.txt` | Build-time file metadata can differ from the installed runtime file; the full-file Range difference is retired | deviation | [D8](../DEVIATIONS.md#d8--compiled-robots-metadata-and-limited-file-response-surface) |
| `/robots.txt` | GET/HEAD ranges: exact single/suffix/open/empty slices, merged intervals, multipart framing/lengths, 400/416 errors and If-Range validators | covered | [server.rs::robots_single_range_has_exact_bytes_and_headers][server]; [server.rs::robots_suffix_open_empty_and_merged_ranges_preserve_file_metadata][server]; [server.rs::robots_invalid_ranges_have_exact_plain_text_errors_without_file_headers][server]; [server.rs::robots_multipart_ranges_have_exact_framing_lengths_and_head_headers][server]; [server.rs::robots_if_range_uses_exact_validators_and_bypasses_invalid_ranges_on_mismatch][server]; [server.rs::robots_range_integer_grammar_keeps_huge_operands_and_digit_guard_explicit][server] |
| `/robots.txt` | GET/HEAD HTTP `Cache-Control: public, max-age=300` | covered | [public_contract.rs::static_methods_and_robots_cache_headers_are_explicit_over_http][public-contract] |
| `/healthz` | GET: exact JSON body, type and length | covered | [server.rs::health_and_root_have_explicit_responses][server] |
| `/`, `/robots.txt`, `/healthz` | GET, HEAD, OPTIONS, POST, PUT, PATCH, DELETE: status/body/Allow/type/length/cache/Location and simple CORS; empty HEAD bodies | covered | [public_contract.rs::static_methods_and_robots_cache_headers_are_explicit_over_http][public-contract]; public edge reachability remains [#33](https://github.com/dynamical-org/api/issues/33) |
| `/healthz/` | GET: 307; omit empty query and preserve nonempty query bytes | covered | [server.rs::slash_redirect_omits_empty_query_and_preserves_nonempty_query][server] |
| `/openapi.json`, `/docs`, `/docs/oauth2-redirect`, `/redoc` | GET, HEAD: 200, media type, length, empty HEAD, no cache/ETag/304 | covered | [catalog_documentation.rs::documentation_get_head_options_and_redirect_contract][catalog-docs] |
| Same four documentation routes | OPTIONS, POST, PUT, PATCH, DELETE: 405 and Allow GET/HEAD; slash forms: 307 | covered | [catalog_documentation.rs::documentation_get_head_options_and_redirect_contract][catalog-docs] |
| `/docs`, `/docs/oauth2-redirect`, `/redoc` | Schema link, Swagger bundle, OAuth callbacks and ReDoc bundle/link | covered | [catalog_documentation.rs::documentation_links_and_oauth_flow_are_usable][catalog-docs]; these assert generated HTML, not browser/CDN availability |
| `/openapi.json` | Route/method inventory and explicit schema exclusions | covered | [openapi.rs::router_and_document_inventory_agree_both_ways_with_explicit_exclusions][openapi] |
| `/openapi.json` | Resolving schema references, required fields, aliases/defaults, request bounds and representation literals | covered | [openapi.rs::every_schema_reference_resolves_and_required_properties_exist][openapi]; [openapi.rs::forecast_property_aliases_defaults_and_required_fields_agree_with_validation][openapi]; [openapi.rs::shared_request_bounds_and_literals_are_enforced_at_the_schema_boundaries][openapi] |
| `/openapi.json` | Response unions, nullability, routing schema and Retry-After error schemas | covered | [openapi.rs::response_schemas_include_unions_nullability_routing_and_retry_headers][openapi]; schemas establish documentation; runtime assertions are listed below |
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
| GET list and forecast POST | Startup warm/single-flight, failure backoff, stale generation serving and refresh publication shared across these routes | covered | [catalog_lifecycle.rs::startup_catalog_and_forecast_share_one_root_child_flight_and_warm_snapshot][catalog-life]; [catalog_lifecycle.rs::startup_failure_backoff_and_refresh_publication_are_common_to_both_routes][catalog-life] |
| GET detail | Startup single-flight, backoff, stale serving and refresh publication under held/failed catalog loads | gap | [#10](https://github.com/dynamical-org/api/issues/10); the shared lifecycle tests request only the list and forecast routes |
| Catalog refresh component | Failed stale refresh preserves snapshot; backoff cap; returned objects are isolated | covered | [refresh.rs::stale_snapshot_is_preserved_backoff_caps_and_returned_values_are_isolated][refresh] |
| Catalog refresh component | Deadline/close/retired flights cannot publish | covered | [refresh.rs::deadline_close_and_retired_generation_cannot_publish][refresh] |
| Catalog input component | Restricted STAC input, URL/host/security and parser contract | deviation | [D11](../DEVIATIONS.md#d11--stac-catalog-input-contract-and-request-targets) |
| Catalog input component | Broader skip/abort/coercion graph assertions | gap | [#10](https://github.com/dynamical-org/api/issues/10) |
| GET list/detail | Encoding over 1 MiB fails with bounded generic 500; unused metadata stays borrowed | deviation | [D17](../DEVIATIONS.md#d17--bounded-catalog-response-encoding) |

## Forecast batches: `POST /v1/forecasts`

| Product/capability | Error, middleware or response | Status | Evidence |
| --- | --- | --- | --- |
| Deterministic rectilinear | Nearest point, exact integer values above 2^53, units, static values, float edge values/null, canonical result parity and mutable cache | covered | [forecasts.rs::forecasts_and_canonical_links_own_values_units_headers_and_usage][forecasts] |
| Latest, explicit and ranged initialization requests; lead/valid-time windows; ensembles | Successful fixture cases and canonical result equality; explicit initialization, two ranged runs and lead limit assertions | covered | [forecasts.rs::windows_runs_ensembles_and_representations_follow_fixture_recipes][forecasts]; this does not assert every selected time/value |
| Forecast temporal boundaries | Unsorted/NaT runs, exact/latest last-duplicate values, strict run bounds, inclusive lead cap and half-open negative/fractional/microsecond masks; exact labels/values over POST and canonical GET | covered | [temporal_breadth.rs::latest_exact_and_strict_run_bounds_choose_last_duplicate_source_values][temporal-breadth]; [temporal_breadth.rs::forecast_half_open_fractional_and_max_lead_masks_own_times_and_values][temporal-breadth] |
| Analysis temporal boundaries | Unsorted/duplicate source order, NaT exclusion, start-inclusive/end-exclusive microsecond bounds and offset normalization; canonical default selects last source time, not maximum | covered | [temporal_breadth.rs::analysis_masks_preserve_unsorted_duplicates_exclude_nat_and_end][temporal-breadth] |
| Request schema | Shared batch/variable/location/label bounds and aliases/defaults | covered | [openapi.rs::shared_request_bounds_and_literals_are_enforced_at_the_schema_boundaries][openapi]; [openapi.rs::forecast_property_aliases_defaults_and_required_fields_agree_with_validation][openapi] (validation component) |
| Body decoding on both POST routes | JSON media/suffix/case/parameter table crossed with UTF-8/16/32, endian and BOM variants; non-JSON/missing media, malformed bytes, character offsets and empty/null envelopes | covered | [request_breadth.rs::media_and_utf_encoding_cross_product_has_exact_http_validation_envelopes][request-breadth]; [request_breadth.rs::malformed_encoding_json_positions_and_empty_bodies_have_owned_http_errors][request-breadth]; surrogate differences remain D2 |
| Schema coercion and required fields | Explicit normalized numeric values; full errors for missing/null/type distinctions, aliases/defaults and field-error suppression; both HTTP models validate mixed batches before compute/catalog | covered | [request_breadth.rs::numeric_coercion_tables_assert_normalized_values_and_full_error_fields][request-breadth]; [request_breadth.rs::missing_null_alias_defaults_and_field_error_suppression_are_distinct][request-breadth]; [request_breadth.rs::required_null_and_alias_error_tables_precede_compute_for_both_body_models][request-breadth] |
| Datetime and digit grammar | Numeric/string epochs, offsets, wide civil years, truncated microseconds and invalid calendar/time syntax; raw JSON versus numeric-string 4,299/4,300/4,301-digit boundaries, signs/zeros/underscores/exponents | covered | [request_breadth.rs::datetime_grammar_owns_normalized_precision_offsets_and_epoch_units][request-breadth]; [request_breadth.rs::digit_limits_distinguish_raw_json_strings_signs_zeros_and_cleaned_retries][request-breadth]; integer coercion/exponent cases also in the numeric table above |
| Multi-error ordering | Complete nested/union/extra and multi-query error objects in schema order, with extras in source order and model validation suppressed on field failure | covered | [request_breadth.rs::multi_error_order_is_schema_then_extras_then_batch_with_union_branches][request-breadth]; field suppression also in the missing/null table above |
| Deep JSON, surrogates and typed formatting extremes | Deliberately bounded parsing and typed output domains | deviation | [D1](../DEVIATIONS.md#d1--fixed-json-recursion-guard), [D2](../DEVIATIONS.md#d2--reject-lone-unicode-surrogates-during-parsing), [D3](../DEVIATIONS.md#d3--preserve-nonzero-sub-minute-datetime-offsets), [D4](../DEVIATIONS.md#d4--wider-typed-integer-output-domain) |
| Product/variable selection | Unknown product 404; analysis on forecast endpoint 422; blank variable list rejected before repository open | covered | [forecasts.rs::product_variable_snapshot_and_canonical_validation_errors_are_public_only][forecasts]; [forecasts.rs::blank_variables_are_rejected_before_any_store_open][forecasts] |
| Forecast POST and canonical GET | Unsupported spatial grid: exact 422 reason before dataset open; canonical GET also rejects unsupported time dimensions with 422; forecast POST rejects analysis type first and hides unknown-time products as 404 | covered | [public_contract.rs::unsupported_product_reasons_and_type_precedence_precede_dataset_open][public-contract] |
| Catalog failure | 503 without Retry-After; query/product context retained in failure analytics | covered | [forecasts.rs::catalog_not_loaded_is_503_and_failure_usage_retains_handler_context][forecasts] |
| Response/read-value budgets | 422; variable selection error before aggregate read budget; response budget before later product lookup | covered | [forecasts.rs::both_budgets_and_selection_errors_precede_loading][forecasts]; [forecasts.rs::batch_response_budget_precedes_read_budget_and_later_product_lookup][forecasts] |
| Admission order | Rate debit before validation; validation before compute; busy 503/Retry-After | covered | [server.rs::validation_precedes_compute_but_follows_rate_debit][server] |
| Request bytes | 16 MiB body cap and 413 envelope | deviation | [D6](../DEVIATIONS.md#d6--bounded-http-request-bodies); **cutover-pending** limit decision: [#31](https://github.com/dynamical-org/api/issues/31) |
| Metadata/response bytes | Metadata/cache/materialization byte bounds | deviation | [D13](../DEVIATIONS.md#d13--bounded-opened-dataset-metadata), [D14](../DEVIATIONS.md#d14--byte-bounded-dataset-retention-and-response-materialization) |
| Large integer-epoch coordinates | Dataset: bounded selector staging with exact eager values, NaT/gaps/duplicates, half-open analysis windows and last positional time; exact/one-over byte admission and isolated allocation peaks | covered | [bounds.rs::coordinate_staging_peak_obeys_cap_before_selectors_or_groups][dataset-bounds]; [epoch_coordinates.rs::batched_epochs_keep_nat_gaps_duplicates_source_order_and_analysis_bounds][epoch-coordinates]; [epoch_coordinates.rs::invalid_epoch_in_a_later_batch_cannot_return_a_partial_dataset][epoch-coordinates]; [epoch.rs::epoch_plan_requires_the_exact_encoding_and_checked_charge][epoch]; [epoch.rs::epoch_batches_preserve_order_and_exact_boundaries][epoch]; [epoch.rs::epoch_deadline_spans_all_batches_and_drops_inflight_work][epoch]; [epoch.rs::cancelling_epoch_axis_drops_inflight_selectors][epoch]; [epoch.rs::malformed_decoded_epoch_batches_fail_before_appending][epoch]; [epoch_coordinates.rs::epoch_batches_cross_unaligned_physical_chunks_without_changing_values][epoch-coordinates]; [cf_time.rs::epoch_conversion_is_independent_of_selection_context][cf-time] |
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
| HEAD, GET, PUT, PATCH, DELETE; plain OPTIONS; slash forms | 405 method/Allow/body/type/length/CORS, plain OPTIONS 204 and slash redirects for GET/HEAD/OPTIONS/POST/PUT/PATCH/DELETE without catalog or compute | covered | [public_contract.rs::data_route_methods_plain_options_and_slash_redirects_need_no_catalog_or_compute][public-contract]; public edge reachability remains [#33](https://github.com/dynamical-org/api/issues/33) |

### Per-caller fair-share admission

| Scope | Capability / failure | Status | Evidence |
| --- | --- | --- | --- |
| Keyed admission | All occupancy combinations at C=6, atomic fair decision before capacity, bounded caller ownership and release, off/unidentified callers | covered | [fair_share.rs::every_boundary_and_unidentified_caller][fair-policy]; [fair_share.rs::admission_is_atomic_and_drop_restores_counts][fair-policy]; [fair_share.rs::off_and_unidentified_still_count_capacity][fair-policy]; [fair_share.rs::keyed_permit_survives_caller_cancellation_and_unwind_without_leaking_keys][fair-policy]; [lib.rs::released_caller_keys_are_removed_and_active_map_is_bounded][fair-map] (policy components) |
| All four data routes | HTTP boundary table; exact fair 429 detail and independent Retry-After, CORS, ordinary failure records; preflight/auth/method/schema/rate before fair, fair before capacity/catalog/location policy | covered | [fair_share.rs::boundary_table_over_http][fair-http]; [fair_share.rs::all_four_routes_return_fair_share_before_catalog_and_geo_and_emit_ordinary_failures][fair-http]; [fair_share.rs::preflight_auth_routing_schema_and_rate_precede_fair_share][fair-http]; [fair_share.rs::pre_routing_auth_and_rate_labels_stay_distinct_from_routed_fair_refusals][fair-http]; [fair_share.rs::fair_share_precedes_location_denial_and_shared_heartbeat_reports_decisions][fair-http] |
| Caller key / exemptions | Same trusted IP/worker key as rate limiting; missing/empty authenticated IP excluded; unauthenticated socket identity; no-client fallback | covered | [fair_share.rs::identity_is_the_rate_key_with_explicit_identification][fair-policy] (policy); [fair_share.rs::worker_identity_and_unidentified_fallbacks_share_the_rate_key][fair-http]; [fair_share.rs::unidentified_http_holders_stay_exempt_and_do_not_enter_caller_occupancy][fair-http] (real HTTP holders in each exempt case) |
| Cross-route / cancellation | A held worker on each route counts against a request on the forecast route after disconnect, until physical work finishes; pre-admission cancellation takes no slot | covered | [fair_share.rs::every_route_holds_its_caller_after_disconnect_until_physical_work_finishes][fair-http]; [fair_share.rs::cancellation_before_admission_takes_no_caller_slot][fair-http] |
| Cancellation during catalog/location await | Async cancellation releases admission before dataset-worker start; detached location work keeps its independent bounds | deviation | [D42](../DEVIATIONS.md#d42--cancellation-before-the-dataset-worker-starts); [fair_share.rs::cancellation_during_async_location_check_releases_admission_before_worker_start][fair-http] |
| Configuration / heartbeat | Small-C defaults inert, explicit activation, invalid B/K/retry rejection, off switch; current maximum identified occupancy and cumulative fair/conservative/capacity decisions | covered | [fair_share.rs::defaults_validation_and_small_capacity][fair-policy]; [fair_share.rs::environment_thresholds_are_validated_even_when_off_and_empty_thresholds_are_unset][fair-http]; [fair_share.rs::off_switch_and_inert_small_capacity_restore_capacity_only_http][fair-http]; [fair_share.rs::telemetry_is_current_occupancy_and_cumulative_decisions_with_conservative_precedence][fair-policy]; [fair_share.rs::fair_share_precedes_location_denial_and_shared_heartbeat_reports_decisions][fair-http] |

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
| Snapshot identifiers | Malformed/unknown immutable ID and canonical branch-name rejection over HTTP; typed missing-branch component error | covered | [historical_snapshots.rs::malformed_unknown_and_branch_snapshot_ids_have_exact_public_404_errors][historical]; [fixtures.rs::unknown_snapshots_have_typed_errors_and_missing_branches_remain_reader_errors][dataset-fixtures] |
| Canonical admission | Exact busy 503/Retry-After 5 and data-limiter 429 envelope; simple CORS/request ID; rate debit before compute with distinct query/data buckets and no catalog/store read | covered | [public_contract.rs::canonical_admission_returns_exact_busy_and_data_rate_limit_responses][public-contract] |
| Historical deterministic forecast snapshots | POST A link retains exact original body/ETag, values, schema, links and immutable caching after main advances to B; TTL, separate pinned keys, LRU eviction/reopen and unchanged-main revalidation | covered | [historical_snapshots.rs::historical_canonical_links_survive_main_advance_and_cache_eviction][historical] |
| Historical schema and times | A rejects B's added variable/run/lead window; B exposes changed values, new variable metadata and appended run/lead times over POST and canonical GET | covered | [historical_snapshots.rs::historical_schema_and_appended_times_are_snapshot_specific][historical] |
| Historical analysis, ensemble and projected snapshots | Main-advance HTTP history across these additional families | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10); the deterministic forecast history above does not establish these families |
| Canonical variable normalization | Trim/sort/deduplicate, blank rejection and 25/26 unique-name boundary; POST raw-list bound precedes dedup; deterministic canonical variable order | covered | [canonical_breadth.rs::canonical_variables_sort_trim_deduplicate_and_bound_after_normalization][canonical-breadth] |
| Analysis window validation | Canonical one-sided/default/equal/reversed/naive/long-range table, exact time/value masks; POST timezone/order/31-day precedence with exact envelopes and boundary successes | covered | [canonical_breadth.rs::canonical_analysis_window_rules_own_one_sided_equal_timezone_and_long_ranges][canonical-breadth]; [canonical_breadth.rs::analysis_post_window_validation_has_exact_precedence_and_31_day_boundary][canonical-breadth] |
| Encoded canonical identifiers | Product/snapshot/index escapes decode once; reserved/Unicode unknown identifiers, encoded slash and branch errors; schema rejection before product lookup | covered | [canonical_breadth.rs::encoded_identifiers_are_decoded_once_and_schema_errors_still_precede_lookup][canonical-breadth] |
| Reader failures on both POST and both canonical data routes | Timeout/throttle 503/Retry-After versus nonvirtual/permanent/decode 500, sanitized bodies, permit release and successful recovery after each injected failure | covered | [canonical_breadth.rs::transient_reader_failure_table_is_identical_on_both_post_and_canonical_routes][canonical-breadth] |

| Historical analysis, ensemble and projected snapshots | Real A/B main commits: half-open analysis windows/latest times, unchanged and changed member identities/cardinality, attested Lambert stored indices/locations; added variable/time values and exact immutable A body/ETag after eviction/reopen | covered | [historical_families.rs][historical-families]; [owned recipes][historical-family-recipes]; this does not establish all historical products or CRS families |

| Historical analysis, ensemble and projected snapshots | Real A/B main commits: half-open analysis windows/latest times, unchanged and changed member identities/cardinality, asymmetric Lambert indices and snapshot-specific locations; added variable/time values and exact immutable A body/ETag after eviction/reopen | covered | [historical_families.rs][historical-families]; [owned recipes][historical-family-recipes]; this does not establish all historical products or CRS families |
| Canonical error breadth | Variable sorting/dedup/25-variable bound, analysis window validation, encoded identifiers and transient failure HTTP tables | gap | [#4](https://github.com/dynamical-org/api/issues/4), [#10](https://github.com/dynamical-org/api/issues/10) |
| Projected forecast | HRRR/HRDPS synthetic values/indices/location, independent SHA-256 ETag, repeat stability, immutable cache and out-of-grid 422 | covered | [projected.rs::projected_forecasts_and_canonical_get_share_cells_values_locations_and_etags][server-projected] |
| HEAD, POST, PUT, PATCH, DELETE; plain OPTIONS; slash forms | 405 method/Allow/body/type/length/CORS, plain OPTIONS 204 and slash redirects for GET/HEAD/OPTIONS/POST/PUT/PATCH/DELETE without catalog or compute | covered | [public_contract.rs::data_route_methods_plain_options_and_slash_redirects_need_no_catalog_or_compute][public-contract]; public edge reachability remains [#33](https://github.com/dynamical-org/api/issues/33) |

## Analyses and run listing

| Route | Method × capability or error | Status | Evidence |
| --- | --- | --- | --- |
| `/v1/analyses` | Typed schema: aliases, required aware ordered window, exact 31-day/microsecond bounds, extras, variable and Unicode query-ID bounds | covered | [analyses.rs::analysis_model_checks_timezone_order_and_elapsed_range_in_order][analysis-contract]; [analyses.rs::analysis_variable_and_query_id_bounds_use_items_and_unicode_codepoints][analysis-contract] |
| `/v1/analyses` | Deterministic values, half-open/source-order times, units, static cardinality, canonical replay, independent body-hash ETags and cache repeats | covered | [analyses.rs::analysis_values_half_open_windows_canonical_headers_and_cache_follow_recipe][analyses]; [query.rs::analysis_windows_preserve_unsorted_duplicate_source_times_and_exclude_end][query] |
| `/v1/analyses` | Ensemble member IDs and dimension-order integer values; both attested projected operations and canonical cells | covered | [analyses.rs::analysis_ensembles_preserve_member_identity_dimension_order_and_integer_values][analyses]; [projected.rs::projected_analyses_share_point_selection_and_canonical_values][server-projected] |
| `/v1/analyses` | Atomic selection before loading, response/read budgets and earlier response budget before later unknown product; blank variables before open; empty windows | covered | [analyses.rs::analysis_validation_and_selection_are_atomic_before_weather_loading][analyses]; [analyses.rs::analysis_empty_variables_precede_open_and_empty_window_has_public_error][analyses] |
| `/v1/analyses` | Both IMERG IDs with synthetic half-hour epoch/global coordinate shapes succeed under the default metadata budget; exact times/fill values and canonical body-hash ETags | covered | [analyses.rs::imerg_shaped_analyses_fit_the_default_metadata_budget_and_replay_canonical][analyses]; generic epoch component assertions below |
| Both data families | Only typed metadata-budget failure maps to safe 422; malformed metadata and unrelated reader limits stay 500 | deviation | [D40](../DEVIATIONS.md#d40--typed-dataset-metadata-budget-rejection); [data.rs::only_the_typed_metadata_budget_open_failure_becomes_422][data-service]; [analyses.rs::metadata_limit_has_safe_http_422_on_analysis_forecast_runs_and_canonical][analyses] |
| `/v1/forecasts/runs` | Typed query defaults, last-wins/ignored extras, bounded integer coercion; unique full-ns ordering, NaT removal and strict filtering | covered | [analyses.rs::runs_accept_last_query_value_extras_and_decimal_integer_coercion][analysis-contract]; [query.rs::run_listing_sorts_unique_full_precision_times_drops_nat_and_filters_strictly][query] |
| `/v1/forecasts/runs` | Metadata-only values/snapshot/limits/empty windows, mutable caching, no ETag or data-query headers; unsupported point grid remains discoverable | covered | [analyses.rs::runs_are_strict_newest_first_metadata_only_and_have_no_query_response_headers][analyses]; [analyses.rs::run_discovery_bypasses_point_grid_eligibility][analyses] |
| `/v1/forecasts/runs` history | Main A to B expiry and a further TTL expiry retaining B; newest-first appended runs, strict filter/limit, mutable headers and both metadata/point cache refresh orders | covered | [historical_families.rs::run_discovery_refreshes_main_in_either_cache_profile_order][historical-families] |
| `/v1/analyses` | All three virtual analysis IDs retain generic 500 before store I/O | covered | [analyses.rs::virtual_analysis_ids_fail_before_repository_io_with_existing_generic_500][analyses] |
| `/v1/forecasts/runs` | Virtual forecast stored coordinates use a separate cache profile with zero virtual authorization; virtual coordinates fail without payload fetch | covered | [analyses.rs::virtual_forecast_runs_use_only_stored_coordinates_without_virtual_authorization][analyses]; [data.rs::metadata_open_recipe_removes_http_and_s3_virtual_authorization][data-service]; [data.rs::metadata_and_full_open_recipe_keys_keep_cache_handles_separate][data-service] |
| Both routes | Schema before compute; lookup/type/time before open, catalog before deferred handler checks; generic runs open error has no Retry-After | covered | [analyses.rs::new_route_schema_checks_precede_compute_and_both_routes_release_permits][analyses]; [analyses.rs::runs_lookup_type_and_time_guards_precede_open_errors][analyses]; [analyses.rs::both_new_routes_preserve_catalog_failure_before_handler_checks][analyses] |
| Both routes | Admission before caller-location checks; refused/unconfigured checks before store work, allowed data/run headers, hidden gated products before type rejection | covered | [geo_gate.rs::runs_and_analyses_admit_before_checks_and_refuse_before_store_work][server-geo]; [geo_gate.rs::runs_and_analyses_fail_closed_without_geo_configuration_before_store_work][server-geo]; [geo_gate.rs::runs_and_analyses_check_hidden_gated_products_before_type_rejection][server-geo] |
| Both routes | Analysis body cap before catalog/compute; cancellation retains permit through owned store work | covered | [analyses.rs::analysis_body_cap_precedes_catalog_and_compute][analyses]; [analyses.rs::cancelled_analysis_and_runs_keep_their_permit_until_owned_store_work_finishes][analyses] |
| Both routes | HEAD/unsupported methods, plain OPTIONS and slash redirect response tables without catalog/compute use | covered | [public_contract.rs::data_route_methods_plain_options_and_slash_redirects_need_no_catalog_or_compute][public-contract]; public edge reachability remains [#33](https://github.com/dynamical-org/api/issues/33) |

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
| CF float64 durations | Dataset: fractional precision and NaT from explicit recipe | covered | [fixtures.rs::coordinate_dtypes_fractional_durations_and_float_bits_follow_the_recipe][dataset-fixtures] |
| CF float64 duration breadth | Full-decoding-unit and auxiliary-duration precision/selection cases | gap | [#10](https://github.com/dynamical-org/api/issues/10) |
| Plain little-endian bytes; width-one omitted/null endian | Dataset/reader: admitted byte forms; big-endian and relaxed multibyte forms rejected | covered | [integer_coordinates.rs::one_byte_endian_forms_keep_multibyte_and_original_api_admission_strict][integer-coords] |
| Plain Blosc/Zstd; indexed sharded Blosc/Zstd with bytes/CRC32C index | Dataset: physical float32, float64, int64 fixture reads follow numeric recipes | covered | [fixtures.rs::forecast_coordinates_metadata_and_cartesian_values_follow_the_recipe][dataset-fixtures] |
| Blosc header typesize 1 under native-width metadata | Reader: float32/int64 exact bytes in full/partial and plain/sharded reads for byte shuffle, bitshuffle and no shuffle; other typesize mismatches and nonnative metadata rejected | covered | [blosc_typesize.rs::byte_typesize_float32_and_int64_decode_header_shuffle_for_partial_and_full_reads][blosc-typesize]; [blosc_typesize.rs::header_typesize_one_bitshuffle_preserves_partial_and_full_numeric_bytes][blosc-typesize]; [blosc_typesize.rs::header_typesize_one_noshuffle_preserves_partial_and_full_numeric_bytes][blosc-typesize]; [blosc_typesize.rs::other_mismatches_and_byte_header_with_nonnative_metadata_stay_rejected][blosc-typesize]; narrow admission is documented in [D33](../DEVIATIONS.md#d33--explicit-catalog-codec-subset-and-lazy-data-variables) |
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
| Ensemble presentation | Query over physical dataset: complete two-time analysis response with exact member IDs and values | covered | [fixtures.rs::complete_ensemble_query_pairs_members_with_recipe_values][dataset-fixtures]; [analyses.rs::analysis_ensembles_preserve_member_identity_dimension_order_and_integer_values][analyses] |
| Ensemble HTTP breadth | Exact numeric member identity/order, source-position values, mixed static/timed variables, canonical GET and ETags over physical bytes, Blosc/Zstd and indexed shards | covered | [ensemble_http.rs::forecast_members_and_static_values_follow_physical_codec_recipes_over_http][ensemble-http]; [ensemble_http.rs::stored_ensemble_zstd_inputs_have_exact_forecast_values_over_http][ensemble-http]; [input recipes](../crates/dynapi-server/tests/support/ensemble_http.md). Includes padded int16/typesize-4 and standalone Zstd member coordinates, signed/unsigned widths, duplicate IDs, byte-sized frame headers and padded shard edges; not every codec-option combination. |
| Ensemble HTTP selection/profile limits | No-member-only queries use ordinary data; no member-filter request field; physical string member codecs remain unsupported | covered | [ensemble_http.rs::member_independent_queries_use_data_without_inventing_a_member_axis][ensemble-http]; [ensemble_http.rs::member_filter_is_not_a_supported_forecast_request_field][ensemble-http]; [ensemble_http.rs::physical_string_members_remain_an_explicit_unsupported_encoding][ensemble-http]. No physical string-member support is claimed. |
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
| `noaa-gfs-forecast-virtual`; `noaa-gefs-forecast-10-day-0-25-degree-virtual`, `noaa-gefs-forecast-16-day-0-5-degree-virtual`, `noaa-gefs-forecast-35-day-0-5-degree-virtual` | Stored-coordinate run listing matched; point serving retains generic 500 before repository I/O | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-hrrr-forecast-18-hour-virtual`, `noaa-hrrr-forecast-48-hour-virtual`; `noaa-rrfs-forecast-sub-hourly-virtual`, `noaa-rrfs-forecast-18-hour-virtual`, `noaa-rrfs-forecast-84-hour-virtual`, `noaa-rrfs-ens-forecast-virtual` | Stored-coordinate run listing matched; point serving retains generic 500 before repository I/O | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-refs-forecast-hourly-virtual`, `noaa-refs-forecast-3-hourly-virtual`; `ecmwf-aifs-single-forecast-virtual` | Stored-coordinate run listing matched (13 products across these three rows); point serving retains generic 500 before repository I/O | gap | [#3](https://github.com/dynamical-org/api/issues/3) |
| `noaa-gfs-analysis` | Sampled recent and historical analysis windows/variables, canonical links and repeat body/ETags matched; forecast run discovery also matched | gap | [#7](https://github.com/dynamical-org/api/issues/7): exhaustive variable/snapshot combinations; fixture-layer serving assertions above |
| `noaa-gefs-analysis` | Recent and historical analysis/canonical windows match with the merged Blosc header-typesize admission (D33), including observed older frames through June 2025 | gap | [#7](https://github.com/dynamical-org/api/issues/7): exhaustive variable/snapshot combinations and historical frame qualification |
| `noaa-hrrr-analysis`, `noaa-mrms-conus-analysis-hourly` | Projected/rectilinear recent and historical analysis windows, variables, canonical links and body/ETags matched | gap | [#7](https://github.com/dynamical-org/api/issues/7): broader boundary/runtime/product qualification |
| `ucsb-chc-chirps-analysis-final`, `ucsb-chc-chirps-analysis-preliminary` | Recent and historical daily analysis windows, land-point numeric precipitation, canonical links and body/ETags matched | gap | [#7](https://github.com/dynamical-org/api/issues/7): broader variable/snapshot/product qualification |
| `noaa-gfs-analysis-virtual`, `noaa-gefs-analysis-0-25-degree-virtual`, `noaa-hrrr-analysis-virtual` | Generic 500 `text/plain` `Internal Server Error` before repository I/O: virtual GRIB eligibility remains excluded | gap | [#3](https://github.com/dynamical-org/api/issues/3); no analysis-ID bypass |
| `nasa-imerg-analysis-early`, `nasa-imerg-analysis-late` | Readable under the unchanged default 64 MiB metadata budget using bounded integer-epoch staging; sampled recent and historical half-hour windows, precipitation/quality values, canonical links and repeat body/ETags matched | gap | [#7](https://github.com/dynamical-org/api/issues/7): broader variable/snapshot/product qualification; [analyses.rs::imerg_shaped_analyses_fit_the_default_metadata_budget_and_replay_canonical][analyses]; [dataset accounting](dataset.md) |

## Shared data-route middleware and transport

| Route/method scope | Capability × error / precedence | Status | Evidence |
| --- | --- | --- | --- |
| OPTIONS preflight, including unknown paths | Origin + Access-Control-Request-Method bypass origin auth and telemetry; encoded forecast paths also bypass an exhausted rate bucket; plain OPTIONS does not | covered | [server.rs::preflight_bypasses_auth_rate_and_telemetry][server]; [server.rs::middleware_url_path_preserves_preflight_and_origin_auth_precedence][server] |
| CORS policy component | Allowed preflight vs disallowed method/headers; simple response vs unhandled 500 exposed headers | covered | [policy.rs::cors_preflight_is_distinct_from_simple_and_500][policy] |
| OPTIONS preflight headers | Exact HTTP allow-methods, max-age, allowed request headers and unique UUID4 request IDs on accepted/rejected preflights; no exposed-header field | covered | [public_contract.rs::preflight_headers_and_request_ids_cover_allowed_and_rejected_requests][public-contract] |
| Origin rejection, rate rejection and unmatched paths | Exact public 403 Forbidden, 429 rate-limit and unmatched-route 404 JSON bytes, length/type, simple CORS/request ID and Retry-After | covered | [public_contract.rs::forbidden_rate_limited_and_unmatched_bodies_are_exact_json][public-contract] |
| Query paths and canonical prefix | Decoded middleware path determines query/data limiter without changing routing; encoded suffixes tested | covered | [server.rs::middleware_url_path_controls_rate_without_changing_routing][server] |
| Forecast POST / encoded path variants | Preflight → auth → rate precedence, including exhausted buckets | covered | [server.rs::middleware_url_path_preserves_preflight_and_origin_auth_precedence][server] |
| Forecast POST | Authenticated worker suffix separates rate buckets; first duplicate auth header wins | covered | [server.rs::origin_auth_controls_cf_rate_keys_and_first_duplicate_header_wins][server] |
| Rate-key policy component | Trust Cloudflare address/worker only after origin auth; normalize worker | covered | [policy.rs::trust_cf_metadata_only_after_auth_and_normalize_the_entire_worker][policy] |
| HTTP rate keys | Distinct authenticated Cloudflare addresses get distinct query buckets despite the same socket peer; unauthenticated address/worker changes share the peer bucket even with a spoofed auth header | covered | [public_contract.rs::http_rate_keys_use_cf_addresses_only_with_origin_authentication][public-contract] |
| Rate-limiter component | Token refill/retry rounding/clock regression and bounded client LRU | covered | [policy.rs::token_refill_retry_and_clock_regression_match_python][policy]; [policy.rs::rejected_clients_refresh_lru_and_eviction_is_bounded][policy] |
| Redirecting paths | Configured scheme + validated Host; ignored forwarded headers; accepted-socket fallback | covered | [server.rs::public_redirect_scheme_uses_host_and_ignores_forwarded_headers][server]; [server.rs::slash_redirect_falls_back_to_accepted_socket_authority][server] |
| Application responses | Optional release marker / required origin auth / startup validation | deviation | [D27](../DEVIATIONS.md#d27--optional-deployment-identity-authentication-and-redirect-scheme), [D9](../DEVIATIONS.md#d9--startup-configuration-validation) |
| Public route/method surface and trusted headers | Origin behavior agrees with edge route/method admission, rate policy and origin-auth/viewer-location header transforms | gap | [#33](https://github.com/dynamical-org/api/issues/33); public reachability and header-policy alignment need qualification independently of origin response tests |
| Container-aware dataset/repository cache identity | Container type/prefix changes miss both caches, declaration reordering keeps identity, and key storage counts toward cache bytes | covered | [D36](../DEVIATIONS.md#d36--container-identity-in-repository-and-dataset-cache-keys); [cache.rs::container_changes_miss_dataset_and_repository_caches][cache]; [cache.rs::container_order_is_not_repository_identity][cache]; [cache.rs::container_key_storage_is_charged_before_cache_retention][cache] |
| Gated-product selection and checked recipe | Exact proxy host on any HTTP container selects the gate; failed catalog lookup does not suppress the next check; refresh during a check opens that checked generation and a changed recipe misses cache next time | covered | [geo_gate_tests.rs::metadata_selects_any_http_container_on_exact_proxy_host][geo-gate-component] (component); [geo_gate.rs::failed_catalog_lookup_does_not_skip_the_next_check][geo-gate]; [geo_gate.rs::catalog_refresh_during_check_opens_exactly_the_checked_recipe][geo-gate] |
| Data products using `wn.dynamical.org` | Origin and viewer authentication required to forward location; denied mixed batch opens no dataset; allowed POST/canonical responses retain ETags with private no-store | covered | [geo_gate.rs::location_needs_both_current_auth_headers_and_nonempty_fields][geo-gate]; [geo_gate.rs::a_refused_mixed_batch_never_opens_any_dataset_and_releases_admission][geo-gate]; [geo_gate.rs::allowed_post_and_canonical_keep_data_and_etags_but_never_shared_cache][geo-gate] |
| Caller-location failures and cache | Missing secret and unexpected status/redirect: 503/Retry-After 5; oversized answers: uncached 503; 204/451 decisions use country/region keys, with 451 expiry asserted at 300 seconds | covered | [geo_gate.rs::absent_secret_fails_closed_without_asking_but_direct_and_unknown_are_unchanged][geo-gate]; [geo_gate.rs::unsuccessful_checks_redirects_and_oversized_details_are_uncached][geo-gate]; [geo_gate.rs::allow_and_deny_cache_keys_include_country_region_and_expire_at_five_minutes][geo-gate]; timeout/transport assertions are component-only: [geo_gate_tests.rs::transport_errors_and_hard_timeout_are_retryable_and_uncached][geo-gate-component] |
| Caller-location placement, resource bounds and privacy | Busy-before-catalog/lookup order; analyses/runs admit before caller-location checks and refuse before store work; bounded/coalesced checks and private responses after identification | deviation | [D37](../DEVIATIONS.md#d37--viewer-trust-and-gate-placement), [D38](../DEVIATIONS.md#d38--bounded-and-coalesced-location-checks), [D39](../DEVIATIONS.md#d39--response-privacy-and-secret-handling); [geo_gate.rs::forecast_and_canonical_busy_precede_catalog_unknown_products_and_geo_checks][geo-gate]; [geo_gate.rs::runs_and_analyses_admit_before_checks_and_refuse_before_store_work][geo-gate]; [geo_gate.rs::responses_after_identification_are_private_including_validation_errors][geo-gate]; [geo_gate_tests.rs::identical_checks_coalesce_and_cancelled_callers_leave_bounded_deadline_tasks][geo-gate-component]; [geo_gate_tests.rs::cached_entries_and_owned_bytes_have_separate_hard_bounds][geo-gate-component] |
| Native HTTP/1 | Hyper parser behavior, bounded connections/header/body/idle deadlines | deviation | [D10](../DEVIATIONS.md#d10--hyper-http1-wire-parsing-and-bounded-native-connections) |
| HTTPS repository / virtual payload reader | No redirects, exact prefix/range/length checks, response caps; trusted repository metadata expansion remains | deviation | [D24](../DEVIATIONS.md#d24--bounded-readonly-repository-http-transport); payload/component tests do not qualify an arbitrary remote repository |
| HTTP virtual payload reader | Declared-prefix success, duplicate values and exact requested byte range | covered | [http.rs::allowed_virtual_chunk_http_repository][reader-http] |
| HTTP virtual payload reader | Undeclared prefix, redirect, mismatched range and chunked overflow rejection | covered | [http.rs::undeclared_virtual_chunk_is_not_fetched][reader-http]; [http.rs::redirect_cross_origin_is_not_followed][reader-http]; [http.rs::inconsistent_content_range_is_rejected][reader-http]; [http.rs::oversized_chunked_body_is_stopped][reader-http] |
| Large WeatherNext ensemble | Full global-plane member/lead selection exceeds sequential reader deadline | deviation | [D25](../DEVIATIONS.md#d25--full-ensemble-global-plane-reads-exceed-the-reader-deadline) |
| Large WeatherNext qualification | Complete full member/lead requests within a justified deadline with measured bounded memory | gap | [#9](https://github.com/dynamical-org/api/issues/9), [#15](https://github.com/dynamical-org/api/issues/15) |

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
| Geo-gate privacy in telemetry | Refusal containing the configured secret fails closed without copying it into response or captured telemetry | covered | [geo_gate.rs::denial_text_trims_python_whitespace_and_empty_text_has_fallback][geo-gate]; this specific assertion does not establish the full denial/error event field and counter table |
| Routing and geo-gate telemetry breadth | Shadow/auto counters/provenance and complete viewer-policy denial/error event assertions | gap | [#3](https://github.com/dynamical-org/api/issues/3), [#10](https://github.com/dynamical-org/api/issues/10); implemented gate HTTP behavior does not establish complete telemetry conditions |

### Weighted request analytics

| Scope | Capability / failure | Status | Evidence |
| --- | --- | --- | --- |
| Sampler policy | T/W strict boundary, all eligible statuses count, non-2xx retained, independent callers | covered | [sampling.rs::threshold_and_strict_probability_boundaries_count_every_eligible_status][sampling-policy] (component); [analytics_sampling.rs::weighted_successes_errors_window_reset_and_worker_identity_follow_http_contract][sampling-http] |
| Fixed window / bounds | Exact rollover and backward reset, 10,000-entry production LRU, saturated counts, atomic concurrent prefix | covered | [sampling.rs::fixed_window_rollover_and_clock_reversal_reset_counts][sampling-policy]; [sampling.rs::lru_and_saturated_counts_bound_both_indexes][sampling-policy]; [sampling.rs::production_limit_retains_only_ten_thousand_caller_ids][sampling-policy]; [sampling.rs::concurrent_calls_keep_an_exact_prefix_and_capped_state][sampling-policy] (components; small injected capacity exercises the production eviction algorithm) |
| Off / anonymous / configuration | Integer unit weights by default and for anonymous requests; positive settings and exact enable switch | covered | [analytics_sampling.rs::analytics_sampling_settings_are_positive_and_enable_only_exact_one][sampling-http]; [analytics_sampling.rs::default_off_and_anonymous_always_emit_integer_unit_weight][sampling-http]; [sampling.rs::anonymous_and_off_never_consult_clock_draw_or_state][sampling-policy] |
| Eligibility / cancellation | Only configured post-auth `/v1/` analytics consume the prefix; rate/handler errors count; static/auth/preflight/cancel do not | covered | [analytics_sampling.rs::only_eligible_analytics_count_and_every_non_success_is_retained][sampling-http]; [analytics_sampling.rs::cancellation_never_consumes_the_analytics_prefix][sampling-http] |
| Failure / terminal independence | Sampler failure sends unit weight; capture failure preserves HTTP response and terminal failure; dropped analytics leaves terminal success intact | covered | [sampling.rs::clock_and_draw_failures_send_unit_weight_and_allow_recovery][sampling-policy]; [analytics_sampling.rs::failing_sampler_emits_each_eligible_event_at_unit_weight][sampling-http]; [analytics_sampling.rs::failing_capture_never_changes_http_responses_or_terminal_failures][sampling-http]; [analytics_sampling.rs::dropping_analytics_preserves_response_and_independent_terminal_success][sampling-http]; [analytics_sampling.rs::disabled_terminal_success_sampling_keeps_weighted_analytics_active][sampling-http] |
| Analytics delivery | Integer weight survives approved-property filtering and loopback capture delivery | covered | [delivery.rs::loopback_wire_contract_and_shutdown_flush_both_destinations][sampling-delivery] (requires `telemetry-delivery`) |

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
exist. Recheck caller-location ordering/privacy deviations and routing/co-tile/open-load
telemetry when adopting a later reference pin. Do not infer coverage from an
issue's closed state, a test name, an OpenAPI schema or a successful small request.

[server]: ../crates/dynapi-server/tests/server.rs
[public-contract]: ../crates/dynapi-server/tests/public_contract.rs
[contract]: ../crates/dynapi-contract/tests/contract.rs
[catalog-docs]: ../crates/dynapi-server/tests/catalog_documentation.rs
[catalog-life]: ../crates/dynapi-server/tests/catalog_lifecycle.rs
[openapi]: ../crates/dynapi-server/src/openapi.rs
[forecasts]: ../crates/dynapi-server/tests/forecasts.rs
[historical]: ../crates/dynapi-server/tests/historical_snapshots.rs
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

[dataset-bounds]: ../crates/dynapi-dataset/tests/bounds.rs
[epoch-coordinates]: ../crates/dynapi-dataset/tests/epoch_coordinates.rs
[epoch]: ../crates/dynapi-dataset/src/epoch.rs
[analyses]: ../crates/dynapi-server/tests/analyses.rs
[analysis-contract]: ../crates/dynapi-contract/tests/analyses.rs
[data-service]: ../crates/dynapi-server/src/data.rs
[server-geo]: ../crates/dynapi-server/tests/geo_gate.rs
[geo-gate]: ../crates/dynapi-server/tests/geo_gate.rs
[geo-gate-component]: ../crates/dynapi-server/src/geo_gate_tests.rs
[blosc-typesize]: ../crates/dynapi-reader/tests/blosc_typesize.rs

[fair-policy]: ../crates/dynapi-policy/tests/fair_share.rs
[fair-http]: ../crates/dynapi-server/tests/fair_share.rs

[fair-map]: ../crates/dynapi-policy/src/lib.rs

[sampling-policy]: ../crates/dynapi-server/src/telemetry/sampling.rs
[sampling-http]: ../crates/dynapi-server/tests/analytics_sampling.rs
[sampling-delivery]: ../crates/dynapi-server/src/telemetry/delivery.rs

[ensemble-http]: ../crates/dynapi-server/tests/ensemble_http.rs

[temporal-breadth]: ../crates/dynapi-server/tests/temporal_breadth.rs
[request-breadth]: ../crates/dynapi-server/tests/request_breadth.rs
[canonical-breadth]: ../crates/dynapi-server/tests/canonical_breadth.rs

[historical-families]: ../crates/dynapi-server/tests/historical_families.rs
[historical-family-recipes]: ../crates/dynapi-server/tests/support/history_families.md
