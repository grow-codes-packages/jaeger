// Loads the responses recorded from the real jaegertracing/jaeger:2.23.0 container (prep verify.sh).

import { readFileSync } from "node:fs";
import type { MockResponse } from "./server.js";

interface Fixture {
  request: { method: string; path: string };
  response: { status: number; headers?: Record<string, string>; body?: unknown; bodyExcerpt?: string };
}

/** The raw recorded fixture. */
export function fixture(name: string): Fixture {
  return JSON.parse(
    readFileSync(new URL(`../fixtures/${name}.json`, import.meta.url), "utf8"),
  ) as Fixture;
}

/** The recorded response body (parsed JSON or text). */
export function fixtureBody(name: string): unknown {
  const r = fixture(name).response;
  return r.body ?? r.bodyExcerpt;
}

/**
 * The recorded response as a mock response, with the recorded headers. JSON bodies are sent
 * serialized with the recorded content type (Jaeger sends JSON errors as text/plain).
 */
export function fixtureResponse(name: string): MockResponse {
  const r = fixture(name).response;
  const body = r.body ?? r.bodyExcerpt ?? "";
  return {
    status: r.status,
    headers: { ...r.headers },
    body: typeof body === "string" ? body : JSON.stringify(body),
  };
}
