import { randomBytes } from "node:crypto";
import { ROOT_CONTEXT, SpanKind } from "@opentelemetry/api";
import { SamplingDecision } from "@opentelemetry/sdk-trace-base";
import { afterEach, describe, expect, it } from "vitest";
import {
  ConfigError,
  JaegerClient,
  JaegerRemoteSampler,
  RateLimiter,
  ValidationError,
  parseSamplingStrategy,
} from "../../src/index.js";
import { fixtureBody, fixtureResponse } from "../helpers/fixtures.js";
import { jaegerServer, params } from "../helpers/jaeger.js";
import type { MockResponse, MockServer } from "../helpers/server.js";

const servers: MockServer[] = [];
const samplers: JaegerRemoteSampler[] = [];
afterEach(async () => {
  await Promise.all(samplers.splice(0).map((s) => s.close()));
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

const fast = { retry: { maxRetries: 0 } };

const IDS = Array.from({ length: 2000 }, () => randomBytes(16).toString("hex"));

function traceIds(n: number): string[] {
  return IDS.slice(0, n);
}

function sampledShare(sampler: JaegerRemoteSampler, name: string, n = 2000): number {
  let yes = 0;
  for (const id of traceIds(n)) {
    const r = sampler.shouldSample(ROOT_CONTEXT, id, name, SpanKind.SERVER, {}, []);
    if (r.decision === SamplingDecision.RECORD_AND_SAMPLED) yes++;
  }
  return yes / n;
}

async function samplingServer(
  handler: (service: string, index: number) => MockResponse | string,
): Promise<MockServer> {
  let i = 0;
  const s = await jaegerServer({
    "/sampling": (_req, url) => handler(url.searchParams.get("service") ?? "", i++),
  });
  servers.push(s);
  return s;
}

function sampler(opts: ConstructorParameters<typeof JaegerRemoteSampler>[0]): JaegerRemoteSampler {
  const s = new JaegerRemoteSampler(opts);
  samplers.push(s);
  return s;
}

describe("parseSamplingStrategy", () => {
  it("parses every recorded variant", () => {
    expect(parseSamplingStrategy(fixtureBody("sampling.default_stock"))).toEqual({
      type: "probabilistic",
      samplingRate: 1,
      operations: [],
    });
    expect(parseSamplingStrategy(fixtureBody("sampling.per_operation"))).toEqual({
      type: "probabilistic",
      samplingRate: 0.8,
      defaultSamplingProbability: 0.8,
      operations: [
        { operation: "GET /fast", samplingRate: 0.2 },
        { operation: "GET /slow", samplingRate: 0.4 },
        { operation: "/health", samplingRate: 0 },
      ],
    });
    expect(parseSamplingStrategy(fixtureBody("sampling.rate_limiting"))).toMatchObject({
      type: "ratelimiting",
      maxTracesPerSecond: 5,
      operations: [{ operation: "/health", samplingRate: 0 }],
    });
    expect(parseSamplingStrategy(fixtureBody("sampling.probabilistic")).samplingRate).toBe(0.25);
  });

  it("defaults omitted numbers to 0 and rejects invalid values", () => {
    expect(
      parseSamplingStrategy({
        strategyType: "RATE_LIMITING",
        rateLimitingSampling: {},
        operationSampling: {
          perOperationStrategies: [{ operation: "x", probabilisticSampling: {} }, { bad: 1 }, 3],
        },
      }),
    ).toEqual({
      type: "ratelimiting",
      maxTracesPerSecond: 0,
      defaultSamplingProbability: 0,
      operations: [{ operation: "x", samplingRate: 0 }],
    });
    expect(parseSamplingStrategy({}).type).toBe("probabilistic");
    expect(parseSamplingStrategy({ strategyType: 1 }).type).toBe("ratelimiting");
    expect(
      parseSamplingStrategy({ operationSampling: { perOperationStrategies: "x" } }).operations,
    ).toEqual([]);
    for (const bad of [
      null,
      [],
      { strategyType: "WEIRD" },
      { probabilisticSampling: { samplingRate: 1.5 } },
      { probabilisticSampling: { samplingRate: "0.5" } },
      { rateLimitingSampling: { maxTracesPerSecond: -1 } },
      { rateLimitingSampling: { maxTracesPerSecond: "5" } },
    ]) {
      expect(() => parseSamplingStrategy(bad)).toThrow(ValidationError);
    }
  });
});

describe("RateLimiter", () => {
  it("starts full and refills at the configured rate", () => {
    let now = 0;
    const limiter = new RateLimiter(2, () => now);
    expect(limiter.maxBalance).toBe(2);
    expect([limiter.check(), limiter.check(), limiter.check()]).toEqual([true, true, false]);
    now += 500;
    expect([limiter.check(), limiter.check()]).toEqual([true, false]);
    now -= 1000; // clock going backwards never adds credit
    expect(limiter.check()).toBe(false);
    const zero = new RateLimiter(0, () => now);
    expect(zero.maxBalance).toBe(1);
    expect(zero.check()).toBe(true);
    now += 10_000;
    expect(zero.check()).toBe(false);
    expect(new RateLimiter(1).check()).toBe(true);
  });
});

describe("JaegerRemoteSampler", () => {
  it("uses the initial rate before the first strategy", () => {
    const s = sampler({ serviceName: "svc", autoStart: false, initialSamplingRate: 0 });
    expect(s.strategy).toBeNull();
    expect(sampledShare(s, "op", 200)).toBe(0);
    expect(s.samplingUrl).toBe("http://127.0.0.1:5778/sampling");
    expect(s.pollIntervalMs).toBe(60_000);
    expect(String(s)).toContain("svc");
  });

  it("per-operation strategies win; other operations use the service rate", async () => {
    const server = await samplingServer(() => "sampling.per_operation");
    const s = sampler({
      serviceName: "sdk-sampling-ops",
      samplingUrl: `${server.url}/sampling`,
      autoStart: false,
      config: fast,
    });
    const strategy = await s.refresh();
    expect(strategy.samplingRate).toBe(0.8);
    expect(params(server.requests[0]).get("service")).toBe("sdk-sampling-ops");
    expect(sampledShare(s, "/health")).toBe(0);
    expect(sampledShare(s, "GET /fast")).toBeGreaterThan(0.1);
    expect(sampledShare(s, "GET /fast")).toBeLessThan(0.3);
    expect(sampledShare(s, "other")).toBeGreaterThan(0.7);
    // deterministic per trace id
    const id = traceIds(1)[0]!;
    const a = s.shouldSample(ROOT_CONTEXT, id, "GET /slow", SpanKind.CLIENT, {}, []);
    const b = s.shouldSample(ROOT_CONTEXT, id, "GET /slow", SpanKind.CLIENT, {}, []);
    expect(a.decision).toBe(b.decision);
  });

  it("rate limiting applies to unlisted operations, listed ones stay probabilistic", async () => {
    const server = await samplingServer(() => "sampling.rate_limiting");
    let now = 0;
    const s = new JaegerRemoteSampler(
      {
        serviceName: "sdk-sampling-rate",
        samplingUrl: `${server.url}/sampling`,
        autoStart: false,
        config: fast,
      },
      { now: () => now },
    );
    samplers.push(s);
    await s.refresh();
    expect(sampledShare(s, "/health", 50)).toBe(0);
    expect(sampledShare(s, "work", 10)).toBe(0.5); // 5 credits, then none
    now += 1000;
    expect(sampledShare(s, "work", 10)).toBe(0.5);
    await s.refresh(); // same rate keeps the limiter
    expect(sampledShare(s, "work", 1)).toBe(0);
  });

  it("falls back to defaultSamplingProbability, then the initial rate", async () => {
    const bodies = [
      { operationSampling: { defaultSamplingProbability: 0 } },
      { strategyType: "PROBABILISTIC" },
      { strategyType: "RATE_LIMITING", rateLimitingSampling: { maxTracesPerSecond: 1 } },
      { strategyType: "PROBABILISTIC", probabilisticSampling: { samplingRate: 1 } },
    ];
    const server = await samplingServer((_s, i) => ({
      status: 200,
      body: bodies[Math.min(i, 3)]!,
    }));
    const s = sampler({
      serviceName: "svc",
      samplingUrl: `${server.url}/sampling`,
      autoStart: false,
      initialSamplingRate: 1,
      config: fast,
    });
    await s.refresh();
    expect(sampledShare(s, "x", 100)).toBe(0);
    await s.refresh();
    expect(sampledShare(s, "x", 100)).toBe(1);
    await s.refresh();
    expect(sampledShare(s, "x", 3)).toBeCloseTo(1 / 3);
    await s.refresh(); // rate limiting removed again
    expect(sampledShare(s, "x", 100)).toBe(1);
  });

  it("polls, reports updates and errors, and keeps the last good strategy", async () => {
    const server = await samplingServer((_s, i) =>
      i === 0
        ? "sampling.probabilistic"
        : i === 1
          ? { status: 200, body: { probabilisticSampling: { samplingRate: 7 } } }
          : i === 2
            ? "sampling.missing_service"
            : "sampling.default_stock",
    );
    const updates: number[] = [];
    const errors: unknown[] = [];
    const s = sampler({
      serviceName: "sdk-sampling-prob",
      samplingUrl: `${server.url}/sampling`,
      pollIntervalMs: 10,
      config: fast,
      onUpdate: (st) => {
        updates.push(st.samplingRate ?? -1);
        throw new Error("callback errors are ignored");
      },
      onError: (e) => errors.push(e),
    });
    s.start(); // idempotent
    await s.ready();
    expect(s.strategy?.samplingRate).toBe(0.25);
    await expect.poll(() => updates.length, { timeout: 5000 }).toBeGreaterThanOrEqual(2);
    expect(updates[0]).toBe(0.25);
    expect(updates[1]).toBe(1);
    expect(errors).toHaveLength(2);
    expect(errors[0]).toBeInstanceOf(ValidationError);
    expect(errors[1]).toMatchObject({ code: "VALIDATION", httpStatus: 400 });
    await s.close();
    await s.shutdown();
    const count = server.requests.length;
    await new Promise((r) => setTimeout(r, 40));
    expect(server.requests.length).toBe(count);
    s.start(); // no restart after close
  });

  it("a poll error without onError is swallowed", async () => {
    const server = await samplingServer(() => ({ status: 500, body: "boom" }));
    const s = sampler({ serviceName: "svc", samplingUrl: `${server.url}/sampling`, config: fast });
    await s.ready();
    expect(s.strategy).toBeNull();
  });

  it("validates its options", () => {
    expect(() => new JaegerRemoteSampler({ serviceName: " " })).toThrow(ConfigError);
    expect(
      () => new JaegerRemoteSampler({ serviceName: "s", pollIntervalMs: 0, autoStart: false }),
    ).toThrow(ConfigError);
    expect(
      () => new JaegerRemoteSampler({ serviceName: "s", initialSamplingRate: 2, autoStart: false }),
    ).toThrow(ConfigError);
    expect(
      () => new JaegerRemoteSampler({ serviceName: "s", samplingUrl: "ftp://x", autoStart: false }),
    ).toThrow(ConfigError);
  });

  it("client.remoteSampler uses the client settings and is closed with the client", async () => {
    const server = await samplingServer(() => fixtureResponse("sampling.default_stock"));
    const client = new JaegerClient({
      samplingUrl: `${server.url}/sampling`,
      token: "sampling-token-1",
      proxy: undefined,
      userAgentSuffix: "ua",
      hooks: {},
      logger: { debug() {}, info() {}, warn() {}, error() {} },
      ...fast,
    });
    const s = client.remoteSampler({ serviceName: "svc" });
    await s.ready();
    expect(s.strategy?.samplingRate).toBe(1);
    expect(server.requests[0]!.headers.authorization).toBe("Bearer sampling-token-1");
    const other = client.remoteSampler({
      serviceName: "svc2",
      samplingUrl: `${server.url}/sampling?x=1`,
      autoStart: false,
      config: { ...fast, proxy: "http://127.0.0.1:1" },
    });
    expect(other.samplingUrl).toContain("x=1");
    await client.close();
    expect(() => client.remoteSampler({ serviceName: "late" })).toThrow(ConfigError);
    await expect(s.refresh()).rejects.toMatchObject({ code: "CONFIG" });
  });
});
