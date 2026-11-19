"""The Jaeger tracing preset: env precedence, endpoints, exporters, presets, init_tracing."""

from __future__ import annotations

import dataclasses
import os
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import (
    OTLPSpanExporter as GrpcExporter,
)
from opentelemetry.exporter.otlp.proto.http.trace_exporter import (
    OTLPSpanExporter as HttpExporter,
)
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.sampling import ParentBased

import grow_jaeger as sdk
from _fixtures import Router, config, response
from grow_jaeger import setup as jsetup
from grow_jaeger.testing import InMemorySpanExporter

# --------------------------------------------------------------------------- endpoints


@pytest.mark.parametrize(
    ("protocol", "endpoint", "expected"),
    [
        ("http/protobuf", None, sdk.JAEGER_OTLP_HTTP_ENDPOINT),
        ("grpc", None, sdk.JAEGER_OTLP_GRPC_ENDPOINT),
        ("http/protobuf", "http://j:4318", "http://j:4318/v1/traces"),
        ("http/protobuf", "http://j:4318/", "http://j:4318/v1/traces"),
        ("http/protobuf", "http://j:4318/custom", "http://j:4318/custom"),
        ("grpc", "http://j:4317", "http://j:4317"),
        ("http/protobuf", "http://[bad", "http://[bad"),
    ],
)
def test_resolve_endpoint(protocol: str, endpoint: str | None, expected: str) -> None:
    assert sdk.resolve_jaeger_endpoint(protocol, endpoint) == expected


# --------------------------------------------------------------------------- env precedence


def env_config(env: dict[str, str], **kw: Any) -> sdk.JaegerTracingConfig:
    return sdk.tracing_config_from_env(env=env, **kw)


def test_env_precedence_grow_over_otel() -> None:
    env = {
        "GROW_JAEGER_OTLP_PROTOCOL": "grpc",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/protobuf",
        "GROW_JAEGER_OTLP_GRPC_ENDPOINT": "http://g:4317",
        "GROW_JAEGER_OTLP_HEADERS": "x-a=1,x%20b=two%3D",
        "OTEL_EXPORTER_OTLP_HEADERS": "x-c=3",
        "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION": "NONE",
        "OTEL_EXPORTER_OTLP_TIMEOUT": "1500",
        "OTEL_SERVICE_NAME": "svc",
        "OTEL_RESOURCE_ATTRIBUTES": "service.name=ignored,team=core",
        "OTEL_TRACES_SAMPLER": "parentbased_traceidratio",
        "OTEL_TRACES_SAMPLER_ARG": "0.25",
        "OTEL_PROPAGATORS": "tracecontext, none ,b3",
    }
    cfg = env_config(env)
    assert cfg.protocol == "grpc"
    assert cfg.endpoint == "http://g:4317"
    assert dict(cfg.headers) == {"x-a": "1", "x b": "two="}
    assert cfg.compression == "none"
    assert cfg.batch.export_timeout_s == 1.5
    assert cfg.service_name == "svc"
    assert dict(cfg.resource_attributes) == {"team": "core"}
    assert cfg.sampler == "parentbased_traceidratio" and cfg.sampler_ratio == 0.25
    assert cfg.propagators == ("tracecontext", "b3")


def test_env_endpoint_chain() -> None:
    assert env_config({"GROW_JAEGER_OTLP_HTTP_ENDPOINT": "http://h:4318"}).endpoint == (
        "http://h:4318/v1/traces"
    )
    assert env_config({"GROW_JAEGER_OTLP_ENDPOINT": "http://any:4318/x"}).endpoint == (
        "http://any:4318/x"
    )
    traces = {
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": "http://t:4318",
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://b:4318",
    }
    assert env_config(traces).endpoint == "http://t:4318"
    base = {"OTEL_EXPORTER_OTLP_ENDPOINT": "http://b:4318/"}
    assert env_config(base).endpoint == "http://b:4318/v1/traces"
    assert env_config(base, protocol="grpc").endpoint == "http://b:4318/"
    assert env_config({}).endpoint is None


