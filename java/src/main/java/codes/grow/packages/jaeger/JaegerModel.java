package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.ValidationException;
import codes.grow.packages.jaeger.tracing.query.DependencyLink;
import codes.grow.packages.jaeger.tracing.query.Span;
import codes.grow.packages.jaeger.tracing.query.Trace;
import codes.grow.packages.jaeger.tracing.query.TraceIds;
import codes.grow.packages.jaeger.tracing.query.TraceSummary;
import com.fasterxml.jackson.databind.JsonNode;
import java.math.BigInteger;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * Converts Jaeger query responses (OTLP/JSON from {@code /api/v3}, the legacy UI model from {@code
 * /api/traces/{id}}, trace summaries and dependency links) into the normalized trace model.
 */
final class JaegerModel {
  /** Service name used when a resource has no {@code service.name}. */
  static final String UNKNOWN_SERVICE = "unknown_service";

  private static final Pattern DIGITS = Pattern.compile("\\d+");
  private static final Pattern ZERO = Pattern.compile("0+");
  private static final BigInteger THOUSAND = BigInteger.valueOf(1000);

  /** Tags Jaeger synthesizes from OTLP fields; mapped back and dropped from the attributes. */
  private static final Set<String> SYNTHETIC_TAGS =
      Set.of(
          "span.kind",
          "otel.status_code",
          "otel.status_description",
          "error",
          "otel.scope.name",
          "otel.scope.version");

  private JaegerModel() {}

  /**
   * Converts an OTLP nanosecond timestamp (decimal string or number) to epoch microseconds with
   * integer arithmetic; 0 for a missing or malformed value.
   *
   * @param value JSON value
   * @return microseconds
   */
  static long nanosToMicros(JsonNode value) {
    if (value == null) {
      return 0;
    }
    if (value.isIntegralNumber()) {
      return Math.floorDiv(value.asLong(), 1000L);
    }
    if (value.isNumber()) {
      return (long) Math.floor(value.asDouble() / 1000.0);
    }
    if (value.isTextual() && DIGITS.matcher(value.asText()).matches()) {
      return new BigInteger(value.asText()).divide(THOUSAND).longValue();
    }
    return 0;
  }

  /**
   * Converts one OTLP/JSON {@code AnyValue} (null when empty).
   *
   * @param value JSON value
   * @return String, Boolean, Long, Double, List or Map
   */
  static Object anyValue(JsonNode value) {
    if (value == null || !value.isObject()) {
      return null;
    }
    JsonNode v;
    if ((v = value.get("stringValue")) != null && v.isTextual()) {
      return v.asText();
    }
    if ((v = value.get("boolValue")) != null && v.isBoolean()) {
      return v.asBoolean();
    }
    if ((v = value.get("intValue")) != null) {
      if (v.isIntegralNumber()) {
        return v.asLong();
      }
      try {
        return Long.parseLong(v.asText());
      } catch (NumberFormatException e) {
        return v.asText();
      }
    }
    if ((v = value.get("doubleValue")) != null) {
      if (v.isNumber()) {
        return v.asDouble();
      }
      try {
        return Double.parseDouble(v.asText());
      } catch (NumberFormatException e) {
        return v.asText();
      }
    }
    if ((v = value.get("bytesValue")) != null && v.isTextual()) {
      return v.asText();
    }
    if ((v = value.get("arrayValue")) != null && v.isObject()) {
      List<Object> out = new ArrayList<>();
      for (JsonNode item : v.path("values")) {
        Object o = anyValue(item);
        if (o != null) {
          out.add(o);
        }
      }
      return out;
    }
    if ((v = value.get("kvlistValue")) != null && v.isObject()) {
      return keyValues(v.get("values"));
    }
    return null;
  }

