"""Jaeger clients (sync and async).

``JaegerClient`` covers the shared client surface (health, ping, server version, close) of
the query service and hangs the Jaeger modules off it: :attr:`~JaegerClient.query` (trace
query API), :meth:`~JaegerClient.init_tracing` (OTLP export) and
:meth:`~JaegerClient.remote_sampler` (remote sampling).
"""

from __future__ import annotations

import json
import re
import time
from collections.abc import Mapping
from datetime import timedelta
from types import TracebackType
from typing import Any

import httpx

from . import _http
from ._core import (
    CancelledError,
    CancelToken,
    ConfigError,
    HealthStatus,
    Request,
    ResourceRegistry,
    Response,
    ServiceError,
    TimeoutError,
    UnavailableError,
    UnsupportedError,
    ValidationError,
)
from ._tracing import TracingConfig
from .config import JaegerConfig
from .query import AsyncJaegerQueryClient, JaegerQueryClient
from .sampler import (
    DEFAULT_INITIAL_SAMPLING_RATE,
    DEFAULT_SAMPLING_POLL_INTERVAL_S,
    JaegerRemoteSampler,
    JaegerSamplingStrategy,
    parse_sampling_strategy,
)
from .setup import JaegerTracingHandle, init_tracing

__all__ = ["AsyncJaegerClient", "JaegerBuildInfo", "JaegerClient"]

_VERSION_RE = re.compile(r"JAEGER_VERSION\s*=\s*(\{[^;]*?\})\s*;")
_TARGET_INFO_RE = re.compile(r'^target_info\{[^}]*\bservice_version="v?([^"]+)"', re.MULTILINE)
_HEALTHY_WORDS = frozenset(("ok", "healthy", "up", "pass", "statusok", "serving"))

JaegerBuildInfo = dict[str, str]
"""Build information embedded in the Jaeger UI (``gitVersion``, ``gitCommit``, ``buildDate``)."""


def _strip_v(version: str) -> str:
    return version[1:] if version[:1] == "v" and version[1:2].isdigit() else version


def _parse_build_info(resp: Response) -> JaegerBuildInfo | None:
    if not resp.ok:
        return None
    match = _VERSION_RE.search(resp.text)
    if match is None:
        return None
    try:
        parsed = json.loads(match.group(1))
    except ValueError:
        return None
    if not isinstance(parsed, Mapping):
        return None
    return {
        k: parsed[k]
        for k in ("gitVersion", "gitCommit", "buildDate")
        if isinstance(parsed.get(k), str) and parsed[k]
    }


def _version_from_metrics(resp: Response, config: JaegerConfig) -> str | None:
    """The version in ``target_info``; ``None`` on a 404 or without the series."""
    if resp.status == 404:
        return None
    if not resp.ok:
        raise _http.status_error(resp, "jaeger.server_version", config)
    match = _TARGET_INFO_RE.search(resp.text)
    return _strip_v(match.group(1)) if match else None


def _unsupported_version() -> UnsupportedError:
    return UnsupportedError("jaeger.server_version: the service does not report a version")


def _root_health(resp: Response) -> HealthStatus | None:
    """``None`` when the query root is healthy, else the unhealthy status."""
    if resp.ok:
        return None
    return HealthStatus(False, "unhealthy", {"httpStatus": str(resp.status)})


def _detail_health(resp: Response) -> HealthStatus:
    """Status from a healthcheckv2 ``/status`` answer."""
    try:
        body: Any = json.loads(resp.content)
    except ValueError:
        body = None
    body = body if isinstance(body, Mapping) else {}
    word = body.get("status") if isinstance(body.get("status"), str) else None
    if isinstance(body.get("healthy"), bool):
        healthy = body["healthy"] and resp.ok
    elif word is not None:
        healthy = resp.ok and word.lower() in _HEALTHY_WORDS
    else:
        healthy = resp.ok
    details: dict[str, str] = {"status": word or ("ok" if resp.ok else "unhealthy")}
    for src, dst in (("start_time", "startTime"), ("status_time", "statusTime")):
        if isinstance(body.get(src), str):
            details[dst] = body[src]
    if not resp.ok:
        details["httpStatus"] = str(resp.status)
    status = "ok" if healthy else (word or "unhealthy")
    return HealthStatus(healthy, status, details)


