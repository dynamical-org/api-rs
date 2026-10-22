# Contract fixture policy

Request bodies, typed formatting inputs and expected examples currently live
inline in `../contract.rs`. They exercise the Rust-owned request/output contract
with explicit values, independently constructed large integers and parser safety
invariants. The large-integer recipe is signed powers of ten at 4,299, 4,300,
4,301 and 5,000 digits; the expected text is a sign, one and the declared zeros.
Depth and surrogate cases own exact rejection envelopes; datetime cases own
offset/microsecond spelling. Ordinary tests need and use no Python.

These focused regressions do not provide exhaustive numeric, Unicode, schema or
encoding coverage. Broader independently specified tables/properties are tracked
as follow-up test work. Maintainers verify output against the reference Python
service during migration, outside this repository. Intentional differences are
recorded in the workspace's `DEVIATIONS.md`.
