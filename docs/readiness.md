# Startup and shutdown

The server starts one best-effort catalog load and waits before binding, for at
most `DYNAMICAL_API_CATALOG_WAIT_SECONDS` (default 10 seconds). A completed
failure or timeout still allows binding: health checks, documentation and
unloaded-catalog 503 responses remain available. The wait does not cancel the
shared load, start retries, or perform a data read. Zero disables the wait.
Choose a limit below the platform's startup timeout, allowing room for process
initialization and binding.

The `api_ready` JSON log records `catalogReady`, `catalogWaitOutcome`
(`ready`, `failed`, or `timeout`), `catalogWaitMs`, and `bindToCatalogReadyMs`.
The last field is zero when the catalog is already available at bind and null
otherwise. If the catalog first becomes available later, `api_catalog_ready`
records its observed bind-to-ready time in milliseconds. Observation polls every
100 ms after bind (10 ms during startup); the timestamps have that granularity.
No catalog URL, release secret or request data is included in these logs.

## Application headers and redirects

`DYNAMICAL_API_RELEASE`, when set, supplies `X-Dynamical-Release` on every
application response: successes, redirects, preflight responses, origin-auth
failures, admission/validation errors and handled/unhandled application errors.
The value must be valid HTTP header content or startup fails. When unset the
header is omitted. It is never added to `Access-Control-Expose-Headers`.
Hyper-generated parser errors, including malformed requests and oversized
headers, bypass the application and do not carry the marker. Framing errors,
body deadlines and disconnects that close the connection without an application
response also produce no marked response.

`DYNAMICAL_API_PUBLIC_SCHEME` accepts `http` (default) or `https` and sets the
scheme in trailing-slash 307 redirects. Deployment wrappers serving HTTPS should
set `https`. The existing Host-header validation and accepted-socket fallback
still determine the authority. Forwarded headers are ignored. Root redirects
continue to the fixed documentation URL; canonical data links remain relative,
and catalog links come from catalog metadata.

`DYNAMICAL_API_REQUIRE_ORIGIN_AUTH=1` refuses startup if
`DYNAMICAL_API_ORIGIN_SECRET` is missing, empty or entirely whitespace. The flag
is off by default. An explicitly empty secret remains invalid even without the
flag. A nonblank secret is preserved exactly for authentication.

## Graceful process shutdown

SIGTERM and SIGINT (Ctrl-C) stop acceptance and close the listening socket.
Existing Hyper connections disable keep-alive and finish their current request.
Idle connections close immediately. Request drain waits for both connection tasks
and zero occupied compute permits, including workers whose clients disconnected.
Permits belong to the workers until their work ends; reaching a timeout does not
release them. At the request deadline remaining HTTP futures are cancelled.
Cancellation cleanup and terminal records finish before telemetry flush; a stuck
request destructor is covered by the same total process watchdog.

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS` | `30` | Maximum request/compute drain time |
| `DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS` | `15` | Additional reserve for cleanup and runtime teardown |

These settings accept finite positive seconds, including fractions; the combined
budget must fit the process clock. Finalization must reserve at least 10 seconds
for stdout and network telemetry, or 14 seconds when `telemetry-delivery` is built
and a nonempty `SENTRY_DSN` is configured (including four seconds for SDK cleanup).
A smaller reserve fails startup with a static diagnostic. The default 15 seconds
covers both cases. The total shutdown bound is their sum,
**45 seconds by default**, starting when the signal is observed. Normal shutdown
usually finishes earlier. Request timeout is reported as an unsuccessful exit.
An independent OS thread enforces the total budget even if a handler, native
decode, synchronous cleanup or runtime teardown stalls. At expiry, Unix uses
`_exit(1)` after a best-effort static, nonblocking stderr diagnostic, without
application locks, allocation, destructors or C exit hooks.
Forced exit can lose responses and pending telemetry and can skip catalog
cleanup. OS scheduling delays are outside a userspace deadline guarantee. The
strict immediate-exit path is Unix-specific; other platforms use process exit.
The explicitly owned Tokio runtime never waits indefinitely for blocking workers.
The watchdog remains armed through runtime teardown and process exit.

The cleanup order is stop accepting → stop and join heartbeat/readiness work →
request and detached-compute drain → telemetry flush → catalog close → runtime
teardown → exit. One watchdog owns the absolute total deadline established when
the signal is observed. Stdout and network telemetry flush concurrently, using
at most their existing 10/14-second bound and only the remaining process time.
The SDK reserve is included in that time. No cleanup step restarts the process
budget or arms a second watchdog. Lifecycle logs use the bounded stdout queue,
so a full pipe cannot block the async shutdown path. On startup failure, with no
request drain, the same watchdog bounds error capture, flush and runtime teardown
at 10/14 seconds.

The deployment's function timeout and container shutdown grace must exceed the
configured total budget, with scheduling headroom. The default 45 seconds is a
server setting, not a claim about Modal's actual grace: deployment qualification
must measure that grace and tune both settings before relying on graceful drain.
The server cannot extend an earlier platform SIGKILL.

Rust subprocess tests exercise pre-bind success/failure/timeout, post-bind catalog
readiness, in-flight completion, refused connections after SIGTERM, SIGINT,
stuck handlers, detached blocking work plus stuck synchronous cleanup, stopped
heartbeat/readiness work, stdout backpressure and stalled telemetry resolution.

## Source preparation availability

`s8PreparationReady` and `s8Preparation` in the existing container heartbeat report
the process-wide source preparation owner. The nested snapshot contains only its
bounded state/stage, operation/aftermath ages, refusal/timeout counts, revision and
closed/occupied flags. Direct library callers can use `preparation_snapshot()`;
optional heartbeat delivery does not own or reset that state. Best-effort
`api_source_preparation` records report observed transitions, including recovery.

An abandoned operation remains occupied; after 90 seconds in aftermath its state
is `overdue`. `faulted` means the supervisor ended unexpectedly with upstream
quiescence unknown. Neither state starts a replacement. Other numeric/metadata
work and the existing health endpoint retain their behavior. Recovery is observed
operation completion or process replacement through the existing operational
procedure. A retry hint does not promise that a stuck operation will recover.

Shutdown stops new preparation admission and drains it alongside requests under
the existing drain deadline. The shutdown record includes `requestDrained` and
`s8Preparation`; `drained` is false while metadata remains owned, even with zero
compute slots or connection tasks. The process deadline remains the final bound.
Do not interpret a dropped task handle or zero request count as metadata drain.
Connect degradation alerts to the operational recovery procedure before enabling
source-backed products; this slice enables no catalog product.
