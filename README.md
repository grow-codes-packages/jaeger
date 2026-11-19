# Grow Codes SDK for Jaeger

[![CI](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml/badge.svg)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/grow-codes-packages/jaeger?sort=semver)](https://github.com/grow-codes-packages/jaeger/releases)
[![Docs](https://img.shields.io/badge/docs-grow--codes--packages.github.io-blue)](https://grow-codes-packages.github.io/jaeger/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Grow Codes](https://img.shields.io/badge/Grow%20Codes-grow.codes-2ea44f)](https://grow.codes)

| Language | Package | Coverage |
| - | - | - |
| TypeScript | [![npm](https://img.shields.io/npm/v/@grow-codes/jaeger)](https://www.npmjs.com/package/@grow-codes/jaeger) | [![TypeScript coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-typescript.json)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml) |
| Rust | [![crates.io](https://img.shields.io/crates/v/grow-jaeger)](https://crates.io/crates/grow-jaeger) [![docs.rs](https://img.shields.io/docsrs/grow-jaeger)](https://docs.rs/grow-jaeger) | [![Rust coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-rust.json)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml) |
| Go | [![Go Reference](https://pkg.go.dev/badge/github.com/grow-codes-packages/jaeger/go.svg)](https://pkg.go.dev/github.com/grow-codes-packages/jaeger/go) | [![Go coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-go.json)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml) |
| Python | [![PyPI](https://img.shields.io/pypi/v/grow-jaeger)](https://pypi.org/project/grow-jaeger/) | [![Python coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-python.json)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml) |
| Java | [![Maven Central](https://img.shields.io/maven-central/v/codes.grow.packages/jaeger)](https://central.sonatype.com/artifact/codes.grow.packages/jaeger) | [![Java coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-java.json)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml) |

Open-source client libraries for [Jaeger](#trademarks-and-disclaimer) in **TypeScript, Rust, Go,
Python and Java** — one API, five idiomatic packages, tested against a real Jaeger in Docker.

- **Typed client** with timeouts, retries with jitter, `Retry-After`, cancellation and one error
  model (`JaegerError` with canonical codes) in every language.
- **Secrets stay secret**: credentials are redacted in `toString`, logs, hooks and error messages.
- **Tracing**: OpenTelemetry setup in one call with the Jaeger OTLP preset (HTTP/protobuf on
  4318 or gRPC on 4317), sampling presets, span helpers, context propagation and framework
  middleware.
- **Query**: read traces back through Jaeger's v3 query API — `getTrace`, `findTraces`,
  `searchTraces`, services, operations and dependencies, in one normalized trace model.
- **Remote sampling**: an OpenTelemetry sampler driven by Jaeger's sampling endpoint (port 5778),
  also enabled by `OTEL_TRACES_SAMPLER=jaeger_remote`.

## Contents

- [Supported service version](#supported-service-version)
- [Install](#install)
- [Quickstart](#quickstart)
- [Configuration](#configuration)
- [Function overview](#function-overview)
- [Testing with Docker](#testing-with-docker)
- [Compatibility](#compatibility)
- [Contributing](#contributing)
- [Security](#security)
- [Licence](#licence)
- [Trademarks and disclaimer](#trademarks-and-disclaimer)
- [About Grow Codes](#about-grow-codes)

## Supported service version

Tested in CI against **`jaegertracing/jaeger:2.23.0`** (the version pinned in the Grow Codes stack catalog). Other
versions with the same API usually work; see [Compatibility](#compatibility) and
[docs/compatibility.md](docs/compatibility.md).

## Install

| Language | Package | Install |
| - | - | - |
| TypeScript (Node.js ≥ 22.12) | `@grow-codes/jaeger` | `npm install @grow-codes/jaeger` |
| Rust (MSRV 1.88) | `grow-jaeger` | `cargo add grow-jaeger` |
| Go (1.26+) | `github.com/grow-codes-packages/jaeger/go` | `go get github.com/grow-codes-packages/jaeger/go` |
| Python (≥ 3.10) | `grow-jaeger` | `pip install grow-jaeger` |
| Java (17+) | `codes.grow.packages:jaeger` | see below |

Maven:

```xml
<dependency>
  <groupId>codes.grow.packages</groupId>
  <artifactId>jaeger</artifactId>
  <version>0.1.0</version>
</dependency>
```

Gradle (Kotlin DSL):

```kotlin
implementation("codes.grow.packages:jaeger:0.1.0")
```

## Quickstart

Every package reads its configuration from the environment (`GROW_JAEGER_URL`,
`GROW_JAEGER_OTLP_HTTP_ENDPOINT`, …, see [Configuration](#configuration)) or from an explicit
config object. Each example starts tracing, sends one span, reads it back through the query API
and checks the service health.

```sh
export GROW_JAEGER_URL=http://127.0.0.1:16686            # query API
export GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318
```

### TypeScript

```ts
import { JaegerClient, currentTraceId, initTracing, withSpan } from "@grow-codes/jaeger";

const tracing = initTracing({ serviceName: "checkout" });
const client = JaegerClient.fromEnv();
try {
  const traceId = await withSpan("place-order", async () => currentTraceId());
  await tracing.forceFlush();
  const trace = await client.query.waitForTrace(traceId!, { timeoutMs: 15_000 });
  console.log(trace.spans.map((s) => s.name), (await client.health()).healthy);
} finally {
  await tracing.shutdown();
  await client.close();
}
```

### Rust

```rust
use grow_jaeger::tracing::{TracingConfig, WaitForTraceOptions};
use grow_jaeger::JaegerClient;
use opentelemetry::trace::{Span as _, Tracer as _};

#[tokio::main]
async fn main() -> Result<(), grow_jaeger::Error> {
    let tracing = grow_jaeger::init_tracing(TracingConfig::new("checkout"))?;
    let mut span = tracing.tracer("checkout", None).start("place-order");
    let trace_id = span.span_context().trace_id().to_string();
    span.end();
    tracing.force_flush(None).await?;

    let client = JaegerClient::from_env()?;
    let trace = client.query().wait_for_trace(&trace_id, WaitForTraceOptions::default()).await?;
    println!("{} spans, healthy: {}", trace.spans.len(), client.health().await?.healthy);
    tracing.shutdown().await?;
    client.close().await;
    Ok(())
}
```

### Go

```go
package main

import (
	"context"
	"fmt"
	"log"

	jaeger "github.com/grow-codes-packages/jaeger/go"
)

func main() {
	ctx := context.Background()
	tracing, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "checkout"})
	if err != nil {
		log.Fatal(err)
	}
	defer tracing.Shutdown(ctx)
	_, span := tracing.Tracer("checkout").Start(ctx, "place-order")
	span.End()
	_ = tracing.ForceFlush(ctx)

	client, err := jaeger.NewClientFromEnv()
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()
	tr, err := client.Query().WaitForTrace(ctx, span.SpanContext().TraceID().String(), jaeger.WaitForTraceOptions{})
	if err != nil {
		log.Fatal(err)
	}
	health, _ := client.Health(ctx)
	fmt.Println(len(tr.Spans), "spans, healthy:", health.Healthy)
}
```

### Python

```python
from grow_jaeger import JaegerClient, current_trace_id, init_tracing, with_span

tracing = init_tracing(service_name="checkout")
with with_span("place-order"):
    trace_id = current_trace_id()
tracing.force_flush()

with JaegerClient.from_env() as client:
    trace = client.query.wait_for_trace(trace_id, timeout_s=15)
    print([s.name for s in trace.spans], client.health().healthy)
tracing.shutdown()
```

Async code uses `AsyncJaegerClient` with the same methods (`async with`, `await client.health()`).

### Java

```java
import codes.grow.packages.jaeger.JaegerClient;
import codes.grow.packages.jaeger.JaegerTracing;
import codes.grow.packages.jaeger.JaegerTracingHandle;
import codes.grow.packages.jaeger.tracing.Spans;
import codes.grow.packages.jaeger.tracing.Traced;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.WaitForTraceOptions;

JaegerTracingHandle tracing = JaegerTracing.initTracing(TracingConfig.builder("checkout").build());
String traceId = Traced.run("place-order", () -> Spans.currentTraceId().orElseThrow());
tracing.forceFlush();
try (JaegerClient client = JaegerClient.fromEnv()) {
    Trace trace = client.query().waitForTrace(traceId, WaitForTraceOptions.defaults());
    System.out.println(trace.spans().size() + " spans, healthy: " + client.health().healthy());
}
tracing.shutdown();
```

More complete programs live in [examples/](examples/) — one folder per language.

## Configuration

All settings can be passed in code or through environment variables. Precedence: **explicit value >
`GROW_JAEGER_*` variable > vendor variable > default.** A present but invalid value raises a
`ConfigError` that names the variable (never its value).

**Client** (`JaegerConfig`, used by the client, the query client and remote samplers):

| Setting | Environment variables (precedence order) | Default |
| - | - | - |
| `url` (query API, port 16686) | `GROW_JAEGER_URL`, `JAEGER_QUERY_URL` | `http://127.0.0.1:16686` |
| `token` (bearer token for an auth proxy; never sent by `health()`) | `GROW_JAEGER_TOKEN` | none |
| `samplingUrl` (remote sampling, port 5778) | `GROW_JAEGER_SAMPLING_URL`, `JAEGER_SAMPLING_ENDPOINT` | `http://127.0.0.1:5778/sampling` |
| `healthUrl` (healthcheckv2 `/status`, port 13133) | `GROW_JAEGER_HEALTH_URL` | none |
| `metricsUrl` (Prometheus `/metrics`, port 8888; version fallback) | `GROW_JAEGER_METRICS_URL` | none |
| `connectTimeout` | `GROW_JAEGER_CONNECT_TIMEOUT` | 10 s |
| `requestTimeout` (per attempt) | `GROW_JAEGER_REQUEST_TIMEOUT` | 30 s |
| `totalTimeout` (including retries) | `GROW_JAEGER_TOTAL_TIMEOUT` | 120 s |
| `retry.maxRetries` | `GROW_JAEGER_MAX_RETRIES` | 3 |
| `tls.caCertPath` | `GROW_JAEGER_CA_CERT` | system roots |
| `tls.clientCertPath` / `tls.clientKeyPath` | `GROW_JAEGER_CLIENT_CERT` / `GROW_JAEGER_CLIENT_KEY` | none |
| `tls.insecureSkipVerify` (tests only) | `GROW_JAEGER_INSECURE_SKIP_VERIFY` | `false` |
| `proxy` | `GROW_JAEGER_PROXY` | none (environment proxies are ignored) |
| `headers`, `userAgentSuffix`, `hooks`, `logger` | — | none |

**Tracing** (`initTracing`; per field: explicit > `GROW_JAEGER_OTLP_*` >
`OTEL_EXPORTER_OTLP_TRACES_*` > `OTEL_EXPORTER_OTLP_*` > default):

| Setting | Environment variables (precedence order) | Default |
| - | - | - |
| `protocol` | `GROW_JAEGER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` |
| `endpoint` | `GROW_JAEGER_OTLP_GRPC_ENDPOINT` / `GROW_JAEGER_OTLP_HTTP_ENDPOINT` (by protocol), `GROW_JAEGER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_ENDPOINT` (HTTP appends `/v1/traces`) | `http://127.0.0.1:4318/v1/traces` (gRPC `http://127.0.0.1:4317`) |
| `headers` | `GROW_JAEGER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, `OTEL_EXPORTER_OTLP_HEADERS` | none |
| `compression` | `GROW_JAEGER_OTLP_COMPRESSION`, `OTEL_EXPORTER_OTLP_TRACES_COMPRESSION`, `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` |
| `batch.exportTimeout` | `GROW_JAEGER_OTLP_TIMEOUT`, `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT`, `OTEL_EXPORTER_OTLP_TIMEOUT` | 30 s |
| `tls.caCertPath` | `GROW_JAEGER_OTLP_CERTIFICATE`, `GROW_JAEGER_OTLP_CA_CERT`, `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CERTIFICATE` | system roots |
| `tls.clientCertPath` | `GROW_JAEGER_OTLP_CLIENT_CERTIFICATE`, `OTEL_EXPORTER_OTLP_TRACES_CLIENT_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE` | none |
| `tls.clientKeyPath` | `GROW_JAEGER_OTLP_CLIENT_KEY`, `OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY`, `OTEL_EXPORTER_OTLP_CLIENT_KEY` | none |
| `serviceName` | `GROW_JAEGER_SERVICE_NAME`, `OTEL_SERVICE_NAME`, `service.name` in `OTEL_RESOURCE_ATTRIBUTES` | required |
| `resourceAttributes` | `OTEL_RESOURCE_ATTRIBUTES` (merged; explicit keys win) | none |
| `sampler`, `samplerRatio` | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` (`jaeger_remote`, `parentbased_jaeger_remote` start the remote sampler) | `parentbased_always_on`, 1.0 |
| `propagators` | `OTEL_PROPAGATORS` | `tracecontext,baggage` |

Durations accept milliseconds (`1500`) or units (`500ms`, `10s`, `1m30s`); booleans accept
`true/false/1/0/yes/no/on/off`. Field names follow each language's convention
(`request_timeout` in Python and Rust, `RequestTimeout` in Go). The full reference is in the
[documentation](https://grow-codes-packages.github.io/jaeger/).

## Function overview

<!-- service:function-overview — the service fills this section; keep the heading. -->

The same functions exist in every language, named per language convention (`getTrace` in
TypeScript and Java, `get_trace` in Python and Rust, `GetTrace` in Go). Every function has an
example in all five languages in the [documentation](https://grow-codes-packages.github.io/jaeger/functions/).

### Client — `JaegerClient`

| Function | Description |
| - | - |
| `health()` | liveness of the query service (`GET /`, no token); healthcheckv2 detail with `healthUrl` |
| `ping()` | authenticated round trip (`GET /api/v3/services`), returns the latency |
| `serverVersion()` / `buildInfo()` | `2.23.0` from the UI page, else `target_info` at `metricsUrl`; `UNSUPPORTED` when neither has one |
| `samplingStrategy(service)` | the remote sampling strategy of a service, fetched once |
| `initTracing(config)` / `remoteSampler(service, interval)` | tracing and samplers that use the client's settings; shut down by `close()` |
| `query` | the `JaegerQueryClient` (shares the client's connections) |
| `close()` | idempotent; flushes and stops everything created through the client |

| TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - |
| `await client.serverVersion()` | `client.server_version().await?` | `client.ServerVersion(ctx)` | `client.server_version()` | `client.serverVersion()` |

### Query — `JaegerQueryClient` (`TraceQueryClient`)

| Function | Description |
| - | - |
| `getTrace(traceId)` / `requireTrace(traceId)` | one trace (`/api/v3/traces/{id}`, legacy fallback); `null` / `NOT_FOUND` when missing |
| `findTraces(query)` | full traces by service, operation, tags, duration, time window, limit |
| `searchTraces(query)` | trace summaries (`/api/v3/trace-summaries`) |
| `listServices()` | services with stored spans |
| `listOperations(service, {spanKind})` / `getOperations(…)` | operation names / names with span kind |
| `getDependencies(range)` | service dependency edges |
| `waitForTrace(traceId, opts)` | poll until a trace is queryable (tests) |

| TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - |
| `await client.query.findTraces({ service: "checkout" })` | `client.query().find_traces(&TraceSearchQuery::service("checkout")).await?` | `client.Query().FindTraces(ctx, jaeger.TraceSearchQuery{Service: "checkout"})` | `client.query.find_traces(service="checkout")` | `client.query().findTraces(TraceSearchQuery.forService("checkout"))` |

### Tracing setup

| Function | Description |
| - | - |
| `initTracing(config)` | OpenTelemetry setup with the Jaeger OTLP preset; returns a handle with `tracer`, `forceFlush`, `shutdown` |
| `tracingConfigFromEnv(config)` / `resolveEndpoint(protocol, endpoint)` | the effective config / exporter URL |
| `SamplingPreset` | `development`, `production`, `followParent`, `off`, `ratio(r)` |
| `withSpan`, `traced`, `currentSpan`, `currentTraceId`, `addEvent`, `recordException`, `setError`, `inject`, `extract`, `traceFields` | shared span helpers and propagation |

| TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - |
| `initTracing({ serviceName: "checkout", ...SamplingPreset.production })` | `init_tracing(SamplingPreset::PRODUCTION.apply(TracingConfig::new("checkout")))?` | `jaeger.InitTracing(ctx, jaeger.SamplingProduction.Apply(jaeger.TracingConfig{ServiceName: "checkout"}))` | `init_tracing(service_name="checkout", **SAMPLING_PRODUCTION.as_kwargs())` | `JaegerTracing.initTracing(SamplingPreset.production().applyTo(TracingConfig.builder("checkout")).build())` |

### Remote sampling

| Function | Description |
| - | - |
| `JaegerRemoteSampler(samplingUrl, service, pollInterval)` | OpenTelemetry sampler: per-operation strategies win, else the rate limiter, else the probability; keeps the last good strategy |
| `parseSamplingStrategy(body)` | parse a `/sampling` response |

| TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - |
| `new JaegerRemoteSampler({ serviceName: "checkout", samplingUrl })` | `JaegerRemoteSampler::new(url, "checkout", Duration::from_secs(60))?` | `jaeger.NewRemoteSampler(url, "checkout", time.Minute)` | `JaegerRemoteSampler(url, "checkout", 60)` | `JaegerRemoteSampler.create(url, "checkout", Duration.ofMinutes(1))` |

<!-- /service:function-overview -->

## Testing with Docker

The integration suite runs every public function against a real Jaeger (`jaegertracing/jaeger:2.23.0`) in Docker.
Each language package is packed and installed into a separate consumer project first, so the tests
use the package exactly as you would.

```sh
make test-docker                     # all five languages
make test-docker LANGS="go python"   # a subset
```

No host ports are published and every run uses its own Compose project, so runs never collide; the
stack is always removed afterwards (`docker compose down -v`). Unit tests need no Docker:
`make test` or `make coverage`.

## Compatibility

| | TypeScript | Rust | Go | Python | Java |
| - | - | - | - | - | - |
| Runtime | Node.js ≥ 22.12, ESM + CJS | stable, MSRV 1.88 | 1.26, 1.27 | 3.10 – 3.14 | 17, 21, 25 |
| Async | `Promise` + `AbortSignal` | `async` (tokio), `blocking` feature | `context.Context` | sync + `asyncio` | sync + `CompletableFuture` |
| Jaeger | `jaegertracing/jaeger:2.23.0` | `jaegertracing/jaeger:2.23.0` | `jaegertracing/jaeger:2.23.0` | `jaegertracing/jaeger:2.23.0` | `jaegertracing/jaeger:2.23.0` |

Functions that Jaeger (or its emulator) cannot serve are listed with the reason in
[docs/compatibility.md](docs/compatibility.md).

## Contributing

Contributions are welcome — please read [CONTRIBUTING.md](CONTRIBUTING.md) and
[AGENTS.md](AGENTS.md) first. All five languages keep the same API; a change lands in all of them
together. Parts of this repository are generated by
[sdk-kit](https://github.com/grow-codes-packages/sdk-kit) and changed there.

## Security

Please report vulnerabilities privately to [opensource@grow.codes](mailto:opensource@grow.codes).
See [SECURITY.md](SECURITY.md).

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE) for attributions.

## Trademarks and disclaimer

This is an independent, community-maintained project. It is **not affiliated with, endorsed or
sponsored by The Linux Foundation (Jaeger is a CNCF project)**. Jaeger is a trademark of The Linux Foundation (Jaeger is a CNCF project); it is used here only to name the
service this SDK connects to. The Jaeger server is licensed separately by its owner
(Apache-2.0).

The software is provided "AS IS", without warranties or conditions of any kind. Grow Codes is not
responsible for any damage, data loss, security incident, outage or cost that results from using it.
See [LICENSE](LICENSE) and [NOTICE](NOTICE).

## About Grow Codes

This SDK is built and maintained by [Grow Codes](https://grow.codes). Grow Codes publishes the
client libraries it builds as open source in the
[grow-codes-packages](https://github.com/grow-codes-packages) organisation, generated from one
shared kit so that every service and every language behaves the same way. Learn more at
[grow.codes](https://grow.codes).
