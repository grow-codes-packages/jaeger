// Service-owned: Jaeger remote sampling (GET {samplingUrl}?service=<name>, port 5778) as an
// OpenTelemetry Sampler.

import type { Attributes, Context, Link, SpanKind } from "@opentelemetry/api";
import {
  SamplingDecision,
  TraceIdRatioBasedSampler,
  type Sampler,
  type SamplingResult,
} from "@opentelemetry/sdk-trace-base";
import { ConfigError, type HttpTransport, ValidationError, redactUrl } from "./core/index.js";
import { JaegerConfig, type JaegerConfigOptions } from "./config.js";
import { createJaegerTransport, type QueryInternals } from "./query.js";

/** A sampling strategy as served by Jaeger, normalized. */
export interface JaegerSamplingStrategy {
  /** Jaeger `strategyType`, lower-case. */
  type: "probabilistic" | "ratelimiting";
  /** Service-level probabilistic rate (`probabilisticSampling.samplingRate`), 0..1. */
  samplingRate?: number;
  /** Service-level rate limit (`rateLimitingSampling.maxTracesPerSecond`), >= 0. */
  maxTracesPerSecond?: number;
  /** `operationSampling.defaultSamplingProbability`, 0..1. */
  defaultSamplingProbability?: number;
  /** Per-operation probabilistic rates (`operationSampling.perOperationStrategies`). */
  operations: { operation: string; samplingRate: number }[];
}

type Obj = Record<string, unknown>;

function obj(value: unknown): Obj | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Obj)
    : undefined;
}

function rate(value: unknown, what: string): number {
  // proto3 JSON omits zero values, so a missing number means 0.
  const n = value === undefined ? 0 : value;
  if (typeof n !== "number" || !Number.isFinite(n) || n < 0 || n > 1)
    throw new ValidationError(`jaeger.sampling: ${what} must be a number between 0 and 1`);
  return n;
}

/**
 * Parses a Jaeger sampling response (`/sampling?service=`) into a {@link JaegerSamplingStrategy}.
 * Missing numbers default to 0 (proto3 JSON). Throws `ValidationError` for a malformed body,
 * a rate outside 0..1 or a negative `maxTracesPerSecond`.
 */
export function parseSamplingStrategy(body: unknown): JaegerSamplingStrategy {
  const root = obj(body);
  if (root === undefined) throw new ValidationError("jaeger.sampling: response is not an object");
  const rawType = root.strategyType ?? "PROBABILISTIC";
  const typeText =
    typeof rawType === "string" || typeof rawType === "number" ? String(rawType).toUpperCase() : "";
  if (
    typeText !== "PROBABILISTIC" &&
    typeText !== "RATE_LIMITING" &&
    typeText !== "0" &&
    typeText !== "1"
  )
    throw new ValidationError("jaeger.sampling: unknown strategyType");
  const out: JaegerSamplingStrategy = {
    type: typeText === "RATE_LIMITING" || typeText === "1" ? "ratelimiting" : "probabilistic",
    operations: [],
  };
  const prob = obj(root.probabilisticSampling);
  if (prob !== undefined) out.samplingRate = rate(prob.samplingRate, "samplingRate");
  const limit = obj(root.rateLimitingSampling);
  if (limit !== undefined) {
    const max = limit.maxTracesPerSecond ?? 0;
    if (typeof max !== "number" || !Number.isFinite(max) || max < 0)
      throw new ValidationError("jaeger.sampling: maxTracesPerSecond must be >= 0");
    out.maxTracesPerSecond = max;
  }
  const ops = obj(root.operationSampling);
  if (ops !== undefined) {
    out.defaultSamplingProbability = rate(
      ops.defaultSamplingProbability,
      "defaultSamplingProbability",
    );
    const list = Array.isArray(ops.perOperationStrategies) ? ops.perOperationStrategies : [];
    for (const entry of list) {
      const e = obj(entry);
      if (e === undefined || typeof e.operation !== "string") continue;
      out.operations.push({
        operation: e.operation,
        samplingRate: rate(obj(e.probabilisticSampling)?.samplingRate, "samplingRate"),
      });
    }
  }
  return out;
}

/** Token-bucket rate limiter (Jaeger semantics: balance starts full, max(1, rate) credits). */
export class RateLimiter {
  readonly creditsPerSecond: number;
  readonly maxBalance: number;
  #balance: number;
  #last: number;
  readonly #now: () => number;

