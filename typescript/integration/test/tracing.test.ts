// Integration tests for the tracing setup, the query client and remote sampling against the real
// Jaeger 2.23.0 all-in-one (docker/compose.yml). Every write is read back through the query API.

import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { after, before, describe, test } from "node:test";
import { ROOT_CONTEXT, SpanKind, SpanStatusCode, context, trace } from "@opentelemetry/api";
import {
  JaegerClient,
  JaegerQueryClient,
  JaegerRemoteSampler,
  NotFoundError,
  ValidationError,
  addEvent,
  buildTree,
  currentSpanId,
  currentTraceId,
  extract,
  initTracing,
  inject,
  normalizeTraceId,
  parseSamplingStrategy,
  recordException,
  setError,
  traceFields,
  traced,
  tracingConfigFromEnv,
  withSpan,
  type JaegerTracingHandle,
  type Trace,
} from "@grow-codes/jaeger";
import { loadBootstrapEnv, runId } from "./env.ts";

const tag = `${runId}-${randomBytes(3).toString("hex")}`.toLowerCase().replace(/[^a-z0-9-]/g, "");
const gateway = `sdk-ts-gw-${tag}`;
const worker = `sdk-ts-wk-${tag}`;
const grpcService = `sdk-ts-grpc-${tag}`;
const helperService = `sdk-ts-helpers-${tag}`;
const startedAt = new Date(Date.now() - 60_000);

let client: JaegerClient;
let query: JaegerQueryClient;
let rootTraceId = "";

function find(t: Trace, name: string) {
  const span = t.spans.find((s) => s.name === name);
  assert.ok(span, `span ${name} missing in ${JSON.stringify(t.spans.map((s) => s.name))}`);
  return span;
}

before(async () => {
  loadBootstrapEnv();
  client = JaegerClient.fromEnv();
  query = client.query;

  // Two services, one trace: gateway (client span) → worker (server span + db child), sent over
  // OTLP HTTP/protobuf (the Jaeger preset default) with context propagated through headers.
  const gw = client.initTracing({
    serviceName: gateway,
    serviceVersion: "1.2.3",
    environment: "sdk-test",
    setGlobal: false,
  });
  const wk = client.initTracing({
    serviceName: worker,
    setGlobal: false,
    resourceAttributes: { team: "sdk" },
  });
  assert.equal(gw.config.protocol, "http/protobuf");
  const root = gw.tracer("sdk-int").startSpan("GET /orders", {
    kind: SpanKind.CLIENT,
    attributes: { "order.id": tag, "http.request.method": "GET" },
  });
  const headers: Record<string, string> = {};
  gw.propagator.inject(trace.setSpan(ROOT_CONTEXT, root), headers, {
    set: (c, k, v) => {
      c[k] = v;
    },
  });
  const remoteCtx = wk.propagator.extract(ROOT_CONTEXT, headers, {
    get: (c, k) => c[k],
    keys: (c) => Object.keys(c),
  });
  const linked = {
    traceId: randomBytes(16).toString("hex"),
    spanId: randomBytes(8).toString("hex"),
    traceFlags: 1,
  };
  const server = wk.tracer("sdk-int").startSpan(
    "handle /orders",
    {
      kind: SpanKind.SERVER,
      attributes: {
        "int.attr": 42,
        "double.attr": 1.5,
        "bool.attr": true,
        "array.attr": ["a", "b"],
      },
      links: [{ context: linked, attributes: { "link.kind": "follows" } }],
    },
    remoteCtx,
  );
  server.addEvent("cache.miss", { "cache.key": "k1" });
  const db = wk
    .tracer("sdk-int")
    .startSpan("db.query", { kind: SpanKind.INTERNAL }, trace.setSpan(remoteCtx, server));
  await new Promise((r) => setTimeout(r, 30));
  db.end();
  server.setStatus({ code: SpanStatusCode.ERROR, message: "boom" });
  server.end();
  root.setStatus({ code: SpanStatusCode.OK });
  root.end();
  rootTraceId = root.spanContext().traceId;
  await gw.forceFlush();
  await wk.forceFlush();
});

after(async () => {
  await client.close();
});

