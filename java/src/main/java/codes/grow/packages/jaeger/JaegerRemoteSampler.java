package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.HttpTransport;
import codes.grow.packages.jaeger.core.JaegerException;
import codes.grow.packages.jaeger.core.Redaction;
import codes.grow.packages.jaeger.core.Secret;
import codes.grow.packages.jaeger.core.TransportRequest;
import codes.grow.packages.jaeger.core.ValidationException;
import io.opentelemetry.api.common.Attributes;
import io.opentelemetry.api.trace.SpanKind;
import io.opentelemetry.context.Context;
import io.opentelemetry.sdk.trace.data.LinkData;
import io.opentelemetry.sdk.trace.samplers.Sampler;
import io.opentelemetry.sdk.trace.samplers.SamplingResult;
import java.time.Duration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import java.util.function.LongSupplier;
import java.util.function.Supplier;

/**
 * OpenTelemetry head {@link Sampler} driven by Jaeger remote sampling: it polls {@code GET
 * <samplingUrl>?service=<service>} every poll interval (default 1 min) and applies the strategy.
 *
 * <p>Rule (the same in every language): a per-operation strategy wins for a listed operation (span
 * name); every other operation uses the service-level rate limiter when the strategy carries {@code
 * rateLimitingSampling} (token bucket, balance starts at {@code max(1, rate)}, so a rate of 0 lets
 * the first trace through, then none), else the service probability ({@code probabilisticSampling},
 * else {@code defaultSamplingProbability}). Probabilistic decisions are deterministic per trace id.
 * Until the first strategy arrives the initial rate (default 0.001) applies; a failed or malformed
 * poll keeps the last good strategy.
 *
 * <p>Use it as the root of {@code Sampler.parentBased(...)} (what {@link JaegerTracing} does by
 * default) and close it when done.
 */
public final class JaegerRemoteSampler implements Sampler, AutoCloseable {
  /** Default poll interval. */
  public static final Duration DEFAULT_POLL_INTERVAL = Duration.ofMinutes(1);

  /** Default probability until the first strategy arrives. */
  public static final double DEFAULT_INITIAL_SAMPLING_RATE = 0.001;

  private final String serviceName;
  private final String samplingUrl;
  private final Duration pollInterval;
  private final double initialSamplingRate;
  private final HttpTransport transport;
  private final Supplier<Optional<Secret>> token;
  private final boolean ownsTransport;
  private final Consumer<JaegerException> onError;
  private final Consumer<JaegerSamplingStrategy> onUpdate;
  private final LongSupplier nanoClock;
  private final Map<Double, Sampler> ratio = new HashMap<>();
  private final AtomicBoolean started = new AtomicBoolean();
  private final AtomicBoolean closed = new AtomicBoolean();
  private final CountDownLatch firstPoll = new CountDownLatch(1);
  private volatile State state;
  private ScheduledExecutorService scheduler;

  /** Strategy in use plus its rate limiter. */
  private record State(JaegerSamplingStrategy strategy, boolean fetched, RateLimiter limiter) {}

  private JaegerRemoteSampler(Builder b, HttpTransport transport, boolean owns) {
    this.serviceName = b.serviceName;
    this.samplingUrl = b.samplingUrl;
    this.pollInterval = b.pollInterval;
    this.initialSamplingRate = b.initialSamplingRate;
    this.transport = transport;
    this.token = b.token;
    this.ownsTransport = owns;
    this.onError = b.onError;
    this.onUpdate = b.onUpdate;
    this.nanoClock = b.nanoClock;
    this.state = new State(JaegerSamplingStrategy.probabilistic(initialSamplingRate), false, null);
  }

