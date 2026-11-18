// Service-owned: the Jaeger tracing preset — OTLP exporter defaults (HTTP/protobuf on 4318 or gRPC
// on 4317), environment precedence and the optional remote sampler.

import { readFileSync } from "node:fs";
import { credentials as grpcCredentials, Metadata } from "@grpc/grpc-js";
import { OTLPTraceExporter as OtlpGrpcExporter } from "@opentelemetry/exporter-trace-otlp-grpc";
import { OTLPTraceExporter as OtlpHttpExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { CompressionAlgorithm } from "@opentelemetry/otlp-exporter-base";
import { ParentBasedSampler, type Sampler, type SpanExporter } from "@opentelemetry/sdk-trace-base";
import { ConfigError, type EnvMap, type TlsOptions, parseDuration } from "./core/index.js";
import { JaegerRemoteSampler, type JaegerRemoteSamplerOptions } from "./sampler.js";
import {
  initTracingWith,
  type InitTracingOptions,
  type ResolvedTracingConfig,
  type SamplerName,
  type TracingConfig,
  type TracingHandle,
  type TracingProtocol,
} from "./tracing/index.js";

/** Default OTLP/HTTP traces endpoint of a Jaeger all-in-one / collector. */
export const JAEGER_OTLP_HTTP_ENDPOINT = "http://127.0.0.1:4318/v1/traces";
/** Default OTLP/gRPC endpoint of a Jaeger all-in-one / collector. */
export const JAEGER_OTLP_GRPC_ENDPOINT = "http://127.0.0.1:4317";
/** Default protocol of the Jaeger preset. */
export const JAEGER_DEFAULT_PROTOCOL: TracingProtocol = "http/protobuf";

/** Remote sampler setting of {@link JaegerTracingConfig}. */
export type RemoteSamplerSetting =
  JaegerRemoteSampler | (Partial<JaegerRemoteSamplerOptions> & { parentBased?: boolean }) | true;

/** {@link TracingConfig} plus the Jaeger remote sampler. */
export interface JaegerTracingConfig extends TracingConfig {
  /**
   * Use Jaeger remote sampling instead of `sampler`: an existing {@link JaegerRemoteSampler}
   * (not closed by the handle), `true`, or sampler options (`serviceName` defaults to the
   * tracing `serviceName`; the handle closes the sampler on `shutdown()`). The remote sampler is
   * the root of a `ParentBasedSampler` unless `parentBased: false`.
   */
  remoteSampler?: RemoteSamplerSetting;
}

/** Options of {@link initTracing}. */
export interface JaegerInitTracingOptions extends InitTracingOptions {
  /**
   * Environment used beneath the explicit config (see {@link tracingConfigFromEnv}); default
   * `process.env`, `false` ignores the environment.
   */
  env?: EnvMap | false;
}

/** A running Jaeger tracing setup. */
export interface JaegerTracingHandle extends TracingHandle {
  /** The remote sampler in use, if any. */
  readonly remoteSampler?: JaegerRemoteSampler;
  /** The exporter endpoint in use. */
  readonly endpoint: string;
}

const PROTOCOLS: readonly string[] = ["grpc", "http/protobuf", "native"];
const SAMPLERS: readonly string[] = [
  "always_on",
  "always_off",
  "traceidratio",
  "parentbased_always_on",
  "parentbased_always_off",
  "parentbased_traceidratio",
];

function hasPath(url: string): boolean {
  try {
    const path = new URL(url).pathname;
    return path !== "" && path !== "/";
  } catch {
    return true;
  }
}

function appendTracesPath(base: string): string {
  return `${base.replace(/\/+$/, "")}/v1/traces`;
}

/**
 * The exporter URL for a protocol: for `http/protobuf`, an endpoint without a path (a base such
 * as `http://jaeger:4318`) gets `/v1/traces`; any other endpoint is used as-is. Without an
 * endpoint, the Jaeger default for the protocol.
 */
export function resolveJaegerEndpoint(protocol: TracingProtocol, endpoint?: string): string {
  if (endpoint === undefined || endpoint === "")
    return protocol === "grpc" ? JAEGER_OTLP_GRPC_ENDPOINT : JAEGER_OTLP_HTTP_ENDPOINT;
  return protocol !== "grpc" && !hasPath(endpoint) ? appendTracesPath(endpoint) : endpoint;
}

class EnvChain {
  readonly #env: EnvMap;
  constructor(env: EnvMap) {
    this.#env = env;
  }

  /** First present variable of `names`. */
  hit(names: readonly string[]): { name: string; value: string } | undefined {
    for (const name of names) {
      const value = this.#env[name];
      if (value !== undefined && value.trim() !== "") return { name, value: value.trim() };
    }
    return undefined;
  }

  /** `GROW_JAEGER_OTLP_<s>` > `OTEL_EXPORTER_OTLP_TRACES_<s>` > `OTEL_EXPORTER_OTLP_<s>`. */
  otlp(suffix: string): { name: string; value: string } | undefined {
    return this.hit([
      `GROW_JAEGER_OTLP_${suffix}`,
      `OTEL_EXPORTER_OTLP_TRACES_${suffix}`,
      `OTEL_EXPORTER_OTLP_${suffix}`,
    ]);
  }
}

function parseKeyValues(raw: string, name: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of raw.split(",")) {
    if (part.trim() === "") continue;
    const eq = part.indexOf("=");
    if (eq <= 0)
      throw new ConfigError(
        `invalid value in environment variable ${name}: expected key=value pairs`,
      );
    try {
      out[decodeURIComponent(part.slice(0, eq).trim())] = decodeURIComponent(
        part.slice(eq + 1).trim(),
      );
    } catch {
      throw new ConfigError(`invalid value in environment variable ${name}: bad percent-encoding`);
    }
  }
  return out;
}

