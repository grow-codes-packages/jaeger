package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.EnvReader;
import codes.grow.packages.jaeger.core.TlsOptions;
import codes.grow.packages.jaeger.tracing.OtlpExporters;
import codes.grow.packages.jaeger.tracing.SamplerTracing;
import codes.grow.packages.jaeger.tracing.SpanExporterFactory;
import codes.grow.packages.jaeger.tracing.Tracing;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.TracingConfig.Compression;
import codes.grow.packages.jaeger.tracing.TracingConfig.Protocol;
import codes.grow.packages.jaeger.tracing.TracingConfig.SamplerType;
import codes.grow.packages.jaeger.tracing.TracingHandle;
import io.opentelemetry.sdk.trace.samplers.Sampler;
import java.net.URI;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Paths;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Optional;

/**
 * The Jaeger tracing preset (SPEC section 5): OpenTelemetry tracing that exports to Jaeger over
 * OTLP, {@code http/protobuf} to {@value #DEFAULT_OTLP_HTTP_ENDPOINT} by default or {@code grpc} to
 * {@value #DEFAULT_OTLP_GRPC_ENDPOINT}, optionally sampled by the Jaeger remote sampler.
 *
 * <pre>{@code
 * try (JaegerTracingHandle tracing = JaegerTracing.initTracing(
 *     TracingConfig.builder("orders").setGlobal(false).build())) {
 *   Tracer tracer = tracing.tracer("orders");
 *   ...
 * }
 * }</pre>
 *
 * <p><b>Environment.</b> {@link #initTracing(TracingConfig)} fills every field that the config
 * leaves at its default from the process environment ({@link #resolveConfig}); explicit values win.
 * Exporter precedence per field: explicit &gt; {@code GROW_JAEGER_OTLP_*} &gt; {@code
 * OTEL_EXPORTER_OTLP_TRACES_*} &gt; {@code OTEL_EXPORTER_OTLP_*} &gt; Jaeger default.
 *
 * <table>
 *   <caption>Environment variables</caption>
 *   <tr><th>Field</th><th>Variables</th></tr>
 *   <tr><td>protocol</td><td>{@code …_PROTOCOL} ({@code grpc}, {@code http/protobuf})</td></tr>
 *   <tr><td>endpoint</td><td>{@code GROW_JAEGER_OTLP_GRPC_ENDPOINT} / {@code
 *       GROW_JAEGER_OTLP_HTTP_ENDPOINT} (by protocol), {@code GROW_JAEGER_OTLP_ENDPOINT}, {@code
 *       OTEL_EXPORTER_OTLP_TRACES_ENDPOINT} (as is), {@code OTEL_EXPORTER_OTLP_ENDPOINT} (base;
 *       HTTP appends {@code /v1/traces})</td></tr>
 *   <tr><td>headers</td><td>{@code …_HEADERS} ({@code k=v,k2=v2}, percent-decoded)</td></tr>
 *   <tr><td>compression</td><td>{@code …_COMPRESSION} ({@code gzip}, {@code none})</td></tr>
 *   <tr><td>export timeout</td><td>{@code …_TIMEOUT} (ms or {@code 10s})</td></tr>
 *   <tr><td>TLS</td><td>{@code …_CERTIFICATE} (alias {@code GROW_JAEGER_OTLP_CA_CERT}), {@code
 *       …_CLIENT_CERTIFICATE}, {@code …_CLIENT_KEY} (paths)</td></tr>
 *   <tr><td>service name</td><td>{@code GROW_JAEGER_SERVICE_NAME}, {@code OTEL_SERVICE_NAME},
 *       {@code service.name} in {@code OTEL_RESOURCE_ATTRIBUTES}</td></tr>
 *   <tr><td>resource</td><td>{@code OTEL_RESOURCE_ATTRIBUTES} (merged, explicit keys
 *       win)</td></tr>
 *   <tr><td>sampler</td><td>{@code OTEL_TRACES_SAMPLER}, {@code OTEL_TRACES_SAMPLER_ARG}; {@code
 *       jaeger_remote} / {@code parentbased_jaeger_remote} enable the remote sampler (arg {@code
 *       endpoint=…,pollingIntervalMs=…,initialSamplingRate=…})</td></tr>
 *   <tr><td>propagators</td><td>{@code OTEL_PROPAGATORS}</td></tr>
 * </table>
 *
 * <p>A present but invalid value raises {@link ConfigException} naming the variable, never its
 * value.
 */
