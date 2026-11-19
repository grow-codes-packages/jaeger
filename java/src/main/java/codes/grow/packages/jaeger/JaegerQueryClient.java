package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.ErrorCode;
import codes.grow.packages.jaeger.core.HttpTransport;
import codes.grow.packages.jaeger.core.JaegerException;
import codes.grow.packages.jaeger.core.Json;
import codes.grow.packages.jaeger.core.NotFoundException;
import codes.grow.packages.jaeger.core.Secret;
import codes.grow.packages.jaeger.core.TransportRequest;
import codes.grow.packages.jaeger.core.TransportResponse;
import codes.grow.packages.jaeger.core.UnsupportedException;
import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.query.DependencyLink;
import codes.grow.packages.jaeger.tracing.query.Span;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceIds;
import codes.grow.packages.jaeger.tracing.query.TraceQueryClient;
import codes.grow.packages.jaeger.tracing.query.TraceSearchQuery;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import com.fasterxml.jackson.databind.JsonNode;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import java.util.TreeSet;
import java.util.concurrent.CompletableFuture;
import java.util.function.Supplier;

/**
 * Reads traces back from Jaeger's query service (port 16686): the v3 HTTP API ({@code /api/v3/*})
 * for traces, search, services and operations, the legacy {@code /api/traces/{id}} as a fallback
 * for {@link #getTrace} when the v3 route is missing, and the legacy {@code /api/dependencies}.
 * Every call is a GET (idempotent, so 5xx responses are retried).
 *
 * <p>Get one from {@link JaegerClient#query()} (shares the client's connections) or create a
 * standalone one with {@link #create(JaegerConfig)} / {@link #fromEnv()} (owns its connections;
 * close it). Every method has a blocking form and an {@code …Async} form returning {@link
 * CompletableFuture}. Thread-safe.
 */
public final class JaegerQueryClient implements TraceQueryClient, AutoCloseable {
  /** Default {@link #getTrace} lookup window. */
  public static final Duration DEFAULT_GET_TRACE_WINDOW = Duration.ofDays(7);

  /** Default search window ({@code end - start}) when the query sets no start. */
  public static final Duration DEFAULT_SEARCH_WINDOW = Duration.ofHours(1);

  /** Default dependency lookback. */
  public static final Duration DEFAULT_DEPENDENCY_LOOKBACK = Duration.ofHours(24);

  /** Span kinds accepted by the operation filter. */
  static final Set<String> SPAN_KINDS =
      Set.of("server", "client", "producer", "consumer", "internal");

  private final HttpTransport transport;
  private final Supplier<Optional<Secret>> token;
  private final boolean owned;
  private volatile Clock clock = Clock.systemUTC();

  JaegerQueryClient(HttpTransport transport, Supplier<Optional<Secret>> token, boolean owned) {
    this.transport = transport;
    this.token = token;
    this.owned = owned;
  }

  /**
   * Creates a standalone query client with its own connections.
   *
   * @param config configuration ({@code url} = query API base URL)
   * @return a new query client; close it when done
   */
  public static JaegerQueryClient create(JaegerConfig config) {
    config.validate();
    return new JaegerQueryClient(
        new HttpTransport(config.toTransportConfig()), config::token, true);
  }

  /**
   * Creates a standalone query client from {@code GROW_JAEGER_URL} / {@code JAEGER_QUERY_URL}.
   *
   * @return a new query client
   */
  public static JaegerQueryClient fromEnv() {
    return create(JaegerConfig.fromEnv());
  }

  /** Replaces the clock used for default time windows (tests). */
  void clock(Clock clock) {
    this.clock = clock;
  }

  // ---------------------------------------------------------------------------------------------
  // HTTP helpers

  private CompletableFuture<TransportResponse> get(
      String path, Map<String, Object> query, String operation) {
    TransportRequest.Builder b = TransportRequest.get(path).operation(operation).acceptStatus(404);
    query.forEach(
        (k, v) -> {
          if (v != null) {
            b.query(k, v);
          }
        });
    token.get().ifPresent(t -> b.header("Authorization", "Bearer " + t.reveal()));
    return transport.sendAsync(b.build());
  }

  /** True when a 404 body is one of Jaeger's JSON error envelopes (v3 or legacy). */
  static boolean isEnvelope(byte[] body) {
    try {
      JsonNode node = Json.tree(body);
      return node.isObject() && (node.path("error").isObject() || node.path("errors").isArray());
    } catch (ValidationException e) {
      return false;
    }
  }

