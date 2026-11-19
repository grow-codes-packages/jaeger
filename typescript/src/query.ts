// Service-owned: JaegerQueryClient, the Jaeger implementation of the SPEC §5 TraceQueryClient.

import {
  DEFAULT_REQUEST_ID_HEADERS,
  HttpTransport,
  NotFoundError,
  type QueryValue,
  type TransportOptions,
  UnsupportedError,
  ValidationError,
} from "./core/index.js";
import { JaegerConfig, type JaegerConfigOptions } from "./config.js";
import {
  dependenciesFromBody,
  legacyToTraces,
  otlpToTraces,
  summariesFromBody,
} from "./normalize.js";
import {
  BaseTraceQueryClient,
  normalizeTraceId,
  type DependencyLink,
  type DependencyOptions,
  type SpanKindName,
  type Trace,
  type TraceSearchQuery,
  type TraceSummary,
} from "./tracing/index.js";

/** Per-call options of the query client. */
export interface QueryCallOptions {
  /** Cancels the call (`CancelledError`). */
  signal?: AbortSignal;
}

/** Options of {@link JaegerQueryClient.getTrace}. */
export interface GetTraceOptions extends QueryCallOptions {
  /** Start of the lookup window (a storage hint). Default: 7 days before `end`. */
  start?: Date;
  /** End of the lookup window. Default: now. */
  end?: Date;
  /** Skip Jaeger's span adjusters (`rawTraces=true`). Default: server default (false). */
  rawTraces?: boolean;
}

/** Search filter of {@link JaegerQueryClient.findTraces} and `searchTraces`. */
export interface JaegerTraceQuery extends TraceSearchQuery {
  /** Skip Jaeger's span adjusters (`query.rawTraces=true`). */
  rawTraces?: boolean;
}

/** Span kinds accepted by {@link JaegerQueryClient.getOperations}. */
export type OperationSpanKind = Exclude<SpanKindName, "unspecified">;

/** Options of {@link JaegerQueryClient.listOperations} / {@link JaegerQueryClient.getOperations}. */
export interface ListOperationsOptions extends QueryCallOptions {
  /** Only operations with this span kind (case-insensitive; sent lower-case). */
  spanKind?: OperationSpanKind;
}

/** One operation of a service as reported by `/api/v3/operations`. */
export interface JaegerOperation {
  name: string;
  /** `server`, `client`, `producer`, `consumer`, `internal` or `""`. */
  spanKind: string;
}

/**
 * Options of {@link JaegerQueryClient.getDependencies}: either the category window
 * (`start`/`end`) or Jaeger's native `endTs` + `lookbackMs` (these win when given).
 */
export interface JaegerDependencyOptions extends DependencyOptions, QueryCallOptions {
  /** End of the window (Jaeger `endTs`). Default: `end`, else now. */
  endTs?: Date;
  /** Window length in ms (Jaeger `lookback`). Default: `endTs - start`, else 24 h. */
  lookbackMs?: number;
  /** Keep only the edges that touch this service. */
  service?: string;
}

/** Test seams (undici `MockAgent`, fake sleep, clock). Not part of the stable API. */
export type QueryInternals = Pick<TransportOptions, "dispatcher" | "sleep" | "now">;

/** Anything that carries a configured Jaeger transport (a {@link JaegerClient}). */
export interface TransportSource {
  readonly transport: HttpTransport;
}

const HOUR_MS = 3_600_000;
const DAY_MS = 24 * HOUR_MS;
/** Default `getTrace` lookup window. */
export const DEFAULT_GET_TRACE_WINDOW_MS = 7 * DAY_MS;
/** Default search window (`end - start`) when the query sets none. */
export const DEFAULT_SEARCH_WINDOW_MS = HOUR_MS;
/** Default dependency lookback. */
export const DEFAULT_DEPENDENCY_LOOKBACK_MS = DAY_MS;
/** Default `limit` of searches (SPEC §5). */
export const DEFAULT_SEARCH_LIMIT = 20;

const SPAN_KINDS: ReadonlySet<string> = new Set([
  "server",
  "client",
  "producer",
  "consumer",
  "internal",
]);