describe("initTracing + getTrace round trip (OTLP HTTP/protobuf)", () => {
  test("waitForTrace / getTrace return the normalized tree", async () => {
    const t = await query.waitForTrace(rootTraceId, {
      minSpans: 3,
      intervalMs: 250,
      timeoutMs: 30_000,
    });
    assert.equal(t.traceId, rootTraceId);
    const root = find(t, "GET /orders");
    const server = find(t, "handle /orders");
    const db = find(t, "db.query");
    assert.equal(root.serviceName, gateway);
    assert.equal(root.kind, "client");
    assert.equal(root.status.code, "ok");
    assert.equal(root.attributes["order.id"], tag);
    assert.equal(root.parentSpanId, undefined);
    assert.equal(server.serviceName, worker);
    assert.equal(server.parentSpanId, root.spanId);
    assert.equal(server.kind, "server");
    assert.deepEqual(server.status, { code: "error", message: "boom" });
    assert.equal(server.attributes["int.attr"], 42);
    assert.equal(server.attributes["double.attr"], 1.5);
    assert.equal(server.attributes["bool.attr"], true);
    assert.deepEqual(server.attributes["array.attr"], ["a", "b"]);
    assert.equal(server.events[0]?.name, "cache.miss");
    assert.equal(server.events[0]?.attributes["cache.key"], "k1");
    assert.equal(server.links[0]?.attributes["link.kind"], "follows");
    assert.equal(db.parentSpanId, server.spanId);
    assert.ok(db.durationMicros >= 20_000, `duration ${db.durationMicros}`);
    const tree = buildTree(t.spans);
    assert.equal(tree.length, 1);
    assert.equal(tree[0]?.children[0]?.span.name, "handle /orders");

    const same = await query.getTrace(rootTraceId.toUpperCase(), { rawTraces: true });
    assert.equal(same?.spans.length, 3);
    const windowed = await query.requireTrace(rootTraceId, { start: startedAt, end: new Date() });
    assert.equal(windowed.spans.length, 3);
  });

  test("getTrace of an unknown id is null; requireTrace throws NotFoundError; bad ids never reach the server", async () => {
    const unknown = randomBytes(16).toString("hex");
    assert.equal(await query.getTrace(unknown), null);
    await assert.rejects(query.requireTrace(unknown), NotFoundError);
    await assert.rejects(query.getTrace("not-a-trace-id"), ValidationError);
    assert.equal(normalizeTraceId("ABC"), "00000000000000000000000000000abc");
  });
});

describe("search, services, operations, dependencies", () => {
  test("findTraces filters by service, operation, tags and duration", async () => {
    const byService = await query.findTraces({ service: worker, start: startedAt });
    assert.ok(byService.some((t) => t.traceId === rootTraceId));
    const byAll = await query.findTraces({
      service: gateway,
      operation: "GET /orders",
      tags: { "order.id": tag },
      minDurationMicros: 1,
      maxDurationMicros: 60_000_000,
      start: startedAt,
      end: new Date(Date.now() + 60_000),
      limit: 5,
    });
    assert.deepEqual(
      byAll.map((t) => t.traceId),
      [rootTraceId],
    );
    assert.equal(byAll[0]?.spans.length, 3);
    assert.deepEqual(
      await query.findTraces({ service: gateway, operation: "nope", start: startedAt }),
      [],
    );
  });

  test("searchTraces returns trace summaries", async () => {
    const hits = await query.searchTraces({
      service: gateway,
      tags: { "order.id": tag },
      start: startedAt,
    });
    assert.equal(hits.length, 1);
    assert.equal(hits[0]?.traceId, rootTraceId);
    assert.equal(hits[0]?.rootServiceName, gateway);
    assert.equal(hits[0]?.rootName, "GET /orders");
    assert.equal(hits[0]?.spanCount, 3);
    assert.ok((hits[0]?.durationMicros ?? 0) > 0);
    assert.deepEqual(await query.searchTraces({ service: `none-${tag}`, start: startedAt }), []);
  });

  test("listServices contains both services (and Jaeger itself)", async () => {
    const services = await query.listServices();
    assert.ok(services.includes(gateway));
    assert.ok(services.includes(worker));
    assert.ok(services.includes("jaeger"));
  });

  test("listOperations / getOperations, with the spanKind filter", async () => {
    assert.deepEqual(await query.listOperations(worker), ["db.query", "handle /orders"]);
    assert.deepEqual(await query.listOperations(worker, { spanKind: "server" }), [
      "handle /orders",
    ]);
    const ops = await query.getOperations(worker);
    assert.deepEqual(ops.map((o) => `${o.name}:${o.spanKind}`).sort(), [
      "db.query:internal",
      "handle /orders:server",
    ]);
    assert.deepEqual(await query.listOperations(`none-${tag}`), []);
  });

  test("getDependencies reports the gateway → worker edge", async () => {
    const native = await query.getDependencies({
      endTs: new Date(Date.now() + 60_000),
      lookbackMs: 3_600_000,
      service: worker,
    });
    assert.ok(
      native.some((d) => d.parent === gateway && d.child === worker && d.callCount >= 1),
      JSON.stringify(native),
    );
    const window = await query.getDependencies({
      start: startedAt,
      end: new Date(Date.now() + 60_000),
    });
    assert.ok(window.some((d) => d.parent === gateway && d.child === worker));
    const old = await query.getDependencies({
      endTs: new Date(Date.now() - 86_400_000),
      lookbackMs: 60_000,
    });
    assert.ok(!old.some((d) => d.parent === gateway));
  });

  test("a standalone JaegerQueryClient reads the same trace", async () => {
    const standalone = JaegerQueryClient.fromEnv();
    try {
      assert.equal((await standalone.getTrace(rootTraceId))?.spans.length, 3);
    } finally {
      await standalone.close();
    }
  });
});