  private static String requestId(TransportResponse r) {
    return r.requestId().or(() -> r.header("traceresponse")).orElse(null);
  }

  private JaegerException notFound(TransportResponse r, String operation) {
    List<String> secrets = new ArrayList<>();
    token.get().filter(t -> !t.isCleared()).ifPresent(t -> secrets.add(t.reveal()));
    String message = HttpTransport.errorMessage(operation, 404, r.headers(), r.body(), secrets);
    return JaegerException.of(ErrorCode.NOT_FOUND, message, 404, requestId(r), null, null);
  }

  /**
   * Maps a 404: the JSON envelope means "no such item" ({@link NotFoundException}); any other body
   * (Go's {@code 404 page not found}, a proxy page) means the route is missing ({@link
   * UnsupportedException}).
   */
  private JaegerException missing(TransportResponse r, String path, String operation) {
    JaegerException nf = notFound(r, operation);
    if (isEnvelope(r.body())) {
      return nf;
    }
    return new UnsupportedException(
        operation + ": route " + path + " is not available on this server",
        404,
        requestId(r),
        null,
        nf);
  }

  private static <T> CompletableFuture<T> failed(Throwable t) {
    return CompletableFuture.failedFuture(t);
  }

  private static String rfc3339(Instant instant) {
    return instant.toString();
  }

  // ---------------------------------------------------------------------------------------------
  // getTrace

  /**
   * Fetches one trace: {@code GET /api/v3/traces/{id}} (window: the last 7 days), falling back to
   * the legacy {@code GET /api/traces/{id}} when the v3 route is missing.
   *
   * @param traceId 32 or 16 hex characters (16 is left-padded)
   * @return the trace, or empty when Jaeger does not know it
   * @throws ValidationException for a malformed id (no request is sent)
   */
  @Override
  public Optional<Trace> getTrace(String traceId) {
    return getTrace(traceId, GetTraceOptions.defaults());
  }

  /**
   * Fetches one trace with an explicit lookup window and raw flag.
   *
   * @param traceId trace id
   * @param options window and {@code rawTraces}
   * @return the trace, or empty when Jaeger does not know it
   */
  public Optional<Trace> getTrace(String traceId, GetTraceOptions options) {
    return HttpTransport.await(getTraceAsync(traceId, options), "jaeger.getTrace");
  }

  @Override
  public CompletableFuture<Optional<Trace>> getTraceAsync(String traceId) {
    return getTraceAsync(traceId, GetTraceOptions.defaults());
  }

  /**
   * Asynchronous {@link #getTrace(String, GetTraceOptions)}.
   *
   * @param traceId trace id
   * @param options window and {@code rawTraces}
   * @return future trace
   */
  public CompletableFuture<Optional<Trace>> getTraceAsync(String traceId, GetTraceOptions options) {
    String id;
    Instant start;
    Instant end;
    try {
      id = TraceIds.normalizeTraceId(traceId);
      end = options.end().orElseGet(clock::instant);
      start = options.start().orElse(end.minus(DEFAULT_GET_TRACE_WINDOW));
      if (start.isAfter(end)) {
        throw new ValidationException("jaeger.getTrace: start must not be after end");
      }
    } catch (JaegerException e) {
      return failed(e);
    }
    String operation = "jaeger.getTrace " + id;
    String path = "/api/v3/traces/" + id;
    Map<String, Object> q = new LinkedHashMap<>();
    q.put("startTime", rfc3339(start));
    q.put("endTime", rfc3339(end));
    q.put("rawTraces", options.rawTraces().orElse(null));
    return get(path, q, operation)
        .thenCompose(
            r -> {
              if (r.status() != 404) {
                return CompletableFuture.completedFuture(
                    pick(JaegerModel.otlpTraces(r.jsonNode().get("result")), id));
              }
              if (isEnvelope(r.body())) {
                return CompletableFuture.completedFuture(Optional.<Trace>empty());
              }
              return legacyTrace(id, start, end, options);
            });
  }

