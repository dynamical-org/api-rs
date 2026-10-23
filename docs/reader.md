# Snapshot-pinned numeric reader

The selection API is provisional. Public request validation, product selection,
service budgets and response presentation belong to the layers above the reader.
`dynapi-read` is a JSON test adapter, not a production transport or process boundary.

## Provisional contract

```rust,ignore
read_selection(&store_descriptor, snapshot_id, array_path, &selection).await
```

Descriptors are a local path, anonymous S3 `{bucket, prefix, region}`, or HTTP(S)
`{url}`. S3 explicitly disables credential discovery. HTTP URLs accept an optional
trailing slash. Both network backends use bounded retries/timeouts. All sessions are
readonly at the exact snapshot ID. A missing snapshot errors; there is no branch
fallback. Array paths are relative, with no empty, `.` or `..` components.

Provide one selector per stored dimension, in stored order: a nonnegative, zero-based
integer or an ordered list of integers. Scalars remove their axes. Lists retain axes
and preserve order and duplicates. Multiple lists are Cartesian (xarray plain-list
`isel`), not vectorized. Empty lists produce zero-length axes; other selectors are
still validated. The attested int64 scalar layout uses an empty selector list and
returns shape `[]`; other stored scalar layouts are excluded.

The result has `dtype` (`float32`, `float64` or `int64`), retained `shape`, and C-contiguous
little-endian bytes. The byte length equals the checked shape product times dtype
width. The reader decodes stored numeric values; it does not implement CF scaling,
masking of finite sentinels, or implicit date/time conversion. Explicit CF conversion
is a separate `cf_time::decode` call, described below. Typed errors are internal
reader errors, not HTTP contracts: `InvalidSelection`, `SizeLimit`, `Unsupported`,
`Snapshot`, `Storage`, `Decode`. Malformed CLI JSON has `InvalidRequest`.

The CLI reads exactly one JSON value on stdin and emits one JSON response on stdout:

```json
{"store_descriptor":"./fixtures/forecast.icechunk","snapshot_id":"EXPLICIT_SNAPSHOT_ID","array_path":"temperature_2m","selection":[0,[4,2,0,2],0,0]}
```

HTTP virtual chunks require the explicitly declared containers, supplied through
`read_selection_traced_with_containers` or `read_cf_selection_traced_with_containers`.
The existing entry points authorize none. The adapter accepts the same declarations:

```json
{"store_descriptor":{"url":"https://example.invalid/forecast.icechunk/"},"snapshot_id":"EXPLICIT_SNAPSHOT_ID","array_path":"wind_u_10m","selection":[0,0,0,360,720],"virtual_chunk_containers":[{"url_prefix":"https://example.invalid/chunks/"}]}
```

Each declaration has `url_prefix` (HTTP(S), ending in `/`) and optional `name` for
`vcc://name/...` references. Duplicate prefixes/names are rejected. The reader
replaces persisted containers with these anonymous HTTP declarations and grants
only anonymous HTTP access, enforced against the original exact prefixes by
the runtime fetcher. It supplies no authorization headers, S3 credentials, or environment credential discovery. Undeclared references fail
before a payload request. `../crates/dynapi-reader/tests/http.rs` covers repository
reads, ranged virtual reads and authorization failures using loopback only, with
synthetic input objects in `../crates/dynapi-reader/tests/fixtures/http` and
expected typed bytes derived independently from the documented recipe.
All HTTP acceptance cases run with `cargo test --workspace`, without Python.

Icechunk 2.2.2 matches raw URL prefixes for callback dispatch but decodes URL
prefixes for its native fallback. Runtime-only origin routes ensure encoded
slashes also reach the bounded callback; it checks the actual re-encoded request
URL against the original declared prefixes before any I/O. The native virtual
fallback is disabled with an intentionally invalid native timeout option, which
fails client construction before I/O; the callback ignores that option and uses
its own client. A durable test exercises that fallback directly. These runtime
routes do not alter the original names or persist a broader authorization.