describe("OTLP gRPC export", () => {
  test("protocol grpc (GROW_JAEGER_OTLP_GRPC_ENDPOINT) round-trips", async () => {
    const config = tracingConfigFromEnv({
      serviceName: grpcService,
      protocol: "grpc",
      setGlobal: false,
    });
    assert.equal(config.endpoint, process.env.GROW_JAEGER_OTLP_GRPC_ENDPOINT);
    const handle = initTracing(config);
    try {
      const span = handle.tracer("sdk-int").startSpan("grpc-root", { attributes: { via: "grpc" } });
      span.end();
      await handle.forceFlush();
      const t = await query.waitForTrace(span.spanContext().traceId, { intervalMs: 250 });
      assert.equal(find(t, "grpc-root").serviceName, grpcService);
      assert.equal(find(t, "grpc-root").attributes.via, "grpc");
    } finally {
      await handle.shutdown();
    }
  });
});

describe("span helpers with the global provider", () => {
  test("withSpan, traced, events, errors, propagation and traceFields round-trip", async () => {
    const handle: JaegerTracingHandle = initTracing({ serviceName: helperService });
    let traceId = "";
    try {
      await withSpan(
        "helper-root",
        async () => {
          traceId = currentTraceId() ?? "";
          assert.match(traceId, /^[0-9a-f]{32}$/);
          assert.match(currentSpanId() ?? "", /^[0-9a-f]{16}$/);
          assert.equal(traceFields().trace_id, traceId);
          addEvent("helper.event", { n: 1 });
          const carrier: Record<string, string> = {};
          inject(carrier);
          assert.ok(carrier.traceparent?.includes(traceId));
          const remote = extract(carrier);
          assert.equal(trace.getSpanContext(remote)?.traceId, traceId);
          const work = traced(async (x: number) => x * 2, "helper-traced");
          assert.equal(await work(21), 42);
          await withSpan("helper-failing", () => {
            const err = new Error("helper boom");
            recordException(err);
            setError(err);
          });
          // sync throw: recorded on the span, then rethrown synchronously
          assert.throws(
            () =>
              withSpan("helper-throws", () => {
                throw new Error("thrown");
              }),
            /thrown/,
          );
          // async rejection: same, as a rejected promise
          await assert.rejects(
            withSpan("helper-rejects", async () => {
              await Promise.resolve();
              throw new Error("rejected");
            }),
            /rejected/,
          );
        },
        { attributes: { helper: true }, kind: SpanKind.SERVER },
      );
      assert.equal(context.active(), ROOT_CONTEXT);
      await handle.forceFlush();
      const t = await query.waitForTrace(traceId, { minSpans: 5, intervalMs: 250 });
      const root = find(t, "helper-root");
      assert.equal(root.attributes.helper, true);
      assert.equal(root.events.find((e) => e.name === "helper.event")?.attributes.n, 1);
      assert.equal(find(t, "helper-traced").parentSpanId, root.spanId);
      assert.equal(find(t, "helper-failing").status.code, "error");
      assert.ok(find(t, "helper-failing").events.some((e) => e.name === "exception"));
      assert.equal(find(t, "helper-throws").status.code, "error");
      assert.equal(find(t, "helper-rejects").status.code, "error");
    } finally {
      await handle.shutdown();
    }
  });
});