def _health_from_error(err: ServiceError) -> HealthStatus:
    if isinstance(err, (CancelledError, ConfigError)):
        raise err
    word = "unreachable" if isinstance(err, (UnavailableError, TimeoutError)) else "unhealthy"
    return HealthStatus(healthy=False, status=word, details={"error": err.message})


def _health_req(path: str, config: JaegerConfig) -> Request:
    return _http.get(path, "jaeger.health", config, auth=False, max_retries=0)


def _ping_req(config: JaegerConfig) -> Request:
    return _http.get("/api/v3/services", "jaeger.ping", config)


def _sampling_req(path: str, service: str, config: JaegerConfig) -> Request:
    if not isinstance(service, str) or not service.strip():
        raise ValidationError("jaeger.sampling: service is required")
    return _http.get(path, f"jaeger.sampling {service}", config, params={"service": service})


def _sampling_strategy(
    resp: Response, service: str, config: JaegerConfig
) -> JaegerSamplingStrategy:
    if not resp.ok:
        raise _http.status_error(resp, f"jaeger.sampling {service}", config)
    return parse_sampling_strategy(resp.json())


def _ping_check(resp: Response, config: JaegerConfig) -> None:
    _http.expect_json(resp, "jaeger.ping", config, "/api/v3/services")


class JaegerClient:
    """Synchronous Jaeger client. Thread-safe; use as a context manager or call :meth:`close`.

    Every I/O method accepts ``cancel=`` (a :class:`CancelToken`) to abort from another thread.
    :meth:`close` also shuts down every tracing handle and remote sampler created through the
    client.
    """

    def __init__(
        self,
        config: JaegerConfig | None = None,
        *,
        http_transport: httpx.BaseTransport | None = None,
    ) -> None:
        if config is None:
            config = JaegerConfig.from_env()
        self.config = config.validate()
        self._http_transport = http_transport
        self._resources = ResourceRegistry()
        self._pool = _http.TransportPool(self.config, http_transport=http_transport)
        self._query: JaegerQueryClient | None = None

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None, **overrides: Any) -> JaegerClient:
        """Build a client from ``GROW_JAEGER_*`` and the vendor variables (``JAEGER_QUERY_URL``,
        ``JAEGER_SAMPLING_ENDPOINT``)."""
        return cls(JaegerConfig.from_env(env, **overrides))

    @property
    def resources(self) -> ResourceRegistry:
        """Background resources (tracing handles, samplers) stopped by :meth:`close`."""
        return self._resources

    @property
    def query(self) -> JaegerQueryClient:
        """The trace query client (shares this client's connections)."""
        if self._query is None:
            self._query = JaegerQueryClient(self.config, _transport=self._pool.main)
        return self._query

    def request(self, req: Request, *, cancel: CancelToken | None = None) -> Response:
        """Send a raw request to the query API (retries, hooks, redaction)."""
        return self._pool.main.request(req, cancel=cancel)

    def health(self, *, cancel: CancelToken | None = None) -> HealthStatus:
        """Liveness of the query service: ``GET /`` (the stack catalog health target).

        With ``health_url`` set, the healthcheckv2 ``/status`` answer decides and adds detail.
        Never raises for an unhealthy or unreachable service (only ``CANCELLED``/``CONFIG``).
        """
        try:
            resp = self._pool.main.request(_health_req("/", self.config), cancel=cancel)
            status = _root_health(resp)
            if status is not None:
                return status
            if self.config.health_url is None:
                return HealthStatus(True, "ok", {})
            transport, path = self._pool.for_url(self.config.health_url)
            return _detail_health(transport.request(_health_req(path, self.config), cancel=cancel))
        except ServiceError as err:
            return _health_from_error(err)

    def ping(self, *, cancel: CancelToken | None = None) -> float:
        """Round trip through the query API (``GET /api/v3/services``); latency in ms."""
        started = time.monotonic()
        _ping_check(self._pool.main.request(_ping_req(self.config), cancel=cancel), self.config)
        return (time.monotonic() - started) * 1000.0

    def build_info(self, *, cancel: CancelToken | None = None) -> JaegerBuildInfo | None:
        """Build information parsed from the UI page (``GET /``); ``None`` when absent."""
        req = _http.get("/", "jaeger.server_version", self.config)
        resp = self._pool.main.request(req, cancel=cancel)
        if not resp.ok and resp.status != 404:
            raise _http.status_error(resp, "jaeger.server_version", self.config)
        return _parse_build_info(resp)

    def server_version(self, *, cancel: CancelToken | None = None) -> str:
        """Server version without the leading ``v`` (``2.23.0``).

        Parsed from ``JAEGER_VERSION`` in the UI page, else from ``target_info`` at
        ``metrics_url``. :class:`UnsupportedError` when neither reports one.
        """
        info = self.build_info(cancel=cancel)
        if info and "gitVersion" in info:
            return _strip_v(info["gitVersion"])
        if self.config.metrics_url is not None:
            transport, path = self._pool.for_url(self.config.metrics_url)
            resp = transport.request(
                _http.get(path, "jaeger.server_version", self.config), cancel=cancel
            )
            version = _version_from_metrics(resp, self.config)
            if version:
                return version
        raise _unsupported_version()

    def sampling_strategy(
        self, service: str, *, cancel: CancelToken | None = None
    ) -> JaegerSamplingStrategy:
        """The remote sampling strategy of ``service`` (one ``GET {sampling_url}?service=``)."""
        transport, path = self._pool.for_url(self.config.sampling_url)
        resp = transport.request(_sampling_req(path, service, self.config), cancel=cancel)
        return _sampling_strategy(resp, service, self.config)

    def init_tracing(
        self, config: TracingConfig | None = None, **kwargs: Any
    ) -> JaegerTracingHandle:
        """Start OpenTelemetry tracing to Jaeger (see :func:`grow_jaeger.init_tracing`). The
        handle is shut down by :meth:`close`."""
        if self._resources.closed:
            raise ConfigError("jaeger.init_tracing: client is closed")
        kwargs.setdefault("sampler_config", self.config)
        handle = init_tracing(config, **kwargs)
        self._resources.register(handle)
        return handle

    def remote_sampler(
        self,
        service: str,
        poll_interval_s: float | timedelta = DEFAULT_SAMPLING_POLL_INTERVAL_S,
        *,
        sampling_url: str | None = None,
        initial_sampling_rate: float = DEFAULT_INITIAL_SAMPLING_RATE,
        **kwargs: Any,
    ) -> JaegerRemoteSampler:
        """A :class:`JaegerRemoteSampler` with this client's settings (``sampling_url``,
        timeouts, TLS, proxy, headers, token); closed by :meth:`close`."""
        if self._resources.closed:
            raise ConfigError("jaeger.remote_sampler: client is closed")
        kwargs.setdefault("http_transport", self._http_transport)
        sampler = JaegerRemoteSampler(
            sampling_url,
            service,
            poll_interval_s,
            initial_sampling_rate=initial_sampling_rate,
            config=self.config,
            **kwargs,
        )
        self._resources.register(sampler)
        return sampler

    def close(self) -> None:
        """Shut down tracing handles (flushing them) and samplers, close connections
        (idempotent)."""
        self._resources.stop_all()
        self._pool.close()

    def __enter__(self) -> JaegerClient:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"JaegerClient(config={self.config!r})"


