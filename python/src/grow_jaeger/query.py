"""``JaegerQueryClient``: the Jaeger implementation of the SPEC §5 ``TraceQueryClient``.

Reads traces back from Jaeger's query service (port 16686): the v3 HTTP API (``/api/v3/*``)
for traces, search, services and operations, the legacy ``/api/traces/{id}`` as a fallback for
``get_trace`` and the legacy ``/api/dependencies``. Every call is a GET (retried on 5xx).
"""

from __future__ import annotations

import dataclasses
import json
from collections.abc import Callable, Mapping
from datetime import datetime, timedelta
from types import TracebackType
from typing import Any

import httpx

from . import _http
from ._core import (
    AsyncHttpTransport,
    CancelToken,
    HttpTransport,
    NotFoundError,
    Request,
    ValidationError,
    to_seconds,
)
from ._tracing import (
    AsyncBaseTraceQueryClient,
    BaseTraceQueryClient,
    DependencyLink,
    Trace,
    TraceSearchQuery,
    TraceSummary,
    datetime_to_micros,
    normalize_trace_id,
)
from .config import JaegerConfig
from .normalize import (
    dependencies_from_body,
    legacy_to_traces,
    otlp_to_traces,
    summaries_from_body,
)

__all__ = [
    "DEFAULT_DEPENDENCY_LOOKBACK_S",
    "DEFAULT_GET_TRACE_WINDOW_S",
    "DEFAULT_SEARCH_LIMIT",
    "DEFAULT_SEARCH_WINDOW_S",
    "OPERATION_SPAN_KINDS",
    "AsyncJaegerQueryClient",
    "JaegerOperation",
    "JaegerQueryClient",
    "JaegerTraceQuery",
]

#: Default ``get_trace`` lookup window (a storage hint): 7 days before ``end``.
DEFAULT_GET_TRACE_WINDOW_S = 7 * 86_400.0
#: Default search window when the query sets no ``start``: 1 hour before ``end``.
DEFAULT_SEARCH_WINDOW_S = 3_600.0
#: Default dependency lookback: 24 hours.
DEFAULT_DEPENDENCY_LOOKBACK_S = 86_400.0
#: Default ``limit`` of searches (SPEC §5).
DEFAULT_SEARCH_LIMIT = 20
#: Span kinds accepted by ``list_operations(span_kind=…)`` (sent lower-case).
OPERATION_SPAN_KINDS: tuple[str, ...] = ("server", "client", "producer", "consumer", "internal")

Clock = Callable[[], datetime]


@dataclasses.dataclass(frozen=True)
class JaegerTraceQuery(TraceSearchQuery):
    """:class:`TraceSearchQuery` plus ``raw_traces`` (skip Jaeger's span adjusters)."""

    raw_traces: bool | None = None


@dataclasses.dataclass(frozen=True)
class JaegerOperation:
    """One operation of a service as reported by ``/api/v3/operations``."""

    name: str
    #: ``server``, ``client``, ``producer``, ``consumer``, ``internal`` or ``""``.
    span_kind: str = ""


# --------------------------------------------------------------------------- request building


def _pick(traces: list[Trace], trace_id: str) -> Trace | None:
    trace = next((t for t in traces if t.trace_id == trace_id), traces[0] if traces else None)
    return trace if trace is not None and trace.spans else None


def _duration(micros: int | None, what: str) -> str | None:
    if micros is None:
        return None
    if isinstance(micros, bool) or not isinstance(micros, (int, float)) or micros < 0:
        raise ValidationError(f"jaeger.search: {what} must be a non-negative number")
    return f"{round(micros)}us"


def _build_query(query: TraceSearchQuery | None, filters: Mapping[str, Any]) -> TraceSearchQuery:
    if query is None:
        return JaegerTraceQuery(**filters)
    return dataclasses.replace(query, **filters) if filters else query


