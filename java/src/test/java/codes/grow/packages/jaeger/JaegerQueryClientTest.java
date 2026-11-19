package codes.grow.packages.jaeger;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import codes.grow.packages.jaeger.core.CancelledException;
import codes.grow.packages.jaeger.core.EnvReader;
import codes.grow.packages.jaeger.core.JaegerException;
import codes.grow.packages.jaeger.core.MockServer;
import codes.grow.packages.jaeger.core.NotFoundException;
import codes.grow.packages.jaeger.core.PermissionDeniedException;
import codes.grow.packages.jaeger.core.RetryPolicy;
import codes.grow.packages.jaeger.core.UnsupportedException;
import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.query.DependencyLink;
import codes.grow.packages.jaeger.tracing.query.Span;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceSearchQuery;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import codes.grow.packages.jaeger.tracing.query.WaitForTraceOptions;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletionException;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class JaegerQueryClientTest {
  private static final String ID = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";
  private static final Instant NOW = Instant.parse("2026-11-18T21:00:00Z");
  private static final String ROUTE_404 = "404 page not found\n";

  private MockServer server;
  private JaegerQueryClient q;

  @BeforeEach
  void start() throws Exception {
    server = MockServer.start();
    q =
        JaegerQueryClient.create(
            JaegerConfig.builder()
                .url(server.url())
                .token("qtok-1234567")
                .retry(RetryPolicy.builder().initialBackoff(Duration.ofMillis(1)).build())
                .build());
    q.clock(Clock.fixed(NOW, ZoneOffset.UTC));
  }

  @AfterEach
  void stop() {
    q.close();
    server.close();
  }

  private String uri(int i) {
    return URLDecoder.decode(server.requests().get(i).uri(), StandardCharsets.UTF_8);
  }

  private void route404() {
    server.enqueue(404, ROUTE_404, "Content-Type", "text/plain; charset=utf-8");
  }

  @Test
  void getTraceV3() {
    Fixtures.enqueue(server, "getTrace.v3_ok");
    Trace t = q.getTrace(ID.toUpperCase()).orElseThrow();
    assertThat(t.traceId()).isEqualTo(ID);
    assertThat(t.spans()).hasSize(3);
    Span root = t.root().orElseThrow();
    assertThat(root.name()).isEqualTo("GET /orders");
    assertThat(root.kind()).isEqualTo(Span.Kind.CLIENT);
    assertThat(root.serviceName()).isEqualTo("sdk-gw-86837b62");
    assertThat(root.status().code()).isEqualTo(Span.StatusCode.OK);
    assertThat(root.durationMicros()).isEqualTo(1_200_000);
    assertThat(root.startMicros()).isEqualTo(1795034918142512L);
    Span handle = t.spansNamed("handle /orders").get(0);
    assertThat(handle.parentSpanId()).contains("55bc22d6caba571a");
    assertThat(handle.kind()).isEqualTo(Span.Kind.SERVER);
    assertThat(handle.serviceName()).isEqualTo("sdk-wk-86837b62");
    assertThat(handle.attributes())
        .containsEntry("int.attr", 42L)
        .containsEntry("double.attr", 1.5)
        .containsEntry("bool.attr", true)
        .containsEntry("array.attr", List.of("a", "b"));
    assertThat(handle.events()).hasSize(1);
    assertThat(handle.events().get(0).name()).isEqualTo("cache.miss");
    assertThat(handle.events().get(0).attributes()).containsEntry("cache.key", "k1");
    assertThat(handle.links().get(0).traceId()).isEqualTo("85092f96d876e30b507cfd1361b0325e");
    assertThat(handle.status().code()).isEqualTo(Span.StatusCode.ERROR);
    assertThat(handle.status().message()).contains("boom");
    assertThat(t.spansNamed("db.query").get(0).kind()).isEqualTo(Span.Kind.INTERNAL);
    String u = uri(0);
    assertThat(u)
        .startsWith("/api/v3/traces/" + ID + "?")
        .contains("startTime=2026-11-11T21:00:00Z")
        .contains("endTime=2026-11-18T21:00:00Z")
        .doesNotContain("rawTraces");
    assertThat(server.requests().get(0).header("Authorization")).isEqualTo("Bearer qtok-1234567");
  }

  @Test
  void getTraceOptionsAndShortId() {
    Fixtures.enqueue(server, "getTrace.v3_ok");
    server.enqueue(200, "{\"result\":{\"resourceSpans\":[]}}");
    Instant s = NOW.minusSeconds(3600);
    assertThat(q.getTrace(ID, GetTraceOptions.window(s, NOW).withRawTraces(true))).isPresent();
    assertThat(uri(0))
        .contains("startTime=2026-11-18T20:00:00Z")
        .contains("endTime=2026-11-18T21:00:00Z")
        .contains("rawTraces=true");
    assertThat(q.getTraceAsync("b94006dad19af3af").join()).isEmpty();
    assertThat(uri(1)).startsWith("/api/v3/traces/0000000000000000b94006dad19af3af?");
    assertThatThrownBy(() -> q.getTrace(ID, GetTraceOptions.window(NOW, s)))
        .isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getTrace("not-hex")).isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getTrace("00000000000000000000000000000000"))
        .isInstanceOf(ValidationException.class);
    assertThat(server.requests()).hasSize(2);
    GetTraceOptions d = new GetTraceOptions(null, null, null);
    assertThat(d).isEqualTo(GetTraceOptions.defaults());
  }

  @Test
  void getTraceNotFoundAndErrors() {
    Fixtures.enqueue(server, "getTrace.v3_not_found");
    Fixtures.enqueue(server, "getTrace.v3_invalid_id");
    server.enqueue(403, "{\"error\":{\"httpCode\":403,\"message\":\"denied\"}}");
    assertThat(q.getTrace("ffffffffffffffffffffffffffffffff")).isEmpty();
    assertThatThrownBy(() -> q.getTrace(ID))
        .isInstanceOf(ValidationException.class)
        .hasMessageContaining("malformed parameter");
    assertThatThrownBy(() -> q.getTrace(ID)).isInstanceOf(PermissionDeniedException.class);
  }

  @Test
  void getTraceLegacyFallback() {
    route404();
    Fixtures.enqueue(server, "getTraceLegacy.ok");
    Trace t = q.getTrace(ID).orElseThrow();
    assertThat(t.spans()).hasSize(3);
    Span root = t.root().orElseThrow();
    assertThat(root.kind()).isEqualTo(Span.Kind.CLIENT);
    assertThat(root.status().code()).isEqualTo(Span.StatusCode.OK);
    assertThat(root.attributes())
        .containsEntry("order.id", "86837b62")
        .doesNotContainKeys("span.kind", "otel.scope.name", "otel.status_code");
    Span handle = t.spansNamed("handle /orders").get(0);
    assertThat(handle.parentSpanId()).contains("55bc22d6caba571a");
    assertThat(handle.serviceName()).isEqualTo("sdk-wk-86837b62");
    assertThat(handle.status().code()).isEqualTo(Span.StatusCode.ERROR);
    assertThat(handle.status().message()).contains("boom");
    assertThat(handle.links()).hasSize(1);
    assertThat(handle.events().get(0).name()).isEqualTo("cache.miss");
    assertThat(handle.events().get(0).attributes()).containsEntry("cache.key", "k1");
    assertThat(handle.attributes()).containsEntry("int.attr", 42L).doesNotContainKey("error");
    assertThat(uri(1))
        .startsWith("/api/traces/" + ID + "?")
        .contains("start=" + (NOW.minus(Duration.ofDays(7)).toEpochMilli() * 1000))
        .contains("end=" + NOW.toEpochMilli() * 1000);

    route404();
    Fixtures.enqueue(server, "getTraceLegacy.not_found");
    assertThat(q.getTrace(ID)).isEmpty();
    route404();
    route404();
    assertThatThrownBy(() -> q.getTrace(ID, GetTraceOptions.defaults().withRawTraces(false)))
        .isInstanceOf(UnsupportedException.class)
        .hasMessageContaining("/api/traces/" + ID)
        .satisfies(e -> assertThat(((JaegerException) e).httpStatus()).hasValue(404))
        .hasCauseInstanceOf(NotFoundException.class);
    assertThat(uri(5)).contains("raw=false");
  }

  @Test
  void requireTrace() {
    Fixtures.enqueue(server, "getTrace.v3_ok");
    Fixtures.enqueue(server, "getTrace.v3_not_found");
    assertThat(q.requireTrace(ID).traceId()).isEqualTo(ID);
    assertThatThrownBy(() -> q.requireTrace(ID)).isInstanceOf(NotFoundException.class);
  }

  @Test
  void waitForTracePollsUntilFound() {
    Fixtures.enqueue(server, "getTrace.v3_not_found");
    Fixtures.enqueue(server, "getTrace.v3_ok");
    Trace t =
        q.waitForTrace(
            ID, new WaitForTraceOptions(Duration.ofSeconds(5), Duration.ofMillis(10), 3));
    assertThat(t.spans()).hasSize(3);
    assertThat(server.requests()).hasSize(2);
  }

  @Test
  void findTraces() {
    Fixtures.enqueue(server, "findTraces.by_service");
    TraceSearchQuery query =
        TraceSearchQuery.builder()
            .service("sdk-gw-86837b62")
            .operation("GET /orders")
            .tag("order.id", "86837b62")
            .minDurationMicros(1500L)
            .maxDurationMicros(2_000_000L)
            .limit(5)
            .build();
    List<Trace> traces = q.findTraces(query);
    assertThat(traces).isNotEmpty();
    assertThat(traces.get(0).traceId()).isEqualTo(ID);
    assertThat(uri(0))
        .startsWith("/api/v3/traces?")
        .contains("query.serviceName=sdk-gw-86837b62")
        .contains("query.operationName=GET /orders")
        .contains("query.attributes={\"order.id\":\"86837b62\"}")
        .contains("query.durationMin=1500us")
        .contains("query.durationMax=2000000us")
        .contains("query.startTimeMin=2026-11-18T20:00:00Z")
        .contains("query.startTimeMax=2026-11-18T21:00:00Z")
        .contains("query.searchDepth=5");

    Fixtures.enqueue(server, "findTraces.no_match");
    assertThat(q.findTracesAsync(TraceSearchQuery.builder().build()).join()).isEmpty();
    assertThat(uri(1))
        .doesNotContain("serviceName")
        .doesNotContain("attributes")
        .doesNotContain("duration")
        .contains("query.searchDepth=20");

    route404();
    assertThatThrownBy(() -> q.findTraces(TraceSearchQuery.forService("x")))
        .isInstanceOf(UnsupportedException.class);
    Fixtures.enqueue(server, "findTraces.bad_duration");
    assertThatThrownBy(() -> q.findTraces(TraceSearchQuery.forService("x")))
        .isInstanceOf(ValidationException.class);
    Instant future = NOW.plusSeconds(60);
    assertThatThrownBy(() -> q.findTraces(TraceSearchQuery.builder().start(future).build()))
        .isInstanceOf(ValidationException.class);
    assertThatThrownBy(
            () -> q.findTracesAsync(TraceSearchQuery.builder().start(future).build()).join())
        .hasCauseInstanceOf(ValidationException.class);
  }

  @Test
  void searchTraces() {
    Fixtures.enqueue(server, "findTraceSummaries.ok");
    Instant s = NOW.minusSeconds(60);
    List<TraceSummary> sums =
        q.searchTraces(TraceSearchQuery.builder().service("sdk-gw-86837b62").start(s).build());
    TraceSummary first = sums.get(0);
    assertThat(first.traceId()).isEqualTo(ID);
    assertThat(first.rootServiceName()).contains("sdk-gw-86837b62");
    assertThat(first.rootName()).contains("GET /orders");
    assertThat(first.spanCount()).isEqualTo(3);
    assertThat(first.durationMicros()).isEqualTo(1_200_000);
    assertThat(first.startMicros()).isEqualTo(1795034918142512L);
    assertThat(sums.get(1).traceId()).isEqualTo("0000000000000000b94006dad19af3af");
    assertThat(uri(0))
        .startsWith("/api/v3/trace-summaries?")
        .contains("query.startTimeMin=2026-11-18T20:59:00Z");
    Fixtures.enqueue(server, "findTraceSummaries.empty");
    assertThat(q.searchTracesAsync(TraceSearchQuery.builder().build()).join()).isEmpty();
    Fixtures.enqueue(server, "findTraces.no_match");
    assertThat(q.searchTraces(TraceSearchQuery.builder().build())).isEmpty();
    route404();
    assertThatThrownBy(() -> q.searchTraces(TraceSearchQuery.builder().build()))
        .isInstanceOf(UnsupportedException.class);
    Instant future = NOW.plusSeconds(60);
    assertThatThrownBy(() -> q.searchTraces(TraceSearchQuery.builder().start(future).build()))
        .isInstanceOf(ValidationException.class);
  }

  @Test
  void listServicesSorted() {
    Fixtures.enqueue(server, "listServices.ok");
    List<String> services = q.listServices();
    assertThat(services).isSorted().contains("jaeger", "grow-bootstrap").hasSize(10);
    server.enqueue(200, "{\"services\":[\"b\",1,\"a\"]}");
    assertThat(q.listServicesAsync().join()).containsExactly("a", "b");
    route404();
    assertThatThrownBy(q::listServices).isInstanceOf(UnsupportedException.class);
    server.enqueue(404, "{\"error\":{\"httpCode\":404,\"message\":\"nope\"}}");
    assertThatThrownBy(q::listServices).isInstanceOf(NotFoundException.class);
  }

  @Test
  void operations() {
    Fixtures.enqueue(server, "listOperations.ok");
    assertThat(q.getOperations("sdk-wk-86837b62"))
        .containsExactly(
            new JaegerOperation("handle /orders", "server"),
            new JaegerOperation("db.query", "internal"));
    assertThat(uri(0)).isEqualTo("/api/v3/operations?service=sdk-wk-86837b62");

    Fixtures.enqueue(server, "listOperations.span_kind_server");
    assertThat(q.getOperations("sdk-wk-86837b62", "SERVER")).hasSize(1);
    assertThat(uri(1)).contains("spanKind=server").contains("service=sdk-wk-86837b62");

    server.enqueue(
        200,
        "{\"operations\":[{\"name\":\"b\",\"spanKind\":\"server\"},{\"name\":\"a\"},"
            + "{\"name\":\"b\",\"spanKind\":\"client\"},{\"spanKind\":\"x\"}]}");
    assertThat(q.listOperations("svc")).containsExactly("a", "b");
    server.enqueue(200, "{\"operations\":[{\"name\":\"z\",\"spanKind\":\"client\"}]}");
    assertThat(q.listOperations("svc", Span.Kind.CLIENT)).containsExactly("z");
    assertThat(uri(3)).contains("spanKind=client");
    server.enqueue(200, "{\"operations\":[]}");
    assertThat(q.getOperations("svc", Span.Kind.PRODUCER)).isEmpty();
    server.enqueue(200, "{}");
    assertThat(q.listOperationsAsync("svc").join()).isEmpty();
    server.enqueue(200, "{\"operations\":[{\"name\":\"y\"}]}");
    assertThat(q.listOperations("svc", "Internal")).containsExactly("y");
    assertThat(new JaegerOperation("n", null).spanKind()).isEmpty();

    int before = server.requests().size();
    assertThatThrownBy(() -> q.getOperations(" ")).isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.listOperations(null)).isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getOperations("svc", "weird"))
        .isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getOperations("svc", Span.Kind.UNSPECIFIED))
        .isInstanceOf(ValidationException.class);
    assertThat(q.listOperations("svc", (Span.Kind) null)).isEmpty();
    assertThat(server.requests()).hasSize(before + 1);
  }

  @Test
  void dependencies() {
    Fixtures.enqueue(server, "getDependencies.ok");
    List<DependencyLink> links = q.getDependencies(NOW.minusSeconds(3600), NOW);
    assertThat(links)
        .contains(new DependencyLink("sdk-gw-86837b62", "sdk-wk-86837b62", 1))
        .hasSize(2);
    assertThat(uri(0))
        .startsWith("/api/dependencies?")
        .contains("endTs=" + NOW.toEpochMilli())
        .contains("lookback=3600000");

    server.enqueue(200, "{\"data\":[]}");
    assertThat(q.getDependencies(null, (Instant) null)).isEmpty();
    assertThat(uri(1)).contains("endTs=" + NOW.toEpochMilli()).contains("lookback=86400000");

    server.enqueue(
        200, "{\"data\":[{\"parent\":\"a\",\"child\":\"b\",\"callCount\":3},{\"parent\":\"x\"}]}");
    assertThat(q.getDependencies(NOW, Duration.ofMinutes(5), "a"))
        .containsExactly(new DependencyLink("a", "b", 3));
    assertThat(uri(2)).contains("lookback=300000").contains("service=a");

    server.enqueue(200, "{\"data\":null}");
    assertThat(q.getDependencies(null, (Duration) null)).isEmpty();
    server.enqueue(200, "{\"data\":[]}");
    assertThat(q.getDependenciesAsync(NOW.minusSeconds(10), null).join()).isEmpty();
    server.enqueue(200, "{\"data\":[]}");
    assertThat(q.getDependencies(NOW, Duration.ofMinutes(1), "")).isEmpty();
    assertThat(uri(5)).doesNotContain("service");

    assertThatThrownBy(() -> q.getDependencies(NOW, Duration.ZERO))
        .isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getDependencies(NOW, Duration.ofNanos(10)))
        .isInstanceOf(ValidationException.class);
    assertThatThrownBy(() -> q.getDependencies(NOW.plusSeconds(5), NOW))
        .isInstanceOf(ValidationException.class);
    Fixtures.enqueue(server, "getDependencies.bad_lookback");
    assertThatThrownBy(() -> q.getDependencies(NOW, Duration.ofMinutes(1)))
        .isInstanceOf(ValidationException.class)
        .hasMessageContaining("positive duration");
    Fixtures.enqueue(server, "getDependencies.v3_route_missing");
    assertThatThrownBy(() -> q.getDependencies(NOW, Duration.ofMinutes(1)))
        .isInstanceOf(UnsupportedException.class);
  }

  @Test
  void retriesServerErrorsAndCarriesRequestId() {
    server
        .enqueue(503, "busy", "Content-Type", "text/plain")
        .enqueue(200, "{\"services\":[\"a\"]}");
    assertThat(q.listServices()).containsExactly("a");
    assertThat(server.requests()).hasSize(2);
    server.enqueue(
        404,
        "{\"error\":{\"httpCode\":404,\"message\":\"qtok-1234567 missing\"}}",
        "traceresponse",
        "00-11b16b09ed8dc189225e418444d52b57-e934437bf8f19de2-01");
    assertThatThrownBy(q::listServices)
        .isInstanceOf(NotFoundException.class)
        .satisfies(
            e ->
                assertThat(((JaegerException) e).requestId())
                    .contains("00-11b16b09ed8dc189225e418444d52b57-e934437bf8f19de2-01"))
        .message()
        .doesNotContain("qtok-1234567");
  }

  @Test
  void envelopeDetection() {
    assertThat(JaegerQueryClient.isEnvelope("{\"error\":{}}".getBytes(StandardCharsets.UTF_8)))
        .isTrue();
    assertThat(JaegerQueryClient.isEnvelope("{\"errors\":[]}".getBytes(StandardCharsets.UTF_8)))
        .isTrue();
    assertThat(JaegerQueryClient.isEnvelope("{\"error\":\"x\"}".getBytes(StandardCharsets.UTF_8)))
        .isFalse();
    assertThat(JaegerQueryClient.isEnvelope("[1]".getBytes(StandardCharsets.UTF_8))).isFalse();
    assertThat(JaegerQueryClient.isEnvelope(ROUTE_404.getBytes(StandardCharsets.UTF_8))).isFalse();
    assertThat(JaegerQueryClient.isEnvelope(new byte[0])).isFalse();
  }

  @Test
  void standaloneLifecycle() throws Exception {
    Map<String, String> env = Map.of(EnvReader.DEFAULT_PREFIX + "URL", server.url());
    try (JaegerQueryClient own =
        JaegerQueryClient.create(JaegerConfig.builder().fromEnv(EnvReader.of(env)).build())) {
      server.enqueue(200, "{\"services\":[]}");
      assertThat(own.listServices()).isEmpty();
      assertThat(server.requests().get(0).header("Authorization")).isNull();
    }
    try (JaegerQueryClient fromEnv = JaegerQueryClient.fromEnv()) {
      assertThat(fromEnv.toString()).contains("JaegerQueryClient");
    }
    q.close();
    assertThatThrownBy(q::listServices).isInstanceOf(CancelledException.class);
    assertThatThrownBy(() -> q.getTraceAsync(ID).join())
        .isInstanceOf(CompletionException.class)
        .hasCauseInstanceOf(CancelledException.class);
    assertThat(Optional.of(q)).isPresent();
  }
}