The native virtual-chunk transport disables redirects: every 3xx fails without
requesting its destination, even on the same origin. Successful range responses
must be HTTP 206 with the exact requested Content-Range and, when supplied, a
matching Content-Length. Streaming collection is capped to that requested length
before copying each frame, including unframed and chunked bodies. No allocation
size comes from response headers. Missing or truncated bytes fail; object_store
retains its one retry (including remaining-range retries), 100 ms–1 s backoff,
5 s connect / 20 s read and request timeouts, and the outer 90 s deadline.
Reqwest protocol retries are explicitly disabled; object_store owns the retry
budget for requests and remaining-range body recovery. hyper-util can still
redispatch a request that was never written on a canceled pooled connection;
that adds no origin-side request. The GOAWAY witness uses fresh connections,
so it does not qualify pooled-connection cancellation. A physical-attempt guard
spans the client send through body EOF, error or drop.
Icechunk still checks recorded ETags/modification times. Legacy error bodies are
discarded before object_store can collect them. Tests measure early connection closure from
the server: inconsistent Content-Length closes before any body bytes, while
chunked/unframed overflow closes after at most 17 bytes (the 16-byte requested
range plus one excess byte). That byte count is what the staged test server observed. The
general guarantee is that the collector never holds more than the requested range: it
rejects the first data frame that would overflow it.

Repository HTTP uses a separate readonly implementation of Icechunk's storage
trait. Every config, repo, snapshot, manifest and ordinary stored-chunk read goes
through a no-redirect client; no native HTTP repository fallback is used. Object
keys are appended as path segments beneath the repository URL. Credentials,
query strings and fragments in repository URLs are rejected. Writes and object
listing are unsupported. Branch lookup uses the bounded repo object.

Full `config.yaml` and `repo` responses are capped at 1 MiB; other full metadata
objects at 64 MiB; chunks and individual requested ranges at 72 MiB + 16 bytes
(the existing int64 chunk, shard-index and codec envelope). Invalid or oversized
requested ranges fail before I/O. Full reads require HTTP 200; ranged reads
require HTTP 206 with the exact checked Content-Range endpoints and a numeric
total containing that range. A supplied Content-Length must fit the cap and
equal the requested range length. Streaming collection checks each frame before
copying, limits buffer growth, and checks EOF against the expected length.
Missing lengths remain bounded by the object cap or exact range length. Content
encodings other than identity are refused. Headers never determine an allocation.
Error/redirect bodies are dropped, including on the single retry for request
transport failures, 429 or 5xx. The client uses 5 s connect and 20 s read/request
timeouts under the reader's 90 s deadline. Repository range requests are sequential.
Typed missing objects remain missing; other failures cannot become fill data.

**Remaining repository-content trust requirement:** these are encoded transport
bounds, not a whole-process memory guarantee. Icechunk can preallocate from
repository metadata and decompress/parse snapshots and manifests after delivery;
its metadata expansion and caches are not bounded by this transport. Hostile
compressed metadata can still exhaust memory. Repository contents must remain
trusted until the upstream asset decode path is separately bounded. The reader
still has native dependencies and this HTTP implementation does not establish
WASM support.

Success: `{"dtype":"float32","shape":[4],"data_b64":"..."}`.
Failure: `{"error":{"kind":"Unsupported","message":"..."}}`.
Optional `DYNAPI_READ_TRACE=1` writes diagnostics to stderr. JSON/base64 allocation
and copying are test-adapter costs, not a chosen final transport.

## Attested capability

Before opening a decoder, the reader inspects metadata and rejects configurations
outside this allowlist:

- Zarr v3, stored `float32`/`float64`/`int64`, regular positive chunk shapes, default `/`
  chunk-key encoding, no storage transformers, at most 16 dimensions.
- Float codec chains: `bytes`, or `bytes` then `blosc`, optionally preceded by
  one `scale_offset` for float32 with NaN fill. Non-scalar int64 admits
  only the deployed little-endian `bytes` + `blosc` chain, with typesize 8, zstd,
  clevel 3, shuffle `shuffle`, blocksize 0. Int64 sharding and other chains are excluded.
