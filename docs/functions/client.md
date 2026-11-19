# Client functions

`JaegerClient` talks to the Jaeger query service (port 16686) and is the entry point to the other
modules: `query` ([Trace queries](query.md)), `initTracing` ([Tracing setup](tracing-setup.md))
and `remoteSampler` / `samplingStrategy` ([Remote sampling](sampling.md)). Create one per process
and close it on shutdown: `close()` is idempotent and also shuts down every tracing handle and
remote sampler created through the client. The shared behaviour (timeouts, retries, hooks,
errors) is described in [Client](../client.md).

## health

`GET /` on the query port (the stack catalog health target), **without** the token. With
`healthUrl` set (healthcheckv2, port 13133, `/status`), its answer is added to `details` with the
prefix `healthcheck.`; an unhealthy answer there gives `healthy=false`, `status="degraded"`.
Never throws for an unhealthy or unreachable service (`status="unreachable"`), only for
`CANCELLED` and `CONFIG`.

=== "TypeScript"

    ```ts
    const health = await client.health();
    if (!health.healthy) console.warn(health.status, health.details);
    ```

=== "Rust"

    ```rust
    let health = client.health().await?;
    println!("{} {}", health.healthy, health.status);
    ```

=== "Go"

    ```go
    hs, err := client.Health(ctx)
    if err == nil && !hs.Healthy {
        log.Printf("jaeger %s %v", hs.Status, hs.Details)
    }
    ```

=== "Python"

    ```python
    health = client.health()
    print(health.healthy, health.status, health.details)
    ```

=== "Java"

    ```java
    HealthStatus health = client.health();
    System.out.println(health.healthy() + " " + health.status());
    ```

## ping

Authenticated round trip through the query API (`GET /api/v3/services`); returns the latency.
Raises `AuthenticationError` when an auth proxy rejects the token.

=== "TypeScript"

    ```ts
    const latencyMs = await client.ping();
    ```

=== "Rust"

    ```rust
    let latency: std::time::Duration = client.ping().await?;
    ```

=== "Go"

    ```go
    latency, err := client.Ping(ctx)
    ```

=== "Python"

    ```python
    latency_ms = client.ping()
    ```

=== "Java"

    ```java
    Duration latency = client.ping();
    ```

## serverVersion / buildInfo

`serverVersion()` returns the version without the leading `v` (`2.23.0`): from
`JAEGER_VERSION` in the UI page (`GET /`), else from `target_info{service_version=…}` at
`metricsUrl` (port 8888). It raises `UnsupportedError` when neither source reports one; a 404 from
either source means "no version", not `NotFoundError`. `buildInfo()` returns the whole
`JAEGER_VERSION` object (`gitVersion`, `gitCommit`, `buildDate`) or `null`.

=== "TypeScript"

    ```ts
    const version = await client.serverVersion();   // "2.23.0"
    const info = await client.buildInfo();          // { gitVersion: "v2.23.0", gitCommit, buildDate } | null
    ```

=== "Rust"

    ```rust
    let version = client.server_version().await?;
    if let Some(info) = client.build_info().await? {
        println!("{:?} {:?}", info.git_version, info.git_commit);
    }
    ```

=== "Go"

    ```go
    version, err := client.ServerVersion(ctx)   // errors.Is(err, jaeger.ErrUnsupported)
    info, err := client.BuildInfo(ctx)           // *jaeger.BuildInfo or nil
    ```

=== "Python"

    ```python
    version = client.server_version()
    info = client.build_info()   # {"gitVersion": "v2.23.0", ...} or None
    ```

=== "Java"

    ```java
    String version = client.serverVersion();
    Optional<JaegerBuildInfo> info = client.buildInfo();
    ```

## samplingStrategy

Fetches the remote sampling strategy of a service once: `GET {samplingUrl}?service=<name>`, with
the client's settings and token. A blank service raises `ValidationError` without a request. See
[Remote sampling](sampling.md) for the strategy model.

=== "TypeScript"

    ```ts
    const strategy = await client.samplingStrategy("checkout");
    console.log(strategy.type, strategy.samplingRate, strategy.operations);
    ```

=== "Rust"

    ```rust
    let strategy = client.sampling_strategy("checkout").await?;
    println!("{:?} {}", strategy.strategy_type, strategy.sampling_rate);
    ```

=== "Go"

    ```go
    strategy, err := client.SamplingStrategy(ctx, "checkout")
    fmt.Println(strategy.StrategyType, strategy.SamplingRate)
    ```

=== "Python"

    ```python
    strategy = client.sampling_strategy("checkout")
    print(strategy.type, strategy.sampling_rate)
    ```

=== "Java"

    ```java
    JaegerSamplingStrategy strategy = client.samplingStrategy("checkout");
    System.out.println(strategy.samplingRate());
    ```

## initTracing (client)

The same as the package-level [`initTracing`](tracing-setup.md), but a remote sampler started for
`OTEL_TRACES_SAMPLER=jaeger_remote` (or requested in code) uses the client's configuration
(`samplingUrl`, token, TLS), and `close()` shuts the handle down (flushing its spans).

=== "TypeScript"

    ```ts
    const handle = client.initTracing({ serviceName: "checkout" });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::{tracing::TracingConfig, JaegerTracingOptions};

    let handle = client.init_tracing(TracingConfig::new("checkout"), JaegerTracingOptions::default())?;
    ```

=== "Go"

    ```go
    h, err := client.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "checkout"})
    ```

=== "Python"

    ```python
    handle = client.init_tracing(service_name="checkout")
    ```

=== "Java"

    ```java
    JaegerTracingHandle handle = client.initTracing(TracingConfig.builder("checkout").build());
    ```

## remoteSampler

A started [`JaegerRemoteSampler`](sampling.md) for a service that uses the client's settings
(`samplingUrl`, timeouts, TLS, proxy, headers, token). `close()` stops it.

=== "TypeScript"

    ```ts
    const sampler = client.remoteSampler({ serviceName: "checkout", pollIntervalMs: 60_000 });
    ```

=== "Rust"

    ```rust
    let sampler = client.remote_sampler("checkout", Duration::from_secs(60))?;
    ```

=== "Go"

    ```go
    sampler, err := client.NewRemoteSampler("checkout", time.Minute)
    ```

=== "Python"

    ```python
    sampler = client.remote_sampler("checkout", 60)
    ```

=== "Java"

    ```java
    JaegerRemoteSampler sampler = client.remoteSampler("checkout", Duration.ofMinutes(1));
    ```

## close

Idempotent. Shuts down the tracing handles (flushing their batch processors) and the samplers
created through the client, then releases connections. Later calls fail with `CONFIG` (TypeScript,
Python) or `CANCELLED` (Rust, Go, Java).

=== "TypeScript"

    ```ts
    await client.close();
    ```

=== "Rust"

    ```rust
    client.close().await;
    ```

=== "Go"

    ```go
    defer client.Close()
    ```

=== "Python"

    ```python
    with JaegerClient.from_env() as client:   # or client.close(); async: await client.aclose()
        ...
    ```

=== "Java"

    ```java
    try (JaegerClient client = JaegerClient.fromEnv()) {
        // ...
    }
    ```