function parseRemoteArg(
  raw: string | undefined,
  name: string,
): Partial<JaegerRemoteSamplerOptions> {
  const out: Partial<JaegerRemoteSamplerOptions> = {};
  if (raw === undefined) return out;
  const values = parseKeyValues(raw, name);
  if (values.endpoint !== undefined) out.samplingUrl = values.endpoint;
  if (values.pollingIntervalMs !== undefined) {
    const ms = parseDuration(values.pollingIntervalMs);
    if (ms === undefined)
      throw new ConfigError(`invalid value in environment variable ${name}: pollingIntervalMs`);
    out.pollIntervalMs = ms;
  }
  if (values.initialSamplingRate !== undefined) {
    const r = Number(values.initialSamplingRate);
    if (!Number.isFinite(r))
      throw new ConfigError(`invalid value in environment variable ${name}: initialSamplingRate`);
    out.initialSamplingRate = r;
  }
  return out;
}

function definedOnly<T extends object>(value: T): Partial<T> {
  return Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined)) as Partial<T>;
}

/**
 * Builds a {@link JaegerTracingConfig} from the environment with `overrides` on top. Exporter
 * precedence (per field): explicit > `GROW_JAEGER_OTLP_*` > `OTEL_EXPORTER_OTLP_TRACES_*` >
 * `OTEL_EXPORTER_OTLP_*` > Jaeger default.
 *
 * | Field | Variables |
 * | - | - |
 * | `protocol` | `…_PROTOCOL` (`grpc`, `http/protobuf`) |
 * | `endpoint` | `GROW_JAEGER_OTLP_GRPC_ENDPOINT` / `GROW_JAEGER_OTLP_HTTP_ENDPOINT` (by protocol), `GROW_JAEGER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (as-is), `OTEL_EXPORTER_OTLP_ENDPOINT` (base; HTTP appends `/v1/traces`) |
 * | `headers` | `…_HEADERS` (`k=v,k2=v2`, percent-decoded) |
 * | `compression` | `…_COMPRESSION` (`gzip`, `none`) |
 * | `batch.exportTimeoutMs` | `…_TIMEOUT` (ms or `10s`) |
 * | `tls` | `…_CERTIFICATE`, `…_CLIENT_CERTIFICATE`, `…_CLIENT_KEY` (paths) |
 * | `serviceName` | `OTEL_SERVICE_NAME`, else `service.name` in `OTEL_RESOURCE_ATTRIBUTES` |
 * | `resourceAttributes` | `OTEL_RESOURCE_ATTRIBUTES` |
 * | `sampler`, `samplerRatio` | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG`; `jaeger_remote` / `parentbased_jaeger_remote` enable {@link JaegerTracingConfig.remoteSampler} (arg `endpoint=…,pollingIntervalMs=…,initialSamplingRate=…`) |
 * | `propagators` | `OTEL_PROPAGATORS` |
 *
 * A present but invalid value throws `ConfigError` naming the variable, never its value.
 */
