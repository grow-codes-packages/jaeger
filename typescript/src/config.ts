// Service-owned: generated once by sdk-kit, then maintained in this repository.

import {
  type CommonConfig,
  type CommonConfigOptions,
  type CommonVendorEnv,
  ConfigError,
  type EnvMap,
  EnvReader,
  type Hooks,
  type ClientLogger,
  REDACTED,
  type RetryPolicy,
  Secret,
  type TlsOptions,
  describeCommonConfig,
  mergeOptions,
  readCommonEnv,
  redactUrl,
  resolveCommonConfig,
  secretValues,
  validateCommonConfig,
} from "./core/index.js";

const INSPECT = Symbol.for("nodejs.util.inspect.custom");

/** Options accepted by {@link JaegerConfig} (all optional). */
export interface JaegerConfigOptions extends CommonConfigOptions {
  /**
   * Query API base URL (port 16686). A reverse-proxy base path is kept
   * (`https://host/jaeger` → `https://host/jaeger/api/v3/...`).
   * Env `GROW_JAEGER_URL`, then `JAEGER_QUERY_URL`. Default `http://127.0.0.1:16686`.
   */
  url?: string;
  /**
   * Optional bearer token sent as `Authorization: Bearer <token>` to the query API (stock Jaeger
   * has no auth; use it for a reverse proxy or `bearer_token_propagation`). Env `GROW_JAEGER_TOKEN`.
   */
  token?: string | Secret;
  /**
   * Remote sampling endpoint (port 5778, path `/sampling`). Env `GROW_JAEGER_SAMPLING_URL`, then
   * `JAEGER_SAMPLING_ENDPOINT`. Default `http://127.0.0.1:5778/sampling`.
   */
  samplingUrl?: string;
  /**
   * Optional healthcheckv2 URL (port 13133, `/status`); when set, `health()` adds its detail.
   * Env `GROW_JAEGER_HEALTH_URL`.
   */
  healthUrl?: string;
  /**
   * Optional Prometheus metrics URL (port 8888, `/metrics`); `serverVersion()` falls back to its
   * `target_info` series. Env `GROW_JAEGER_METRICS_URL`.
   */
  metricsUrl?: string;
}

/** Vendor environment variables read after `GROW_JAEGER_*`, in order. */
export const VENDOR_ENV: CommonVendorEnv & {
  token: readonly string[];
  samplingUrl: readonly string[];
} = {
  url: ["JAEGER_QUERY_URL"],
  token: [],
  samplingUrl: ["JAEGER_SAMPLING_ENDPOINT"],
};

