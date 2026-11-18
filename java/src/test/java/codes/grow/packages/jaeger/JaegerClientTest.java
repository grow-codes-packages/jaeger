package codes.grow.packages.jaeger;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import codes.grow.packages.jaeger.core.AuthenticationException;
import codes.grow.packages.jaeger.core.CancelledException;
import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.EnvReader;
import codes.grow.packages.jaeger.core.HealthStatus;
import codes.grow.packages.jaeger.core.Hooks;
import codes.grow.packages.jaeger.core.Logger;
import codes.grow.packages.jaeger.core.MockServer;
import codes.grow.packages.jaeger.core.RetryPolicy;
import codes.grow.packages.jaeger.core.Secret;
import codes.grow.packages.jaeger.core.TlsOptions;
import codes.grow.packages.jaeger.core.UnsupportedException;
import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import java.net.ServerSocket;
import java.time.Duration;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class JaegerClientTest {
  private static final String P = EnvReader.DEFAULT_PREFIX;

  private MockServer server;

  @BeforeEach
  void start() throws Exception {
    server = MockServer.start();
  }

  @AfterEach
  void stop() {
    server.close();
  }

  private static JaegerConfig.Builder cfg() {
    return JaegerConfig.builder();
  }

  private JaegerClient client() {
    return JaegerClient.builder()
        .url(server.url())
        .token("tok-123456")
        .retry(RetryPolicy.builder().initialBackoff(Duration.ofMillis(1)).build())
        .build();
  }

  @Test
  void healthVariants() {
    server
        .enqueue(200, "<html>ui</html>", "Content-Type", "text/html")
        .enqueue(500, "oops", "Content-Type", "text/plain");
    try (var c = client()) {
      HealthStatus h = c.health();
      assertThat(h.healthy()).isTrue();
      assertThat(h.status()).isEqualTo("ok");
      HealthStatus bad = c.healthAsync().join();
      assertThat(bad.healthy()).isFalse();
      assertThat(bad.details()).containsEntry("httpStatus", "500");
      assertThat(server.requests().get(0).uri()).isEqualTo("/");
      assertThat(server.requests().get(0).header("Authorization")).isNull();
    }
  }

  @Test
  void healthWithHealthcheckDetail() {
    server
        .enqueue(200, "<html/>", "Content-Type", "text/html")
        .enqueue(200, Fixtures.body("health.status_ok"))
        .enqueue(200, "<html/>", "Content-Type", "text/html")
        .enqueue(503, "{\"healthy\":false,\"status\":\"StatusRecoverableError\"}")
        .enqueue(500, "down", "Content-Type", "text/plain");
    try (var c =
        JaegerClient.builder()
            .url(server.url())
            .healthUrl(server.url() + "/status")
            .retry(RetryPolicy.none())
            .build()) {
      HealthStatus h = c.health();
      assertThat(h.healthy()).isTrue();
      assertThat(h.status()).isEqualTo("ok");
      assertThat(h.details()).containsEntry("healthcheck.status", "StatusOK");
      HealthStatus degraded = c.health();
      assertThat(degraded.healthy()).isFalse();
      assertThat(degraded.status()).isEqualTo("degraded");
      assertThat(degraded.details()).containsEntry("healthcheck.status", "StatusRecoverableError");
      // the query root failing skips the detail call
      assertThat(c.health().healthy()).isFalse();
      assertThat(server.requests()).hasSize(5);
      assertThat(server.requests().get(1).uri()).isEqualTo("/status");
    }
  }

  @Test
  void core_health_unreachable() throws Exception {
    int port;
    try (ServerSocket s = new ServerSocket(0)) {
      port = s.getLocalPort();
    }
    String url = "http://127.0.0.1:" + port;
    try (var c = JaegerClient.builder().url(url).build()) {
      HealthStatus h = c.health();
      assertThat(h.healthy()).isFalse();
      assertThat(h.status()).isEqualTo("unreachable");
      assertThat(c.healthAsync().join().status()).isEqualTo("unreachable");
    }
  }

  @Test
  void healthThrowsOnlyForCancel() {
    JaegerClient c = client();
    c.close();
    assertThatThrownBy(c::health).isInstanceOf(CancelledException.class);
    assertThatThrownBy(() -> c.healthAsync().join()).hasCauseInstanceOf(CancelledException.class);
  }

  @Test
  void pingSendsBearerToken() {
    server
        .enqueue(200, "{}")
        .enqueue(401, "{\"message\":\"bad token tok-123456\"}")
        .enqueue(200, "{}");
    try (var c = client()) {
      assertThat(c.ping()).isPositive();
      assertThat(server.requests().get(0).header("Authorization")).isEqualTo("Bearer tok-123456");
      assertThat(server.requests().get(0).uri()).isEqualTo("/api/v3/services");
      assertThatThrownBy(c::ping)
          .isInstanceOf(AuthenticationException.class)
          .message()
          .doesNotContain("tok-123456");
      assertThat(c.pingAsync().join()).isPositive();
    }
  }

  @Test
  void pingWithoutToken() {
    server.enqueue(200, "{}");
    try (var c = JaegerClient.builder().url(server.url()).build()) {
      c.ping();
      assertThat(server.requests().get(0).header("Authorization")).isNull();
    }
  }

  @Test
  void serverVersion() {
    String html =
        "<html><script>// JAEGER_VERSION\n"
            + Fixtures.load("serverVersion.index_html")
                .path("response")
                .path("bodyExcerpt")
                .asText()
            + "</script></html>";
    server
        .enqueue(200, html, "Content-Type", "text/html")
        .enqueue(404, "404 page not found", "Content-Type", "text/plain")
        .enqueue(200, "<html>no version</html>", "Content-Type", "text/html")
        .enqueue(200, html, "Content-Type", "text/html");
    try (var c = client()) {
      assertThat(c.serverVersion()).isEqualTo("2.23.0");
      assertThatThrownBy(c::serverVersion).isInstanceOf(UnsupportedException.class);
      assertThatThrownBy(c::serverVersion).isInstanceOf(UnsupportedException.class);
      JaegerBuildInfo info = c.buildInfoAsync().join().orElseThrow();
      assertThat(info.gitVersion()).contains("v2.23.0");
      assertThat(info.gitCommit()).contains("d723f91447f29a345c288f37eb8d1ce7807bc0be");
      assertThat(info.buildDate()).contains("2026-11-04T16:02:09Z");
      assertThat(server.requests().get(0).header("Authorization")).isEqualTo("Bearer tok-123456");
    }
  }

  @Test
  void serverVersionMetricsFallback() {
    String metrics =
        "# HELP target_info Target metadata\n"
            + "target_info{service_instance_id=\"x\",service_name=\"jaeger\","
            + "service_version=\"v2.23.0\"} 1\n";
    server
        .enqueue(404, "404 page not found", "Content-Type", "text/plain")
        .enqueue(200, metrics, "Content-Type", "text/plain")
        .enqueue(200, "<html/>", "Content-Type", "text/html")
        .enqueue(200, "other_metric 1\n", "Content-Type", "text/plain")
        .enqueue(200, "<html/>", "Content-Type", "text/html")
        .enqueue(404, "", "Content-Type", "text/plain")
        .enqueue(200, "<html>JAEGER_VERSION = {broken};</html>", "Content-Type", "text/html")
        .enqueue(200, "target_info{service_version=\"3.0.0\"} 1\n", "Content-Type", "text/plain")
        .enqueue(200, "JAEGER_VERSION = {\"gitVersion\":\"\"};", "Content-Type", "text/html")
        .enqueue(500, "x", "Content-Type", "text/plain");
    try (var c =
        JaegerClient.builder()
            .url(server.url())
            .metricsUrl(server.url() + "/metrics")
            .retry(RetryPolicy.none())
            .build()) {
      assertThat(c.serverVersion()).isEqualTo("2.23.0");
      assertThatThrownBy(c::serverVersion).isInstanceOf(UnsupportedException.class);
      assertThatThrownBy(c::serverVersion).isInstanceOf(UnsupportedException.class);
      assertThat(c.serverVersionAsync().join()).isEqualTo("3.0.0");
      assertThatThrownBy(c::serverVersion).isNotInstanceOf(UnsupportedException.class);
      assertThat(server.requests().get(1).uri()).isEqualTo("/metrics");
    }
    assertThat(JaegerClient.parseBuildInfo("JAEGER_VERSION = {\"gitCommit\":1};"))
        .hasValueSatisfying(i -> assertThat(i.gitCommit()).isEmpty());
    assertThat(new JaegerBuildInfo(null, null, null).gitVersion()).isEmpty();
  }

  @Test
  void samplingStrategyAndRemoteSampler() {
    server.fallback(ex -> MockServer.send(ex, 200, Fixtures.body("sampling.per_operation")));
    JaegerClient c =
        JaegerClient.builder()
            .url(server.url())
            .samplingUrl(server.url() + "/sampling")
            .token("tok-123456")
            .build();
    {
      JaegerSamplingStrategy s = c.samplingStrategy("sdk-sampling-ops");
      assertThat(s.samplingRate()).hasValue(0.8);
      assertThat(server.requests().get(0).uri()).isEqualTo("/sampling?service=sdk-sampling-ops");
      assertThat(server.requests().get(0).header("Authorization")).isEqualTo("Bearer tok-123456");
      assertThatThrownBy(() -> c.samplingStrategy(" ")).isInstanceOf(ValidationException.class);
      JaegerRemoteSampler sampler = c.remoteSampler("sdk-sampling-ops", Duration.ofHours(1));
      assertThat(sampler.awaitFirstPoll(Duration.ofSeconds(10))).isTrue();
      assertThat(sampler.strategy()).isPresent();
      assertThat(sampler.samplingUrl()).isEqualTo(server.url() + "/sampling");
      JaegerTracingHandle tracing =
          c.initTracing(
              TracingConfig.builder("svc").setGlobal(false).build(),
              JaegerTracing.Options.builder().ignoreEnv());
      assertThat(tracing.isShutdown()).isFalse();
      JaegerTracingHandle tracing2 =
          c.initTracing(TracingConfig.builder("svc2").setGlobal(false).build());
      c.close();
      assertThat(sampler.isClosed()).isTrue();
      assertThat(tracing.isShutdown()).isTrue();
      assertThat(tracing2.isShutdown()).isTrue();
      assertThatThrownBy(() -> c.remoteSampler("x", null)).isInstanceOf(CancelledException.class);
      assertThatThrownBy(() -> c.initTracing(TracingConfig.builder("svc").build()))
          .isInstanceOf(CancelledException.class);
    }
  }

  @Test
  void queryIsShared() {
    server.enqueue(200, Fixtures.body("listServices.ok"));
    try (var c = client()) {
      assertThat(c.query().listServices()).contains("jaeger");
      c.query().close();
      assertThat(c.transport().isClosed()).isFalse();
      assertThat(c.query().toString()).contains("127.0.0.1");
    }
  }

  @Test
  void serviceConfig() {
    Map<String, String> env = new HashMap<>();
    env.put("JAEGER_QUERY_URL", "http://vendor:16686");
    env.put("JAEGER_SAMPLING_ENDPOINT", "http://vendor:5778/sampling");
    JaegerConfig vendor = JaegerConfig.fromEnv(env);
    assertThat(vendor.url()).isEqualTo("http://vendor:16686");
    assertThat(vendor.samplingUrl()).isEqualTo("http://vendor:5778/sampling");
    assertThat(vendor.healthUrl()).isEmpty();
    assertThat(vendor.metricsUrl()).isEmpty();
    env.put(P + "URL", "http://grow:16686");
    env.put(P + "SAMPLING_URL", "http://grow:5778/sampling");
    env.put(P + "HEALTH_URL", "http://grow:13133/status");
    env.put(P + "METRICS_URL", "http://grow:8888/metrics");
    JaegerConfig grow = JaegerConfig.fromEnv(env);
    assertThat(grow.url()).isEqualTo("http://grow:16686");
    assertThat(grow.samplingUrl()).isEqualTo("http://grow:5778/sampling");
    assertThat(grow.healthUrl()).contains("http://grow:13133/status");
    assertThat(grow.metricsUrl()).contains("http://grow:8888/metrics");
    JaegerConfig explicit =
        cfg()
            .samplingUrl("http://x:1/sampling")
            .healthUrl("http://x:2/status")
            .metricsUrl("http://x:3/metrics")
            .fromEnv(EnvReader.of(env))
            .build();
    assertThat(explicit.samplingUrl()).isEqualTo("http://x:1/sampling");
    assertThat(explicit.healthUrl()).contains("http://x:2/status");
    assertThat(explicit.metricsUrl()).contains("http://x:3/metrics");
    assertThat(explicit.toString()).contains("samplingUrl=http://x:1/sampling");
    JaegerConfig copy = explicit.toBuilder().token("t0k-copy").build();
    assertThat(copy.metricsUrl()).isEqualTo(explicit.metricsUrl());
    assertThat(copy.toString()).doesNotContain("t0k-copy");
    assertThat(JaegerConfig.fromEnv(Map.of()).samplingUrl())
        .isEqualTo(JaegerConfig.DEFAULT_SAMPLING_URL);
    assertThat(JaegerConfig.DEFAULT_URL).isEqualTo("http://127.0.0.1:16686");
    assertThatThrownBy(() -> cfg().samplingUrl("ftp://x").build())
        .isInstanceOf(ConfigException.class)
        .hasMessageContaining("samplingUrl");
    assertThatThrownBy(() -> cfg().healthUrl("http://[").build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().metricsUrl("relative").build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().token(" ").build()).isInstanceOf(ConfigException.class);
    Secret cleared = Secret.of("gone");
    cleared.clear();
    assertThat(cfg().token(cleared).build().token()).isPresent();
  }

  @Test
  void core_close_idempotent() throws Exception {
    AtomicInteger closed = new AtomicInteger();
    JaegerClient c = client();
    c.register(closed::incrementAndGet);
    c.register(
        () -> {
          throw new IllegalStateException("ignored");
        });
    c.close();
    c.close();
    assertThat(c.isClosed()).isTrue();
    assertThat(closed).hasValue(1);
    assertThat(c.transport().isClosed()).isTrue();
    assertThatThrownBy(c::ping).isInstanceOf(CancelledException.class);
  }

  @Test
  void builderSetsEveryField() {
    Hooks hooks = new Hooks() {};
    Logger logger = new Logger() {};
    try (var c =
        JaegerClient.builder()
            .url(server.url())
            .token(Secret.of("s3cr3t-value"))
            .connectTimeout(Duration.ofSeconds(1))
            .requestTimeout(Duration.ofSeconds(2))
            .totalTimeout(Duration.ofSeconds(3))
            .retry(RetryPolicy.none())
            .tls(TlsOptions.defaults())
            .proxy("http://proxy.invalid:3128")
            .header("X-A", "1")
            .userAgentSuffix("app/1")
            .hooks(hooks)
            .logger(logger)
            .build()) {
      JaegerConfig cfg = c.config();
      assertThat(cfg.url()).isEqualTo(server.url());
      assertThat(cfg.token()).isPresent();
      assertThat(cfg.connectTimeout()).isEqualTo(Duration.ofSeconds(1));
      assertThat(cfg.requestTimeout()).isEqualTo(Duration.ofSeconds(2));
      assertThat(cfg.totalTimeout()).isEqualTo(Duration.ofSeconds(3));
      assertThat(cfg.retry().maxRetries()).isZero();
      assertThat(cfg.tls().isCustom()).isFalse();
      assertThat(cfg.proxy()).contains("http://proxy.invalid:3128");
      assertThat(cfg.headers()).containsEntry("X-A", "1");
      assertThat(cfg.userAgentSuffix()).contains("app/1");
      assertThat(cfg.hooks()).isSameAs(hooks);
      assertThat(cfg.logger()).isSameAs(logger);
      assertThat(c.toString()).doesNotContain("s3cr3t-value");
      assertThat(c.transport().config().knownSecrets()).containsExactly("s3cr3t-value");
      cfg.token().get().clear();
      assertThat(c.transport().config().knownSecrets()).isEmpty();
    }
  }

  @Test
  void core_redaction_config() {
    JaegerConfig cfg =
        cfg()
            .url("http://user:pw@h:1")
            .token("tok-very-secret")
            .header("Authorization", "Bearer tok-very-secret")
            .proxy("http://u:proxypass@p:3128")
            .build();
    assertThat(cfg.toString())
        .doesNotContain("tok-very-secret")
        .doesNotContain("proxypass")
        .doesNotContain(":pw@")
        .contains("token=[REDACTED]");
    assertThat(cfg().build().toString()).contains("token=null").contains("proxy=null");
  }

  @Test
  void core_config_env_precedence() {
    Map<String, String> env = new HashMap<>();
    env.put(P + "URL", "http://from-env:1");
    env.put(P + "TOKEN", "env-token");
    env.put(P + "CONNECT_TIMEOUT", "1500");
    env.put(P + "REQUEST_TIMEOUT", "2s");
    env.put(P + "TOTAL_TIMEOUT", "1m");
    env.put(P + "MAX_RETRIES", "5");
    env.put(P + "CA_CERT", "/tmp/ca.pem");
    env.put(P + "CLIENT_CERT", "/tmp/c.pem");
    env.put(P + "CLIENT_KEY", "/tmp/k.pem");
    env.put(P + "INSECURE_SKIP_VERIFY", "on");
    env.put(P + "PROXY", "http://proxy:1");
    JaegerConfig fromEnv = JaegerConfig.fromEnv(env);
    assertThat(fromEnv.url()).isEqualTo("http://from-env:1");
    assertThat(fromEnv.token().map(Secret::reveal)).contains("env-token");
    assertThat(fromEnv.connectTimeout()).isEqualTo(Duration.ofMillis(1500));
    assertThat(fromEnv.requestTimeout()).isEqualTo(Duration.ofSeconds(2));
    assertThat(fromEnv.totalTimeout()).isEqualTo(Duration.ofMinutes(1));
    assertThat(fromEnv.retry().maxRetries()).isEqualTo(5);
    assertThat(fromEnv.tls().insecureSkipVerify()).isTrue();
    assertThat(fromEnv.tls().caCertPath()).isPresent();
    assertThat(fromEnv.proxy()).contains("http://proxy:1");
    // explicit values win over the environment
    JaegerConfig explicit =
        cfg()
            .url("http://explicit:2")
            .token("explicit-token")
            .connectTimeout(Duration.ofSeconds(9))
            .requestTimeout(Duration.ofSeconds(9))
            .totalTimeout(Duration.ofSeconds(9))
            .retry(RetryPolicy.none())
            .tls(TlsOptions.defaults())
            .proxy("http://p2:1")
            .fromEnv(EnvReader.of(env))
            .build();
    assertThat(explicit.url()).isEqualTo("http://explicit:2");
    assertThat(explicit.token().map(Secret::reveal)).contains("explicit-token");
    assertThat(explicit.retry().maxRetries()).isZero();
    assertThat(explicit.tls().insecureSkipVerify()).isFalse();
    assertThat(explicit.proxy()).contains("http://p2:1");
    // defaults when nothing is set
    JaegerConfig defaults = JaegerConfig.fromEnv(Map.of());
    assertThat(defaults.url()).isEqualTo(JaegerConfig.DEFAULT_URL);
    assertThat(defaults.token()).isEmpty();
    assertThat(defaults.connectTimeout()).isEqualTo(Duration.ofSeconds(10));
    assertThat(defaults.requestTimeout()).isEqualTo(Duration.ofSeconds(30));
    assertThat(defaults.totalTimeout()).isEqualTo(Duration.ofSeconds(120));
    assertThat(defaults.retry().maxRetries()).isEqualTo(3);
    // invalid values name the variable, never the value
    Map<String, String> badTimeout = Map.of(P + "TOTAL_TIMEOUT", "forever");
    assertThatThrownBy(() -> JaegerConfig.fromEnv(badTimeout))
        .isInstanceOf(ConfigException.class)
        .hasMessageContaining(P + "TOTAL_TIMEOUT")
        .message()
        .doesNotContain("forever");
    Map<String, String> badRetries = Map.of(P + "MAX_RETRIES", "-1");
    assertThatThrownBy(() -> JaegerConfig.fromEnv(badRetries))
        .isInstanceOf(ConfigException.class)
        .hasMessageContaining(P + "MAX_RETRIES");
    assertThat(JaegerConfig.fromEnv()).isNotNull();
  }

  @Test
  void validation() {
    assertThatThrownBy(() -> cfg().url("ftp://x").build()).isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().url("http://[").build()).isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().url("mailto:x").build()).isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().connectTimeout(Duration.ZERO).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().requestTimeout(Duration.ofSeconds(-1)).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().totalTimeout(Duration.ZERO).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().tls(TlsOptions.builder().clientCertPem("x").build()).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().proxy("relative").build()).isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> cfg().proxy("http://[").build()).isInstanceOf(ConfigException.class);
    assertThat(cfg().token((String) null).build().token()).isEmpty();
  }

  @Test
  void createAndFromEnv() {
    try (var c = JaegerClient.create(cfg().url(server.url()).build())) {
      assertThat(c.isClosed()).isFalse();
    }
    try (var c = JaegerClient.fromEnv()) {
      assertThat(c.config()).isNotNull();
    }
    EnvReader env = EnvReader.of(Map.of(P + "URL", server.url()));
    try (var c = JaegerClient.builder().fromEnv(env).build()) {
      assertThat(c.config().url()).isEqualTo(server.url());
    }
  }
}
