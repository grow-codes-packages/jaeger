package codes.grow.packages.jaeger.integration;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import codes.grow.packages.jaeger.GetTraceOptions;
import codes.grow.packages.jaeger.JaegerClient;
import codes.grow.packages.jaeger.JaegerConfig;
import codes.grow.packages.jaeger.JaegerOperation;
import codes.grow.packages.jaeger.JaegerQueryClient;
import codes.grow.packages.jaeger.JaegerRemoteSampler;
import codes.grow.packages.jaeger.JaegerSamplingStrategy;
import codes.grow.packages.jaeger.JaegerTracing;
import codes.grow.packages.jaeger.JaegerTracingHandle;
import codes.grow.packages.jaeger.SamplingPreset;
import codes.grow.packages.jaeger.core.CancelledException;
import codes.grow.packages.jaeger.core.HealthStatus;
import codes.grow.packages.jaeger.core.NotFoundException;
import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.SpanOptions;
import codes.grow.packages.jaeger.tracing.Spans;
import codes.grow.packages.jaeger.tracing.Traced;
import codes.grow.packages.jaeger.tracing.TracedHttpClient;
import codes.grow.packages.jaeger.tracing.TracedProxy;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.query.DependencyLink;
import codes.grow.packages.jaeger.tracing.query.Span;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceSearchQuery;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import codes.grow.packages.jaeger.tracing.query.WaitForTraceOptions;
import io.opentelemetry.api.common.AttributeKey;
import io.opentelemetry.api.common.Attributes;
import io.opentelemetry.api.trace.SpanContext;
import io.opentelemetry.api.trace.SpanKind;
import io.opentelemetry.api.trace.Tracer;
import io.opentelemetry.context.Context;
import io.opentelemetry.context.Scope;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.security.SecureRandom;
import java.time.Duration;
import java.time.Instant;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * Integration tests against the real Jaeger from docker/compose.yml. Every public function of the
 * package is called at least once with a round-trip assertion (send spans, read them back through
 * the query API).
 */
class JaegerIntegrationTest {
  private static final SecureRandom RANDOM = new SecureRandom();

  private Map<String, String> env;
  private JaegerClient client;
  private JaegerQueryClient query;

  @BeforeEach
  void setUp() throws Exception {
    env = ConformanceTest.environment();
    assumeTrue(env.containsKey("GROW_JAEGER_URL"), "GROW_JAEGER_URL is not set; use compose");
    client = JaegerClient.create(JaegerConfig.fromEnv(env));
    query = client.query();
  }

  @AfterEach
  void tearDown() {
    if (client != null) {
      client.close();
    }
  }

  private static String run() {
    return Long.toHexString(RANDOM.nextLong() & 0xffffffffL);
  }

  private JaegerTracingHandle tracing(String service, TracingConfig.Protocol protocol) {
    TracingConfig config =
        TracingConfig.builder(service)
            .serviceVersion("1.2.3")
            .environment("sdk-test")
            .protocol(protocol)
            .setGlobal(false)
            .batch(
                new TracingConfig.BatchSettings(
                    2048, 512, Duration.ofMillis(100), Duration.ofSeconds(30)))
            .build();
    return client.initTracing(config, JaegerTracing.Options.builder().env(env));
  }

  private Trace waitTrace(String traceId, int minSpans) {
    return query.waitForTrace(
        traceId, new WaitForTraceOptions(Duration.ofSeconds(30), Duration.ofMillis(250), minSpans));
  }

  private static Span span(Trace trace, String name) {
    List<Span> spans = trace.spansNamed(name);
    assertFalse(spans.isEmpty(), "no span " + name + " in " + trace.traceId());
    return spans.get(0);
  }

