import { inspect } from "node:util";
import { afterEach, describe, expect, it } from "vitest";
import {
  ConfigError,
  PACKAGE_KEY,
  REDACTED,
  RetryPolicy,
  Secret,
  VENDOR_ENV,
  JaegerClient,
  JaegerConfig,
} from "../src/index.js";
import { fixtureBody, fixtureResponse } from "./helpers/fixtures.js";
import { jaegerServer, type Route } from "./helpers/jaeger.js";
import { type MockServer, closedPort, startServer } from "./helpers/server.js";

const servers: MockServer[] = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

const INDEX_HTML = `<!doctype html><html><head><script>// ${fixtureBody("serverVersion.index_html") as string}
const JAEGER_CONFIG = {};</script></head></html>`;

async function server(routes: Record<string, Route>): Promise<MockServer> {
  const s = await jaegerServer(routes);
  servers.push(s);
  return s;
}

const fast = { retry: { maxRetries: 1, initialBackoffMs: 1 } };

describe("JaegerConfig", () => {
  it("uses Jaeger defaults and freezes", () => {
    const c = new JaegerConfig();
    expect(c.url).toBe("http://127.0.0.1:16686");
    expect(c.samplingUrl).toBe("http://127.0.0.1:5778/sampling");
    expect(c.healthUrl).toBeUndefined();
    expect(c.metricsUrl).toBeUndefined();
    expect(c.requestTimeoutMs).toBe(30_000);
    expect(c.retry).toBeInstanceOf(RetryPolicy);
    expect(c.token).toBeUndefined();
    expect(Object.isFrozen(c)).toBe(true);
    expect(JaegerConfig.from(c)).toBe(c);
    expect(JaegerConfig.from().url).toBe(c.url);
    expect(JaegerConfig.ENV_PREFIX).toBe(`GROW_${PACKAGE_KEY.toUpperCase()}_`);
  });

  it("keeps optional settings", () => {
    const hooks = {};
    const logger = { debug() {}, info() {}, warn() {}, error() {} };
    const c = new JaegerConfig({
      proxy: "http://p:1",
      userAgentSuffix: "s",
      hooks,
      logger,
      token: new Secret("abcd"),
      healthUrl: "http://j:13133/status",
      metricsUrl: "http://j:8888/metrics",
    });
    expect(c.proxy).toBe("http://p:1");
    expect(c.userAgentSuffix).toBe("s");
    expect(c.hooks).toBe(hooks);
    expect(c.logger).toBe(logger);
    expect(c.knownSecrets()).toEqual(["abcd"]);
    expect(c.toJSON()).toMatchObject({
      healthUrl: "http://j:13133/status",
      metricsUrl: "http://j:8888/metrics",
    });
  });

  it("validate() raises ConfigError", () => {
    expect(() => new JaegerConfig({ url: "ftp://x" })).toThrow(ConfigError);
    expect(() => new JaegerConfig({ token: "   " })).toThrow(ConfigError);
    expect(() => new JaegerConfig({ samplingUrl: "nope" })).toThrow(/samplingUrl/);
    expect(() => new JaegerConfig({ healthUrl: "ftp://h" })).toThrow(/healthUrl/);
    expect(() => new JaegerConfig({ metricsUrl: "x" })).toThrow(/metricsUrl/);
  });

  it("core.redaction.config: toString/JSON/inspect hide credentials", () => {
    const c = new JaegerConfig({
      token: "super-secret-tok",
      headers: { "x-api-key": "hdr-secret" },
      samplingUrl: "http://user:pw-secret@s:5778/sampling",
    });
    for (const form of [String(c), JSON.stringify(c), inspect(c), `${c.token}`]) {
      expect(form).not.toContain("super-secret-tok");
      expect(form).not.toContain("hdr-secret");
      expect(form).not.toContain("pw-secret");
    }
    expect(String(c)).toContain(REDACTED);
    expect(String(c)).toMatch(/^JaegerConfig\(/);
    expect(new JaegerConfig().toJSON().token).toBeNull();
  });

  it("core.config.env-precedence: explicit > GROW_* > vendor > default", () => {
    expect(VENDOR_ENV.url).toEqual(["JAEGER_QUERY_URL"]);
    expect(VENDOR_ENV.samplingUrl).toEqual(["JAEGER_SAMPLING_ENDPOINT"]);
    const env = {
      GROW_JAEGER_URL: "http://grow:1",
      JAEGER_QUERY_URL: "http://vendor:2",
      GROW_JAEGER_TOKEN: "grow-token",
      GROW_JAEGER_REQUEST_TIMEOUT: "5s",
      GROW_JAEGER_MAX_RETRIES: "7",
      JAEGER_SAMPLING_ENDPOINT: "http://vendor-sampling:5778/sampling",
      GROW_JAEGER_HEALTH_URL: "http://grow:13133/status",
      GROW_JAEGER_METRICS_URL: "http://grow:8888/metrics",
    };
    const fromEnv = JaegerConfig.fromEnv({}, env);
    expect(fromEnv.url).toBe("http://grow:1");
    expect(fromEnv.token?.reveal()).toBe("grow-token");
    expect(fromEnv.requestTimeoutMs).toBe(5000);
    expect(fromEnv.retry.maxRetries).toBe(7);
    expect(fromEnv.samplingUrl).toBe("http://vendor-sampling:5778/sampling");
    expect(fromEnv.healthUrl).toBe("http://grow:13133/status");
    expect(fromEnv.metricsUrl).toBe("http://grow:8888/metrics");
    expect(
      JaegerConfig.fromEnv({}, { ...env, GROW_JAEGER_SAMPLING_URL: "http://grow:5778/sampling" })
        .samplingUrl,
    ).toBe("http://grow:5778/sampling");

    expect(JaegerConfig.fromEnv({}, { JAEGER_QUERY_URL: "http://vendor:2" }).url).toBe(
      "http://vendor:2",
    );
    const explicit = JaegerConfig.fromEnv(
      { url: "http://explicit:3", token: "explicit-token", retry: { initialBackoffMs: 1 } },
      env,
    );
    expect(explicit.url).toBe("http://explicit:3");
    expect(explicit.token?.reveal()).toBe("explicit-token");
    expect(explicit.retry.maxRetries).toBe(7);
    expect(explicit.retry.initialBackoffMs).toBe(1);
    expect(JaegerConfig.fromEnv({}, {}).url).toBe(JaegerConfig.DEFAULT_URL);
    expect(JaegerConfig.fromEnv().url).toBeTypeOf("string");

    let err: unknown;
    try {
      JaegerConfig.fromEnv({}, { GROW_JAEGER_TOTAL_TIMEOUT: "forever-and-secret" });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(ConfigError);
    expect((err as Error).message).toContain("GROW_JAEGER_TOTAL_TIMEOUT");
    expect((err as Error).message).not.toContain("forever-and-secret");
  });
});

describe("JaegerClient", () => {
  it("health probes GET / (HTML) and ping probes /api/v3/services", async () => {
    const s = await server({
      "/": { status: 200, headers: { "content-type": "text/html" }, body: INDEX_HTML },
      "/api/v3/services": "listServices.ok",
    });
    const client = new JaegerClient({ url: s.url, token: "t0ken-value", ...fast });
    expect(await client.health()).toEqual({ healthy: true, status: "ok", details: {} });
    expect(s.requests[0]!.headers.authorization).toBeUndefined();
    expect(await client.ping()).toBeGreaterThanOrEqual(0);
    expect(s.requests[1]!.url).toBe("/api/v3/services");
    expect(s.requests[1]!.headers.authorization).toBe("Bearer t0ken-value");
    expect(s.requests[1]!.headers["user-agent"]).toContain(`grow-codes-${PACKAGE_KEY}/`);
    expect(client.transport.userAgent).toMatch(/\(typescript\)$/);
    await client.close();
  });

  it("health adds the healthcheckv2 /status detail when healthUrl is set", async () => {
    const s = await server({
      "/": { status: 200, body: "<html></html>", headers: { "content-type": "text/html" } },
      "/status": "health.status_ok",
      "/bad": { status: 200, body: { healthy: false, status: "StatusRecoverableError" } },
      "/word": { status: 200, body: { status: "degraded" } },
      "/empty": { status: 200, body: "" },
      "/down": { status: 503, body: { healthy: false } },
    });
    const ok = new JaegerClient({ url: s.url, healthUrl: `${s.url}/status`, ...fast });
    expect(await ok.health()).toEqual({
      healthy: true,
      status: "ok",
      details: {
        status: "StatusOK",
        startTime: "2026-11-18T20:48:29.787729523Z",
        statusTime: "2026-11-18T20:48:29.810012479Z",
      },
    });
    const bad = new JaegerClient({ url: s.url, healthUrl: `${s.url}/bad`, ...fast });
    expect(await bad.health()).toMatchObject({ healthy: false, status: "StatusRecoverableError" });
    const word = new JaegerClient({ url: s.url, healthUrl: `${s.url}/word`, ...fast });
    expect(await word.health()).toMatchObject({ healthy: false, status: "degraded" });
    const empty = new JaegerClient({ url: s.url, healthUrl: `${s.url}/empty`, ...fast });
    expect(await empty.health()).toMatchObject({ healthy: true, status: "ok" });
    const down = new JaegerClient({
      url: s.url,
      healthUrl: `${s.url}/down`,
      retry: { maxRetries: 0 },
    });
    expect(await down.health()).toMatchObject({
      healthy: false,
      status: "unhealthy",
      details: { httpStatus: "503" },
    });
    for (const c of [ok, bad, word, empty, down]) await c.close();
  });

  it("health of a failing query root is unhealthy", async () => {
    const s = await startServer([{ status: 500 }]);
    servers.push(s);
    const sick = new JaegerClient({ url: s.url, retry: { maxRetries: 0 } });
    expect(await sick.health()).toMatchObject({ healthy: false, status: "unhealthy" });
    await sick.close();
  });

  it("core.health.unreachable: closed port -> healthy=false, status=unreachable", async () => {
    const port = await closedPort();
    const client = new JaegerClient({ url: `http://127.0.0.1:${port}`, ...fast });
    expect(await client.health()).toEqual({
      healthy: false,
      status: "unreachable",
      details: { error: "UNAVAILABLE" },
    });
    await client.close();
  });

  it("health rethrows cancellation", async () => {
    const s = await server({ "/": { status: 200, body: "" } });
    const client = new JaegerClient({ url: s.url });
    await expect(client.health({ signal: AbortSignal.abort() })).rejects.toMatchObject({
      code: "CANCELLED",
    });
    await client.close();
  });

  it("ping maps a proxy 401 to AuthenticationError without leaking the token", async () => {
    const s = await server({ "/api/v3/services": { status: 401, body: { message: "bad token" } } });
    const client = new JaegerClient(
      new JaegerConfig({ url: s.url, token: "wrong-token-xyz", ...fast }),
    );
    const err = await client.ping().catch((e: unknown) => e);
    expect(err).toMatchObject({ code: "AUTHENTICATION", httpStatus: 401 });
    expect((err as Error).message).not.toContain("wrong-token-xyz");
    await client.close();
  });

  it("serverVersion parses JAEGER_VERSION from the UI page without the leading v", async () => {
    const s = await server({
      "/": { status: 200, headers: { "content-type": "text/html" }, body: INDEX_HTML },
    });
    const client = new JaegerClient({ url: s.url, ...fast });
    expect(await client.serverVersion()).toBe("2.23.0");
    expect(await client.buildInfo()).toEqual({
      gitVersion: "v2.23.0",
      gitCommit: "d723f91447f29a345c288f37eb8d1ce7807bc0be",
      buildDate: "2026-11-04T16:02:09Z",
    });
    await client.close();
  });

  it("serverVersion falls back to target_info at metricsUrl, else UnsupportedError", async () => {
    const metrics =
      '# HELP target_info Target metadata\ntarget_info{service_instance_id="x",service_name="jaeger",service_version="v2.23.0"} 1\n';
    const s = await server({
      "/": {
        status: 200,
        headers: { "content-type": "text/html" },
        body: "<html>no version</html>",
      },
      "/metrics": { status: 200, body: metrics },
      "/metrics-empty": { status: 200, body: "up 1\n" },
      "/broken/": {
        status: 200,
        headers: { "content-type": "text/html" },
        body: "JAEGER_VERSION = {broken};",
      },
      "/odd/": {
        status: 200,
        headers: { "content-type": "text/html" },
        body: 'JAEGER_VERSION = {"gitVersion":"","gitCommit":1};',
      },
    });
    const withMetrics = new JaegerClient({ url: s.url, metricsUrl: `${s.url}/metrics`, ...fast });
    expect(await withMetrics.serverVersion()).toBe("2.23.0");
    const noVersion = new JaegerClient({
      url: s.url,
      metricsUrl: `${s.url}/metrics-empty`,
      ...fast,
    });
    await expect(noVersion.serverVersion()).rejects.toMatchObject({ code: "UNSUPPORTED" });
    const bare = new JaegerClient({ url: s.url, ...fast });
    await expect(bare.serverVersion()).rejects.toMatchObject({ code: "UNSUPPORTED" });
    const broken = new JaegerClient({ url: `${s.url}/broken`, ...fast });
    expect(await broken.buildInfo()).toBeNull();
    await expect(broken.serverVersion()).rejects.toMatchObject({ code: "UNSUPPORTED" });
    const odd = new JaegerClient({ url: `${s.url}/odd`, ...fast });
    expect(await odd.buildInfo()).toEqual({});
    const noUi = await server({});
    const disabled = new JaegerClient({ url: noUi.url, ...fast });
    expect(await disabled.buildInfo()).toBeNull();
    await disabled.close();
    const emptyBody = await server({ "/": { status: 200, body: "" } });
    const empty = new JaegerClient({ url: emptyBody.url, ...fast });
    expect(await empty.buildInfo()).toBeNull();
    for (const c of [withMetrics, noVersion, bare, broken, odd, empty]) await c.close();
  });

  it("serverVersion: a 404 on / and on metricsUrl is UnsupportedError, other errors propagate", async () => {
    const s = await server({
      "/": { status: 404, body: "404 page not found" },
      "/metrics": { status: 404, body: 'target_info{service_version="v9.9.9"} 1' },
      "/metrics-down": { status: 400, body: "bad" },
    });
    const missing = new JaegerClient({ url: s.url, metricsUrl: `${s.url}/metrics`, ...fast });
    await expect(missing.serverVersion()).rejects.toMatchObject({ code: "UNSUPPORTED" });
    const down = new JaegerClient({ url: s.url, metricsUrl: `${s.url}/metrics-down`, ...fast });
    await expect(down.serverVersion()).rejects.toMatchObject({ code: "VALIDATION" });
    await missing.close();
    await down.close();
  });

  it("samplingStrategy fetches the strategy once with the token; blank service is VALIDATION", async () => {
    const s = await server({ "/sampling": "sampling.per_operation" });
    const client = new JaegerClient({
      url: s.url,
      samplingUrl: `${s.url}/sampling`,
      token: "t0ken-value",
      ...fast,
    });
    const strategy = await client.samplingStrategy("checkout");
    expect(strategy.type).toBe("probabilistic");
    expect(strategy.operations.length).toBeGreaterThan(0);
    expect(s.requests[0]!.url).toBe("/sampling?service=checkout");
    expect(s.requests[0]!.headers.authorization).toBe("Bearer t0ken-value");
    await expect(client.samplingStrategy(" ")).rejects.toMatchObject({ code: "VALIDATION" });
    expect(s.requests).toHaveLength(1);
    await client.close();
  });

  it("core.close.idempotent: close twice is safe", async () => {
    const s = await server({ "/": { status: 200, body: "" } });
    const client = new JaegerClient({ url: s.url });
    await client.health();
    await client.close();
    await client.close();
    await expect(client.ping()).rejects.toMatchObject({ code: "CONFIG" });
  });

  it("fromEnv builds from GROW_JAEGER_* variables", async () => {
    const s = await server({ "/api/v3/services": fixtureResponse("listServices.ok") });
    const key = "GROW_JAEGER_URL";
    const saved = process.env[key];
    process.env[key] = s.url;
    try {
      const client = JaegerClient.fromEnv({ token: "t0ken-value" });
      expect(client.config.url).toBe(s.url);
      expect(await client.ping()).toBeGreaterThanOrEqual(0);
      await client.close();
    } finally {
      if (saved === undefined) delete process.env[key];
      else process.env[key] = saved;
    }
  });
});
