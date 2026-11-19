package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.TracingHandle;
import io.opentelemetry.api.trace.Tracer;
import io.opentelemetry.context.propagation.ContextPropagators;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.trace.SdkTracerProvider;
import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * A running Jaeger tracing setup (from {@link JaegerTracing#initTracing}): the category {@link
 * TracingHandle} plus the exporter endpoint and the remote sampler in use. {@link #shutdown()}
 * flushes, shuts the provider down and closes a remote sampler the setup created.
 */
public final class JaegerTracingHandle implements AutoCloseable {
  private final TracingHandle handle;
  private final String endpoint;
  private final JaegerRemoteSampler remoteSampler;
  private final boolean ownsSampler;
  private final AtomicBoolean shutdown = new AtomicBoolean();

  JaegerTracingHandle(
      TracingHandle handle, String endpoint, JaegerRemoteSampler remoteSampler, boolean owns) {
    this.handle = handle;
    this.endpoint = endpoint;
    this.remoteSampler = remoteSampler;
    this.ownsSampler = owns;
  }

  /**
   * The category handle (for the span helpers, adapters and the conformance target).
   *
   * @return handle
   */
  public TracingHandle handle() {
    return handle;
  }

  /**
   * The exporter endpoint in use.
   *
   * @return URL
   */
  public String endpoint() {
    return endpoint;
  }

  /**
   * The remote sampler in use.
   *
   * @return sampler, or empty when the config's sampler applies
   */
  public Optional<JaegerRemoteSampler> remoteSampler() {
    return Optional.ofNullable(remoteSampler);
  }

  /**
   * The resolved tracing config.
   *
   * @return config
   */
  public TracingConfig config() {
    return handle.config();
  }

  /**
   * The OpenTelemetry SDK instance.
   *
   * @return SDK
   */
  public OpenTelemetrySdk openTelemetry() {
    return handle.openTelemetry();
  }

  /**
   * The tracer provider.
   *
   * @return provider
   */
  public SdkTracerProvider tracerProvider() {
    return handle.tracerProvider();
  }

  /**
   * The configured propagators.
   *
   * @return propagators
   */
  public ContextPropagators propagators() {
    return handle.propagators();
  }

  /**
   * Returns a tracer.
   *
   * @param name instrumentation scope name
   * @return tracer
   */
  public Tracer tracer(String name) {
    return handle.tracer(name);
  }

  /**
   * Returns a tracer with a scope version.
   *
   * @param name instrumentation scope name
   * @param version scope version
   * @return tracer
   */
  public Tracer tracer(String name, String version) {
    return handle.tracer(name, version);
  }

  /** Exports all finished spans, waiting at most the export timeout. */
  public void forceFlush() {
    handle.forceFlush();
  }

  /**
   * Exports all finished spans.
   *
   * @param timeout maximum wait
   */
  public void forceFlush(Duration timeout) {
    handle.forceFlush(timeout);
  }

  /**
   * Asynchronous {@link #forceFlush(Duration)}.
   *
   * @param timeout maximum wait
   * @return completes when flushed
   */
  public CompletableFuture<Void> forceFlushAsync(Duration timeout) {
    return handle.forceFlushAsync(timeout);
  }

  /** Flushes, shuts the provider down and closes an owned remote sampler; idempotent. */
  public void shutdown() {
    if (!shutdown.compareAndSet(false, true)) {
      return;
    }
    try {
      handle.shutdown();
    } finally {
      if (ownsSampler) {
        remoteSampler.close();
      }
    }
  }

  /**
   * Asynchronous {@link #shutdown()}.
   *
   * @return completes when shut down
   */
  public CompletableFuture<Void> shutdownAsync() {
    return CompletableFuture.runAsync(this::shutdown);
  }

  /**
   * Whether {@link #shutdown()} was called.
   *
   * @return shutdown flag
   */
  public boolean isShutdown() {
    return shutdown.get();
  }

  /** Same as {@link #shutdown()}. */
  @Override
  public void close() {
    shutdown();
  }

  @Override
  public String toString() {
    return "JaegerTracingHandle{endpoint="
        + codes.grow.packages.jaeger.core.Redaction.redactUrl(endpoint)
        + ", remoteSampler="
        + remoteSampler
        + "}";
  }
}
