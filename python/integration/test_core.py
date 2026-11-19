"""Core round trips against the real Jaeger plus the core conformance scenarios."""

from __future__ import annotations

import dataclasses
from typing import Any

import pytest
from factories import CORE_CAPABILITIES, core_client

import grow_jaeger as sdk
from grow_jaeger.testing import ConformanceRunner, assert_scenario, pytest_params


def test_health_query_root(env: dict[str, str]) -> None:
    with sdk.JaegerClient(sdk.JaegerConfig(url=env["GROW_JAEGER_URL"])) as c:
        assert c.health() == sdk.HealthStatus(True, "ok", {})


def test_health_with_healthcheck_detail(client: sdk.JaegerClient) -> None:
    assert client.config.health_url, "bootstrap writes GROW_JAEGER_HEALTH_URL"
    status = client.health()
    assert status.healthy, status
    assert status.details["status"] == "StatusOK"
    assert "startTime" in status.details


def test_health_unreachable_detail(env: dict[str, str]) -> None:
    with core_client({**env, "GROW_JAEGER_HEALTH_URL": "http://127.0.0.1:9/status"}) as c:
        status = c.health()
    assert status.healthy is False
    assert status.status == "unreachable"


def test_ping(client: sdk.JaegerClient) -> None:
    assert client.ping() >= 0


def test_server_version_and_build_info(client: sdk.JaegerClient) -> None:
    assert client.server_version() == "2.23.0"
    info = client.build_info()
    assert info is not None
    assert info["gitVersion"] == "v2.23.0"
    assert info["gitCommit"]


def test_server_version_metrics_fallback(env: dict[str, str]) -> None:
    # A query URL whose "/" is no UI page (the healthcheck port) forces the metrics source.
    cfg = sdk.JaegerConfig.from_env(env)
    cfg = dataclasses.replace(cfg, url=cfg.health_url.rsplit("/", 1)[0])  # type: ignore[union-attr]
    with sdk.JaegerClient(cfg) as c:
        assert c.build_info() is None
        assert c.server_version() == "2.23.0"


def test_close_is_idempotent(env: dict[str, str]) -> None:
    c = core_client(env)
    assert c.ping() >= 0
    c.close()
    c.close()
    with pytest.raises(sdk.CancelledError):
        c.ping()


@pytest.mark.parametrize("scenario", pytest_params(categories=["core"]))
def test_core_conformance(env: dict[str, str], scenario: dict[str, Any]) -> None:
    runner = ConformanceRunner(
        {"core": lambda ctx: core_client(env)}, capabilities=CORE_CAPABILITIES
    )
    assert_scenario(runner, scenario)