  constructor(creditsPerSecond: number, now: () => number = Date.now) {
    this.creditsPerSecond = creditsPerSecond;
    this.maxBalance = Math.max(1, creditsPerSecond);
    this.#balance = this.maxBalance;
    this.#now = now;
    this.#last = now();
  }

  /** Takes one credit; false when none is available. */
  check(): boolean {
    const now = this.#now();
    const elapsed = Math.max(0, now - this.#last) / 1000;
    this.#last = now;
    this.#balance = Math.min(this.maxBalance, this.#balance + elapsed * this.creditsPerSecond);
    if (this.#balance >= 1) {
      this.#balance -= 1;
      return true;
    }
    return false;
  }
}

/** Options of {@link JaegerRemoteSampler}. */
export interface JaegerRemoteSamplerOptions {
  /** Service whose strategy is fetched (`?service=`). Required. */
  serviceName: string;
  /** Sampling endpoint. Default: `config.samplingUrl` (`http://127.0.0.1:5778/sampling`). */
  samplingUrl?: string;
  /** Poll interval in ms. Default 60000. */
  pollIntervalMs?: number;
  /** Probabilistic rate used until the first strategy arrives. Default 0.001. */
  initialSamplingRate?: number;
  /** Transport settings (timeouts, retry, TLS, proxy, headers, token). */
  config?: JaegerConfigOptions;
  /** Start polling at once (first fetch immediately). Default true. */
  autoStart?: boolean;
  /** Called when a poll fails; the last good strategy stays in use. */
  onError?: (error: unknown) => void;
  /** Called after each successful poll. */
  onUpdate?: (strategy: JaegerSamplingStrategy) => void;
}

/** Default poll interval of {@link JaegerRemoteSampler}. */
export const DEFAULT_SAMPLING_POLL_INTERVAL_MS = 60_000;
/** Default initial sampling rate of {@link JaegerRemoteSampler}. */
export const DEFAULT_INITIAL_SAMPLING_RATE = 0.001;

/**
 * OpenTelemetry `Sampler` driven by Jaeger's remote sampling endpoint. Rule (same in every
 * language): a per-operation strategy wins for a listed operation (span name); every other
 * operation uses the service-level `rateLimitingSampling` when present, else
 * `probabilisticSampling` (else `defaultSamplingProbability`). Probabilistic decisions are
 * deterministic per trace id. Until the first strategy arrives, `initialSamplingRate` applies;
 * a failed or malformed poll keeps the last good strategy.
 *
 * Use it as the root of a `ParentBasedSampler` (what `initTracing({ remoteSampler })` does).
 */
export class JaegerRemoteSampler implements Sampler {
  readonly serviceName: string;
  readonly samplingUrl: string;
  readonly pollIntervalMs: number;
  readonly initialSamplingRate: number;
  readonly #transport: HttpTransport;
  readonly #options: JaegerRemoteSamplerOptions;
  readonly #now: () => number;
  readonly #ratio = new Map<number, TraceIdRatioBasedSampler>();
  #strategy: JaegerSamplingStrategy | null = null;
  #limiter: RateLimiter | null = null;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #running = false;
  #closed = false;
  #first: Promise<void> | null = null;

  constructor(options: JaegerRemoteSamplerOptions, internals: QueryInternals = {}) {
    if (typeof options.serviceName !== "string" || options.serviceName.trim() === "")
      throw new ConfigError("jaeger.remoteSampler: serviceName is required");
    const base = JaegerConfig.from(options.config ?? {});
    const samplingUrl = options.samplingUrl ?? base.samplingUrl;
    const config =
      options.samplingUrl === undefined
        ? base
        : new JaegerConfig({ ...configOptions(base), samplingUrl });
    const pollIntervalMs = options.pollIntervalMs ?? DEFAULT_SAMPLING_POLL_INTERVAL_MS;
    if (!Number.isFinite(pollIntervalMs) || pollIntervalMs <= 0)
      throw new ConfigError("jaeger.remoteSampler: pollIntervalMs must be positive");
    const initial = options.initialSamplingRate ?? DEFAULT_INITIAL_SAMPLING_RATE;
    if (!Number.isFinite(initial) || initial < 0 || initial > 1)
      throw new ConfigError("jaeger.remoteSampler: initialSamplingRate must be between 0 and 1");
    this.serviceName = options.serviceName;
    this.samplingUrl = config.samplingUrl;
    this.pollIntervalMs = pollIntervalMs;
    this.initialSamplingRate = initial;
    this.#options = options;
    this.#now = internals.now ?? Date.now;
    this.#transport = createJaegerTransport(config, internals);
    if (options.autoStart !== false) this.start();
  }

