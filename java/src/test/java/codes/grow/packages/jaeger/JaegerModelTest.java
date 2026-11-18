package codes.grow.packages.jaeger;

import static org.assertj.core.api.Assertions.assertThat;

import codes.grow.packages.jaeger.core.Json;
import codes.grow.packages.jaeger.tracing.query.Span;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import com.fasterxml.jackson.databind.JsonNode;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;

class JaegerModelTest {
  private static final String T = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";

  private static JsonNode json(String s) {
    return Json.tree(s.getBytes(StandardCharsets.UTF_8));
  }

  @Test
  void nanos() {
    assertThat(JaegerModel.nanosToMicros(null)).isZero();
    assertThat(JaegerModel.nanosToMicros(json("1500"))).isEqualTo(1);
    assertThat(JaegerModel.nanosToMicros(json("2500.0"))).isEqualTo(2);
    assertThat(JaegerModel.nanosToMicros(json("\"1795034918142512222\"")))
        .isEqualTo(1795034918142512L);
    assertThat(JaegerModel.nanosToMicros(json("\"-5\""))).isZero();
    assertThat(JaegerModel.nanosToMicros(json("true"))).isZero();
  }

  @Test
  void anyValues() {
    assertThat(JaegerModel.anyValue(null)).isNull();
    assertThat(JaegerModel.anyValue(json("1"))).isNull();
    assertThat(JaegerModel.anyValue(json("{\"intValue\":7}"))).isEqualTo(7L);
    assertThat(JaegerModel.anyValue(json("{\"intValue\":\"99999999999999999999\"}")))
        .isEqualTo("99999999999999999999");
    assertThat(JaegerModel.anyValue(json("{\"doubleValue\":\"2.5\"}"))).isEqualTo(2.5);
    assertThat(JaegerModel.anyValue(json("{\"doubleValue\":\"NaNx\"}"))).isEqualTo("NaNx");
    assertThat(JaegerModel.anyValue(json("{\"bytesValue\":\"AQI=\"}"))).isEqualTo("AQI=");
    assertThat(
            JaegerModel.anyValue(
                json(
                    "{\"kvlistValue\":{\"values\":[{\"key\":\"k\",\"value\":{\"boolValue\":false}},"
                        + "{\"key\":1,\"value\":{}},{\"value\":{\"stringValue\":\"x\"}}]}}")))
        .isEqualTo(Map.of("k", false));
    assertThat(
            JaegerModel.anyValue(json("{\"arrayValue\":{\"values\":[{},{\"intValue\":\"1\"}]}}")))
        .isEqualTo(List.of(1L));
    assertThat(JaegerModel.anyValue(json("{\"stringValue\":1,\"boolValue\":\"x\"}"))).isNull();
    assertThat(JaegerModel.anyValue(json("{\"bytesValue\":1,\"arrayValue\":1,\"kvlistValue\":1}")))
        .isNull();
    assertThat(JaegerModel.keyValues(null)).isEmpty();
  }

  @Test
  void kindsAndStatus() {
    assertThat(JaegerModel.otlpKind(null)).isEqualTo(Span.Kind.UNSPECIFIED);
    assertThat(JaegerModel.otlpKind(json("4"))).isEqualTo(Span.Kind.PRODUCER);
    assertThat(JaegerModel.otlpKind(json("5"))).isEqualTo(Span.Kind.CONSUMER);
    assertThat(JaegerModel.otlpKind(json("0"))).isEqualTo(Span.Kind.UNSPECIFIED);
    assertThat(JaegerModel.otlpKind(json("\"SPAN_KIND_SERVER\""))).isEqualTo(Span.Kind.SERVER);
    assertThat(JaegerModel.otlpKind(json("{}"))).isEqualTo(Span.Kind.UNSPECIFIED);
    assertThat(JaegerModel.otlpStatus(json("\"STATUS_CODE_ERROR\"")))
        .isEqualTo(Span.StatusCode.ERROR);
    assertThat(JaegerModel.otlpStatus(json("null"))).isEqualTo(Span.StatusCode.UNSET);
  }