  @Test
  void clientCore() {
    JaegerConfig cfg = client.config();
    cfg.validate();
    assertFalse(cfg.samplingUrl().equals(JaegerConfig.DEFAULT_SAMPLING_URL));
    assertTrue(cfg.healthUrl().isPresent());

    HealthStatus hs = client.health();
    assertTrue(hs.healthy(), hs.toString());
    assertEquals("ok", hs.status());
    assertEquals("StatusOK", hs.details().get("healthcheck.status"));
    assertTrue(client.healthAsync().join().healthy());
    assertTrue(client.ping().toNanos() > 0);
    assertTrue(client.pingAsync().join().toNanos() > 0);
    assertEquals("2.23.0", client.serverVersion());
    assertEquals("2.23.0", client.serverVersionAsync().join());
    assertEquals("v2.23.0", client.buildInfo().orElseThrow().gitVersion().orElseThrow());

    // the metrics fallback finds the same version where there is no UI page
    String metrics = cfg.metricsUrl().orElseThrow();
    try (JaegerClient metricsOnly =
        JaegerClient.builder().url(metrics).metricsUrl(metrics).build()) {
      assertTrue(metricsOnly.buildInfo().isEmpty());
      assertEquals("2.23.0", metricsOnly.serverVersion());
    }
    try (JaegerClient unreachable =
        JaegerClient.builder().url("http://127.0.0.1:1").connectTimeout(Duration.ofSeconds(1)).build()) {
      HealthStatus down = unreachable.health();
      assertFalse(down.healthy());
      assertEquals("unreachable", down.status());
    }
    try (JaegerClient fromEnv = JaegerClient.fromEnv()) {
      assertTrue(fromEnv.ping().toNanos() > 0);
    }
    client.close();
    client.close();
    assertTrue(client.isClosed());
    assertThrows(CancelledException.class, client::ping);
  }

  @Test
  void traceRoundTripHttpAndGrpc() throws Exception {
    for (TracingConfig.Protocol protocol :
        List.of(TracingConfig.Protocol.HTTP_PROTOBUF, TracingConfig.Protocol.GRPC)) {
      roundTrip(protocol);
    }
  }

