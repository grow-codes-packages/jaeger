"""Tracing → Jaeger → query round trips, remote sampling and the tracing conformance suite.

Every test exports under its own unique service names (the in-memory store is shared with the
other language suites and Jaeger also traces itself as ``jaeger``).
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Iterator
from datetime import datetime, timedelta, timezone
from typing import Any

import pytest
from factories import TRACING_CAPABILITIES, core_client, tracing_handle, tracing_target, unique
from opentelemetry import trace as otel_trace
from opentelemetry.trace import Link, SpanKind, Status, StatusCode

import grow_jaeger as sdk
from grow_jaeger.testing import ConformanceRunner, assert_scenario, pytest_params

WAIT = {"timeout_s": 30, "interval_s": 0.25}


@pytest.fixture
def query(env: dict[str, str]) -> Iterator[sdk.JaegerQueryClient]:
    with sdk.JaegerQueryClient.from_env(env) as q:
        yield q


def _tid(span: otel_trace.Span) -> str:
    return format(span.get_span_context().trace_id, "032x")


def _sid(span: otel_trace.Span) -> str:
    return format(span.get_span_context().span_id, "016x")


def export_order_trace(env: dict[str, str], gw: str, wk: str, **kw: Any) -> tuple[str, str, str]:
    """gw: client span ``GET /orders`` → wk: server span ``handle /orders`` → ``db.query``.

    Returns ``(trace_id, linked_trace_id, run_tag)``.
    """
    tag = unique("tag")
    h_gw = tracing_handle(env, gw, service_version="1.2.3", environment="sdk-test", **kw)
    h_wk = tracing_handle(env, wk, **kw)
    try:
        t_gw, t_wk = h_gw.tracer("it"), h_wk.tracer("it")
        with t_gw.start_as_current_span("linked-root") as other:
            linked = other.get_span_context()
        with t_gw.start_as_current_span(
            "GET /orders", kind=SpanKind.CLIENT, attributes={"order.id": tag}
        ) as root:
            root.set_status(Status(StatusCode.OK))
            time.sleep(0.02)
            with t_wk.start_as_current_span(
                "handle /orders",
                kind=SpanKind.SERVER,
                links=[Link(linked, {"link.kind": "follows"})],
                attributes={
                    "int.attr": 42,
                    "bool.attr": True,
                    "double.attr": 1.5,
                    "array.attr": ["a", "b"],
                },
            ) as child:
                child.add_event("cache.miss", {"cache.key": "k1"})
                child.set_status(Status(StatusCode.ERROR, "boom"))
                with t_wk.start_as_current_span("db.query"):
                    time.sleep(0.01)
        h_gw.force_flush(10)
        h_wk.force_flush(10)
        return _tid(root), format(linked.trace_id, "032x"), tag
    finally:
        h_gw.shutdown()
        h_wk.shutdown()


@pytest.fixture(scope="module")
def order(env: dict[str, str]) -> dict[str, str]:
    gw, wk = unique("gw"), unique("wk")
    trace_id, linked, tag = export_order_trace(env, gw, wk)
    with sdk.JaegerQueryClient.from_env(env) as q:
        q.wait_for_trace(trace_id, min_spans=3, **WAIT)
    return {"gw": gw, "wk": wk, "trace_id": trace_id, "linked": linked, "tag": tag}


# --------------------------------------------------------------------------- query module


def test_get_trace_round_trip(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    trace = query.get_trace(order["trace_id"])
    assert trace is not None
    assert trace.trace_id == order["trace_id"]
    by_name = {s.name: s for s in trace.spans}
    root, child, leaf = by_name["GET /orders"], by_name["handle /orders"], by_name["db.query"]
    assert root.kind == "client"
    assert root.service_name == order["gw"]
    assert root.parent_span_id is None
    assert root.status.code == "ok"
    assert root.attributes["order.id"] == order["tag"]
    assert child.parent_span_id == root.span_id
    assert child.service_name == order["wk"]
    assert child.kind == "server"
    assert child.status == sdk.SpanStatus("error", "boom")
    assert child.attributes["int.attr"] == 42
    assert child.attributes["bool.attr"] is True
    assert child.attributes["double.attr"] == 1.5
    assert child.attributes["array.attr"] == ["a", "b"]
    assert child.events[0].name == "cache.miss"
    assert child.events[0].attributes == {"cache.key": "k1"}
    assert child.links[0].trace_id == order["linked"]
    assert child.links[0].attributes == {"link.kind": "follows"}
    assert leaf.parent_span_id == child.span_id
    assert root.duration_micros >= 20_000
    assert [n.span.name for n in trace.tree()] == ["GET /orders"]


def test_get_trace_options_and_missing(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    now = datetime.now(timezone.utc)
    raw = query.require_trace(
        order["trace_id"], start=now - timedelta(hours=1), end=now, raw_traces=True
    )
    assert len(raw.spans) == 3
    assert query.get_trace("f" * 32) is None
    with pytest.raises(sdk.NotFoundError):
        query.require_trace("e" * 32)
    with pytest.raises(sdk.ValidationError):
        query.get_trace("not-a-trace-id")


def test_find_traces_with_filters(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    traces = query.find_traces(service=order["wk"])
    assert [t.trace_id for t in traces] == [order["trace_id"]]
    assert len(traces[0].spans) == 3
    filtered = query.find_traces(
        sdk.JaegerTraceQuery(
            service=order["gw"],
            operation="GET /orders",
            tags={"order.id": order["tag"]},
            min_duration_micros=1,
            max_duration_micros=60_000_000,
            start=datetime.now(timezone.utc) - timedelta(minutes=30),
            limit=5,
            raw_traces=False,
        )
    )
    assert [t.trace_id for t in filtered] == [order["trace_id"]]
    assert query.find_traces(service=order["gw"], operation="nope") == []
    assert query.find_traces(service=order["gw"], min_duration_micros=3_600_000_000) == []


def test_search_traces_summaries(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    (summary,) = query.search_traces(service=order["gw"], tags={"order.id": order["tag"]})
    assert summary.trace_id == order["trace_id"]
    assert summary.span_count == 3
    assert summary.root_service_name == order["gw"]
    assert summary.root_name == "GET /orders"
    assert summary.duration_micros >= 20_000
    assert query.search_traces(service=unique("nobody")) == []


def test_list_services(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    services = query.list_services()
    assert {order["gw"], order["wk"], "jaeger"} <= set(services)
    assert services == sorted(services)


def test_operations(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    assert query.list_operations(order["wk"]) == ["db.query", "handle /orders"]
    assert query.list_operations(order["wk"], span_kind="SERVER") == ["handle /orders"]
    ops = query.get_operations(order["wk"], span_kind="internal")
    assert ops == [sdk.JaegerOperation("db.query", "internal")]
    assert query.list_operations(unique("nobody")) == []


def test_get_dependencies(query: sdk.JaegerQueryClient, order: dict[str, str]) -> None:
    edge = sdk.DependencyLink(order["gw"], order["wk"], 1)
    assert edge in query.get_dependencies()
    now = datetime.now(timezone.utc)
    assert edge in query.get_dependencies(now, timedelta(minutes=30), service=order["gw"])
    assert edge in query.get_dependencies(start=now - timedelta(hours=1), end=now)
    old = now - timedelta(days=30)
    assert edge not in query.get_dependencies(old, 60)
    with pytest.raises(sdk.ValidationError):
        query.get_dependencies(lookback_s=-1)


def test_wait_for_trace_timeout(query: sdk.JaegerQueryClient) -> None:
    with pytest.raises(sdk.TimeoutError):
        query.wait_for_trace("d" * 32, timeout_s=0.5, interval_s=0.2)


def test_client_query_property(env: dict[str, str], order: dict[str, str]) -> None:
    with core_client(env) as c:
        assert c.query.get_trace(order["trace_id"]) is not None


def test_async_query_and_client(env: dict[str, str], order: dict[str, str]) -> None:
    async def run() -> None:
        async with sdk.AsyncJaegerClient.from_env(env) as c:
            assert (await c.health()).healthy
            assert await c.ping() >= 0
            assert await c.server_version() == "2.23.0"
            assert (await c.build_info()) is not None
            q = c.query
            trace = await q.require_trace(order["trace_id"])
            assert len(trace.spans) == 3
            assert (await q.get_trace("c" * 32)) is None
            found = await q.find_traces(service=order["wk"])
            assert [t.trace_id for t in found] == [order["trace_id"]]
            (summary,) = await q.search_traces(service=order["wk"])
            assert summary.span_count == 3
            assert order["wk"] in await q.list_services()
            assert await q.list_operations(order["wk"], span_kind="server") == ["handle /orders"]
            assert (await q.get_operations(order["wk"]))[0].name
            edge = sdk.DependencyLink(order["gw"], order["wk"], 1)
            assert edge in await q.get_dependencies()
            waited = await q.wait_for_trace(order["trace_id"], timeout_s=5, interval_s=0.2)
            assert waited.trace_id == order["trace_id"]
            strategy = await c.sampling_strategy(unique("svc"))
            assert strategy.sampling_rate == 1.0
        async with sdk.AsyncJaegerQueryClient.from_env(env) as standalone:
            assert order["gw"] in await standalone.list_services()

    asyncio.run(run())


# --------------------------------------------------------------------------- init_tracing


def test_init_tracing_grpc_round_trip(env: dict[str, str], query: sdk.JaegerQueryClient) -> None:
    service = unique("grpc")
    handle = tracing_handle(env, service, protocol="grpc", compression="gzip")
    assert handle.endpoint == env["GROW_JAEGER_OTLP_GRPC_ENDPOINT"]
    with handle.tracer("it").start_as_current_span("grpc-root") as root:
        with handle.tracer("it").start_as_current_span("grpc-child"):
            pass
    handle.force_flush(10)
    handle.shutdown()
    trace = query.wait_for_trace(_tid(root), min_spans=2, **WAIT)
    assert {s.service_name for s in trace.spans} == {service}
    assert {s.name for s in trace.spans} == {"grpc-root", "grpc-child"}


def test_init_tracing_from_otel_env_only(env: dict[str, str], query: sdk.JaegerQueryClient) -> None:
    service = unique("otelenv")
    only_otel = {
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": env["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"],
        "OTEL_SERVICE_NAME": service,
        "OTEL_RESOURCE_ATTRIBUTES": "team=sdk",
        "OTEL_TRACES_SAMPLER": "always_on",
        "OTEL_EXPORTER_OTLP_COMPRESSION": "none",
    }
    with sdk.init_tracing(env=only_otel, set_global=False) as handle:
        assert handle.config.service_name == service
        with handle.tracer("it").start_as_current_span("env-span") as span:
            pass
        handle.force_flush(10)
    trace = query.wait_for_trace(_tid(span), **WAIT)
    assert trace.spans[0].service_name == service


def test_init_tracing_sampling_presets(env: dict[str, str], query: sdk.JaegerQueryClient) -> None:
    service = unique("preset")
    handle = tracing_handle(env, service, **sdk.SAMPLING_OFF.as_kwargs())
    with handle.tracer("it").start_as_current_span("dropped") as span:
        assert not span.get_span_context().trace_flags.sampled
    handle.shutdown()
    cfg = sdk.SAMPLING_DEVELOPMENT.apply(sdk.TracingConfig(service, set_global=False))
    handle = sdk.init_tracing(cfg, env=env, batch=sdk.SpanBatchOptions(scheduled_delay_s=0.2))
    with handle.tracer("it").start_as_current_span("kept") as kept:
        pass
    handle.shutdown()
    assert query.wait_for_trace(_tid(kept), **WAIT).spans[0].name == "kept"
    assert query.get_trace(_tid(span)) is None


def test_global_helpers_round_trip(env: dict[str, str], query: sdk.JaegerQueryClient) -> None:
    """The template span helpers on the global provider set by ``init_tracing``."""
    service = unique("helpers")
    handle = tracing_handle(env, service, set_global=True)
    carrier: dict[str, str] = {}

    @sdk.traced("decorated")
    def work() -> str | None:
        sdk.add_event("inside")
        return sdk.current_span_id()

    with sdk.with_span("helper-root", attributes={"k": "v"}, kind="server") as root:
        trace_id = sdk.current_trace_id()
        assert sdk.trace_fields()["trace_id"] == trace_id
        decorated_id = work()
        sdk.inject(carrier)
        sdk.set_error("bad thing")
    assert "traceparent" in carrier
    ctx = sdk.extract(carrier)
    with sdk.default_tracer().start_as_current_span("remote-child", context=ctx):
        pass
    handle.force_flush(10)
    handle.shutdown()
    assert trace_id == _tid(root)
    trace = query.wait_for_trace(trace_id, min_spans=3, **WAIT)
    by_name = {s.name: s for s in trace.spans}
    assert by_name["decorated"].span_id == decorated_id
    assert by_name["decorated"].events[0].name == "inside"
    assert by_name["helper-root"].status.code == "error"
    assert by_name["remote-child"].parent_span_id == by_name["helper-root"].span_id


def test_client_init_tracing_is_closed_with_client(
    env: dict[str, str], query: sdk.JaegerQueryClient
) -> None:
    service = unique("client")
    c = core_client(env)
    handle = c.init_tracing(service_name=service, env=env, set_global=False, sampler="always_on")
    with handle.tracer("it").start_as_current_span("closed-by-client") as span:
        pass
    c.close()  # flushes the batch processor
    assert handle.is_shut_down
    assert query.wait_for_trace(_tid(span), **WAIT).spans[0].service_name == service


# --------------------------------------------------------------------------- remote sampling


def test_sampling_strategy_variants(env: dict[str, str]) -> None:
    with core_client(env) as stock:
        assert stock.sampling_strategy(unique("svc")).sampling_rate == 1.0
    fixture = {**env, "GROW_JAEGER_SAMPLING_URL": env["GROW_JAEGER_SAMPLING_FIXTURE_URL"]}
    with core_client(fixture) as c:
        ops = c.sampling_strategy("sdk-sampling-ops")
        assert ops.type == "probabilistic"
        assert ops.sampling_rate == 0.8
        assert ops.operation_rate("GET /fast") == 0.2
        assert ops.operation_rate("GET /slow") == 0.4
        rate = c.sampling_strategy("sdk-sampling-rate")
        assert rate.type == "ratelimiting"
        assert rate.max_traces_per_second == 5
        assert c.sampling_strategy("sdk-sampling-prob").sampling_rate == 0.25
        default = c.sampling_strategy(unique("unknown"))
        assert default.sampling_rate == 0.5
        assert default.operation_rate("/health") == 0.0


def test_remote_sampler_polls_the_fixture(env: dict[str, str]) -> None:
    fixture = {**env, "GROW_JAEGER_SAMPLING_URL": env["GROW_JAEGER_SAMPLING_FIXTURE_URL"]}
    with core_client(fixture) as c:
        sampler = c.remote_sampler("sdk-sampling-rate", 0.2)
        assert sampler.ready(10)
        assert sampler.strategy is not None
        assert sampler.strategy.max_traces_per_second == 5
        decisions = [sampler.should_sample(None, 2**127 + i, "work").decision for i in range(20)]
        assert sum(d.is_sampled() for d in decisions) == 5
        assert not sampler.should_sample(None, 1, "/health").decision.is_sampled()
        standalone = sdk.JaegerRemoteSampler(
            env["GROW_JAEGER_SAMPLING_FIXTURE_URL"], "sdk-sampling-ops", 0.2, config=c.config
        )
        assert standalone.ready(10)
        assert standalone.refresh().operation_rate("GET /fast") == 0.2
        standalone.close()
    assert sampler.closed


def test_init_tracing_with_remote_sampler(
    env: dict[str, str], query: sdk.JaegerQueryClient
) -> None:
    """Strategy ``sdk-sampling-live`` = probability 1 except ``/health`` (rate 0)."""
    service = unique("remote")
    handle = tracing_handle(
        env,
        service,
        remote_sampler=sdk.RemoteSamplerOptions(
            service_name="sdk-sampling-live",
            sampling_url=env["GROW_JAEGER_SAMPLING_FIXTURE_URL"],
            poll_interval_s=0.5,
        ),
    )
    sampler = handle.remote_sampler
    assert sampler is not None
    assert sampler.ready(10)
    assert sampler.strategy is not None
    tracer = handle.tracer("it")
    with tracer.start_as_current_span("/health") as dropped:
        pass
    with tracer.start_as_current_span("GET /work") as kept:
        pass
    handle.force_flush(10)
    handle.shutdown()
    assert sampler.closed
    assert not dropped.get_span_context().trace_flags.sampled
    assert query.wait_for_trace(_tid(kept), **WAIT).spans[0].name == "GET /work"
    assert query.get_trace(_tid(dropped)) is None


# --------------------------------------------------------------------------- conformance


@pytest.mark.parametrize("scenario", pytest_params(categories=["tracing"]))
def test_tracing_conformance(env: dict[str, str], scenario: dict[str, Any]) -> None:
    service = unique("conf")
    runner = ConformanceRunner(
        {"tracing": lambda ctx: tracing_target(env, str(ctx.service_name))},
        capabilities=TRACING_CAPABILITIES,
        service_name=service,
    )
    assert_scenario(runner, scenario)
