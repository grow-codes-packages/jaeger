package codes.grow.packages.jaeger;

import java.util.Optional;

/**
 * Build information embedded in the Jaeger UI page ({@code JAEGER_VERSION = {...};}).
 *
 * @param gitVersion version tag, e.g. {@code v2.23.0}
 * @param gitCommit commit hash
 * @param buildDate build date (RFC 3339)
 */
public record JaegerBuildInfo(
    Optional<String> gitVersion, Optional<String> gitCommit, Optional<String> buildDate) {
  /**
   * Normalises null optionals to empty.
   *
   * @param gitVersion version tag
   * @param gitCommit commit hash
   * @param buildDate build date
   */
  public JaegerBuildInfo {
    gitVersion = gitVersion == null ? Optional.empty() : gitVersion;
    gitCommit = gitCommit == null ? Optional.empty() : gitCommit;
    buildDate = buildDate == null ? Optional.empty() : buildDate;
  }
}