class _Core:
    """Request building and response decoding shared by the sync and async clients."""

    def __init__(self, config: JaegerConfig, now: Clock | None) -> None:
        self.config = config
        self.now: Clock = now or _http.utc_now

    def window(
        self, start: datetime | None, end: datetime | None, default_s: float, operation: str
    ) -> tuple[datetime, datetime]:
        end = end if end is not None else self.now()
        start = start if start is not None else end - timedelta(seconds=default_s)
        if datetime_to_micros(start) > datetime_to_micros(end):
            raise ValidationError(f"{operation}: start must not be after end")
        return start, end

    def get_trace_requests(
        self,
        trace_id: str,
        start: datetime | None,
        end: datetime | None,
        raw_traces: bool | None,
    ) -> tuple[str, str, Request, Request]:
        tid = normalize_trace_id(trace_id)
        op = f"jaeger.get_trace {tid}"
        start, end = self.window(start, end, DEFAULT_GET_TRACE_WINDOW_S, op)
        v3 = _http.get(
            f"/api/v3/traces/{tid}",
            op,
            self.config,
            params={
                "startTime": _http.rfc3339(start),
                "endTime": _http.rfc3339(end),
                "rawTraces": raw_traces,
            },
        )
        legacy = _http.get(
            f"/api/traces/{tid}",
            op,
            self.config,
            params={
                "start": datetime_to_micros(start),
                "end": datetime_to_micros(end),
                "raw": raw_traces,
            },
        )
        return tid, op, v3, legacy

    def search_request(self, path: str, operation: str, query: TraceSearchQuery) -> Request:
        start, end = self.window(query.start, query.end, DEFAULT_SEARCH_WINDOW_S, operation)
        limit = query.limit
        if isinstance(limit, bool) or not isinstance(limit, int) or limit <= 0:
            raise ValidationError(f"{operation}: limit must be a positive integer")
        tags = {str(k): str(v) for k, v in (query.tags or {}).items()}
        raw = getattr(query, "raw_traces", None)
        params: dict[str, Any] = {
            "query.serviceName": query.service or None,
            "query.operationName": query.operation or None,
            "query.attributes": json.dumps(tags, separators=(",", ":")) if tags else None,
            "query.durationMin": _duration(query.min_duration_micros, "min_duration_micros"),
            "query.durationMax": _duration(query.max_duration_micros, "max_duration_micros"),
            "query.startTimeMin": _http.rfc3339(start),
            "query.startTimeMax": _http.rfc3339(end),
            "query.searchDepth": limit,
            "query.rawTraces": raw,
        }
        return _http.get(path, operation, self.config, params=params)

    def operations_request(self, service: str, span_kind: str | None) -> Request:
        op = "jaeger.list_operations"
        if not isinstance(service, str) or not service.strip():
            raise ValidationError(f"{op}: service is required")
        kind = span_kind.lower() if span_kind is not None else None
        if kind is not None and kind not in OPERATION_SPAN_KINDS:
            raise ValidationError(f"{op}: unknown span_kind")
        return _http.get(
            "/api/v3/operations", op, self.config, params={"service": service, "spanKind": kind}
        )

    def dependencies_request(
        self,
        end_ts: datetime | None,
        lookback_s: float | timedelta | None,
        start: datetime | None,
        end: datetime | None,
        service: str | None,
    ) -> Request:
        op = "jaeger.get_dependencies"
        end_at = end_ts or end or self.now()
        if lookback_s is not None:
            lookback = to_seconds(lookback_s)
        elif start is not None:
            lookback = (datetime_to_micros(end_at) - datetime_to_micros(start)) / 1e6
        else:
            lookback = DEFAULT_DEPENDENCY_LOOKBACK_S
        if not lookback > 0:
            raise ValidationError(f"{op}: lookback must be a positive duration")
        return _http.get(
            "/api/dependencies",
            op,
            self.config,
            params={
                "endTs": datetime_to_micros(end_at) // 1000,
                "lookback": round(lookback * 1000),
                "service": service or None,
            },
        )

    # ---------------------------------------------------------------- decoding

    def v3_trace(self, resp: Any, tid: str, op: str) -> tuple[bool, Trace | None]:
        """``(done, trace)``; ``done=False`` means: try the legacy route."""
        outcome = _http.classify(resp, op, self.config)
        if outcome == "not_found":
            return True, None
        if outcome == "route_missing":
            return False, None
        body = _http.as_object(resp.json(), op)
        return True, _pick(otlp_to_traces(body.get("result")), tid)

    def legacy_trace(self, resp: Any, tid: str, op: str) -> Trace | None:
        outcome = _http.classify(resp, op, self.config)
        if outcome == "not_found":
            return None
        if outcome == "route_missing":
            raise _http.route_missing(op, "/api/v3/traces or /api/traces")
        return _pick(legacy_to_traces(resp.json()), tid)

    def traces(self, resp: Any, op: str) -> list[Trace]:
        outcome = _http.classify(resp, op, self.config)
        if outcome == "not_found":
            return []  # Jaeger answers "no match" with a 404 envelope on /api/v3/traces
        if outcome == "route_missing":
            raise _http.route_missing(op, "/api/v3/traces")
        return otlp_to_traces(_http.as_object(resp.json(), op).get("result"))

    def summaries(self, resp: Any, op: str) -> list[TraceSummary]:
        outcome = _http.classify(resp, op, self.config)
        if outcome == "not_found":
            return []
        if outcome == "route_missing":
            raise _http.route_missing(op, "/api/v3/trace-summaries")
        return summaries_from_body(resp.json())

    def services(self, resp: Any) -> list[str]:
        op = "jaeger.list_services"
        body = _http.as_object(_http.expect_json(resp, op, self.config, "/api/v3/services"), op)
        services = body.get("services")
        if not isinstance(services, list):
            return []
        return sorted(s for s in services if isinstance(s, str))

    def operations(self, resp: Any) -> list[JaegerOperation]:
        op = "jaeger.list_operations"
        body = _http.as_object(_http.expect_json(resp, op, self.config, "/api/v3/operations"), op)
        items = body.get("operations")
        out = []
        for item in items if isinstance(items, list) else []:
            if isinstance(item, Mapping) and isinstance(item.get("name"), str):
                kind = item.get("spanKind")
                out.append(JaegerOperation(item["name"], kind if isinstance(kind, str) else ""))
        return out

    def dependencies(self, resp: Any) -> list[DependencyLink]:
        op = "jaeger.get_dependencies"
        return dependencies_from_body(_http.expect_json(resp, op, self.config, "/api/dependencies"))


