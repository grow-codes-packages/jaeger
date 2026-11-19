# Functions

The Jaeger API of this SDK. Every function exists in all five languages with the same
behaviour, named per language convention (`getTrace` in TypeScript and Java, `get_trace` in
Python and Rust, `GetTrace` in Go). Each module page shows every function with an example per
language.

| Module | Page | What it does |
| - | - | - |
| Client | [Client functions](functions/client.md) | health, ping, version, build info, sampling strategy, close; entry point to the other modules |
| Query | [Trace queries](functions/query.md) | `JaegerQueryClient`: read traces, search, services, operations, dependencies (Jaeger v3 HTTP API) |
| Tracing setup | [Tracing setup](functions/tracing-setup.md) | `initTracing` with the Jaeger OTLP preset, environment precedence, sampling presets |
| Remote sampling | [Remote sampling](functions/sampling.md) | `JaegerRemoteSampler` driven by Jaeger's sampling endpoint (port 5778) |

The shared tracing helpers (`withSpan`, `traced`, `inject`/`extract`, `traceFields`, framework
adapters) are described in [Tracing](tracing.md); the shared client surface in [Client](client.md).

## Overview

| Function | Description | Integration test |
| - | - | - |
| `health()` | liveness of the query service (`GET /`, without the token); healthcheckv2 detail with `healthUrl` | `core.health` + client suite |
| `ping()` | authenticated round trip (`GET /api/v3/services`) | `core.health` + client suite |
| `serverVersion()` | `2.23.0` from the UI page, else `target_info` at `metricsUrl` | client suite |
| `buildInfo()` | `gitVersion`, `gitCommit`, `buildDate` from the UI page | client suite |
| `samplingStrategy(service)` | the remote sampling strategy of a service, fetched once | sampling suite |
| `initTracing(config)` (client) | tracing whose remote sampler uses the client's config; closed by `close()` | client / tracing suite |
| `remoteSampler(service, pollInterval)` | a `JaegerRemoteSampler` with the client's settings; closed by `close()` | sampling suite |
| `close()` | idempotent; shuts down handles and samplers created through the client | client suite |
| `getTrace(traceId)` | one trace (v3, legacy fallback), `null` when missing | `tracing.tree` + query suite |
| `requireTrace(traceId)` | like `getTrace`, `NotFoundError` when missing | query suite |
| `findTraces(query)` | full traces matching a filter | query suite |
| `searchTraces(query)` | trace summaries matching a filter | query suite |
| `listServices()` | services with stored spans (sorted) | `tracing.services` + query suite |
| `listOperations(service, {spanKind})` | operation names (sorted, de-duplicated) | query suite |
| `getOperations(service, {spanKind})` | operations with their span kind | query suite |
| `getDependencies(range)` | service dependency edges | query suite |
| `waitForTrace(traceId, opts)` | poll until a trace has `minSpans` spans | every tracing test |
| `initTracing(config)` | OpenTelemetry setup with the Jaeger OTLP preset | `tracing.tree` + tracing suite |
| `tracingConfigFromEnv(config)` | the effective tracing config from the environment | tracing suite |
| `resolveEndpoint(protocol, endpoint)` | the exporter URL for a protocol | tracing suite |
| `SamplingPreset` | `development`, `production`, `followParent`, `off`, `ratio(r)` | tracing suite |
| `JaegerRemoteSampler` | OpenTelemetry sampler driven by Jaeger remote sampling | sampling suite |
| `parseSamplingStrategy(body)` | parse a `/sampling` response | sampling suite |

## Service configuration

Precedence for every setting: **explicit value > `GROW_JAEGER_*` > vendor variable > default**.

| Setting | Default | Environment variables (precedence order) |
| - | - | - |
| `url` (query API, port 16686) | `http://127.0.0.1:16686` | `GROW_JAEGER_URL`, `JAEGER_QUERY_URL` |
| `token` (bearer token for an auth proxy) | none | `GROW_JAEGER_TOKEN` |
| `samplingUrl` (remote sampling, port 5778) | `http://127.0.0.1:5778/sampling` | `GROW_JAEGER_SAMPLING_URL`, `JAEGER_SAMPLING_ENDPOINT` |
| `healthUrl` (healthcheckv2, port 13133) | none | `GROW_JAEGER_HEALTH_URL` |
| `metricsUrl` (Prometheus, port 8888) | none | `GROW_JAEGER_METRICS_URL` |

The common transport settings (`GROW_JAEGER_CONNECT_TIMEOUT`, `…_REQUEST_TIMEOUT`,
`…_TOTAL_TIMEOUT`, `…_MAX_RETRIES`, `…_CA_CERT`, `…_CLIENT_CERT`, `…_CLIENT_KEY`,
`…_INSECURE_SKIP_VERIFY`, `…_PROXY`) are described in [Configuration](configuration.md). The
span exporter has its own variables, listed in [Tracing setup](functions/tracing-setup.md#environment).

The token is sent as `Authorization: Bearer <token>` on every request except `health()`, which is
an unauthenticated liveness check. Stock Jaeger has no authentication; the token is for a reverse
proxy in front of it.