  /**
   * Converts an OTLP/JSON {@code KeyValue[]} list into an attribute map.
   *
   * @param values JSON array
   * @return attributes in order
   */
  static Map<String, Object> keyValues(JsonNode values) {
    Map<String, Object> out = new LinkedHashMap<>();
    if (values == null) {
      return out;
    }
    for (JsonNode kv : values) {
      JsonNode key = kv.get("key");
      Object value = anyValue(kv.get("value"));
      if (key != null && key.isTextual() && value != null) {
        out.put(key.asText(), value);
      }
    }
    return out;
  }

  private static String text(JsonNode node, String field) {
    JsonNode v = node.get(field);
    return v != null && v.isTextual() ? v.asText() : null;
  }

  private static String scalar(JsonNode v) {
    return v == null || v.isNull() || v.isContainerNode() ? "" : v.asText();
  }

  static Span.Kind otlpKind(JsonNode kind) {
    String k = scalar(kind);
    switch (k) {
      case "1":
        return Span.Kind.INTERNAL;
      case "2":
        return Span.Kind.SERVER;
      case "3":
        return Span.Kind.CLIENT;
      case "4":
        return Span.Kind.PRODUCER;
      case "5":
        return Span.Kind.CONSUMER;
      default:
        return Span.Kind.parse(k);
    }
  }

  static Span.StatusCode otlpStatus(JsonNode code) {
    String c = scalar(code);
    if (c.equals("2")) {
      return Span.StatusCode.ERROR;
    }
    if (c.equals("1")) {
      return Span.StatusCode.OK;
    }
    return Span.StatusCode.parse(c);
  }

  private static String optionalId(String id) {
    return id == null || id.isEmpty() || ZERO.matcher(id).matches() ? null : id;
  }

  /**
   * Converts one OTLP/JSON span.
   *
   * @param span JSON span
   * @param serviceName service of the enclosing resource
   * @return normalized span
   */
  static Span otlpSpan(JsonNode span, String serviceName) {
    long start = nanosToMicros(span.get("startTimeUnixNano"));
    long end = nanosToMicros(span.get("endTimeUnixNano"));
    String name = text(span, "name");
    Span.Builder b =
        Span.builder(
                nullToEmpty(text(span, "traceId")),
                nullToEmpty(text(span, "spanId")),
                name == null ? "" : name)
            .parentSpanId(optionalId(text(span, "parentSpanId")))
            .kind(otlpKind(span.get("kind")))
            .serviceName(serviceName)
            .times(start, end)
            .attributes(keyValues(span.get("attributes")));
    JsonNode status = span.path("status");
    String message = text(status, "message");
    b.status(otlpStatus(status.get("code")), message == null || message.isEmpty() ? null : message);
    for (JsonNode e : span.path("events")) {
      String en = text(e, "name");
      b.event(
          new Span.Event(
              en == null ? "" : en,
              nanosToMicros(e.get("timeUnixNano")),
              keyValues(e.get("attributes"))));
    }
    for (JsonNode l : span.path("links")) {
      String traceId = text(l, "traceId");
      String spanId = text(l, "spanId");
      if (validIds(traceId, spanId)) {
        b.link(new Span.Link(traceId, spanId, keyValues(l.get("attributes"))));
      }
    }
    return b.build();
  }

  private static String nullToEmpty(String s) {
    return s == null ? "" : s;
  }

  /**
   * Converts an OTLP/JSON {@code TracesData} into traces grouped by trace id (order of first
   * appearance).
   *
   * @param data the {@code result} object (may be null)
   * @return traces
   */
  static List<Trace> otlpTraces(JsonNode data) {
    Map<String, List<Span>> byId = new LinkedHashMap<>();
    if (data == null) {
      return List.of();
    }
    for (JsonNode rs : data.path("resourceSpans")) {
      Object svc = keyValues(rs.path("resource").get("attributes")).get("service.name");
      String service =
          svc instanceof String && !((String) svc).isEmpty() ? (String) svc : UNKNOWN_SERVICE;
      for (JsonNode ss : rs.path("scopeSpans")) {
        for (JsonNode raw : ss.path("spans")) {
          Span span = otlpSpan(raw, service);
          byId.computeIfAbsent(span.traceId(), k -> new ArrayList<>()).add(span);
        }
      }
    }
    List<Trace> out = new ArrayList<>();
    byId.forEach((id, spans) -> out.add(new Trace(id, spans)));
    return out;
  }