  /**
   * Creates and starts a sampler (the ticket signature {@code JaegerRemoteSampler(samplingUrl,
   * service, pollInterval)}).
   *
   * @param samplingUrl sampling endpoint, e.g. {@code http://jaeger:5778/sampling}
   * @param serviceName service whose strategy is fetched
   * @param pollInterval poll interval ({@code null} = 1 min)
   * @return a started sampler
   */
  public static JaegerRemoteSampler create(
      String samplingUrl, String serviceName, Duration pollInterval) {
    return builder(serviceName).samplingUrl(samplingUrl).pollInterval(pollInterval).build();
  }

  /**
   * Starts a builder.
   *
   * @param serviceName service whose strategy is fetched (non-blank)
   * @return a builder
   */
  public static Builder builder(String serviceName) {
    return new Builder(serviceName);
  }

  /**
   * Fetches the strategy of a service once.
   *
   * @param transport transport (absolute {@code samplingUrl} is used as the path)
   * @param token optional bearer token
   * @param samplingUrl sampling endpoint
   * @param service service name
   * @return future strategy
   */
  static CompletableFuture<JaegerSamplingStrategy> fetch(
      HttpTransport transport, Optional<Secret> token, String samplingUrl, String service) {
    String operation = "jaeger.sampling";
    if (service == null || service.isBlank()) {
      return CompletableFuture.failedFuture(
          new ValidationException(operation + ": service is required"));
    }
    TransportRequest.Builder b =
        TransportRequest.get(samplingUrl).operation(operation).query("service", service);
    token.ifPresent(t -> b.header("Authorization", "Bearer " + t.reveal()));
    return transport
        .sendAsync(b.build())
        .thenApply(r -> JaegerSamplingStrategy.parse(r.jsonNode()));
  }

  /**
   * The service whose strategy is applied.
   *
   * @return service name
   */
  public String serviceName() {
    return serviceName;
  }

  /**
   * The sampling endpoint.
   *
   * @return URL
   */
  public String samplingUrl() {
    return samplingUrl;
  }

  /**
   * The poll interval.
   *
   * @return interval
   */
  public Duration pollInterval() {
    return pollInterval;
  }

  /**
   * The probability used until the first strategy arrives.
   *
   * @return rate 0..1
   */
  public double initialSamplingRate() {
    return initialSamplingRate;
  }

  /**
   * The strategy fetched from the server.
   *
   * @return the strategy in use, or empty before the first successful poll
   */
  public Optional<JaegerSamplingStrategy> strategy() {
    State s = state;
    return s.fetched ? Optional.of(s.strategy) : Optional.empty();
  }

  /**
   * Fetches the strategy now and installs it.
   *
   * @return the new strategy
   * @throws JaegerException on failure (the previous strategy stays in use)
   */
  public JaegerSamplingStrategy refresh() {
    return HttpTransport.await(refreshAsync(), "jaeger.sampling");
  }

  /**
   * Asynchronous {@link #refresh()}.
   *
   * @return future strategy
   */
  public CompletableFuture<JaegerSamplingStrategy> refreshAsync() {
    return fetch(transport, token.get(), samplingUrl, serviceName)
        .thenApply(
            s -> {
              install(s);
              return s;
            });
  }

  synchronized void install(JaegerSamplingStrategy strategy) {
    RateLimiter previous = state.limiter;
    RateLimiter limiter = null;
    if (strategy.maxTracesPerSecond().isPresent()) {
      double rate = strategy.maxTracesPerSecond().getAsDouble();
      limiter =
          previous != null && previous.rate == rate ? previous : new RateLimiter(rate, nanoClock);
    }
    state = new State(strategy, true, limiter);
  }

  /** Starts polling (the first poll runs at once in the background). Idempotent. */
  public void start() {
    if (closed.get() || !started.compareAndSet(false, true)) {
      return;
    }
    scheduler =
        Executors.newSingleThreadScheduledExecutor(
            r -> {
              Thread t = new Thread(r, "jaeger-remote-sampler-" + serviceName);
              t.setDaemon(true);
              return t;
            });
    scheduler.scheduleWithFixedDelay(this::poll, 0, pollInterval.toNanos(), TimeUnit.NANOSECONDS);
  }

