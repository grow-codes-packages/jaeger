// A mock Jaeger query server routed by path, answering with recorded fixtures.

import { fixtureResponse } from "./fixtures.js";
import { type MockResponse, type MockServer, type RecordedRequest, startServer } from "./server.js";

export type Route = string | MockResponse | ((req: RecordedRequest, url: URL) => MockResponse | string);

/** Starts a server; each route maps a pathname to a fixture name, a response or a function. */
export async function jaegerServer(routes: Record<string, Route>): Promise<MockServer> {
  return startServer((req) => {
    const url = new URL(req.url, "http://x");
    const route = routes[url.pathname];
    const resolved = typeof route === "function" ? route(req, url) : route;
    if (resolved === undefined) return fixtureResponse("api.unknown_route");
    return typeof resolved === "string" ? fixtureResponse(resolved) : resolved;
  });
}

/** Query parameters of a recorded request. */
export function params(req: RecordedRequest | undefined): URLSearchParams {
  return new URL(req?.url ?? "/", "http://x").searchParams;
}