  // ---------------------------------------------------------------------------------------------
  // Legacy UI model (/api/traces/{id})

  private static Object legacyValue(JsonNode tag) {
    JsonNode v = tag.get("value");
    if (v == null) {
      return null;
    }
    if (v.isTextual()) {
      return v.asText();
    }
    if (v.isBoolean()) {
      return v.asBoolean();
    }
    if (v.isIntegralNumber()) {
      return v.asLong();
    }
    if (v.isNumber()) {
      return v.asDouble();
    }
    return null;
  }

  private static Map<String, Object> legacyTags(JsonNode tags, Set<String> skip) {
    Map<String, Object> out = new LinkedHashMap<>();
    if (tags == null) {
      return out;
    }
    for (JsonNode tag : tags) {
      String key = text(tag, "key");
      Object value = legacyValue(tag);
      if (key != null && value != null && !skip.contains(key)) {
        out.put(key, value);
      }
    }
    return out;
  }

  private static long micros(JsonNode v) {
    return v != null && v.isNumber() ? v.asLong() : 0;
  }

  private static Span legacySpan(JsonNode span, JsonNode processes, String traceId) {
    Map<String, Object> tags = legacyTags(span.get("tags"), Set.of());
    long start = micros(span.get("startTime"));
    long duration = micros(span.get("duration"));
    JsonNode process = processes.path(nullToEmpty(text(span, "processID")));
    String service = text(process, "serviceName");
    Object kindTag = tags.get("span.kind");
    Object statusTag = tags.get("otel.status_code");
    String statusText =
        statusTag instanceof String ? ((String) statusTag).toUpperCase(Locale.ROOT) : "";
    boolean error = statusText.equals("ERROR") || Boolean.TRUE.equals(tags.get("error"));
    Object description = tags.get("otel.status_description");

    JsonNode parentRef = null;
    List<JsonNode> refs = new ArrayList<>();
    for (JsonNode r : span.path("references")) {
      refs.add(r);
    }
    for (JsonNode r : refs) {
      if (sameTrace(r, traceId) && "CHILD_OF".equals(text(r, "refType"))) {
        parentRef = r;
        break;
      }
    }
    if (parentRef == null) {
      for (JsonNode r : refs) {
        if (sameTrace(r, traceId)) {
          parentRef = r;
          break;
        }
      }
    }
    String operation = text(span, "operationName");
    Span.Builder b =
        Span.builder(traceId, nullToEmpty(text(span, "spanID")), operation == null ? "" : operation)
            .kind(kindTag instanceof String ? legacyKind((String) kindTag) : Span.Kind.UNSPECIFIED)
            .serviceName(service == null ? UNKNOWN_SERVICE : service)
            .times(start, start + duration)
            .durationMicros(duration)
            .attributes(legacyTags(span.get("tags"), SYNTHETIC_TAGS));
    if (error) {
      b.status(
          Span.StatusCode.ERROR,
          description instanceof String && !((String) description).isEmpty()
              ? (String) description
              : null);
    } else if (statusText.equals("OK")) {
      b.status(Span.StatusCode.OK, null);
    }
    if (parentRef != null) {
      b.parentSpanId(optionalId(text(parentRef, "spanID")));
    }
    for (JsonNode log : span.path("logs")) {
      Map<String, Object> fields = legacyTags(log.get("fields"), Set.of());
      Object event = fields.get("event");
      b.event(
          new Span.Event(
              event instanceof String ? (String) event : "log",
              micros(log.get("timestamp")),
              legacyTags(log.get("fields"), Set.of("event"))));
    }
    for (JsonNode r : refs) {
      if (r != parentRef && validIds(text(r, "traceID"), text(r, "spanID"))) {
        b.link(new Span.Link(text(r, "traceID"), text(r, "spanID"), Map.of()));
      }
    }
    return b.build();
  }

