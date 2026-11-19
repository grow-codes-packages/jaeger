"""Recorded Jaeger v2.23.0 responses (tests/fixtures, from the prep verify run) as mocks."""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import httpx

import grow_jaeger as sdk

FIXTURES = Path(__file__).parent / "fixtures"
TRACE_ID = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1"


def fixture(name: str) -> dict[str, Any]:
    data: dict[str, Any] = json.loads((FIXTURES / f"{name}.json").read_text("utf-8"))
    return data


def body_bytes(body: Any) -> bytes:
    if isinstance(body, str):
        return body.encode()
    # Jaeger ends its JSON error envelopes with a newline (and sends them as text/plain).
    return (json.dumps(body) + "\n").encode()


def response(name: str, **override: Any) -> httpx.Response:
    """The recorded response of fixture ``name`` (status, content type, traceresponse, body)."""
    rec = fixture(name)["response"]
    body = rec.get("body")
    if body is None:
        body = rec.get("bodyExcerpt", "")
    headers = dict(rec.get("headers") or {})
    headers.update(override.pop("headers", {}))
    return httpx.Response(
        override.pop("status", rec["status"]), headers=headers, content=body_bytes(body)
    )


def body(name: str) -> Any:
    return fixture(name)["response"]["body"]


Handler = Callable[[httpx.Request], httpx.Response]


class Router:
    """httpx mock transport: path → queue of responses; records every request."""

    def __init__(self) -> None:
        self.routes: dict[str, list[httpx.Response | Handler]] = {}
        self.seen: list[httpx.Request] = []

    def add(self, path: str, *responses: httpx.Response | Handler) -> Router:
        self.routes.setdefault(path, []).extend(responses)
        return self

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.seen.append(request)
        queue = self.routes.get(request.url.path)
        if not queue:
            return httpx.Response(404, text="404 page not found\n")
        item = queue.pop(0) if len(queue) > 1 else queue[0]
        if callable(item):
            return item(request)
        return httpx.Response(item.status_code, headers=item.headers, content=item.content)

    def transport(self) -> httpx.MockTransport:
        return httpx.MockTransport(self)

    def params(self, path: str) -> dict[str, str]:
        req = next(r for r in self.seen if r.url.path == path)
        return dict(req.url.params)


def no_jitter() -> float:
    return 0.0


def config(url: str = "http://jaeger:16686", **kw: Any) -> sdk.JaegerConfig:
    kw.setdefault(
        "retry", sdk.RetryPolicy(initial_backoff=0.001, max_backoff=0.002, random=no_jitter)
    )
    return sdk.JaegerConfig(url=url, **kw)
