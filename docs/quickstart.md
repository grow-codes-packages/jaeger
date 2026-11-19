# Quickstart

Connect to Jaeger, check that it is healthy, make an authenticated call and close the client.
The client reads `GROW_JAEGER_URL` and `GROW_JAEGER_TOKEN` (and the vendor variables
listed in [Configuration](configuration.md)), or takes the values in code.

```sh
export GROW_JAEGER_URL=http://127.0.0.1:16686
export GROW_JAEGER_TOKEN=...   # never commit real tokens
```

=== "TypeScript"

    ```ts
    import { JaegerClient } from "@grow-codes/jaeger";

    const client = JaegerClient.fromEnv();
    try {
      const health = await client.health();
      console.log("healthy:", health.healthy, health.status);
      console.log("ping:", await client.ping(), "ms");
      console.log("server version:", await client.serverVersion());
    } finally {
      await client.close();
    }
    ```

=== "Rust"

    ```rust
    use grow_jaeger::JaegerClient;

    #[tokio::main]
    async fn main() -> grow_jaeger::Result<()> {
        let client = JaegerClient::from_env()?;
        let health = client.health().await?;
        println!("healthy: {} {}", health.healthy, health.status);
        println!("ping: {:?}", client.ping().await?);
        println!("server version: {}", client.server_version().await?);
        client.close().await;
        Ok(())
    }
    ```

=== "Go"

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
        fmt.Println("healthy:", health.Healthy, health.Status)

        latency, err := client.Ping(ctx)
        if err != nil {
            log.Fatal(err)
        }
        fmt.Println("ping:", latency)

        version, err := client.ServerVersion(ctx)
        if err != nil {
            log.Fatal(err)
        }
        fmt.Println("server version:", version)
    }
    ```

=== "Python"

    ```python
    from grow_jaeger import JaegerClient

    with JaegerClient.from_env() as client:
        health = client.health()
        print("healthy:", health.healthy, health.status)
        print("ping:", client.ping(), "ms")
        print("server version:", client.server_version())
    ```

    Async code uses `AsyncJaegerClient` with the same methods:

    ```python
    import asyncio
    from grow_jaeger import AsyncJaegerClient

    async def main() -> None:
        async with AsyncJaegerClient.from_env() as client:
            print(await client.health())

    asyncio.run(main())
    ```

=== "Java"

    ```java
    import codes.grow.packages.jaeger.JaegerClient;
    import codes.grow.packages.jaeger.core.HealthStatus;

    public final class Quickstart {
      public static void main(String[] args) {
        try (JaegerClient client = JaegerClient.fromEnv()) {
          HealthStatus health = client.health();
          System.out.println("healthy: " + health.healthy() + " " + health.status());
          System.out.println("ping: " + client.ping().toMillis() + " ms");
          System.out.println("server version: " + client.serverVersion());
        }
      }
    }
    ```

## Send a trace and read it back

`initTracing` exports over OTLP HTTP/protobuf to port 4318 (`GROW_JAEGER_OTLP_HTTP_ENDPOINT`, or
the standard `OTEL_EXPORTER_OTLP_ENDPOINT`); the query client reads the trace back from port 16686.

```sh
export GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318
```

=== "TypeScript"

    ```ts
    import { JaegerClient, currentTraceId, initTracing, withSpan } from "@grow-codes/jaeger";

    const tracing = initTracing({ serviceName: "checkout" });
    const traceId = await withSpan("place-order", async () => currentTraceId());
    await tracing.forceFlush();

    const client = JaegerClient.fromEnv();
    const trace = await client.query.waitForTrace(traceId!, { timeoutMs: 15_000, intervalMs: 250 });
    console.log(trace.spans.map((s) => `${s.serviceName} ${s.name}`));
    await tracing.shutdown();
    await client.close();
    ```

=== "Rust"

    ```rust
    use grow_jaeger::tracing::{TracingConfig, WaitForTraceOptions};
    use opentelemetry::trace::{Span as _, Tracer as _};

    let tracing = grow_jaeger::init_tracing(TracingConfig::new("checkout"))?;
    let mut span = tracing.tracer("checkout", None).start("place-order");
    let trace_id = span.span_context().trace_id().to_string();
    span.end();
    tracing.force_flush(None).await?;

    let client = grow_jaeger::JaegerClient::from_env()?;
    let trace = client.query().wait_for_trace(&trace_id, WaitForTraceOptions::default()).await?;
    println!("{} spans", trace.spans.len());
    tracing.shutdown().await?;
    ```

=== "Go"

    ```go
    h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "checkout"})
    if err != nil {
        log.Fatal(err)
    }
    _, span := h.Tracer("checkout").Start(ctx, "place-order")
    span.End()
    _ = h.ForceFlush(ctx)

    client, err := jaeger.NewClientFromEnv()
    if err != nil {
        log.Fatal(err)
    }
    defer client.Close()
    tr, err := client.Query().WaitForTrace(ctx, span.SpanContext().TraceID().String(), jaeger.WaitForTraceOptions{})
    if err != nil {
        log.Fatal(err)
    }
    fmt.Println(len(tr.Spans), "spans")
    _ = h.Shutdown(ctx)
    ```

=== "Python"

    ```python
    from grow_jaeger import JaegerClient, current_trace_id, init_tracing, with_span

    tracing = init_tracing(service_name="checkout")
    with with_span("place-order"):
        trace_id = current_trace_id()
    tracing.force_flush()

    with JaegerClient.from_env() as client:
        trace = client.query.wait_for_trace(trace_id, timeout_s=15, interval_s=0.25)
        print([s.name for s in trace.spans])
    tracing.shutdown()
    ```

=== "Java"

    ```java
    JaegerTracingHandle tracing = JaegerTracing.initTracing(TracingConfig.builder("checkout").build());
    String traceId = Traced.run("place-order", () -> Spans.currentTraceId().orElseThrow());
    tracing.forceFlush();

    try (JaegerClient client = JaegerClient.fromEnv()) {
      Trace trace = client.query().waitForTrace(traceId, WaitForTraceOptions.defaults());
      System.out.println(trace.spans().size() + " spans");
    }
    tracing.shutdown();
    ```

## Next steps

- [Configuration](configuration.md): timeouts, retries, TLS, proxy, environment variables.
- [Functions](functions.md): the Jaeger API of this SDK.
- [Error handling](errors.md): error codes and retries.
