package codes.grow.packages.jaeger;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import codes.grow.packages.jaeger.core.ConfigException;
import codes.grow.packages.jaeger.core.JaegerException;
import codes.grow.packages.jaeger.core.MockServer;
import codes.grow.packages.jaeger.core.RetryPolicy;
import codes.grow.packages.jaeger.core.ValidationException;
import io.opentelemetry.api.common.Attributes;
import io.opentelemetry.api.trace.SpanKind;
import io.opentelemetry.context.Context;
import io.opentelemetry.sdk.trace.samplers.SamplingDecision;
import java.time.Duration;
import java.util.List;
import java.util.OptionalDouble;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class JaegerSamplerTest {
  /** Trace id whose low 8 bytes are small: sampled by any ratio above ~0. */
  private static final String LOW = "00000000000000000000000000000001";

  /** Trace id whose low 8 bytes are maximal: dropped by any ratio below 1. */
  private static final String HIGH = "ffffffffffffffff7ffffffffffffffe";

  private MockServer server;

  @BeforeEach
  void start() throws Exception {
    server = MockServer.start();
  }

  @AfterEach
  void stop() {
    server.close();
  }

  private static SamplingDecision decide(JaegerRemoteSampler s, String traceId, String name) {
    return s.shouldSample(
            Context.root(), traceId, name, SpanKind.INTERNAL, Attributes.empty(), List.of())
        .getDecision();
  }

  private JaegerRemoteSampler.Builder builder(String service) {
    return JaegerRemoteSampler.builder(service)
        .samplingUrl(server.url() + "/sampling")
        .config(
            JaegerConfig.builder()
                .url(server.url())
                .retry(RetryPolicy.none())
                .token("stok-1234567")
                .build())
        .autoStart(false);
  }

  @Test
  void parseFixtures() {
    JaegerSamplingStrategy stock =
        JaegerSamplingStrategy.parse(Fixtures.body("sampling.default_stock"));
    assertThat(stock.strategyType()).isEqualTo(JaegerSamplingStrategy.PROBABILISTIC);
    assertThat(stock.samplingRate()).hasValue(1);
    assertThat(stock.operations()).isEmpty();
    assertThat(stock.maxTracesPerSecond()).isEmpty();

    JaegerSamplingStrategy ops =
        JaegerSamplingStrategy.parse(Fixtures.body("sampling.per_operation"));
    assertThat(ops.samplingRate()).hasValue(0.8);
    assertThat(ops.defaultSamplingProbability()).hasValue(0.8);
    assertThat(ops.operationRate("GET /fast")).contains(0.2);
    assertThat(ops.operationRate("/health")).contains(0.0);
    assertThat(ops.operationRate("nope")).isEmpty();

    JaegerSamplingStrategy rate =
        JaegerSamplingStrategy.parse(Fixtures.body("sampling.rate_limiting"));
    assertThat(rate.strategyType()).isEqualTo(JaegerSamplingStrategy.RATE_LIMITING);
    assertThat(rate.maxTracesPerSecond()).hasValue(5);
    assertThat(rate.samplingRate()).isEmpty();
    assertThat(rate.serviceRate()).hasValue(0.5);

    JaegerSamplingStrategy prob =
        JaegerSamplingStrategy.parse(Fixtures.body("sampling.probabilistic"));
    assertThat(prob.samplingRate()).hasValue(0.25);
    assertThat(
            JaegerSamplingStrategy.parse(Fixtures.body("sampling.default_strategy")).samplingRate())
        .hasValue(0.5);
  }

  @Test
  void parseEdgeCases() {
    JaegerSamplingStrategy empty = JaegerSamplingStrategy.parse("{}");
    assertThat(empty.strategyType()).isEqualTo(JaegerSamplingStrategy.PROBABILISTIC);
    assertThat(empty.serviceRate()).isEmpty();
    assertThat(JaegerSamplingStrategy.parse("{\"strategyType\":1}").strategyType())
        .isEqualTo(JaegerSamplingStrategy.RATE_LIMITING);
    assertThat(JaegerSamplingStrategy.parse("{\"strategyType\":0}").strategyType())
        .isEqualTo(JaegerSamplingStrategy.PROBABILISTIC);
    assertThat(JaegerSamplingStrategy.parse("{\"strategyType\":null}").strategyType())
        .isEqualTo(JaegerSamplingStrategy.PROBABILISTIC);
    JaegerSamplingStrategy zero =
        JaegerSamplingStrategy.parse(
            "{\"probabilisticSampling\":{},\"rateLimitingSampling\":{},"
                + "\"operationSampling\":{\"perOperationStrategies\":["
                + "{\"operation\":\"/h\",\"probabilisticSampling\":{}},{\"operation\":3},{}]}}");
    assertThat(zero.samplingRate()).hasValue(0);
    assertThat(zero.maxTracesPerSecond()).hasValue(0);
    assertThat(zero.operations())
        .containsExactly(new JaegerSamplingStrategy.OperationStrategy("/h", 0));
    for (String bad :
        List.of(
            "[1]",
            "{\"strategyType\":\"WEIRD\"}",
            "{\"probabilisticSampling\":{\"samplingRate\":1.5}}",
            "{\"probabilisticSampling\":{\"samplingRate\":\"x\"}}",
            "{\"rateLimitingSampling\":{\"maxTracesPerSecond\":-1}}",
            "{\"operationSampling\":{\"defaultSamplingProbability\":2}}",
            "{\"operationSampling\":{\"defaultLowerBoundTracesPerSecond\":-1}}",
            "not json")) {
      assertThatThrownBy(() -> JaegerSamplingStrategy.parse(bad))
          .as(bad)
          .isInstanceOf(ValidationException.class);
    }
    assertThatThrownBy(
            () -> JaegerSamplingStrategy.parse((com.fasterxml.jackson.databind.JsonNode) null))
        .isInstanceOf(ValidationException.class);
    JaegerSamplingStrategy nulls =
        new JaegerSamplingStrategy(JaegerSamplingStrategy.PROBABILISTIC, null, null, null, 0, null);
    assertThat(nulls.operations()).isEmpty();
    assertThat(nulls.samplingRate()).isEmpty();
  }

  @Test
  void initialRateUntilFirstPoll() {
    try (JaegerRemoteSampler s = builder("svc").initialSamplingRate(0).build()) {
      assertThat(s.strategy()).isEmpty();
      assertThat(decide(s, LOW, "op")).isEqualTo(SamplingDecision.DROP);
      assertThat(s.initialSamplingRate()).isZero();
      assertThat(s.serviceName()).isEqualTo("svc");
      assertThat(s.pollInterval()).isEqualTo(JaegerRemoteSampler.DEFAULT_POLL_INTERVAL);
      assertThat(s.getDescription()).contains("svc").contains("/sampling");
      assertThat(s.toString()).isEqualTo(s.getDescription());
    }
  }

  @Test
  void perOperationWinsThenServiceRate() {
    server.enqueue(200, Fixtures.body("sampling.per_operation"));
    try (JaegerRemoteSampler s = builder("sdk-sampling-ops").build()) {
      JaegerSamplingStrategy st = s.refresh();
      assertThat(s.strategy()).contains(st);
      assertThat(server.requests().get(0).uri()).isEqualTo("/sampling?service=sdk-sampling-ops");
      assertThat(server.requests().get(0).header("Authorization")).isEqualTo("Bearer stok-1234567");
      assertThat(decide(s, LOW, "/health")).isEqualTo(SamplingDecision.DROP);
      assertThat(decide(s, LOW, "GET /fast")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
      assertThat(decide(s, HIGH, "GET /fast")).isEqualTo(SamplingDecision.DROP);
      assertThat(decide(s, LOW, "other")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
      assertThat(decide(s, HIGH, "other")).isEqualTo(SamplingDecision.DROP);
    }
  }

  @Test
  void rateLimiterAppliesWhenPresent() {
    AtomicLong now = new AtomicLong();
    server.enqueue(200, Fixtures.body("sampling.rate_limiting"));
    server.enqueue(
        200,
        "{\"strategyType\":\"RATE_LIMITING\",\"rateLimitingSampling\":{\"maxTracesPerSecond\":5}}");
    server.enqueue(200, "{\"rateLimitingSampling\":{\"maxTracesPerSecond\":0}}");
    server.enqueue(200, "{\"probabilisticSampling\":{\"samplingRate\":1}}");
    try (JaegerRemoteSampler s = builder("sdk-sampling-rate").nanoClock(now::get).build()) {
      s.refresh();
      // listed operation keeps its own probability
      assertThat(decide(s, LOW, "/health")).isEqualTo(SamplingDecision.DROP);
      int sampled = 0;
      for (int i = 0; i < 10; i++) {
        if (decide(s, HIGH, "op") == SamplingDecision.RECORD_AND_SAMPLE) {
          sampled++;
        }
      }
      assertThat(sampled).isEqualTo(5);
      now.addAndGet(1_000_000_000L);
      assertThat(decide(s, HIGH, "op")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
      // same rate keeps the bucket (still has 4 credits)
      s.refresh();
      assertThat(decide(s, HIGH, "op")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
      // rate 0: the first trace passes, then none
      s.refresh();
      assertThat(decide(s, HIGH, "op")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
      assertThat(decide(s, LOW, "op")).isEqualTo(SamplingDecision.DROP);
      now.addAndGet(10_000_000_000L);
      assertThat(decide(s, LOW, "op")).isEqualTo(SamplingDecision.DROP);
      // back to probabilistic
      s.refresh();
      assertThat(decide(s, HIGH, "op")).isEqualTo(SamplingDecision.RECORD_AND_SAMPLE);
    }
  }

  @Test
  void failedPollKeepsLastGoodStrategy() throws Exception {
    List<JaegerException> errors = new CopyOnWriteArrayList<>();
    List<JaegerSamplingStrategy> updates = new CopyOnWriteArrayList<>();
    server.enqueue(200, Fixtures.body("sampling.probabilistic"));
    server.enqueue(200, "garbage", "Content-Type", "text/plain");
    server.fallback(ex -> MockServer.send(ex, 400, "'service' parameter must be provided once\n"));
    try (JaegerRemoteSampler s =
        builder("sdk-sampling-prob")
            .pollInterval(Duration.ofMillis(20))
            .onError(
                e -> {
                  errors.add(e);
                  throw new IllegalStateException("callback errors are ignored");
                })
            .onUpdate(updates::add)
            .autoStart(true)
            .build()) {
      assertThat(s.awaitFirstPoll(Duration.ofSeconds(10))).isTrue();
      long deadline = System.nanoTime() + 10_000_000_000L;
      while (errors.size() < 2 && System.nanoTime() < deadline) {
        Thread.sleep(10);
      }
      assertThat(updates).hasSize(1);
      assertThat(errors.size()).isGreaterThanOrEqualTo(2);
      assertThat(errors.get(0)).isInstanceOf(ValidationException.class);
      assertThat(s.strategy().orElseThrow().samplingRate()).hasValue(0.25);
      s.start();
    }
  }

  @Test
  void builderValidationAndDefaults() {
    assertThatThrownBy(() -> JaegerRemoteSampler.builder(" ").build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> JaegerRemoteSampler.builder(null).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> builder("s").pollInterval(Duration.ZERO).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> builder("s").initialSamplingRate(1.5).build())
        .isInstanceOf(ConfigException.class);
    assertThatThrownBy(() -> builder("s").samplingUrl("ftp://x").build())
        .isInstanceOf(ConfigException.class);
    try (JaegerRemoteSampler s =
        JaegerRemoteSampler.builder("s").autoStart(false).onError(null).onUpdate(null).build()) {
      assertThat(s.samplingUrl()).isEqualTo(JaegerConfig.DEFAULT_SAMPLING_URL);
      assertThat(s.awaitFirstPoll(Duration.ofMillis(1))).isFalse();
    }
    JaegerRemoteSampler created =
        JaegerRemoteSampler.create(server.url() + "/sampling", "svc", null);
    assertThat(created.awaitFirstPoll(Duration.ofSeconds(10))).isTrue();
    created.close();
    created.close();
    created.start();
    assertThat(created.isClosed()).isTrue();
    assertThat(created.awaitFirstPoll(Duration.ofMillis(1))).isTrue();
    Thread.currentThread().interrupt();
    try (JaegerRemoteSampler s = builder("s").build()) {
      assertThat(s.awaitFirstPoll(Duration.ofSeconds(1))).isFalse();
    }
    assertThat(Thread.interrupted()).isTrue();
    assertThat(JaegerSamplingStrategy.probabilistic(0.3).samplingRate())
        .isEqualTo(OptionalDouble.of(0.3));
  }
}