function isHttpUrl(value: string): boolean {
  try {
    const u = new URL(value);
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

/** Immutable configuration of {@link JaegerClient}. */
export class JaegerConfig implements CommonConfig {
  /** Default query endpoint. */
  static readonly DEFAULT_URL = "http://127.0.0.1:16686";
  /** Default remote sampling endpoint. */
  static readonly DEFAULT_SAMPLING_URL = "http://127.0.0.1:5778/sampling";
  /** Prefix of every environment variable. */
  static readonly ENV_PREFIX = "GROW_JAEGER_";

  readonly url: string;
  readonly connectTimeoutMs: number;
  readonly requestTimeoutMs: number;
  readonly totalTimeoutMs: number;
  readonly retry: RetryPolicy;
  readonly tls: Readonly<TlsOptions>;
  readonly proxy?: string;
  readonly headers: Readonly<Record<string, string>>;
  readonly userAgentSuffix?: string;
  readonly hooks?: Hooks;
  readonly logger?: ClientLogger;
  /** Optional bearer token for the query API. */
  readonly token?: Secret;
  /** Remote sampling endpoint. */
  readonly samplingUrl: string;
  /** Optional healthcheckv2 `/status` URL. */
  readonly healthUrl?: string;
  /** Optional Prometheus metrics URL (version fallback). */
  readonly metricsUrl?: string;

  /** Applies defaults, validates (throws {@link ConfigError}) and freezes. */
  constructor(options: JaegerConfigOptions = {}) {
    const common = resolveCommonConfig(options, JaegerConfig.DEFAULT_URL);
    this.url = common.url;
    this.connectTimeoutMs = common.connectTimeoutMs;
    this.requestTimeoutMs = common.requestTimeoutMs;
    this.totalTimeoutMs = common.totalTimeoutMs;
    this.retry = common.retry;
    this.tls = common.tls;
    this.headers = common.headers;
    if (common.proxy !== undefined) this.proxy = common.proxy;
    if (common.userAgentSuffix !== undefined) this.userAgentSuffix = common.userAgentSuffix;
    if (common.hooks !== undefined) this.hooks = common.hooks;
    if (common.logger !== undefined) this.logger = common.logger;
    const token = Secret.from(options.token);
    if (token !== undefined) this.token = token;
    this.samplingUrl = options.samplingUrl ?? JaegerConfig.DEFAULT_SAMPLING_URL;
    if (options.healthUrl !== undefined) this.healthUrl = options.healthUrl;
    if (options.metricsUrl !== undefined) this.metricsUrl = options.metricsUrl;
    this.validate();
    Object.freeze(this);
  }

  /** Returns `options` when it already is a config, otherwise builds one. */
  static from(options: JaegerConfigOptions = {}): JaegerConfig {
    if (options instanceof JaegerConfig) return options;
    return new JaegerConfig(options);
  }

  /**
   * Builds a config from the environment. Precedence: explicit `overrides` >
   * `GROW_JAEGER_<NAME>` > vendor variables ({@link VENDOR_ENV}: `JAEGER_QUERY_URL`,
   * `JAEGER_SAMPLING_ENDPOINT`) > defaults.
   */
  static fromEnv(overrides: JaegerConfigOptions = {}, env: EnvMap = process.env): JaegerConfig {
    const reader = new EnvReader(JaegerConfig.ENV_PREFIX, env);
    const fromEnv: JaegerConfigOptions = readCommonEnv(reader, VENDOR_ENV);
    const token = reader.secret("TOKEN", VENDOR_ENV.token);
    if (token !== undefined) fromEnv.token = token;
    const samplingUrl = reader.string("SAMPLING_URL", VENDOR_ENV.samplingUrl);
    if (samplingUrl !== undefined) fromEnv.samplingUrl = samplingUrl;
    const healthUrl = reader.string("HEALTH_URL");
    if (healthUrl !== undefined) fromEnv.healthUrl = healthUrl;
    const metricsUrl = reader.string("METRICS_URL");
    if (metricsUrl !== undefined) fromEnv.metricsUrl = metricsUrl;
    const merged = mergeOptions<JaegerConfigOptions>(fromEnv, overrides);
    return new JaegerConfig(merged);
  }

  /** Throws {@link ConfigError} when a setting is invalid. */
  validate(): void {
    validateCommonConfig(this);
    if (this.token?.reveal().trim() === "")
      throw new ConfigError("invalid configuration: token must not be blank");
    for (const key of ["samplingUrl", "healthUrl", "metricsUrl"] as const) {
      const value = this[key];
      if (value !== undefined && !isHttpUrl(value))
        throw new ConfigError(`invalid configuration: ${key} must be an http or https URL`);
    }
  }

  /** Credential values to redact from error messages and logs. */
  knownSecrets(): string[] {
    return secretValues(this.token);
  }

  /** Debug form with every credential redacted. */
  toJSON(): Record<string, unknown> {
    const out: Record<string, unknown> = {
      ...describeCommonConfig(this),
      token: this.token === undefined ? null : REDACTED,
      samplingUrl: redactUrl(this.samplingUrl),
    };
    if (this.healthUrl !== undefined) out.healthUrl = redactUrl(this.healthUrl);
    if (this.metricsUrl !== undefined) out.metricsUrl = redactUrl(this.metricsUrl);
    return out;
  }

  toString(): string {
    return `JaegerConfig(${JSON.stringify(this.toJSON())})`;
  }

  [INSPECT](): string {
    return this.toString();
  }
}
