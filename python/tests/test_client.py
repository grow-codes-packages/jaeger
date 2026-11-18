"""JaegerClient / AsyncJaegerClient: health, ping, version, modules, close; JaegerConfig."""

from __future__ import annotations

import asyncio
from typing import Any

import httpx
import pytest

import grow_jaeger as sdk
from _fixtures import Router, body, config, response
from _support import MockServer, Reply, closed_port_url, make_config

TOKEN = "client-token-789"
HTML = response("serverVersion.index_html")
METRICS = 'target_info{service_name="jaeger",service_version="v2.23.0"} 1\n'
EXTRA = {
    "health_url": "http://jaeger:13133/status",
    "metrics_url": "http://jaeger:8888/metrics",
    "sampling_url": "http://jaeger:5778/sampling",
}


def client(router: Router, **kw: Any) -> sdk.JaegerClient:
    return sdk.JaegerClient(config(**kw), http_transport=router.transport())


def aclient(router: Router, **kw: Any) -> sdk.AsyncJaegerClient:
    return sdk.AsyncJaegerClient(config(**kw), http_transport=router.transport())


# --------------------------------------------------------------------------- config


def test_config_from_env_precedence() -> None:
    env = {
        "GROW_JAEGER_URL": "http://grow:16686",
        "JAEGER_QUERY_URL": "http://vendor:16686",
        "JAEGER_SAMPLING_ENDPOINT": "http://vendor:5778/sampling",
        "GROW_JAEGER_HEALTH_URL": "http://h:13133/status",
        "GROW_JAEGER_METRICS_URL": "http://m:8888/metrics",
        "GROW_JAEGER_TOKEN": TOKEN,
    }
    cfg = sdk.JaegerConfig.from_env(env)
    assert cfg.url == "http://grow:16686"
    assert cfg.sampling_url == "http://vendor:5778/sampling"
    assert cfg.health_url == "http://h:13133/status"
    assert cfg.metrics_url == "http://m:8888/metrics"
    assert cfg.token is not None and cfg.token.reveal() == TOKEN
    assert TOKEN not in repr(cfg)
    assert cfg.known_secrets() == [TOKEN]
    vendor = sdk.JaegerConfig.from_env({"JAEGER_QUERY_URL": "http://vendor:16686"})
    assert vendor.url == "http://vendor:16686"
    assert sdk.JaegerConfig.from_env({}, url="http://explicit:1").url == "http://explicit:1"
    defaults = sdk.JaegerConfig.from_env({})
    assert defaults.url == sdk.DEFAULT_URL and defaults.sampling_url == sdk.DEFAULT_SAMPLING_URL
    assert defaults.with_changes(health_url="http://x:1/status").health_url == "http://x:1/status"


@pytest.mark.parametrize(
    "kw",
    [
        {"token": "  "},
        {"sampling_url": "ftp://x"},
        {"health_url": "nope"},
        {"metrics_url": "http://"},
        {"url": "jaeger:16686"},
    ],
)
def test_config_validation(kw: dict[str, Any]) -> None:
    with pytest.raises(sdk.ConfigError):
        sdk.JaegerConfig(**kw).validate()


# --------------------------------------------------------------------------- health


def test_health_probes_the_query_root() -> None:
    router = Router().add("/", HTML)
    with client(router, token=TOKEN) as c:
        assert c.health() == sdk.HealthStatus(True, "ok", {})
    assert router.seen[0].headers["authorization"] == f"Bearer {TOKEN}"


def test_health_with_healthcheck_detail() -> None:
    router = (
        Router()
        .add("/", HTML)
        .add(
            "/status",
            response("health.status_ok"),
            httpx.Response(503, json={"healthy": False, "status": "StatusRecoverableError"}),
            httpx.Response(200, json={"status": "Degraded"}),
            httpx.Response(200, text="fine"),
            httpx.Response(500, text="fine"),
        )
    )
    c = client(router, **EXTRA)
    ok = c.health()
    assert ok.healthy and ok.status == "ok"
    assert ok.details["status"] == "StatusOK" and "startTime" in ok.details
    bad = c.health()
    assert bad == sdk.HealthStatus(
        False, "StatusRecoverableError", {"status": "StatusRecoverableError", "httpStatus": "503"}
    )
    assert c.health() == sdk.HealthStatus(False, "Degraded", {"status": "Degraded"})
    assert c.health() == sdk.HealthStatus(True, "ok", {"status": "ok"})
    assert c.health() == sdk.HealthStatus(
        False, "unhealthy", {"status": "unhealthy", "httpStatus": "500"}
    )


