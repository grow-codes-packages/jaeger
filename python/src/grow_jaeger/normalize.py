"""Convert Jaeger query responses into the normalized SPEC §5 trace model.

Two wire formats exist: OTLP/JSON ``TracesData`` from ``/api/v3/*`` and the legacy UI model
from ``/api/traces/{id}``. Summaries come from ``/api/v3/trace-summaries`` and dependency
links from the legacy ``/api/dependencies``. Every function here is pure (no I/O).
"""

from __future__ import annotations

import math
from collections.abc import Mapping
from typing import Any

from ._tracing import (
    DependencyLink,
    Span,
    SpanEvent,
    SpanKindName,
    SpanLink,
    SpanStatus,
    StatusCodeName,
    Trace,
    TraceSummary,
    normalize_trace_id,
)

__all__ = [
    "any_value",
    "dependencies_from_body",
    "key_values",
    "legacy_to_traces",
    "nanos_to_micros",
    "otlp_span_to_span",
    "otlp_to_traces",
    "summaries_from_body",
]

_UNKNOWN_SERVICE = "unknown_service"


def _objects(value: Any) -> list[Mapping[str, Any]]:
    return [v for v in value if isinstance(v, Mapping)] if isinstance(value, list) else []


def _obj(value: Any) -> Mapping[str, Any]:
    return value if isinstance(value, Mapping) else {}


def _str(value: Any) -> str | None:
    return value if isinstance(value, str) else None