/**
 * True when a 404 means "this route does not exist" (Go's plain-text `404 page not found`, or a
 * proxy body that is not Jaeger's JSON envelope) rather than "no such trace".
 */
export function isRouteMissing(err: unknown): boolean {
  return (
    err instanceof NotFoundError &&
    /: (?:404 page not found|not found) \(HTTP 404\)$/i.test(err.message)
  );
}

/**
 * Builds the HTTP transport for a Jaeger config (bearer token, `traceresponse` request id).
 * With `auth: false` the token is never sent (used by `health()`, which is unauthenticated).
 */
export function createJaegerTransport(
  config: JaegerConfig,
  internals: QueryInternals = {},
  auth = true,
): HttpTransport {
  return new HttpTransport({
    ...internals,
    config,
    defaultHeaders: (): Record<string, string> =>
      !auth || config.token === undefined
        ? {}
        : { authorization: `Bearer ${config.token.reveal()}` },
    knownSecrets: () => config.knownSecrets(),
    requestIdHeaders: [...DEFAULT_REQUEST_ID_HEADERS, "traceresponse"],
  });
}

function isTransportSource(value: unknown): value is TransportSource {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { transport?: unknown }).transport instanceof HttpTransport
  );
}

function rfc3339(date: Date, what: string): string {
  if (Number.isNaN(date.getTime()))
    throw new ValidationError(`jaeger: ${what} is not a valid date`);
  return date.toISOString();
}

function durationParam(micros: number | undefined, what: string): string | undefined {
  if (micros === undefined) return undefined;
  if (!Number.isFinite(micros) || micros < 0)
    throw new ValidationError(`jaeger.searchTraces: ${what} must be a non-negative number`);
  return `${Math.round(micros)}us`;
}

/**
 * Reads traces back from Jaeger's query service (port 16686): the v3 HTTP API (`/api/v3/*`) for
 * traces, search, services and operations, the legacy `/api/traces/{id}` as a fallback for
 * `getTrace`, and the legacy `/api/dependencies`. Every call is a GET (retried on 5xx).
 *
 * Get one from {@link JaegerClient.query} (shares the client's connections) or construct it from
 * a config (owns its own connections; call {@link close}).
 */
export class JaegerQueryClient extends BaseTraceQueryClient {
  readonly #transport: HttpTransport;
  readonly #owned: boolean;
  readonly #now: () => number;

  /**
   * @param source a {@link JaegerClient} (shared transport) or config options (own transport).
   * @param internals test seams.
   */
  constructor(source: TransportSource | JaegerConfigOptions = {}, internals: QueryInternals = {}) {
    super();
    this.#now = internals.now ?? Date.now;
    if (isTransportSource(source)) {
      this.#transport = source.transport;
      this.#owned = false;
    } else {
      this.#transport = createJaegerTransport(JaegerConfig.from(source), internals);
      this.#owned = true;
    }
  }

  /** Builds a standalone query client from `GROW_JAEGER_*` / `JAEGER_QUERY_URL`. */
  static fromEnv(overrides: JaegerConfigOptions = {}): JaegerQueryClient {
    return new JaegerQueryClient(JaegerConfig.fromEnv(overrides));
  }