def test_health_root_errors() -> None:
    router = Router().add("/", httpx.Response(502, text="bad gateway"))
    status = client(router).health()
    assert status == sdk.HealthStatus(False, "unhealthy", {"httpStatus": "502"})
    assert len(router.seen) == 1  # health never retries


def test_health_unreachable_and_cancel() -> None:
    """core.health.unreachable"""
    c = sdk.JaegerClient(make_config(closed_port_url()))
    status = c.health()
    assert status.healthy is False and status.status == "unreachable"
    assert "error" in status.details
    c.close()
    with pytest.raises(sdk.CancelledError):
        c.health()


def test_health_other_errors_are_unhealthy() -> None:
    def boom(_: httpx.Request) -> httpx.Response:
        raise httpx.UnsupportedProtocol("weird")

    router = Router().add("/", boom)
    with pytest.raises(sdk.ConfigError):
        client(router).health()


# --------------------------------------------------------------------------- ping, version


def test_ping() -> None:
    router = Router().add("/api/v3/services", response("listServices.ok"))
    latency = client(router).ping()
    assert isinstance(latency, float) and latency >= 0


def test_ping_errors() -> None:
    router = Router().add("/api/v3/services", httpx.Response(401, text="no"))
    with pytest.raises(sdk.AuthenticationError):
        client(router).ping()
    with pytest.raises(sdk.UnsupportedError):
        client(Router()).ping()


def test_server_version_from_ui_page() -> None:
    router = Router().add("/", HTML)
    c = client(router)
    assert c.server_version() == "2.23.0"
    info = c.build_info()
    assert info == {
        "gitVersion": "v2.23.0",
        "gitCommit": "d723f91447f29a345c288f37eb8d1ce7807bc0be",
        "buildDate": "2026-11-04T16:02:09Z",
    }


@pytest.mark.parametrize(
    "root",
    [
        httpx.Response(200, text="<html>no version here</html>"),
        httpx.Response(200, text="JAEGER_VERSION = {broken};"),
        httpx.Response(200, text="JAEGER_VERSION = {};"),
        httpx.Response(404, text="404 page not found"),
    ],
)
def test_server_version_metrics_fallback(root: httpx.Response) -> None:
    router = Router().add("/", root).add("/metrics", httpx.Response(200, text=METRICS))
    assert client(router, **EXTRA).server_version() == "2.23.0"


def test_server_version_unsupported() -> None:
    router = Router().add("/", httpx.Response(200, text="<html/>"))
    with pytest.raises(sdk.UnsupportedError):
        client(router).server_version()
    router.add("/metrics", httpx.Response(200, text="# nothing\n"))
    with pytest.raises(sdk.UnsupportedError):
        client(router, **EXTRA).server_version()
    router2 = Router().add("/", httpx.Response(200, text="x")).add("/metrics", httpx.Response(500))
    with pytest.raises(sdk.UnsupportedError):
        client(router2, retry=sdk.RetryPolicy(max_retries=0), **EXTRA).server_version()


def test_server_version_root_error_propagates() -> None:
    router = Router().add("/", httpx.Response(403, text="denied"))
    with pytest.raises(sdk.PermissionDeniedError):
        client(router).server_version()


# --------------------------------------------------------------------------- modules


def test_query_property_shares_connections() -> None:
    router = Router().add("/api/v3/services", response("listServices.ok"))
    c = client(router)
    assert c.query is c.query
    assert "jaeger" in c.query.list_services()
    resp = c.request(sdk.Request("GET", "/api/v3/services"))
    assert resp.ok
    c.query.close()  # shared: no-op
    assert c.query.list_services()
    c.close()
    with pytest.raises(sdk.CancelledError):
        c.query.list_services()


def test_sampling_strategy() -> None:
    router = Router().add(
        "/sampling", response("sampling.per_operation"), response("sampling.missing_service")
    )
    c = client(router, **EXTRA)
    strategy = c.sampling_strategy("sdk-sampling-ops")
    assert strategy.operation_rate("GET /slow") == 0.4
    assert router.params("/sampling") == {"service": "sdk-sampling-ops"}
    with pytest.raises(sdk.ValidationError):
        c.sampling_strategy("x")
    with pytest.raises(sdk.ConfigError):
        c.sampling_strategy("")


