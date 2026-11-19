# Trace queries

`JaegerQueryClient` reads traces back from Jaeger's query service (port 16686). It implements the
category interface `TraceQueryClient` and uses:

- the **v3 HTTP API** for traces, search, services and operations (`/api/v3/traces/{id}`,
  `/api/v3/traces`, `/api/v3/trace-summaries`, `/api/v3/services`, `/api/v3/operations`);
- the legacy `/api/traces/{id}` as a **fallback** for `getTrace` when the v3 route is missing
  (a plain-text `404 page not found`, or a proxy page that is not Jaeger's JSON envelope);
- the legacy `/api/dependencies` (Jaeger has no v3 HTTP dependencies route).

Get one from the client (it shares the client's connections and token) or build a standalone one
from a config (it owns its connections; close it).

Results use the normalized model: `Trace { traceId, spans[] }` with spans sorted by start time,
`Span { traceId, spanId, parentSpanId, name, kind, serviceName, startMicros, endMicros,
durationMicros, status, attributes, events, links }`. Trace ids are 32 lower-case hex characters;
16-hex ids are left-padded; anything else raises `ValidationError` without a request.

=== "TypeScript"

    ```ts
    import { JaegerClient, JaegerQueryClient } from "@grow-codes/jaeger";

    const client = JaegerClient.fromEnv();
    const query = client.query;                       // shared connections
    const standalone = JaegerQueryClient.fromEnv();   // own connections: await standalone.close()
    ```

=== "Rust"

    ```rust
    let client = grow_jaeger::JaegerClient::from_env()?;
    let query = client.query(); // &JaegerQueryClient, shares the client's transport
    ```

=== "Go"

    ```go
    client, err := jaeger.NewClientFromEnv()
    if err != nil {
        return err
    }
    q := client.Query() // *jaeger.QueryClient
    ```

=== "Python"

    ```python
    from grow_jaeger import JaegerClient, AsyncJaegerClient

    client = JaegerClient.from_env()
    query = client.query               # JaegerQueryClient
    aclient = AsyncJaegerClient.from_env()
    aquery = aclient.query             # AsyncJaegerQueryClient (same methods, awaitable)
    ```

=== "Java"

    ```java
    JaegerClient client = JaegerClient.fromEnv();
    JaegerQueryClient query = client.query();  // every method also has an …Async variant
    ```

## getTrace / requireTrace

`getTrace(traceId, {start?, end?, rawTraces?})` returns the trace or `null` when it does not exist
(Jaeger's JSON 404 envelope). The default lookup window is the last 7 days (the memory store
ignores it). `requireTrace` raises `NotFoundError` instead of returning `null`.

=== "TypeScript"

    ```ts
    const trace = await client.query.getTrace("4bf92f3577b34da6a3ce929d0e0e4736");
    if (trace !== null) console.log(trace.spans.map((s) => s.name));
    const sure = await client.query.requireTrace(traceId, { rawTraces: true });
    ```

=== "Rust"

    ```rust
    if let Some(trace) = client.query().get_trace("4bf92f3577b34da6a3ce929d0e0e4736").await? {
        println!("{} spans", trace.spans.len());
    }
    let sure = client.query().require_trace(&trace_id).await?;
    let raw = client
        .query()
        .get_trace_with(&trace_id, GetTraceOptions { raw_traces: true, ..Default::default() })
        .await?;
    ```

=== "Go"

    ```go
    tr, err := client.Query().GetTrace(ctx, "4bf92f3577b34da6a3ce929d0e0e4736")
    if err != nil {
        return err
    }
    if tr != nil {
        fmt.Println(len(tr.Spans), "spans")
    }
    sure, err := client.Query().RequireTrace(ctx, traceID) // errors.Is(err, jaeger.ErrNotFound)
    raw, err := client.Query().GetTraceWith(ctx, traceID, jaeger.GetTraceOptions{RawTraces: true})
    ```

=== "Python"

    ```python
    trace = client.query.get_trace("4bf92f3577b34da6a3ce929d0e0e4736")
    if trace is not None:
        print([s.name for s in trace.spans])
    sure = client.query.require_trace(trace_id, raw_traces=True)
    ```

=== "Java"

    ```java
    Optional<Trace> trace = client.query().getTrace("4bf92f3577b34da6a3ce929d0e0e4736");
    trace.ifPresent(t -> System.out.println(t.spans().size() + " spans"));
    Trace sure = client.query().requireTrace(traceId); // NotFoundException when missing
    ```

## findTraces / searchTraces

Both take the same filter: `{service, operation, tags, minDurationMicros, maxDurationMicros,
start, end, limit}`. Jaeger needs both time bounds, so the SDK always sends them (default: the last
hour); `limit` defaults to 20. `tags` match attributes on any span of the trace.
`findTraces` returns full traces (`/api/v3/traces`); `searchTraces` returns
`TraceSummary { traceId, rootServiceName, rootName, startMicros, durationMicros, spanCount }`
(`/api/v3/trace-summaries`). No match returns an empty list.

=== "TypeScript"

    ```ts
    const traces = await client.query.findTraces({
      service: "checkout",
      operation: "POST /orders",
      tags: { "http.response.status_code": "500" },
      minDurationMicros: 1_000,
      limit: 10,
    });
    const summaries = await client.query.searchTraces({ service: "checkout" });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::tracing::TraceSearchQuery;

    let mut q = TraceSearchQuery::service("checkout");
    q.operation = Some("POST /orders".into());
    q.tags.insert("http.response.status_code".into(), "500".into());
    q.min_duration = Some(Duration::from_millis(1));
    q.limit = 10;
    let traces = client.query().find_traces(&q).await?;
    let summaries = client.query().search_traces(&TraceSearchQuery::service("checkout")).await?;
    ```

=== "Go"

    ```go
    traces, err := client.Query().FindTraces(ctx, jaeger.TraceSearchQuery{
        Service:     "checkout",
        Operation:   "POST /orders",
        Tags:        map[string]string{"http.response.status_code": "500"},
        MinDuration: time.Millisecond,
        Limit:       10,
    })
    summaries, err := client.Query().SearchTraces(ctx, jaeger.TraceSearchQuery{Service: "checkout"})
    ```

=== "Python"

    ```python
    traces = client.query.find_traces(
        service="checkout",
        operation="POST /orders",
        tags={"http.response.status_code": "500"},
        min_duration_micros=1_000,
        limit=10,
    )
    summaries = client.query.search_traces(service="checkout")
    ```

=== "Java"

    ```java
    List<Trace> traces = client.query().findTraces(TraceSearchQuery.builder()
        .service("checkout")
        .operation("POST /orders")
        .tag("http.response.status_code", "500")
        .limit(10)
        .build());
    List<TraceSummary> summaries =
        client.query().searchTraces(TraceSearchQuery.forService("checkout"));
    ```

## listServices

Service names with stored spans, sorted (`/api/v3/services`). The all-in-one also reports
itself as `jaeger`.

=== "TypeScript"

    ```ts
    const services = await client.query.listServices();
    ```

=== "Rust"

    ```rust
    let services = client.query().list_services().await?;
    ```

=== "Go"

    ```go
    services, err := client.Query().ListServices(ctx)
    ```

=== "Python"

    ```python
    services = client.query.list_services()
    ```

=== "Java"

    ```java
    List<String> services = client.query().listServices();
    ```

## listOperations / getOperations

`listOperations(service, {spanKind?})` returns operation names, sorted and de-duplicated across
span kinds; `getOperations(service, {spanKind?})` returns `{name, spanKind}` pairs. The span kind
filter (`server`, `client`, `producer`, `consumer`, `internal`) is case-insensitive and always
sent lower-case (Jaeger silently returns nothing for `SERVER`). An empty service raises
`ValidationError` without a request; an unknown service returns an empty list.

=== "TypeScript"

    ```ts
    const names = await client.query.listOperations("checkout");
    const server = await client.query.getOperations("checkout", { spanKind: "server" });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::ListOperationsOptions;
    use grow_jaeger::tracing::SpanKind;

    let names = client.query().list_operations("checkout", ListOperationsOptions::default()).await?;
    let server = client
        .query()
        .get_operations("checkout", ListOperationsOptions::span_kind(SpanKind::Server))
        .await?;
    ```

=== "Go"

    ```go
    names, err := client.Query().ListOperations(ctx, "checkout")
    serverNames, err := client.Query().ListOperationsWith(ctx, "checkout", jaeger.ListOperationsOptions{SpanKind: jaeger.SpanKindServer})
    server, err := client.Query().GetOperations(ctx, "checkout", jaeger.ListOperationsOptions{SpanKind: jaeger.SpanKindServer})
    ```

=== "Python"

    ```python
    names = client.query.list_operations("checkout")
    server = client.query.get_operations("checkout", span_kind="server")
    ```

=== "Java"

    ```java
    List<String> names = client.query().listOperations("checkout");
    List<JaegerOperation> server = client.query().getOperations("checkout", "server");
    ```

## getDependencies

Service dependency edges `DependencyLink { parent, child, callCount }` from the legacy
`/api/dependencies`. Pass the category window (`start`/`end`, default the last 24 h) or Jaeger's
native `endTs` + `lookback`; an optional `service` keeps only edges that touch it. The memory
store derives edges on the fly; Cassandra, Elasticsearch and OpenSearch need the
`spark-dependencies` job, so an empty list is normal there.

=== "TypeScript"

    ```ts
    const lastDay = await client.query.getDependencies();
    const window = await client.query.getDependencies({ endTs: new Date(), lookbackMs: 3_600_000 });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::tracing::DependencyOptions;

    let last_day = client.query().get_dependencies(DependencyOptions::default()).await?;
    let window = client
        .query()
        .get_dependencies_at(Some(SystemTime::now()), Some(Duration::from_secs(3600)))
        .await?;
    ```

=== "Go"

    ```go
    lastDay, err := client.Query().GetDependencies(ctx, jaeger.DependencyOptions{})
    window, err := client.Query().GetDependenciesAt(ctx, time.Now(), time.Hour)
    ```

=== "Python"

    ```python
    last_day = client.query.get_dependencies()
    window = client.query.get_dependencies(end_ts=datetime.now(timezone.utc), lookback_s=3600)
    ```

=== "Java"

    ```java
    List<DependencyLink> lastDay =
        client.query().getDependencies(Instant.now().minus(Duration.ofDays(1)), Instant.now());
    List<DependencyLink> window = client.query().getDependencies(Instant.now(), Duration.ofHours(1));
    ```

## waitForTrace

Test helper: polls `getTrace` until the trace has at least `minSpans` spans (defaults: timeout
30 s, interval 1 s, 1 span) and raises `TimeoutError` otherwise. An ingested span is queryable
about 200 ms after the export ack, so tests usually poll every 200–500 ms.

=== "TypeScript"

    ```ts
    const trace = await client.query.waitForTrace(traceId, { timeoutMs: 15_000, intervalMs: 250 });
    ```

=== "Rust"

    ```rust
    use grow_jaeger::tracing::WaitForTraceOptions;

    let opts = WaitForTraceOptions::default()
        .timeout(Duration::from_secs(15))
        .interval(Duration::from_millis(250));
    let trace = client.query().wait_for_trace(&trace_id, opts).await?;
    ```

=== "Go"

    ```go
    tr, err := client.Query().WaitForTrace(ctx, traceID, jaeger.WaitForTraceOptions{
        Timeout: 15 * time.Second, Interval: 250 * time.Millisecond,
    })
    ```

=== "Python"

    ```python
    trace = client.query.wait_for_trace(trace_id, timeout_s=15, interval_s=0.25)
    ```

=== "Java"

    ```java
    Trace trace = client.query().waitForTrace(
        traceId, new WaitForTraceOptions(Duration.ofSeconds(15), Duration.ofMillis(250), 1));
    ```

## Errors

| Situation | Result |
| - | - |
| trace does not exist (JSON 404 envelope) | `getTrace` → `null`; `requireTrace` → `NOT_FOUND` |
| search without a match | empty list |
| route missing on the server (plain-text 404) | `getTrace` falls back to `/api/traces/{id}`; other calls → `UNSUPPORTED` |
| bad id, bad window, empty service, unknown span kind | `VALIDATION`, no request |
| 400 from Jaeger (e.g. bad duration) | `VALIDATION` with Jaeger's message |
| 401 / 403 from an auth proxy | `AUTHENTICATION` / `PERMISSION_DENIED` |

Error request ids fall back to Jaeger's `traceresponse` header.
