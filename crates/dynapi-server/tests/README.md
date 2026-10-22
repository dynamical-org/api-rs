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
`forecasts.rs` uses the committed synthetic Icechunk repositories from
`dynapi-dataset` through a loopback catalog and repository host. Assertions derive
from their numeric recipes, explicit validation cases and header/link invariants.
It covers nearest points, runs/windows, ensembles, value/read budgets, cache
TTL/LRU, canonical forecast/analysis reads, generic and retryable error mapping,
byte admission, representation literals and client cancellation during a read.
Transient error fixtures inject typed reader failures at the reader boundary;
a separate case exercises an actual upstream HTTP read failure. No stored
reference-service responses are used.

The `harness` feature also enables loopback store substitution for synthetic
catalog recipes and one-shot typed reader failure injection. These hooks are
omitted from ordinary builds. These tests do not qualify remaining handlers,
projected transforms, routing or real telemetry delivery.
Maintainers verify migration output against the reference Python service outside
the repository and record intentional differences in `DEVIATIONS.md`.