  private void roundTrip(TracingConfig.Protocol protocol) throws Exception {
    String run = run();
    String service =
        "sdk-java-" + (protocol == TracingConfig.Protocol.GRPC ? "grpc" : "http") + "-" + run;
    JaegerTracingHandle h = tracing(service, protocol);
    assertTrue(
        h.endpoint().endsWith(protocol == TracingConfig.Protocol.GRPC ? ":4317" : "/v1/traces"),
        h.endpoint());
    Tracer tracer = h.tracer("integration", "0.1.0");

    io.opentelemetry.api.trace.Span other = tracer.spanBuilder("linked-" + run).setNoParent().startSpan();
    other.end();

    String rootName = "GET /orders " + run;
    io.opentelemetry.api.trace.Span root =
        tracer
            .spanBuilder(rootName)
            .setSpanKind(SpanKind.SERVER)
            .setAllAttributes(
                Attributes.builder()
                    .put("order.id", run)
                    .put("int.attr", 42L)
                    .put("double.attr", 1.5)
                    .put("bool.attr", true)
                    .put(AttributeKey.stringArrayKey("array.attr"), List.of("a", "b"))
                    .build())
            .startSpan();
    String traceId = root.getSpanContext().getTraceId();
    Scope scope = root.makeCurrent();
    try {
      assertEquals(traceId, Spans.currentTraceId().orElseThrow());
      assertEquals(root.getSpanContext().getSpanId(), Spans.currentSpanId().orElseThrow());
      assertEquals(traceId, Spans.traceFields().get("trace_id"));
      assertEquals(root, Spans.currentSpan());
      IllegalStateException boom =
          assertThrows(
              IllegalStateException.class,
              () ->
                  Spans.withSpan(
                      "db.query",
                      SpanOptions.builder()
                          .tracer(tracer)
                          .kind(SpanKind.CLIENT)
                          .link(other.getSpanContext())
                          .build(),
                      () -> {
                        Spans.addEvent("cache.miss", Map.of("cache.key", "k1"));
                        throw new IllegalStateException("boom");
                      }));
      assertEquals("boom", boom.getMessage());
      Spans.withSpan(
          "traced-ok",
          SpanOptions.builder().tracer(tracer).attribute("step", 2L).build(),
          () -> Spans.recordException(new RuntimeException("recorded only")));
      io.opentelemetry.api.trace.Span failing = Spans.start("set-error", SpanOptions.builder().tracer(tracer).build());
      Spans.setError(failing, new RuntimeException("explicit"));
      failing.end();
    } finally {
      scope.close();
      root.end();
    }
    h.forceFlush();

    Trace t = waitTrace(traceId, 4);
    assertEquals(traceId, t.traceId());
    assertEquals(4, t.spans().size(), t.toString());
    Span r = span(t, rootName);
    assertTrue(r.isRoot());
    assertEquals(Span.Kind.SERVER, r.kind());
    assertEquals(service, r.serviceName());
    assertEquals(run, r.attributes().get("order.id"));
    assertEquals(42L, r.attributes().get("int.attr"));
    assertEquals(1.5, r.attributes().get("double.attr"));
    assertEquals(true, r.attributes().get("bool.attr"));
    assertEquals(List.of("a", "b"), r.attributes().get("array.attr"));
    Span db = span(t, "db.query");
    assertEquals(r.spanId(), db.parentSpanId().orElseThrow());
    assertEquals(Span.Kind.CLIENT, db.kind());
    assertEquals(Span.StatusCode.ERROR, db.status().code());
    assertEquals("boom", db.status().message().orElseThrow());
    assertTrue(
        db.events().stream()
            .anyMatch(e -> e.name().equals("cache.miss") && "k1".equals(e.attributes().get("cache.key"))));
    assertEquals(1, db.links().size());
    assertEquals(other.getSpanContext().getTraceId(), db.links().get(0).traceId());
    Span ok = span(t, "traced-ok");
    assertEquals(2L, ok.attributes().get("step"));
    assertTrue(ok.events().stream().anyMatch(e -> e.name().equals("exception")));
    Span se = span(t, "set-error");
    assertEquals(Span.StatusCode.ERROR, se.status().code());
    assertEquals("explicit", se.status().message().orElseThrow());

    assertEquals(4, query.getTrace(traceId).orElseThrow().spans().size());
    assertEquals(4, query.getTraceAsync(traceId).join().orElseThrow().spans().size());
    Instant now = Instant.now();
    Trace raw =
        query
            .getTrace(traceId, GetTraceOptions.window(now.minusSeconds(3600), now.plusSeconds(60)).withRawTraces(true))
            .orElseThrow();
    assertEquals(4, raw.spans().size());
    assertEquals(traceId, query.requireTrace(traceId).traceId());
    assertTrue(query.getTrace("ffffffffffffffffffffffffffffff01").isEmpty());
    assertThrows(NotFoundException.class, () -> query.requireTrace("ffffffffffffffffffffffffffffff02"));
    assertThrows(ValidationException.class, () -> query.getTrace("not-a-trace-id"));

    TraceSearchQuery filter =
        TraceSearchQuery.builder()
            .service(service)
            .operation(rootName)
            .tag("order.id", run)
            .minDurationMicros(0L)
            .maxDurationMicros(3_600_000_000L)
            .build();
    List<Trace> found = query.findTraces(filter);
    assertEquals(1, found.size(), found.toString());
    assertEquals(traceId, found.get(0).traceId());
    assertEquals(4, found.get(0).spans().size());
    assertEquals(1, query.findTracesAsync(filter).join().size());
    assertTrue(
        query.findTraces(TraceSearchQuery.builder().service(service).operation("nope").build()).isEmpty());

    List<TraceSummary> sums =
        query.searchTraces(TraceSearchQuery.builder().service(service).limit(10).build());
    assertTrue(
        sums.stream()
            .anyMatch(
                s ->
                    s.traceId().equals(traceId)
                        && s.rootName().orElse("").equals(rootName)
                        && s.rootServiceName().orElse("").equals(service)
                        && s.spanCount() == 4
                        && s.durationMicros() > 0),
        sums.toString());
    assertFalse(query.searchTracesAsync(TraceSearchQuery.forService(service)).join().isEmpty());
    assertTrue(
        query.searchTraces(TraceSearchQuery.builder().service("sdk-java-none-" + run).build()).isEmpty());

    List<String> services = query.listServices();
    assertTrue(services.contains(service) && services.contains("jaeger"), services.toString());
    assertTrue(query.listServicesAsync().join().contains(service));
    List<String> ops = query.listOperations(service);
    assertTrue(ops.contains(rootName) && ops.contains("db.query"), ops.toString());
    assertEquals(ops, query.listOperationsAsync(service).join());
    assertEquals(List.of(rootName), query.listOperations(service, "SERVER"));
    assertEquals(List.of("db.query"), query.listOperations(service, Span.Kind.CLIENT));
    assertEquals(List.of(rootName), query.listOperationsAsync(service, "server").join());
    List<JaegerOperation> server = query.getOperations(service, "server");
    assertEquals(List.of(new JaegerOperation(rootName, "server")), server);
    assertEquals(server, query.getOperations(service, Span.Kind.SERVER));
    assertEquals(server, query.getOperationsAsync(service, "SERVER").join());
    assertTrue(query.getOperations(service).contains(new JaegerOperation("db.query", "client")));
    assertTrue(query.listOperations("sdk-java-unknown-" + run).isEmpty());
    h.shutdown();
    assertTrue(h.isShutdown());
  }