def test_init_tracing_and_remote_sampler_are_closed_with_the_client() -> None:
    router = Router().add("/sampling", response("sampling.probabilistic"))
    c = client(router, **EXTRA)
    handle = c.init_tracing(service_name="svc", env=False, set_global=False)
    sampler = c.remote_sampler("svc", 3600)
    assert sampler.ready(5)
    assert sampler.strategy is not None and sampler.strategy.sampling_rate == 0.25
    c.close()
    c.close()
    assert handle.is_shut_down and sampler.closed
    with pytest.raises(sdk.ConfigError):
        c.init_tracing(service_name="svc", env=False, set_global=False)
    with pytest.raises(sdk.ConfigError):
        c.remote_sampler("svc")


def test_client_defaults_to_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("JAEGER_QUERY_URL", "http://from-env:16686")
    c = sdk.JaegerClient()
    assert c.config.url == "http://from-env:16686"
    assert isinstance(c.resources, sdk.ResourceRegistry)
    assert "JaegerClient" in repr(c)
    c.close()
    assert sdk.JaegerClient.from_env({}, url="http://x:1").config.url == "http://x:1"


def test_real_http_server_round_trip(server: MockServer) -> None:
    server.enqueue("/", Reply(200, HTML.text))
    server.enqueue("/api/v3/services", Reply(200, body("listServices.ok")))
    with sdk.JaegerClient(make_config(server.url, token=TOKEN)) as c:
        assert c.server_version() == "2.23.0"
        assert c.ping() >= 0
        assert TOKEN not in repr(c)
    ping = next(s for s in server.seen if s.path == "/api/v3/services")
    assert ping.headers["Authorization"] == f"Bearer {TOKEN}"


# --------------------------------------------------------------------------- async


async def test_async_client_surface() -> None:
    router = (
        Router()
        .add("/", HTML, httpx.Response(200, text="x"), httpx.Response(500, text="x"))
        .add("/status", response("health.status_ok"))
        .add("/api/v3/services", response("listServices.ok"))
        .add("/metrics", httpx.Response(200, text=METRICS))
        .add("/sampling", response("sampling.rate_limiting"))
    )
    async with aclient(router, retry=sdk.RetryPolicy(max_retries=0), **EXTRA) as c:
        assert (await c.health()).details["status"] == "StatusOK"
        assert await c.ping() >= 0
        assert await c.server_version() == "2.23.0"  # metrics fallback
        assert (await c.health()).status == "unhealthy"
        with pytest.raises(sdk.UnavailableError):
            await c.build_info()
        assert (await c.sampling_strategy("sdk-sampling-rate")).max_traces_per_second == 5
        assert "jaeger" in await c.query.list_services()
        assert c.query is c.query
        assert (await c.request(sdk.Request("GET", "/api/v3/services"))).ok
        handle = c.init_tracing(service_name="svc", env=False, set_global=False)
        sampler = c.remote_sampler("svc", 3600, auto_start=False)
        assert isinstance(c.resources, sdk.ResourceRegistry)
        assert "AsyncJaegerClient" in repr(c)
    assert handle.is_shut_down and sampler.closed
    with pytest.raises(sdk.ConfigError):
        c.init_tracing(service_name="svc", env=False, set_global=False)
    with pytest.raises(sdk.ConfigError):
        c.remote_sampler("svc")


async def test_async_health_root_and_unreachable() -> None:
    router = Router().add("/", httpx.Response(503, text="down"))
    assert (await aclient(router).health()).status == "unhealthy"
    c = sdk.AsyncJaegerClient(make_config(closed_port_url()))
    assert (await c.health()).status == "unreachable"
    html = aclient(Router().add("/", HTML))
    assert (await html.health()) == sdk.HealthStatus(True, "ok", {})
    assert await html.server_version() == "2.23.0"
    await c.aclose()
    await c.close()


async def test_async_server_version_unsupported_and_cancel() -> None:
    router = Router().add("/", httpx.Response(200, text="<html/>"))
    with pytest.raises(sdk.UnsupportedError):
        await aclient(router).server_version()
    slow_router = Router().add("/api/v3/services", response("listServices.ok"))
    c = aclient(slow_router)
    task = asyncio.ensure_future(c.ping())
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task


def test_async_client_defaults_to_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GROW_JAEGER_URL", "http://env:16686")
    assert sdk.AsyncJaegerClient().config.url == "http://env:16686"
    assert sdk.AsyncJaegerClient.from_env({}).config.url == sdk.DEFAULT_URL
