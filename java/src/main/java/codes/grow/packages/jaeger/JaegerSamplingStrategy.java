package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.Json;
import codes.grow.packages.jaeger.core.ValidationException;
import com.fasterxml.jackson.databind.JsonNode;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalDouble;

/**
 * A Jaeger remote sampling strategy (the {@code GET /sampling?service=} response), normalized.
 * Missing numbers count as 0 (proto3 JSON omits zero values); a missing {@code strategyType} means
 * {@code PROBABILISTIC}.
 *
 * @param strategyType {@link #PROBABILISTIC} or {@link #RATE_LIMITING}
 * @param samplingRate {@code probabilisticSampling.samplingRate} (0..1), empty when absent
 * @param maxTracesPerSecond {@code rateLimitingSampling.maxTracesPerSecond} (&gt;= 0), empty when
 *     absent
 * @param defaultSamplingProbability {@code operationSampling.defaultSamplingProbability}, empty
 *     when there is no {@code operationSampling}
 * @param defaultLowerBoundTracesPerSecond {@code
 *     operationSampling.defaultLowerBoundTracesPerSecond} (reported only; the SDK does not enforce
 *     lower bounds)
 * @param operations per-operation probabilities ({@code operationSampling.perOperationStrategies})
 */
public record JaegerSamplingStrategy(
    String strategyType,
    OptionalDouble samplingRate,
    OptionalDouble maxTracesPerSecond,
    OptionalDouble defaultSamplingProbability,
    double defaultLowerBoundTracesPerSecond,
    List<OperationStrategy> operations) {
  /** Strategy type of a probabilistic strategy. */
  public static final String PROBABILISTIC = "PROBABILISTIC";

  /** Strategy type of a rate-limiting strategy. */
  public static final String RATE_LIMITING = "RATE_LIMITING";

  /**
   * The sampling probability of one operation (span name).
   *
   * @param operation operation name
   * @param samplingRate probability 0..1
   */
  public record OperationStrategy(String operation, double samplingRate) {
    /**
     * Validates the fields.
     *
     * @param operation operation name
     * @param samplingRate probability
     */
    public OperationStrategy {
      Objects.requireNonNull(operation, "operation");
      checkRate(samplingRate, "samplingRate");
    }
  }

  /**
   * Validates the strategy.
   *
   * @param strategyType type
   * @param samplingRate service probability
   * @param maxTracesPerSecond service rate limit
   * @param defaultSamplingProbability default operation probability
   * @param defaultLowerBoundTracesPerSecond lower bound
   * @param operations per-operation strategies
   * @throws ValidationException for an unknown type, a rate outside 0..1 or a negative limit
   */
  public JaegerSamplingStrategy {
    if (!PROBABILISTIC.equals(strategyType) && !RATE_LIMITING.equals(strategyType)) {
      throw new ValidationException("jaeger.sampling: unknown strategyType");
    }
    samplingRate = samplingRate == null ? OptionalDouble.empty() : samplingRate;
    maxTracesPerSecond = maxTracesPerSecond == null ? OptionalDouble.empty() : maxTracesPerSecond;
    defaultSamplingProbability =
        defaultSamplingProbability == null ? OptionalDouble.empty() : defaultSamplingProbability;
    samplingRate.ifPresent(r -> checkRate(r, "samplingRate"));
    defaultSamplingProbability.ifPresent(r -> checkRate(r, "defaultSamplingProbability"));
    if (maxTracesPerSecond.isPresent() && !(maxTracesPerSecond.getAsDouble() >= 0)) {
      throw new ValidationException("jaeger.sampling: maxTracesPerSecond must be >= 0");
    }
    if (!(defaultLowerBoundTracesPerSecond >= 0)) {
      throw new ValidationException(
          "jaeger.sampling: defaultLowerBoundTracesPerSecond must be >= 0");
    }
    operations = operations == null ? List.of() : List.copyOf(operations);
  }

  private static void checkRate(double rate, String what) {
    if (!(rate >= 0 && rate <= 1)) {
      throw new ValidationException("jaeger.sampling: " + what + " must be between 0 and 1");
    }
  }

  /**
   * A plain probabilistic strategy (what a sampler uses before the first poll).
   *
   * @param samplingRate probability 0..1
   * @return strategy
   */
  public static JaegerSamplingStrategy probabilistic(double samplingRate) {
    return new JaegerSamplingStrategy(
        PROBABILISTIC,
        OptionalDouble.of(samplingRate),
        OptionalDouble.empty(),
        OptionalDouble.empty(),
        0,
        List.of());
  }

  /**
   * Parses a sampling response body.
   *
   * @param body UTF-8 JSON
   * @return the strategy
   * @throws ValidationException for malformed JSON or invalid values
   */
  public static JaegerSamplingStrategy parse(String body) {
    return parse(Json.tree(body.getBytes(StandardCharsets.UTF_8)));
  }

  /**
   * Parses a sampling response.
   *
   * @param root JSON object
   * @return the strategy
   * @throws ValidationException for invalid values
   */
  public static JaegerSamplingStrategy parse(JsonNode root) {
    if (root == null || !root.isObject()) {
      throw new ValidationException("jaeger.sampling: response is not an object");
    }
    JsonNode type = root.get("strategyType");
    String typeText =
        type == null || type.isNull() ? PROBABILISTIC : type.asText().toUpperCase(Locale.ROOT);
    String strategyType;
    if (typeText.equals(PROBABILISTIC) || typeText.equals("0")) {
      strategyType = PROBABILISTIC;
    } else if (typeText.equals(RATE_LIMITING) || typeText.equals("1")) {
      strategyType = RATE_LIMITING;
    } else {
      throw new ValidationException("jaeger.sampling: unknown strategyType");
    }
    OptionalDouble rate = OptionalDouble.empty();
    JsonNode prob = root.get("probabilisticSampling");
    if (prob != null && prob.isObject()) {
      rate = OptionalDouble.of(number(prob.get("samplingRate"), "samplingRate"));
    }
    OptionalDouble max = OptionalDouble.empty();
    JsonNode limit = root.get("rateLimitingSampling");
    if (limit != null && limit.isObject()) {
      max = OptionalDouble.of(number(limit.get("maxTracesPerSecond"), "maxTracesPerSecond"));
    }
    OptionalDouble defaultProbability = OptionalDouble.empty();
    double lowerBound = 0;
    List<OperationStrategy> ops = new ArrayList<>();
    JsonNode opSampling = root.get("operationSampling");
    if (opSampling != null && opSampling.isObject()) {
      defaultProbability =
          OptionalDouble.of(
              number(opSampling.get("defaultSamplingProbability"), "defaultSamplingProbability"));
      lowerBound =
          number(
              opSampling.get("defaultLowerBoundTracesPerSecond"),
              "defaultLowerBoundTracesPerSecond");
      for (JsonNode e : opSampling.path("perOperationStrategies")) {
        JsonNode op = e.get("operation");
        if (op == null || !op.isTextual()) {
          continue;
        }
        ops.add(
            new OperationStrategy(
                op.asText(), number(e.path("probabilisticSampling").get("samplingRate"), "rate")));
      }
    }
    return new JaegerSamplingStrategy(strategyType, rate, max, defaultProbability, lowerBound, ops);
  }

  private static double number(JsonNode v, String what) {
    if (v == null || v.isNull()) {
      return 0;
    }
    if (!v.isNumber()) {
      throw new ValidationException("jaeger.sampling: " + what + " must be a number");
    }
    return v.asDouble();
  }

  /**
   * The probability of a listed operation.
   *
   * @param operation operation (span) name
   * @return its per-operation probability, or empty when the operation is not listed
   */
  public Optional<Double> operationRate(String operation) {
    for (OperationStrategy o : operations) {
      if (o.operation().equals(operation)) {
        return Optional.of(o.samplingRate());
      }
    }
    return Optional.empty();
  }

  /**
   * The service-level probability: {@code samplingRate}, else {@code defaultSamplingProbability}.
   *
   * @return the probability, or empty when the strategy has neither
   */
  public OptionalDouble serviceRate() {
    return samplingRate.isPresent() ? samplingRate : defaultSamplingProbability;
  }
}
