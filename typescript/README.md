# @grow-codes/**KEY**

TypeScript SDK for [**DISPLAY**](https://grow-codes-packages.github.io/jaeger/) by Grow Codes.
Node.js 22.12 or newer, ESM and CommonJS, full type definitions.

```sh
npm install @grow-codes/jaeger
```

```ts
import { JaegerClient } from "@grow-codes/jaeger";

// Reads GROW_JAEGER_URL, GROW_JAEGER_TOKEN, timeouts, TLS and proxy settings.
const client = JaegerClient.fromEnv();
try {
  console.log(await client.health()); // { healthy: true, status: "ok", details: {} }
  console.log(`ping: ${(await client.ping()).toFixed(1)} ms`);
} finally {
  await client.close();
}
```

Every error is a `JaegerError` subclass with a canonical `code` (`NOT_FOUND`,
`AUTHENTICATION`, `RATE_LIMITED`, …), `httpStatus`, `requestId` and `retryable`. Credentials
never appear in messages, logs, hook data or `toString()` output.

Test helpers (in-memory fakes and the conformance runner) live in
`@grow-codes/jaeger/testing`.

Documentation: https://grow-codes-packages.github.io/jaeger/ · Licence: Apache-2.0.
**DISPLAY** is a trademark of **VENDOR**; this package is not affiliated with or endorsed by it.
