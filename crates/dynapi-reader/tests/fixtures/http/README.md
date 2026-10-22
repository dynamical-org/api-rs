# Synthetic HTTP Icechunk fixture

This immutable synthetic Icechunk repository and its encoded virtual objects are
served by a loopback HTTP test server. Rust-owned tests assert typed values and
exact bits from the declared values/formulas, plus errors, access restrictions
and resource bounds. No Python is needed or used by the test suite.

Input bytes were generated locally on 2026-10-21 with CPython 3.14.6,
icechunk 2.2.2, zarr 3.3.0, numpy 2.5.1
and numcodecs 0.16.5. Only fixture bytes and this description are committed.
All payloads were generated locally; coordinate metadata mirrors the public
WeatherNext layout. No forecast payloads are included.
`manifest.json` records the final snapshot ID and the four plain values.

Generation recipe:

1. Create a local Icechunk repository with
   `RepositoryConfig(inline_chunk_threshold_bytes=0)` and two named HTTP
   `VirtualChunkContainer`s (`allowed`, `denied`), using `http_store()` and
   placeholder prefixes `http://127.0.0.1:1/{name}/`.
2. Create a root Zarr group. Create `stored`, `allowed`, `denied`, `boundary`
   arrays with shape/chunks `(4,)`, float32, little-endian `BytesCodec`, no
   compressor, NaN fill, dimension `sample` and `units: K`.
3. Write `[1.25, -0.0, -3.5, 42.0]` to `stored`. For the other arrays call
   `session.store.set_virtual_ref("{name}/c/0", location, offset=7, length=16)`.
   Locations are `vcc://allowed/values.bin`, `vcc://denied/values.bin` and
   `https://example.invalid/chunks-escape/values.bin` (the last with
   `validate_container=False`). `values.bin` contains seven padding bytes,
   the same four little-endian floats, and seven trailer bytes.
4. Create `lz4_values`, float32 shape/chunks `(1024,)`, with little-endian
   bytes and `BloscCodec(typesize=4, cname="lz4", clevel=5, shuffle="shuffle",
   blocksize=0)`, NaN fill, dimension `large_sample` and `units: m s-1`. Encode `arange(1024, dtype="<f4")`
   with matching `numcodecs.Blosc`, assert a compressed LZ4 frame, and put it
   between the same padding/trailer in `lz4.bin`. Set a virtual reference
   to `vcc://allowed/lz4.bin`, offset 7, length 1146.
5. Create `y` = `linspace(-90, 90, 721)` and `x` = `arange(1440)/4` as float64
   arrays, with full-axis chunks, little-endian bytes and Blosc Zstd level 3,
   shuffle, blocksize 0. Use the exact geographic coordinate metadata in
   `../../../src/coordinate_profiles.json` (without shape),
   adding each array's shape, `zarr_format: 3`, `node_type: array` and
   `storage_transformers: []`.
6. Commit the sessions and record the last snapshot ID in `manifest.json`.
   No Icechunk credentials or external data are used during generation.

Tests serve the repository bytes over an ephemeral loopback HTTP server with
HEAD and byte-range GET support. The request explicitly remaps named `allowed`
references to that server. Tests cover stored HTTP payloads, nonzero virtual
byte ranges, omitted/undeclared containers, an adjacent-prefix escape, metadata
validation, and 403/404/429/503 storage errors. The server binds only 127.0.0.1
and shuts down after each test. External networking is not required.

The fixture now belongs to the Rust integration test `tests/http.rs`. All 47
acceptance cases run with plain `cargo test`, using Tokio's loopback HTTP server;
no Python interpreter, subprocess reader binary, or external service is needed.
Metadata rejection tests copy the repository to a temporary directory and modify
and commit metadata through the Rust Icechunk API.

Expected values are authored in Rust from this recipe: `x[i] = i / 4`,
`y[i] = -90 + i / 4`, and `lz4_values[i] = i`. These integer and quarter-step
values are exactly representable. Tests compare full coordinate and LZ4 buffers,
shape/dtype, selected order and duplicates, and the float32 bits of
`[1.25, -0.0, -3.5, 42.0]`. The expected bytes do not come from the served input
objects or a decoder's saved answers. The seven-byte virtual offset, exact range
and padded decode charge are asserted separately. Generation tooling is not a
test dependency; maintainers verify reference-service behavior outside the repo.