  private void poll() {
    try {
      JaegerSamplingStrategy s = refresh();
      safe(() -> onUpdate.accept(s));
    } catch (JaegerException e) {
      if (!closed.get()) {
        safe(() -> onError.accept(e));
      }
    } finally {
      firstPoll.countDown();
    }
  }

  private static void safe(Runnable r) {
    try {
      r.run();
    } catch (RuntimeException ignored) {
      // callback errors never stop polling
    }
  }

  /**
   * Waits until the first poll since {@link #start()} finished (successfully or not).
   *
   * @param timeout maximum wait
   * @return true when the first poll finished in time
   */
  public boolean awaitFirstPoll(Duration timeout) {
    try {
      return firstPoll.await(timeout.toNanos(), TimeUnit.NANOSECONDS);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      return false;
    }
  }

  /**
   * Whether {@link #close()} was called.
   *
   * @return true once closed
   */
  public boolean isClosed() {
    return closed.get();
  }

  /** Stops polling and releases connections it owns. Idempotent. */
  @Override
  public void close() {
    if (!closed.compareAndSet(false, true)) {
      return;
    }
    if (scheduler != null) {
      scheduler.shutdownNow();
    }
    firstPoll.countDown();
    if (ownsTransport) {
      transport.close();
    }
  }

  private Sampler ratio(double rate) {
    synchronized (ratio) {
      return ratio.computeIfAbsent(rate, Sampler::traceIdRatioBased);
    }
  }

  @Override
  public SamplingResult shouldSample(
      Context parentContext,
      String traceId,
      String name,
      SpanKind spanKind,
      Attributes attributes,
      List<LinkData> parentLinks) {
    State s = state;
    Optional<Double> op = s.strategy.operationRate(name);
    if (op.isPresent()) {
      return ratio(op.get())
          .shouldSample(parentContext, traceId, name, spanKind, attributes, parentLinks);
    }
    if (s.limiter != null) {
      return s.limiter.check() ? SamplingResult.recordAndSample() : SamplingResult.drop();
    }
    double rate = s.strategy.serviceRate().orElse(initialSamplingRate);
    return ratio(rate)
        .shouldSample(parentContext, traceId, name, spanKind, attributes, parentLinks);
  }

  @Override
  public String getDescription() {
    return "JaegerRemoteSampler{service="
        + serviceName
        + ", url="
        + Redaction.redactUrl(samplingUrl)
        + "}";
  }

  @Override
  public String toString() {
    return getDescription();
  }

  /** Token bucket with Jaeger semantics: balance starts full at {@code max(1, rate)}. */
  static final class RateLimiter {
    final double rate;
    private final double maxBalance;
    private final LongSupplier nanoClock;
    private double balance;
    private long last;

    RateLimiter(double rate, LongSupplier nanoClock) {
      this.rate = rate;
      this.maxBalance = Math.max(1, rate);
      this.balance = maxBalance;
      this.nanoClock = nanoClock;
      this.last = nanoClock.getAsLong();
    }

    synchronized boolean check() {
      long now = nanoClock.getAsLong();
      double elapsed = Math.max(0, now - last) / 1e9;
      last = now;
      balance = Math.min(maxBalance, balance + elapsed * rate);
      if (balance >= 1) {
        balance -= 1;
        return true;
      }
      return false;
    }
  }

  /** Builder of a {@link JaegerRemoteSampler}. */
  public static final class Builder {
    private final String serviceName;
    private String samplingUrl;
    private Duration pollInterval = DEFAULT_POLL_INTERVAL;
    private double initialSamplingRate = DEFAULT_INITIAL_SAMPLING_RATE;
    private JaegerConfig config;
    private HttpTransport sharedTransport;
    private Supplier<Optional<Secret>> token = Optional::empty;
    private Consumer<JaegerException> onError = e -> {};
    private Consumer<JaegerSamplingStrategy> onUpdate = s -> {};
    private boolean autoStart = true;
    private LongSupplier nanoClock = System::nanoTime;

