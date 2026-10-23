# Private GRIB decoder qualification

`dynapi-reader/src/grib` is a native library module without a reader, transport,
server, catalog, or global codec-registry caller. It does not enable a product or
change existing float32 decoding. Its dependencies pin gribberish, its types and
its macros to 1.5.0 with codec defaults disabled. PNG, OpenJPEG and libaec are not
part of this dependency path. Native qualification does not establish WASM runtime
support or whole-process memory bounds.

## Input and ownership

One call borrows a complete GRIB2 message and an attested f64 padded chunk profile:
shape, adjacent y/x axes, singleton other axes, data-order flags and an optional
validated f64 scale/offset. It returns one owned f64 vector and shape; a consuming
conversion produces little-endian bytes. These are private types, not a new wire
format or supported external API. The profile does not admit the special GRIB
latitude/longitude codec mode or calculate projected coordinates.

The encoded limit is 32 MiB; the padded decoded limit is 56 MiB. `charges` validates
the same shape profile and the complete logical encoded length before a caller
allocates an input buffer. Its qualification targets, rounded up to 64 KiB, are:

- F = 4E + 2 MiB, with E the complete logical message length.
- X = 8D + 64 KiB, with D the full padded f64 plane, not selected output bytes.

At maximum D (58,720,256 bytes), the 5.3 all-but-one bitmap witness measured
128,180,508 peak live bytes including its 2,351,315-byte owned input. The peak
minus input was 125,829,193 bytes, about 2.14D (roughly 2.2D rounded upward),
beside X = 469,827,584 bytes. This witness includes row/longitude ordering,
f64 scale/offset and final byte conversion; preflight alone requested zero bytes.
This measurement does not tighten X or establish bounds for other profiles.

The library does not acquire permits. A future caller must fund the input backing,
decode/reorder/scale and result/scatter lifetime before those operations. Borrowing
a short slice of a larger allocation does not charge or release that larger backing;
the caller must account its actual retained capacity. Retaining the result retains
its allocation. No cancellation preemption or early credit release is implied by a
synchronous call. At maximum E, F is 130 MiB; a 256 MiB F pool funds one such unit.

## Qualified surface and restrictions

Admission is by the entire checked profile, not its template number. Preflight
runs before the first gribberish constructor and uses no dynamic scratch buffer.
It checks section order, exact framing, required section sizes, grid/shape counts,
packed counts, bitmap length/popcount, group-table/data bounds and zero padding.
Repeated fields, trailing messages, GRIB1, image templates and CCSDS are rejected.

Data grid templates 3.0 and 3.30 have fixed lengths, row-major/nonstaggered scans,
and positive nonmissing spacing in the admitted directions. Geographic grids use
microdegree units. The wrapper computes the same near-global longitude-roll gate
and northern-row order from checked fields and moves values in place. Lambert
ordering uses the sign of the row step and does not instantiate a projection; CRS
and coordinate qualification remain outside this module.

Packing 5.0 admits nonzero widths up to 31 bits, exact packed byte counts and legal
padding. Zero-bit fields are deliberately rejected, including nonzero reference
values that the pinned decoder would incorrectly replace with zero, and empty
bitmap streams that would lose missing masks. No constant output is synthesized.

Packing 5.3 admits first/second-order spatial differencing, descriptors of 1–4
octets, group-table/effective value widths up to 31 bits, and no/primary/secondary
missing management. Missing values do not advance the reconstruction state.
Zero groups are deliberately rejected as constants before the pinned bit loader
can panic. Zero descriptor octets are an unsupported descriptor width, not a
constant field. Zero group-reference width alone is allowed; it is not a constant test.
Groups must have positive lengths and their sum must equal the packed count.
Previous/predefined bitmap reuse is not admitted.

Additional implementation-specific restrictions:

- Every group-table bit product is at most 2^24, making the pinned f32 alignment
  calculation exact. Larger valid tables are unqualified.
- Zero-bit group widths with a nonzero width reference and multiple groups with
  zero-bit length fields are unqualified: the pinned implementation ignores the
  corresponding references. No successful values are claimed for those profiles.
- Preflight checks the exact u32 length multiplication/addition and width×length
  operations. It also checks raw+reference and the exact left-associated i32
  reconstruction intermediates, including `2 * previous` before subtraction.
  Mathematical rearrangements cannot justify accepting an intermediate overflow.
- Reference/scaling factors must be finite and GRIB binary/decimal factors must
  be nonzero after f64 computation. Float output overflow still follows IEEE
  arithmetic. Invalid or unqualified scale profiles are typed failures.

The integer-state validation exists to reject overflow before unchecked upstream
operations; it does not produce decoded values or replace the GRIB decoder.
Malformed framing/count/padding failures are distinguished from valid but
unqualified profiles and size/arithmetic limits. `Message::data().resize` is not
used to establish consistency: preflight establishes exact counts first. No
runtime catch-unwind boundary is used as admission policy.

The optional Zarr scale/offset applies once after decoding/reordering, using f64
`decoded / scale + offset`, with a rounded division before addition. Scale defaults
to 1 and offset to 0; nonfinite parameters and either signed zero scale are rejected.
Input NaNs/infinities and signed zeros follow those IEEE operations. This transform
is distinct from GRIB binary/decimal scaling and any later API unit conversion.

## Evidence provided by tests

Fixtures are constructed in Rust from explicit section recipes and independently
stated values: signed scale factors, first/second differences, missing groups,
bitmap/padding, row/longitude order, f64 parameter precision and typed constant
rejections. No reference-service captures or Python/network dependency is used.
Exact-error fixtures defend bitmap popcount in both directions, group totals
before resize/bitmap expansion, ignored zero-bit multi-group length references,
descriptor widths above four octets and oversized zero-width groups before value
iteration. Deterministic truncation/byte mutations sample geographic, Lambert and
bitmap specimens; they check for panics and do not prove exhaustive rejection.
Catching a panic is only a test oracle.

The test allocator records current-thread requested live bytes and their peak,
including conservative old/new overlap during reallocation. It separately records
cumulative allocation bytes; cumulative is not reported as a peak. Samples allocate
and retain an owned input inside the measurement through result byte conversion,
and check zero remaining live bytes. Cases include maximum E and D, small and
all-missing units, bitmap expansion, group tables, malformed input and four
independent simultaneous calls. Fixture generators remain outside this ownership
boundary; the actual unit input copy is included. Byte-balance underflow is
flagged, but this counter is not a pointer ownership tracker and cannot detect
every release of an allocation made before measurement. The controlled samples
therefore also require the source ownership audit: pre-existing fixture/profile
backing remains alive outside the measured operation. The malformed-input probe
separately measures additional allocations after borrowing existing bytes; it does
not claim that its caller's retained input occupies zero bytes.

These are Rust allocation and source-lifetime witnesses, not allocator fragmentation,
RSS, upstream metadata/cache, shared pool, cancellation or transport proofs.
External migration comparisons remain outside repository fixtures. Constant-profile
rejection deliberately differs from the reference decoder's wrong values/panic;
future HTTP error mapping is not established here. See D35 in DEVIATIONS.md.

Format references: [NCEP 5.0](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp5-0.shtml),
[NCEP 5.3](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp5-3.shtml),
[NCEP 7.3](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp7-3.shtml).
