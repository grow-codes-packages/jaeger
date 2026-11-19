import { describe, expect, it } from "vitest";
import {
  ValidationError,
  anyValue,
  dependenciesFromBody,
  keyValues,
  legacyToTraces,
  nanosToMicros,
  otlpSpanToSpan,
  otlpToTraces,
  summariesFromBody,
} from "../../src/index.js";
import { fixtureBody } from "../helpers/fixtures.js";

const TID = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";

describe("nanosToMicros / anyValue / keyValues", () => {
  it("converts nanosecond strings with integer arithmetic", () => {
    expect(nanosToMicros("1795034918142512222")).toBe(1795034918142512);
    expect(nanosToMicros(1_500_000)).toBe(1500);
    expect(nanosToMicros("x")).toBe(0);
    expect(nanosToMicros(undefined)).toBe(0);
  });

  it("maps every AnyValue variant", () => {
    expect(anyValue({ stringValue: "s" })).toBe("s");
    expect(anyValue({ boolValue: false })).toBe(false);
    expect(anyValue({ intValue: "42" })).toBe(42);
    expect(anyValue({ intValue: 7 })).toBe(7);
    expect(anyValue({ intValue: "9223372036854775807" })).toBe("9223372036854775807");
    expect(anyValue({ doubleValue: 1.5 })).toBe(1.5);
    expect(anyValue({ doubleValue: "NaN" })).toBe("NaN");
    expect(anyValue({ bytesValue: "AQI=" })).toBe("AQI=");
    expect(
      anyValue({ arrayValue: { values: [{ stringValue: "a" }, {}, { intValue: "1" }] } }),
    ).toEqual(["a", 1]);
    expect(anyValue({ arrayValue: {} })).toEqual([]);
    expect(
      anyValue({ kvlistValue: { values: [{ key: "k", value: { boolValue: true } }] } }),
    ).toEqual({ k: true });
    expect(anyValue({})).toBeUndefined();
    expect(anyValue(null)).toBeUndefined();
    expect(
      keyValues([{ key: "a", value: { stringValue: "b" } }, { key: "x" }, { value: {} }, 3]),
    ).toEqual({
      a: "b",
    });
    expect(keyValues(undefined)).toEqual({});
  });
});

describe("otlpToTraces", () => {
  it("normalizes the recorded v3 trace", () => {
    const body = fixtureBody("getTrace.v3_ok") as { result: unknown };
    const traces = otlpToTraces(body.result);
    expect(traces).toHaveLength(1);
    const trace = traces[0]!;
    expect(trace.traceId).toBe(TID);
    expect(trace.spans).toHaveLength(3);
    const [root, child] = trace.spans;
    expect(root).toMatchObject({
      spanId: "55bc22d6caba571a",
      name: "GET /orders",
      kind: "client",
      serviceName: "sdk-gw-86837b62",
      startMicros: 1795034918142512,
      endMicros: 1795034919342512,
      durationMicros: 1_200_000,
      status: { code: "ok" },
      attributes: { "http.request.method": "GET", "order.id": "86837b62" },
    });
    expect(root!.parentSpanId).toBeUndefined();
    expect(child).toMatchObject({
      parentSpanId: "55bc22d6caba571a",
      kind: "server",
      serviceName: "sdk-wk-86837b62",
      attributes: {
        "array.attr": ["a", "b"],
        "bool.attr": true,
        "double.attr": 1.5,
        "int.attr": 42,
        "str.attr": "v",
      },
      events: [
        { name: "cache.miss", timeMicros: 1795034918342512, attributes: { "cache.key": "k1" } },
      ],
      links: [
        {
          traceId: "85092f96d876e30b507cfd1361b0325e",
          spanId: "00f067aa0ba902b7",
          attributes: { "link.kind": "follows" },
        },
      ],
    });
    expect(child!.status.code).toBe("error");
    expect(child!.status.message).toBe("boom");
    // sorted by start time
    const starts = trace.spans.map((s) => s.startMicros);
    expect([...starts].sort((a, b) => a - b)).toEqual(starts);
  });

  it("groups pooled search results by trace id", () => {
    const body = fixtureBody("findTraces.by_service") as { result: unknown };
    const traces = otlpToTraces(body.result);
    expect(traces.length).toBeGreaterThan(0);
    for (const t of traces) for (const s of t.spans) expect(s.traceId).toBe(t.traceId);
  });

  it("handles string enums, missing fields and garbage", () => {
    expect(otlpToTraces(undefined)).toEqual([]);
    expect(otlpToTraces({ resourceSpans: [{ scopeSpans: [{ spans: [] }] }, 1] })).toEqual([]);
    const span = otlpSpanToSpan(
      {
        traceId: "AB",
        spanId: "01",
        parentSpanId: "0000000000000000",
        kind: "SPAN_KIND_PRODUCER",
        status: { code: "STATUS_CODE_ERROR", message: "" },
      },
      "svc",
    );
    expect(span).toMatchObject({
      traceId: "000000000000000000000000000000ab",
      spanId: "0000000000000001",
      name: "",
      kind: "producer",
      status: { code: "error" },
      durationMicros: 0,
    });
    expect(span.parentSpanId).toBeUndefined();
    expect(
      otlpSpanToSpan(
        { traceId: "1", spanId: "2", kind: 9, status: { code: "STATUS_CODE_OK" } },
        "s",
      ),
    ).toMatchObject({ kind: "unspecified", status: { code: "ok" } });
    expect(otlpSpanToSpan({ traceId: "1", spanId: "2" }, "s").status).toEqual({ code: "unset" });
    expect(() => otlpSpanToSpan({ spanId: "2" }, "s")).toThrow(ValidationError);
    const unknown = otlpToTraces({
      resourceSpans: [
        {
          resource: {},
          scopeSpans: [
            {
              spans: [
                { traceId: "1", spanId: "2" },
                { traceId: "1", spanId: "3" },
              ],
            },
          ],
        },
      ],
    });
    expect(unknown[0]!.spans.map((s) => s.serviceName)).toEqual([
      "unknown_service",
      "unknown_service",
    ]);
  });
});

