# Compatibility

## Runtimes

| Language | Supported | Tested in CI |
| - | - | - |
| TypeScript | Node.js ≥ 22.12, ESM and CommonJS | Node 22, 24 |
| Rust | MSRV 1.88, edition 2021, tokio | stable, 1.88 |
| Go | Go ≥ 1.26 | 1.26, 1.27 |
| Python | ≥ 3.10 | 3.10, 3.13, 3.14 |
| Java | ≥ 17 (Java 17 bytecode) | Temurin 17, 21, 25 |

## Jaeger versions

The integration suite runs against `jaegertracing/jaeger:2.23.0` (the Grow Codes stack catalog
tag), started exactly as Grow stacks start it: the all-in-one with in-memory storage and no
authentication, plus a second instance with a file-based strategies config for remote sampling.

The SDK uses Jaeger's **v3 HTTP query API** (`/api/v3/*`). Jaeger 2.23.0 has removed the legacy
v1 search routes (`/api/services`, `/api/services/{s}/operations`, `/api/traces?service=`), so
servers older than the v3 API are not supported for search, services and operations (`UNSUPPORTED`).
`getTrace` falls back to the legacy `/api/traces/{id}` when `/api/v3/traces/{id}` is missing, and
`getDependencies` always uses the legacy `/api/dependencies` (there is no v3 HTTP route).

## Jaeger behaviour worth knowing

| Behaviour | Effect on the SDK |
| - | - |
| v3 search needs both `startTimeMin` and `startTimeMax` | the SDK always sends both (default: the last hour) |
| "no match" is `404` on `/api/v3/traces` but `200 {"summaries":[]}` on `/api/v3/trace-summaries` | both map to `[]` |
| v3 error bodies are JSON sent as `text/plain`; an unknown route is a plain-text `404 page not found` | JSON envelope 404 = "not found"; plain-text 404 = route missing |
| `spanKind` in `/api/v3/operations` is case-sensitive (`SERVER` returns nothing) | always sent lower-case |
| an all-zero trace id is accepted and then hides older traces from search in the memory store | the SDK and tests never export zero ids (OpenTelemetry SDKs do not create them) |
| the all-in-one traces its own query API as service `jaeger` | `listServices()` is never empty; tests use unique service names |
| the memory store ignores the `getTrace` time window | the window is only a storage hint |
| the version is only in the UI page (`JAEGER_VERSION`) and in `target_info` on `:8888/metrics` | `serverVersion()` reads both; there is no `/api/version` |
| dependencies are computed on the fly by the memory store only | Cassandra/Elasticsearch/OpenSearch need the `spark-dependencies` job; `[]` is normal |

## Functions covered by unit tests only

Every public function is called against the real service in the Docker suite. These paths cannot
be produced by a stock Jaeger container and are covered by unit tests (mock HTTP servers) in all
five languages:

| Function / path | Reason |
| - | - |
| `AUTHENTICATION` / `PERMISSION_DENIED` (401/403) | stock Jaeger has no authentication (the `token` is for an auth proxy) |
| `RATE_LIMITED` / `Retry-After` (429, 503) | Jaeger does not rate-limit the query API |
| `UNSUPPORTED` on 501 | Jaeger never answers 501 |
| `getTrace` legacy fallback (`/api/traces/{id}`) | 2.23.0 always has the v3 route; a live test would need a proxy that hides `/api/v3/*` |
| `UNSUPPORTED` for a missing v3 search/services/operations route | same reason |
| live strategy reload in a running sampler | the test containers do not mount the strategies volume (reload itself is verified by the prep fixture) |
| rate-limiter refill timing | needs an injectable clock |
| OTLP exporter TLS (CA, client certificate) | the test Jaeger runs without TLS |

The gRPC query port (16685), the gRPC sampling port (5779) and adaptive sampling are not used by
the SDK.

## Skipped conformance scenarios

| Scenario | Reason |
| - | - |
| — | none: `core.health`, `tracing.services` and `tracing.tree` pass in all five languages |

## Language differences

The API is the same in every language; only the conventions differ (naming, durations, async
model). Deliberate differences:

| Topic | Difference |
| - | - |
| gRPC exporter with `insecureSkipVerify` | **Python and Rust** raise `ConfigError`: grpcio and tonic cannot skip certificate verification. TypeScript, Go and Java support it (tests only). Use a CA bundle instead. |
| Tracing handle type | Go names it `*jaeger.TracingSetup` (Go style forbids `jaeger.JaegerTracingHandle`); it embeds the category `*TracingHandle`. |
| Operations with a span-kind filter | Go has no overloading: `ListOperations(ctx, service)`, `ListOperationsWith(ctx, service, opts)` (names) and `GetOperations(ctx, service, opts)` (name + kind). |
| `getTrace` options | Go and Rust use `GetTraceWith` / `get_trace_with`; Rust and Go dependencies with `endTs`/`lookback` use `GetDependenciesAt` / `get_dependencies_at`. |
| Closed client | TypeScript and Python raise `ConfigError`, Rust, Go and Java `CancelledError`. |
| Rust blocking client | `blocking::JaegerClient` mirrors the client and query functions; tracing setup and remote samplers need a tokio runtime and are only on the async client. |
| Python async | `AsyncJaegerClient` / `AsyncJaegerQueryClient` have the same methods; `init_tracing` and `remote_sampler` are synchronous (the sampler polls in a thread). |
