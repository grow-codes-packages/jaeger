# Remote sampling

`JaegerRemoteSampler(samplingUrl, service, pollInterval)` is an OpenTelemetry head sampler driven
by Jaeger's remote sampling endpoint (`GET {samplingUrl}?service=<name>`, port 5778, default
`http://127.0.0.1:5778/sampling`). It polls every `pollInterval` (default 1 minute) and applies
the strategy the collector serves (from a strategies file, or adaptive sampling).

**Decision rule (the same in every language):**

1. A per-operation strategy (`operationSampling.perOperationStrategies`) wins for a listed
   operation (the span name).
2. Every other operation uses the rate limiter whenever `rateLimitingSampling` is present
   (token bucket with a burst of `max(1, rate)` that starts full, so a rate of 0 lets the first
   trace through and then none, as in Jaeger), else `probabilisticSampling.samplingRate`, else
   `operationSampling.defaultSamplingProbability`.
3. Until the first strategy arrives, `initialSamplingRate` applies (default 0.001).
4. A failed or malformed poll keeps the last good strategy.

Probabilistic decisions are deterministic per trace id. Use the sampler as the root of a
parent-based sampler (what `initTracing` does unless told otherwise).

=== "TypeScript"

    ```ts
    import { JaegerRemoteSampler, initTracing } from "@grow-codes/jaeger";

    const sampler = new JaegerRemoteSampler({
      serviceName: "checkout",
      samplingUrl: "http://jaeger:5778/sampling",
      pollIntervalMs: 60_000,
    });
    await sampler.ready();
    const tracing = initTracing({ serviceName: "checkout", remoteSampler: sampler });
    // ... later: await tracing.shutdown(); await sampler.close();
    ```

=== "Rust"

    ```rust
    let sampler = grow_jaeger::JaegerRemoteSampler::new(
        "http://jaeger:5778/sampling",
        "checkout",
        Duration::from_secs(60),
    )?;
    sampler.ready().await?;
    let tracing = grow_jaeger::init_tracing_with_remote_sampler(TracingConfig::new("checkout"), sampler.clone())?;
    ```

=== "Go"

    ```go
    sampler, err := jaeger.NewRemoteSampler("http://jaeger:5778/sampling", "checkout", time.Minute)
    if err != nil {
        return err
    }
    defer sampler.Close()
    h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "checkout"}, jaeger.WithSampler(sampler))
    ```

=== "Python"

    ```python
    from grow_jaeger import JaegerRemoteSampler, init_tracing

    sampler = JaegerRemoteSampler("http://jaeger:5778/sampling", "checkout", 60)
    tracing = init_tracing(service_name="checkout", remote_sampler=sampler)
    ```

=== "Java"

    ```java
    JaegerRemoteSampler sampler =
        JaegerRemoteSampler.create("http://jaeger:5778/sampling", "checkout", Duration.ofMinutes(1));
    JaegerTracingHandle tracing = JaegerTracing.initTracing(
        TracingConfig.builder("checkout").build(),
        JaegerTracing.Options.builder().remoteSampler(sampler, true));
    ```

Other members: `refresh()` (fetch now; throws on failure, the previous strategy stays),
`strategy` (the strategy in use, `null` before the first poll), `close()` (idempotent; also
`shutdown()` in TypeScript), and the client shortcuts [`samplingStrategy(service)`](client.md#samplingstrategy)
and [`remoteSampler(service, pollInterval)`](client.md#remotesampler).

## Strategy model

`parseSamplingStrategy(body)` turns a `/sampling` response into the normalized strategy: type
(`probabilistic` / `ratelimiting`), `samplingRate`, `maxTracesPerSecond`,
`defaultSamplingProbability` and the per-operation rates. Missing numbers count as 0 (proto3
JSON); a rate outside 0..1 or a negative limit raises `ValidationError`.

=== "TypeScript"

    ```ts
    parseSamplingStrategy({ probabilisticSampling: { samplingRate: 0.5 } }).samplingRate; // 0.5
    ```

=== "Rust"

    ```rust
    grow_jaeger::parse_sampling_strategy(br#"{"probabilisticSampling":{"samplingRate":0.5}}"#)?;
    ```

=== "Go"

    ```go
    s, err := jaeger.ParseSamplingStrategy([]byte(`{"probabilisticSampling":{"samplingRate":0.5}}`))
    ```

=== "Python"

    ```python
    parse_sampling_strategy({"probabilisticSampling": {"samplingRate": 0.5}}).sampling_rate
    ```

=== "Java"

    ```java
    JaegerSamplingStrategy.parse("{\"probabilisticSampling\":{\"samplingRate\":0.5}}");
    ```

## From the environment

`OTEL_TRACES_SAMPLER=jaeger_remote` (root sampler) or `parentbased_jaeger_remote` makes
`initTracing` start a remote sampler for the tracing service name and close it on `shutdown()`.
`OTEL_TRACES_SAMPLER_ARG` takes `endpoint=<url>,pollingIntervalMs=<ms>,initialSamplingRate=<0..1>`.
Without `endpoint`, the URL, token and TLS settings come from the sampling config: the client's
config for `client.initTracing`, an explicit sampler config (TypeScript `samplerConfig`, Rust
`sampler_config`, Go `WithSamplerConfig`, Python `sampler_config`, Java `samplerConfig`), else
`GROW_JAEGER_SAMPLING_URL` / `JAEGER_SAMPLING_ENDPOINT` from the same environment. An explicit
sampler in code wins over the environment.

```sh
export OTEL_TRACES_SAMPLER=parentbased_jaeger_remote
export OTEL_TRACES_SAMPLER_ARG=endpoint=http://jaeger:5778/sampling,pollingIntervalMs=30000
```
