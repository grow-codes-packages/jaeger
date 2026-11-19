import { afterEach, describe, expect, it } from "vitest";
import {
  JaegerClient,
  JaegerQueryClient,
  NotFoundError,
  PermissionDeniedError,
  UnsupportedError,
  ValidationError,
  isRouteMissing,
} from "../../src/index.js";
import { fixtureResponse } from "../helpers/fixtures.js";
import { jaegerServer, params, type Route } from "../helpers/jaeger.js";
import type { MockServer } from "../helpers/server.js";

const TID = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";
const NOW = Date.parse("2026-11-18T21:00:00.000Z");
const fast = { retry: { maxRetries: 1, initialBackoffMs: 1 } };

const servers: MockServer[] = [];
const clients: JaegerQueryClient[] = [];
afterEach(async () => {
  await Promise.all(clients.splice(0).map((c) => c.close()));
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function setup(
  routes: Record<string, Route>,
): Promise<{ s: MockServer; q: JaegerQueryClient }> {
  const s = await jaegerServer(routes);
  servers.push(s);
  const q = new JaegerQueryClient({ url: s.url, ...fast }, { now: () => NOW });
  clients.push(q);
  return { s, q };
}

describe("getTrace", () => {
  it("reads /api/v3/traces/{id} with the default 7-day window", async () => {
    const { s, q } = await setup({ [`/api/v3/traces/${TID}`]: "getTrace.v3_ok" });
    const trace = await q.getTrace(TID.toUpperCase());
    expect(trace?.traceId).toBe(TID);
    expect(trace?.spans).toHaveLength(3);
    const p = params(s.requests[0]);
    expect(p.get("endTime")).toBe("2026-11-18T21:00:00.000Z");
    expect(p.get("startTime")).toBe("2026-11-11T21:00:00.000Z");
    expect(p.has("rawTraces")).toBe(false);
  });

  it("left-pads 16-hex ids and sends an explicit window and rawTraces", async () => {
    const { s, q } = await setup({
      "/api/v3/traces/0000000000000000b94006dad19af3af": "getTrace.v3_short_id",
    });
    const start = new Date("2026-11-18T20:00:00Z");
    const end = new Date("2026-11-18T20:30:00Z");
    const trace = await q.getTrace("b94006dad19af3af", { start, end, rawTraces: true });
    expect(trace?.traceId).toBe("0000000000000000b94006dad19af3af");
    const p = params(s.requests[0]);
    expect(p.get("startTime")).toBe(start.toISOString());
    expect(p.get("endTime")).toBe(end.toISOString());
    expect(p.get("rawTraces")).toBe("true");
  });

  it("returns null for the v3 not-found envelope (no legacy call)", async () => {
    const { s, q } = await setup({
      "/api/v3/traces/ffffffffffffffffffffffffffffffff": "getTrace.v3_not_found",
    });
    expect(await q.getTrace("ffffffffffffffffffffffffffffffff")).toBeNull();
    expect(s.requests).toHaveLength(1);
    await expect(q.requireTrace("ffffffffffffffffffffffffffffffff")).rejects.toBeInstanceOf(
      NotFoundError,
    );
  });

  it("returns null for an empty result", async () => {
    const { q } = await setup({ [`/api/v3/traces/${TID}`]: { status: 200, body: { result: {} } } });
    expect(await q.getTrace(TID)).toBeNull();
  });

  it("falls back to /api/traces/{id} when the v3 route is missing", async () => {
    const { s, q } = await setup({ [`/api/traces/${TID}`]: "getTraceLegacy.ok" });
    const trace = await q.requireTrace(TID, {
      start: new Date(NOW - 1000),
      end: new Date(NOW),
      rawTraces: false,
    });
    expect(trace.spans).toHaveLength(3);
    expect(s.requests.map((r) => new URL(r.url, "http://x").pathname)).toEqual([
      `/api/v3/traces/${TID}`,
      `/api/traces/${TID}`,
    ]);
    const p = params(s.requests[1]);
    expect(p.get("start")).toBe(String((NOW - 1000) * 1000));
    expect(p.get("end")).toBe(String(NOW * 1000));
    expect(p.get("raw")).toBe("false");
  });

  it("falls back on a proxy HTML 404 and maps the legacy not-found to null", async () => {
    const { q } = await setup({
      "/api/v3/traces/ffffffffffffffffffffffffffffffff": {
        status: 404,
        headers: { "content-type": "text/html" },
        body: "<html>nope</html>",
      },
      "/api/traces/ffffffffffffffffffffffffffffffff": "getTraceLegacy.not_found",
    });
    expect(await q.getTrace("ffffffffffffffffffffffffffffffff")).toBeNull();
  });

  it("raises UnsupportedError when neither route exists", async () => {
    const { q } = await setup({});
    const err = await q.getTrace(TID).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(UnsupportedError);
    expect((err as UnsupportedError).httpStatus).toBe(404);
  });

  it("rejects invalid ids and windows without a request", async () => {
    const { s, q } = await setup({});
    await expect(q.getTrace("zz")).rejects.toBeInstanceOf(ValidationError);
    await expect(q.getTrace("0".repeat(32))).rejects.toBeInstanceOf(ValidationError);
    await expect(
      q.getTrace(TID, { start: new Date(NOW), end: new Date(NOW - 1) }),
    ).rejects.toBeInstanceOf(ValidationError);
    await expect(q.getTrace(TID, { start: new Date("nope") })).rejects.toBeInstanceOf(
      ValidationError,
    );
    expect(s.requests).toHaveLength(0);
  });

  it("maps 400, 403 and 501 to SPEC errors with traceresponse as request id", async () => {
    const { q } = await setup({
      [`/api/v3/traces/${TID}`]: "getTrace.v3_bad_time",
      "/api/v3/traces/00000000000000000000000000000001": {
        status: 403,
        headers: { "content-type": "text/plain", traceresponse: "00-aa-bb-01" },
        body: JSON.stringify({ error: { httpCode: 403, message: "access denied" } }),
      },
      "/api/v3/traces/00000000000000000000000000000002": {
        status: 501,
        body: { error: { httpCode: 501, message: "not implemented by storage" } },
      },
    });
    const bad = await q.getTrace(TID).catch((e: unknown) => e);
    expect(bad).toBeInstanceOf(ValidationError);
    expect((bad as ValidationError).message).toContain("HTTP 400");
    expect((bad as ValidationError).requestId).toMatch(/^00-/);
    const denied = await q.getTrace("1").catch((e: unknown) => e);
    expect(denied).toBeInstanceOf(PermissionDeniedError);
    expect((denied as PermissionDeniedError).requestId).toBe("00-aa-bb-01");
    expect((denied as Error).message).toContain("access denied");
    await expect(q.getTrace("2")).rejects.toBeInstanceOf(UnsupportedError);
  });

  it("isRouteMissing distinguishes route and trace 404s", () => {
    expect(isRouteMissing(new NotFoundError("x: 404 page not found (HTTP 404)"))).toBe(true);
    expect(isRouteMissing(new NotFoundError("x: not found (HTTP 404)"))).toBe(true);
    expect(isRouteMissing(new NotFoundError("x: No traces found (HTTP 404)"))).toBe(false);
    expect(isRouteMissing(new NotFoundError("x: trace not found (HTTP 404)"))).toBe(false);
    expect(isRouteMissing(new Error("x: not found (HTTP 404)"))).toBe(false);
  });
});

describe("findTraces / searchTraces", () => {
  it("findTraces sends every filter in the v3 format and groups traces", async () => {
    const { s, q } = await setup({ "/api/v3/traces": "findTraces.by_service" });
    const start = new Date("2026-11-18T20:38:43.142Z");
    const end = new Date("2026-11-18T20:58:43.142Z");
    const traces = await q.findTraces({
      service: "sdk-wk-86837b62",
      operation: "handle /orders",
      tags: { "order.id": "86837b62" },
      minDurationMicros: 1000,
      maxDurationMicros: 2_500_000.4,
      start,
      end,
      limit: 5,
      rawTraces: true,
    });
    expect(traces.length).toBeGreaterThan(0);
    const p = params(s.requests[0]);
    expect(Object.fromEntries(p)).toEqual({
      "query.serviceName": "sdk-wk-86837b62",
      "query.operationName": "handle /orders",
      "query.attributes": '{"order.id":"86837b62"}',
      "query.durationMin": "1000us",
      "query.durationMax": "2500000us",
      "query.startTimeMin": start.toISOString(),
      "query.startTimeMax": end.toISOString(),
      "query.searchDepth": "5",
      "query.rawTraces": "true",
    });
  });

  it("findTraces defaults to the last hour and limit 20; no match is []", async () => {
    const { s, q } = await setup({ "/api/v3/traces": "findTraces.no_match" });
    expect(await q.findTraces({ service: "", operation: "", tags: {} })).toEqual([]);
    expect(await q.findTraces()).toEqual([]);
    expect(Object.fromEntries(params(s.requests[0]))).toEqual({
      "query.startTimeMin": "2026-11-18T20:00:00.000Z",
      "query.startTimeMax": "2026-11-18T21:00:00.000Z",
      "query.searchDepth": "20",
    });
  });

  it("findTraces maps a 400 and validates input locally", async () => {
    const { s, q } = await setup({ "/api/v3/traces": "findTraces.bad_duration" });
    await expect(q.findTraces({ service: "x" })).rejects.toBeInstanceOf(ValidationError);
    expect(s.requests).toHaveLength(1);
    await expect(q.findTraces({ limit: 0 })).rejects.toBeInstanceOf(ValidationError);
    await expect(q.findTraces({ limit: 1.5 })).rejects.toBeInstanceOf(ValidationError);
    await expect(q.findTraces({ minDurationMicros: -1 })).rejects.toBeInstanceOf(ValidationError);
    await expect(q.findTraces({ maxDurationMicros: Number.NaN })).rejects.toBeInstanceOf(
      ValidationError,
    );
    await expect(
      q.findTraces({ start: new Date(NOW), end: new Date(NOW - 1) }),
    ).rejects.toBeInstanceOf(ValidationError);
    expect(s.requests).toHaveLength(1);
  });

  it("findTraces against a server without the v3 API is UnsupportedError", async () => {
    const { q } = await setup({});
    await expect(q.findTraces()).rejects.toBeInstanceOf(UnsupportedError);
  });

  it("searchTraces uses /api/v3/trace-summaries", async () => {
    const { s, q } = await setup({
      "/api/v3/trace-summaries": (_req, url) =>
        url.searchParams.get("query.serviceName") === "none"
          ? "findTraceSummaries.empty"
          : "findTraceSummaries.ok",
    });
    const hits = await q.searchTraces({ service: "sdk-gw-86837b62" });
    expect(hits).toHaveLength(2);
    expect(hits[0]).toMatchObject({ traceId: TID, rootName: "GET /orders", spanCount: 3 });
    expect(params(s.requests[0]).get("query.searchDepth")).toBe("20");
    expect(await q.searchTraces({ service: "none" })).toEqual([]);
    expect(await q.searchTraces()).toHaveLength(2);
  });

  it("searchTraces maps the JSON 404 envelope to [] and a missing route to UnsupportedError", async () => {
    const { q } = await setup({ "/api/v3/trace-summaries": "findTraces.no_match" });
    expect(await q.searchTraces({ service: "none" })).toEqual([]);
    const { q: bare } = await setup({});
    await expect(bare.searchTraces({ service: "x" })).rejects.toMatchObject({
      code: "UNSUPPORTED",
    });
  });
});

describe("services, operations, dependencies", () => {
  it("listServices returns the sorted list", async () => {
    const { q } = await setup({ "/api/v3/services": "listServices.ok" });
    const services = await q.listServices();
    expect(services).toContain("jaeger");
    expect(services).toEqual([...services].sort());
  });

  it("listServices tolerates a missing list and reports a removed route", async () => {
    const { q } = await setup({ "/api/v3/services": { status: 200, body: {} } });
    expect(await q.listServices()).toEqual([]);
    const { q: old } = await setup({ "/api/services": "listServicesLegacy.removed" });
    await expect(old.listServices()).rejects.toBeInstanceOf(UnsupportedError);
  });

  it("getOperations / listOperations send lower-case spanKind", async () => {
    const { s, q } = await setup({
      "/api/v3/operations": (_req, url) =>
        url.searchParams.get("spanKind") === "server"
          ? "listOperations.span_kind_server"
          : "listOperations.ok",
    });
    expect(await q.getOperations("sdk-wk-86837b62")).toEqual([
      { name: "handle /orders", spanKind: "server" },
      { name: "db.query", spanKind: "internal" },
    ]);
    expect(await q.listOperations("sdk-wk-86837b62")).toEqual(["db.query", "handle /orders"]);
    expect(await q.listOperations("sdk-wk-86837b62", { spanKind: "SERVER" as "server" })).toEqual([
      "handle /orders",
    ]);
    expect(params(s.requests[2]).get("spanKind")).toBe("server");
    expect(params(s.requests[2]).get("service")).toBe("sdk-wk-86837b62");
  });

  it("operations: dedupe, odd entries, local validation", async () => {
    const { s, q } = await setup({
      "/api/v3/operations": {
        status: 200,
        body: {
          operations: [
            { name: "a", spanKind: "server" },
            { name: "a", spanKind: "client" },
            { name: "b" },
            3,
            {},
          ],
        },
      },
    });
    expect(await q.getOperations("svc")).toEqual([
      { name: "a", spanKind: "server" },
      { name: "a", spanKind: "client" },
      { name: "b", spanKind: "" },
    ]);
    expect(await q.listOperations("svc")).toEqual(["a", "b"]);
    await expect(q.listOperations(" ")).rejects.toBeInstanceOf(ValidationError);
    await expect(q.listOperations("svc", { spanKind: "bogus" as "server" })).rejects.toBeInstanceOf(
      ValidationError,
    );
    expect(s.requests).toHaveLength(2);
    const { q: empty } = await setup({ "/api/v3/operations": { status: 200, body: {} } });
    expect(await empty.listOperations("svc")).toEqual([]);
  });

  it("getDependencies: native endTs/lookback, category window and defaults", async () => {
    const { s, q } = await setup({ "/api/dependencies": "getDependencies.ok" });
    const links = await q.getDependencies({
      endTs: new Date(1795034983494),
      lookbackMs: 3_600_000,
      service: "sdk-wk-86837b62",
    });
    expect(links).toContainEqual({
      parent: "sdk-gw-86837b62",
      child: "sdk-wk-86837b62",
      callCount: 1,
    });
    expect(Object.fromEntries(params(s.requests[0]))).toEqual({
      endTs: "1795034983494",
      lookback: "3600000",
      service: "sdk-wk-86837b62",
    });
    await q.getDependencies({ start: new Date(NOW - 60_000), end: new Date(NOW) });
    expect(Object.fromEntries(params(s.requests[1]))).toEqual({
      endTs: String(NOW),
      lookback: "60000",
    });
    await q.getDependencies({ service: "" });
    expect(Object.fromEntries(params(s.requests[2]))).toEqual({
      endTs: String(NOW),
      lookback: "86400000",
    });
    await q.getDependencies();
    await expect(q.getDependencies({ lookbackMs: 0 })).rejects.toBeInstanceOf(ValidationError);
    await expect(
      q.getDependencies({ start: new Date(NOW), end: new Date(NOW - 1) }),
    ).rejects.toBeInstanceOf(ValidationError);
    await expect(q.getDependencies({ endTs: new Date("x") })).rejects.toBeInstanceOf(
      ValidationError,
    );
    expect(s.requests).toHaveLength(4);
  });

  it("getDependencies maps the legacy 400 envelope", async () => {
    const { q } = await setup({ "/api/dependencies": "getDependencies.bad_lookback" });
    const err = await q.getDependencies().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ValidationError);
    expect((err as Error).message).toContain("must be a positive duration");
  });
});

describe("waitForTrace, construction, close", () => {
  it("waitForTrace polls until the trace has enough spans", async () => {
    let calls = 0;
    const { q } = await setup({
      [`/api/v3/traces/${TID}`]: () =>
        ++calls < 3 ? fixtureResponse("getTrace.v3_not_found") : "getTrace.v3_ok",
    });
    const trace = await q.waitForTrace(TID, { intervalMs: 5, timeoutMs: 5000, minSpans: 3 });
    expect(trace.spans).toHaveLength(3);
    expect(calls).toBe(3);
  });

  it("client.query shares the client's transport; standalone owns its own", async () => {
    const s = await jaegerServer({ "/api/v3/services": "listServices.ok" });
    servers.push(s);
    const client = new JaegerClient({ url: s.url, token: "query-token-123" });
    expect(client.query).toBe(client.query);
    expect(await client.query.listServices()).toContain("jaeger");
    expect(s.requests[0]!.headers.authorization).toBe("Bearer query-token-123");
    await client.query.close(); // no-op for the shared transport
    expect(await client.query.listServices()).toContain("jaeger");
    await client.close();
    await expect(client.query.listServices()).rejects.toMatchObject({ code: "CANCELLED" });

    const standalone = new JaegerQueryClient(client.config);
    await standalone.close();
    await standalone.close();
    await expect(standalone.listServices()).rejects.toMatchObject({ code: "CANCELLED" });
  });

  it("fromEnv reads GROW_JAEGER_URL, then JAEGER_QUERY_URL", async () => {
    const s = await jaegerServer({ "/api/v3/services": "listServices.ok" });
    servers.push(s);
    const saved = { grow: process.env.GROW_JAEGER_URL, vendor: process.env.JAEGER_QUERY_URL };
    delete process.env.GROW_JAEGER_URL;
    process.env.JAEGER_QUERY_URL = s.url;
    try {
      const q = JaegerQueryClient.fromEnv();
      clients.push(q);
      expect(await q.listServices()).toContain("jaeger");
      const explicit = JaegerQueryClient.fromEnv({ url: "http://explicit:1" });
      clients.push(explicit);
    } finally {
      if (saved.grow !== undefined) process.env.GROW_JAEGER_URL = saved.grow;
      if (saved.vendor === undefined) delete process.env.JAEGER_QUERY_URL;
      else process.env.JAEGER_QUERY_URL = saved.vendor;
    }
  });

  it("keeps a reverse-proxy base path", async () => {
    const s = await jaegerServer({ "/jaeger/api/v3/services": "listServices.ok" });
    servers.push(s);
    const q = new JaegerQueryClient({ url: `${s.url}/jaeger` });
    clients.push(q);
    expect(await q.listServices()).toContain("jaeger");
  });

  it("honours cancellation", async () => {
    const { q } = await setup({ "/api/v3/services": "listServices.ok" });
    const signal = AbortSignal.abort();
    for (const call of [
      q.listServices({ signal }),
      q.getTrace(TID, { signal }),
      q.findTraces({}, { signal }),
      q.searchTraces({}, { signal }),
      q.listOperations("svc", { signal }),
      q.getDependencies({ signal }),
    ]) {
      await expect(call).rejects.toMatchObject({ code: "CANCELLED" });
    }
  });
});