  @Test
  void dependenciesAndPropagation() {
    String run = run();
    String parentSvc = "sdk-java-dep-a-" + run;
    String childSvc = "sdk-java-dep-b-" + run;
    JaegerTracingHandle a = tracing(parentSvc, TracingConfig.Protocol.HTTP_PROTOBUF);
    JaegerTracingHandle b = tracing(childSvc, TracingConfig.Protocol.GRPC);

    io.opentelemetry.api.trace.Span call =
        a.tracer("a").spanBuilder("call-b").setSpanKind(SpanKind.CLIENT).startSpan();
    Context actx = Context.root().with(call);
    Map<String, String> carrier = new HashMap<>();
    Spans.inject(actx, carrier, a.propagators());
    assertTrue(carrier.containsKey("traceparent"), carrier.toString());
    Map<String, List<String>> headers = new HashMap<>();
    Spans.injectHeaders(actx, headers, a.propagators());
    assertEquals(carrier.get("traceparent"), headers.get("traceparent").get(0));
    Context fromHeaders = Spans.extractHeaders(headers, b.propagators());
    Context fromCarrier = Spans.extract(carrier, b.propagators());
    assertEquals(
        call.getSpanContext().getTraceId(),
        io.opentelemetry.api.trace.Span.fromContext(fromCarrier).getSpanContext().getTraceId());
    b.tracer("b")
        .spanBuilder("handle-call")
        .setParent(fromHeaders)
        .setSpanKind(SpanKind.SERVER)
        .startSpan()
        .end();
    call.end();
    a.forceFlush();
    b.forceFlush();

    Trace t = waitTrace(call.getSpanContext().getTraceId(), 2);
    Span handle = span(t, "handle-call");
    assertEquals(call.getSpanContext().getSpanId(), handle.parentSpanId().orElseThrow());
    assertEquals(childSvc, handle.serviceName());

    DependencyLink want = new DependencyLink(parentSvc, childSvc, 1);
    Instant now = Instant.now();
    assertTrue(query.getDependencies(now.minusSeconds(3600), (Instant) null).contains(want));
    assertTrue(query.getDependenciesAsync(now.minusSeconds(3600), now.plusSeconds(60)).join().contains(want));
    assertTrue(query.getDependencies(now.plusSeconds(60), Duration.ofHours(1)).contains(want));
    List<DependencyLink> filtered = query.getDependencies(now.plusSeconds(60), Duration.ofHours(1), childSvc);
    assertTrue(filtered.contains(want), filtered.toString());
    assertTrue(
        query.getDependenciesAsync(now.plusSeconds(60), Duration.ofHours(1), parentSvc).join().contains(want));
    assertFalse(query.getDependencies(now.minus(Duration.ofDays(1)), Duration.ofMinutes(1)).contains(want));
  }

  /** Interface for the TracedProxy round trip. */
  interface Orders {
    @Traced.Method
    String place(String id);
  }

