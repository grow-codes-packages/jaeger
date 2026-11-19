---
name: jaeger-sdk
description: Add and use the Grow Codes SDK for Jaeger (`jaeger`) in TypeScript, Rust, Go, Python or Java projects. Use when code must send traces to Jaeger (OpenTelemetry OTLP preset), read traces back through the Jaeger query API, use Jaeger remote sampling, or talk to Jaeger otherwise - installing @grow-codes/jaeger, the grow-jaeger crate or PyPI package, the Go module github.com/grow-codes-packages/jaeger/go or Maven codes.grow.packages:jaeger, configuring it with GROW_JAEGER_* variables, handling JaegerError codes, testing with the bundled fakes, or running the Docker integration suite of this repository.
---

# Jaeger SDK (Grow Codes)

One API for Jaeger in five languages (categories: tracing). Pick the package for the
project's language, configure it from the environment, and handle errors by their canonical code.

## 1. Add the package

| Language | Command | Import |
| - | - | - |
| TypeScript | `npm install @grow-codes/jaeger` | `import { JaegerClient } from "@grow-codes/jaeger"` |
| Rust | `cargo add grow-jaeger` (add `tokio` with `macros,rt-multi-thread`) | `use grow_jaeger::JaegerClient;` |
| Go | `go get github.com/grow-codes-packages/jaeger/go` | `jaeger "github.com/grow-codes-packages/jaeger/go"` |
| Python | `pip install grow-jaeger` (or `uv add grow-jaeger`) | `from grow_jaeger import JaegerClient` |
| Java | Maven `codes.grow.packages:jaeger` (Gradle `implementation("codes.grow.packages:jaeger:<version>")`) | `import codes.grow.packages.jaeger.JaegerClient;` |

Check the latest version in the README badges before pinning one.

## 2. Configure

Prefer environment configuration so no credential is written into code:

```sh
export GROW_JAEGER_URL=http://jaeger:16686                 # query API (UI port)
export GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://jaeger:4318   # span export (or OTEL_EXPORTER_OTLP_ENDPOINT)
export GROW_JAEGER_SERVICE_NAME=checkout                   # or OTEL_SERVICE_NAME
export GROW_JAEGER_SAMPLING_URL=http://jaeger:5778/sampling  # only for remote sampling
export GROW_JAEGER_TOKEN=...   # only behind an auth proxy; from your secret store, never committed
```

Precedence: explicit config value > `GROW_JAEGER_*` > vendor variables (`JAEGER_QUERY_URL`,
`JAEGER_SAMPLING_ENDPOINT`) > default. The exporter reads explicit > `GROW_JAEGER_OTLP_*` >
`OTEL_EXPORTER_OTLP_TRACES_*` > `OTEL_EXPORTER_OTLP_*` > default (`http/protobuf` to
`http://127.0.0.1:4318/v1/traces`; `grpc` uses 4317). Optional: `GROW_JAEGER_HEALTH_URL`
(healthcheckv2, 13133) and `GROW_JAEGER_METRICS_URL` (8888, version fallback).
Other variables: `…_CONNECT_TIMEOUT`, `…_REQUEST_TIMEOUT`, `…_TOTAL_TIMEOUT` (`10s`, `1500`),
`…_MAX_RETRIES`, `…_CA_CERT`, `…_CLIENT_CERT`, `…_CLIENT_KEY`, `…_PROXY`. Environment proxies
(`HTTPS_PROXY`) are ignored unless `proxy` is set.

## 3. Use

Create one client per process and close it on shutdown (TypeScript `await client.close()`, Rust
`client.close().await`, Go `defer client.Close()`, Python `with …`/`close()`, Java
try-with-resources).

- `health()` never throws for an unhealthy service; check `healthy`.
- `ping()` verifies credentials (`AUTHENTICATION` error when they are wrong).
- All I/O is retried with jitter on 429/502/503/504 and connection errors; only idempotent calls
  are retried on 500.