def test_explicit_values_win() -> None:
    env = {
        "OTEL_SERVICE_NAME": "env-svc",
        "OTEL_EXPORTER_OTLP_TIMEOUT": "2s",
        "OTEL_EXPORTER_OTLP_CERTIFICATE": "/env/ca.pem",
        "OTEL_EXPORTER_OTLP_CLIENT_KEY": "/env/key.pem",
        "OTEL_RESOURCE_ATTRIBUTES": "a=env,b=env",
        "OTEL_TRACES_SAMPLER": "parentbased_jaeger_remote",
    }
    base = sdk.TracingConfig(
        "explicit",
        batch=sdk.SpanBatchOptions(export_timeout_s=9),
        tls=sdk.TlsConfig(ca_cert_path="/mine/ca.pem"),
        resource_attributes={"b": "mine"},
        sampler="always_off",
    )
    cfg = sdk.tracing_config_from_env(base, env=env)
    assert cfg.service_name == "explicit"
    assert cfg.batch.export_timeout_s == 9
    assert cfg.tls == sdk.TlsConfig(ca_cert_path="/mine/ca.pem", client_key_path="/env/key.pem")
    assert dict(cfg.resource_attributes) == {"a": "env", "b": "mine"}
    assert cfg.sampler == "always_off" and cfg.remote_sampler is None


def test_env_timeout_fills_default_batch_and_tls() -> None:
    cfg = env_config(
        {"OTEL_EXPORTER_OTLP_TIMEOUT": "2s", "OTEL_EXPORTER_OTLP_CERTIFICATE": "/ca.pem"},
        service_name="s",
    )
    assert cfg.batch.export_timeout_s == 2.0
    assert cfg.tls == sdk.TlsConfig(ca_cert_path="/ca.pem")


def test_remote_sampler_from_env() -> None:
    cfg = env_config(
        {
            "OTEL_TRACES_SAMPLER": "jaeger_remote",
            "OTEL_TRACES_SAMPLER_ARG": "endpoint=http://s:5778/sampling,pollingIntervalMs=500,"
            "initialSamplingRate=0.5",
            "OTEL_SERVICE_NAME": "svc",
        }
    )
    assert cfg.remote_sampler == sdk.RemoteSamplerOptions(
        sampling_url="http://s:5778/sampling",
        poll_interval_s=0.5,
        initial_sampling_rate=0.5,
        parent_based=False,
    )
    plain = env_config({"OTEL_TRACES_SAMPLER": "parentbased_jaeger_remote"})
    assert plain.remote_sampler == sdk.RemoteSamplerOptions()
    explicit = env_config(
        {"OTEL_TRACES_SAMPLER": "always_on"}, remote_sampler=sdk.RemoteSamplerOptions()
    )
    assert explicit.sampler == "parentbased_always_on"


@pytest.mark.parametrize(
    ("env", "variable"),
    [
        ({"GROW_JAEGER_OTLP_PROTOCOL": "thrift"}, "GROW_JAEGER_OTLP_PROTOCOL"),
        ({"OTEL_EXPORTER_OTLP_HEADERS": "novalue"}, "OTEL_EXPORTER_OTLP_HEADERS"),
        ({"OTEL_EXPORTER_OTLP_HEADERS": "=x"}, "OTEL_EXPORTER_OTLP_HEADERS"),
        ({"OTEL_EXPORTER_OTLP_COMPRESSION": "br"}, "OTEL_EXPORTER_OTLP_COMPRESSION"),
        ({"OTEL_EXPORTER_OTLP_TIMEOUT": "soon"}, "OTEL_EXPORTER_OTLP_TIMEOUT"),
        ({"OTEL_EXPORTER_OTLP_TIMEOUT": "0"}, "OTEL_EXPORTER_OTLP_TIMEOUT"),
        ({"OTEL_TRACES_SAMPLER": "sometimes"}, "OTEL_TRACES_SAMPLER"),
        (
            {"OTEL_TRACES_SAMPLER": "traceidratio", "OTEL_TRACES_SAMPLER_ARG": "2"},
            "OTEL_TRACES_SAMPLER_ARG",
        ),
        (
            {"OTEL_TRACES_SAMPLER": "traceidratio", "OTEL_TRACES_SAMPLER_ARG": "x"},
            "OTEL_TRACES_SAMPLER_ARG",
        ),
        (
            {
                "OTEL_TRACES_SAMPLER": "jaeger_remote",
                "OTEL_TRACES_SAMPLER_ARG": "pollingIntervalMs=x",
            },
            "OTEL_TRACES_SAMPLER_ARG",
        ),
        (
            {
                "OTEL_TRACES_SAMPLER": "jaeger_remote",
                "OTEL_TRACES_SAMPLER_ARG": "initialSamplingRate=x",
            },
            "OTEL_TRACES_SAMPLER_ARG",
        ),
    ],
)
def test_invalid_env_names_the_variable(env: dict[str, str], variable: str) -> None:
    with pytest.raises(sdk.ConfigError) as info:
        env_config(env)
    assert variable in str(info.value)