def _names(operations: list[JaegerOperation]) -> list[str]:
    return sorted({o.name for o in operations})


def _not_found(trace_id: str) -> NotFoundError:
    return NotFoundError(f"jaeger.get_trace {normalize_trace_id(trace_id)}: trace not found")


# --------------------------------------------------------------------------- sync client


class JaegerQueryClient(BaseTraceQueryClient):
    """Synchronous trace query client (implements :class:`~grow_jaeger.TraceQueryClient`).

    Get one from :attr:`JaegerClient.query <grow_jaeger.JaegerClient.query>` (shares the
    client's connections) or construct it from a config (owns its connections; call
    :meth:`close` or use it as a context manager). Every method accepts ``cancel=``.
    """

    def __init__(
        self,
        config: JaegerConfig | None = None,
        *,
        http_transport: httpx.BaseTransport | None = None,
        now: Clock | None = None,
        _transport: HttpTransport | None = None,
    ) -> None:
        self.config = (config if config is not None else JaegerConfig.from_env()).validate()
        self._core = _Core(self.config, now)
        self._owned = _transport is None
        self._transport = _transport or HttpTransport(
            self.config, known_secrets=self.config.known_secrets, http_transport=http_transport
        )

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None, **overrides: Any) -> JaegerQueryClient:
        """Standalone query client from ``GROW_JAEGER_*`` / ``JAEGER_QUERY_URL``."""
        return cls(JaegerConfig.from_env(env, **overrides))

    def _send(self, req: Request, cancel: CancelToken | None) -> Any:
        return self._transport.request(req, cancel=cancel)

    def get_trace(
        self,
        trace_id: str,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        raw_traces: bool | None = None,
        cancel: CancelToken | None = None,
    ) -> Trace | None:
        """One trace: ``GET /api/v3/traces/{id}``, falling back to the legacy
        ``GET /api/traces/{id}`` when the v3 route is missing. ``None`` when the trace does
        not exist.

        ``trace_id`` is 32 or 16 hex characters (16 is left-padded); anything else raises
        :class:`ValidationError` without a request. ``start``/``end`` form the lookup window
        (a storage hint; default: the 7 days before now).
        """
        tid, op, v3, legacy = self._core.get_trace_requests(trace_id, start, end, raw_traces)
        done, trace = self._core.v3_trace(self._send(v3, cancel), tid, op)
        if done:
            return trace
        return self._core.legacy_trace(self._send(legacy, cancel), tid, op)

    def require_trace(
        self,
        trace_id: str,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        raw_traces: bool | None = None,
        cancel: CancelToken | None = None,
    ) -> Trace:
        """Like :meth:`get_trace` but raises :class:`NotFoundError` for a missing trace."""
        trace = self.get_trace(trace_id, start=start, end=end, raw_traces=raw_traces, cancel=cancel)
        if trace is None:
            raise _not_found(trace_id)
        return trace

    def find_traces(
        self,
        query: TraceSearchQuery | None = None,
        *,
        cancel: CancelToken | None = None,
        **filters: Any,
    ) -> list[Trace]:
        """Full traces matching a search: ``GET /api/v3/traces``.

        Pass a :class:`TraceSearchQuery` / :class:`JaegerTraceQuery` or its fields as keywords
        (``service``, ``operation``, ``tags``, ``min_duration_micros``, ``max_duration_micros``,
        ``start``, ``end``, ``limit``, ``raw_traces``). Both time bounds are always sent
        (default: the last hour); ``limit`` (default 20) caps the number of traces. No match
        returns ``[]``.
        """
        q = _build_query(query, filters)
        op = "jaeger.find_traces"
        req = self._core.search_request("/api/v3/traces", op, q)
        return self._core.traces(self._send(req, cancel), op)

    def search_traces(
        self,
        query: TraceSearchQuery | None = None,
        *,
        cancel: CancelToken | None = None,
        **filters: Any,
    ) -> list[TraceSummary]:
        """Trace summaries: ``GET /api/v3/trace-summaries`` (same filter as
        :meth:`find_traces`)."""
        q = _build_query(query, filters)
        op = "jaeger.search_traces"
        req = self._core.search_request("/api/v3/trace-summaries", op, q)
        return self._core.summaries(self._send(req, cancel), op)

    def list_services(self, *, cancel: CancelToken | None = None) -> list[str]:
        """Service names that reported spans: ``GET /api/v3/services`` (sorted)."""
        req = _http.get("/api/v3/services", "jaeger.list_services", self.config)
        return self._core.services(self._send(req, cancel))

    def get_operations(
        self,
        service: str,
        *,
        span_kind: str | None = None,
        cancel: CancelToken | None = None,
    ) -> list[JaegerOperation]:
        """Operations of a service with their span kind: ``GET /api/v3/operations``.

        ``span_kind`` (``server``, ``client``, ``producer``, ``consumer``, ``internal``,
        case-insensitive) is sent lower-case. An empty ``service`` raises
        :class:`ValidationError` without a request.
        """
        req = self._core.operations_request(service, span_kind)
        return self._core.operations(self._send(req, cancel))

    def list_operations(
        self,
        service: str,
        *,
        span_kind: str | None = None,
        cancel: CancelToken | None = None,
    ) -> list[str]:
        """Operation names of a service, de-duplicated across span kinds and sorted."""
        return _names(self.get_operations(service, span_kind=span_kind, cancel=cancel))

    def get_dependencies(
        self,
        end_ts: datetime | None = None,
        lookback_s: float | timedelta | None = None,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        service: str | None = None,
        cancel: CancelToken | None = None,
    ) -> list[DependencyLink]:
        """Service dependency edges: legacy ``GET /api/dependencies?endTs=&lookback=``.

        Either Jaeger's native ``end_ts`` + ``lookback_s`` (these win) or the category window
        ``start``/``end``. Defaults: end now, lookback 24 h. ``service`` keeps only the edges
        that touch it. The memory store derives edges on the fly; Cassandra/Elasticsearch/
        OpenSearch need the ``spark-dependencies`` job, so ``[]`` is normal there.
        """
        req = self._core.dependencies_request(end_ts, lookback_s, start, end, service)
        return self._core.dependencies(self._send(req, cancel))

    def close(self) -> None:
        """Release connections when this client owns them (no-op for ``client.query``)."""
        if self._owned:
            self._transport.close()

    def __enter__(self) -> JaegerQueryClient:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"JaegerQueryClient(config={self.config!r})"


