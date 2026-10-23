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
Repeated fields, trailing messages, GRIB1 and image templates are rejected.

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

Packing 5.42 admits only options 14, block size 32 and reference sample interval
128. Integer widths 2–16 use the pinned decoder's exact f32 integer intermediate
before f64 GRIB scaling. Width 1 remains unsupported by the pinned decoder.
Code-ID widths follow its table: 2–3 bits use one ID bit, 4 uses two,
5–8 use three and 9–16 use four. Wider samples remain unqualified: the pinned
three-byte storage configuration disagrees with its output size, and its four-byte conversion
interprets bytes as f32 rather than integer samples. Multi-byte CCSDS samples on
big-endian hosts are unsupported. A final reference interval may have fewer than
128 blocks, and the last block may have 1–31 output samples.

The CCSDS preflight walks code IDs, reference samples, Rice unary/remainder fields,
second-extension pairs and zero runs without reconstructing or allocating a field.
It checks exact output counts and byte padding, word ranges and RSI boundaries,
including the end-of-sequence meaning of the remainder-of-segment zero code.
Partial final blocks take the pinned incremental output branches: uncompressed
copy emits r samples; split reads all 32-reference unary codes but emits only
r-reference remainders; second extension can stop between a pair's values; zero
output stops at the attested count. The qualified encoding retains that block's
complete encoded fields. Preflight checks the unused suffix with slow reads:
at most (32-r) uncompressed sample fields, the remaining split remainders, or
the remaining bounded second-extension unary fields. Zero output has no such
suffix. At most seven zero padding bits follow the complete final block;
arbitrary trailing bytes or omitted encoded suffix fields are rejected.
It also follows the pinned decoder's fast/incremental branch choices and byte
read-ahead. Short seven-byte fast unary tails, insufficient fast remainder margins
and second-extension table indices above 90 are explicitly unsupported, even when
the abstract code stream could be valid. A header-only bound does not qualify
these data-dependent paths. Uncompressed, Rice and second-extension values fit
the declared width;
unsigned postprocessing therefore stays inside that width and nonnegative i32.

Zero-bit 5.42 fields are qualified only for a positive-zero reference, no bitmap
and an empty payload, after the existing finite GRIB factor checks. The pinned
empty-vector/zero-resize behavior is correct for precisely that zero profile.
Nonzero or negative-zero references and masked zero-bit fields remain typed
constant rejections. This does not add a general constant decoder or change the
5.0/5.3 constant policy.

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
used to repair inconsistent counts: preflight establishes exact counts first,
except for the separately qualified true-zero 5.42 profile described above. No
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
bitmap specimens, as well as CCSDS whole-message and mixed-block payload specimens.
The CCSDS payload mutations also compare admitted values with a test-only format
oracle using logical bits, triangular pairs and unsigned inverse differencing,
independent of the pinned buffering and fast paths. Exact RSI-crossing, padding
and short SE-output fixtures defend the implementation-specific guards. These
checks do not prove exhaustive rejection.
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
future HTTP error mapping is not established here. See D35 and D36 in
DEVIATIONS.md. D35 describes the earlier 5.0/5.3 slice; the separate CCSDS
qualification here and in D36 extends that private library profile.

Format references: [NCEP 5.0](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp5-0.shtml),
[NCEP 5.3](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp5-3.shtml),
[NCEP 7.3](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp7-3.shtml),
[NCEP 5.42](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_temp5-42.shtml),
[CCSDS 121.0-B-3](https://ccsds.org/Pubs/121x0b3.pdf).

## CCSDS allocation phases

For 5.42, F funds the retained complete input and compressed copies, including
the packed BitVec/Vec and the VecDeque input queue. They overlap during state
construction: at most three E-sized backings plus the fixed state. Sections,
including optional local-use padding, borrow the original input; the 25-byte
packing template is copied. The bit-vector conversion transfers its backing.
F = 4E + 2 MiB covers the compressed backings and metadata. The 16 KiB RSI
scratch remains an X term even during state construction.

During nonzero-width decoding the packed Vec has been released. The queue and caller input
coexist with geometric output-byte capacity, RSI scratch and then a f32 vector.
With P packed samples, P <= D/8, output bytes <=2P, byte capacity <4P and f32
storage 4P. Before f64 collection the queue, byte vector and RSI buffer are
released. Conservatively including old/new f64 reallocation overlap and the
still-owned f32 input gives <3.5D plus bounded metadata. Bitmap expansion and
final byte conversion are separate later phases with at most 3D backing. X =
8D + 64 KiB covers these decoded buffers; compressed copies remain an F term.
A peak minus only the retained input is therefore not an X-only bound for CCSDS.
Tiny zero-bit units have bounded minimum Vec capacities covered by the fixed
64 KiB term rather than these proportional nonzero-width bounds.
The controlled peak tests also assert the source-derived combined phase bound
`peak - retained_E <= 2E + 3.5D + 64 KiB`, independently of the looser F+X target.

Native debug witnesses retain the complete input through reorder, f64 scale and
LE conversion. At D = 58,720,256, the 16-bit uncompressed stream measured
144,818,408 peak bytes; a 9-bit half-bitmap stream measured 122,544,327; the
qualified zero-bit field measured 117,440,711. A deliberately inefficient valid
Rice stream with E = 33,554,432 and D = 1,048,576 measured 100,665,537 peak bytes
against F = 136,314,880 and X = 8,454,144. The last case makes compressed copies
visible independently of a large D allowance. Preflight requests zero heap bytes.
A combined E = 33,554,432 and D = 58,720,256 Rice witness measured
163,577,905 peak bytes against F+X = 606,142,464, and satisfies the source-derived
phase bound above.
These phase bounds and controlled requested-allocation witnesses retain the
allocator ownership, process-memory and integration limitations stated above.
