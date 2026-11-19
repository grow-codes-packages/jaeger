// Service-owned: generated once by sdk-kit, then maintained in this repository.

import {
  CancelledError,
  type HealthStatus,
  type HttpTransport,
  type TransportOptions,
  UnsupportedError,
  healthStatus,
  isHealthyWord,
  probeHealth,
} from "./core/index.js";
import { JaegerConfig, type JaegerConfigOptions } from "./config.js";
import { JaegerQueryClient, createJaegerTransport } from "./query.js";
import {
  JaegerRemoteSampler,
  type JaegerRemoteSamplerOptions,
  type JaegerSamplingStrategy,
  fetchSamplingStrategy,
} from "./sampler.js";
import {
  initTracing,
  type JaegerInitTracingOptions,
  type JaegerTracingConfig,
  type JaegerTracingHandle,
} from "./setup.js";

/** Per-call options. */
export interface CallOptions {
  /** Cancels the call (`CancelledError`). */
  signal?: AbortSignal;
}

/** Test seams (undici `MockAgent`, fake sleep, clock). Not part of the stable API. */
export type ClientInternals = Pick<TransportOptions, "dispatcher" | "sleep" | "now">;

/** Build information embedded in the Jaeger UI (`JAEGER_VERSION`). */
export interface JaegerBuildInfo {
  /** e.g. `v2.23.0`. */
  gitVersion?: string;
  gitCommit?: string;
  buildDate?: string;
}

const VERSION_RE = /JAEGER_VERSION\s*=\s*(\{[^;]*?\})\s*;/;
const TARGET_INFO_RE = /^target_info\{[^}]*\bservice_version="v?([^"]+)"/m;

function stripV(version: string): string {
  return version.replace(/^v(?=\d)/, "");
}

/**
 * Client for Jaeger: health, ping and version of the query service, plus the query API
 * ({@link query}), tracing setup ({@link initTracing}) and remote sampling ({@link remoteSampler}).
 * {@link close} also shuts down every tracing handle and sampler created through this client.
 */
export class JaegerClient {
  /** The immutable configuration. */
  readonly config: JaegerConfig;
  readonly #transport: HttpTransport;
  readonly #healthTransport: HttpTransport;
  readonly #internals: ClientInternals;
  readonly #handles = new Set<JaegerTracingHandle>();
  readonly #samplers = new Set<JaegerRemoteSampler>();
  #query: JaegerQueryClient | undefined;
  #closed = false;

  /** Accepts a {@link JaegerConfig} or plain options. */
  constructor(config: JaegerConfigOptions = {}, internals: ClientInternals = {}) {
    const cfg = JaegerConfig.from(config);
    this.config = cfg;
    this.#internals = internals;
    this.#transport = createJaegerTransport(cfg, internals);
    this.#healthTransport =
      cfg.token === undefined ? this.#transport : createJaegerTransport(cfg, internals, false);
  }

  /** Builds a client from `GROW_JAEGER_*` (and vendor) environment variables. */
  static fromEnv(overrides: JaegerConfigOptions = {}): JaegerClient {
    return new JaegerClient(JaegerConfig.fromEnv(overrides));
  }

  /** The shared HTTP transport (for service modules). @internal */
  get transport(): HttpTransport {
    return this.#transport;
  }

  /** The trace query client (shares this client's connections). */
  get query(): JaegerQueryClient {
    this.#query ??= new JaegerQueryClient(this, this.#internals);
    return this.#query;
  }

