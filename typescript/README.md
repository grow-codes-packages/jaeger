# @grow-codes/jaeger

TypeScript SDK for [Jaeger](https://grow-codes-packages.github.io/jaeger/) by Grow Codes: OpenTelemetry
tracing preset (OTLP HTTP/protobuf or gRPC), a query client for Jaeger's v3 HTTP API and a remote
sampler. Node.js 22.12 or newer, ESM and CommonJS, full type definitions. Tested against
`jaegertracing/jaeger:2.23.0`.

```sh
npm install @grow-codes/jaeger
```

```ts
import { JaegerClient, withSpan } from "@grow-codes/jaeger";

// Reads GROW_JAEGER_URL (or JAEGER_QUERY_URL), OTEL_EXPORTER_OTLP_* / GROW_JAEGER_OTLP_*, …
const client = JaegerClient.fromEnv();
try {
  const tracing = client.initTracing({ serviceName: "checkout" }); // OTLP → :4318
  let traceId = "";
  await withSpan("GET /orders", async (span) => {
    traceId = span.spanContext().traceId;
  });
  await tracing.forceFlush();
  const trace = await client.query.waitForTrace(traceId);
  console.log(trace.spans.map((s) => s.name));
  console.log(await client.query.searchTraces({ service: "checkout" }));
} finally {
  await client.close(); // also shuts down the tracing handle
}
```

| Area     | API                                                                                                                                                                                                                 |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Client   | `JaegerClient` (`health`, `ping`, `serverVersion`, `buildInfo`, `close`, `query`, `initTracing`, `remoteSampler`), `JaegerConfig.fromEnv()`                                                                         |
| Tracing  | `initTracing`, `tracingConfigFromEnv`, `jaegerExporterFactory`, `resolveJaegerEndpoint`, span helpers (`withSpan`, `traced`, `inject`, `extract`, `traceFields`, …), Express/Fastify/NestJS adapters, `tracedFetch` |
| Query    | `JaegerQueryClient`: `getTrace`, `requireTrace`, `findTraces`, `searchTraces`, `listServices`, `listOperations`, `getOperations`, `getDependencies`, `waitForTrace`                                                 |
| Sampling | `JaegerRemoteSampler`, `parseSamplingStrategy`                                                                                                                                                                      |

Every error is a `JaegerError` subclass with a canonical `code` (`NOT_FOUND`, `VALIDATION`,
`UNAVAILABLE`, …), `httpStatus`, `requestId` (Jaeger's `traceresponse` header) and `retryable`.
Credentials never appear in messages, logs, hook data or `toString()` output.

Test helpers (in-memory fakes and the conformance runner) live in `@grow-codes/jaeger/testing`.

Documentation: https://grow-codes-packages.github.io/jaeger/ · Licence: Apache-2.0.
Jaeger is a trademark of The Linux Foundation; this package is not affiliated with or endorsed by it.