- Errors carry `code` (`CONFIG`, `AUTHENTICATION`, `PERMISSION_DENIED`, `NOT_FOUND`, `CONFLICT`,
  `RATE_LIMITED`, `VALIDATION`, `UNAVAILABLE`, `TIMEOUT`, `UNSUPPORTED`, `CANCELLED`), `retryable`,
  `httpStatus` and `requestId`. Branch on the code, not on the message text. Go:
  `errors.Is(err, jaeger.ErrNotFound)`.

### Tracing

Call `initTracing({ serviceName })` once at startup, wrap work in `withSpan(name, fn)` or the
`traced` helper, propagate context with `inject`/`extract`, and call `shutdown()` on exit. Use the
trace query client to read traces back (`getTrace`, `searchTraces`, `waitForTrace`).

<!-- service:skill-usage — the service adds its own functions and examples here. -->

### Jaeger specifics

| Task | TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - | - |
| Start tracing | `initTracing({ serviceName })` | `init_tracing(TracingConfig::new(name))?` | `jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: name})` | `init_tracing(service_name=name)` | `JaegerTracing.initTracing(TracingConfig.builder(name).build())` |
| Sampling preset | `...SamplingPreset.production` | `SamplingPreset::PRODUCTION.apply(cfg)` | `jaeger.SamplingProduction.Apply(cfg)` | `**SAMPLING_PRODUCTION.as_kwargs()` | `SamplingPreset.production().applyTo(builder)` |
| Read a trace | `client.query.getTrace(id)` | `client.query().get_trace(id).await?` | `client.Query().GetTrace(ctx, id)` | `client.query.get_trace(id)` | `client.query().getTrace(id)` |
| Search | `client.query.findTraces({ service })` / `searchTraces` | `find_traces(&q)` / `search_traces` | `FindTraces` / `SearchTraces` | `find_traces(service=…)` / `search_traces` | `findTraces` / `searchTraces` |
| Wait in a test | `client.query.waitForTrace(id, { timeoutMs })` | `wait_for_trace(id, opts)` | `WaitForTrace(ctx, id, opts)` | `wait_for_trace(id, timeout_s=…)` | `waitForTrace(id, opts)` |
| Remote sampling | `initTracing({ serviceName, remoteSampler: true })` | `init_tracing_with_sampler(cfg, sampler)` | `jaeger.InitTracing(ctx, cfg, jaeger.WithSampler(s))` | `init_tracing(service_name=…, remote_sampler=True)` | `client.initTracing(cfg, Options.builder().remoteSampling(RemoteSampling.defaults()))` |

Rules that save debugging time:

- Call `forceFlush()` (or `shutdown()`) before reading a trace back; spans are batched.
- Searches always send a time window (default: the last hour); widen `start`/`end` for older
  traces. `searchTraces` returns summaries, `findTraces` full traces; no match is `[]`.
- Span-kind filters are sent lower-case; `listOperations` returns names, `getOperations`
  `{name, spanKind}`.
- `OTEL_TRACES_SAMPLER=parentbased_jaeger_remote` turns on remote sampling without code changes
  (URL from `OTEL_TRACES_SAMPLER_ARG=endpoint=…` or `GROW_JAEGER_SAMPLING_URL`).
- Use unique service names in tests: the all-in-one reports itself as `jaeger`, and never export
  all-zero trace ids (they break search in the memory store).
- `serverVersion()` returns `2.23.0` (no `v`); `UNSUPPORTED` when the UI is disabled and no
  `metricsUrl` is set.

## 4. Test code that uses the SDK

Use the in-memory fakes from the testing module instead of a real Jaeger in unit tests:
TypeScript `@grow-codes/jaeger/testing`, Rust feature `testing`, Go package `testkit`, Python
`grow_jaeger.testing`, Java `codes.grow.packages.jaeger.testing`.

## 5. Work on this repository

Read `AGENTS.md` first. Short version: the five languages keep the same API; files with the
`Generated by sdk-kit` header are changed in sdk-kit, not here; `make lint coverage` must pass
(≥ 90 % lines and branches); `make test-docker LANGS="<lang>"` runs the integration suite against
`jaegertracing/jaeger:2.23.0`.
