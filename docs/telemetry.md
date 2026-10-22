# Telemetry

The native server emits newline-delimited JSON to stdout. Handler success records
(`api_request`) use `DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE` (default `0.1`);
`api_failure` and `api_container` records are unsampled. Cheap health, static and
redirect responses have no handler success record. A cancelled request claims a
single failure record and releases its admitted-request count immediately.

## Optional delivery

Build production delivery explicitly:

```sh
cargo build --locked --release -p dynapi-server --features telemetry-delivery
```

Configure `POSTHOG_PROJECT_TOKEN` for analytics and `SENTRY_DSN` for errors in the
deployment environment. Empty or absent settings disable their sink. With both
unset, even a feature-enabled binary creates no delivery worker or HTTP client.
An ordinary build omits the feature and sends nothing to either service. Tests
inject recording sinks; delivery tests use stalled fakes or a local HTTP collector.

`POSTHOG_HOST` defaults to `https://us.i.posthog.com`. The adapter sends a JSON
capture request to `<host>/capture/`, with the project token in the JSON body.
Use HTTPS in deployments. Redirects and inherited HTTP proxies are disabled.
Malformed sink configuration disables delivery with a static diagnostic; the
diagnostic never includes a configured URL, token or DSN. Stdout remains active.

The request task attempts an immediate enqueue into a 256-entry queue, dropping
on overflow. Events larger than 64 KiB are rejected before enqueue. Counters
saturate at `u64::MAX`; network calls have a two-second timeout. Failed analytics
delivery is dropped without retry. The Sentry SDK also maintains its own bounded
30-envelope transport queue and respects server rate limits. Errors are never
sampled by the application. Shutdown uses one ten-second drain/flush budget plus
a four-second Sentry reserve for its bounded blocking enqueue and HTTP-thread
join: the nominal total bound with Sentry is fourteen seconds, subject to OS
scheduling. Without Sentry the bound is ten seconds. Application draining stops
before consuming the reserve; the remaining budget is passed to SDK close, and
cleanup is awaited rather than detached. Drain expiry drops remaining application
records and counts them; SDK close may also discard pending envelopes. Queue
overflow, transport failures or drain expiry cannot change an HTTP response.

Configured delivery adds `telemetryDropped` and `telemetryFailed` to stdout
heartbeats so application-queue losses and failed send attempts are visible.
The Sentry SDK's internal rate-limit and envelope-loss accounting is separate.

## Error and identity boundaries

Analytics emit for `/v1/` requests after origin authentication, including errors
and rate rejections. Origin-authentication 403s and CORS preflights emit no
analytics. Distinct IDs are the first 16 hex characters of HMAC-SHA256 over the
Cloudflare client address with the authenticated origin key; without that key or
address they are `anonymous`. The address and key are never included in a record.
Both `x-dynamical-origin-auth` and `x-dynamical-viewer-auth` headers are excluded
from stdout, analytics and error records.

Errors capture unhandled handler failures, validation-encoding failures,
non-admission server errors, and startup failures. Expected client errors and
task cancellation produce no error report. Admission 503 responses carry an
explicit `ServerBusy` marker and are filtered. Error tags are `client_kind`, a
keyed `caller` when available, and a validated `cf_worker`. Each event carries its
own tags, so concurrent requests cannot share scope state.

Error events use bounded static Rust categories and native capture-site stacks.
No request context, body, raw path/query, headers, error message, panic payload,
frame locals, source text or absolute source path is attached. Automatic SDK
request, panic, logging, tracing, context and profiling integrations are disabled.
Python and native SDK envelope metadata and stack frames differ deliberately;
see `D-tel-1` in [DEVIATIONS.md](../DEVIATIONS.md).

## Heartbeat

`DYNAMICAL_API_HEARTBEAT_SECONDS` defaults to 30 seconds. An unparseable value
uses the default; zero, negative and non-finite values disable it. The first
record follows one full interval. The heartbeat stops during shutdown and owns
no strong reference to the application.

Heartbeat records include admitted-request and compute-slot counts, oldest slot
age, process CPU deltas/utilization, available cgroup throttling deltas, and
`nativeThreads` on Linux. Unsupported measurements are omitted rather than
reported as zero. `nativeThreads` counts native process threads; it replaces the
reference runtime's `pythonThreads` field. Resource inputs are injectable in tests.

The load observer accumulates `opens`, `loads` and `productsLoaded`. Successful
loads progress through product, snapshot, session, variable and warm classes;
failed loads count but never warm the registry. Eight most recent successful
snapshots per product and eight sessions per snapshot are retained for coldness.
Only product identifiers and aggregate counts/milliseconds are published; snapshot
IDs, session identities, variable names and coordinates are excluded.
