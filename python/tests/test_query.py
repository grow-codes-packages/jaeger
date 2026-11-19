"""JaegerQueryClient / AsyncJaegerQueryClient against recorded Jaeger responses."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from typing import Any

import httpx
import pytest

import grow_jaeger as sdk
from _fixtures import TRACE_ID, Router, config, response

NOW = datetime(2026, 11, 18, 21, 0, 0, tzinfo=timezone.utc)


def client(router: Router, **kw: Any) -> sdk.JaegerQueryClient:
    return sdk.JaegerQueryClient(config(**kw), http_transport=router.transport(), now=lambda: NOW)


def aclient(router: Router, **kw: Any) -> sdk.AsyncJaegerQueryClient:
    return sdk.AsyncJaegerQueryClient(
        config(**kw), http_transport=router.transport(), now=lambda: NOW
    )


V3 = f"/api/v3/traces/{TRACE_ID}"
LEGACY = f"/api/traces/{TRACE_ID}"

# --------------------------------------------------------------------------- get_trace


def test_get_trace_v3_normalizes_the_otlp_model() -> None:
    router = Router().add(V3, response("getTrace.v3_ok"))
    with client(router) as q:
        trace = q.get_trace(TRACE_ID.upper())
    assert trace is not None
    assert trace.trace_id == TRACE_ID
    assert [s.name for s in trace.spans] == ["GET /orders", "handle /orders", "db.query"]
    root, child, leaf = trace.spans
    assert root.parent_span_id is None
    assert root.kind == "client" and root.service_name == "sdk-gw-86837b62"
    assert root.status == sdk.SpanStatus("ok")
    assert root.duration_micros == 1_200_000
    assert root.start_micros == 1795034918142512
    assert root.attributes == {"http.request.method": "GET", "order.id": "86837b62"}
    assert child.parent_span_id == root.span_id
    assert child.kind == "server" and child.service_name == "sdk-wk-86837b62"
    assert child.attributes == {
        "array.attr": ["a", "b"],
        "bool.attr": True,
        "double.attr": 1.5,
        "int.attr": 42,
        "str.attr": "v",
    }
    assert child.status == sdk.SpanStatus("error", "boom")
    assert child.events[0].name == "cache.miss"
    assert child.events[0].attributes == {"cache.key": "k1"}
    assert child.links[0].trace_id == "85092f96d876e30b507cfd1361b0325e"
    assert child.links[0].attributes == {"link.kind": "follows"}
    assert leaf.kind == "internal" and leaf.status.code == "unset"
    assert [n.span.name for n in trace.tree()] == ["GET /orders"]
    params = router.params(V3)
    assert params["startTime"] == "2026-11-11T21:00:00.000000Z"
    assert params["endTime"] == "2026-11-18T21:00:00.000000Z"
    assert "rawTraces" not in params


def test_get_trace_window_and_raw_traces() -> None:
    router = Router().add(V3, response("getTrace.v3_ok"))
    start = datetime(2026, 11, 18, 20, 0)  # naive = UTC
    q = client(router)
    q.get_trace(TRACE_ID, start=start, end=NOW, raw_traces=True)
    params = router.params(V3)
    assert params["startTime"] == "2026-11-18T20:00:00.000000Z"
    assert params["rawTraces"] == "true"
    with pytest.raises(sdk.ValidationError):
        q.get_trace(TRACE_ID, start=NOW, end=NOW - timedelta(seconds=1))


def test_get_trace_short_id_is_left_padded() -> None:
    padded = "0000000000000000" + "a3f1b2c4d5e6f708"
    router = Router().add(f"/api/v3/traces/{padded}", response("getTrace.v3_not_found"))
    assert client(router).get_trace("A3F1B2C4D5E6F708") is None


@pytest.mark.parametrize("bad", ["", "xyz", "0" * 32, "1" * 33])
def test_get_trace_rejects_bad_ids_without_a_request(bad: str) -> None:
    router = Router()
    with pytest.raises(sdk.ValidationError):
        client(router).get_trace(bad)
    assert router.seen == []


def test_get_trace_not_found_envelope_is_none() -> None:
    router = Router().add(V3, response("getTrace.v3_not_found"))
    q = client(router)
    assert q.get_trace(TRACE_ID) is None
    with pytest.raises(sdk.NotFoundError) as info:
        q.require_trace(TRACE_ID)
    assert TRACE_ID in str(info.value)
    assert not any(r.url.path == LEGACY for r in router.seen)


def test_get_trace_empty_result_is_none() -> None:
    router = Router().add(V3, httpx.Response(200, json={"result": {"resourceSpans": []}}))
    assert client(router).get_trace(TRACE_ID) is None


def test_get_trace_falls_back_to_legacy_on_missing_route() -> None:
    router = (
        Router().add(V3, response("api.unknown_route")).add(LEGACY, response("getTraceLegacy.ok"))
    )
    q = client(router)
    trace = q.require_trace(TRACE_ID, raw_traces=False)
    root, child, leaf = trace.spans
    assert root.name == "GET /orders" and root.kind == "client"
    assert root.status == sdk.SpanStatus("ok")
    assert root.attributes == {"http.request.method": "GET", "order.id": "86837b62"}
    assert root.service_name == "sdk-gw-86837b62"
    assert child.parent_span_id == root.span_id
    assert child.status == sdk.SpanStatus("error", "boom")
    assert child.links[0].trace_id == "85092f96d876e30b507cfd1361b0325e"
    assert child.events[0].name == "cache.miss"
    assert child.events[0].attributes == {"cache.key": "k1"}
    assert child.attributes["int.attr"] == 42
    assert "error" not in child.attributes and "span.kind" not in child.attributes
    assert leaf.service_name == "sdk-wk-86837b62" and leaf.duration_micros == 5000
    params = router.params(LEGACY)
    assert params["raw"] == "false"
    assert params["end"] == str(int(NOW.timestamp() * 1_000_000))


def test_get_trace_legacy_not_found_and_route_missing() -> None:
    router = (
        Router()
        .add(V3, response("api.unknown_route"))
        .add(LEGACY, response("getTraceLegacy.not_found"))
    )
    assert client(router).get_trace(TRACE_ID) is None
    router = Router().add(V3, httpx.Response(404, text="<html>nope</html>"))
    with pytest.raises(sdk.UnsupportedError) as info:
        client(router).get_trace(TRACE_ID)
    assert info.value.http_status == 404


def test_get_trace_maps_errors_with_request_id() -> None:
    router = Router().add(V3, response("getTrace.v3_invalid_id"))
    with pytest.raises(sdk.ValidationError) as info:
        client(router).get_trace(TRACE_ID)
    err = info.value
    assert err.http_status == 400
    assert "invalid length for TraceID" in err.message
    assert err.request_id is not None and err.request_id.startswith("00-")


@pytest.mark.parametrize(
    ("status", "cls"),
    [
        (401, sdk.AuthenticationError),
        (403, sdk.PermissionDeniedError),
        (501, sdk.UnsupportedError),
        (500, sdk.UnavailableError),
    ],
)
def test_query_status_mapping(status: int, cls: type[sdk.JaegerError]) -> None:
    envelope = {"error": {"httpCode": status, "message": f"failure {status}"}}
    router = Router().add("/api/v3/services", httpx.Response(status, json=envelope))
    with pytest.raises(cls) as info:
        client(router, retry=sdk.RetryPolicy(max_retries=0)).list_services()
    assert info.value.http_status == status
    assert f"failure {status}" in info.value.message


def test_retries_5xx_then_succeeds() -> None:
    router = Router().add(
        "/api/v3/services",
        httpx.Response(503, text="busy"),
        response("listServices.ok"),
    )
    assert "jaeger" in client(router).list_services()
    assert len(router.seen) == 2


def test_token_is_sent_and_redacted() -> None:
    secret = "query-token-123456"
    seen: dict[str, str] = {}

    def reply(req: httpx.Request) -> httpx.Response:
        seen.update(req.headers)
        return httpx.Response(401, text=f"bad token {secret}")

    router = Router().add("/api/v3/services", reply)
    q = client(router, token=secret)
    with pytest.raises(sdk.AuthenticationError) as info:
        q.list_services()
    assert seen["authorization"] == f"Bearer {secret}"
    assert secret not in str(info.value) and secret not in repr(q)


# --------------------------------------------------------------------------- search


def test_find_traces_sends_every_filter() -> None:
    router = Router().add("/api/v3/traces", response("findTraces.by_service"))
    q = client(router)
    traces = q.find_traces(
        service="sdk-wk-86837b62",
        operation="handle /orders",
        tags={"order.id": "86837b62", "n": 1},  # type: ignore[dict-item]
        min_duration_micros=1000,
        max_duration_micros=2_000_000.4,  # type: ignore[arg-type]
        limit=5,
        raw_traces=True,
    )
    assert traces and all(isinstance(t, sdk.Trace) for t in traces)
    assert traces[0].trace_id == TRACE_ID
    params = router.params("/api/v3/traces")
    assert params == {
        "query.serviceName": "sdk-wk-86837b62",
        "query.operationName": "handle /orders",
        "query.attributes": json.dumps({"order.id": "86837b62", "n": "1"}, separators=(",", ":")),
        "query.durationMin": "1000us",
        "query.durationMax": "2000000us",
        "query.startTimeMin": "2026-11-18T20:00:00.000000Z",
        "query.startTimeMax": "2026-11-18T21:00:00.000000Z",
        "query.searchDepth": "5",
        "query.rawTraces": "true",
    }


def test_find_traces_defaults_and_no_match() -> None:
    router = Router().add("/api/v3/traces", response("findTraces.no_match"))
    assert client(router).find_traces() == []
    params = router.params("/api/v3/traces")
    assert params["query.searchDepth"] == "20"
    assert "query.serviceName" not in params


def test_find_traces_accepts_query_objects_and_overrides() -> None:
    router = Router().add("/api/v3/traces", response("findTraces.no_match"))
    base = sdk.TraceSearchQuery(service="a", limit=3)
    client(router).find_traces(base, limit=7)
    assert router.params("/api/v3/traces")["query.searchDepth"] == "7"
    client(router).find_traces(sdk.JaegerTraceQuery(service="b"))


@pytest.mark.parametrize(
    "kw",
    [
        {"limit": 0},
        {"limit": True},
        {"min_duration_micros": -1},
        {"max_duration_micros": "5"},
        {"start": NOW, "end": NOW - timedelta(hours=1)},
    ],
)
def test_search_validation(kw: dict[str, Any]) -> None:
    router = Router()
    with pytest.raises(sdk.ValidationError):
        client(router).search_traces(**kw)
    assert router.seen == []


def test_find_traces_maps_bad_parameters() -> None:
    router = Router().add("/api/v3/traces", response("findTraces.bad_duration"))
    with pytest.raises(sdk.ValidationError) as info:
        client(router).find_traces(service="x")
    assert "missing unit" in info.value.message


def test_search_traces_maps_summaries() -> None:
    router = Router().add("/api/v3/trace-summaries", response("findTraceSummaries.ok"))
    summaries = client(router).search_traces(service="sdk-gw-86837b62")
    first = summaries[0]
    assert first == sdk.TraceSummary(
        trace_id=TRACE_ID,
        start_micros=1795034918142512,
        duration_micros=1_200_000,
        span_count=3,
        root_service_name="sdk-gw-86837b62",
        root_name="GET /orders",
    )
    assert first.to_json()["rootName"] == "GET /orders"


def test_search_traces_empty_and_404() -> None:
    router = Router().add(
        "/api/v3/trace-summaries",
        response("findTraceSummaries.empty"),
        response("getTrace.v3_not_found"),
    )
    q = client(router)
    assert q.search_traces() == []
    assert q.search_traces() == []


def test_search_and_find_route_missing_are_unsupported() -> None:
    router = Router()
    q = client(router)
    with pytest.raises(sdk.UnsupportedError):
        q.search_traces()
    with pytest.raises(sdk.UnsupportedError):
        q.find_traces()
    with pytest.raises(sdk.UnsupportedError):
        q.list_services()


# --------------------------------------------------------------------------- services, ops


def test_list_services_sorted() -> None:
    router = Router().add("/api/v3/services", response("listServices.ok"))
    services = client(router).list_services()
    assert services == sorted(services) and "jaeger" in services


def test_list_services_odd_shapes() -> None:
    router = Router().add(
        "/api/v3/services",
        httpx.Response(200, json={"services": None}),
        httpx.Response(200, json=[1]),
        httpx.Response(200, json={"services": ["b", 1, "a"]}),
    )
    q = client(router)
    assert q.list_services() == []
    with pytest.raises(sdk.ValidationError):
        q.list_services()
    assert q.list_services() == ["a", "b"]


def test_list_services_not_found_envelope_raises() -> None:
    router = Router().add("/api/v3/services", response("getTrace.v3_not_found"))
    with pytest.raises(sdk.NotFoundError):
        client(router).list_services()


def test_operations() -> None:
    router = Router().add(
        "/api/v3/operations",
        response("listOperations.ok"),
        httpx.Response(
            200,
            json={
                "operations": [
                    {"name": "b", "spanKind": "server"},
                    {"name": "b", "spanKind": "client"},
                    {"name": "a"},
                    {"nope": 1},
                    "x",
                ]
            },
        ),
        httpx.Response(200, json={"operations": None}),
    )
    q = client(router)
    ops = q.get_operations("svc", span_kind="SERVER")
    assert ops == [
        sdk.JaegerOperation("handle /orders", "server"),
        sdk.JaegerOperation("db.query", "internal"),
    ]
    assert router.params("/api/v3/operations") == {"service": "svc", "spanKind": "server"}
    assert q.list_operations("svc") == ["a", "b"]
    assert q.list_operations("svc") == []


@pytest.mark.parametrize(("service", "kind"), [("", None), ("  ", None), ("svc", "weird")])
def test_operations_validation(service: str, kind: str | None) -> None:
    router = Router()
    with pytest.raises(sdk.ValidationError):
        client(router).list_operations(service, span_kind=kind)
    assert router.seen == []


# --------------------------------------------------------------------------- dependencies


def test_dependencies_defaults() -> None:
    router = Router().add("/api/dependencies", response("getDependencies.ok"))
    links = client(router).get_dependencies()
    assert sdk.DependencyLink("sdk-gw-86837b62", "sdk-wk-86837b62", 1) in links
    params = router.params("/api/dependencies")
    assert params == {"endTs": str(int(NOW.timestamp() * 1000)), "lookback": "86400000"}


def test_dependencies_native_and_window_forms() -> None:
    router = Router().add("/api/dependencies", response("getDependencies.service_filter"))
    q = client(router)
    end = NOW - timedelta(minutes=5)
    q.get_dependencies(end, timedelta(minutes=30), service="svc")
    assert router.params("/api/dependencies") == {
        "endTs": str(int(end.timestamp() * 1000)),
        "lookback": "1800000",
        "service": "svc",
    }
    router.seen.clear()
    q.get_dependencies(start=NOW - timedelta(hours=2), end=NOW)
    assert router.params("/api/dependencies")["lookback"] == "7200000"
    with pytest.raises(sdk.ValidationError):
        q.get_dependencies(lookback_s=0)
    with pytest.raises(sdk.ValidationError):
        q.get_dependencies(start=NOW, end=NOW)


def test_dependencies_errors_and_odd_shapes() -> None:
    router = Router().add(
        "/api/dependencies",
        response("getDependencies.bad_lookback"),
        httpx.Response(200, json={"data": [{"parent": "a"}, {"parent": "a", "child": "b"}]}),
    )
    q = client(router)
    with pytest.raises(sdk.ValidationError) as info:
        q.get_dependencies()
    assert "positive duration" in info.value.message
    assert q.get_dependencies() == [sdk.DependencyLink("a", "b", 0)]


# --------------------------------------------------------------------------- wait, lifecycle


def test_wait_for_trace_polls_until_min_spans() -> None:
    router = Router().add(
        V3,
        response("getTrace.v3_not_found"),
        response("getTrace.v3_ok"),
    )
    trace = client(router).wait_for_trace(TRACE_ID, timeout_s=5, interval_s=0.01, min_spans=3)
    assert len(trace.spans) == 3


def test_wait_for_trace_times_out() -> None:
    router = Router().add(V3, response("getTrace.v3_not_found"))
    with pytest.raises(sdk.TimeoutError):
        client(router).wait_for_trace(TRACE_ID, timeout_s=0.05, interval_s=0.01)


def test_query_client_from_env_and_protocol() -> None:
    q = sdk.JaegerQueryClient.from_env({"JAEGER_QUERY_URL": "http://q:16686"})
    assert q.config.url == "http://q:16686"
    assert isinstance(q, sdk.TraceQueryClient)
    assert "JaegerQueryClient" in repr(q)
    q.close()
    q.close()
    aq = sdk.AsyncJaegerQueryClient.from_env({})
    assert isinstance(aq, sdk.AsyncTraceQueryClient)
    assert "AsyncJaegerQueryClient" in repr(aq)


def test_query_client_defaults_to_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GROW_JAEGER_URL", "http://env-host:16686")
    assert sdk.JaegerQueryClient().config.url == "http://env-host:16686"
    assert sdk.AsyncJaegerQueryClient().config.url == "http://env-host:16686"


def test_cancel_token_aborts() -> None:
    token = sdk.CancelToken()
    token.cancel()
    router = Router().add("/api/v3/services", response("listServices.ok"))
    with pytest.raises(sdk.CancelledError):
        client(router).list_services(cancel=token)


# --------------------------------------------------------------------------- async


async def test_async_query_client_round_trip() -> None:
    router = (
        Router()
        .add(V3, response("api.unknown_route"))
        .add(LEGACY, response("getTraceLegacy.ok"))
        .add("/api/v3/traces", response("findTraces.by_service"))
        .add("/api/v3/trace-summaries", response("findTraceSummaries.ok"))
        .add("/api/v3/services", response("listServices.ok"))
        .add("/api/v3/operations", response("listOperations.ok"))
        .add("/api/dependencies", response("getDependencies.ok"))
    )
    async with aclient(router) as q:
        trace = await q.require_trace(TRACE_ID)
        assert len(trace.spans) == 3
        assert (await q.find_traces(service="sdk-wk-86837b62"))[0].trace_id == TRACE_ID
        assert (await q.search_traces(sdk.TraceSearchQuery(limit=2)))[0].span_count == 3
        assert "jaeger" in await q.list_services()
        assert await q.list_operations("svc", span_kind="server") == ["db.query", "handle /orders"]
        assert await q.get_dependencies(NOW, 60)
        waited = await q.wait_for_trace(TRACE_ID, timeout_s=1, interval_s=0.01)
        assert waited.trace_id == TRACE_ID


async def test_async_get_trace_v3_and_not_found() -> None:
    router = Router().add(V3, response("getTrace.v3_ok"), response("getTrace.v3_not_found"))
    q = aclient(router)
    assert (await q.get_trace(TRACE_ID)) is not None
    assert (await q.get_trace(TRACE_ID)) is None
    with pytest.raises(sdk.NotFoundError):
        await q.require_trace(TRACE_ID)
    await q.close()