  private CompletableFuture<Optional<Trace>> legacyTrace(
      String id, Instant start, Instant end, GetTraceOptions options) {
    String operation = "jaeger.getTrace " + id + " (legacy)";
    String path = "/api/traces/" + id;
    Map<String, Object> q = new LinkedHashMap<>();
    q.put("start", TraceSearchQuery.toMicros(start));
    q.put("end", TraceSearchQuery.toMicros(end));
    q.put("raw", options.rawTraces().orElse(null));
    return get(path, q, operation)
        .thenApply(
            r -> {
              if (r.status() == 404) {
                JaegerException e = missing(r, path, operation);
                if (e instanceof NotFoundException) {
                  return Optional.empty();
                }
                throw e;
              }
              return pick(JaegerModel.legacyTraces(r.jsonNode()), id);
            });
  }

  private static Optional<Trace> pick(List<Trace> traces, String id) {
    Trace match = null;
    for (Trace t : traces) {
      if (t.traceId().equals(id)) {
        match = t;
        break;
      }
    }
    if (match == null && !traces.isEmpty()) {
      match = traces.get(0);
    }
    return match == null || match.spans().isEmpty() ? Optional.empty() : Optional.of(match);
  }

  /**
   * Like {@link #getTrace(String)} but throws when the trace does not exist.
   *
   * @param traceId trace id
   * @return the trace
   * @throws NotFoundException when Jaeger does not know the trace
   */
  public Trace requireTrace(String traceId) {
    return getTrace(traceId)
        .orElseThrow(
            () ->
                new NotFoundException(
                    "jaeger.getTrace " + TraceIds.normalizeTraceId(traceId) + ": trace not found"));
  }

  // ---------------------------------------------------------------------------------------------
  // Search

  private Map<String, Object> searchParams(TraceSearchQuery query, String operation) {
    Instant end = query.end().orElseGet(clock::instant);
    Instant start = query.start().orElse(end.minus(DEFAULT_SEARCH_WINDOW));
    if (start.isAfter(end)) {
      throw new ValidationException(operation + ": start must not be after end");
    }
    Map<String, Object> q = new LinkedHashMap<>();
    q.put("query.serviceName", query.service().filter(s -> !s.isEmpty()).orElse(null));
    q.put("query.operationName", query.operation().filter(s -> !s.isEmpty()).orElse(null));
    q.put("query.attributes", query.tags().isEmpty() ? null : Json.stringify(query.tags()));
    if (query.minDurationMicros().isPresent()) {
      q.put("query.durationMin", query.minDurationMicros().getAsLong() + "us");
    }
    if (query.maxDurationMicros().isPresent()) {
      q.put("query.durationMax", query.maxDurationMicros().getAsLong() + "us");
    }
    q.put("query.startTimeMin", rfc3339(start));
    q.put("query.startTimeMax", rfc3339(end));
    q.put("query.searchDepth", query.limit());
    return q;
  }

  /**
   * Searches full traces: {@code GET /api/v3/traces}. Both time bounds are always sent (default:
   * the last hour); {@code limit} (default 20) caps the number of traces. No match returns an empty
   * list.
   *
   * @param query filter
   * @return matching traces with all their spans
   */
  public List<Trace> findTraces(TraceSearchQuery query) {
    return HttpTransport.await(findTracesAsync(query), "jaeger.findTraces");
  }

  /**
   * Asynchronous {@link #findTraces}.
   *
   * @param query filter
   * @return future traces
   */
  public CompletableFuture<List<Trace>> findTracesAsync(TraceSearchQuery query) {
    String operation = "jaeger.findTraces";
    String path = "/api/v3/traces";
    Map<String, Object> q;
    try {
      q = searchParams(query, operation);
    } catch (JaegerException e) {
      return failed(e);
    }
    return get(path, q, operation)
        .thenApply(
            r -> {
              if (r.status() == 404) {
                JaegerException e = missing(r, path, operation);
                if (e instanceof NotFoundException) {
                  return List.<Trace>of();
                }
                throw e;
              }
              return JaegerModel.otlpTraces(r.jsonNode().get("result"));
            });
  }

  /**
   * Searches trace summaries: {@code GET /api/v3/trace-summaries} (same filter as {@link
   * #findTraces}).
   *
   * @param query filter
   * @return one summary per matching trace
   */
  @Override
  public List<TraceSummary> searchTraces(TraceSearchQuery query) {
    return HttpTransport.await(searchTracesAsync(query), "jaeger.searchTraces");
  }

