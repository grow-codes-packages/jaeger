package codes.grow.packages.jaeger.integration;

import codes.grow.packages.jaeger.JaegerClient;
import codes.grow.packages.jaeger.JaegerTracing;
import codes.grow.packages.jaeger.JaegerTracingHandle;
import codes.grow.packages.jaeger.core.EnvReader;
import codes.grow.packages.jaeger.testing.ConformanceRunner;
import codes.grow.packages.jaeger.testing.tracing.TracingTarget;
import codes.grow.packages.jaeger.tracing.TracingConfig;
import java.io.IOException;
import java.io.UncheckedIOException;
import java.time.Duration;
import java.util.Map;

/**
 * Conformance targets of this service: {@code core} is the client, {@code tracing} exports OTLP to
 * the real Jaeger (the Jaeger preset from the environment) and reads back through the query
 * client. Jaeger serves every tracing capability: the v3 query API, services and dependencies.
 */
final class Factories {
  /** Capabilities the deployment under test offers (see the scenario {@code requires}). */
  static final String[] CAPABILITIES = {"version", "query", "services", "dependencies"};

  /** Service name of the tracing scenarios ({@code ${serviceName}}). */
  static final String SERVICE_NAME = "grow-conformance-java";

  private Factories() {}

  static void register(ConformanceRunner.Builder runner, EnvReader env) {
    Map<String, String> vars;
    try {
      vars = ConformanceTest.environment();
    } catch (IOException e) {
      throw new UncheckedIOException(e);
    }
    runner.serviceName(SERVICE_NAME);
    runner.target("core", scenario -> JaegerClient.builder().fromEnv(env).build());
    runner.target(
        "tracing",
        scenario -> {
          JaegerClient client = JaegerClient.builder().fromEnv(env).build();
          TracingConfig config =
              TracingConfig.builder(SERVICE_NAME)
                  .setGlobal(false)
                  .batch(
                      new TracingConfig.BatchSettings(
                          2048, 512, Duration.ofMillis(100), Duration.ofSeconds(30)))
                  .build();
          JaegerTracingHandle handle =
              client.initTracing(config, JaegerTracing.Options.builder().env(vars));
          return TracingTarget.of(handle.handle(), client.query());
        });
  }
}
