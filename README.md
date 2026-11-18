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
- **Tracing**: OpenTelemetry setup in one call, span helpers, context propagation, framework
  middleware and a trace query client.

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
| Rust (MSRV 1.85) | `grow-jaeger` | `cargo add grow-jaeger` |
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
`GROW_JAEGER_TOKEN`, …, see [Configuration](#configuration)) or from an explicit config
object. The examples check the service health and make one authenticated call.

### TypeScript

```ts
import { JaegerClient } from "@grow-codes/jaeger";

const client = JaegerClient.fromEnv();
try {
  const health = await client.health();
  console.log(health.healthy, health.status);
  console.log("latency ms:", await client.ping());
} finally {
  await client.close();
}
```

### Rust

```rust
use grow_jaeger::JaegerClient;

#[tokio::main]
async fn main() -> Result<(), grow_jaeger::Error> {
    let client = JaegerClient::from_env()?;
    let health = client.health().await?;
    println!("{} {}", health.healthy, health.status);
    println!("latency: {:?}", client.ping().await?);
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
	client, err := jaeger.NewClientFromEnv()
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	health, err := client.Health(ctx)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(health.Healthy, health.Status)
}
```

### Python

```python
from grow_jaeger import JaegerClient

with JaegerClient.from_env() as client:
    health = client.health()
    print(health.healthy, health.status)
    print("latency ms:", client.ping())
```

Async code uses `AsyncJaegerClient` with the same methods (`async with`, `await client.health()`).

### Java

```java
import codes.grow.packages.jaeger.JaegerClient;

try (JaegerClient client = JaegerClient.fromEnv()) {
    var health = client.health();
    System.out.println(health.healthy() + " " + health.status());
    System.out.println("latency ms: " + client.ping());
}
```

More complete programs live in [examples/](examples/) — one folder per language.

## Configuration

All settings can be passed in code or through environment variables. Precedence: **explicit value >
`GROW_JAEGER_*` variable > vendor variable > default.** A present but invalid value raises a
`ConfigError` that names the variable (never its value).

| Setting | Environment variable | Default |
| - | - | - |
| `url` | `GROW_JAEGER_URL` | service default |
| `token` | `GROW_JAEGER_TOKEN` | none |
| `connectTimeout` | `GROW_JAEGER_CONNECT_TIMEOUT` | 10 s |
| `requestTimeout` (per attempt) | `GROW_JAEGER_REQUEST_TIMEOUT` | 30 s |
| `totalTimeout` (including retries) | `GROW_JAEGER_TOTAL_TIMEOUT` | 120 s |
| `retry.maxRetries` | `GROW_JAEGER_MAX_RETRIES` | 3 |
| `tls.caCertPath` | `GROW_JAEGER_CA_CERT` | system roots |
| `tls.clientCertPath` / `tls.clientKeyPath` | `GROW_JAEGER_CLIENT_CERT` / `GROW_JAEGER_CLIENT_KEY` | none |
| `tls.insecureSkipVerify` (tests only) | `GROW_JAEGER_INSECURE_SKIP_VERIFY` | `false` |
| `proxy` | `GROW_JAEGER_PROXY` | none (environment proxies are ignored) |
| `headers`, `userAgentSuffix`, `hooks`, `logger` | — | none |

Vendor variables that are also read: `OTEL_EXPORTER_OTLP_*`, `JAEGER_QUERY_URL`, `GROW_JAEGER_*`.

Durations accept milliseconds (`1500`) or units (`500ms`, `10s`, `1m30s`); booleans accept
`true/false/1/0/yes/no/on/off`. Field names follow each language's convention
(`request_timeout` in Python and Rust, `RequestTimeout` in Go). The full reference is in the
[documentation](https://grow-codes-packages.github.io/jaeger/).

## Function overview

<!-- service:function-overview — the service fills this section; keep the heading. -->

The same functions exist in every language, named per language convention (`getString` in
TypeScript and Java, `get_string` in Python and Rust, `GetString` in Go).

| Area | Functions | Description |
| - | - | - |
| Client | `health`, `ping`, `serverVersion`, `close` | liveness, authenticated round trip, server version, shutdown |
| Tracing | `initTracing`, `withSpan`, `traced`, `inject`, `extract`, `getTrace`, `searchTraces`, `listServices`, `listOperations`, `getDependencies`, `waitForTrace` | OpenTelemetry setup and trace queries |

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
| Runtime | Node.js ≥ 22.12, ESM + CJS | stable, MSRV 1.85 | 1.26, 1.27 | 3.10 – 3.14 | 17, 21, 25 |
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