- Scalar int64 admits only `bytes` little-endian + `zstd` with exactly
  `{"level":0,"checksum":false}`. This small layout is decoded separately with
  a fixed eight-byte destination through the same readonly Icechunk store. Frames
  must be one ordinary Zstd frame, have a known content size of exactly 8, no
  dictionary/checksum/reserved flags, and no trailing bytes. Encoded input is at
  most 64 bytes; the header independently limits non-single-segment windows to
  1 KiB, and the decoder also sets WindowLogMax(10). See D-encb-1 for the explicit
  header check, since fixed-buffer decoding can bypass that decoder parameter. Other standalone Zstd
  layouts remain excluded. The existing C-backed Zstd dependency is now direct;
  this adds no package versions to the lockfile. The existing attestation also
  requires the format's unused header bit to be zero. This intentionally rejects
  frames a general Zstd decoder may accept while ignoring that bit; it is stricter
  than the reference Python service, rather than an invalid-format requirement.
- The dataset entry point additionally admits rank-one int16 `bytes` little-endian
  + standalone `zstd` with exactly `{"level":0,"checksum":false}`, for member
  coordinates. Full padded chunks are at most 128 decoded bytes; frames are at
  most 256 encoded bytes. One ordinary frame must declare exactly that content
  size, with no dictionary, checksum, reserved flags, concatenation or trailing
  bytes. An exact-size destination and WindowLogMax(10) bound decode; the frame
  header independently limits a non-single-segment window to 1 KiB. Missing
  chunks use the admitted integer fill; edge padding is removed before scatter.
  This reuses the existing direct Zstd dependency. Other standalone non-scalar
  Zstd layouts remain unsupported and report a Zstd-specific typed error.
