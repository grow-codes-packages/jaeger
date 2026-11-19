# Tracing setup

`initTracing(config)` sets up the official OpenTelemetry SDK of each language to export to Jaeger
over OTLP and returns a `JaegerTracingHandle` (Go: `*TracingSetup`) with `tracerProvider`,
`tracer(name, version)`, `forceFlush()`, `shutdown()` (idempotent), the exporter `endpoint` and
the `remoteSampler` in use. Jaeger ingests OTLP natively, so the official OTLP exporters are used
unchanged; the package adds the Jaeger defaults, the environment precedence, sampling presets and
remote sampling.

| | Default |
| - | - |
| Protocol | `http/protobuf` (works through HTTP proxies); `grpc` selectable |
| Endpoint | `http://127.0.0.1:4318/v1/traces` (HTTP) or `http://127.0.0.1:4317` (gRPC); an HTTP endpoint without a path gets `/v1/traces` |
| Sampler | `parentbased_always_on` |
| Propagators | W3C `tracecontext` + `baggage` |
| Compression | `gzip` |
| `setGlobal` | `true` |

=== "TypeScript"

    ```ts
    import { initTracing, withSpan } from "@grow-codes/jaeger";

    const tracing = initTracing({ serviceName: "checkout", serviceVersion: "1.4.0" });
    await withSpan("charge", async () => {
      // ...
    });
    await tracing.shutdown();
    ```

=== "Rust"

    ```rust
    use grow_jaeger::tracing::TracingConfig;

    let tracing = grow_jaeger::init_tracing(TracingConfig::new("checkout").with_service_version("1.4.0"))?;
    // ... spans via tracing.tracer("checkout", None) or the `tracing` crate layer
    tracing.shutdown().await?;
    ```

=== "Go"

    ```go
    h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "checkout", ServiceVersion: "1.4.0"})
    if err != nil {
        return err
    }
    defer h.Shutdown(context.Background())
    err = jaeger.Traced(ctx, "charge", func(ctx context.Context) error { return nil })
    ```

=== "Python"

    ```python
    from grow_jaeger import init_tracing, with_span

    tracing = init_tracing(service_name="checkout", service_version="1.4.0")
    with with_span("charge"):
        ...
    tracing.shutdown()
    ```

=== "Java"

    ```java
    JaegerTracingHandle tracing = JaegerTracing.initTracing(
        TracingConfig.builder("checkout").serviceVersion("1.4.0").build());
    Traced.run("charge", () -> { /* ... */ });
    tracing.shutdown();
    ```

## Environment

The environment fills every field the explicit config leaves unset (pass `env: false` /
`ignoreEnv()` / `WithEnv(map)` to change that). Exporter precedence per field: **explicit >
`GROW_JAEGER_OTLP_*` > `OTEL_EXPORTER_OTLP_TRACES_*` > `OTEL_EXPORTER_OTLP_*` > default**. A
present but invalid value raises `ConfigError` naming the variable, never its value.

