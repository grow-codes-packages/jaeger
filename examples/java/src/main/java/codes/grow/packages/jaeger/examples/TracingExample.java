package codes.grow.packages.jaeger.examples;

import codes.grow.packages.jaeger.JaegerClient;
import codes.grow.packages.jaeger.JaegerOperation;
import codes.grow.packages.jaeger.JaegerSamplingStrategy;
import codes.grow.packages.jaeger.JaegerTracing;
import codes.grow.packages.jaeger.JaegerTracingHandle;
import codes.grow.packages.jaeger.SamplingPreset;
import codes.grow.packages.jaeger.tracing.SpanOptions;
import codes.grow.packages.jaeger.tracing.Spans;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.query.DependencyLink;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceSearchQuery;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import io.opentelemetry.api.trace.Span;
import io.opentelemetry.api.trace.SpanKind;
import io.opentelemetry.api.trace.Tracer;
import io.opentelemetry.context.Scope;
import java.time.Instant;
import java.util.List;
import java.util.Map;

/**
 * Tracing round trip: export spans to Jaeger over OTLP, read them back through the query API,
 * list services and operations, show the dependency graph and the remote sampling strategy.
 *
 * <p>Uses {@code GROW_JAEGER_URL} (query, 16686), {@code GROW_JAEGER_OTLP_HTTP_ENDPOINT} or the
 * {@code OTEL_EXPORTER_OTLP_*} variables (export, default {@code http://127.0.0.1:4318}) and
 * {@code GROW_JAEGER_SAMPLING_URL} (remote sampling, default {@code http://127.0.0.1:5778}).
 */
public final class TracingExample {
  private TracingExample() {}

  /**
   * Runs the example.
   *
   * @param args unused
   */
  public static void main(String[] args) {
    String service = "java-example-" + Long.toHexString(System.nanoTime());
    try (JaegerClient client = JaegerClient.fromEnv()) {
      TracingConfig config =
          SamplingPreset.development()
              .applyTo(TracingConfig.builder(service).serviceVersion("1.0.0").setGlobal(false))
              .build();
      JaegerTracingHandle tracing = client.initTracing(config); // shut down by client.close()
      System.out.println("exporting to " + tracing.endpoint());
      Tracer tracer = tracing.tracer("example");

      Span root = tracer.spanBuilder("GET /orders").setSpanKind(SpanKind.SERVER).startSpan();
      Scope scope = root.makeCurrent();
      try {
        Spans.withSpan(
            "db.query",
            SpanOptions.builder().tracer(tracer).kind(SpanKind.CLIENT).build(),
            () -> Spans.addEvent("cache.miss", Map.of("cache.key", "orders")));
        System.out.println("log fields: " + Spans.traceFields());
      } finally {
        scope.close();
        root.end();
      }
      tracing.forceFlush();

      String traceId = root.getSpanContext().getTraceId();
      Trace trace = client.query().waitForTrace(traceId);
      System.out.println("trace " + trace.traceId() + " has " + trace.spans().size() + " spans");
      for (var s : trace.spans()) {
        System.out.println("  " + s.name() + " kind=" + s.kind() + " " + s.durationMicros() + " us");
      }
      List<Trace> found = client.query().findTraces(TraceSearchQuery.forService(service));
      List<TraceSummary> summaries = client.query().searchTraces(TraceSearchQuery.forService(service));
      System.out.println("findTraces: " + found.size() + ", searchTraces: " + summaries);
      System.out.println("services: " + client.query().listServices());
      List<JaegerOperation> ops = client.query().getOperations(service, "server");
      System.out.println("server operations: " + ops);
      List<DependencyLink> links =
          client.query().getDependencies(Instant.now().minusSeconds(3600), Instant.now());
      System.out.println("dependencies: " + links);
      JaegerSamplingStrategy strategy = client.samplingStrategy(service);
      System.out.println("sampling strategy: " + strategy);
      System.out.println("env-based tracing config: " + JaegerTracing.configFromEnv(service));
    }
  }
}