- `scale_offset` accepts only a leading configured codec with optional numeric
  `scale` and `offset`, defaulting to 1 and 0. Both must be finite after float32
  conversion; scale must remain nonzero. Unknown keys, string parameters, other
  dtypes, finite fills and additional array transforms are excluded. Decode uses
  float32 division followed by float32 addition, `(stored / scale) + offset`,
  in place on the decoded inner subset before scatter. Parameters are converted
  to float32 first; there is no float64 intermediate or fused operation. NaN fill
  remains NaN. Blosc validation still uses the original chain's metadata. See the
  [Zarr extension specification](https://github.com/zarr-developers/zarr-extensions/tree/main/codecs/scale_offset).
- `bytes` must specify exactly `{"endian":"little"}`. Big-endian storage is excluded.
- `blosc` must have exactly `typesize`, `cname`, `clevel`, `shuffle`, `blocksize`:
  matching numeric width, `zstd`, integer level 0–9, `noshuffle`/`shuffle`/`bitshuffle`,
  and automatic block size `0`. WeatherNext additionally admits only float32 LZ4
  with level 5, `shuffle`, typesize 4 and blocksize 0. Other metadata compressors
  and unknown options are rejected. Compressed frame headers must match the
  admitted compressor;
  uncompressed Blosc memcpy frames are a supported fallback, as detailed below.
- Alternatively, exactly one `sharding_indexed` codec: inner codec chain as above;
  inner shape divides outer shape; index chain is little-endian `bytes` then `crc32c`;
  index at `start` or `end`. No additional shard options/codecs are accepted.
- Fill is numeric, `NaN`, `Infinity`, or `-Infinity`; absent shards and inner chunks
  fill through zarrs; scalar int64 absence returns its numeric Zarr fill. Int64 fill
  must be a representable integer without a masking `_FillValue` attribute.
  Corrupt payloads and truncated/bad-checksum shard indexes give typed decode errors.
- General array attributes are a closed allowlist: `long_name`, `short_name`,
  `standard_name`, `units`, `step_type`, `comment`, `coordinates`, `grid_mapping`,
  `_FillValue`. Unknown attributes (including `_Encoding`, arbitrary `dtype` and
  `bounds`) are refused before decoder construction.
- Reserved coordinate basenames `latitude`, `longitude`, geographic `x`/`y`,
  `init_time`, `lead_time`,
  `valid_time`, `expected_forecast_length`, `ingested_forecast_length` and
  `spatial_ref` require their exact pinned tuple in `coordinate_profiles.json`:
  dtype, fill, chunk grid/key encoding, codecs, dimension names (when present)
  and complete attribute object. Logical shape may vary. This admits only the
  attested per-array additions: latitude/longitude axis and statistics; time
  statistics plus their exact calendar/dtype tuples; and spatial_ref CRS fields.
  These attributes are not added to the general allowlist.
- The canonical base64 NaN `_FillValue` (`AAAAAAAA+H8=`), emitted by xarray for
  both float widths, is allowed only alongside NaN Zarr fill. String `NaN` and
  base64 float32 forms remain excluded. Time conversion is still explicit.
- An array named by any sibling's `bounds` attribute is refused, including the
  reserved coordinate profiles. Xarray can inherit time units through that link.
  The sibling scan still refuses more than 4,096 sibling nodes or a returned
  sibling metadata object larger than 1 MiB. Its metadata gets are included in
  the logical access/byte trace.

The supported layouts include plain bytes, plain Blosc, sharded Blosc and sharded
bytes; standalone float Zstd and unrecognized configurations are rejected. The dataset
entry point already attests data variables lazily; an unsupported data variable
does not prevent opening and reading supported siblings. Query dimension
coordinates remain eager, so an unsupported required coordinate can fail open.
The stricter stored-value entry point retains its pinned coordinate profiles;
dataset mode uses its separate descriptive-metadata/coordinate admission.
Migration checks compared dtype and shape first, then uint32/uint64 views and raw
bytes, including malformed frames for the int64 scalar Zstd exception.

## Bounded work and selective reads

Output is limited to 64 MiB, one decoded float inner chunk to 56 MiB, one int64
chunk to 64 MiB (other integer widths keep 16 MiB), the shard index to 8 MiB, selector entries to 1,048,576, and touched
inner chunks to 65,536. Size products are checked before allocation or decoder
construction. Reads/decode are sequential, with codec concurrency 1 and a
90-second reader timeout. These are prototype reader bounds, not a production
admission system or cache scheduler. The pinned `valid_time` padded chunk
[27740,209] needs 46,381,280 bytes even when a selection returns only four values.
The exact coordinate profiles keep their attested dtype, fill, chunk/codec and
attribute tuples; logical array shape may vary within the checked limits.
Profiles are selected by basename, including in nested groups. They establish a
supported schema, not dataset provenance or downstream product eligibility.
Complete Zarr v3 metadata is parsed before selection shortcuts or scalar decoding;
unknown top-level fields are accepted only with `must_understand: false`.

Admission also scans the same group's immediate siblings for `bounds` references.
It refuses more than 4,096 immediate group nodes (including the selected array)
or a returned sibling metadata object larger than 1 MiB. These checks run after listing/fetching; they limit accepted
scan/parse work, not all Icechunk repository metadata or fetch allocations. An
empty stored-value selection still validates metadata, coordinate profiles and sibling
bounds, but fetches no chunk payloads. Explicit CF reads of dimension coordinates
retain the full decoding unit even for empty selections, as described below.

The reader groups selected positions by inner chunk, decodes each touched chunk
once, and scatters selected values into the output. Ordered duplicates do not
cause duplicate decode work. The trace records logical get/range calls, including
target metadata, sibling-scan metadata, shard indexes and selected payloads. It
distinguishes four byte counts:

- `decoded_bytes`: full padded inner-chunk work units, including fill units;
- `subset_bytes`: materialized inner subsets clipped to array bounds;
- `output_bytes`: the final selected buffer, including duplicates;
- `store_bytes`: bytes returned by logical Zarr gets, including sibling metadata,
  encoded payloads and repeated gets.

For f32 shape [3], chunk [2], shard [4], selecting [2] reports 8 decoded-unit bytes,
4 subset bytes and 4 output bytes. Migration checks verified exact payload/index
ranges and poisoned an untouched inner payload to detect unwanted decoding. Full
inner chunks are decoded; plain chunks and single-inner shards can be fetched in
full. Multi-inner sparse shards use ranges, and indexes may be fetched again for
another touched inner chunk in the same shard.

This instrument records get/range deliveries at the Zarr/Icechunk session boundary,
not listing operations or physical S3/filesystem traffic. Icechunk can load
manifests containing inline bytes and can cache/retry below this boundary. No
service memory savings follow from these counters.

`zarrs_icechunk` 0.5.0 loses typed missing-chunk results in partial reads. The
observing wrapper uses the same readonly Icechunk session for typed partial gets:
only a typed NotFound becomes fill. Empty encoded data and other storage errors
remain errors; there is no string-based missing-data classification. The scalar
int64 path returns its attested numeric fill when its chunk is absent.

The adapter validates selected payloads before zarrs can decode them. Icechunk's
logical `getsize` reads the chunk reference before payload fetch. Encoded chunks
and inner payload ranges are capped at the dtype ceiling plus 16 bytes of Blosc
overhead; plain bytes must equal the full padded inner size, including at array
edges. The separate scalar Zstd path retains its 64-byte encoded/eight-byte decoded
bounds described above.

For Blosc, the 16-byte header's little-endian `nbytes` must exactly match the
metadata-admitted full padded inner size, and `cbytes` must match the fetched
length. An inconsistent 128 MiB frame is rejected before expansion. Migration-time RSS checks measured the reader subprocess below 96 MiB on plain,
partial-shard and full single-inner-shard paths. Those checks are not part of this
repository; logical byte counters alone do not establish a process-memory bound.

The frame policy retains the metadata-admitted compressor, typesize and shuffle
mode and checks them before zarrs receives a selected frame. Blosc dispatches from
the frame header, so a metadata check alone does not establish the actual decoder.
The header audit uses the pinned c-blosc bundled by blosc-src 0.3.8:

- Byte 0 must be supported Blosc format version 2, with its 16-byte header. Reserved
  flag bit 3 (`0x08`) is rejected. No future format/flag interpretation is admitted.
- For compressed frames (memcpy flag bit 1 clear), bits 5–7 must equal the admitted
  compressor id (**4** for Zstd, **1** for the admitted LZ4 layout), byte 1 must be
  compressor format version **1**, and shuffle bits 0/2 must exactly match metadata:
  0 for noshuffle, `0x01` for shuffle, `0x04` for bitshuffle. Setting both shuffle
  bits is rejected. LZ4, zlib and blosclz frames cannot reach those decoders through
  Zstd metadata; Zstd frames likewise cannot reach that decoder through LZ4 metadata.
- Byte 3 typesize must match the admitted metadata for **both compressed
  and memcpy** frames, with one exception: header typesize 1 is accepted when
  metadata typesize equals the stored dtype width. The decoder uses the header
  typesize for unshuffling and partial-read alignment. Byte shuffle with item size
  1 is an identity transform, and header typesize 1 aligns every numeric read. Dataset integer layouts
  can declare a different shuffle item size (for example int16 with typesize 4);
  that case still requires an exact header match. All other mismatches are
  rejected before decode. Size, compressor, version and shuffle checks remain.
- Memcpy frames (bit 1 set) copy raw bytes and bypass compressor dispatch and both
  shuffle transforms in the pinned C decoder. Their compressor/version/shuffle bits
  are therefore ignored, rather than requiring zstd/shuffle bits that do not execute.
  Format version, reserved flags, matching typesize, nbytes/cbytes bounds and
  `cbytes == nbytes + 16` remain mandatory. Migration checks covered memcpy frames
  from zstd/LZ4/zlib/blosclz and all three shuffle settings on all read paths.
- Bytes 8–11 hold the actual blocksize, which must be positive and <=nbytes. Metadata
  blocksize 0 requests automatic encoder sizing, so it is not compared literally
  to the nonzero frame blocksize. Known split flag bit 4 may be set or clear: it is
  an internal layout choice with no corresponding admitted metadata setting, and
  stays within the same checked compressor decode. The pinned decoder validates block
  offsets/lengths. Compression level is an encoding-only choice, not a frame decode
  selector or a serialized level field; its 0..9 metadata check is retained.

Migration checks exercised compressor and transform mismatch rejection on plain,
partial-shard and full single-inner-shard reads in debug and release builds.

Before zarrs sees a shard index, the adapter checks every non-missing offset/length
with checked addition, ordering, data-region bounds, and encoded-size limits. Index
bytes cannot be referenced as payloads. Both u64::MAX fields together are the only
missing-entry sentinel. Zarrs still checks CRC32C; a valid checksum does not establish
range safety. Payload range conversion repeats bounds/overflow checks **before**
constructing an Icechunk range or slicing inline data. Full-shard decode is allowed
only for bounded single-inner shards, whose index and frame are checked first.
Partial reads retain at most one validated index and fetch/decode only touched inner
payloads. Migration checks covered CRC-valid malformed inline indexes in debug
and release builds; there is no panic-catching fallback. Validating every entry
is intentionally stricter than the reference Python service: Rust rejects an out-of-bounds
unselected entry even when all selected payloads would otherwise be readable.

## Catalog units and staging ledger

The 56 MiB float ceiling (58,720,256 bytes) covers every inventoried root-array
layout. Largest units are analysis arrays: IMERG 57,600,000 bytes, MRMS
25,920,000 bytes and HRRR 17,496,000 bytes. IFS forecast units require
17,756,160 bytes (15-day) and 18,213,120 bytes (46-day six-hourly).
Concurrent decode pools must budget for the largest admitted per-dtype unit:
64 MiB, the int64 ceiling (`max(MAX_CHUNK_BYTES, MAX_INTEGER_CHUNK_BYTES)`).
Output, index, metadata, selector and work budgets remain independent.

For one sequential numeric decode unit, a conservative Rust staging charge is
`2E + 3D + 5I + O`, plus metadata and selector/group charges:

| Term | Charge |
| --- | --- |
| E | Encoded frame delivery/cache staging, at most the dtype ceiling + 16 bytes |
| D | Full padded decoded inner unit, at most 56 MiB (int64: 64 MiB) |
| I | Shard index staging and validated ranges, charged separately; encoded index <=8 MiB |
| O | Retained selected output, at most 64 MiB across the read |

The five index terms cover encoded/decoded index staging, the retained u64 index
and up to twice the nonmissing-entry count for the validated-range Vec capacity. Scale/offset
runs in place and adds no D-sized buffer. The small standalone int16 Zstd path
instead uses <=256 encoded and <=128 decoded bytes, plus its decoder context and
at most a 1 KiB window, as in the existing scalar decoder. Traces continue to
charge full padded decoded work and the returned subset bytes.

`float_chunk_boundary_and_in_place_scale_obey_unit_staging_charge` checks exact
56 MiB decode units with and without the transform using an isolated counting
allocator, against this charge and a fixed 2 MiB fixture/upstream allowance. A
chunk one float larger fails before payload allocation; the arithmetic unit test
also checks exactly one byte above the ceiling. These tests count Rust-requested
allocations. Native codec scratch, upstream repository metadata expansion/caches,
and whole-process RSS remain outside this charge; it is not a process memory cap.

WeatherNext pressure-level groups are outside the point-query contract in both
services and outside the reference dataset root. Historical transposed pressure
arrays need 215,953,920 decoded bytes each and return SizeLimit before constructing
a codec. Transpose remains unadmitted. Operational pressure arrays may fit the
byte ceiling but require unsupported pressure dimensions at the query layer.
IMERG's full-axis selector staging exceeds the separate 64 MiB dataset-open
budget. The dataset adapter can instead retain exact eager integer-epoch axes
using sequential batches of at most 262,144 selectors under that unchanged cap;
see [dataset accounting](dataset.md). This does not change physical chunk decode
units: multiple batches selecting the same inner chunk decode it again. Float
duration dimensions retain their required full-axis CF context.

## Explicit CF time decoding

`cf_time::REVISION` is `m1b-cf-time-v1`. The module uses numeric operations and
serde only: no Tokio, threads, storage, clocks, or native codec calls. The enclosing
reader still has native I/O/C-backed dependencies; this module alone does not prove
the reader crate builds for WASM.

The caller passes a stored `TypedBuffer` and a `cf_time::Request` containing that
revision and its associated attributes. The caller owns the metadata/buffer
association. Accepted tuples are exactly:

| Stored dtype | units | calendar | dtype attribute | _FillValue attribute |
| --- | --- | --- | --- | --- |
| int64 | `seconds since 1970-01-01` | `proleptic_gregorian` | absent | absent |
| float64 | `seconds` | absent | `timedelta64[us]` | `AAAAAAAA+H8=` |

No epoch, unit, calendar or dtype aliases are accepted. Scaling, finite masking
sentinels and other CF transforms remain excluded. The first tuple covers
`init_time` and `valid_time`; the second covers `lead_time` and both forecast-length
auxiliary arrays. Known descriptive attributes are ignored by the pure CF operation; unknown
attributes are rejected. Stored coordinate admission checks the complete pinned
attribute object. Output preserves
shape and contains little-endian int64 bits tagged `datetime64[ns]` or
`timedelta64[ns]`. Integer i64::MIN and floating NaNs become NaT (i64::MIN).
Integer epoch zero is valid, not missing. Checked conversion rejects out-of-ns-range
values and infinities (D-encb-2 records xarray's per-element NaT difference for
overflowing float durations). Fractional seconds follow xarray's successive s→ms→us→ns
multiplications and integer truncation. This operation decodes the supplied buffer;
for float time data, xarray's resolution choice can depend on other values in the
same decoded buffer, so callers of the pure function must supply the complete
corresponding decoding unit before selecting decoded values.

The installed xarray defaults preserve the deployed duration `dtype` hint as
`timedelta64[us]` for integral seconds; migration checks normalized default-decoded
values to nanoseconds for the public API's time arithmetic. Sub-microsecond values
can produce nanosecond resolution. Scalar NaT has an xarray/NumPy datetime dtype
artifact; the reference Python service normalizes using the coordinate's declared time kind.

The CLI accepts an optional `cf_time` object with `revision` and `attributes`; omitting
it keeps stored-value behavior. It calls `read_cf_selection_traced`, which reads and
decodes a complete float duration dimension coordinate (one dimension named for
the array basename) before applying scalar/list/empty selections. This matches
xarray's eager index construction; fractional values in unselected chunks can
change selected nanoseconds. Full units obey the existing byte/selector/chunk
bounds and timeout; oversized units are rejected. The trace includes complete-unit
read/decode work, while output_bytes counts only the selected result. Auxiliary
durations retain xarray's lazy selection-before-decoding behavior. Integer epoch
seconds have no value-dependent context and keep selective reads. The pure
`cf_time::decode` remains separate, and `read_selection` returns stored values.

The pinned coordinate tuples are stored in
`../crates/dynapi-reader/src/coordinate_profiles.json`. Migration checks used
smaller logical shapes with the same codec chains, padded chunks, fill values and
CF attributes. They compared raw/decoded bits, ordered and empty selections,
initialization boundaries, NaT/fill, fractional leads, range errors and malformed
scalar Zstd frames. Those development checks are distinct from the Rust-owned
tests committed in this repository.



### Private provider transport policy

The shared range connector/collector also has private, opt-in anonymous-provider
primitives. They do not enable catalog products, provider authorization or GRIB
reader support. The legacy HTTP adapter retains its conditional formatting,
error mapping, proxy configuration and outer deadline.

Provider policy uses a proxy-free client, identity encoding and a 30-second
absolute deadline across attempts, backoff and body collection. It verifies
required ETag/Last-Modified responses and exact range lengths after collection.
A missing or unparseable required validator is classified as an object change
before object_store erases the header error. A complete 412 status has that same
classification regardless of its ignored body.
Only a terminal S3 503 with a complete valid bounded Error/Code=SlowDown document
has that typed transient classification. Earlier attempts cannot promote a final
404, generic error, changed validator or interrupted body. Whole-operation expiry
has its own classification. No provider diagnostic text is synthesized for the
legacy mapper.

The quick-xml slice parser borrows a UTF-8 error buffer capped at 4096 bytes,
uses a fixed eight-element tag stack and admits at most 256 events and one direct
Code child of at most 64 ASCII bytes. Each element admits at most 32 attributes;
a fixed 512-byte borrowed-key set detects duplicates without quick-xml's growing
attribute-tracking allocation. It rejects malformed documents/attributes,
entities, DTD, CDATA, nested/duplicate/attributed Code, unsupported encoding,
multiple roots and trailing data. Attribute checks and parser errors can allocate
bounded scratch; opaque HTTP/TLS buffers are outside the retained-error cap.
Loopback unit tests cover physical HTTP/2 refused streams, outer connection
retries, terminal-attempt transitions, validators, EOF/drop/deadline ownership,
parser boundaries and measured parser scratch. These are transport witnesses;
provider endpoint parity and whole-service memory remain qualification work.
