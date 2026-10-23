# Historical family recipes

`historical_families.rs` constructs each repository in a private temporary
directory using the Rust Icechunk API. No committed repository is changed and no
reference-service output is a fixture. A and B are real commits on the same main
branch, served through the loopback catalog/store host without restarting the
application or clearing its caches.

All arrays use Zarr v3, one whole-array chunk, a little-endian bytes codec and
explicit dimension names. Count is int64; humidity is float64 with unit `%`.
The catalog advertises both variables throughout; humidity exists only in B.
The root declares the scalar `spatial_ref` coordinate. Geographic WGS84 and the
attested Lambert conformal WKT come from the existing `live-crs.json` input.

| Family | Coordinates and weather dimensions |
| --- | --- |
| Analysis | Latitude `[44,42]`, longitude `[0,60]`; `[time,latitude,longitude]` |
| Ensemble | Same spatial axes; `[init_time,lead_time,ensemble_member,latitude,longitude]`; A member IDs `[7,-2,7]`; B either retains those IDs or changes to `[-2,11]` |
| Lambert forecast | Stored y `[3000,0,-3000]` and x `[-3000,0,3000]` in metres; `[init_time,lead_time,y,x]` |
| Run discovery | Geographic deterministic forecast, `[init_time,lead_time,latitude,longitude]` |

A analysis times are July 30, 2026 at 00:00 and 01:00 UTC; B appends 02:00.
Forecast A initialization times are 00:00 and 06:00; B appends 12:00. Forecast A
lead seconds are `[0,3600]`; B appends 7200. Epoch coordinates are int64 seconds
since 1970-01-01 with the proleptic Gregorian calendar. Leads are float64 with
CF duration attributes (`seconds`, `timedelta64[us]`, encoded NaN `_FillValue`).

For zero-based run ordinal `r` (zero for analyses), time/lead ordinal `t`, member
ordinal `m` (zero for non-ensemble fields), and flat spatial index `s`:

- A count: `1000 + 100*r + 1000*m + 10*t + s`.
- B count: `2000 + 100*r + 1000*m + 10*t + s`.
- B humidity: `50 + 10*r + m + t + s`.

Member identity is deliberately independent of ordinal. The two A members
identified as 7 have counts `[1000,1010]` and `[3000,3010]` at `(0,0)` for the
00:00 run. The changed B axis drops a duplicate, changes identity and cardinality,
and retains source order; its first member is -2 but has ordinal-zero B values.
At Lambert `(yIndex=1,xIndex=1)`, `s=4`, the projection origin is
`(latitude=38.5,longitude=-97.5)` and A/B counts are `[1004,1014]` /
`[2004,2014,2024]`. Returned coordinates must match across POST and canonical GET
and both commits, with a small numerical tolerance only for the independently
known origin. Historical replay compares exact bytes and ETags.

Analysis requests `[01:00,03:00)` yield A `[1010]` and B `[2010,2020]`.
`[01:00,02:00)` excludes the new 02:00 record, `[02:00,03:00)` is empty in A,
and an unbounded canonical query selects the last time of its own snapshot.
The test spells A's complete canonical response bytes independently.

The dataset cache holds two entries. A POST and its pinned GET warm main A and
pinned A. After committing B, clock 29 still serves main A; clock 30 refreshes
main to B. Opening pinned B evicts pinned A. Reopening A must fetch its snapshot,
return byte-identical body/ETag and immutable caching, then become a cache hit.
Schema errors reject humidity on A; B POST/canonical expose its metadata and
known values. The same process covers both ensemble axis variants and Lambert.

Run discovery warms its metadata-only profile and the point-query profile.
Both may serve A before expiry. At 30 seconds both must expose B regardless of
which profile refreshes first: newest-first runs `[12:00,06:00,00:00]`, with a
strict after-06:00 filter and limit one returning only 12:00. Revalidation at 60
seconds retains B. Successful run listings have the mutable 30-second cache
policy and no ETag or query execution/cache headers. These contracts follow the
reference Python service's route, query selection and response-header policies;
the tests run entirely offline in Rust.