  @Test
  void httpAdaptersAndTracedProxy() throws Exception {
    String run = run();
    String service = "sdk-java-adapters-" + run;
    JaegerTracingHandle h = tracing(service, TracingConfig.Protocol.HTTP_PROTOBUF);
    Tracer tracer = h.tracer("adapters");
    TracedHttpClient http = TracedHttpClient.wrap(HttpClient.newHttpClient(), tracer, h.propagators());
    Orders orders = TracedProxy.create(Orders.class, id -> "placed " + id, tracer);

    io.opentelemetry.api.trace.Span root = tracer.spanBuilder("adapters-root-" + run).startSpan();
    Scope scope = root.makeCurrent();
    try {
      HttpResponse<String> resp =
          http.send(
              HttpRequest.newBuilder(URI.create(client.config().url() + "/api/v3/services")).build(),
              HttpResponse.BodyHandlers.ofString());
      assertEquals(200, resp.statusCode());
      assertEquals(200, http.sendAsync(
              HttpRequest.newBuilder(URI.create(client.config().url() + "/api/v3/services")).build(),
              HttpResponse.BodyHandlers.ofString())
          .join()
          .statusCode());
      assertEquals("placed 7", orders.place("7"));
    } finally {
      scope.close();
      root.end();
    }
    h.forceFlush();
    Trace t = waitTrace(root.getSpanContext().getTraceId(), 4);
    long clients =
        t.spans().stream()
            .filter(s -> s.kind() == Span.Kind.CLIENT && s.serviceName().equals(service))
            .filter(s -> s.parentSpanId().orElse("").equals(root.getSpanContext().getSpanId()))
            .count();
    assertEquals(2, clients, t.toString());
    assertFalse(span(t, "Orders.place").parentSpanId().isEmpty());
  }

