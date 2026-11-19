package codes.grow.packages.jaeger;

import java.time.Instant;
import java.util.Optional;

/**
 * Options of {@link JaegerQueryClient#getTrace(String, GetTraceOptions)}.
 *
 * @param start start of the lookup window (a storage hint); empty = 7 days before {@code end}
 * @param end end of the lookup window; empty = now
 * @param rawTraces skip Jaeger's span adjusters ({@code rawTraces=true}); empty = server default
 */
public record GetTraceOptions(
    Optional<Instant> start, Optional<Instant> end, Optional<Boolean> rawTraces) {
  /**
   * Normalises null optionals to empty.
   *
   * @param start window start
   * @param end window end
   * @param rawTraces raw flag
   */
  public GetTraceOptions {
    start = start == null ? Optional.empty() : start;
    end = end == null ? Optional.empty() : end;
    rawTraces = rawTraces == null ? Optional.empty() : rawTraces;
  }

  /**
   * The defaults: last 7 days, server-side adjusters on.
   *
   * @return default options
   */
  public static GetTraceOptions defaults() {
    return new GetTraceOptions(Optional.empty(), Optional.empty(), Optional.empty());
  }

  /**
   * Options with an explicit window.
   *
   * @param start window start or {@code null}
   * @param end window end or {@code null}
   * @return options
   */
  public static GetTraceOptions window(Instant start, Instant end) {
    return new GetTraceOptions(
        Optional.ofNullable(start), Optional.ofNullable(end), Optional.empty());
  }

  /**
   * Returns a copy with {@code rawTraces} set.
   *
   * @param raw skip Jaeger's adjusters
   * @return options
   */
  public GetTraceOptions withRawTraces(boolean raw) {
    return new GetTraceOptions(start, end, Optional.of(raw));
  }
}