  /** True when both ids normalise (links with broken ids are dropped, not fatal). */
  static boolean validIds(String traceId, String spanId) {
    if (traceId == null || spanId == null || !TraceIds.isValidTraceId(traceId)) {
      return false;
    }
    try {
      TraceIds.normalizeSpanId(spanId);
      return true;
    } catch (ValidationException e) {
      return false;
    }
  }

  private static boolean sameTrace(JsonNode ref, String traceId) {
    String id = text(ref, "traceID");
    return id != null
        && TraceIds.isValidTraceId(id)
        && TraceIds.normalizeTraceId(id).equals(traceId);
  }

  private static Span.Kind legacyKind(String kind) {
    Span.Kind k = Span.Kind.parse(kind);
    return kind.equals(kind.toLowerCase(Locale.ROOT)) ? k : Span.Kind.UNSPECIFIED;
  }

  /**
   * Converts the legacy {@code /api/traces/{id}} body ({@code {data: [{traceID, spans,
   * processes}]}}).
   *
   * @param body JSON body
   * @return traces
   */
  static List<Trace> legacyTraces(JsonNode body) {
    List<Trace> out = new ArrayList<>();
    for (JsonNode t : body.path("data")) {
      String traceId = TraceIds.normalizeTraceId(nullToEmpty(text(t, "traceID")));
      JsonNode processes = t.path("processes");
      List<Span> spans = new ArrayList<>();
      for (JsonNode s : t.path("spans")) {
        spans.add(legacySpan(s, processes, traceId));
      }
      out.add(new Trace(traceId, spans));
    }
    return out;
  }

  // ---------------------------------------------------------------------------------------------
  // Summaries, dependencies, operations

  /**
   * Converts {@code /api/v3/trace-summaries} entries.
   *
   * @param body JSON body
   * @return summaries
   */
  static List<TraceSummary> summaries(JsonNode body) {
    List<TraceSummary> out = new ArrayList<>();
    for (JsonNode s : body.path("summaries")) {
      long start = nanosToMicros(s.get("minStartTimeUnixNano"));
      long end = nanosToMicros(s.get("maxEndTimeUnixNano"));
      String rootService = text(s, "rootServiceName");
      String rootName = text(s, "rootOperationName");
      out.add(
          new TraceSummary(
              nullToEmpty(text(s, "traceId")),
              rootService == null || rootService.isEmpty() ? null : rootService,
              rootName == null || rootName.isEmpty() ? null : rootName,
              start,
              Math.max(0, end - start),
              s.path("spanCount").asInt(0)));
    }
    return out;
  }

  /**
   * Converts the legacy {@code /api/dependencies} body.
   *
   * @param body JSON body
   * @return links
   */
  static List<DependencyLink> dependencies(JsonNode body) {
    List<DependencyLink> out = new ArrayList<>();
    for (JsonNode d : body.path("data")) {
      String parent = text(d, "parent");
      String child = text(d, "child");
      if (parent != null && child != null) {
        out.add(new DependencyLink(parent, child, d.path("callCount").asLong(0)));
      }
    }
    return out;
  }

  /**
   * Converts the {@code /api/v3/operations} body.
   *
   * @param body JSON body
   * @return operations in server order
   */
  static List<JaegerOperation> operations(JsonNode body) {
    List<JaegerOperation> out = new ArrayList<>();
    for (JsonNode o : body.path("operations")) {
      String name = text(o, "name");
      if (name != null) {
        String kind = text(o, "spanKind");
        out.add(new JaegerOperation(name, kind == null ? "" : kind));
      }
    }
    return out;
  }
}
