package codes.grow.packages.jaeger.tracing;

import codes.grow.packages.jaeger.core.ConfigException;
import io.opentelemetry.api.GlobalOpenTelemetry;
import io.opentelemetry.context.propagation.ContextPropagators;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.trace.SdkTracerProvider;
import io.opentelemetry.sdk.trace.export.BatchSpanProcessor;
import io.opentelemetry.sdk.trace.export.SpanExporter;
import io.opentelemetry.sdk.trace.samplers.Sampler;
import java.util.Map;
import java.util.Objects;

/**
 * Service-owned companion of {@link Tracing#initWith}: the same setup with a caller-supplied head
 * {@link Sampler} (for example the Jaeger remote sampler), which {@link TracingConfig} cannot
 * express. It lives in this package because {@link TracingHandle}'s constructor is package-private.
 *
 * <p>Difference to {@link Tracing#initWith}: the handle is not registered as the "current" handle
 * of the span helpers ({@link Spans}); they use it through the global OpenTelemetry when {@code
 * setGlobal} is true, otherwise pass the handle's tracer explicitly.
 */
public final class SamplerTracing {
  private SamplerTracing() {}

  /**
   * Builds and starts a tracer provider like {@link Tracing#initWith(TracingConfig,
   * SpanExporterFactory)} but with {@code sampler} instead of {@code config.sampler()}.
   *
   * @param config tracing config
   * @param exporterFactory creates the span exporter
   * @param sampler head sampler
   * @return handle
   * @throws ConfigException for unknown propagators or when a global OpenTelemetry already exists
   */
  public static TracingHandle initWith(
      TracingConfig config, SpanExporterFactory exporterFactory, Sampler sampler) {
    Objects.requireNonNull(config, "config");
    Objects.requireNonNull(exporterFactory, "exporterFactory");
    Objects.requireNonNull(sampler, "sampler");
    ContextPropagators propagators = Tracing.propagators(config.propagators(), Map.of());
    SpanExporter exporter = exporterFactory.create(config);
    TracingConfig.BatchSettings batch = config.batch();
    SdkTracerProvider provider =
        SdkTracerProvider.builder()
            .setResource(Tracing.resource(config))
            .setSampler(sampler)
            .addSpanProcessor(
                BatchSpanProcessor.builder(exporter)
                    .setMaxQueueSize(batch.maxQueueSize())
                    .setMaxExportBatchSize(batch.maxExportBatchSize())
                    .setScheduleDelay(batch.scheduledDelay())
                    .setExporterTimeout(batch.exportTimeout())
                    .build())
            .build();
    OpenTelemetrySdk sdk =
        OpenTelemetrySdk.builder().setTracerProvider(provider).setPropagators(propagators).build();
    if (config.setGlobal()) {
      try {
        GlobalOpenTelemetry.set(sdk);
      } catch (IllegalStateException e) {
        sdk.shutdown();
        throw new ConfigException(
            "tracing: a global OpenTelemetry is already registered; use setGlobal(false)", e);
      }
    }
    return new TracingHandle(sdk, config);
  }
}
