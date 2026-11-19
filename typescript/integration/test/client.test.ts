// Integration tests for JaegerClient against the real service (docker/compose.yml).

import assert from "node:assert/strict";
import { before, test } from "node:test";
import { JaegerClient, JaegerConfig, JaegerError } from "@grow-codes/jaeger";
import { loadBootstrapEnv } from "./env.ts";

before(() => {
  loadBootstrapEnv();
  assert.ok(process.env.GROW_JAEGER_URL, "GROW_JAEGER_URL must be set");
});

test("config: fromEnv reads GROW_JAEGER_* and the bootstrap URLs", () => {
  const config = JaegerConfig.fromEnv();
  assert.equal(config.url, process.env.GROW_JAEGER_URL);
  assert.equal(config.samplingUrl, process.env.GROW_JAEGER_SAMPLING_URL);
  assert.equal(config.healthUrl, process.env.GROW_JAEGER_HEALTH_URL);
  config.validate();
  assert.match(String(config), /^JaegerConfig\(/);
});

test("health reports a healthy service, with the healthcheckv2 detail", async () => {
  const client = JaegerClient.fromEnv();
  try {
    const health = await client.health();
    assert.equal(health.healthy, true, JSON.stringify(health));
    assert.equal(health.status, "ok");
    assert.equal(health.details.status, "StatusOK");
    const bare = await JaegerClient.fromEnv({ healthUrl: undefined }).health();
    assert.equal(bare.healthy, true);
  } finally {
    await client.close();
  }
});

test("health of an unreachable endpoint is unreachable, never an exception", async () => {
  const client = new JaegerClient({ url: "http://127.0.0.1:9", retry: { maxRetries: 0 } });
  try {
    const health = await client.health();
    assert.equal(health.healthy, false);
    assert.equal(health.status, "unreachable");
  } finally {
    await client.close();
  }
});

test("ping round-trips through /api/v3/services", async () => {
  const client = JaegerClient.fromEnv();
  try {
    const latency = await client.ping();
    assert.equal(typeof latency, "number");
    assert.ok(latency >= 0);
  } finally {
    await client.close();
  }
});

test("serverVersion is 2.23.0 (UI page) and matches target_info from the metrics port", async () => {
  const client = JaegerClient.fromEnv();
  try {
    assert.equal(await client.serverVersion(), "2.23.0");
    const info = await client.buildInfo();
    assert.equal(info?.gitVersion, "v2.23.0");
    assert.match(info?.gitCommit ?? "", /^[0-9a-f]{7,}$/);
    // metrics fallback: a base URL without the UI page (the metrics port itself)
    const metricsUrl = process.env.GROW_JAEGER_METRICS_URL ?? "";
    const metricsOnly = new JaegerClient({ url: new URL(metricsUrl).origin, metricsUrl });
    assert.equal(await metricsOnly.buildInfo(), null);
    assert.equal(await metricsOnly.serverVersion(), "2.23.0");
    await metricsOnly.close();
  } finally {
    await client.close();
    await client.close();
  }
});

test("close is idempotent and later calls fail with CONFIG", async () => {
  const client = JaegerClient.fromEnv();
  await client.close();
  await client.close();
  await assert.rejects(client.ping(), (err: unknown) => {
    assert.ok(err instanceof JaegerError);
    assert.equal(err.code, "CONFIG");
    return true;
  });
});
