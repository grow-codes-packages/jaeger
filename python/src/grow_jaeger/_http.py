"""Service-private HTTP helpers: per-origin transports and Jaeger response classification.

The generated transport joins a path to one base URL. Jaeger needs several endpoints (query,
sampling, healthcheckv2, metrics), so the client keeps one transport per origin.
"""

from __future__ import annotations

import dataclasses
import json
import threading
from collections.abc import Mapping
from datetime import datetime, timezone
from typing import Any, Literal
from urllib.parse import urlsplit

import httpx

from ._core import (
    AsyncHttpTransport,
    HttpTransport,
    Request,
    Response,
    ServiceError,
    UnsupportedError,
    ValidationError,
    error_from_status,
    parse_retry_after,
)
from .config import JaegerConfig

__all__: list[str] = []

Outcome = Literal["ok", "not_found", "route_missing"]

#: Jaeger answers every query with its own server span; used as the error ``request_id``.
TRACERESPONSE = "traceresponse"


def split_url(url: str) -> tuple[str, str]:
    """``("scheme://host:port", "/path?query")`` for an absolute URL."""
    parts = urlsplit(url)
    path = parts.path or "/"
    if parts.query:
        path += "?" + parts.query
    return f"{parts.scheme}://{parts.netloc}", path


def auth_headers(config: JaegerConfig) -> dict[str, str]:
    """``Authorization: Bearer`` when a token is configured."""
    if config.token is None:
        return {}
    return {"Authorization": f"Bearer {config.token.reveal()}"}


def get(path: str, operation: str, config: JaegerConfig, **kw: Any) -> Request:
    """A GET request that returns error responses for :func:`classify` (retries still apply)."""
    return Request(
        "GET",
        path,
        operation=operation,
        headers=auth_headers(config),
        raise_for_status=False,
        **kw,
    )


def is_json_envelope(content: bytes) -> bool:
    """True for Jaeger's JSON error envelope (v3 ``{"error":…}``, legacy ``{"errors":…}``).

    A 404 with this body means "no such record"; any other 404 body (``404 page not found``,
    a proxy page) means the route does not exist.
    """
    try:
        doc = json.loads(content)
    except ValueError:
        return False
    return isinstance(doc, dict) and ("error" in doc or "errors" in doc)


def status_error(resp: Response, operation: str, config: JaegerConfig) -> ServiceError:
    """SPEC error for an error response; ``request_id`` falls back to ``traceresponse``."""
    retry_after = None
    if resp.status in (429, 503):
        retry_after = parse_retry_after(resp.headers.get("retry-after"))
    err = error_from_status(
        resp.status,
        operation,
        content=resp.content,
        headers=resp.headers,
        known_secrets=config.known_secrets(),
        retry_after=retry_after,
    )
    if err.request_id is None:
        err.request_id = resp.headers.get(TRACERESPONSE) or None
    return err


def classify(resp: Response, operation: str, config: JaegerConfig) -> Outcome:
    """``ok`` for 2xx, ``not_found``/``route_missing`` for 404; raises for anything else."""
    if 200 <= resp.status < 300:
        return "ok"
    if resp.status == 404:
        return "not_found" if is_json_envelope(resp.content) else "route_missing"
    raise status_error(resp, operation, config)


def route_missing(operation: str, route: str) -> UnsupportedError:
    return UnsupportedError(f"{operation}: the server has no {route} route", http_status=404)


def expect_json(resp: Response, operation: str, config: JaegerConfig, route: str) -> Any:
    """Decoded body of a 2xx response; ``NotFoundError`` for the JSON 404 envelope,
    ``UnsupportedError`` for a missing route, mapped errors otherwise."""
    outcome = classify(resp, operation, config)
    if outcome == "route_missing":
        raise route_missing(operation, route)
    if outcome == "not_found":
        raise status_error(resp, operation, config)
    return resp.json()


def as_object(body: Any, operation: str) -> Mapping[str, Any]:
    if not isinstance(body, Mapping):
        raise ValidationError(f"{operation}: unexpected response shape")
    return body


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def rfc3339(value: datetime) -> str:
    """RFC 3339 UTC with microseconds (naive datetimes are UTC)."""
    if value.tzinfo is None:
        value = value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


# --------------------------------------------------------------------------- transport pools


class TransportPool:
    """One synchronous transport per origin, all with the client's settings."""

    def __init__(
        self,
        config: JaegerConfig,
        *,
        http_transport: httpx.BaseTransport | None = None,
        main: HttpTransport | None = None,
    ) -> None:
        self.config = config
        self._http_transport = http_transport
        self._lock = threading.Lock()
        self._owned: dict[str, HttpTransport] = {}
        self.main = main or self._make(config.url)
        self._owns_main = main is None

    def _make(self, base: str) -> HttpTransport:
        cfg = dataclasses.replace(self.config, url=base)
        return HttpTransport(
            cfg, known_secrets=cfg.known_secrets, http_transport=self._http_transport
        )

    def for_url(self, url: str) -> tuple[HttpTransport, str]:
        """Transport and path for an absolute URL."""
        base, path = split_url(url)
        with self._lock:
            transport = self._owned.get(base)
            if transport is None:
                transport = self._owned[base] = self._make(base)
        return transport, path

    def close(self) -> None:
        with self._lock:
            owned, self._owned = list(self._owned.values()), {}
        for transport in owned:
            transport.close()
        if self._owns_main:
            self.main.close()


class AsyncTransportPool:
    """One asynchronous transport per origin, all with the client's settings."""

    def __init__(
        self,
        config: JaegerConfig,
        *,
        http_transport: httpx.AsyncBaseTransport | None = None,
        main: AsyncHttpTransport | None = None,
    ) -> None:
        self.config = config
        self._http_transport = http_transport
        self._owned: dict[str, AsyncHttpTransport] = {}
        self.main = main or self._make(config.url)
        self._owns_main = main is None

    def _make(self, base: str) -> AsyncHttpTransport:
        cfg = dataclasses.replace(self.config, url=base)
        return AsyncHttpTransport(
            cfg, known_secrets=cfg.known_secrets, http_transport=self._http_transport
        )

    def for_url(self, url: str) -> tuple[AsyncHttpTransport, str]:
        base, path = split_url(url)
        transport = self._owned.get(base)
        if transport is None:
            transport = self._owned[base] = self._make(base)
        return transport, path

    async def aclose(self) -> None:
        owned, self._owned = list(self._owned.values()), {}
        for transport in owned:
            await transport.aclose()
        if self._owns_main:
            await self.main.aclose()
