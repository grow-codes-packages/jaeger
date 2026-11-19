// Runs the shared conformance scenarios (conformance/scenarios) against the real Jaeger.
// Capabilities: core `version` (serverVersion from the UI page); tracing `query` + `services`.

import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { JaegerClient, initTracing } from "@grow-codes/jaeger";
import { formatReport, runConformance } from "@grow-codes/jaeger/testing";
import { loadBootstrapEnv, scenariosDir } from "./env.ts";

let shared: JaegerClient;

before(() => {
  loadBootstrapEnv();
  shared = JaegerClient.fromEnv();
});

after(async () => {
  await shared.close();
});

test("conformance: core", async () => {
  const report = await runConformance({
    scenariosDir,
    categories: ["core"],
    factories: { core: () => JaegerClient.fromEnv() },
    capabilities: { core: ["version"] },
  });
  console.log(formatReport(report));
  assert.equal(report.failed, 0, formatReport(report));
  assert.equal(report.skipped, 0, formatReport(report));
  assert.ok(report.passed > 0);
});

test("conformance: tracing", async () => {
  const report = await runConformance({
    scenariosDir,
    categories: ["tracing"],
    factories: {
      tracing: ({ run }) => ({
        // Unique service per scenario run: Jaeger reports itself as `jaeger`, and stale names
        // from earlier runs must never satisfy `listServices`.
        handle: initTracing({ serviceName: `conformance-ts-${run}`, setGlobal: false }),
        query: shared.query,
      }),
    },
    capabilities: { tracing: ["query", "services"] },
  });
  console.log(formatReport(report));
  assert.equal(report.failed, 0, formatReport(report));
  assert.equal(report.skipped, 0, formatReport(report));
  assert.ok(report.passed >= 2);
});
