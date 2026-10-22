# Dataset input fixtures

These 21 local Icechunk repositories contain synthetic values. They were written
with Icechunk 2.2.2 and Zarr 3.3.0. They exercise physical Zarr/codec compatibility;
they are inputs, with no saved decoded views, classifications or service responses.
Tests run entirely in Rust. Maintainers verify reference-service behavior outside
this repository, and record intentional differences in `DEVIATIONS.md`.

`manifest.json` lists repository names and SHA-256 hashes of input files only.
Each `input.json` supplies an immutable snapshot ID, collection metadata, request
inputs and direct-read selectors. No expected selectors, values, errors, statuses
or response bodies are stored there. The retained repositories include their
synthetic history. Titles, commit descriptions and example query IDs have been
normalized to remove development labels; fixed-width metadata text substitutions
preserve the encoded metadata structure. Numeric payloads and array layouts are
unchanged. Input integrity hashes describe the normalized files.

## Forecast construction

The base `forecast` repository has source dimensions
`(init_time, lead_time, latitude, longitude) = (2, 5, 4, 6)`:

- `init_time`: 2026-07-30 at 00:00 and 06:00 UTC, stored as int64 epoch seconds.
- `lead_time`: float64 seconds `[0, 3600, 7200, 10800, 14400]`.
- `latitude`: float64 `[44, 42, 40, 38]`.
- `longitude`: float64 `[0, 60, 120, 180, 240, 300]`.
- `temperature_2m`: for flat C-order index `i`, float32 arithmetic
  `float32(i) / float32(10) + float32(10)`, with unit `degree_Celsius`.
  At spatial index `(0, 0)` in both runs, replace the five leads with float32
  `[NaN, -0.0, 1e-5, smallest positive subnormal, 3e38]`.
- `count`: int64 `2^53 + 1 + i`, in the same dimension order as temperature.
- `static`: float64 `i` for a `(latitude, longitude) = (4, 6)` array, unit `m`.
- `valid_time`: run plus lead, an auxiliary `(init_time, lead_time)` coordinate.
- `spatial_ref`: scalar int64 zero with both WKT attributes set to the
  `sphere-6371229` input in `live-crs.json`; it is referenced as a coordinate.

The root attributes include `_FillValue: 42`, `units: seconds`, a synthetic title
and nested JSON containing integer `2^53 + 1`, float `1e-20`, and
`[true, null, "°C"]`. Temporal arrays use `units: seconds since 1970-01-01` and
`calendar: proleptic_gregorian`; durations use `units: seconds` and
`dtype: timedelta64[us]`. Floating arrays carry an encoded NaN `_FillValue`.
Auxiliary references appear in root and applicable variable `coordinates` strings.

For data arrays, inner chunks are 1 along init_time, 2 along lead_time/time/member/
latitude/y, and 3 along longitude/x. Temperature uses indexed shards twice the
inner chunk width except on init_time, with little-endian bytes, Blosc zstd level
3/shuffle and a CRC32C shard index. Other nonscalar arrays use bytes plus Blosc
zstd level 3/shuffle. Coordinate chunks span their complete logical arrays.
Scalars use bytes plus Zstd level 0 without checksum. Blosc typesize equals stored
element width except where explicitly stated below.

| Repository | Changes to the forecast construction |
| --- | --- |
| `analysis` | Keep run zero, rename lead_time to time, and assign five hourly UTC times starting 2026-07-30 00:00. Drop run, lead and auxiliary valid_time arrays. |
| `float32` | Cast latitude/longitude to float32 and supply descriptive geographic axis attributes. |
| `int64` | Cast latitude/longitude to int64. |
| `fractional` | Replace leads with durations in ns `[0, 250000000, 1000001000, 3600000000000, NaT]`, stored as float64 seconds; recompute valid_time. |
| `ensemble` | Members int64 `[0, 2, 7]`; duplicate base values across members. Temperature order is `(init_time, lead_time, ensemble_member, latitude, longitude)`; count order is `(ensemble_member, longitude, init_time, latitude, lead_time)`. Static has no member axis. |
| `geographic_xy` | Rename latitude/longitude to y/x with unchanged values. |
| `projected` | Rename to y/x, then set y to `[0, 1000, 2000, 3000]` and x to `[0, 1000, 2000, 3000, 4000, 5000]`. `crs_wkt` is Lambert conformal; `spatial_ref` is geographic, exercising precedence. |
| `missing_crs` | Geographic x/y values with empty spatial_ref attributes. |
| `fallback_crs` | Geographic x/y values, empty crs_wkt and geographic spatial_ref attribute. |
| `unreferenced_crs` | Geographic x/y values, but spatial_ref is a data variable without coordinate references. |

Source-order expectations follow C-order strides and Cartesian list indexing.
For example, forecast count at `(run, lead, lat, lon)` is
`2^53 + 1 + (((run * 5 + lead) * 4 + lat) * 6 + lon)`.
The binary64 fractional-second input 1.000001 lies below its decimal value;
successive s/ms/us/ns scaling followed by truncation yields 1,000,000,999 ns.
The quarter-second input is exact.
List selectors retain duplicates; selectors for absent dimensions do nothing.
Static data use `lat * 6 + lon` irrespective of run/lead selectors.

## Coordinate membership construction

