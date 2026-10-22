# Catalog input fixtures

`stac/` contains 75 public STAC source documents (two catalog roots and 73
collection documents), plus 32 synthetic valid, malformed and security inputs.
These are input documents, not transformed collections or reference-service
answers. The public documents were obtained from the production/staging STAC
endpoints; the synthetic documents were authored as format/security examples.
The public dynamical-catalog 1.0.1 package generated the source catalog format.

`stac/manifest.json` contains the offline URL-to-file map and SHA-256 digests of
the uncompressed document bytes. Synthetic entries contain only filenames and
input digests. Gzip is a storage encoding; the loader reads every document
locally. Tests never fetch those public URLs. Malformed JSON and non-object
documents intentionally remain malformed inputs.

`parsing.rs` owns selected field/default/order, numeric, type, parser-bound and
host/target rules with explicit Rust literals and error classes. Reader-mapping
tests own S3/HTTP descriptors, declared-container order/deduplication and scalar
region spelling. Numeric token assertions run both catalog-alone and with
workspace feature unification, including the required `1e-07` spelling.

The native graph test derives membership from the raw source document IDs and
checks required variable/dimension relationships after offline refresh. It does
not compare a saved transformed collection. Small fake graphs and loopback tests
own skip/abort, retry/deadline, refresh, single-flight and URL/no-GET behavior;
cache tests use explicit values, fake-clock arithmetic and identity/concurrency
invariants. Internationalized/ACE hosts and unrepresentable targets fail closed
as documented in workspace deviation D11.

Fixtures include obviously fake credential material for rejection tests. No
credential is used to fetch or decode data. Preserve the input bytes and their
declared construction/URL meaning when changing tests; do not derive expected
answers by serializing the implementation under test. No Python is needed or
used by the test suite. Maintainers verify migration behavior against the
reference Python service outside the repository. The broader parsing, numeric,
URL and graph policy matrix remains follow-up coverage in the tracking issue.