public final class JaegerTracing {
  /** Default OTLP/HTTP traces endpoint of a Jaeger all-in-one or collector. */
  public static final String DEFAULT_OTLP_HTTP_ENDPOINT = "http://127.0.0.1:4318/v1/traces";

  /** Default OTLP/gRPC endpoint of a Jaeger all-in-one or collector. */
  public static final String DEFAULT_OTLP_GRPC_ENDPOINT = "http://127.0.0.1:4317";

  /** Default protocol of the preset (works through HTTP proxies). */
  public static final Protocol DEFAULT_PROTOCOL = Protocol.HTTP_PROTOBUF;

  private static final String GROW = EnvReader.DEFAULT_PREFIX;
  private static final Duration DEFAULT_EXPORT_TIMEOUT =
      TracingConfig.BatchSettings.defaults().exportTimeout();

  private JaegerTracing() {}

  // ---------------------------------------------------------------------------------------------
  // Endpoint and exporter

  /**
   * The exporter URL for a protocol: for {@code http/protobuf}, an endpoint without a path (a base
   * such as {@code http://jaeger:4318}) gets {@code /v1/traces}; any other endpoint is used as is.
   * Without an endpoint, the Jaeger default of the protocol.
   *
   * @param protocol protocol
   * @param endpoint endpoint or {@code null}
   * @return the exporter URL
   */
  public static String resolveEndpoint(Protocol protocol, String endpoint) {
    if (endpoint == null || endpoint.isBlank()) {
      return protocol == Protocol.GRPC ? DEFAULT_OTLP_GRPC_ENDPOINT : DEFAULT_OTLP_HTTP_ENDPOINT;
    }
    if (protocol == Protocol.GRPC || hasPath(endpoint)) {
      return endpoint;
    }
    return endpoint.replaceAll("/+$", "") + "/v1/traces";
  }

  private static boolean hasPath(String url) {
    try {
      String path = URI.create(url).getPath();
      return path != null && !path.isEmpty() && !path.equals("/");
    } catch (IllegalArgumentException e) {
      return true;
    }
  }

  /**
   * The span exporter factory of the preset: resolves the endpoint ({@link #resolveEndpoint}) and
   * builds the official OTLP exporter for the protocol. {@code native} raises {@link
   * ConfigException} (Jaeger ingests OTLP).
   *
   * @return factory
   */
  public static SpanExporterFactory exporterFactory() {
    return config -> {
      if (config.protocol() == Protocol.NATIVE) {
        throw new ConfigException(
            "tracing: Jaeger has no native protocol; use grpc or http/protobuf");
      }
      String endpoint = resolveEndpoint(config.protocol(), config.endpoint().orElse(null));
      return OtlpExporters.create(config.toBuilder().endpoint(endpoint).build());
    };
  }

  // ---------------------------------------------------------------------------------------------
  // Environment

  /** First non-blank variable of a list. */
  private static final class Env {
    private final Map<String, String> env;

    Env(Map<String, String> env) {
      this.env = env;
    }

    String[] hit(String... names) {
      for (String name : names) {
        String v = env.get(name);
        if (v != null && !v.isBlank()) {
          return new String[] {name, v.trim()};
        }
      }
      return null;
    }