describe("remote sampling (port 5778)", () => {
  const fixtureUrl = () => process.env.GROW_JAEGER_SAMPLING_FIXTURE_URL ?? "";

  test("stock all-in-one serves PROBABILISTIC 1", async () => {
    const sampler = client.remoteSampler({ serviceName: gateway, autoStart: false });
    const strategy = await sampler.refresh();
    assert.deepEqual(strategy, { type: "probabilistic", samplingRate: 1, operations: [] });
    await sampler.close();
  });

  test("per-operation, rate-limiting, probabilistic and default strategies from the strategies file", async () => {
    const ops = new JaegerRemoteSampler({
      serviceName: "sdk-sampling-ops",
      samplingUrl: fixtureUrl(),
    });
    const rate = new JaegerRemoteSampler({
      serviceName: "sdk-sampling-rate",
      samplingUrl: fixtureUrl(),
      autoStart: false,
    });
    const prob = new JaegerRemoteSampler({
      serviceName: "sdk-sampling-prob",
      samplingUrl: fixtureUrl(),
      autoStart: false,
    });
    const unknown = new JaegerRemoteSampler({
      serviceName: `unknown-${tag}`,
      samplingUrl: fixtureUrl(),
      autoStart: false,
    });
    try {
      await ops.ready();
      assert.equal(ops.strategy?.samplingRate, 0.8);
      assert.deepEqual(
        ops.strategy?.operations.map((o) => [o.operation, o.samplingRate]),
        [
          ["GET /fast", 0.2],
          ["GET /slow", 0.4],
          ["/health", 0],
        ],
      );
      assert.deepEqual(await rate.refresh().then((s) => [s.type, s.maxTracesPerSecond]), [
        "ratelimiting",
        5,
      ]);
      assert.equal((await prob.refresh()).samplingRate, 0.25);
      const def = await unknown.refresh();
      assert.equal(def.samplingRate, 0.5);
      assert.ok(def.operations.some((o) => o.operation === "/health" && o.samplingRate === 0));
      assert.equal(
        parseSamplingStrategy({ probabilisticSampling: { samplingRate: 0.5 } }).samplingRate,
        0.5,
      );
    } finally {
      await Promise.all([ops.close(), rate.close(), prob.close(), unknown.close()]);
    }
  });

  test("initTracing({ remoteSampler }) drops /health and keeps sampled spans, read back from Jaeger", async () => {
    const service = "sdk-sampling-ops";
    const handle = initTracing({
      serviceName: service,
      setGlobal: false,
      remoteSampler: { samplingUrl: fixtureUrl(), initialSamplingRate: 0 },
    });
    try {
      await handle.remoteSampler?.ready();
      const tracer = handle.tracer("sdk-int");
      const health = tracer.startSpan("/health");
      assert.equal(health.isRecording(), false);
      health.end();
      // "other" uses the service rate 0.8: find a trace id the sampler keeps
      let kept: ReturnType<typeof tracer.startSpan> | undefined;
      for (let i = 0; i < 50 && kept === undefined; i++) {
        const span = tracer.startSpan(`kept-${tag}`);
        if (span.isRecording()) kept = span;
        span.end();
      }
      assert.ok(kept, "no span was sampled at rate 0.8");
      await handle.forceFlush();
      const t = await query.waitForTrace(kept.spanContext().traceId, { intervalMs: 250 });
      assert.equal(t.spans[0]?.name, `kept-${tag}`);
      assert.equal(await query.getTrace(health.spanContext().traceId), null);
    } finally {
      await handle.shutdown();
    }
  });
});
