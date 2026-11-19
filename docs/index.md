# Jaeger SDK

[![Grow Codes](https://img.shields.io/badge/Grow%20Codes-grow.codes-2ea44f)](https://grow.codes)
[![CI](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml/badge.svg)](https://github.com/grow-codes-packages/jaeger/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](license.md)

One API for **Jaeger** in five languages: TypeScript, Rust, Go, Python and Java. Every package
has the same functions, options, defaults, error codes and behaviour, tested against a real
`jaegertracing/jaeger:2.23.0` in Docker.

| Language | Package | Coverage |
| - | - | - |
| TypeScript | [`@grow-codes/jaeger`](https://www.npmjs.com/package/@grow-codes/jaeger) | ![TypeScript coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-typescript.json) |
| Rust | [`grow-jaeger`](https://crates.io/crates/grow-jaeger) | ![Rust coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-rust.json) |
| Go | [`github.com/grow-codes-packages/jaeger/go`](https://pkg.go.dev/github.com/grow-codes-packages/jaeger/go) | ![Go coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-go.json) |
| Python | [`grow-jaeger`](https://pypi.org/project/grow-jaeger/) | ![Python coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-python.json) |
| Java | [`codes.grow.packages:jaeger`](https://central.sonatype.com/artifact/codes.grow.packages/jaeger) | ![Java coverage](https://img.shields.io/endpoint?url=https://grow-codes-packages.github.io/jaeger/badges/coverage-java.json) |

## What you get

- **Client** with health, authenticated ping and server version ([Client](client.md)).
- **Configuration** from code or `GROW_JAEGER_*` / vendor environment variables, immutable
  and validated ([Configuration](configuration.md)).
- **Retries** with full-jitter backoff and `Retry-After`, connect/request/total timeouts and
  cancellation in every language.
- **Typed errors** with one set of codes across languages ([Error handling](errors.md)).
- **Redaction everywhere**: credentials never appear in errors, logs, hooks or `toString`
  ([Redaction](redaction.md)).
- **Tracing**: OpenTelemetry setup, span helpers, propagation, framework adapters and trace
  queries ([Tracing](tracing.md)).
- **Service functions** for the Jaeger API ([Functions](functions.md)).

Start with [Install](install.md) and the [Quickstart](quickstart.md).

!!! note "Independent project"
    This SDK is made by [Grow Codes](https://grow.codes). Jaeger is a trademark of
    its owner (The Linux Foundation (Jaeger is a CNCF project)); this project is not affiliated with or endorsed by it. See
    [License & disclaimer](license.md).