    private Builder(String serviceName) {
      this.serviceName = serviceName;
    }

    /**
     * Sets the sampling endpoint (default: {@code config.samplingUrl()}).
     *
     * @param samplingUrl URL
     * @return this builder
     */
    public Builder samplingUrl(String samplingUrl) {
      this.samplingUrl = samplingUrl;
      return this;
    }

    /**
     * Sets the poll interval (default 1 min).
     *
     * @param pollInterval positive interval, {@code null} = default
     * @return this builder
     */
    public Builder pollInterval(Duration pollInterval) {
      this.pollInterval = pollInterval == null ? DEFAULT_POLL_INTERVAL : pollInterval;
      return this;
    }

    /**
     * Sets the probability used until the first strategy arrives (default 0.001).
     *
     * @param rate 0..1
     * @return this builder
     */
    public Builder initialSamplingRate(double rate) {
      this.initialSamplingRate = rate;
      return this;
    }

    /**
     * Sets the transport settings (timeouts, retries, TLS, proxy, headers, token, samplingUrl).
     *
     * @param config configuration
     * @return this builder
     */
    public Builder config(JaegerConfig config) {
      this.config = config;
      return this;
    }

    /** Uses a client's transport instead of creating one (not closed by the sampler). */
    Builder transport(HttpTransport transport, Supplier<Optional<Secret>> token) {
      this.sharedTransport = transport;
      this.token = token;
      return this;
    }

    /**
     * Receives poll errors (the last good strategy stays in use).
     *
     * @param onError callback
     * @return this builder
     */
    public Builder onError(Consumer<JaegerException> onError) {
      this.onError = onError == null ? e -> {} : onError;
      return this;
    }

    /**
     * Called after each successful poll.
     *
     * @param onUpdate callback
     * @return this builder
     */
    public Builder onUpdate(Consumer<JaegerSamplingStrategy> onUpdate) {
      this.onUpdate = onUpdate == null ? s -> {} : onUpdate;
      return this;
    }

    /**
     * Whether {@link #build()} starts polling (default true).
     *
     * @param autoStart start at once
     * @return this builder
     */
    public Builder autoStart(boolean autoStart) {
      this.autoStart = autoStart;
      return this;
    }

    /** Replaces the monotonic clock of the rate limiter (tests). */
    Builder nanoClock(LongSupplier nanoClock) {
      this.nanoClock = nanoClock;
      return this;
    }

    /**
     * Validates and builds (and starts, unless {@code autoStart(false)}) the sampler.
     *
     * @return the sampler
     * @throws ConfigException for a blank service, a bad URL, a non-positive interval or an initial
     *     rate outside 0..1
     */
    public JaegerRemoteSampler build() {
      if (serviceName == null || serviceName.isBlank()) {
        throw new ConfigException("jaeger.remoteSampler: serviceName is required");
      }
      if (pollInterval.isZero() || pollInterval.isNegative()) {
        throw new ConfigException("jaeger.remoteSampler: pollInterval must be positive");
      }
      if (!(initialSamplingRate >= 0 && initialSamplingRate <= 1)) {
        throw new ConfigException(
            "jaeger.remoteSampler: initialSamplingRate must be between 0 and 1");
      }
      JaegerConfig base = config != null ? config : JaegerConfig.builder().build();
      if (samplingUrl == null) {
        samplingUrl = base.samplingUrl();
      } else {
        base = base.toBuilder().samplingUrl(samplingUrl).build();
      }
      JaegerRemoteSampler sampler;
      if (sharedTransport != null) {
        sampler = new JaegerRemoteSampler(this, sharedTransport, false);
      } else {
        JaegerConfig c = base;
        token = c::token;
        sampler = new JaegerRemoteSampler(this, new HttpTransport(c.toTransportConfig()), true);
      }
      if (autoStart) {
        sampler.start();
      }
      return sampler;
    }
  }
}
