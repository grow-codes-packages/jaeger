import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { JaegerClient } from "../../src/client.js";
import {
  type ConformanceOptions,
  conformanceCases,
  formatReport,
  runConformance,
} from "../../src/testing/index.js";
import { type MockServer, startServer } from "../helpers/server.js";

const scenariosDir = fileURLToPath(new URL("../../../conformance/scenarios/", import.meta.url));
let server: MockServer;

beforeAll(async () => {
  server = await startServer((req) => {
    if (req.url === "/")
      return {
        status: 200,
        headers: { "content-type": "text/html" },
        body: 'JAEGER_VERSION = {"gitVersion":"v2.23.0"};',
      };
    if (req.url === "/api/v3/services") return { status: 200, body: { services: [] } };
    return { status: 404, body: "404 page not found" };
  });
});

afterAll(async () => {
  await server.close();
});

function options(): ConformanceOptions {
  return {
    scenariosDir,
    categories: ["core"],
    factories: {
      core: () => new JaegerClient({ url: server.url, token: "conformance-token" }),
    },
    capabilities: { core: ["version"] },
  };
}

describe("conformance (core) against a local mock of the Jaeger query API", () => {
  it("every core scenario passes", async () => {
    const report = await runConformance(options());
    expect(report.failed, formatReport(report)).toBe(0);
    expect(report.skipped).toBe(0);
    expect(report.results.map((r) => r.id)).toContain("core.health");
  });

  for (const c of conformanceCases(options())) {
    it.skipIf(c.skip !== undefined)(`case ${c.id}`, async () => {
      expect((await c.run()).status).toBe("passed");
    });
  }
});