  @Override
  public CompletableFuture<List<TraceSummary>> searchTracesAsync(TraceSearchQuery query) {
    String operation = "jaeger.searchTraces";
    String path = "/api/v3/trace-summaries";
    Map<String, Object> q;
    try {
      q = searchParams(query, operation);
    } catch (JaegerException e) {
      return failed(e);
    }
    return get(path, q, operation)
        .thenApply(
            r -> {
              if (r.status() == 404) {
                JaegerException e = missing(r, path, operation);
                if (e instanceof NotFoundException) {
                  return List.<TraceSummary>of();
                }
                throw e;
              }
              return JaegerModel.summaries(r.jsonNode());
            });
  }

  private JsonNode okJson(TransportResponse r, String path, String operation) {
    if (r.status() == 404) {
      throw missing(r, path, operation);
    }
    return r.jsonNode();
  }

  // ---------------------------------------------------------------------------------------------
  // Services and operations

  /**
   * Service names that reported spans: {@code GET /api/v3/services}, sorted. Jaeger also lists
   * itself ({@code jaeger}).
   *
   * @return service names
   */
  @Override
  public List<String> listServices() {
    return HttpTransport.await(listServicesAsync(), "jaeger.listServices");
  }

  @Override
  public CompletableFuture<List<String>> listServicesAsync() {
    String operation = "jaeger.listServices";
    String path = "/api/v3/services";
    return get(path, Map.of(), operation)
        .thenApply(
            r -> {
              List<String> out = new ArrayList<>();
              for (JsonNode s : okJson(r, path, operation).path("services")) {
                if (s.isTextual()) {
                  out.add(s.asText());
                }
              }
              out.sort(null);
              return out;
            });
  }

  /**
   * Operations of a service with their span kind: {@code GET /api/v3/operations}.
   *
   * @param service service name (non-blank)
   * @return operations in server order
   * @throws ValidationException for a blank service (no request is sent)
   */
  public List<JaegerOperation> getOperations(String service) {
    return getOperations(service, (String) null);
  }

  /**
   * Operations of a service filtered by span kind (sent lower-case, as Jaeger requires).
   *
   * @param service service name (non-blank)
   * @param spanKind {@code server}, {@code client}, {@code producer}, {@code consumer} or {@code
   *     internal} (case-insensitive), or {@code null} for all
   * @return operations
   * @throws ValidationException for a blank service or an unknown span kind
   */
  public List<JaegerOperation> getOperations(String service, String spanKind) {
    return HttpTransport.await(getOperationsAsync(service, spanKind), "jaeger.listOperations");
  }

  /**
   * Operations of a service filtered by span kind.
   *
   * @param service service name
   * @param spanKind span kind ({@link Span.Kind#UNSPECIFIED} is rejected)
   * @return operations
   */
  public List<JaegerOperation> getOperations(String service, Span.Kind spanKind) {
    return getOperations(service, spanKind == null ? null : spanKind.wireValue());
  }

  /**
   * Asynchronous {@link #getOperations(String, String)}.
   *
   * @param service service name
   * @param spanKind span kind or {@code null}
   * @return future operations
   */
  public CompletableFuture<List<JaegerOperation>> getOperationsAsync(
      String service, String spanKind) {
    String operation = "jaeger.listOperations";
    String path = "/api/v3/operations";
    if (service == null || service.isBlank()) {
      return failed(new ValidationException(operation + ": service is required"));
    }
    String kind = spanKind == null ? null : spanKind.trim().toLowerCase(Locale.ROOT);
    if (kind != null && !SPAN_KINDS.contains(kind)) {
      return failed(new ValidationException(operation + ": unknown spanKind"));
    }
    Map<String, Object> q = new LinkedHashMap<>();
    q.put("service", service);
    q.put("spanKind", kind);
    return get(path, q, operation)
        .thenApply(r -> JaegerModel.operations(okJson(r, path, operation)));
  }

  /**
   * Operation names of a service, de-duplicated across span kinds and sorted.
   *
   * @param service service name
   * @return operation names
   */
  @Override
  public List<String> listOperations(String service) {
    return listOperations(service, (String) null);
  }

  /**
   * Operation names of a service with one span kind, de-duplicated and sorted.
   *
   * @param service service name
   * @param spanKind span kind (case-insensitive) or {@code null}
   * @return operation names
   */
  public List<String> listOperations(String service, String spanKind) {
    return HttpTransport.await(listOperationsAsync(service, spanKind), "jaeger.listOperations");
  }