  async #get<T>(
    path: string,
    operation: string,
    query: Record<string, QueryValue>,
    signal: AbortSignal | undefined,
  ): Promise<T | undefined> {
    try {
      const res = await this.#transport.get<T | undefined>(path, {
        operation,
        query,
        ...(signal === undefined ? {} : { signal }),
      });
      return res.data;
    } catch (err) {
      if (isRouteMissing(err)) {
        throw new UnsupportedError(`${operation}: route ${path} is not available on this server`, {
          cause: err,
          httpStatus: 404,
        });
      }
      throw err;
    }
  }

  /**
   * Fetches one trace: `GET /api/v3/traces/{id}`, falling back to the legacy
   * `GET /api/traces/{id}` when the v3 route is missing. Returns `null` when the trace does not
   * exist. The id may be 32 or 16 hex characters (left-padded); anything else throws
   * `ValidationError` without a request.
   */
  async getTrace(traceId: string, opts: GetTraceOptions = {}): Promise<Trace | null> {
    const id = normalizeTraceId(traceId);
    const end = opts.end ?? new Date(this.#now());
    const start = opts.start ?? new Date(end.getTime() - DEFAULT_GET_TRACE_WINDOW_MS);
    const startText = rfc3339(start, "start");
    const endText = rfc3339(end, "end");
    if (start.getTime() > end.getTime())
      throw new ValidationError("jaeger.getTrace: start must not be after end");
    const operation = `jaeger.getTrace ${id}`;
    try {
      const data = await this.#get<{ result?: unknown }>(
        `/api/v3/traces/${id}`,
        operation,
        { startTime: startText, endTime: endText, rawTraces: opts.rawTraces },
        opts.signal,
      );
      return pick(otlpToTraces(data?.result), id);
    } catch (err) {
      if (err instanceof UnsupportedError && isRouteMissing(err.cause)) {
        return this.#getLegacyTrace(id, start, end, opts);
      }
      if (err instanceof NotFoundError) return null;
      throw err;
    }
  }

  async #getLegacyTrace(
    id: string,
    start: Date,
    end: Date,
    opts: GetTraceOptions,
  ): Promise<Trace | null> {
    try {
      const data = await this.#get<unknown>(
        `/api/traces/${id}`,
        `jaeger.getTrace ${id} (legacy)`,
        { start: start.getTime() * 1000, end: end.getTime() * 1000, raw: opts.rawTraces },
        opts.signal,
      );
      return pick(legacyToTraces(data), id);
    } catch (err) {
      if (err instanceof NotFoundError) return null;
      throw err;
    }
  }

  /** Like {@link getTrace} but throws `NotFoundError` when the trace does not exist. */
  async requireTrace(traceId: string, opts: GetTraceOptions = {}): Promise<Trace> {
    const trace = await this.getTrace(traceId, opts);
    if (trace === null)
      throw new NotFoundError(`jaeger.getTrace ${normalizeTraceId(traceId)}: trace not found`);
    return trace;
  }

  #searchParams(query: JaegerTraceQuery, operation: string): Record<string, QueryValue> {
    const end = query.end ?? new Date(this.#now());
    const start = query.start ?? new Date(end.getTime() - DEFAULT_SEARCH_WINDOW_MS);
    const startText = rfc3339(start, "start");
    const endText = rfc3339(end, "end");
    if (start.getTime() > end.getTime())
      throw new ValidationError(`${operation}: start must not be after end`);
    const limit = query.limit ?? DEFAULT_SEARCH_LIMIT;
    if (!Number.isInteger(limit) || limit <= 0)
      throw new ValidationError(`${operation}: limit must be a positive integer`);
    const tags = query.tags ?? {};
    return {
      "query.serviceName": query.service === "" ? undefined : query.service,
      "query.operationName": query.operation === "" ? undefined : query.operation,
      "query.attributes": Object.keys(tags).length === 0 ? undefined : JSON.stringify(tags),
      "query.durationMin": durationParam(query.minDurationMicros, "minDurationMicros"),
      "query.durationMax": durationParam(query.maxDurationMicros, "maxDurationMicros"),
      "query.startTimeMin": startText,
      "query.startTimeMax": endText,
      "query.searchDepth": limit,
      "query.rawTraces": query.rawTraces,
    };
  }

  /**
   * Searches full traces: `GET /api/v3/traces`. Both time bounds are always sent (default: the
   * last hour); `limit` (default 20) caps the number of traces. No match returns `[]`.
   */
  async findTraces(query: JaegerTraceQuery = {}, opts: QueryCallOptions = {}): Promise<Trace[]> {
    const operation = "jaeger.findTraces";
    const params = this.#searchParams(query, operation);
    try {
      const data = await this.#get<{ result?: unknown }>(
        "/api/v3/traces",
        operation,
        params,
        opts.signal,
      );
      return otlpToTraces(data?.result);
    } catch (err) {
      if (err instanceof NotFoundError) return [];
      throw err;
    }
  }

  /**
   * Searches trace summaries: `GET /api/v3/trace-summaries` (same filter as {@link findTraces}).
   * Returns one {@link TraceSummary} per matching trace; Jaeger's JSON 404 envelope ("no
   * traces found") returns `[]`.
   */
  async searchTraces(
    query: JaegerTraceQuery = {},
    opts: QueryCallOptions = {},
  ): Promise<TraceSummary[]> {
    const operation = "jaeger.searchTraces";
    const params = this.#searchParams(query, operation);
    try {
      const data = await this.#get<unknown>(
        "/api/v3/trace-summaries",
        operation,
        params,
        opts.signal,
      );
      return summariesFromBody(data);
    } catch (err) {
      if (err instanceof NotFoundError) return [];
      throw err;
    }
  }

  /** Service names that reported spans: `GET /api/v3/services` (sorted). */
  async listServices(opts: QueryCallOptions = {}): Promise<string[]> {
    const data = await this.#get<{ services?: unknown }>(
      "/api/v3/services",
      "jaeger.listServices",
      {},
      opts.signal,
    );
    const services = Array.isArray(data?.services) ? data.services : [];
    return services.filter((s): s is string => typeof s === "string").sort();
  }

  /**
   * Operations of a service with their span kind: `GET /api/v3/operations` (the `spanKind`
   * filter is sent lower-case). An empty service throws `ValidationError` without a request.
   */
  async getOperations(
    service: string,
    opts: ListOperationsOptions = {},
  ): Promise<JaegerOperation[]> {
    if (service.trim() === "")
      throw new ValidationError("jaeger.listOperations: service is required");
    const spanKind = opts.spanKind?.toLowerCase();
    if (spanKind !== undefined && !SPAN_KINDS.has(spanKind))
      throw new ValidationError("jaeger.listOperations: unknown spanKind");
    const data = await this.#get<{ operations?: unknown }>(
      "/api/v3/operations",
      "jaeger.listOperations",
      { service, spanKind },
      opts.signal,
    );
    const operations = Array.isArray(data?.operations) ? (data.operations as unknown[]) : [];
    return operations
      .filter(
        (o): o is { name: string; spanKind?: unknown } =>
          typeof o === "object" && o !== null && typeof (o as { name?: unknown }).name === "string",
      )
      .map((o) => ({ name: o.name, spanKind: typeof o.spanKind === "string" ? o.spanKind : "" }));
  }

  /** Operation names of a service, de-duplicated across span kinds and sorted. */
  async listOperations(service: string, opts: ListOperationsOptions = {}): Promise<string[]> {
    const operations = await this.getOperations(service, opts);
    return [...new Set(operations.map((o) => o.name))].sort();
  }

  /**
   * Service dependency edges: legacy `GET /api/dependencies?endTs=&lookback=`. The memory store
   * derives them on the fly; Cassandra/Elasticsearch/OpenSearch need the `spark-dependencies`
   * job, so `[]` is normal there.
   */
  override async getDependencies(opts: JaegerDependencyOptions = {}): Promise<DependencyLink[]> {
    const operation = "jaeger.getDependencies";
    const endTs = opts.endTs ?? opts.end ?? new Date(this.#now());
    rfc3339(endTs, "endTs");
    const lookbackMs =
      opts.lookbackMs ??
      (opts.start === undefined
        ? DEFAULT_DEPENDENCY_LOOKBACK_MS
        : endTs.getTime() - opts.start.getTime());
    if (!Number.isFinite(lookbackMs) || lookbackMs <= 0)
      throw new ValidationError(`${operation}: lookback must be a positive duration`);
    const data = await this.#get<unknown>(
      "/api/dependencies",
      operation,
      {
        endTs: endTs.getTime(),
        lookback: Math.round(lookbackMs),
        service: opts.service === "" ? undefined : opts.service,
      },
      opts.signal,
    );
    return dependenciesFromBody(data);
  }

  /** Releases connections when this client owns them (no-op for `client.query`). Idempotent. */
  async close(): Promise<void> {
    if (this.#owned) await this.#transport.close();
  }
}

function pick(traces: Trace[], id: string): Trace | null {
  const trace = traces.find((t) => t.traceId === id) ?? traces[0];
  return trace === undefined || trace.spans.length === 0 ? null : trace;
}
