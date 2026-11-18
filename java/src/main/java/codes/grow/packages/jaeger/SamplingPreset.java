package codes.grow.packages.jaeger;

import codes.grow.packages.jaeger.tracing.TracingConfig;
import codes.grow.packages.jaeger.tracing.TracingConfig.SamplerType;
import java.util.Objects;

/**
 * A named head-sampling choice for {@link TracingConfig} (the same presets in every language).
 *
 * <pre>{@code
 * TracingConfig config =
 *     SamplingPreset.production().applyTo(TracingConfig.builder("orders")).build();
 * }</pre>
 *
 * @param sampler sampler type
 * @param ratio ratio for the {@code *traceidratio} samplers (0..1)
 */
public record SamplingPreset(SamplerType sampler, double ratio) {
  /** Records every trace ({@code always_on}). */
  public static final SamplingPreset DEVELOPMENT = new SamplingPreset(SamplerType.ALWAYS_ON, 1);

  /** Keeps 10 % of new traces and follows the parent ({@code parentbased_traceidratio} 0.1). */
  public static final SamplingPreset PRODUCTION =
      new SamplingPreset(SamplerType.PARENTBASED_TRACEIDRATIO, 0.1);

  /** Records new traces and follows the parent (the OpenTelemetry default). */
  public static final SamplingPreset FOLLOW_PARENT =
      new SamplingPreset(SamplerType.PARENTBASED_ALWAYS_ON, 1);

  /** Records nothing ({@code always_off}). */
  public static final SamplingPreset OFF = new SamplingPreset(SamplerType.ALWAYS_OFF, 1);

  /**
   * Validates the preset.
   *
   * @param sampler sampler type
   * @param ratio ratio 0..1
   */
  public SamplingPreset {
    Objects.requireNonNull(sampler, "sampler");
    if (!(ratio >= 0 && ratio <= 1)) {
      throw new IllegalArgumentException("ratio must be between 0 and 1");
    }
  }

  /**
   * The {@link #DEVELOPMENT} preset.
   *
   * @return preset
   */
  public static SamplingPreset development() {
    return DEVELOPMENT;
  }

  /**
   * The {@link #PRODUCTION} preset.
   *
   * @return preset
   */
  public static SamplingPreset production() {
    return PRODUCTION;
  }

  /**
   * The {@link #FOLLOW_PARENT} preset.
   *
   * @return preset
   */
  public static SamplingPreset followParent() {
    return FOLLOW_PARENT;
  }

  /**
   * The {@link #OFF} preset.
   *
   * @return preset
   */
  public static SamplingPreset off() {
    return OFF;
  }

  /**
   * A {@code parentbased_traceidratio} preset; a ratio of 0 or less gives {@code
   * parentbased_always_off}, 1 or more {@link #FOLLOW_PARENT}.
   *
   * @param ratio fraction of new traces to keep
   * @return preset
   */
  public static SamplingPreset ratio(double ratio) {
    if (ratio <= 0) {
      return new SamplingPreset(SamplerType.PARENTBASED_ALWAYS_OFF, 1);
    }
    if (ratio >= 1) {
      return FOLLOW_PARENT;
    }
    return new SamplingPreset(SamplerType.PARENTBASED_TRACEIDRATIO, ratio);
  }

  /**
   * Sets this preset's sampler and ratio on a builder.
   *
   * @param builder tracing config builder
   * @return the same builder
   */
  public TracingConfig.Builder applyTo(TracingConfig.Builder builder) {
    return builder.sampler(sampler).samplerRatio(ratio);
  }
}