| Field | Variables |
| - | - |
| protocol | `GROW_JAEGER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, `OTEL_EXPORTER_OTLP_PROTOCOL` (`grpc`, `http/protobuf`) |
| endpoint | `GROW_JAEGER_OTLP_GRPC_ENDPOINT` or `GROW_JAEGER_OTLP_HTTP_ENDPOINT` (by protocol), `GROW_JAEGER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (used as is), `OTEL_EXPORTER_OTLP_ENDPOINT` (a base; HTTP appends `/v1/traces`) |
| headers | `…_HEADERS` (`k=v,k2=v2`, percent-decoded) |
| compression | `…_COMPRESSION` (`gzip`, `none`) |
| export timeout | `…_TIMEOUT` (milliseconds or `10s`) |
| TLS CA | `GROW_JAEGER_OTLP_CERTIFICATE`, `GROW_JAEGER_OTLP_CA_CERT`, `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CERTIFICATE` |
| TLS client cert / key | `…_CLIENT_CERTIFICATE`, `…_CLIENT_KEY` (same chain) |
| service name | `GROW_JAEGER_SERVICE_NAME`, `OTEL_SERVICE_NAME`, else `service.name` in `OTEL_RESOURCE_ATTRIBUTES` |
| resource attributes | `OTEL_RESOURCE_ATTRIBUTES`, merged with the explicit attributes (explicit wins per key) |
| sampler | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG`; `jaeger_remote` / `parentbased_jaeger_remote` start a [remote sampler](sampling.md#from-the-environment) |
| propagators | `OTEL_PROPAGATORS` (`none` entries are ignored) |

`tracingConfigFromEnv(config)` returns the effective config without starting anything;
`resolveEndpoint(protocol, endpoint)` returns the exporter URL for a protocol.

=== "TypeScript"

    ```ts
    import { resolveJaegerEndpoint, tracingConfigFromEnv } from "@grow-codes/jaeger";

    const effective = tracingConfigFromEnv({ serviceName: "checkout" });
    resolveJaegerEndpoint("http/protobuf", "http://jaeger:4318"); // "http://jaeger:4318/v1/traces"
    ```

=== "Rust"

    ```rust
    use grow_jaeger::core::EnvReader;
    use grow_jaeger::tracing::{Protocol, TracingConfig};

    let effective = grow_jaeger::tracing_config_from_env(TracingConfig::new("checkout"), &EnvReader::new())?;
    grow_jaeger::resolve_endpoint(Protocol::HttpProtobuf, Some("http://jaeger:4318"));
    ```

=== "Go"

    ```go
    effective, err := jaeger.TracingConfigFromEnv(jaeger.TracingConfig{ServiceName: "checkout"}, nil)
    jaeger.ResolveEndpoint(jaeger.ProtocolHTTPProtobuf, "http://jaeger:4318")
    ```

=== "Python"

    ```python
    from grow_jaeger import resolve_jaeger_endpoint, tracing_config_from_env

    effective = tracing_config_from_env(service_name="checkout")
    resolve_jaeger_endpoint("http/protobuf", "http://jaeger:4318")
    ```

=== "Java"

    ```java
    TracingConfig effective = JaegerTracing.configFromEnv("checkout");
    JaegerTracing.resolveEndpoint(Protocol.HTTP_PROTOBUF, "http://jaeger:4318");
    ```

## Sampling presets

| Preset | Sampler | Ratio |
| - | - | - |
| `development` | `always_on` | 1 |
| `production` | `parentbased_traceidratio` | 0.1 |
| `followParent` | `parentbased_always_on` (the OpenTelemetry default) | 1 |
| `off` | `always_off` | 1 |
| `ratio(r)` | `parentbased_traceidratio` r; `r <= 0` → `parentbased_always_off`, `r >= 1` → `followParent` | r |

=== "TypeScript"

    ```ts
    import { SamplingPreset, initTracing } from "@grow-codes/jaeger";

    initTracing({ serviceName: "checkout", ...SamplingPreset.production });
    initTracing({ serviceName: "batch", ...SamplingPreset.ratio(0.25) });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::{tracing::TracingConfig, SamplingPreset};

    let config = SamplingPreset::PRODUCTION.apply(TracingConfig::new("checkout"));
    let batch = SamplingPreset::ratio(0.25).apply(TracingConfig::new("batch"));
    ```

=== "Go"

    ```go
    cfg := jaeger.SamplingProduction.Apply(jaeger.TracingConfig{ServiceName: "checkout"})
    batch := jaeger.SamplingRatio(0.25).Apply(jaeger.TracingConfig{ServiceName: "batch"})
    ```

=== "Python"

    ```python
    from grow_jaeger import SAMPLING_PRODUCTION, init_tracing, sampling_ratio

    init_tracing(service_name="checkout", **SAMPLING_PRODUCTION.as_kwargs())
    init_tracing(service_name="batch", **sampling_ratio(0.25).as_kwargs())
    ```

=== "Java"

    ```java
    TracingConfig config =
        SamplingPreset.production().applyTo(TracingConfig.builder("checkout")).build();
    TracingConfig batch = SamplingPreset.ratio(0.25).applyTo(TracingConfig.builder("batch")).build();
    ```

## Exporter factory

The exporter factory used by `initTracing` is public, so it can be combined with the category
`initTracingWith(config, factory)` or a custom provider: TypeScript `jaegerExporterFactory`, Rust
`jaeger_exporter`, Go `NewExporter`, Python `jaeger_exporter_factory`, Java
`JaegerTracing.exporterFactory()`. `protocol: "native"` raises `ConfigError` (Jaeger ingests OTLP).
gRPC with `insecureSkipVerify` raises `ConfigError` in Python and Rust (grpcio and tonic cannot
skip certificate verification); see [Compatibility](../compatibility.md).