  /**
   * Operation names of a service with one span kind.
   *
   * @param service service name
   * @param spanKind span kind
   * @return operation names
   */
  public List<String> listOperations(String service, Span.Kind spanKind) {
    return listOperations(service, spanKind == null ? null : spanKind.wireValue());
  }

  @Override
  public CompletableFuture<List<String>> listOperationsAsync(String service) {
    return listOperationsAsync(service, null);
  }

  /**
   * Asynchronous {@link #listOperations(String, String)}.
   *
   * @param service service name
   * @param spanKind span kind or {@code null}
   * @return future names
   */
  public CompletableFuture<List<String>> listOperationsAsync(String service, String spanKind) {
    return getOperationsAsync(service, spanKind)
        .thenApply(
            ops -> {
              TreeSet<String> names = new TreeSet<>();
              ops.forEach(o -> names.add(o.name()));
              return List.copyOf(names);
            });
  }

  // ---------------------------------------------------------------------------------------------
  // Dependencies

  /**
   * Service dependency edges in a time range (category form): {@code endTs = end ?? now}, {@code
   * lookback = endTs - (start ?? endTs - 24h)}.
   *
   * @param start range start or {@code null}
   * @param end range end or {@code null}
   * @return links
   */
  @Override
  public List<DependencyLink> getDependencies(Instant start, Instant end) {
    return HttpTransport.await(getDependenciesAsync(start, end), "jaeger.getDependencies");
  }

  @Override
  public CompletableFuture<List<DependencyLink>> getDependenciesAsync(Instant start, Instant end) {
    Instant endTs = end != null ? end : clock.instant();
    Duration lookback =
        start == null ? DEFAULT_DEPENDENCY_LOOKBACK : Duration.between(start, endTs);
    return getDependenciesAsync(endTs, lookback, null);
  }

  /**
   * Service dependency edges: legacy {@code GET /api/dependencies?endTs=&lookback=}. The memory
   * store derives them on the fly; Cassandra/Elasticsearch/OpenSearch need the {@code
   * spark-dependencies} job, so an empty list is normal there.
   *
   * @param endTs end of the window ({@code null} = now)
   * @param lookback window length ({@code null} = 24 h), must be positive
   * @return links
   */
  public List<DependencyLink> getDependencies(Instant endTs, Duration lookback) {
    return getDependencies(endTs, lookback, null);
  }

  /**
   * Service dependency edges that touch one service.
   *
   * @param endTs end of the window ({@code null} = now)
   * @param lookback window length ({@code null} = 24 h)
   * @param service keep only edges touching this service ({@code null} = all)
   * @return links
   */
  public List<DependencyLink> getDependencies(Instant endTs, Duration lookback, String service) {
    return HttpTransport.await(
        getDependenciesAsync(endTs, lookback, service), "jaeger.getDependencies");
  }

  /**
   * Asynchronous {@link #getDependencies(Instant, Duration, String)}.
   *
   * @param endTs end of the window
   * @param lookback window length
   * @param service service filter
   * @return future links
   */
  public CompletableFuture<List<DependencyLink>> getDependenciesAsync(
      Instant endTs, Duration lookback, String service) {
    String operation = "jaeger.getDependencies";
    String path = "/api/dependencies";
    Instant e = endTs != null ? endTs : clock.instant();
    Duration lb = lookback != null ? lookback : DEFAULT_DEPENDENCY_LOOKBACK;
    if (lb.isZero() || lb.isNegative() || lb.toMillis() <= 0) {
      return failed(new ValidationException(operation + ": lookback must be a positive duration"));
    }
    Map<String, Object> q = new LinkedHashMap<>();
    q.put("endTs", e.toEpochMilli());
    q.put("lookback", lb.toMillis());
    q.put("service", service == null || service.isEmpty() ? null : service);
    return get(path, q, operation)
        .thenApply(r -> JaegerModel.dependencies(okJson(r, path, operation)));
  }

  /** Releases connections when this client owns them (no-op for {@code client.query()}). */
  @Override
  public void close() {
    if (owned) {
      transport.close();
    }
  }

  @Override
  public String toString() {
    return "JaegerQueryClient{"
        + codes.grow.packages.jaeger.core.Redaction.redactUrl(transport.config().baseUrl())
        + "}";
  }
}
