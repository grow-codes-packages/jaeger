package codes.grow.packages.jaeger;

import java.util.Objects;

/**
 * One operation of a service as reported by {@code GET /api/v3/operations}.
 *
 * @param name operation (span) name
 * @param spanKind {@code server}, {@code client}, {@code producer}, {@code consumer}, {@code
 *     internal} or {@code ""} when the server reports none
 */
public record JaegerOperation(String name, String spanKind) {
  /**
   * Validates the fields.
   *
   * @param name operation name
   * @param spanKind span kind (null becomes {@code ""})
   */
  public JaegerOperation {
    Objects.requireNonNull(name, "name");
    spanKind = spanKind == null ? "" : spanKind;
  }
}