  /** The strategy in use, or `null` before the first successful poll. */
  get strategy(): JaegerSamplingStrategy | null {
    return this.#strategy;
  }

  /**
   * Fetches the strategy once and installs it. Throws on failure (the previous strategy stays
   * in use).
   */
  async refresh(opts: { signal?: AbortSignal } = {}): Promise<JaegerSamplingStrategy> {
    const res = await this.#transport.get(this.samplingUrl, {
      operation: `jaeger.sampling ${this.serviceName}`,
      query: { service: this.serviceName },
      ...(opts.signal === undefined ? {} : { signal: opts.signal }),
    });
    const strategy = parseSamplingStrategy(res.data);
    this.#install(strategy);
    return strategy;
  }

  #install(strategy: JaegerSamplingStrategy): void {
    const previous = this.#strategy?.maxTracesPerSecond;
    this.#strategy = strategy;
    if (strategy.maxTracesPerSecond === undefined) this.#limiter = null;
    else if (this.#limiter === null || previous !== strategy.maxTracesPerSecond)
      this.#limiter = new RateLimiter(strategy.maxTracesPerSecond, this.#now);
  }

  /** Starts polling (first fetch immediately). Idempotent. */
  start(): void {
    if (this.#running || this.#closed) return;
    this.#running = true;
    this.#first = this.#poll();
  }

  /** Resolves after the first poll since {@link start} finished (successfully or not). */
  async ready(): Promise<void> {
    await this.#first;
  }

  async #poll(): Promise<void> {
    try {
      const strategy = await this.refresh();
      safe(() => this.#options.onUpdate?.(strategy));
    } catch (err) {
      safe(() => this.#options.onError?.(err));
    }
    if (!this.#running) return;
    this.#timer = setTimeout(() => void this.#poll(), this.pollIntervalMs);
    this.#timer.unref();
  }

  /** Stops polling and releases connections. Idempotent. */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#running = false;
    clearTimeout(this.#timer);
    await this.#first?.catch(() => undefined);
    clearTimeout(this.#timer);
    await this.#transport.close();
  }

  /** Alias of {@link close} (OpenTelemetry naming). */
  shutdown(): Promise<void> {
    return this.close();
  }

  #probabilistic(r: number): TraceIdRatioBasedSampler {
    let sampler = this.#ratio.get(r);
    if (sampler === undefined) {
      sampler = new TraceIdRatioBasedSampler(r);
      this.#ratio.set(r, sampler);
    }
    return sampler;
  }

  /** The sampling decision for a new span (see the class description for the rule). */
  shouldSample(
    context: Context,
    traceId: string,
    spanName: string,
    _spanKind: SpanKind,
    _attributes: Attributes,
    _links: Link[],
  ): SamplingResult {
    const strategy = this.#strategy;
    if (strategy === null) {
      return this.#probabilistic(this.initialSamplingRate).shouldSample(context, traceId);
    }
    const op = strategy.operations.find((o) => o.operation === spanName);
    if (op !== undefined)
      return this.#probabilistic(op.samplingRate).shouldSample(context, traceId);
    if (this.#limiter !== null) {
      return {
        decision: this.#limiter.check()
          ? SamplingDecision.RECORD_AND_SAMPLED
          : SamplingDecision.NOT_RECORD,
      };
    }
    const r =
      strategy.samplingRate ?? strategy.defaultSamplingProbability ?? this.initialSamplingRate;
    return this.#probabilistic(r).shouldSample(context, traceId);
  }

  toString(): string {
    return `JaegerRemoteSampler{service=${this.serviceName}, url=${redactUrl(this.samplingUrl)}}`;
  }
}

function safe(fn: () => void): void {
  try {
    fn();
  } catch {
    // Callback errors never stop polling.
  }
}

function configOptions(config: JaegerConfig): JaegerConfigOptions {
  const out: JaegerConfigOptions = {
    url: config.url,
    connectTimeoutMs: config.connectTimeoutMs,
    requestTimeoutMs: config.requestTimeoutMs,
    totalTimeoutMs: config.totalTimeoutMs,
    retry: config.retry,
    tls: config.tls,
    headers: config.headers,
  };
  if (config.proxy !== undefined) out.proxy = config.proxy;
  if (config.userAgentSuffix !== undefined) out.userAgentSuffix = config.userAgentSuffix;
  if (config.hooks !== undefined) out.hooks = config.hooks;
  if (config.logger !== undefined) out.logger = config.logger;
  if (config.token !== undefined) out.token = config.token;
  return out;
}
