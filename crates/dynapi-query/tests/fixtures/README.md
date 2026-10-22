# Query fixture policy

The small dataset, collection, request and failure inputs are constructed in
`../query.rs`. They drive Rust-owned selection, grouping, response and ordering
regressions without a store or network. A single spatial point and initialization
have explicitly declared lead times and values: 10 K at lead zero, 20 K at lead
one. The 2016 epoch is computed from 46 ordinary years plus eleven leap days.

Expected groups distinguish separately opened views. Explicit values and a
hand-written canonical URI check contract-to-query execution without narrowing
wide civil dates or large lead limits. Constructed i64 extrema and powers of ten
establish arithmetic failure order; spy readers and budgets check zero work on
rejection. Expected responses are not derived with the encoder under test.

This focused target does not replace the full spatial, temporal, budget, grouping,
response and header coverage planned for follow-up work. Tests need and use no
Python. Verification against the reference Python service happens outside the
repository during migration; intentional differences belong in `DEVIATIONS.md`.