def nanos_to_micros(value: Any) -> int:
    """Epoch microseconds from an OTLP nanosecond timestamp (decimal string or number).

    Uses integer arithmetic; a missing or malformed value gives 0.
    """
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        return value // 1000
    if isinstance(value, float):
        return int(value // 1000) if math.isfinite(value) else 0
    if isinstance(value, str) and value.isdigit():
        return int(value) // 1000
    return 0


def any_value(value: Any) -> Any:
    """Python value of one OTLP/JSON ``AnyValue`` (``None`` when empty or unknown).

    ``intValue`` (sent as a string) becomes ``int``, ``arrayValue`` a list and ``kvlistValue``
    a dict.
    """
    if not isinstance(value, Mapping):
        return None
    if isinstance(value.get("stringValue"), str):
        return value["stringValue"]
    if isinstance(value.get("boolValue"), bool):
        return value["boolValue"]
    if "intValue" in value:
        raw = value["intValue"]
        try:
            return int(raw)
        except (TypeError, ValueError):
            return str(raw)
    if "doubleValue" in value:
        raw = value["doubleValue"]
        try:
            return float(raw)
        except (TypeError, ValueError):
            return str(raw)
    if isinstance(value.get("bytesValue"), str):
        return value["bytesValue"]
    if isinstance(value.get("arrayValue"), Mapping):
        items = [any_value(v) for v in value["arrayValue"].get("values") or []]
        return [v for v in items if v is not None]
    if isinstance(value.get("kvlistValue"), Mapping):
        return key_values(value["kvlistValue"].get("values"))
    return None


def key_values(values: Any) -> dict[str, Any]:
    """Attribute map from an OTLP/JSON ``KeyValue[]`` list (empty values are skipped)."""
    out: dict[str, Any] = {}
    for kv in _objects(values):
        key = _str(kv.get("key"))
        value = any_value(kv.get("value"))
        if key is not None and value is not None:
            out[key] = value
    return out


_OTLP_KINDS: dict[str, SpanKindName] = {
    "0": "unspecified",
    "1": "internal",
    "2": "server",
    "3": "client",
    "4": "producer",
    "5": "consumer",
    "SPAN_KIND_UNSPECIFIED": "unspecified",
    "SPAN_KIND_INTERNAL": "internal",
    "SPAN_KIND_SERVER": "server",
    "SPAN_KIND_CLIENT": "client",
    "SPAN_KIND_PRODUCER": "producer",
    "SPAN_KIND_CONSUMER": "consumer",
}


def _otlp_status(status: Any) -> SpanStatus:
    data = _obj(status)
    code = str(data.get("code", 0))
    name: StatusCodeName = "unset"
    if code in ("2", "STATUS_CODE_ERROR"):
        name = "error"
    elif code in ("1", "STATUS_CODE_OK"):
        name = "ok"
    return SpanStatus(name, _str(data.get("message")) or None)


def otlp_span_to_span(span: Mapping[str, Any], service_name: str) -> Span:
    """One OTLP/JSON span (with the service name of its resource) as a normalized span."""
    return Span(
        trace_id=_str(span.get("traceId")) or "",
        span_id=_str(span.get("spanId")) or "",
        parent_span_id=_str(span.get("parentSpanId")),
        name=_str(span.get("name")) or "",
        kind=_OTLP_KINDS.get(str(span.get("kind", 0)), "unspecified"),
        service_name=service_name,
        start_micros=nanos_to_micros(span.get("startTimeUnixNano")),
        end_micros=nanos_to_micros(span.get("endTimeUnixNano")),
        status=_otlp_status(span.get("status")),
        attributes=key_values(span.get("attributes")),
        events=[
            SpanEvent(
                _str(e.get("name")) or "",
                nanos_to_micros(e.get("timeUnixNano")),
                key_values(e.get("attributes")),
            )
            for e in _objects(span.get("events"))
        ],
        links=[
            SpanLink(
                _str(link.get("traceId")) or "",
                _str(link.get("spanId")) or "",
                key_values(link.get("attributes")),
            )
            for link in _objects(span.get("links"))
        ],
    )


def otlp_to_traces(data: Any) -> list[Trace]:
    """Traces from an OTLP/JSON ``TracesData`` (``{"resourceSpans": [...]}``).

    Spans are grouped by trace id in order of first appearance and sorted per SPEC §5.
    """
    by_id: dict[str, list[Span]] = {}
    for rs in _objects(_obj(data).get("resourceSpans")):
        resource = key_values(_obj(rs.get("resource")).get("attributes"))
        name = resource.get("service.name")
        service = name if isinstance(name, str) and name else _UNKNOWN_SERVICE
        for ss in _objects(rs.get("scopeSpans")):
            for raw in _objects(ss.get("spans")):
                span = otlp_span_to_span(raw, service)
                by_id.setdefault(span.trace_id, []).append(span)
    return [Trace(trace_id, spans) for trace_id, spans in by_id.items()]


# --------------------------------------------------------------------------- legacy model

#: Tags Jaeger synthesizes from OTLP fields; mapped back and dropped from the attributes.
_SYNTHETIC_TAGS = frozenset(
    (
        "span.kind",
        "otel.status_code",
        "otel.status_description",
        "error",
        "otel.scope.name",
        "otel.scope.version",
    )
)
_LEGACY_KINDS: frozenset[str] = frozenset(("internal", "server", "client", "producer", "consumer"))


def _legacy_tags(tags: Any, skip: frozenset[str] = frozenset()) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for tag in _objects(tags):
        key = _str(tag.get("key"))
        value = tag.get("value")
        if key is not None and key not in skip and isinstance(value, (str, bool, int, float)):
            out[key] = value
    return out


def _micros(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    return int(value)


def _legacy_span(span: Mapping[str, Any], processes: Mapping[str, Any], trace_id: str) -> Span:
    tags = _legacy_tags(span.get("tags"))
    start = _micros(span.get("startTime"))
    duration = _micros(span.get("duration"))
    process = _obj(processes.get(_str(span.get("processID")) or ""))
    kind_tag = tags.get("span.kind")
    status_tag = str(tags.get("otel.status_code") or "").upper()
    description = tags.get("otel.status_description")
    is_error = status_tag == "ERROR" or tags.get("error") is True
    code: StatusCodeName = "error" if is_error else ("ok" if status_tag == "OK" else "unset")
    message = description if is_error and isinstance(description, str) and description else None

    refs = _objects(span.get("references"))
    same_trace = [
        r
        for r in refs
        if isinstance(r.get("traceID"), str) and normalize_trace_id(r["traceID"]) == trace_id
    ]
    parent_ref = next((r for r in same_trace if r.get("refType") == "CHILD_OF"), None)
    if parent_ref is None and same_trace:
        parent_ref = same_trace[0]
    links = [
        SpanLink(r["traceID"], _str(r.get("spanID")) or "")
        for r in refs
        if r is not parent_ref and isinstance(r.get("traceID"), str)
    ]
    events = []
    for log in _objects(span.get("logs")):
        fields = _legacy_tags(log.get("fields"))
        name = fields.get("event")
        events.append(
            SpanEvent(
                name if isinstance(name, str) else "log",
                _micros(log.get("timestamp")),
                _legacy_tags(log.get("fields"), frozenset(("event",))),
            )
        )
    return Span(
        trace_id=trace_id,
        span_id=_str(span.get("spanID")) or "",
        parent_span_id=_str(parent_ref.get("spanID")) if parent_ref is not None else None,
        name=_str(span.get("operationName")) or "",
        kind=kind_tag if kind_tag in _LEGACY_KINDS else "unspecified",
        service_name=_str(process.get("serviceName")) or _UNKNOWN_SERVICE,
        start_micros=start,
        end_micros=start + duration,
        duration_micros=duration,
        status=SpanStatus(code, message),
        attributes=_legacy_tags(span.get("tags"), _SYNTHETIC_TAGS),
        events=events,
        links=links,
    )


def legacy_to_traces(body: Any) -> list[Trace]:
    """Traces from the legacy ``/api/traces/{id}`` body (``{"data": [{traceID, spans, …}]}``).

    Parent = first ``CHILD_OF`` reference in the same trace (else the first such reference);
    other references become links. ``span.kind``, ``otel.status_code``/``_description`` and
    ``error`` tags become the kind and status; ``logs`` become events (``event`` field = name).
    """
    traces = []
    for t in _objects(_obj(body).get("data")):
        trace_id = normalize_trace_id(_str(t.get("traceID")) or "")
        processes = _obj(t.get("processes"))
        spans = [_legacy_span(s, processes, trace_id) for s in _objects(t.get("spans"))]
        traces.append(Trace(trace_id, spans))
    return traces


# --------------------------------------------------------------------------- summaries, deps


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def summaries_from_body(body: Any) -> list[TraceSummary]:
    """SPEC ``TraceSummary`` values from a ``/api/v3/trace-summaries`` body."""
    out = []
    for s in _objects(_obj(body).get("summaries")):
        start = nanos_to_micros(s.get("minStartTimeUnixNano"))
        end = nanos_to_micros(s.get("maxEndTimeUnixNano"))
        out.append(
            TraceSummary(
                trace_id=normalize_trace_id(_str(s.get("traceId")) or ""),
                start_micros=start,
                duration_micros=max(0, end - start),
                span_count=_int(s.get("spanCount")),
                root_service_name=_str(s.get("rootServiceName")) or None,
                root_name=_str(s.get("rootOperationName")) or None,
            )
        )
    return out


def dependencies_from_body(body: Any) -> list[DependencyLink]:
    """Dependency links from a legacy ``/api/dependencies`` body (``{"data": [...]}``)."""
    return [
        DependencyLink(d["parent"], d["child"], _int(d.get("callCount")))
        for d in _objects(_obj(body).get("data"))
        if isinstance(d.get("parent"), str) and isinstance(d.get("child"), str)
    ]
