# Constructed encoding inputs

These files are codec inputs, not saved decoder or service answers. The Rust
constructor at `../../../examples/construct_encoding_fixtures.rs` writes the
numeric byte buffers and 274 Blosc frames using the already pinned encoders.
From the repository root, regenerate with:

```sh
cargo run --locked -p dynapi-reader --example construct_encoding_fixtures -- crates/dynapi-reader/tests/fixtures/encoding-breadth
```

`inputs.json` identifies each raw buffer, literal integer values and producer
frame. Its short shuffle-name aliases refer to the same level-three file;
files are not duplicated. These aliases are informational; the test matrices use
the explicit level keys. `duration-inputs.json` is a separate literal recipe
and is not rewritten by the constructor. Tests construct repositories, shard
indexes, memcpy headers, scalar frames and corruptions in temporary directories.
They derive expected numeric values from these literals and the formulas below.
Neither regeneration nor tests require Python, network or service responses.

## Integer and float buffers

For each `int8`, `uint8`, `int16`, `uint16`, `int32`, `uint32`, `int64`, the first
seven of 256 little-endian elements are `[minimum, 0, maximum, fill, 7, 1, fill]`.
Signed fill is -11; unsigned fill is 23. Remaining elements are fill. Extrema
are the exact dtype limits, including int64 values beyond binary64 precision.
The two 256-element float32 buffers are `0.25 + i % 4` and `1000.5 + i % 4`.

Each buffer has actual producer Zstd levels 0–9 with `noshuffle`, `shuffle` and
`bitshuffle`, native typesize and blocksize zero. Level zero produces memcpy;
the constructor requires levels 1–9 to produce compressed frames. Width-four
buffers additionally have LZ4 level five / shuffle. Tests independently construct
forced memcpy headers for every metadata combination. The nine raw buffers are
also committed, and expectations use integer literals or exactly representable
float arithmetic, rather than decoding a raw file to obtain an answer.

## Shards, missing values and malformed inputs

The float matrix crosses 63 configurations (bytes; 30 Zstd level/shuffle pairs
with producer and forced memcpy; LZ4 with both forms), plain/start/end index
locations and stored/all-missing payloads: 378 cases. A logical array has 2,048
float32 elements, inner chunks of 256 and outer shards of 1,024. Inner entries
zero/two hold buffers A/B; entries one/three are absent, and the second whole
shard is absent. Fill is -7.25. Full, sparse `[513,0,257,513,1039]` and empty
selections assert shape, dtype and exact bytes. The sparse values are
`[1001.5,0.25,-7.25,1001.5,-7.25]` with duplicate order preserved.

Each index entry is a little-endian uint64 offset/length pair; missing entries
are `(2^64-1, 2^64-1)`. Four entries plus a CRC32C give 68 index bytes.
CRC32C uses the reflected polynomial 0x82f63b78 and checks the independent
`123456789 -> 0xe3069283` vector. Index mutations recompute CRC for arithmetic
faults, covering index overlap, range overflow, half-missing sentinels, outside
payloads and invalid unselected entries. Bad CRC and short indexes are separate
cases. All seven faults cross 63 configurations and both index locations:
882 cases. Arithmetic faults additionally assert the adapter's pre-decode message,
so a later decoder failure cannot stand in for that guard. See the [Zarr indexed sharding specification](https://zarr-specs.readthedocs.io/en/latest/v3/codecs/sharding-indexed/index.html).

The 2,613 frame cases cross every configuration and all three layouts. They
cover short headers, version/reserved/type/size/block fields, dispatch/shuffle
fields (byte-shuffle bit 0 and bitshuffle bit 2 separately), truncated/trailing
data and compressed-stream length corruption.
Memcpy ignores compressor/version/shuffle dispatch fields; these cases must
succeed with exact values. Plain or memcpy payload changes are valid data and
must return the changed value, not an invented integrity failure. This table
asserts the reader's admitted header policy; it does not claim that arbitrary
payload bit changes are detectable without a checksum.

## CF duration units and scalar Zstd

`duration-inputs.json` stores binary64 seconds; null means an absent NaN-fill
chunk. Full dimension coordinates decode all chunks before selection; auxiliary
arrays decode selected values. Mixed `[5000000001.0, 1e-9, NaN]` needs nanosecond
promotion: IEEE ties-to-even produces `5000000000999999488` for the large value,
512 ns below its integral-seconds result. Selecting only the large auxiliary
value gives exact `5000000001000000000`. Fractional, negative, NaT, duplicate,
empty and unselected-overflow cases assert dtype, shape, nanosecond bytes and
full-unit versus selected work. The generic traced API and descriptive dataset
entry point are checked separately with their respective admitted metadata.
D-encb-2 records the existing whole-unit typed rejection where the reference
converts an overflowing full-coordinate value to per-element NaT.

`scalar_zstd.rs` constructs ordinary single-segment raw/RLE frames and explicit
non-single-segment frames according to [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878).
Int64 extrema, -1, zero, 2^53+1 and missing fill are literal expectations. Valid
frames exactly 64 and 65 encoded bytes distinguish the input cap from format
validity. Other cases cover flags, content size, actual decoded length, reserved
blocks, truncation, concatenation and trailing data. The private guard test owns
empty input because Icechunk refuses to store zero-length chunks. Explicit
window descriptors distinguish 1 KiB (accepted), 1,152 bytes and 1 GiB (rejected);
see D-encb-1 for this deliberately narrow scalar policy.