describe("legacyToTraces", () => {
  it("maps the recorded legacy model to the same normalized trace as v3", () => {
    const legacy = legacyToTraces(fixtureBody("getTraceLegacy.ok"))[0]!;
    const v3 = otlpToTraces((fixtureBody("getTrace.v3_ok") as { result: unknown }).result)[0]!;
    expect(legacy.traceId).toBe(TID);
    expect(
      legacy.spans.map((s) => [s.spanId, s.parentSpanId, s.name, s.kind, s.serviceName]),
    ).toEqual(v3.spans.map((s) => [s.spanId, s.parentSpanId, s.name, s.kind, s.serviceName]));
    const child = legacy.spans.find((s) => s.spanId === "02811a5d0fecd819")!;
    expect(child.status).toEqual({ code: "error", message: "boom" });
    expect(child.attributes).toEqual({
      "array.attr": '["a","b"]',
      "bool.attr": true,
      "double.attr": 1.5,
      "int.attr": 42,
      "str.attr": "v",
    });
    expect(child.events).toEqual([
      { name: "cache.miss", timeMicros: 1795034918342512, attributes: { "cache.key": "k1" } },
    ]);
    expect(child.links).toEqual([
      { traceId: "85092f96d876e30b507cfd1361b0325e", spanId: "00f067aa0ba902b7", attributes: {} },
    ]);
    expect(child.durationMicros).toBe(1_000_000);
    expect(legacy.spans[0]!.status).toEqual({ code: "ok" });
  });

  it("covers fallbacks: FOLLOWS_FROM parent, log without event, odd tags", () => {
    const [trace] = legacyToTraces({
      data: [
        {
          traceID: "abc",
          processes: { p1: { serviceName: "svc" } },
          spans: [
            {
              spanID: "1",
              operationName: "op",
              references: [{ refType: "FOLLOWS_FROM", traceID: "abc", spanID: "9" }],
              startTime: 10,
              duration: 5,
              tags: [
                { key: "span.kind", value: "weird" },
                { key: "error", value: true },
                { key: "bin", type: "binary", value: null },
                { key: "n", value: 3 },
              ],
              logs: [{ timestamp: 11, fields: [{ key: "k", value: "v" }] }, { fields: [] }],
              processID: "p1",
            },
            { spanID: "2", processID: "missing" },
          ],
        },
        "junk",
      ],
    });
    const [a, b] = trace!.spans.sort((x, y) => x.spanId.localeCompare(y.spanId));
    expect(a).toMatchObject({
      traceId: "00000000000000000000000000000abc",
      parentSpanId: "0000000000000009",
      kind: "unspecified",
      serviceName: "svc",
      status: { code: "error" },
      attributes: { n: 3 },
      links: [],
      events: [
        { name: "log", timeMicros: 11, attributes: { k: "v" } },
        { name: "log", timeMicros: 0, attributes: {} },
      ],
    });
    expect(b).toMatchObject({
      serviceName: "unknown_service",
      startMicros: 0,
      durationMicros: 0,
      name: "",
    });
    expect(b!.parentSpanId).toBeUndefined();
    expect(legacyToTraces(null)).toEqual([]);
  });
});

describe("summaries and dependencies", () => {
  it("maps trace-summaries 1:1 onto TraceSummary", () => {
    const summaries = summariesFromBody(fixtureBody("findTraceSummaries.ok"));
    expect(summaries[0]).toEqual({
      traceId: TID,
      rootServiceName: "sdk-gw-86837b62",
      rootName: "GET /orders",
      startMicros: 1795034918142512,
      durationMicros: 1_200_000,
      spanCount: 3,
    });
    expect(summaries[1]!.traceId).toBe("0000000000000000b94006dad19af3af");
    expect(summariesFromBody(fixtureBody("findTraceSummaries.empty"))).toEqual([]);
    expect(summariesFromBody(undefined)).toEqual([]);
    expect(
      summariesFromBody({ summaries: [{ traceId: "1", spanCount: "2", rootServiceName: "" }] }),
    ).toEqual([
      {
        traceId: "00000000000000000000000000000001",
        startMicros: 0,
        durationMicros: 0,
        spanCount: 2,
      },
    ]);
    expect(summariesFromBody({ summaries: [{ traceId: "1" }] })[0]!.spanCount).toBe(0);
  });

  it("maps dependency links", () => {
    expect(dependenciesFromBody(fixtureBody("getDependencies.ok"))).toContainEqual({
      parent: "sdk-gw-86837b62",
      child: "sdk-wk-86837b62",
      callCount: 1,
    });
    expect(dependenciesFromBody(undefined)).toEqual([]);
    expect(
      dependenciesFromBody({
        data: [
          { parent: "a", child: "b", callCount: "3" },
          { parent: "a" },
          { parent: "a", child: "c" },
        ],
      }),
    ).toEqual([
      { parent: "a", child: "b", callCount: 3 },
      { parent: "a", child: "c", callCount: 0 },
    ]);
  });
});
