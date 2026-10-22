# Server test policy

Loopback application and raw-socket tests own the supported route, middleware,
header, body, telemetry and connection behavior using explicit cases and
invariants. Packaged robots bytes/metadata and documented temporary limitations
are tested directly. No Python is needed or used by the test suite.

`server.rs` exercises parser/framing behavior, deadlines, capacity, cancellation,
middleware order and injectable telemetry. It also checks robots GET/HEAD and
full-body Range policy, the asset mtime/size metadata rule, root redirects and
health responses. UUID4 format/uniqueness and HTTP Date are invariants rather than
fixed clocks or saved values. The `harness` feature enables panic-injection cases.

The wider route/method and middleware/error matrices remain follow-up coverage.
These tests do not qualify unfinished data handlers or real telemetry delivery.
Maintainers verify migration output against the reference Python service outside
the repository and record intentional differences in `DEVIATIONS.md`.