The following repositories start from an independent two-time analysis grid:

- time: int64 epoch seconds `[1785369600, 1785373200]` with the epoch CF attributes.
- y: float64 `[39.5, 41, 43.25]`; x: float64 `[-103, -100]`.
- temperature: float32 C-order `arange(12)` reshaped `(time, y, x) = (2, 3, 2)`,
  with `(0,0,0)=NaN`, `(1,0,0)=+Infinity`, `(0,1,0)=-0.0`, unit `K`.
- count: int64 `2^53 + 3 + arange(12)`, source order `(x, time, y) = (2, 2, 3)`.
- spatial_ref: scalar int64 zero using the `grib-sphere` WKT input, referenced
  globally and by temperature. Each nonscalar chunk length is `min(2, axis size)`;
  codecs follow the bytes/Blosc and scalar Zstd recipe above.

| Repository | Modification |
| --- | --- |
| `coord_non_subset` | Remove global coordinates attribute; spatial_ref has shape `[1]` on unrelated dimension crs_axis and remains referenced by temperature. |
| `coord_self_ref` | Temperature's coordinates string references itself as well as spatial_ref. |
| `global_aux_data` | Root coordinates string is `spatial_ref temperature_2m`. |
| `implicit_y` | Remove the stored y array. Queries use virtual integer indexes `[0, 1, 2]`. |
| `coord_attr_null` | Root coordinates attribute is JSON null; variable references remain. |
| `coord_attr_list` | Root coordinates attribute is JSON `["spatial_ref"]`; variable references remain. |
| `ensemble_int16` | Add members int16 `[0, 2, 7]`, chunk `[51]`, fill -1, bytes little endian and Blosc zstd level 3/shuffle/typesize **4**. Stack temperature/count along source axis 1, adding 0/100/200 for successive members. Data chunks are `min(2, size)`. |

## One-byte ensemble construction

`members_i1` and `members_u1` use three hourly times beginning at epoch second
1785369600, y float32 `[20, 22, 24, 26]`, and x float64
`[-110, -108, -106, -104, -102]`. Base temperature is float32
`arange(60).reshape(3,4,5) + 270.25`; stack five copies plus member-position
`0..4` on source axis 1, producing `(time, ensemble_member, y, x)`.
Members are `[-128, 1, 127, 7, 3]` for int8 and `[0, 1, 255, 7, 3]` for uint8.
Chunks use `min(3, size)`, with bytes (no endian field for width one) and Blosc
zstd level 3/shuffle/typesize 1. Float fill is NaN, signed integer fill is -1 and
unsigned fill is zero. The scalar geographic spatial_ref uses `sphere-6371229`.

`one-byte-integers.json` contains separate raw Zarr metadata for length-three
int8/uint8 member arrays, chunk `[3]`, fill 0 and `{"name":"bytes"}`.
Tests supply min/zero/max and zero/7/max byte buffers themselves. Multibyte endian
admission, lazy string axes and implicit-axis cases are generated in Rust by
`tests/common/mod.rs`, with explicit types, shapes, codecs and fill values.

## Deployed coordinate layout construction

`deployed_coordinates` preserves public coordinate codec and CF metadata layouts
with synthetic values. Latitude is `90 - i/4` for `i=0..720`; longitude is
`i/4 - 180` for `i=0..1439`. Init times are int64 epoch seconds
`[1619827200, INT64_MIN, -9223372036, -1, 0, 9223372036, 1792519200]`.
Lead seconds are float64 `[0, 3600, 7200, 9000, 86400, 1382400, NaN]`.
The 7×7 valid_time input is int64 `1619827200 + row + column`, encoded with its
full padded chunk `[27740, 209]`. Expected/ingested forecast-length arrays use
seconds `[1382400,0,1,NaN,3600,7200,86400]` and
`[0,1382400,NaN,3600,7200,86400,1]`; spatial_ref stores scalar 42.

Nested groups contain fractional duration seconds `[0.25,-0.25,0.1,1.000001,
1e-9,-1e-9,NaN]`, mixed arrays `[5000000001,1e-9,NaN]`, a mixed-chunk lead array
`[5000000001, 208 zeros, 1e-9]`, and large duration seconds
`[9223372036,-9223372036,0,NaN]`. These groups must stay outside the root view;
auxiliary valid_time payloads must stay lazy during open. Stored time/codec
metadata remain bounded separately from the adapter's metadata charge.

## CRS and metadata inputs

`live-crs.json` contains seven public WKT inputs only. Six have angular geographic
axes on spheres/ellipsoids, including one derived geographic pole rotation. One
uses a Lambert conformal projection. The Rust policy table explicitly chooses
Geographic for the six named spellings, Projected for Lambert and every unknown
or changed spelling. It also checks empty, malformed and identifier-only inputs.
These are policy choices, not loaded service classifications.

`live-coordinate-dtypes.json` retains public metadata dtype/shape/identifier
inputs. Its test deduplicates axis-name/dtype pairs and actually opens a small
Rust-built array for each pair, checking the chosen CF type or lazy string policy.
Inventory totals and derived query-axis annotations are not test expectations.
The separate `bounds` test binary owns the counting allocator and uses declared
axis sizes, byte widths, caps and fixed margins; it must remain isolated from
ordinary concurrent tests.
