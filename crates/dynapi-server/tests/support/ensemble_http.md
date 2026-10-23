# Ensemble HTTP input recipes

`ensemble.rs` constructs temporary physical Icechunk repositories in Rust. No
response, selector or value captured from another service is loaded by these tests.
The original `dynapi-dataset/tests/fixtures/ensemble` repository supplies a second,
independent stored Blosc/Zstd input with its documented numeric construction.

The constructed forecast axes are two UTC runs (2026-07-30 00:00 and 06:00),
three leads (0, 6 and 12 hours), three members, latitude `[44, 40]`, and longitude
`[-103, -99]`. Epochs use int64 seconds with proleptic Gregorian CF metadata;
leads use float64 seconds with the documented microsecond-duration attributes.
Member identity is deliberately separate from member position:

- int16 `[7, -2, 7]`, chunk 51, little-endian bytes plus standalone Zstd or
  Blosc/Zstd with typesize 4; the repeated ID must retain distinct position values;
- int8 `[-128, 7, 127]` and uint8 `[255, 0, 7]`, Blosc/Zstd/typesize 1;
- int32 `[-2147483648, 7, 2147483647]`, uint16 `[65535, 0, 7]` and
  uint32 `[4294967295, 0, 7]` with plain bytes;
- int64 `[9007199254740993, -2, 7]` with plain bytes, preserving JSON integer identity.

For source indices `(r,l,m,y,x)`, let `s = 10000*r + 1000*l + 100*m + 10*y + x`.
Temperature is float32 `s+0.25` in `(run,lead,member,lat,lon)` order; wind is
float64 `s+0.5` in `(lon,lead,lat,run,member)`; count is int64 `2^53+1+s` in
`(member,lon,run,lat,lead)`. Deterministic temperature has no member dimension,
uses `(lon,run,lat,lead)` order and omits `100*m`. Orography is float64
`100+10*y+x` in `(lat,lon)` order. Its per-member response is a one-element list,
independent of the selected lead count. Deterministic temperature repeats its
selected lead list for each member. A query containing only deterministic
variables uses the ordinary `data` shape without an invented member axis.

Inner chunks are 1 on run and 2 on every other dimension. Sharded cases double
lead/member chunk widths, use an end index of little-endian offset/length pairs
and CRC32C, and retain padded edge cells. Tests exercise unsharded frames, indexed
shards and byte-sized frame headers under native-width metadata. Numeric fill
cells are NaN for floats and zero for integers; selectors never include padding.

The source encoder constructs Blosc v1 compressed-mode, unsplit Zstd blocks with
byte shuffle, including a short trailing shuffle item. Its Zstd frames use the
RFC 8878 single-segment/raw-block format with explicit lengths. This exercises
Zstd decoding and shuffle rather than the Blosc memcpy shortcut; entropy coding
and every compression-option combination are outside this recipe. The stored
ensemble fixture additionally covers normally compressed physical inputs.

Both selected points and runs have independently calculated two-lead values.
HTTP assertions own each member ID/value, valid-time and lead list, mixed-variable
shape, unit, canonical forecast identity, immutable GET policy, repeat body and
SHA-256 ETag. There is no member-filter request field: it is rejected before store
reads. Physical string/vlen-UTF8 member arrays remain an existing unsupported
encoding and return the existing generic error; these tests do not add string
codec support. The current public ensemble member coordinates are int16.