    String[] otlp(String suffix, String... growAliases) {
      List<String> names = new ArrayList<>();
      names.add(GROW + "OTLP_" + suffix);
      for (String alias : growAliases) {
        names.add(GROW + "OTLP_" + alias);
      }
      names.add("OTEL_EXPORTER_OTLP_TRACES_" + suffix);
      names.add("OTEL_EXPORTER_OTLP_" + suffix);
      return hit(names.toArray(new String[0]));
    }
  }

  private static ConfigException invalid(String variable, String expected) {
    return new ConfigException(
        "tracing: invalid value in environment variable "
            + variable
            + " (expected "
            + expected
            + ")");
  }

  /**
   * Parses an OpenTelemetry {@code k=v,k2=v2} list (keys and values trimmed and percent-decoded).
   *
   * @param raw text
   * @param variable variable name for the error message
   * @return the pairs in order
   * @throws ConfigException for a malformed list
   */
  static Map<String, String> parseKeyValues(String raw, String variable) {
    Map<String, String> out = new LinkedHashMap<>();
    for (String part : raw.split(",")) {
      if (part.isBlank()) {
        continue;
      }
      int eq = part.indexOf('=');
      if (eq <= 0 || part.substring(0, eq).isBlank()) {
        throw invalid(variable, "key=value pairs");
      }
      try {
        out.put(
            URLDecoder.decode(part.substring(0, eq).trim(), StandardCharsets.UTF_8),
            URLDecoder.decode(part.substring(eq + 1).trim(), StandardCharsets.UTF_8));
      } catch (IllegalArgumentException e) {
        throw invalid(variable, "valid percent-encoding");
      }
    }
    return out;
  }

  /**
   * The service name from the environment: {@code GROW_JAEGER_SERVICE_NAME}, {@code
   * OTEL_SERVICE_NAME}, then {@code service.name} in {@code OTEL_RESOURCE_ATTRIBUTES}.
   *
   * @param env variables
   * @return the name, or empty
   */
  public static Optional<String> serviceNameFromEnv(Map<String, String> env) {
    Env e = new Env(env);
    String[] hit = e.hit(GROW + "SERVICE_NAME", "OTEL_SERVICE_NAME");
    if (hit != null) {
      return Optional.of(hit[1]);
    }
    String[] attrs = e.hit("OTEL_RESOURCE_ATTRIBUTES");
    if (attrs != null) {
      String name = parseKeyValues(attrs[1], attrs[0]).get("service.name");
      if (name != null && !name.isBlank()) {
        return Optional.of(name);
      }
    }
    return Optional.empty();
  }

  /**
   * Builds a config from the process environment (see the class description).
   *
   * @param serviceName service name, or {@code null} to take it from the environment
   * @return the config
   * @throws ConfigException when no service name is configured or a variable is invalid
   */
  public static TracingConfig configFromEnv(String serviceName) {
    return configFromEnv(serviceName, System.getenv());
  }

  /**
   * Builds a config from an explicit variable map (see the class description).
   *
   * @param serviceName service name, or {@code null} to take it from the environment
   * @param env variables
   * @return the config
   * @throws ConfigException when no service name is configured or a variable is invalid
   */
  public static TracingConfig configFromEnv(String serviceName, Map<String, String> env) {
    String name = serviceName;
    if (name == null || name.isBlank()) {
      name =
          serviceNameFromEnv(env)
              .orElseThrow(
                  () ->
                      new ConfigException(
                          "tracing: no service name (set GROW_JAEGER_SERVICE_NAME or"
                              + " OTEL_SERVICE_NAME)"));
    }
    return resolveConfig(TracingConfig.builder(name).build(), env);
  }