  @Test
  void remoteSampling() {
    String run = run();
    String fixtureUrl = env.get("GROW_JAEGER_SAMPLING_FIXTURE_URL");
    assertNotNull(fixtureUrl, "GROW_JAEGER_SAMPLING_FIXTURE_URL missing from /bootstrap/env");

    // stock all-in-one: probability 1 for every service
    JaegerSamplingStrategy stock = client.samplingStrategy("sdk-java-stock-" + run);
    assertEquals(JaegerSamplingStrategy.PROBABILISTIC, stock.strategyType());
    assertEquals(1.0, stock.samplingRate().orElseThrow());
    assertEquals(1.0, client.samplingStrategyAsync("sdk-java-stock-" + run).join().serviceRate().orElseThrow());
    assertThrows(ValidationException.class, () -> client.samplingStrategy(" "));

    // strategies file of the jaeger-sampling fixture instance
    try (JaegerRemoteSampler ops =
        JaegerRemoteSampler.builder("sdk-sampling-ops").samplingUrl(fixtureUrl).autoStart(false).build()) {
      JaegerSamplingStrategy s = ops.refresh();
      assertEquals(0.8, s.samplingRate().orElseThrow());
      assertEquals(0.2, s.operationRate("GET /fast").orElseThrow());
      assertEquals(0.4, s.operationRate("GET /slow").orElseThrow());
      assertEquals(0.0, s.operationRate("/health").orElseThrow());
      assertEquals(s, ops.strategy().orElseThrow());
    }
    try (JaegerRemoteSampler rate =
        JaegerRemoteSampler.builder("sdk-sampling-rate").samplingUrl(fixtureUrl).autoStart(false).build()) {
      JaegerSamplingStrategy s = rate.refreshAsync().join();
      assertEquals(JaegerSamplingStrategy.RATE_LIMITING, s.strategyType());
      assertEquals(5.0, s.maxTracesPerSecond().orElseThrow());
    }
    AtomicReference<JaegerSamplingStrategy> updated = new AtomicReference<>();
    try (JaegerRemoteSampler prob =
        JaegerRemoteSampler.builder("sdk-sampling-prob")
            .samplingUrl(fixtureUrl)
            .pollInterval(Duration.ofSeconds(30))
            .onUpdate(updated::set)
            .build()) {
      assertTrue(prob.awaitFirstPoll(Duration.ofSeconds(15)));
      assertEquals(0.25, updated.get().samplingRate().orElseThrow());
    }
    try (JaegerRemoteSampler created = JaegerRemoteSampler.create(fixtureUrl, "sdk-sampling-unknown-" + run, null)) {
      assertTrue(created.awaitFirstPoll(Duration.ofSeconds(15)));
      assertEquals(0.5, created.strategy().orElseThrow().samplingRate().orElseThrow());
    }

    // client sampler: stock probability 1
    JaegerRemoteSampler fromClient = client.remoteSampler("sdk-java-cs-" + run, Duration.ofSeconds(30));
    assertTrue(fromClient.awaitFirstPoll(Duration.ofSeconds(15)));
    assertEquals(1.0, fromClient.strategy().orElseThrow().samplingRate().orElseThrow());

    // tracing sampled by the remote strategy (probability 1): the spans come back
    String service = "sdk-java-remote-" + run;
    JaegerTracingHandle h =
        client.initTracing(
            SamplingPreset.development()
                .applyTo(TracingConfig.builder(service).setGlobal(false))
                .batch(new TracingConfig.BatchSettings(2048, 512, Duration.ofMillis(100), Duration.ofSeconds(30)))
                .build(),
            JaegerTracing.Options.builder()
                .env(env)
                .remoteSampling(JaegerTracing.RemoteSampling.defaults().withInitialSamplingRate(0)));
    JaegerRemoteSampler sampler = h.remoteSampler().orElseThrow();
    assertTrue(sampler.awaitFirstPoll(Duration.ofSeconds(15)));
    io.opentelemetry.api.trace.Span sp = h.tracer("remote").spanBuilder("sampled-" + run).startSpan();
    String traceId = sp.getSpanContext().getTraceId();
    assertTrue(sp.getSpanContext().isSampled());
    sp.end();
    h.forceFlush();
    assertEquals(service, waitTrace(traceId, 1).spans().get(0).serviceName());
    h.shutdown();
    assertTrue(sampler.isClosed());

    // the environment asks for remote sampling (OTEL_TRACES_SAMPLER=parentbased_jaeger_remote)
    Map<String, String> remoteEnv = new HashMap<>(env);
    remoteEnv.put("OTEL_TRACES_SAMPLER", "parentbased_jaeger_remote");
    remoteEnv.put("OTEL_TRACES_SAMPLER_ARG", "initialSamplingRate=0");
    remoteEnv.put("GROW_JAEGER_SERVICE_NAME", "sdk-java-envremote-" + run);
    try (JaegerTracingHandle fromEnv = JaegerTracing.initTracingFromEnv(null, remoteEnv)) {
      JaegerRemoteSampler s = fromEnv.remoteSampler().orElseThrow();
      assertTrue(s.awaitFirstPoll(Duration.ofSeconds(15)));
      assertEquals("sdk-java-envremote-" + run, fromEnv.config().serviceName());
      io.opentelemetry.api.trace.Span e = fromEnv.tracer("env").spanBuilder("env-" + run).startSpan();
      SpanContext ctx = e.getSpanContext();
      e.end();
      fromEnv.forceFlush();
      assertTrue(ctx.isSampled());
      assertEquals(1, waitTrace(ctx.getTraceId(), 1).spans().size());
    }
  }

  @Test
  void standaloneQueryClientAndPresets() {
    try (JaegerQueryClient own = JaegerQueryClient.create(JaegerConfig.fromEnv(env))) {
      assertTrue(own.listServices().contains("jaeger"));
    }
    try (JaegerQueryClient fromEnv = JaegerQueryClient.fromEnv()) {
      assertTrue(fromEnv.listServices().contains("jaeger"));
    }
    assertEquals(
        TracingConfig.SamplerType.PARENTBASED_TRACEIDRATIO, SamplingPreset.production().sampler());
    assertEquals(TracingConfig.SamplerType.PARENTBASED_ALWAYS_ON, SamplingPreset.followParent().sampler());
    assertEquals(TracingConfig.SamplerType.ALWAYS_OFF, SamplingPreset.off().sampler());
    assertEquals(0.3, SamplingPreset.ratio(0.3).ratio());
    TracingConfig resolved = JaegerTracing.configFromEnv("sdk-java-cfg", env);
    assertTrue(resolved.endpoint().orElseThrow().startsWith("http://"));
    assertEquals(
        JaegerTracing.DEFAULT_OTLP_HTTP_ENDPOINT,
        JaegerTracing.resolveEndpoint(TracingConfig.Protocol.HTTP_PROTOBUF, null));
  }
}