def test_from_env_defaults_to_os_environ(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GROW_JAEGER_SERVICE_NAME", "from-os")
    assert sdk.tracing_config_from_env().service_name == "from-os"


# --------------------------------------------------------------------------- presets


def test_sampling_presets() -> None:
    cfg = sdk.TracingConfig("s")
    assert sdk.SAMPLING_PRODUCTION.apply(cfg).sampler_ratio == 0.1
    assert sdk.SAMPLING_DEVELOPMENT.as_kwargs() == {"sampler": "always_on", "sampler_ratio": 1.0}
    assert sdk.SAMPLING_OFF.sampler == "always_off"
    assert sdk.sampling_ratio(0) == sdk.SamplingPreset("parentbased_always_off")
    assert sdk.sampling_ratio(1.5) is sdk.SAMPLING_FOLLOW_PARENT
    assert sdk.sampling_ratio(0.3) == sdk.SamplingPreset("parentbased_traceidratio", 0.3)


# --------------------------------------------------------------------------- exporters


def tcfg(**kw: Any) -> sdk.TracingConfig:
    return sdk.TracingConfig("svc", **kw)


def test_http_exporter(tmp_path: Path) -> None:
    exporter = sdk.jaeger_exporter_factory(
        tcfg(endpoint="http://j:4318", headers={"Authorization": "Bearer x"}, compression="none")
    )
    assert isinstance(exporter, HttpExporter)
    assert exporter._endpoint == "http://j:4318/v1/traces"
    exporter.shutdown()
    insecure = sdk.jaeger_exporter_factory(
        tcfg(tls=sdk.TlsConfig(insecure_skip_verify=True, ca_cert_pem="-----BEGIN-----"))
    )
    assert isinstance(insecure, HttpExporter)
    insecure.shutdown()
    assert jsetup._PEM_FILES and os.path.exists(jsetup._PEM_FILES[-1])
    jsetup._cleanup_pem_files()


def test_grpc_exporter(tmp_path: Path) -> None:
    plain = sdk.jaeger_exporter_factory(
        tcfg(protocol="grpc", endpoint="http://j:4317", headers={"X-Key": "v"})
    )
    assert isinstance(plain, GrpcExporter)
    plain.shutdown()
    ca = tmp_path / "ca.pem"
    ca.write_text("not really a cert")
    secure = sdk.jaeger_exporter_factory(
        tcfg(protocol="grpc", compression="none", tls=sdk.TlsConfig(ca_cert_path=str(ca)))
    )
    assert isinstance(secure, GrpcExporter)
    secure.shutdown()
    pem = sdk.jaeger_exporter_factory(
        tcfg(protocol="grpc", endpoint="https://j:4317", tls=sdk.TlsConfig(ca_cert_pem="x"))
    )
    pem.shutdown()


@pytest.mark.parametrize(
    "kw",
    [
        {"protocol": "native"},
        {"protocol": "grpc", "tls": sdk.TlsConfig(insecure_skip_verify=True)},
        {"protocol": "grpc", "tls": sdk.TlsConfig(ca_cert_path="/does/not/exist.pem")},
    ],
)
def test_exporter_config_errors(kw: dict[str, Any]) -> None:
    with pytest.raises(sdk.ConfigError):
        sdk.jaeger_exporter_factory(tcfg(**kw))


# --------------------------------------------------------------------------- init_tracing


def test_init_tracing_exports_through_the_handle() -> None:
    memory = InMemorySpanExporter()
    handle = sdk.init_tracing(
        service_name="unit-svc",
        env=False,
        set_global=False,
        span_processors=[SimpleSpanProcessor(memory)],
        **sdk.SAMPLING_DEVELOPMENT.as_kwargs(),
    )
    assert handle.endpoint == sdk.JAEGER_OTLP_HTTP_ENDPOINT
    assert handle.remote_sampler is None
    with handle.tracer("t").start_as_current_span("op"):
        pass
    assert [s.name for s in memory.spans] == ["op"]
    assert memory.spans[0].service_name == "unit-svc"
    handle.stop()
    handle.shutdown()
    assert handle.is_shut_down


def test_init_tracing_reads_env_mapping() -> None:
    handle = sdk.init_tracing(
        env={"OTEL_SERVICE_NAME": "env-svc", "GROW_JAEGER_OTLP_GRPC_ENDPOINT": "http://g:4317"},
        protocol="grpc",
        set_global=False,
    )
    assert handle.config.service_name == "env-svc"
    assert handle.endpoint == "http://g:4317"
    handle.shutdown()


def test_init_tracing_requires_a_service_name() -> None:
    with pytest.raises(sdk.ConfigError):
        sdk.init_tracing(env=False, set_global=False)


def test_init_tracing_with_remote_sampler_options() -> None:
    router = Router().add("/sampling", response("sampling.probabilistic"))
    cfg = config(sampling_url="http://s:5778/sampling")
    handle = sdk.init_tracing(
        sdk.JaegerTracingConfig(
            "svc",
            set_global=False,
            remote_sampler=sdk.RemoteSamplerOptions(config=cfg, poll_interval_s=timedelta(hours=1)),
        ),
        env=False,
    )
    sampler = handle.remote_sampler
    assert sampler is not None and sampler.service == "svc"
    assert isinstance(handle.tracer_provider.sampler, ParentBased)
    handle.shutdown()
    assert sampler.closed
    del router


def test_init_tracing_with_existing_sampler_is_not_closed() -> None:
    sampler = sdk.JaegerRemoteSampler(None, "svc", config=config(), auto_start=False)
    handle = sdk.init_tracing(
        service_name="svc", remote_sampler=sampler, env=False, set_global=False
    )
    assert handle.tracer_provider.sampler is not sampler  # parent-based wrapper
    handle.shutdown()
    assert not sampler.closed
    sampler.close()


def test_init_tracing_remote_true_and_not_parent_based(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GROW_JAEGER_SAMPLING_URL", "http://127.0.0.1:9/sampling")
    handle = sdk.init_tracing(
        service_name="svc",
        remote_sampler=True,
        env={"GROW_JAEGER_SAMPLING_URL": "http://127.0.0.1:9/sampling"},
        set_global=False,
    )
    assert handle.remote_sampler is not None
    assert handle.remote_sampler.sampling_url == "http://127.0.0.1:9/sampling"
    handle.shutdown()
    handle2 = sdk.init_tracing(
        service_name="svc",
        remote_sampler=sdk.RemoteSamplerOptions(parent_based=False),
        set_global=False,
    )
    assert handle2.tracer_provider.sampler is handle2.remote_sampler
    handle2.shutdown()
    handle3 = sdk.init_tracing(service_name="svc", remote_sampler=True, env=False, set_global=False)
    assert handle3.remote_sampler is not None
    assert handle3.remote_sampler.sampling_url == sdk.DEFAULT_SAMPLING_URL
    handle3.shutdown()


def test_init_tracing_remote_sampler_failure_shuts_down() -> None:
    with pytest.raises(sdk.ConfigError):
        sdk.init_tracing(
            service_name="svc",
            remote_sampler=sdk.RemoteSamplerOptions(poll_interval_s=0),
            env=False,
            set_global=False,
        )


def test_jaeger_tracing_config_repr_redacts_headers() -> None:
    cfg = sdk.JaegerTracingConfig("s", headers={"Authorization": "Bearer top-secret-value"})
    assert "top-secret-value" not in repr(cfg)
    assert dataclasses.replace(cfg, remote_sampler=True).remote_sampler is True


def test_ca_cert_alias_and_order() -> None:
    cfg = env_config(
        {
            "GROW_JAEGER_OTLP_CA_CERT": "/alias.pem",
            "OTEL_EXPORTER_OTLP_CERTIFICATE": "/otel.pem",
        },
        service_name="s",
    )
    assert cfg.tls == sdk.TlsConfig(ca_cert_path="/alias.pem")
    cfg = env_config(
        {"GROW_JAEGER_OTLP_CERTIFICATE": "/grow.pem", "GROW_JAEGER_OTLP_CA_CERT": "/alias.pem"},
        service_name="s",
    )
    assert cfg.tls == sdk.TlsConfig(ca_cert_path="/grow.pem")


def test_client_init_tracing_remote_sampler_uses_client_config() -> None:
    c = sdk.JaegerClient(config(sampling_url="http://127.0.0.1:9/client-sampling"))
    handle = c.init_tracing(
        service_name="svc",
        remote_sampler=sdk.RemoteSamplerOptions(poll_interval_s=3600),
        env=False,
        set_global=False,
    )
    assert handle.remote_sampler is not None
    assert handle.remote_sampler.sampling_url == "http://127.0.0.1:9/client-sampling"
    c.close()
