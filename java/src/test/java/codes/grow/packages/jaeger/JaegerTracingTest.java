package codes.grow.packages.jaeger;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.MockServer;
import codes.grow.packages.jaeger.core.TlsOptions;
import codes.grow.packages.jaeger.tracing.SamplerTracing;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.TracingConfig.Compression;
import codes.grow.packages.jaeger.tracing.TracingConfig.Protocol;
import codes.grow.packages.jaeger.tracing.TracingConfig.SamplerType;
import io.opentelemetry.api.GlobalOpenTelemetry;
import io.opentelemetry.api.trace.Span;
import io.opentelemetry.sdk.trace.export.SpanExporter;
import io.opentelemetry.sdk.trace.samplers.Sampler;
import java.nio.file.Paths;
import java.time.Duration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class JaegerTracingTest {
  private static final String G = "GROW_JAEGER_";

  private MockServer server;

  @BeforeEach
  void start() throws Exception {
    server = MockServer.start();
  }

  @AfterEach
  void stop() {
    server.close();
  }

  private static TracingConfig.Builder cfg(String name) {
    return TracingConfig.builder(name).setGlobal(false);
  }

  @Test
  void endpoints() {
    assertThat(JaegerTracing.resolveEndpoint(Protocol.HTTP_PROTOBUF, null))
        .isEqualTo(JaegerTracing.DEFAULT_OTLP_HTTP_ENDPOINT);
    assertThat(JaegerTracing.resolveEndpoint(Protocol.GRPC, " "))
        .isEqualTo(JaegerTracing.DEFAULT_OTLP_GRPC_ENDPOINT);
    assertThat(JaegerTracing.resolveEndpoint(Protocol.HTTP_PROTOBUF, "http://j:4318/"))
        .isEqualTo("http://j:4318/v1/traces");
    assertThat(JaegerTracing.resolveEndpoint(Protocol.HTTP_PROTOBUF, "http://j:4318/custom"))
        .isEqualTo("http://j:4318/custom");
    assertThat(JaegerTracing.resolveEndpoint(Protocol.GRPC, "http://j:4317"))
        .isEqualTo("http://j:4317");
    assertThat(JaegerTracing.resolveEndpoint(Protocol.HTTP_PROTOBUF, "http://[bad"))
        .isEqualTo("http://[bad");
    assertThat(JaegerTracing.DEFAULT_PROTOCOL).isEqualTo(Protocol.HTTP_PROTOBUF);
  }

  @Test
  void exporterFactory() {
    SpanExporter http = JaegerTracing.exporterFactory().create(cfg("s").build());
    assertThat(http.toString()).contains("4318/v1/traces");
    http.shutdown();
    SpanExporter grpc =
        JaegerTracing.exporterFactory().create(cfg("s").protocol(Protocol.GRPC).build());
    assertThat(grpc.toString()).contains("4317");
    grpc.shutdown();
    assertThatThrownBy(
            () ->
                JaegerTracing.exporterFactory().create(cfg("s").protocol(Protocol.NATIVE).build()))
        .isInstanceOf(ConfigException.class);
  }

  @Test
  void serviceNameFromEnv() {
    assertThat(JaegerTracing.serviceNameFromEnv(Map.of())).isEmpty();
    assertThat(
            JaegerTracing.serviceNameFromEnv(
                Map.of(G + "SERVICE_NAME", "grow", "OTEL_SERVICE_NAME", "otel")))
        .contains("grow");
    assertThat(JaegerTracing.serviceNameFromEnv(Map.of("OTEL_SERVICE_NAME", "otel")))
        .contains("otel");
    assertThat(
            JaegerTracing.serviceNameFromEnv(
                Map.of("OTEL_RESOURCE_ATTRIBUTES", "a=b,service.name=res")))
        .contains("res");
    assertThat(JaegerTracing.serviceNameFromEnv(Map.of("OTEL_RESOURCE_ATTRIBUTES", "a=b")))
        .isEmpty();
    assertThat(JaegerTracing.configFromEnv(null, Map.of("OTEL_SERVICE_NAME", "o")).serviceName())
        .isEqualTo("o");
    assertThat(
            JaegerTracing.configFromEnv("explicit", Map.of("OTEL_SERVICE_NAME", "o")).serviceName())
        .isEqualTo("explicit");
    assertThatThrownBy(() -> JaegerTracing.configFromEnv(" ", Map.of()))
        .isInstanceOf(ConfigException.class);
    assertThat(JaegerTracing.configFromEnv("p")).isNotNull();
  }

  @Test
  void resolveConfigPrecedence() {
    Map<String, String> env = new HashMap<>();
    env.put(G + "OTLP_PROTOCOL", "grpc");
    env.put("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
    env.put(G + "OTLP_GRPC_ENDPOINT", "http://jaeger:4317");
    env.put(G + "OTLP_HTTP_ENDPOINT", "http://jaeger:4318");
    env.put(G + "OTLP_HEADERS", "x-a=1,x%20b=two%3D");
    env.put("OTEL_EXPORTER_OTLP_TRACES_COMPRESSION", "none");
    env.put("OTEL_EXPORTER_OTLP_TIMEOUT", "5s");
    env.put(G + "OTLP_CA_CERT", "/ca.pem");
    env.put("OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE", "/c.pem");
    env.put("OTEL_EXPORTER_OTLP_CLIENT_KEY", "/k.pem");
    env.put("OTEL_RESOURCE_ATTRIBUTES", "team=a,region=eu,service.name=ignored");
    env.put("OTEL_TRACES_SAMPLER", "parentbased_traceidratio");
    env.put("OTEL_TRACES_SAMPLER_ARG", "0.25");
    env.put("OTEL_PROPAGATORS", "tracecontext, b3 ,none,");
    TracingConfig c = JaegerTracing.resolveConfig(cfg("s").build(), env);
    assertThat(c.protocol()).isEqualTo(Protocol.GRPC);
    assertThat(c.endpoint()).contains("http://jaeger:4317");
    assertThat(c.headers()).containsEntry("x-a", "1").containsEntry("x b", "two=");
    assertThat(c.compression()).isEqualTo(Compression.NONE);
    assertThat(c.batch().exportTimeout()).isEqualTo(Duration.ofSeconds(5));
    TlsOptions tls = c.tls().orElseThrow();
    assertThat(tls.caCertPath()).contains(Paths.get("/ca.pem"));
    assertThat(tls.clientCertPath()).contains(Paths.get("/c.pem"));
    assertThat(tls.clientKeyPath()).contains(Paths.get("/k.pem"));
    assertThat(c.resourceAttributes())
        .containsEntry("team", "a")
        .containsEntry("region", "eu")
        .doesNotContainKey("service.name");
    assertThat(c.sampler()).isEqualTo(SamplerType.PARENTBASED_TRACEIDRATIO);
    assertThat(c.samplerRatio()).isEqualTo(0.25);
    assertThat(c.propagators()).containsExactly("tracecontext", "b3");

    // explicit values win; resource attributes merge per key
    TracingConfig explicit =
        cfg("s")
            .protocol(Protocol.GRPC)
            .endpoint("http://explicit:4317")
            .header("h", "v")
            .compression(Compression.NONE)
            .batch(
                new TracingConfig.BatchSettings(
                    10, 5, Duration.ofSeconds(1), Duration.ofSeconds(2)))
            .tls(TlsOptions.defaults())
            .resourceAttributes(Map.of("team", "explicit"))
            .sampler(SamplerType.ALWAYS_ON)
            .propagators(List.of("b3multi"))
            .build();
    TracingConfig r = JaegerTracing.resolveConfig(explicit, env);
    assertThat(r.endpoint()).contains("http://explicit:4317");
    assertThat(r.headers()).containsOnlyKeys("h");
    assertThat(r.batch().exportTimeout()).isEqualTo(Duration.ofSeconds(2));
    assertThat(r.tls().orElseThrow().caCertPath()).isEmpty();
    assertThat(r.resourceAttributes())
        .containsEntry("team", "explicit")
        .containsEntry("region", "eu");
    assertThat(r.sampler()).isEqualTo(SamplerType.ALWAYS_ON);
    assertThat(r.propagators()).containsExactly("b3multi");

    // HTTP: GROW base gets /v1/traces
    env.remove(G + "OTLP_PROTOCOL");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).endpoint())
        .contains("http://jaeger:4318/v1/traces");
  }

  @Test
  void endpointChain() {
    Map<String, String> env = new HashMap<>();
    env.put("OTEL_EXPORTER_OTLP_ENDPOINT", "http://base:4318/");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).endpoint())
        .contains("http://base:4318/v1/traces");
    assertThat(
            JaegerTracing.resolveConfig(cfg("s").protocol(Protocol.GRPC).build(), env).endpoint())
        .contains("http://base:4318/");
    env.put("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://traces:4318/v1/traces");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).endpoint())
        .contains("http://traces:4318/v1/traces");
    env.put(G + "OTLP_ENDPOINT", "http://grow:4318");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).endpoint())
        .contains("http://grow:4318/v1/traces");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), Map.of()).endpoint()).isEmpty();
    env.put("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "HTTP/PROTOBUF");
    env.put("OTEL_EXPORTER_OTLP_COMPRESSION", "GZIP");
    env.put("OTEL_EXPORTER_OTLP_CERTIFICATE", "/only-ca.pem");
    env.put("OTEL_TRACES_SAMPLER", "always_off");
    env.put("OTEL_TRACES_SAMPLER_ARG", "0.5");
    TracingConfig c = JaegerTracing.resolveConfig(cfg("s").build(), env);
    assertThat(c.protocol()).isEqualTo(Protocol.HTTP_PROTOBUF);
    assertThat(c.compression()).isEqualTo(Compression.GZIP);
    assertThat(c.tls().orElseThrow().clientCertPath()).isEmpty();
    assertThat(c.sampler()).isEqualTo(SamplerType.ALWAYS_OFF);
    assertThat(c.samplerRatio()).isEqualTo(1.0);
    env.put("OTEL_TRACES_SAMPLER", "traceidratio");
    env.remove("OTEL_TRACES_SAMPLER_ARG");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).samplerRatio()).isEqualTo(1.0);
    env.put("OTEL_TRACES_SAMPLER", "jaeger_remote");
    assertThat(JaegerTracing.resolveConfig(cfg("s").build(), env).sampler())
        .isEqualTo(SamplerType.PARENTBASED_ALWAYS_ON);
  }

  @Test
  void invalidEnvNamesVariableNotValue() {
    List<Map<String, String>> bad =
        List.of(
            Map.of(G + "OTLP_PROTOCOL", "smoke-signals"),
            Map.of("OTEL_EXPORTER_OTLP_HEADERS", "novalue"),
            Map.of("OTEL_EXPORTER_OTLP_HEADERS", "=v"),
            Map.of("OTEL_EXPORTER_OTLP_HEADERS", "k=%zz"),
            Map.of("OTEL_EXPORTER_OTLP_COMPRESSION", "smoke-signals"),
            Map.of("OTEL_EXPORTER_OTLP_TIMEOUT", "smoke-signals"),
            Map.of("OTEL_EXPORTER_OTLP_TIMEOUT", "0"),
            Map.of("OTEL_TRACES_SAMPLER", "smoke-signals"),
            Map.of(
                "OTEL_TRACES_SAMPLER", "traceidratio", "OTEL_TRACES_SAMPLER_ARG", "smoke-signals"),
            Map.of("OTEL_TRACES_SAMPLER", "traceidratio", "OTEL_TRACES_SAMPLER_ARG", "7"));
    for (Map<String, String> env : bad) {
      assertThatThrownBy(() -> JaegerTracing.resolveConfig(cfg("s").build(), env))
          .as(env.toString())
          .isInstanceOf(ConfigException.class)
          .message()
          .doesNotContain("smoke-signals");
    }
  }

  @Test
  void remoteSamplingFromEnv() {
    assertThat(JaegerTracing.remoteSamplingFromEnv(Map.of())).isEmpty();
    assertThat(JaegerTracing.remoteSamplingFromEnv(Map.of("OTEL_TRACES_SAMPLER", "always_on")))
        .isEmpty();
    JaegerTracing.RemoteSampling root =
        JaegerTracing.remoteSamplingFromEnv(Map.of("OTEL_TRACES_SAMPLER", "jaeger_remote"))
            .orElseThrow();
    assertThat(root.parentBased()).isFalse();
    assertThat(root.samplingUrl()).isEmpty();
    JaegerTracing.RemoteSampling pb =
        JaegerTracing.remoteSamplingFromEnv(
                Map.of(
                    "OTEL_TRACES_SAMPLER",
                    "parentbased_jaeger_remote",
                    "OTEL_TRACES_SAMPLER_ARG",
                    "endpoint=http://j:5778/sampling,pollingIntervalMs=5000,initialSamplingRate=0.5"))
            .orElseThrow();
    assertThat(pb.parentBased()).isTrue();
    assertThat(pb.samplingUrl()).contains("http://j:5778/sampling");
    assertThat(pb.pollInterval()).isEqualTo(Duration.ofSeconds(5));
    assertThat(pb.initialSamplingRate()).isEqualTo(0.5);
    assertThat(
            JaegerTracing.remoteSamplingFromEnv(
                    Map.of(
                        "OTEL_TRACES_SAMPLER", "jaeger_remote", "OTEL_TRACES_SAMPLER_ARG", "x=1"))
                .orElseThrow()
                .pollInterval())
        .isEqualTo(JaegerRemoteSampler.DEFAULT_POLL_INTERVAL);
    for (String arg :
        List.of("pollingIntervalMs=soon", "pollingIntervalMs=0", "initialSamplingRate=x")) {
      assertThatThrownBy(
              () ->
                  JaegerTracing.remoteSamplingFromEnv(
                      Map.of(
                          "OTEL_TRACES_SAMPLER", "jaeger_remote", "OTEL_TRACES_SAMPLER_ARG", arg)))
          .isInstanceOf(ConfigException.class);
    }
    JaegerTracing.RemoteSampling n = new JaegerTracing.RemoteSampling(null, null, 0.1, true);
    assertThat(n.samplingUrl()).isEmpty();
    assertThat(n.pollInterval()).isEqualTo(JaegerRemoteSampler.DEFAULT_POLL_INTERVAL);
  }

  @Test
  void initTracingExportsToResolvedEndpoint() {
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("export-svc").endpoint(server.url()).compression(Compression.NONE).build(),
            JaegerTracing.Options.builder().ignoreEnv())) {
      assertThat(h.endpoint()).isEqualTo(server.url() + "/v1/traces");
      assertThat(h.remoteSampler()).isEmpty();
      assertThat(h.config().serviceName()).isEqualTo("export-svc");
      Span span = h.tracer("t", "1.0").spanBuilder("op").startSpan();
      span.end();
      h.tracer("t").spanBuilder("op2").startSpan().end();
      h.forceFlush();
      h.forceFlush(Duration.ofSeconds(5));
      h.forceFlushAsync(Duration.ofSeconds(5)).join();
      assertThat(server.requests()).isNotEmpty();
      assertThat(server.requests().get(0).uri()).isEqualTo("/v1/traces");
      assertThat(server.requests().get(0).header("Content-Type"))
          .isEqualTo("application/x-protobuf");
      assertThat(h.openTelemetry()).isNotNull();
      assertThat(h.tracerProvider()).isNotNull();
      assertThat(h.propagators()).isNotNull();
      assertThat(h.handle()).isNotNull();
      assertThat(h.toString()).contains("/v1/traces");
      h.shutdownAsync().join();
      assertThat(h.isShutdown()).isTrue();
    }
  }

  @Test
  void initTracingWithEnvMapAndSamplers() {
    Map<String, String> env = new HashMap<>();
    env.put(G + "OTLP_HTTP_ENDPOINT", server.url());
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("env-svc").build(), JaegerTracing.Options.builder().env(env))) {
      assertThat(h.endpoint()).isEqualTo(server.url() + "/v1/traces");
    }
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("custom").build(), JaegerTracing.Options.builder().sampler(Sampler.alwaysOff()))) {
      assertThat(h.tracerProvider().getSampler()).isEqualTo(Sampler.alwaysOff());
    }
    try (JaegerTracingHandle h = JaegerTracing.initTracing(cfg("plain").build())) {
      assertThat(h.endpoint()).isNotBlank();
    }

    // an existing remote sampler is wrapped and never closed by the handle
    JaegerRemoteSampler existing =
        JaegerRemoteSampler.builder("svc")
            .samplingUrl(server.url() + "/sampling")
            .autoStart(false)
            .build();
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("remote").build(),
            JaegerTracing.Options.builder().ignoreEnv().remoteSampler(existing, true))) {
      assertThat(h.remoteSampler()).contains(existing);
      assertThat(h.tracerProvider().getSampler().getDescription()).startsWith("ParentBased");
    }
    assertThat(existing.isClosed()).isFalse();
    existing.close();

    // remoteSampling creates and owns a sampler (root, not parent based)
    JaegerRemoteSampler owned;
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("owned").build(),
            JaegerTracing.Options.builder()
                .ignoreEnv()
                .remoteSampling(
                    JaegerTracing.RemoteSampling.defaults()
                        .withSamplingUrl(server.url() + "/sampling")
                        .withPollInterval(Duration.ofHours(1))
                        .withInitialSamplingRate(1)
                        .withParentBased(false)))) {
      owned = h.remoteSampler().orElseThrow();
      assertThat(owned.serviceName()).isEqualTo("owned");
      assertThat(owned.initialSamplingRate()).isEqualTo(1.0);
      assertThat(h.tracerProvider().getSampler()).isSameAs(owned);
    }
    assertThat(owned.isClosed()).isTrue();

    // the environment asks for remote sampling; URL from the sampler config
    env.put("OTEL_TRACES_SAMPLER", "parentbased_jaeger_remote");
    env.put(G + "SAMPLING_URL", server.url() + "/sampling");
    GlobalOpenTelemetry.resetForTest();
    try (JaegerTracingHandle h = JaegerTracing.initTracingFromEnv("from-env", env)) {
      JaegerRemoteSampler s = h.remoteSampler().orElseThrow();
      assertThat(s.samplingUrl()).isEqualTo(server.url() + "/sampling");
      assertThat(s.awaitFirstPoll(Duration.ofSeconds(10))).isTrue();
    } finally {
      GlobalOpenTelemetry.resetForTest();
    }
    // an explicit sampler in the config wins over jaeger_remote
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("explicit").sampler(SamplerType.ALWAYS_ON).build(),
            JaegerTracing.Options.builder().env(env))) {
      assertThat(h.remoteSampler()).isEmpty();
    }
    // a sampler config passed in options
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("cfg").build(),
            JaegerTracing.Options.builder()
                .ignoreEnv()
                .remoteSampling(JaegerTracing.RemoteSampling.defaults())
                .samplerConfig(JaegerConfig.builder().samplingUrl(server.url() + "/s2").build()))) {
      assertThat(h.remoteSampler().orElseThrow().samplingUrl()).isEqualTo(server.url() + "/s2");
    }
    try (JaegerTracingHandle h =
        JaegerTracing.initTracing(
            cfg("defaults").build(),
            JaegerTracing.Options.builder()
                .ignoreEnv()
                .remoteSampling(JaegerTracing.RemoteSampling.defaults()))) {
      assertThat(h.remoteSampler().orElseThrow().samplingUrl())
          .isEqualTo(JaegerConfig.DEFAULT_SAMPLING_URL);
    }
    try (JaegerTracingHandle h = JaegerTracing.initTracingFromEnv("x")) {
      assertThat(h.endpoint()).isNotBlank();
    } finally {
      GlobalOpenTelemetry.resetForTest();
    }
  }

  @Test
  void ownedSamplerClosedWhenSetupFails() {
    assertThatThrownBy(
            () ->
                JaegerTracing.initTracing(
                    cfg("bad").propagators(List.of("nope")).build(),
                    JaegerTracing.Options.builder()
                        .ignoreEnv()
                        .remoteSampling(
                            JaegerTracing.RemoteSampling.defaults()
                                .withSamplingUrl(server.url() + "/sampling"))))
        .isInstanceOf(ConfigException.class);
    JaegerRemoteSampler existing = JaegerRemoteSampler.builder("svc").autoStart(false).build();
    assertThatThrownBy(
            () ->
                JaegerTracing.initTracing(
                    cfg("bad").propagators(List.of("nope")).build(),
                    JaegerTracing.Options.builder().ignoreEnv().remoteSampler(existing, false)))
        .isInstanceOf(ConfigException.class);
    assertThat(existing.isClosed()).isFalse();
    existing.close();
  }

  @Test
  void samplerTracingGlobal() {
    GlobalOpenTelemetry.resetForTest();
    try {
      var h =
          SamplerTracing.initWith(
              TracingConfig.builder("g").build(),
              JaegerTracing.exporterFactory(),
              Sampler.alwaysOn());
      assertThatThrownBy(
              () ->
                  SamplerTracing.initWith(
                      TracingConfig.builder("g2").build(),
                      JaegerTracing.exporterFactory(),
                      Sampler.alwaysOn()))
          .isInstanceOf(ConfigException.class);
      h.shutdown();
    } finally {
      GlobalOpenTelemetry.resetForTest();
    }
  }

  @Test
  void samplingPresets() {
    assertThat(SamplingPreset.development()).isEqualTo(SamplingPreset.DEVELOPMENT);
    assertThat(SamplingPreset.production().sampler())
        .isEqualTo(SamplerType.PARENTBASED_TRACEIDRATIO);
    assertThat(SamplingPreset.production().ratio()).isEqualTo(0.1);
    assertThat(SamplingPreset.followParent().sampler())
        .isEqualTo(SamplerType.PARENTBASED_ALWAYS_ON);
    assertThat(SamplingPreset.off().sampler()).isEqualTo(SamplerType.ALWAYS_OFF);
    assertThat(SamplingPreset.ratio(0).sampler()).isEqualTo(SamplerType.PARENTBASED_ALWAYS_OFF);
    assertThat(SamplingPreset.ratio(2)).isEqualTo(SamplingPreset.FOLLOW_PARENT);
    assertThat(SamplingPreset.ratio(0.3).ratio()).isEqualTo(0.3);
    TracingConfig c = SamplingPreset.PRODUCTION.applyTo(cfg("p")).build();
    assertThat(c.sampler()).isEqualTo(SamplerType.PARENTBASED_TRACEIDRATIO);
    assertThat(c.samplerRatio()).isEqualTo(0.1);
    assertThatThrownBy(() -> new SamplingPreset(SamplerType.ALWAYS_ON, 2))
        .isInstanceOf(IllegalArgumentException.class);
  }
}