class AsyncJaegerClient:
    """Asynchronous Jaeger client; use ``async with`` or call :meth:`aclose`.

    Cancel a call by cancelling its task: the SDK raises :class:`CancelledError`, which is also
    an :class:`asyncio.CancelledError`. Tracing handles and remote samplers are thread based
    and shared with the sync API.
    """

    def __init__(
        self,
        config: JaegerConfig | None = None,
        *,
        http_transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        if config is None:
            config = JaegerConfig.from_env()
        self.config = config.validate()
        self._resources = ResourceRegistry()
        self._pool = _http.AsyncTransportPool(self.config, http_transport=http_transport)
        self._query: AsyncJaegerQueryClient | None = None

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None, **overrides: Any) -> AsyncJaegerClient:
        """Build a client from the environment (see :meth:`JaegerClient.from_env`)."""
        return cls(JaegerConfig.from_env(env, **overrides))

    @property
    def resources(self) -> ResourceRegistry:
        """Background resources stopped by :meth:`aclose`."""
        return self._resources

    @property
    def query(self) -> AsyncJaegerQueryClient:
        """The async trace query client (shares this client's connections)."""
        if self._query is None:
            self._query = AsyncJaegerQueryClient(self.config, _transport=self._pool.main)
        return self._query

    async def request(self, req: Request) -> Response:
        """Send a raw request to the query API."""
        return await self._pool.main.request(req)

    async def health(self) -> HealthStatus:
        """See :meth:`JaegerClient.health`."""
        try:
            resp = await self._pool.main.request(_health_req("/", self.config))
            status = _root_health(resp)
            if status is not None:
                return status
            if self.config.health_url is None:
                return HealthStatus(True, "ok", {})
            transport, path = self._pool.for_url(self.config.health_url)
            return _detail_health(await transport.request(_health_req(path, self.config)))
        except ServiceError as err:
            return _health_from_error(err)

    async def ping(self) -> float:
        """See :meth:`JaegerClient.ping`."""
        started = time.monotonic()
        _ping_check(await self._pool.main.request(_ping_req(self.config)), self.config)
        return (time.monotonic() - started) * 1000.0

    async def build_info(self) -> JaegerBuildInfo | None:
        """See :meth:`JaegerClient.build_info`."""
        resp = await self._pool.main.request(_http.get("/", "jaeger.server_version", self.config))
        if not resp.ok and resp.status != 404:
            raise _http.status_error(resp, "jaeger.server_version", self.config)
        return _parse_build_info(resp)

    async def server_version(self) -> str:
        """See :meth:`JaegerClient.server_version`."""
        info = await self.build_info()
        if info and "gitVersion" in info:
            return _strip_v(info["gitVersion"])
        if self.config.metrics_url is not None:
            transport, path = self._pool.for_url(self.config.metrics_url)
            resp = await transport.request(_http.get(path, "jaeger.server_version", self.config))
            version = _version_from_metrics(resp, self.config)
            if version:
                return version
        raise _unsupported_version()

    async def sampling_strategy(self, service: str) -> JaegerSamplingStrategy:
        """See :meth:`JaegerClient.sampling_strategy`."""
        transport, path = self._pool.for_url(self.config.sampling_url)
        resp = await transport.request(_sampling_req(path, service, self.config))
        return _sampling_strategy(resp, service, self.config)

    def init_tracing(
        self, config: TracingConfig | None = None, **kwargs: Any
    ) -> JaegerTracingHandle:
        """See :meth:`JaegerClient.init_tracing`."""
        if self._resources.closed:
            raise ConfigError("jaeger.init_tracing: client is closed")
        kwargs.setdefault("sampler_config", self.config)
        handle = init_tracing(config, **kwargs)
        self._resources.register(handle)
        return handle

    def remote_sampler(
        self,
        service: str,
        poll_interval_s: float | timedelta = DEFAULT_SAMPLING_POLL_INTERVAL_S,
        *,
        sampling_url: str | None = None,
        initial_sampling_rate: float = DEFAULT_INITIAL_SAMPLING_RATE,
        **kwargs: Any,
    ) -> JaegerRemoteSampler:
        """See :meth:`JaegerClient.remote_sampler`."""
        if self._resources.closed:
            raise ConfigError("jaeger.remote_sampler: client is closed")
        sampler = JaegerRemoteSampler(
            sampling_url,
            service,
            poll_interval_s,
            initial_sampling_rate=initial_sampling_rate,
            config=self.config,
            **kwargs,
        )
        self._resources.register(sampler)
        return sampler

    async def aclose(self) -> None:
        """Shut down tracing handles and samplers, close connections (idempotent)."""
        self._resources.stop_all()
        await self._pool.aclose()

    close = aclose

    async def __aenter__(self) -> AsyncJaegerClient:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.aclose()

    def __repr__(self) -> str:
        return f"AsyncJaegerClient(config={self.config!r})"