export function tracingConfigFromEnv(
  overrides: Partial<JaegerTracingConfig> = {},
  env: EnvMap = process.env,
): JaegerTracingConfig {
  const chain = new EnvChain(env);
  const fromEnv: Partial<JaegerTracingConfig> = {};

  const protocolHit = chain.otlp("PROTOCOL");
  if (protocolHit !== undefined) {
    if (!PROTOCOLS.includes(protocolHit.value))
      throw new ConfigError(
        `invalid value in environment variable ${protocolHit.name}: expected grpc or http/protobuf`,
      );
    fromEnv.protocol = protocolHit.value as TracingProtocol;
  }
  const protocol = overrides.protocol ?? fromEnv.protocol ?? JAEGER_DEFAULT_PROTOCOL;
  const specific =
    protocol === "grpc" ? "GROW_JAEGER_OTLP_GRPC_ENDPOINT" : "GROW_JAEGER_OTLP_HTTP_ENDPOINT";
  const grow = chain.hit([specific, "GROW_JAEGER_OTLP_ENDPOINT"]);
  const tracesEndpoint = chain.hit(["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"]);
  const baseEndpoint = chain.hit(["OTEL_EXPORTER_OTLP_ENDPOINT"]);
  if (grow !== undefined) fromEnv.endpoint = resolveJaegerEndpoint(protocol, grow.value);
  else if (tracesEndpoint !== undefined) fromEnv.endpoint = tracesEndpoint.value;
  else if (baseEndpoint !== undefined)
    fromEnv.endpoint =
      protocol === "grpc" ? baseEndpoint.value : appendTracesPath(baseEndpoint.value);

  const headers = chain.otlp("HEADERS");
  if (headers !== undefined) fromEnv.headers = parseKeyValues(headers.value, headers.name);
  const compression = chain.otlp("COMPRESSION");
  if (compression !== undefined) {
    const value = compression.value.toLowerCase();
    if (value !== "gzip" && value !== "none")
      throw new ConfigError(
        `invalid value in environment variable ${compression.name}: expected gzip or none`,
      );
    fromEnv.compression = value;
  }
  const timeout = chain.otlp("TIMEOUT");
  if (timeout !== undefined) {
    const ms = parseDuration(timeout.value);
    if (ms === undefined || ms <= 0)
      throw new ConfigError(
        `invalid value in environment variable ${timeout.name}: expected a duration`,
      );
    fromEnv.batch = { exportTimeoutMs: ms };
  }
  const tls: TlsOptions = {};
  const ca = chain.otlp("CERTIFICATE");
  if (ca !== undefined) tls.caCertPath = ca.value;
  const cert = chain.otlp("CLIENT_CERTIFICATE");
  if (cert !== undefined) tls.clientCertPath = cert.value;
  const key = chain.otlp("CLIENT_KEY");
  if (key !== undefined) tls.clientKeyPath = key.value;
  if (Object.keys(tls).length > 0) fromEnv.tls = tls;

  const resource = chain.hit(["OTEL_RESOURCE_ATTRIBUTES"]);
  const resourceAttributes =
    resource === undefined ? {} : parseKeyValues(resource.value, resource.name);
  if (resource !== undefined) {
    const { "service.name": _name, ...rest } = resourceAttributes;
    fromEnv.resourceAttributes = rest;
  }
  const serviceName = chain.hit(["OTEL_SERVICE_NAME"])?.value ?? resourceAttributes["service.name"];
  if (serviceName !== undefined) fromEnv.serviceName = serviceName;

  const sampler = chain.hit(["OTEL_TRACES_SAMPLER"]);
  const samplerArg = chain.hit(["OTEL_TRACES_SAMPLER_ARG"]);
  if (sampler !== undefined) {
    const value = sampler.value.toLowerCase();
    if (value === "jaeger_remote" || value === "parentbased_jaeger_remote") {
      fromEnv.remoteSampler = {
        ...parseRemoteArg(samplerArg?.value, "OTEL_TRACES_SAMPLER_ARG"),
        parentBased: value === "parentbased_jaeger_remote",
      };
    } else {
      if (!SAMPLERS.includes(value))
        throw new ConfigError(
          `invalid value in environment variable ${sampler.name}: unknown sampler`,
        );
      fromEnv.sampler = value as SamplerName;
      if (value.endsWith("traceidratio") && samplerArg !== undefined) {
        const ratio = Number(samplerArg.value);
        if (!Number.isFinite(ratio) || ratio < 0 || ratio > 1)
          throw new ConfigError(
            `invalid value in environment variable ${samplerArg.name}: expected a ratio between 0 and 1`,
          );
        fromEnv.samplerRatio = ratio;
      }
    }
  }
  const propagators = chain.hit(["OTEL_PROPAGATORS"]);
  if (propagators !== undefined)
    fromEnv.propagators = propagators.value
      .split(",")
      .map((p) => p.trim())
      .filter((p) => p !== "");

  const explicit = definedOnly(overrides);
  const merged: Partial<JaegerTracingConfig> = { ...fromEnv, ...explicit };
  if (fromEnv.batch !== undefined || explicit.batch !== undefined)
    merged.batch = { ...fromEnv.batch, ...definedOnly(explicit.batch ?? {}) };
  if (fromEnv.tls !== undefined || explicit.tls !== undefined)
    merged.tls = { ...fromEnv.tls, ...definedOnly(explicit.tls ?? {}) };
  if (explicit.sampler !== undefined && explicit.remoteSampler === undefined)
    delete merged.remoteSampler;
  return { ...merged, serviceName: merged.serviceName ?? "" };
}