  @Test
  void otlpEdgeCases() {
    assertThat(JaegerModel.otlpTraces(null)).isEmpty();
    List<Trace> traces =
        JaegerModel.otlpTraces(
            json(
                "{\"resourceSpans\":[{\"scopeSpans\":[{\"spans\":[{\"traceId\":\""
                    + T
                    + "\",\"spanId\":\"0000000000000001\",\"parentSpanId\":\"0000000000000000\","
                    + "\"status\":{\"code\":2,\"message\":\"\"},"
                    + "\"events\":[{}],\"links\":[{\"traceId\":\""
                    + T
                    + "\",\"spanId\":\"0000000000000002\"},{\"traceId\":\"zz\",\"spanId\":\"01\"},"
                    + "{\"traceId\":\""
                    + T
                    + "\"},{\"traceId\":\""
                    + T
                    + "\",\"spanId\":\"xyz\"}]}]}]},"
                    + "{\"resource\":{\"attributes\":[{\"key\":\"service.name\",\"value\":"
                    + "{\"stringValue\":\"\"}}]},\"scopeSpans\":[{\"spans\":[{\"traceId\":\""
                    + T
                    + "\",\"spanId\":\"0000000000000003\",\"name\":\"n\"}]}]}]}"));
    assertThat(traces).hasSize(1);
    Span s = traces.get(0).spans().get(0);
    assertThat(s.serviceName()).isEqualTo(JaegerModel.UNKNOWN_SERVICE);
    assertThat(s.name()).isEmpty();
    assertThat(s.parentSpanId()).isEmpty();
    assertThat(s.status().code()).isEqualTo(Span.StatusCode.ERROR);
    assertThat(s.status().message()).isEmpty();
    assertThat(s.events().get(0).name()).isEmpty();
    assertThat(s.links()).hasSize(1);
    assertThat(traces.get(0).spans().get(1).serviceName()).isEqualTo(JaegerModel.UNKNOWN_SERVICE);
  }

  @Test
  void legacyEdgeCases() {
    List<Trace> traces =
        JaegerModel.legacyTraces(
            json(
                "{\"data\":[{\"traceID\":\""
                    + T
                    + "\",\"spans\":["
                    + "{\"spanID\":\"0000000000000001\",\"tags\":["
                    + "{\"key\":\"span.kind\",\"value\":\"SERVER\"},{\"key\":\"error\",\"value\":true},"
                    + "{\"key\":\"n\",\"value\":3},{\"key\":\"d\",\"value\":1.5},{\"key\":\"o\",\"value\":{}},"
                    + "{\"key\":\"nv\"},{\"value\":\"x\"}],"
                    + "\"logs\":[{\"fields\":[{\"key\":\"msg\",\"value\":\"hi\"}]}]},"
                    + "{\"spanID\":\"0000000000000002\",\"operationName\":\"child\",\"processID\":\"p9\","
                    + "\"references\":[{\"refType\":\"FOLLOWS_FROM\",\"traceID\":\""
                    + T
                    + "\",\"spanID\":\"0000000000000001\"},{\"refType\":\"CHILD_OF\",\"traceID\":\"zz\"},"
                    + "{\"refType\":\"CHILD_OF\"}],"
                    + "\"tags\":[{\"key\":\"span.kind\",\"value\":7}]}]}]}"));
    Trace t = traces.get(0);
    Span first = t.spansNamed("").get(0);
    assertThat(first.kind()).isEqualTo(Span.Kind.UNSPECIFIED);
    assertThat(first.status().code()).isEqualTo(Span.StatusCode.ERROR);
    assertThat(first.status().message()).isEmpty();
    assertThat(first.serviceName()).isEqualTo(JaegerModel.UNKNOWN_SERVICE);
    assertThat(first.attributes()).containsEntry("n", 3L).containsEntry("d", 1.5).hasSize(2);
    assertThat(first.events().get(0).name()).isEqualTo("log");
    assertThat(first.events().get(0).timeMicros()).isZero();
    Span child = t.spansNamed("child").get(0);
    assertThat(child.parentSpanId()).contains("0000000000000001");
    assertThat(child.links()).isEmpty();
    assertThat(child.kind()).isEqualTo(Span.Kind.UNSPECIFIED);
    assertThat(JaegerModel.legacyTraces(json("{}"))).isEmpty();
  }

  @Test
  void summariesDependenciesOperations() {
    List<TraceSummary> sums =
        JaegerModel.summaries(
            json("{\"summaries\":[{\"traceId\":\"" + T + "\",\"rootServiceName\":\"\"}]}"));
    assertThat(sums.get(0).rootServiceName()).isEmpty();
    assertThat(sums.get(0).rootName()).isEmpty();
    assertThat(sums.get(0).spanCount()).isZero();
    assertThat(JaegerModel.dependencies(json("{\"data\":[{\"child\":\"c\"}]}"))).isEmpty();
    assertThat(JaegerModel.operations(json("{\"operations\":[{\"name\":\"a\",\"spanKind\":5}]}")))
        .containsExactly(new JaegerOperation("a", ""));
  }
}