# --------------------------------------------------------------------------- async client


class AsyncJaegerQueryClient(AsyncBaseTraceQueryClient):
    """Asynchronous :class:`JaegerQueryClient` (implements
    :class:`~grow_jaeger.AsyncTraceQueryClient`); cancel a call by cancelling its task."""

    def __init__(
        self,
        config: JaegerConfig | None = None,
        *,
        http_transport: httpx.AsyncBaseTransport | None = None,
        now: Clock | None = None,
        _transport: AsyncHttpTransport | None = None,
    ) -> None:
        self.config = (config if config is not None else JaegerConfig.from_env()).validate()
        self._core = _Core(self.config, now)
        self._owned = _transport is None
        self._transport = _transport or AsyncHttpTransport(
            self.config, known_secrets=self.config.known_secrets, http_transport=http_transport
        )

    @classmethod
    def from_env(
        cls, env: Mapping[str, str] | None = None, **overrides: Any
    ) -> AsyncJaegerQueryClient:
        """Standalone async query client from the environment."""
        return cls(JaegerConfig.from_env(env, **overrides))

    async def get_trace(
        self,
        trace_id: str,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        raw_traces: bool | None = None,
    ) -> Trace | None:
        """See :meth:`JaegerQueryClient.get_trace`."""
        tid, op, v3, legacy = self._core.get_trace_requests(trace_id, start, end, raw_traces)
        done, trace = self._core.v3_trace(await self._transport.request(v3), tid, op)
        if done:
            return trace
        return self._core.legacy_trace(await self._transport.request(legacy), tid, op)

    async def require_trace(
        self,
        trace_id: str,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        raw_traces: bool | None = None,
    ) -> Trace:
        """See :meth:`JaegerQueryClient.require_trace`."""
        trace = await self.get_trace(trace_id, start=start, end=end, raw_traces=raw_traces)
        if trace is None:
            raise _not_found(trace_id)
        return trace

    async def find_traces(
        self, query: TraceSearchQuery | None = None, **filters: Any
    ) -> list[Trace]:
        """See :meth:`JaegerQueryClient.find_traces`."""
        op = "jaeger.find_traces"
        req = self._core.search_request("/api/v3/traces", op, _build_query(query, filters))
        return self._core.traces(await self._transport.request(req), op)

    async def search_traces(
        self, query: TraceSearchQuery | None = None, **filters: Any
    ) -> list[TraceSummary]:
        """See :meth:`JaegerQueryClient.search_traces`."""
        op = "jaeger.search_traces"
        req = self._core.search_request("/api/v3/trace-summaries", op, _build_query(query, filters))
        return self._core.summaries(await self._transport.request(req), op)

    async def list_services(self) -> list[str]:
        """See :meth:`JaegerQueryClient.list_services`."""
        req = _http.get("/api/v3/services", "jaeger.list_services", self.config)
        return self._core.services(await self._transport.request(req))

    async def get_operations(
        self, service: str, *, span_kind: str | None = None
    ) -> list[JaegerOperation]:
        """See :meth:`JaegerQueryClient.get_operations`."""
        req = self._core.operations_request(service, span_kind)
        return self._core.operations(await self._transport.request(req))

    async def list_operations(self, service: str, *, span_kind: str | None = None) -> list[str]:
        """See :meth:`JaegerQueryClient.list_operations`."""
        return _names(await self.get_operations(service, span_kind=span_kind))

    async def get_dependencies(
        self,
        end_ts: datetime | None = None,
        lookback_s: float | timedelta | None = None,
        *,
        start: datetime | None = None,
        end: datetime | None = None,
        service: str | None = None,
    ) -> list[DependencyLink]:
        """See :meth:`JaegerQueryClient.get_dependencies`."""
        req = self._core.dependencies_request(end_ts, lookback_s, start, end, service)
        return self._core.dependencies(await self._transport.request(req))

    async def aclose(self) -> None:
        """Release connections when this client owns them."""
        if self._owned:
            await self._transport.aclose()

    close = aclose

    async def __aenter__(self) -> AsyncJaegerQueryClient:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.aclose()

    def __repr__(self) -> str:
        return f"AsyncJaegerQueryClient(config={self.config!r})"
