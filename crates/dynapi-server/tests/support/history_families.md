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
| Lambert forecast | A stored y `[6000,0,-3000]`, x `[-3000,0,9000]`; B y `[7000,0,-3000]`, x `[-3000,0,10000]`, all in metres; `[init_time,lead_time,y,x]` |
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
At Lambert `(yIndex=0,xIndex=2)`, `s=2`, A uses `(x=9000,y=6000)` and B
uses `(x=10000,y=7000)`. These non-origin offsets are unequal, and swapping stored
index lookups instead selects `(-3000,-3000)`. A/B counts are `[1002,1012]` /
`[2002,2012,2022]`, with B humidity `[52,53,54]`.

The independently specified locations use the spherical LCC inverse (Snyder,
USGS Professional Paper 1395, section 15), directly from the WKT parameters:
`R=6371229 m`, origin/standard parallels `phi0=38.5 degrees`, central meridian
`lambda0=-97.5 degrees`, zero false easting/northing. In radians:

- `n = sin(phi0)` (equal standard parallels).
- `F = cos(phi0) * tan(pi/4 + phi0/2)^n / n`.
- `rho0 = R*F / tan(pi/4 + phi0/2)^n`.
- `rho = hypot(x, rho0-y)`, `theta = atan2(x, rho0-y)`.
- `phi = 2*atan((R*F/rho)^(1/n)) - pi/2`, `lambda = lambda0 + theta/n`.

Converting to degrees gives A `(38.55391184367471,-97.39650417743823)` and B
`(38.56289405072302,-97.38499028347202)`, each asserted within `1e-10` degrees.
The POST location is A's known point; B's moved cell remains its nearest point.
After B advances, A's cold reopened canonical response must retain its original
location, `/points/0,2` link, values, exact bytes and ETag. POST/canonical at B
must instead return B's known location. No projection implementation computes
fixture expectations during tests.

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
strict after-06:00 filter and limit one returning only 12:00. After another TTL expiry at 60 seconds, the run list still reflects B. Successful run listings have the mutable 30-second cache
policy and no ETag or query execution/cache headers. These contracts follow the
reference Python service's route, query selection and response-header policies;
the tests run entirely offline in Rust.