function readPem(path: string | undefined): Buffer | undefined {
  if (path === undefined) return undefined;
  try {
    return readFileSync(path);
  } catch {
    throw new ConfigError("tracing: cannot read a TLS file (check the configured path)");
  }
}

/**
 * Builds the OTLP span exporter for a resolved config (the `exporterFactory` of
 * `initTracingWith`): `http/protobuf` → `@opentelemetry/exporter-trace-otlp-http`, `grpc` →
 * `@opentelemetry/exporter-trace-otlp-grpc`. Headers, compression, timeout and TLS come from
 * the config. `native` throws `ConfigError` (Jaeger ingests OTLP).
 */
export function jaegerExporterFactory(config: ResolvedTracingConfig): SpanExporter {
  if (config.protocol === "native")
    throw new ConfigError("tracing: Jaeger has no native protocol; use grpc or http/protobuf");
  const url = resolveJaegerEndpoint(config.protocol, config.endpoint);
  const compression =
    config.compression === "gzip" ? CompressionAlgorithm.GZIP : CompressionAlgorithm.NONE;
  const ca = readPem(config.tls.caCertPath) ?? config.tls.caCertPem;
  const cert = readPem(config.tls.clientCertPath);
  const key = readPem(config.tls.clientKeyPath);
  const timeoutMillis = config.batch.exportTimeoutMs;
  if (config.protocol === "grpc") {
    const metadata = new Metadata();
    for (const [k, v] of Object.entries(config.headers)) metadata.set(k.toLowerCase(), v);
    const secure =
      url.startsWith("https:") ||
      ca !== undefined ||
      cert !== undefined ||
      config.tls.insecureSkipVerify === true;
    const credentials = secure
      ? grpcCredentials.createSsl(
          ca === undefined ? null : Buffer.from(ca),
          key ?? null,
          cert ?? null,
          config.tls.insecureSkipVerify === true ? { checkServerIdentity: () => undefined } : {},
        )
      : grpcCredentials.createInsecure();
    return new OtlpGrpcExporter({ url, metadata, compression, timeoutMillis, credentials });
  }
  const agent: Record<string, unknown> = {};
  if (ca !== undefined) agent.ca = ca;
  if (cert !== undefined) agent.cert = cert;
  if (key !== undefined) agent.key = key;
  if (config.tls.insecureSkipVerify === true) agent.rejectUnauthorized = false;
  return new OtlpHttpExporter({
    url,
    headers: { ...config.headers },
    compression,
    timeoutMillis,
    ...(Object.keys(agent).length > 0 ? { httpAgentOptions: agent } : {}),
  });
}

