"""Factories the integration suite and the conformance runner use (owned by this repository).

Jaeger runs without auth in docker/compose.yml; /bootstrap/env carries every endpoint (query,
OTLP gRPC/HTTP, sampling, healthcheckv2, metrics, the sampling fixture instance).
"""

from __future__ import annotations

import os
import random
import string
from collections.abc import Mapping
from typing import Any

import grow_jaeger as sdk
from grow_jaeger.testing import TracingTarget

URL_VAR = "GROW_JAEGER_URL"

#: Capabilities of the deployment under test for the core scenarios ("version" = serverVersion).
CORE_CAPABILITIES: set[str] = {"version"}

#: Capabilities for the tracing scenarios: query, services, dependencies.
TRACING_CAPABILITIES: set[str] = {"query", "services", "dependencies"}


def unique(prefix: str) -> str:
    """A service name no other test (or language suite) uses: the memory store is shared."""
    run = os.environ.get("GROW_TEST_RUN_ID", "local")[:12]
    rand = "".join(random.choices(string.ascii_lowercase + string.digits, k=8))
    return f"{prefix}-py-{run}-{rand}"


def core_client(env: Mapping[str, str]) -> sdk.JaegerClient:
    """A client configured from the test environment (URLs from /bootstrap/env)."""
    return sdk.JaegerClient.from_env(env)


def async_core_client(env: Mapping[str, str]) -> sdk.AsyncJaegerClient:
    return sdk.AsyncJaegerClient.from_env(env)


def tracing_handle(env: Mapping[str, str], service_name: str, **kw: Any) -> sdk.JaegerTracingHandle:
    """Tracing to the real Jaeger: exporter settings from the bootstrap env, fast batches."""
    kw.setdefault("set_global", False)
    kw.setdefault("sampler", "always_on")
    kw.setdefault("batch", sdk.SpanBatchOptions(scheduled_delay_s=0.2))
    return sdk.init_tracing(service_name=service_name, env=env, **kw)


def tracing_target(env: Mapping[str, str], service_name: str) -> TracingTarget:
    """A ``testing.TracingTarget(handle, query)`` exporting as ``service_name``."""
    return TracingTarget(tracing_handle(env, service_name), sdk.JaegerQueryClient.from_env(env))
