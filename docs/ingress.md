# Origin routing and ingress contract

The [route manifest](route-contract.v1.json) describes this server's origin
operations and protocol behavior. It is generated from the same `ROUTES`
inventory used by dispatch, with generic `data`, `catalog`, `documentation`,
`health`, `redirect` and `static` intent annotations. It contains no deployment
admission decisions or policy settings.

## Reachability ownership

A **public operation** is an explicitly registered method, or an explicit plain
OPTIONS responder, in the origin manifest. An **origin response** is behavior
such as a method error, slash redirect or global middleware preflight. These
terms describe origin intent and test scope. They do not assert that a deployed
request reaches the origin.

Deployment routing determines external reachability. Its owner must compare the
candidate server's manifest with the deployed route/method and header handling,
including URL normalization, redirects and middleware responses. The public
repository checks its origin contract; deployment-specific validation remains
with the deployment owner. Origin tests alone do not qualify that configuration.
The coverage matrix uses this distinction when describing method tables.

A successful preflight does not declare a method on an endpoint. Unsupported
methods and slash alternatives can have origin responses even when they are not
public operations. OpenAPI has its own documented scope and excludes some
infrastructure, documentation and OPTIONS operations; it is not a substitute for
the complete origin inventory.

## Manifest behavior and maintenance

`schemaVersion: 1` and `scope: origin` identify the format. Each route records its
path template, operation intent and registered methods. `head.registered` states
whether HEAD is registered; every HEAD response suppresses body bytes, including
errors. `plainOptionsStatus` distinguishes the explicit 204 responder from a 405
method error. The latter's `Allow` header names the registered methods; GET does
not implicitly add HEAD. These router outcomes occur after authentication and
other earlier middleware checks.

`directRedirectStatus` describes a redirect operation when present.
`slashAlternativeRedirect` describes a different path form, with its common
profile in `pathBehavior.slashRedirect`: remove trailing slashes, redirect with
307 before method checking when the alternate path matches, and preserve the
query. The configured public scheme and validated Host or accepted socket
address determine that redirect's origin. The root has no slash alternative.
See [application redirects](readiness.md#application-headers-and-redirects).

Paths are percent-decoded once with lossy UTF-8 before dispatch. Parameters match
one nonempty decoded segment; static paths tolerate one final LF. Unmatched
router paths give 404. Deployment normalization is a separate input and must be
qualified by its owner.

Preflight is separate from plain OPTIONS: an OPTIONS request containing both
Origin and Access-Control-Request-Method reaches global CORS middleware before
origin authentication and routing, including on unknown paths. Its accepted and
rejected response statuses are recorded separately. Ordinary OPTIONS has no
such exemption. The manifest records this origin behavior without turning it
into a wildcard public operation.

Regenerate from the repository root:

```sh
cargo run --locked -p dynapi-server --features harness --example route-contract > docs/route-contract.v1.json
```

The exporter and its metadata API require `harness`; ordinary runtime builds do
not include them. CI compares the committed bytes with a fresh derivation from
`ROUTES`. HTTP tests independently consume the committed manifest and exercise
real handlers, method errors, HEAD, plain OPTIONS, redirects, decoded paths and
preflight precedence using fictional fixtures. Changes to the router require
reviewing and regenerating this contract and updating affected tests. The
manifest is not a saved service response or product-data qualification.

## Trusted request headers

Origin authentication and viewer attestation have separate roles, as specified
in [D37](../DEVIATIONS.md#d37--viewer-trust-and-gate-placement).

- `X-Dynamical-Origin-Auth` authenticates trusted ingress when
  `DYNAMICAL_API_ORIGIN_SECRET` is configured. Ingress must overwrite client input
  with the configured credential. Public deployments can require it at startup
  with `DYNAMICAL_API_REQUIRE_ORIGIN_AUTH=1`; the credential should be nonblank,
  ASCII and valid HTTP header content. Missing or mismatched ASCII authentication
  returns 403 before routing. The preflight exception above remains explicit.
- `X-Dynamical-Viewer-Auth` is a separate attestation. Both authentication headers
  must match the configured origin secret before viewer location is trusted.
  Origin authentication alone does not attest location.
- `X-Dynamical-Viewer-Country` and `X-Dynamical-Viewer-Region-Code` must be
  overwritten with trusted observations. Unattested, absent or empty location
  fields are treated as unknown. Client-supplied location fields must not survive
  as trusted values, including through case variants or duplicate headers.
- `CF-Connecting-IP` and optional `CF-Worker` are caller metadata, trusted only
  after origin authentication. Worker metadata distinguishes caller keys; it is
  not authentication. Without origin authentication, socket identity is used.
  Missing or empty trusted client IP leaves the caller unidentified for fair
  share, so deployment validation must cover how normal callers receive that
  metadata. These platform fields have a different role from viewer attestation.

Ingress must preserve ordinary request fields needed by the API, including
Origin, access-control request fields, content type, ranges and conditionals,
and the response metadata used by clients. Its owner validates overwrite,
attestation and forwarding behavior with the deployed configuration. Credential
values, provisioning, policy contents and deployment identifiers are outside this
public contract. The service's independent location-check credential is not an
inbound viewer attestation and must not be supplied by clients.