  /**
   * Fills every field of {@code explicit} that is still at its default from the environment and
   * merges {@code OTEL_RESOURCE_ATTRIBUTES} beneath the explicit resource attributes. A field set
   * explicitly to its default value also takes the environment (Java configs cannot tell the two
   * apart). {@code serviceName}, {@code serviceVersion} and {@code environment} always come from
   * {@code explicit}.
   *
   * @param explicit the explicit config
   * @param env variables
   * @return the resolved config
   * @throws ConfigException when a variable is invalid
   */
  public static TracingConfig resolveConfig(TracingConfig explicit, Map<String, String> env) {
    Env e = new Env(env);
    TracingConfig.Builder b = explicit.toBuilder();

    Protocol protocol = explicit.protocol();
    String[] p = e.otlp("PROTOCOL");
    if (protocol == DEFAULT_PROTOCOL && p != null) {
      switch (p[1].toLowerCase(Locale.ROOT)) {
        case "grpc":
          protocol = Protocol.GRPC;
          break;
        case "http/protobuf":
          protocol = Protocol.HTTP_PROTOBUF;
          break;
        default:
          throw invalid(p[0], "grpc or http/protobuf");
      }
      b.protocol(protocol);
    }

    if (explicit.endpoint().isEmpty()) {
      String specific =
          protocol == Protocol.GRPC ? GROW + "OTLP_GRPC_ENDPOINT" : GROW + "OTLP_HTTP_ENDPOINT";
      String[] grow = e.hit(specific, GROW + "OTLP_ENDPOINT");
      String[] traces = e.hit("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
      String[] base = e.hit("OTEL_EXPORTER_OTLP_ENDPOINT");
      if (grow != null) {
        b.endpoint(resolveEndpoint(protocol, grow[1]));
      } else if (traces != null) {
        b.endpoint(traces[1]);
      } else if (base != null) {
        b.endpoint(
            protocol == Protocol.GRPC ? base[1] : base[1].replaceAll("/+$", "") + "/v1/traces");
      }
    }

    String[] headers = e.otlp("HEADERS");
    if (explicit.headers().isEmpty() && headers != null) {
      b.headers(parseKeyValues(headers[1], headers[0]));
    }

    String[] compression = e.otlp("COMPRESSION");
    if (explicit.compression() == Compression.GZIP && compression != null) {
      switch (compression[1].toLowerCase(Locale.ROOT)) {
        case "gzip":
          b.compression(Compression.GZIP);
          break;
        case "none":
          b.compression(Compression.NONE);
          break;
        default:
          throw invalid(compression[0], "gzip or none");
      }
    }

    String[] timeout = e.otlp("TIMEOUT");
    TracingConfig.BatchSettings batch = explicit.batch();
    if (batch.exportTimeout().equals(DEFAULT_EXPORT_TIMEOUT) && timeout != null) {
      Duration d =
          EnvReader.parseDuration(timeout[1])
              .filter(x -> !x.isZero())
              .orElseThrow(() -> invalid(timeout[0], "a duration such as 10000 or 10s"));
      b.batch(
          new TracingConfig.BatchSettings(
              batch.maxQueueSize(), batch.maxExportBatchSize(), batch.scheduledDelay(), d));
    }

    if (explicit.tls().isEmpty()) {
      String[] ca = e.otlp("CERTIFICATE", "CA_CERT");
      String[] cert = e.otlp("CLIENT_CERTIFICATE");
      String[] key = e.otlp("CLIENT_KEY");
      if (ca != null || cert != null || key != null) {
        TlsOptions.Builder t = TlsOptions.builder();
        if (ca != null) {
          t.caCertPath(Paths.get(ca[1]));
        }
        if (cert != null) {
          t.clientCertPath(Paths.get(cert[1]));
        }
        if (key != null) {
          t.clientKeyPath(Paths.get(key[1]));
        }
        b.tls(t.build());
      }
    }

    String[] resource = e.hit("OTEL_RESOURCE_ATTRIBUTES");
    if (resource != null) {
      Map<String, Object> merged = new LinkedHashMap<>(parseKeyValues(resource[1], resource[0]));
      merged.remove("service.name");
      merged.putAll(explicit.resourceAttributes());
      b.resourceAttributes(merged);
    }

    String[] sampler = e.hit("OTEL_TRACES_SAMPLER");
    if (sampler != null && isDefaultSampler(explicit)) {
      String name = sampler[1].toLowerCase(Locale.ROOT);
      if (!name.equals("jaeger_remote") && !name.equals("parentbased_jaeger_remote")) {
        SamplerType type = null;
        for (SamplerType t : SamplerType.values()) {
          if (t.wireValue().equals(name)) {
            type = t;
          }
        }
        if (type == null) {
          throw invalid(sampler[0], "a known sampler");
        }
        b.sampler(type);
        String[] arg = e.hit("OTEL_TRACES_SAMPLER_ARG");
        if (name.endsWith("traceidratio") && arg != null) {
          double ratio;
          try {
            ratio = Double.parseDouble(arg[1]);
          } catch (NumberFormatException ex) {
            throw invalid(arg[0], "a ratio between 0 and 1");
          }
          if (!(ratio >= 0 && ratio <= 1)) {
            throw invalid(arg[0], "a ratio between 0 and 1");
          }
          b.samplerRatio(ratio);
        }
      }
    }

    String[] propagators = e.hit("OTEL_PROPAGATORS");
    if (propagators != null && explicit.propagators().equals(TracingConfig.DEFAULT_PROPAGATORS)) {
      List<String> list = new ArrayList<>();
      for (String s : propagators[1].split(",")) {
        if (!s.isBlank()) {
          list.add(s.trim());
        }
      }
      b.propagators(list);
    }
    return b.build();
  }

  private static boolean isDefaultSampler(TracingConfig c) {
    return c.sampler() == SamplerType.PARENTBASED_ALWAYS_ON && c.samplerRatio() == 1.0;
  }

  /**
   * Remote sampling requested by {@code OTEL_TRACES_SAMPLER=jaeger_remote} (root sampler) or {@code
   * parentbased_jaeger_remote}, with {@code OTEL_TRACES_SAMPLER_ARG} {@code
   * endpoint=…,pollingIntervalMs=…,initialSamplingRate=…}.
   *
   * @param env variables
   * @return the settings, or empty when the variable does not ask for remote sampling
   * @throws ConfigException for an invalid argument
   */
  public static Optional<RemoteSampling> remoteSamplingFromEnv(Map<String, String> env) {
    Env e = new Env(env);
    String[] sampler = e.hit("OTEL_TRACES_SAMPLER");
    if (sampler == null) {
      return Optional.empty();
    }
    String name = sampler[1].toLowerCase(Locale.ROOT);
    if (!name.equals("jaeger_remote") && !name.equals("parentbased_jaeger_remote")) {
      return Optional.empty();
    }
    RemoteSampling r = RemoteSampling.defaults().withParentBased(name.startsWith("parentbased_"));
    String[] arg = e.hit("OTEL_TRACES_SAMPLER_ARG");
    if (arg != null) {
      Map<String, String> values = parseKeyValues(arg[1], arg[0]);
      if (values.containsKey("endpoint")) {
        r = r.withSamplingUrl(values.get("endpoint"));
      }
      if (values.containsKey("pollingIntervalMs")) {
        Duration d =
            EnvReader.parseDuration(values.get("pollingIntervalMs"))
                .filter(x -> !x.isZero())
                .orElseThrow(() -> invalid(arg[0], "pollingIntervalMs as a duration"));
        r = r.withPollInterval(d);
      }
      if (values.containsKey("initialSamplingRate")) {
        try {
          r = r.withInitialSamplingRate(Double.parseDouble(values.get("initialSamplingRate")));
        } catch (NumberFormatException ex) {
          throw invalid(arg[0], "initialSamplingRate as a number");
        }
      }
    }
    return Optional.of(r);
  }

  // ---------------------------------------------------------------------------------------------
  // Setup

  /**
   * Remote sampler settings for {@link Options#remoteSampling}.
   *
   * @param samplingUrl sampling endpoint; empty = {@link JaegerConfig#samplingUrl()} of the sampler
   *     config
   * @param pollInterval poll interval
   * @param initialSamplingRate probability until the first strategy arrives
   * @param parentBased wrap the sampler in {@code Sampler.parentBased} (default true)
   */
  public record RemoteSampling(
      Optional<String> samplingUrl,
      Duration pollInterval,
      double initialSamplingRate,
      boolean parentBased) {
    /**
     * Normalises nulls.
     *
     * @param samplingUrl URL
     * @param pollInterval interval
     * @param initialSamplingRate initial rate
     * @param parentBased parent based
     */
    public RemoteSampling {
      samplingUrl = samplingUrl == null ? Optional.empty() : samplingUrl;
      pollInterval =
          pollInterval == null ? JaegerRemoteSampler.DEFAULT_POLL_INTERVAL : pollInterval;
    }

    /**
     * The defaults: sampler config URL, 1 min, 0.001, parent based.
     *
     * @return settings
     */
    public static RemoteSampling defaults() {
      return new RemoteSampling(
          Optional.empty(),
          JaegerRemoteSampler.DEFAULT_POLL_INTERVAL,
          JaegerRemoteSampler.DEFAULT_INITIAL_SAMPLING_RATE,
          true);
    }

    /**
     * Copy with a sampling URL.
     *
     * @param url URL
     * @return settings
     */
    public RemoteSampling withSamplingUrl(String url) {
      return new RemoteSampling(
          Optional.ofNullable(url), pollInterval, initialSamplingRate, parentBased);
    }

    /**
     * Copy with a poll interval.
     *
     * @param interval interval
     * @return settings
     */
    public RemoteSampling withPollInterval(Duration interval) {
      return new RemoteSampling(samplingUrl, interval, initialSamplingRate, parentBased);
    }

    /**
     * Copy with an initial rate.
     *
     * @param rate 0..1
     * @return settings
     */
    public RemoteSampling withInitialSamplingRate(double rate) {
      return new RemoteSampling(samplingUrl, pollInterval, rate, parentBased);
    }

    /**
     * Copy with the parent-based flag.
     *
     * @param parentBased wrap in {@code Sampler.parentBased}
     * @return settings
     */
    public RemoteSampling withParentBased(boolean parentBased) {
      return new RemoteSampling(samplingUrl, pollInterval, initialSamplingRate, parentBased);
    }
  }

  /** Options of {@link #initTracing(TracingConfig, Options)}. */
  public static final class Options {
    private Map<String, String> env;
    private boolean ignoreEnv;
    private Sampler sampler;
    private JaegerRemoteSampler remoteSampler;
    private boolean remoteParentBased = true;
    private RemoteSampling remoteSampling;
    private JaegerConfig samplerConfig;

    private Options() {}

    /**
     * Starts empty options (process environment, sampler from the config).
     *
     * @return options
     */
    public static Options builder() {
      return new Options();
    }

    /**
     * Reads this variable map instead of the process environment.
     *
     * @param env variables
     * @return this
     */
    public Options env(Map<String, String> env) {
      this.env = env;
      return this;
    }

    /**
     * Ignores the environment: the config is used as is.
     *
     * @return this
     */
    public Options ignoreEnv() {
      this.ignoreEnv = true;
      return this;
    }

    /**
     * Uses this head sampler instead of {@code config.sampler()} (not wrapped, not closed).
     *
     * @param sampler sampler
     * @return this
     */
    public Options sampler(Sampler sampler) {
      this.sampler = sampler;
      return this;
    }

    /**
     * Uses an existing remote sampler (wrapped in {@code Sampler.parentBased} unless {@code
     * parentBased} is false; not closed by the handle).
     *
     * @param sampler remote sampler
     * @param parentBased wrap in {@code Sampler.parentBased}
     * @return this
     */
    public Options remoteSampler(JaegerRemoteSampler sampler, boolean parentBased) {
      this.remoteSampler = sampler;
      this.remoteParentBased = parentBased;
      return this;
    }

    /**
     * Creates a remote sampler for the config's service name (closed by the handle).
     *
     * @param settings sampler settings
     * @return this
     */
    public Options remoteSampling(RemoteSampling settings) {
      this.remoteSampling = settings;
      return this;
    }

    /**
     * Transport settings of a created remote sampler (default: {@link JaegerConfig#fromEnv} of the
     * same environment).
     *
     * @param config configuration
     * @return this
     */
    public Options samplerConfig(JaegerConfig config) {
      this.samplerConfig = config;
      return this;
    }

    /** Sets the sampler config only when none was set (used by {@link JaegerClient}). */
    Options defaultSamplerConfig(JaegerConfig config) {
      if (samplerConfig == null) {
        samplerConfig = config;
      }
      return this;
    }
  }

  /**
   * Starts tracing with the process environment beneath {@code config} (see the class description).
   *
   * @param config tracing config
   * @return the running setup; shut it down before exit
   */
  public static JaegerTracingHandle initTracing(TracingConfig config) {
    return initTracing(config, Options.builder());
  }

  /**
   * Starts tracing with the service name and every setting from the process environment.
   *
   * @param serviceName service name or {@code null} (environment)
   * @return the running setup
   */
  public static JaegerTracingHandle initTracingFromEnv(String serviceName) {
    return initTracingFromEnv(serviceName, System.getenv());
  }

  /**
   * Starts tracing with the service name and every setting from a variable map.
   *
   * @param serviceName service name or {@code null} (environment)
   * @param env variables
   * @return the running setup
   */
  public static JaegerTracingHandle initTracingFromEnv(
      String serviceName, Map<String, String> env) {
    return initTracing(configFromEnv(serviceName, env), Options.builder().env(env));
  }

  /**
   * Starts tracing.
   *
   * @param config tracing config
   * @param options environment and sampler options
   * @return the running setup
   * @throws ConfigException for invalid settings
   */
  public static JaegerTracingHandle initTracing(TracingConfig config, Options options) {
    Map<String, String> env =
        options.ignoreEnv ? null : options.env != null ? options.env : System.getenv();
    TracingConfig resolved = env == null ? config : resolveConfig(config, env);
    resolved =
        resolved.toBuilder()
            .endpoint(resolveEndpoint(resolved.protocol(), resolved.endpoint().orElse(null)))
            .build();
    String endpoint = resolved.endpoint().orElseThrow();
    if (options.sampler != null) {
      return new JaegerTracingHandle(
          SamplerTracing.initWith(resolved, exporterFactory(), options.sampler),
          endpoint,
          null,
          false);
    }
    JaegerRemoteSampler remote = options.remoteSampler;
    boolean parentBased = options.remoteParentBased;
    boolean owned = false;
    if (remote == null) {
      RemoteSampling settings = options.remoteSampling;
      if (settings == null && env != null && isDefaultSampler(config)) {
        settings = remoteSamplingFromEnv(env).orElse(null);
      }
      if (settings != null) {
        JaegerConfig base =
            options.samplerConfig != null
                ? options.samplerConfig
                : env != null ? JaegerConfig.fromEnv(env) : JaegerConfig.builder().build();
        remote =
            JaegerRemoteSampler.builder(resolved.serviceName())
                .config(base)
                .samplingUrl(settings.samplingUrl().orElse(null))
                .pollInterval(settings.pollInterval())
                .initialSamplingRate(settings.initialSamplingRate())
                .build();
        parentBased = settings.parentBased();
        owned = true;
      }
    }
    if (remote == null) {
      return new JaegerTracingHandle(
          Tracing.initWith(resolved, exporterFactory()), endpoint, null, false);
    }
    try {
      Sampler s = parentBased ? Sampler.parentBased(remote) : remote;
      TracingHandle handle = SamplerTracing.initWith(resolved, exporterFactory(), s);
      return new JaegerTracingHandle(handle, endpoint, remote, owned);
    } catch (RuntimeException ex) {
      if (owned) {
        remote.close();
      }
      throw ex;
    }
  }
}