function installSampler(handle: TracingHandle, sampler: Sampler): void {
  // initTracingWith has no sampler option yet (reported to sdk-kit); the provider keeps the
  // options it hands to new tracers in `_tracerOptions`. Tracers are created lazily, so swapping
  // the sampler before the handle is returned affects every tracer.
  const options = (handle.tracerProvider as unknown as { _tracerOptions?: { sampler?: unknown } })
    ._tracerOptions;
  if (options?.sampler === undefined)
    throw new ConfigError(
      "tracing: this @opentelemetry/sdk-trace-base version cannot take a custom sampler",
    );
  options.sampler = sampler;
}

/**
 * Sets up OpenTelemetry tracing for Jaeger (SPEC §5): OTLP `http/protobuf` to
 * `http://127.0.0.1:4318/v1/traces` by default, or `grpc` to `http://127.0.0.1:4317`. The
 * environment fills every field the config leaves unset (see {@link tracingConfigFromEnv}); pass
 * `{ env: false }` to ignore it. With `remoteSampler` the Jaeger remote strategy decides sampling.
 */
export function initTracing(
  config: JaegerTracingConfig,
  options: JaegerInitTracingOptions = {},
): JaegerTracingHandle {
  const { env, ...initOptions } = options;
  const effective =
    env === false ? { ...config } : tracingConfigFromEnv(config, env ?? process.env);
  const protocol = effective.protocol ?? JAEGER_DEFAULT_PROTOCOL;
  const endpoint = resolveJaegerEndpoint(protocol, effective.endpoint);
  const { remoteSampler: remote, ...tracingConfig } = effective;
  const handle = initTracingWith(
    { ...tracingConfig, protocol, endpoint },
    jaegerExporterFactory,
    initOptions,
  );
  if (remote === undefined) return { ...handle, endpoint };

  let sampler: JaegerRemoteSampler;
  let owned = false;
  let parentBased = true;
  try {
    if (remote instanceof JaegerRemoteSampler) {
      sampler = remote;
    } else {
      const opts = remote === true ? {} : remote;
      const { parentBased: pb, ...samplerOpts } = opts;
      parentBased = pb !== false;
      sampler = new JaegerRemoteSampler({
        ...samplerOpts,
        serviceName: samplerOpts.serviceName ?? handle.config.serviceName,
      });
      owned = true;
    }
    installSampler(handle, parentBased ? new ParentBasedSampler({ root: sampler }) : sampler);
  } catch (err) {
    void handle.shutdown();
    throw err;
  }
  let closed: Promise<void> | null = null;
  return {
    ...handle,
    endpoint,
    remoteSampler: sampler,
    shutdown(): Promise<void> {
      closed ??= (async () => {
        try {
          await handle.shutdown();
        } finally {
          if (owned) await sampler.close();
        }
      })();
      return closed;
    },
  };
}
