// Jaeger TypeScript SDK example: health and version, send a small trace over OTLP, read it back
// through the query API, search, list services/operations/dependencies and fetch the remote
// sampling strategy.
//
//   (cd ../../typescript && npm install && npm run build)   # or use the published package
//   npm install
//   GROW_JAEGER_URL=http://127.0.0.1:16686 \
//   GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318 \
//   GROW_JAEGER_SAMPLING_URL=http://127.0.0.1:5778/sampling npm start

import { SpanKind } from "@opentelemetry/api";
import { JaegerClient, JaegerError, withSpan } from "@grow-codes/jaeger";

const client = JaegerClient.fromEnv({
  // Warnings only; the default logger is silent.
  logger: { debug() {}, info() {}, warn: console.warn, error: console.error },
});
console.log(`config: ${client.config}`); // credentials print as [REDACTED]

const serviceName = `example-ts-${Date.now().toString(36)}`;

try {
  const health = await client.health(); // never throws for an unreachable service
  console.log("health:", health);
  if (!health.healthy) process.exit(1);
  console.log("server version:", await client.serverVersion());
  console.log(`ping: ${(await client.ping()).toFixed(1)} ms`);

  // OTLP HTTP/protobuf to :4318 by default (gRPC with protocol: "grpc"); setGlobal registers the
  // provider so withSpan/traced use it. client.close() shuts the handle down.
  const tracing = client.initTracing({ serviceName, serviceVersion: "1.0.0" });
  let traceId = "";
  await withSpan(
    "GET /orders",
    async (span) => {
      traceId = span.spanContext().traceId;
      await withSpan(
        "db.query",
        async () => new Promise((r) => setTimeout(r, 20)),
      );
    },
    { kind: SpanKind.SERVER, attributes: { "order.id": "42" } },
  );
  await tracing.forceFlush();

  const trace = await client.query.waitForTrace(traceId, {
    minSpans: 2,
    intervalMs: 250,
  });
  for (const s of trace.spans) {
    console.log(
      `  ${s.name} (${s.kind}) ${s.durationMicros} µs parent=${s.parentSpanId ?? "-"}`,
    );
  }

  const hits = await client.query.searchTraces({
    service: serviceName,
    tags: { "order.id": "42" },
  });
  console.log("search:", hits);
  console.log(
    "services include ours:",
    (await client.query.listServices()).includes(serviceName),
  );
  console.log("operations:", await client.query.listOperations(serviceName));
  console.log(
    "server operations:",
    await client.query.getOperations(serviceName, { spanKind: "server" }),
  );
  console.log("dependencies (24 h):", await client.query.getDependencies());

  const sampler = client.remoteSampler({ serviceName, autoStart: false });
  console.log("sampling strategy:", await sampler.refresh());
} catch (err) {
  if (err instanceof JaegerError) {
    // Canonical code + HTTP status + request id; the message never contains credentials.
    console.error(`${err.code}: ${err.message}`, {
      httpStatus: err.httpStatus,
      requestId: err.requestId,
    });
    process.exitCode = 1;
  } else {
    throw err;
  }
} finally {
  await client.close();
}
