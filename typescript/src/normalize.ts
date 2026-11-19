// Service-owned: converts Jaeger query responses (OTLP/JSON from /api/v3, the legacy UI model from
// /api/traces/{id}) into the normalized SPEC §5 trace model.

import {
  normalizeSpanId,
  normalizeTraceId,
  sortSpans,
  type DependencyLink,
  type Span,
  type SpanAttrValue,
  type SpanAttributes,
  type SpanKindName,
  type Trace,
  type TraceSummary,
} from "./tracing/index.js";

type Obj = Record<string, unknown>;

function isObj(value: unknown): value is Obj {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function objects(value: unknown): Obj[] {
  return list(value).filter(isObj);
}

/** Text of a JSON scalar (`""` for objects, arrays and missing values). */
function scalarText(value: unknown): string {
  return typeof value === "string" ||
    typeof value === "number" ||
    typeof value === "boolean" ||
    typeof value === "bigint"
    ? String(value)
    : "";
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/**
 * Converts an OTLP nanosecond timestamp (decimal string or number) to epoch microseconds using
 * integer arithmetic. Returns 0 for a missing or malformed value.
 */
export function nanosToMicros(value: unknown): number {
  if (typeof value === "number" && Number.isFinite(value)) return Math.floor(value / 1000);
  if (typeof value === "string" && /^\d+$/.test(value)) return Number(BigInt(value) / 1000n);
  return 0;
}

/** Converts one OTLP/JSON `AnyValue` into an attribute value (`undefined` when empty). */
export function anyValue(value: unknown): SpanAttrValue | undefined {
  if (!isObj(value)) return undefined;
  if (typeof value.stringValue === "string") return value.stringValue;
  if (typeof value.boolValue === "boolean") return value.boolValue;
  if (value.intValue !== undefined) {
    const n = Number(value.intValue);
    if (typeof value.intValue === "number") return value.intValue;
    return Number.isSafeInteger(n) ? n : scalarText(value.intValue);
  }
  if (value.doubleValue !== undefined) {
    const d = Number(value.doubleValue);
    return Number.isFinite(d) ? d : scalarText(value.doubleValue);
  }
  if (typeof value.bytesValue === "string") return value.bytesValue;
  if (isObj(value.arrayValue)) {
    return list(value.arrayValue.values)
      .map(anyValue)
      .filter((v): v is SpanAttrValue => v !== undefined);
  }
  if (isObj(value.kvlistValue)) return keyValues(value.kvlistValue.values);
  return undefined;
}

/** Converts an OTLP/JSON `KeyValue[]` list into an attribute map. */
export function keyValues(values: unknown): SpanAttributes {
  const out: SpanAttributes = {};
  for (const kv of objects(values)) {
    const key = asString(kv.key);
    const value = anyValue(kv.value);
    if (key !== undefined && value !== undefined) out[key] = value;
  }
  return out;
}

const OTLP_KINDS: Record<string, SpanKindName> = {
  "0": "unspecified",
  "1": "internal",
  "2": "server",
  "3": "client",
  "4": "producer",
  "5": "consumer",
  SPAN_KIND_UNSPECIFIED: "unspecified",
  SPAN_KIND_INTERNAL: "internal",
  SPAN_KIND_SERVER: "server",
  SPAN_KIND_CLIENT: "client",
  SPAN_KIND_PRODUCER: "producer",
  SPAN_KIND_CONSUMER: "consumer",
};

function otlpKind(kind: unknown): SpanKindName {
  return OTLP_KINDS[scalarText(kind ?? 0)] ?? "unspecified";
}

function otlpStatus(status: unknown): Span["status"] {
  const s = isObj(status) ? status : {};
  const code = scalarText(s.code ?? 0);
  const name =
    code === "2" || code === "STATUS_CODE_ERROR"
      ? "error"
      : code === "1" || code === "STATUS_CODE_OK"
        ? "ok"
        : "unset";
  const message = asString(s.message);
  return message !== undefined && message !== "" ? { code: name, message } : { code: name };
}

function optionalSpanId(id: unknown): string | undefined {
  return typeof id === "string" && id !== "" && !/^0+$/.test(id) ? normalizeSpanId(id) : undefined;
}

/** Converts one OTLP/JSON span (with its resource's service name) into the query model. */
export function otlpSpanToSpan(span: Obj, serviceName: string): Span {
  const startMicros = nanosToMicros(span.startTimeUnixNano);
  const endMicros = nanosToMicros(span.endTimeUnixNano);
  const out: Span = {
    traceId: normalizeTraceId(asString(span.traceId) ?? ""),
    spanId: normalizeSpanId(asString(span.spanId) ?? ""),
    name: asString(span.name) ?? "",
    kind: otlpKind(span.kind),
    serviceName,
    startMicros,
    endMicros,
    durationMicros: Math.max(0, endMicros - startMicros),
    status: otlpStatus(span.status),
    attributes: keyValues(span.attributes),
    events: objects(span.events).map((e) => ({
      name: asString(e.name) ?? "",
      timeMicros: nanosToMicros(e.timeUnixNano),
      attributes: keyValues(e.attributes),
    })),
    links: objects(span.links).map((l) => ({
      traceId: normalizeTraceId(asString(l.traceId) ?? ""),
      spanId: normalizeSpanId(asString(l.spanId) ?? ""),
      attributes: keyValues(l.attributes),
    })),
  };
  const parent = optionalSpanId(span.parentSpanId);
  if (parent !== undefined) out.parentSpanId = parent;
  return out;
}

/**
 * Converts an OTLP/JSON `TracesData` (`{resourceSpans: [...]}`) into normalized traces, grouped
 * by trace id in order of first appearance, spans sorted per SPEC §5.
 */
export function otlpToTraces(data: unknown): Trace[] {
  const byId = new Map<string, Span[]>();
  const root = isObj(data) ? data : {};
  for (const rs of objects(root.resourceSpans)) {
    const resource = isObj(rs.resource) ? keyValues(rs.resource.attributes) : {};
    const name = resource["service.name"];
    const serviceName = typeof name === "string" && name !== "" ? name : "unknown_service";
    for (const ss of objects(rs.scopeSpans)) {
      for (const raw of objects(ss.spans)) {
        const span = otlpSpanToSpan(raw, serviceName);
        const spans = byId.get(span.traceId);
        if (spans === undefined) byId.set(span.traceId, [span]);
        else spans.push(span);
      }
    }
  }
  return [...byId].map(([traceId, spans]) => ({ traceId, spans: sortSpans(spans) }));
}

// ---------------------------------------------------------------------------------------------
// Legacy UI model (/api/traces/{id})

/** Tags Jaeger synthesizes from OTLP fields; they are mapped back and dropped from attributes. */
const SYNTHETIC_TAGS: ReadonlySet<string> = new Set([
  "span.kind",
  "otel.status_code",
  "otel.status_description",
  "error",
  "otel.scope.name",
  "otel.scope.version",
]);

const LEGACY_KINDS: ReadonlySet<string> = new Set([
  "internal",
  "server",
  "client",
  "producer",
  "consumer",
]);

function legacyValue(tag: Obj): SpanAttrValue | undefined {
  const value = tag.value;
  if (typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") return value;
  return undefined;
}

function legacyTags(tags: unknown, skip: ReadonlySet<string> = new Set()): SpanAttributes {
  const out: SpanAttributes = {};
  for (const tag of objects(tags)) {
    const key = asString(tag.key);
    const value = legacyValue(tag);
    if (key !== undefined && value !== undefined && !skip.has(key)) out[key] = value;
  }
  return out;
}

function legacySpan(span: Obj, processes: Obj, traceId: string): Span {
  const tags = legacyTags(span.tags);
  const startMicros = typeof span.startTime === "number" ? Math.trunc(span.startTime) : 0;
  const durationMicros = typeof span.duration === "number" ? Math.trunc(span.duration) : 0;
  const process = isObj(processes[asString(span.processID) ?? ""])
    ? (processes[asString(span.processID) ?? ""] as Obj)
    : {};
  const kindTag = typeof tags["span.kind"] === "string" ? tags["span.kind"] : "";
  const statusTag = typeof tags["otel.status_code"] === "string" ? tags["otel.status_code"] : "";
  const description = tags["otel.status_description"];
  const isError = statusTag.toUpperCase() === "ERROR" || tags.error === true;
  const status: Span["status"] = {
    code: isError ? "error" : statusTag.toUpperCase() === "OK" ? "ok" : "unset",
  };
  if (isError && typeof description === "string" && description !== "")
    status.message = description;

  const refs = objects(span.references);
  const sameTrace = refs.filter(
    (r) => typeof r.traceID === "string" && normalizeTraceId(r.traceID) === traceId,
  );
  const parentRef = sameTrace.find((r) => r.refType === "CHILD_OF") ?? sameTrace[0];
  const out: Span = {
    traceId,
    spanId: normalizeSpanId(asString(span.spanID) ?? ""),
    name: asString(span.operationName) ?? "",
    kind: LEGACY_KINDS.has(kindTag) ? (kindTag as SpanKindName) : "unspecified",
    serviceName: asString(process.serviceName) ?? "unknown_service",
    startMicros,
    endMicros: startMicros + durationMicros,
    durationMicros,
    status,
    attributes: legacyTags(span.tags, SYNTHETIC_TAGS),
    events: objects(span.logs).map((log) => {
      const fields = legacyTags(log.fields);
      const name = typeof fields.event === "string" ? fields.event : "log";
      return {
        name,
        timeMicros: typeof log.timestamp === "number" ? Math.trunc(log.timestamp) : 0,
        attributes: legacyTags(log.fields, new Set(["event"])),
      };
    }),
    links: refs
      .filter((r) => r !== parentRef && typeof r.traceID === "string")
      .map((r) => ({
        traceId: normalizeTraceId(r.traceID as string),
        spanId: normalizeSpanId(asString(r.spanID) ?? ""),
        attributes: {},
      })),
  };
  if (parentRef !== undefined) {
    const parent = optionalSpanId(parentRef.spanID);
    if (parent !== undefined) out.parentSpanId = parent;
  }
  return out;
}

/** Converts the legacy `/api/traces/{id}` body (`{data: [{traceID, spans, processes}]}`). */
export function legacyToTraces(body: unknown): Trace[] {
  const root = isObj(body) ? body : {};
  return objects(root.data).map((t) => {
    const traceId = normalizeTraceId(asString(t.traceID) ?? "");
    const processes = isObj(t.processes) ? t.processes : {};
    return {
      traceId,
      spans: sortSpans(objects(t.spans).map((s) => legacySpan(s, processes, traceId))),
    };
  });
}

// ---------------------------------------------------------------------------------------------
// Summaries, dependencies

/** Converts `/api/v3/trace-summaries` entries into SPEC `TraceSummary` values. */
export function summariesFromBody(body: unknown): TraceSummary[] {
  const root = isObj(body) ? body : {};
  return objects(root.summaries).map((s) => {
    const start = nanosToMicros(s.minStartTimeUnixNano);
    const end = nanosToMicros(s.maxEndTimeUnixNano);
    const summary: TraceSummary = {
      traceId: normalizeTraceId(asString(s.traceId) ?? ""),
      startMicros: start,
      durationMicros: Math.max(0, end - start),
      spanCount: typeof s.spanCount === "number" ? s.spanCount : Number(s.spanCount ?? 0),
    };
    const rootService = asString(s.rootServiceName);
    if (rootService !== undefined && rootService !== "") summary.rootServiceName = rootService;
    const rootName = asString(s.rootOperationName);
    if (rootName !== undefined && rootName !== "") summary.rootName = rootName;
    return summary;
  });
}

/** Converts the legacy `/api/dependencies` body into dependency links. */
export function dependenciesFromBody(body: unknown): DependencyLink[] {
  const root = isObj(body) ? body : {};
  return objects(root.data)
    .filter((d) => typeof d.parent === "string" && typeof d.child === "string")
    .map((d) => ({
      parent: d.parent as string,
      child: d.child as string,
      callCount: typeof d.callCount === "number" ? d.callCount : Number(d.callCount ?? 0),
    }));
}
