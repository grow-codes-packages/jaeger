package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.AuthenticationException;
import codes.grow.packages.jaeger.core.CancelledException;
import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.EnvReader;
import codes.grow.packages.jaeger.core.HealthCheck;
import codes.grow.packages.jaeger.core.HealthStatus;
import codes.grow.packages.jaeger.core.Hooks;
import codes.grow.packages.jaeger.core.HttpTransport;
import codes.grow.packages.jaeger.core.Json;
import codes.grow.packages.jaeger.core.Logger;
import codes.grow.packages.jaeger.core.RetryPolicy;
import codes.grow.packages.jaeger.core.Secret;
import codes.grow.packages.jaeger.core.TlsOptions;
import codes.grow.packages.jaeger.core.TransportRequest;
import codes.grow.packages.jaeger.core.TransportResponse;
import codes.grow.packages.jaeger.core.UnsupportedException;
import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import com.fasterxml.jackson.databind.JsonNode;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Client for Jaeger: health, ping and version of the query service, the trace query API ({@link
 * #query()}), OpenTelemetry tracing setup ({@link #initTracing}) and remote sampling ({@link
 * #samplingStrategy}, {@link #remoteSampler}).
 *
 * <p>Implements SPEC section 2.4. Thread-safe; create one per Jaeger and close it when done: {@link
 * #close()} also shuts down every tracing handle and sampler created through the client.
 *
 * <pre>{@code
 * try (JaegerClient client = JaegerClient.fromEnv()) {
 *   HealthStatus health = client.health();
 *   List<String> services = client.query().listServices();
 * }
 * }</pre>
 *
 * <p>Every I/O method has a blocking form and an {@code …Async} form returning {@link
 * CompletableFuture}.
 */
public final class JaegerClient implements AutoCloseable {
  private static final Pattern VERSION =
      Pattern.compile("JAEGER_VERSION\\s*=\\s*(\\{[^;]*?\\})\\s*;");
  private static final Pattern TARGET_INFO =
      Pattern.compile("(?m)^target_info\\{[^}]*\\bservice_version=\"v?([^\"]+)\"");

  private final JaegerConfig config;
  private final HttpTransport transport;
  private final JaegerQueryClient query;
  private final AtomicBoolean closed = new AtomicBoolean();
  private final List<AutoCloseable> resources = new ArrayList<>();

  private JaegerClient(JaegerConfig config) {
    this.config = config;
    this.transport = new HttpTransport(config.toTransportConfig());
    this.query = new JaegerQueryClient(transport, config::token, false);
  }

  /**
   * Creates a client.
   *
   * @param config validated configuration
   * @return a new client
   * @throws ConfigException when TLS or proxy settings are invalid
   */
  public static JaegerClient create(JaegerConfig config) {
    config.validate();
    return new JaegerClient(config);
  }

  /**
   * Creates a client from {@code GROW_JAEGER_*} and the vendor variables ({@code JAEGER_QUERY_URL},
   * {@code JAEGER_SAMPLING_ENDPOINT}).
   *
   * @return a new client
   */
  public static JaegerClient fromEnv() {
    return create(JaegerConfig.fromEnv());
  }

  /**
   * Starts a client builder.
   *
   * @return a new builder
   */
  public static Builder builder() {
    return new Builder();
  }

  /**
   * Returns the configuration.
   *
   * @return the configuration
   */
  public JaegerConfig config() {
    return config;
  }

  /**
   * Returns the HTTP transport, for service modules built on this client.
   *
   * @return the transport
   */
  public HttpTransport transport() {
    return transport;
  }

  /**
   * The trace query client; it shares this client's connections (closing it is a no-op).
   *
   * @return the query client
   */
  public JaegerQueryClient query() {
    return query;
  }

  /**
   * Registers a resource (sampler, tracing handle) closed with the client.
   *
   * @param resource the resource
   */
  public void register(AutoCloseable resource) {
    synchronized (resources) {
      resources.add(resource);
    }
  }

  private void assertOpen(String operation) {
    if (closed.get()) {
      throw new CancelledException(operation + ": client is closed");
    }
  }

  /**
   * Liveness of the query service: {@code GET /} on the query port (the stack catalog health
   * target), without credentials. With {@code healthUrl} set, the healthcheckv2 result is added to
   * the details with the prefix {@code healthcheck.}; an unhealthy result there gives {@code
   * degraded}. Never throws for an unhealthy or unreachable service.
   *
   * @return the health status
   * @throws CancelledException when interrupted or closed
   */
  public HealthStatus health() {
    return HttpTransport.await(healthAsync(), "health");
  }

  /**
   * Asynchronous {@link #health()}.
   *
   * @return a future with the health status
   */
  public CompletableFuture<HealthStatus> healthAsync() {
    return HealthCheck.checkAsync(transport, "/")
        .thenCompose(
            hs -> {
              Optional<String> healthUrl = config.healthUrl();
              if (!hs.healthy() || healthUrl.isEmpty()) {
                return CompletableFuture.completedFuture(hs);
              }
              return HealthCheck.checkAsync(transport, healthUrl.get())
                  .thenApply(detail -> merge(hs, detail));
            });
  }

  private static HealthStatus merge(HealthStatus hs, HealthStatus detail) {
    Map<String, String> details = new LinkedHashMap<>(hs.details());
    details.put("healthcheck.status", detail.status());
    detail.details().forEach((k, v) -> details.put("healthcheck." + k, v));
    return detail.healthy()
        ? new HealthStatus(true, hs.status(), details)
        : new HealthStatus(false, "degraded", details);
  }

  /**
   * Round trip through the query API ({@code GET /api/v3/services}), with the token when set.
   *
   * @return the latency
   * @throws AuthenticationException on bad credentials behind an auth proxy
   */
  public Duration ping() {
    return HttpTransport.await(pingAsync(), "ping");
  }

  /**
   * Asynchronous {@link #ping()}.
   *
   * @return a future with the latency
   */
  public CompletableFuture<Duration> pingAsync() {
    long start = System.nanoTime();
    return transport
        .sendAsync(authorized(TransportRequest.get("/api/v3/services").operation("ping")))
        .thenApply(r -> Duration.ofNanos(Math.max(1, System.nanoTime() - start)));
  }

  /**
   * Build information parsed from the UI page ({@code GET /}).
   *
   * @return the build info, or empty when the page has none or the UI is disabled (404)
   */
  public Optional<JaegerBuildInfo> buildInfo() {
    return HttpTransport.await(buildInfoAsync(), "serverVersion");
  }

  /**
   * Asynchronous {@link #buildInfo()}.
   *
   * @return a future with the build info
   */
  public CompletableFuture<Optional<JaegerBuildInfo>> buildInfoAsync() {
    return transport
        .sendAsync(
            authorized(
                TransportRequest.get("/")
                    .operation("serverVersion")
                    .header("Accept", "text/html")
                    .acceptStatus(404)))
        .thenApply(r -> r.status() == 404 ? Optional.empty() : parseBuildInfo(r.text()));
  }

  static Optional<JaegerBuildInfo> parseBuildInfo(String html) {
    Matcher m = VERSION.matcher(html);
    if (!m.find()) {
      return Optional.empty();
    }
    JsonNode node;
    try {
      node = Json.tree(m.group(1).getBytes(StandardCharsets.UTF_8));
    } catch (ValidationException e) {
      return Optional.empty();
    }
    return Optional.of(
        new JaegerBuildInfo(
            textField(node, "gitVersion"),
            textField(node, "gitCommit"),
            textField(node, "buildDate")));
  }

  private static Optional<String> textField(JsonNode node, String field) {
    JsonNode v = node.get(field);
    return v != null && v.isTextual() && !v.asText().isEmpty()
        ? Optional.of(v.asText())
        : Optional.empty();
  }

  private static String stripV(String version) {
    return version.length() > 1 && version.charAt(0) == 'v' && Character.isDigit(version.charAt(1))
        ? version.substring(1)
        : version;
  }

  /**
   * Server version without the leading {@code v} (e.g. {@code 2.23.0}): {@code JAEGER_VERSION} in
   * the UI page, else {@code target_info{service_version=…}} at {@code metricsUrl}.
   *
   * @return the version string
   * @throws UnsupportedException when neither source reports a version
   */
  public String serverVersion() {
    return HttpTransport.await(serverVersionAsync(), "serverVersion");
  }

  /**
   * Asynchronous {@link #serverVersion()}.
   *
   * @return a future with the version
   */
  public CompletableFuture<String> serverVersionAsync() {
    return buildInfoAsync()
        .thenCompose(
            info -> {
              Optional<String> v = info.flatMap(JaegerBuildInfo::gitVersion);
              if (v.isPresent()) {
                return CompletableFuture.completedFuture(stripV(v.get()));
              }
              Optional<String> metricsUrl = config.metricsUrl();
              if (metricsUrl.isEmpty()) {
                return CompletableFuture.failedFuture(unsupportedVersion());
              }
              return transport
                  .sendAsync(
                      authorized(
                          TransportRequest.get(metricsUrl.get())
                              .operation("serverVersion")
                              .header("Accept", "text/plain")
                              .acceptStatus(404)))
                  .thenApply(JaegerClient::versionFromMetrics);
            });
  }

  private static String versionFromMetrics(TransportResponse r) {
    Matcher m = TARGET_INFO.matcher(r.status() == 404 ? "" : r.text());
    if (!m.find()) {
      throw unsupportedVersion();
    }
    return stripV(m.group(1));
  }

  private static UnsupportedException unsupportedVersion() {
    return new UnsupportedException("serverVersion: the service does not report a version");
  }

  private TransportRequest authorized(TransportRequest.Builder b) {
    config.token().ifPresent(t -> b.header("Authorization", "Bearer " + t.reveal()));
    return b.build();
  }

  /**
   * Fetches the remote sampling strategy of a service once ({@code GET <samplingUrl>?service=}).
   *
   * @param service service name (non-blank)
   * @return the strategy
   * @throws ValidationException for a blank service (no request) or a malformed response
   */
  public JaegerSamplingStrategy samplingStrategy(String service) {
    return HttpTransport.await(samplingStrategyAsync(service), "jaeger.sampling");
  }

  /**
   * Asynchronous {@link #samplingStrategy(String)}.
   *
   * @param service service name
   * @return future strategy
   */
  public CompletableFuture<JaegerSamplingStrategy> samplingStrategyAsync(String service) {
    return JaegerRemoteSampler.fetch(transport, config.token(), config.samplingUrl(), service);
  }

  /**
   * Creates and starts a {@link JaegerRemoteSampler} for a service that uses this client's settings
   * and connections ({@code samplingUrl}, timeouts, TLS, proxy, headers, token). It is closed by
   * {@link #close()}.
   *
   * @param service service name
   * @param pollInterval poll interval ({@code null} = 1 min)
   * @return the started sampler
   */
  public JaegerRemoteSampler remoteSampler(String service, Duration pollInterval) {
    assertOpen("remoteSampler");
    JaegerRemoteSampler sampler =
        JaegerRemoteSampler.builder(service)
            .config(config)
            .transport(transport, config::token)
            .pollInterval(pollInterval)
            .build();
    register(sampler);
    return sampler;
  }

  /**
   * Starts OpenTelemetry tracing to Jaeger (see {@link JaegerTracing#initTracing(TracingConfig)});
   * a remote sampler requested by the environment uses this client's configuration. The handle is
   * shut down by {@link #close()}.
   *
   * @param tracingConfig tracing config
   * @return the running setup
   */
  public JaegerTracingHandle initTracing(TracingConfig tracingConfig) {
    return initTracing(tracingConfig, JaegerTracing.Options.builder());
  }

  /**
   * Starts OpenTelemetry tracing to Jaeger with options; created remote samplers default to this
   * client's configuration. The handle is shut down by {@link #close()}.
   *
   * @param tracingConfig tracing config
   * @param options environment and sampler options
   * @return the running setup
   */
  public JaegerTracingHandle initTracing(
      TracingConfig tracingConfig, JaegerTracing.Options options) {
    assertOpen("initTracing");
    JaegerTracingHandle handle =
        JaegerTracing.initTracing(tracingConfig, options.defaultSamplerConfig(config));
    register(handle);
    return handle;
  }

  /**
   * Returns whether {@link #close()} was called.
   *
   * @return true once closed
   */
  public boolean isClosed() {
    return closed.get();
  }

  /**
   * Shuts down the tracing handles (flushing their batch processors) and samplers created through
   * this client, then releases connections. Idempotent.
   */
  @Override
  public void close() {
    if (!closed.compareAndSet(false, true)) {
      return;
    }
    List<AutoCloseable> copy;
    synchronized (resources) {
      copy = new ArrayList<>(resources);
      resources.clear();
    }
    for (AutoCloseable r : copy) {
      try {
        r.close();
      } catch (Exception ignored) {
        // best effort: close must not throw
      }
    }
    transport.close();
  }

  @Override
  public String toString() {
    return "JaegerClient{" + config + "}";
  }

  /** Builder that configures and creates a client in one chain. */
  public static final class Builder {
    private final JaegerConfig.Builder config = JaegerConfig.builder();

    private Builder() {}

    /**
     * Fills unset fields from an environment reader.
     *
     * @param env reader, e.g. {@link EnvReader#system()}
     * @return this builder
     */
    public Builder fromEnv(EnvReader env) {
      config.fromEnv(env);
      return this;
    }

    /**
     * Sets the endpoint base URL.
     *
     * @param url URL
     * @return this builder
     */
    public Builder url(String url) {
      config.url(url);
      return this;
    }

    /**
     * Sets the API token.
     *
     * @param token token
     * @return this builder
     */
    public Builder token(String token) {
      config.token(token);
      return this;
    }

    /**
     * Sets the API token.
     *
     * @param token token
     * @return this builder
     */
    public Builder token(Secret token) {
      config.token(token);
      return this;
    }

    /**
     * Sets the connect timeout.
     *
     * @param d duration
     * @return this builder
     */
    public Builder connectTimeout(Duration d) {
      config.connectTimeout(d);
      return this;
    }

    /**
     * Sets the per-attempt timeout.
     *
     * @param d duration
     * @return this builder
     */
    public Builder requestTimeout(Duration d) {
      config.requestTimeout(d);
      return this;
    }

    /**
     * Sets the total timeout.
     *
     * @param d duration
     * @return this builder
     */
    public Builder totalTimeout(Duration d) {
      config.totalTimeout(d);
      return this;
    }

    /**
     * Sets the retry policy.
     *
     * @param retry policy
     * @return this builder
     */
    public Builder retry(RetryPolicy retry) {
      config.retry(retry);
      return this;
    }

    /**
     * Sets the TLS options.
     *
     * @param tls options
     * @return this builder
     */
    public Builder tls(TlsOptions tls) {
      config.tls(tls);
      return this;
    }

    /**
     * Sets the proxy URL.
     *
     * @param proxy URL
     * @return this builder
     */
    public Builder proxy(String proxy) {
      config.proxy(proxy);
      return this;
    }

    /**
     * Adds a header.
     *
     * @param name name
     * @param value value
     * @return this builder
     */
    public Builder header(String name, String value) {
      config.header(name, value);
      return this;
    }

    /**
     * Sets the user agent suffix.
     *
     * @param suffix suffix
     * @return this builder
     */
    public Builder userAgentSuffix(String suffix) {
      config.userAgentSuffix(suffix);
      return this;
    }

    /**
     * Sets hooks.
     *
     * @param hooks hooks
     * @return this builder
     */
    public Builder hooks(Hooks hooks) {
      config.hooks(hooks);
      return this;
    }

    /**
     * Sets the logger.
     *
     * @param logger logger
     * @return this builder
     */
    public Builder logger(Logger logger) {
      config.logger(logger);
      return this;
    }

    /**
     * Sets the remote sampling endpoint.
     *
     * @param url URL
     * @return this builder
     */
    public Builder samplingUrl(String url) {
      config.samplingUrl(url);
      return this;
    }

    /**
     * Sets the healthcheckv2 URL.
     *
     * @param url URL
     * @return this builder
     */
    public Builder healthUrl(String url) {
      config.healthUrl(url);
      return this;
    }

    /**
     * Sets the Prometheus metrics URL (version fallback).
     *
     * @param url URL
     * @return this builder
     */
    public Builder metricsUrl(String url) {
      config.metricsUrl(url);
      return this;
    }

    /**
     * Builds the configuration and the client.
     *
     * @return a new client
     * @throws ConfigException when the configuration is invalid
     */
    public JaegerClient build() {
      return create(config.build());
    }
  }
}
