package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.core.Json;
import codes.grow.packages.jaeger.core.MockServer;
import com.fasterxml.jackson.databind.JsonNode;
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/** Recorded Jaeger v2.23.0 responses (src/test/resources/fixtures, copied from the prep run). */
final class Fixtures {
  private Fixtures() {}

  static JsonNode load(String name) {
    try (InputStream in = Fixtures.class.getResourceAsStream("/fixtures/" + name + ".json")) {
      if (in == null) {
        throw new IllegalArgumentException("no fixture " + name);
      }
      return Json.tree(in.readAllBytes());
    } catch (IOException e) {
      throw new UncheckedIOException(e);
    }
  }

  /** The response body as sent on the wire (JSON text or plain text). */
  static String body(String name) {
    JsonNode body = load(name).path("response").get("body");
    if (body == null || body.isNull()) {
      return "";
    }
    return body.isTextual() ? body.asText() : Json.stringify(body);
  }

  static int status(String name) {
    return load(name).path("response").path("status").asInt();
  }

  /** Queues the recorded response (status, body, content type and traceresponse header). */
  static void enqueue(MockServer server, String name) {
    JsonNode headers = load(name).path("response").path("headers");
    List<String> h = new ArrayList<>();
    for (Map.Entry<String, JsonNode> e : headers.properties()) {
      String key = e.getKey().equalsIgnoreCase("content-type") ? "Content-Type" : e.getKey();
      h.add(key);
      h.add(e.getValue().asText());
    }
    server.enqueue(status(name), body(name), h.toArray(new String[0]));
  }
}