  /**
   * Liveness of the query service: `GET /` on the query port (the stack catalog health target),
   * without the token. With `healthUrl` set, the healthcheckv2 `/status` answer is added (`healthy=false` when it
   * reports unhealthy). Never throws for an unhealthy or unreachable service (only `CANCELLED`
   * and `CONFIG`).
   */
  health(options: CallOptions = {}): Promise<HealthStatus> {
    return probeHealth(async () => {
      await this.#healthTransport.get("/", {
        operation: "health",
        responseType: "none",
        ...options,
      });
      const healthUrl = this.config.healthUrl;
      if (healthUrl === undefined) return healthStatus("ok");
      const res = await this.#healthTransport.get<Record<string, unknown> | undefined>(healthUrl, {
        operation: "health",
        ...options,
      });
      const body = res.data ?? {};
      const word = typeof body.status === "string" ? body.status : "ok";
      const healthy =
        typeof body.healthy === "boolean"
          ? body.healthy
          : isHealthyWord(word) || word === "StatusOK";
      const details: Record<string, unknown> = { status: word };
      if (body.start_time !== undefined) details.startTime = body.start_time;
      if (body.status_time !== undefined) details.statusTime = body.status_time;
      return healthStatus(healthy ? "ok" : word, details, healthy);
    });
  }

  /** Round trip through the query API: `GET /api/v3/services`. Returns the latency in ms. */
  async ping(options: CallOptions = {}): Promise<number> {
    const start = performance.now();
    await this.#transport.get("/api/v3/services", {
      operation: "ping",
      responseType: "none",
      ...options,
    });
    return performance.now() - start;
  }

  /**
   * Build information parsed from the UI page (`GET /`); `null` when the page has none or the UI
   * is disabled (404).
   */
  async buildInfo(options: CallOptions = {}): Promise<JaegerBuildInfo | null> {
    const res = await this.#transport.get<string | undefined>("/", {
      operation: "serverVersion",
      responseType: "text",
      allowStatus: [404],
      ...options,
    });
    const match = res.status === 404 ? null : VERSION_RE.exec(res.data ?? "");
    if (match === null) return null;
    try {
      const parsed = JSON.parse(match[1] as string) as Record<string, unknown>;
      const info: JaegerBuildInfo = {};
      for (const key of ["gitVersion", "gitCommit", "buildDate"] as const) {
        const value = parsed[key];
        if (typeof value === "string" && value !== "") info[key] = value;
      }
      return info;
    } catch {
      return null;
    }
  }

  /**
   * Server version without the leading `v` (e.g. `2.23.0`): parsed from `JAEGER_VERSION` in the
   * UI page, else from `target_info{service_version=…}` at `metricsUrl`. Throws
   * `UnsupportedError` when neither source reports one (a 404 from either source is not
   * `NotFoundError`).
   */
  async serverVersion(options: CallOptions = {}): Promise<string> {
    const info = await this.buildInfo(options);
    if (info?.gitVersion !== undefined) return stripV(info.gitVersion);
    const metricsUrl = this.config.metricsUrl;
    if (metricsUrl !== undefined) {
      const res = await this.#transport.get<string | undefined>(metricsUrl, {
        operation: "serverVersion",
        responseType: "text",
        allowStatus: [404],
        ...options,
      });
      const match = res.status === 404 ? null : TARGET_INFO_RE.exec(res.data ?? "");
      if (match !== null) return stripV(match[1] as string);
    }
    throw new UnsupportedError("serverVersion: the service does not report a version");
  }

  /**
   * Fetches the remote sampling strategy of a service once (`GET {samplingUrl}?service=`), with
   * this client's settings and token. A blank service throws `ValidationError` without a
   * request.
   */
  samplingStrategy(service: string, options: CallOptions = {}): Promise<JaegerSamplingStrategy> {
    return fetchSamplingStrategy(this.#transport, this.config.samplingUrl, service, options);
  }

  /**
   * Starts OpenTelemetry tracing to Jaeger (see the package-level `initTracing`); a remote
   * sampler it creates uses this client's configuration (`samplingUrl`, token, TLS). The handle
   * is shut down by {@link close}.
   */
  initTracing(
    config: JaegerTracingConfig,
    options: JaegerInitTracingOptions = {},
  ): JaegerTracingHandle {
    this.#assertOpen("initTracing");
    const handle = initTracing(config, { samplerConfig: this.config, ...options });
    this.#handles.add(handle);
    return handle;
  }

  /**
   * Creates a {@link JaegerRemoteSampler} that uses this client's settings (`samplingUrl`,
   * timeouts, TLS, proxy, headers). It is closed by {@link close}.
   */
  remoteSampler(
    options: Omit<JaegerRemoteSamplerOptions, "config"> & { config?: JaegerConfigOptions },
  ): JaegerRemoteSampler {
    this.#assertOpen("remoteSampler");
    const sampler = new JaegerRemoteSampler(
      { ...options, config: options.config ?? this.config },
      this.#internals,
    );
    this.#samplers.add(sampler);
    return sampler;
  }

  #assertOpen(operation: string): void {
    if (this.#closed) {
      throw new CancelledError(`${operation}: client is closed`);
    }
  }

  /**
   * Shuts down the tracing handles (flushing their batch processors) and samplers created
   * through this client, then releases connections. Idempotent.
   */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    const handles = [...this.#handles].map((h) => h.shutdown());
    const samplers = [...this.#samplers].map((s) => s.close());
    this.#handles.clear();
    this.#samplers.clear();
    await Promise.allSettled([...handles, ...samplers]);
    if (this.#healthTransport !== this.#transport) await this.#healthTransport.close();
    await this.#transport.close();
  }
}
