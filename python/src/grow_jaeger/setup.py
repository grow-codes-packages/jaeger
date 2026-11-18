"""The Jaeger tracing preset: OTLP exporter defaults (HTTP/protobuf on 4318 or gRPC on 4317),
environment precedence, sampling presets and the optional remote sampler.

Jaeger ingests OTLP natively, so the official OpenTelemetry OTLP exporters are used as-is.
"""

from __future__ import annotations

import atexit
import contextlib
import dataclasses
import os
import tempfile
from collections.abc import Mapping, Sequence
from datetime import timedelta
from typing import Any, Literal, TypeAlias
from urllib.parse import unquote, urlsplit

from opentelemetry.sdk.trace import SpanProcessor
from opentelemetry.sdk.trace.export import SpanExporter
from opentelemetry.sdk.trace.id_generator import IdGenerator
from opentelemetry.sdk.trace.sampling import ParentBased, Sampler

from ._core import ConfigError, TlsConfig, parse_duration
from ._tracing import (
    SAMPLERS,
    PropagatorFactory,
    SamplerName,
    SpanBatchOptions,
    TraceProtocol,
    TracingConfig,
    TracingHandle,
    init_tracing_with,
)
from .config import JaegerConfig
from .sampler import (
    DEFAULT_INITIAL_SAMPLING_RATE,
    DEFAULT_SAMPLING_POLL_INTERVAL_S,
    JaegerRemoteSampler,
)

__all__ = [
    "JAEGER_DEFAULT_PROTOCOL",
    "JAEGER_OTLP_GRPC_ENDPOINT",
    "JAEGER_OTLP_HTTP_ENDPOINT",
    "SAMPLING_DEVELOPMENT",
    "SAMPLING_FOLLOW_PARENT",
    "SAMPLING_OFF",
    "SAMPLING_PRODUCTION",
    "JaegerTracingConfig",
    "JaegerTracingHandle",
    "RemoteSamplerOptions",
    "RemoteSamplerSetting",
    "SamplingPreset",
    "init_tracing",
    "jaeger_exporter_factory",
    "resolve_jaeger_endpoint",
    "sampling_ratio",
    "tracing_config_from_env",
]

#: Default OTLP/HTTP traces endpoint of a Jaeger all-in-one or collector.
JAEGER_OTLP_HTTP_ENDPOINT = "http://127.0.0.1:4318/v1/traces"
#: Default OTLP/gRPC endpoint of a Jaeger all-in-one or collector.
JAEGER_OTLP_GRPC_ENDPOINT = "http://127.0.0.1:4317"
#: Default protocol of the Jaeger preset (works through HTTP proxies).
JAEGER_DEFAULT_PROTOCOL: TraceProtocol = "http/protobuf"

_GROW_OTLP = "GROW_JAEGER_OTLP_"
_OTEL_TRACES = "OTEL_EXPORTER_OTLP_TRACES_"
_OTEL = "OTEL_EXPORTER_OTLP_"


# --------------------------------------------------------------------------- remote sampler


@dataclasses.dataclass(frozen=True)
class RemoteSamplerOptions:
    """How ``init_tracing`` builds a :class:`JaegerRemoteSampler` (owned by the handle).

    ``service_name`` defaults to the tracing ``service_name``; ``sampling_url`` and the
    transport settings to ``config`` (default :meth:`JaegerConfig.from_env`). With
    ``parent_based`` (default) the remote sampler is the root of a ``ParentBased`` sampler.
    """

    service_name: str | None = None
    sampling_url: str | None = None
    poll_interval_s: float | timedelta = DEFAULT_SAMPLING_POLL_INTERVAL_S
    initial_sampling_rate: float = DEFAULT_INITIAL_SAMPLING_RATE
    parent_based: bool = True
    config: JaegerConfig | None = None


#: ``remote_sampler`` setting: an existing sampler (not closed by the handle), ``True`` (all
#: defaults) or :class:`RemoteSamplerOptions`.
RemoteSamplerSetting: TypeAlias = "JaegerRemoteSampler | RemoteSamplerOptions | bool"


@dataclasses.dataclass(frozen=True, repr=False)
class JaegerTracingConfig(TracingConfig):
    """:class:`TracingConfig` plus the Jaeger remote sampler (``remote_sampler``).

    When ``remote_sampler`` is set it replaces ``sampler``/``sampler_ratio``.
    """

    remote_sampler: JaegerRemoteSampler | RemoteSamplerOptions | bool | None = None


# --------------------------------------------------------------------------- sampling presets


@dataclasses.dataclass(frozen=True)
class SamplingPreset:
    """A named head-sampling choice; :meth:`apply` sets ``sampler`` and ``sampler_ratio``."""

    sampler: SamplerName
    ratio: float = 1.0

    def apply(self, config: TracingConfig) -> TracingConfig:
        """Copy of ``config`` with this preset's sampler and ratio."""
        return dataclasses.replace(config, sampler=self.sampler, sampler_ratio=self.ratio)

    def as_kwargs(self) -> dict[str, Any]:
        """``{"sampler": …, "sampler_ratio": …}`` for ``init_tracing(**preset.as_kwargs())``."""
        return {"sampler": self.sampler, "sampler_ratio": self.ratio}


#: Record every trace (``always_on``).
SAMPLING_DEVELOPMENT = SamplingPreset("always_on", 1.0)
#: Keep 10 % of new traces and follow the parent decision (``parentbased_traceidratio`` 0.1).
SAMPLING_PRODUCTION = SamplingPreset("parentbased_traceidratio", 0.1)
#: Record new traces and follow the parent decision (the OpenTelemetry default).
SAMPLING_FOLLOW_PARENT = SamplingPreset("parentbased_always_on", 1.0)
#: Record nothing (``always_off``).
SAMPLING_OFF = SamplingPreset("always_off", 1.0)


def sampling_ratio(ratio: float) -> SamplingPreset:
    """``parentbased_traceidratio`` preset; ``<= 0`` → ``parentbased_always_off``, ``>= 1`` →
    ``parentbased_always_on``."""
    if ratio <= 0:
        return SamplingPreset("parentbased_always_off", 1.0)
    if ratio >= 1:
        return SAMPLING_FOLLOW_PARENT
    return SamplingPreset("parentbased_traceidratio", ratio)


# --------------------------------------------------------------------------- endpoints + env


def _has_path(url: str) -> bool:
    try:
        path = urlsplit(url).path
    except ValueError:
        return True
    return path not in ("", "/")


def _traces_path(base: str) -> str:
    return base.rstrip("/") + "/v1/traces"


def resolve_jaeger_endpoint(protocol: str, endpoint: str | None = None) -> str:
    """Exporter URL for a protocol.

    For ``http/protobuf`` an endpoint without a path (a base such as ``http://jaeger:4318``)
    gets ``/v1/traces``; any other endpoint is used as-is. Without an endpoint: the Jaeger
    default for the protocol.
    """
    if not endpoint:
        return JAEGER_OTLP_GRPC_ENDPOINT if protocol == "grpc" else JAEGER_OTLP_HTTP_ENDPOINT
    if protocol != "grpc" and not _has_path(endpoint):
        return _traces_path(endpoint)
    return endpoint


class _Env:
    def __init__(self, env: Mapping[str, str]) -> None:
        self._env = env

    def hit(self, *names: str) -> tuple[str, str] | None:
        for name in names:
            value = self._env.get(name)
            if value is not None and value.strip():
                return name, value.strip()
        return None

    def otlp(self, suffix: str) -> tuple[str, str] | None:
        """``GROW_JAEGER_OTLP_<s>`` > ``OTEL_EXPORTER_OTLP_TRACES_<s>`` >
        ``OTEL_EXPORTER_OTLP_<s>``."""
        return self.hit(_GROW_OTLP + suffix, _OTEL_TRACES + suffix, _OTEL + suffix)


def _key_values(raw: str, name: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for part in raw.split(","):
        if not part.strip():
            continue
        key, sep, value = part.partition("=")
        if not sep or not key.strip():
            raise ConfigError(f"config: {name} must be a list of key=value pairs")
        out[unquote(key.strip())] = unquote(value.strip())
    return out


def _remote_from_env(raw: str | None, name: str, parent_based: bool) -> RemoteSamplerOptions:
    values = _key_values(raw, name) if raw else {}
    kwargs: dict[str, Any] = {"parent_based": parent_based}
    if "endpoint" in values:
        kwargs["sampling_url"] = values["endpoint"]
    if "pollingIntervalMs" in values:
        try:
            kwargs["poll_interval_s"] = parse_duration(values["pollingIntervalMs"])
        except ValueError:
            raise ConfigError(f"config: {name} has an invalid pollingIntervalMs") from None
    if "initialSamplingRate" in values:
        try:
            kwargs["initial_sampling_rate"] = float(values["initialSamplingRate"])
        except ValueError:
            raise ConfigError(f"config: {name} has an invalid initialSamplingRate") from None
    return RemoteSamplerOptions(**kwargs)


_DEFAULTS: dict[str, Any] = {
    f.name: (f.default_factory() if f.default_factory is not dataclasses.MISSING else f.default)
    for f in dataclasses.fields(JaegerTracingConfig)
    if f.name != "service_name"
}


def _explicit(config: TracingConfig | None, overrides: Mapping[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    if config is not None:
        if config.service_name:
            out["service_name"] = config.service_name
        for name, default in _DEFAULTS.items():
            if not hasattr(config, name):
                continue
            value = getattr(config, name)
            if value != default and value is not None:
                out[name] = value
    out.update({k: v for k, v in overrides.items() if v is not None})
    return out


def _env_values(env: Mapping[str, str], protocol_hint: str | None) -> dict[str, Any]:
    chain = _Env(env)
    values: dict[str, Any] = {}
    protocol_hit = chain.otlp("PROTOCOL")
    if protocol_hit is not None:
        name, raw = protocol_hit
        if raw.lower() not in ("grpc", "http/protobuf"):
            raise ConfigError(f"config: {name} must be grpc or http/protobuf")
        values["protocol"] = raw.lower()
    protocol = protocol_hint or values.get("protocol") or JAEGER_DEFAULT_PROTOCOL
    specific = _GROW_OTLP + ("GRPC_ENDPOINT" if protocol == "grpc" else "HTTP_ENDPOINT")
    grow = chain.hit(specific, _GROW_OTLP + "ENDPOINT")
    traces = chain.hit(_OTEL_TRACES + "ENDPOINT")
    base = chain.hit(_OTEL + "ENDPOINT")
    if grow is not None:
        values["endpoint"] = resolve_jaeger_endpoint(protocol, grow[1])
    elif traces is not None:
        values["endpoint"] = traces[1]
    elif base is not None:
        values["endpoint"] = base[1] if protocol == "grpc" else _traces_path(base[1])
    headers = chain.otlp("HEADERS")
    if headers is not None:
        values["headers"] = _key_values(headers[1], headers[0])
    compression = chain.otlp("COMPRESSION")
    if compression is not None:
        if compression[1].lower() not in ("gzip", "none"):
            raise ConfigError(f"config: {compression[0]} must be gzip or none")
        values["compression"] = compression[1].lower()
    timeout = chain.otlp("TIMEOUT")
    if timeout is not None:
        try:
            seconds = parse_duration(timeout[1])
        except ValueError:
            seconds = 0.0
        if not seconds > 0:
            raise ConfigError(f"config: {timeout[0]} is not a valid duration")
        values["export_timeout_s"] = seconds
    tls: dict[str, str] = {}
    for field, suffix in (
        ("ca_cert_path", "CERTIFICATE"),
        ("client_cert_path", "CLIENT_CERTIFICATE"),
        ("client_key_path", "CLIENT_KEY"),
    ):
        hit = chain.otlp(suffix)
        if hit is not None:
            tls[field] = hit[1]
    if tls:
        values["tls"] = tls
    resource_hit = chain.hit("OTEL_RESOURCE_ATTRIBUTES")
    resource = _key_values(resource_hit[1], resource_hit[0]) if resource_hit else {}
    service = chain.hit("GROW_JAEGER_SERVICE_NAME", "OTEL_SERVICE_NAME")
    if service is not None:
        values["service_name"] = service[1]
    elif resource.get("service.name"):
        values["service_name"] = resource["service.name"]
    resource.pop("service.name", None)
    if resource:
        values["resource_attributes"] = resource
    sampler = chain.hit("OTEL_TRACES_SAMPLER")
    arg = chain.hit("OTEL_TRACES_SAMPLER_ARG")
    if sampler is not None:
        name = sampler[1].lower()
        if name in ("jaeger_remote", "parentbased_jaeger_remote"):
            values["remote_sampler"] = _remote_from_env(
                arg[1] if arg else None,
                "OTEL_TRACES_SAMPLER_ARG",
                parent_based=name == "parentbased_jaeger_remote",
            )
        elif name not in SAMPLERS:
            raise ConfigError(f"config: {sampler[0]} names an unknown sampler")
        else:
            values["sampler"] = name
            if name.endswith("traceidratio") and arg is not None:
                try:
                    ratio = float(arg[1])
                except ValueError:
                    ratio = -1.0
                if not 0.0 <= ratio <= 1.0:
                    raise ConfigError(f"config: {arg[0]} must be a number between 0 and 1")
                values["sampler_ratio"] = ratio
    propagators = chain.hit("OTEL_PROPAGATORS")
    if propagators is not None:
        names = [p.strip() for p in propagators[1].split(",")]
        values["propagators"] = tuple(p for p in names if p and p != "none")
    return values


def tracing_config_from_env(
    config: TracingConfig | None = None,
    *,
    env: Mapping[str, str] | None = None,
    **overrides: Any,
) -> JaegerTracingConfig:
    """A :class:`JaegerTracingConfig` from the environment beneath ``config``/``overrides``.

    A field of ``config`` counts as explicit when it differs from the field default; keyword
    ``overrides`` (any :class:`JaegerTracingConfig` field) are always explicit. Per field:
    explicit > ``GROW_JAEGER_OTLP_*`` > ``OTEL_EXPORTER_OTLP_TRACES_*`` >
    ``OTEL_EXPORTER_OTLP_*`` > Jaeger default.

    ============================  ====================================================
    Field                         Variables
    ============================  ====================================================
    ``protocol``                  ``…_PROTOCOL`` (``grpc``, ``http/protobuf``)
    ``endpoint``                  ``GROW_JAEGER_OTLP_GRPC_ENDPOINT`` /
                                  ``GROW_JAEGER_OTLP_HTTP_ENDPOINT`` (by protocol),
                                  ``GROW_JAEGER_OTLP_ENDPOINT``,
                                  ``OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`` (as-is),
                                  ``OTEL_EXPORTER_OTLP_ENDPOINT`` (base; HTTP appends
                                  ``/v1/traces``)
    ``headers``                   ``…_HEADERS`` (``k=v,k2=v2``, percent-decoded)
    ``compression``               ``…_COMPRESSION`` (``gzip``, ``none``)
    ``batch.export_timeout_s``    ``…_TIMEOUT`` (milliseconds or ``10s``)
    ``tls``                       ``…_CERTIFICATE``, ``…_CLIENT_CERTIFICATE``,
                                  ``…_CLIENT_KEY`` (paths)
    ``service_name``              ``GROW_JAEGER_SERVICE_NAME``, ``OTEL_SERVICE_NAME``,
                                  ``service.name`` in ``OTEL_RESOURCE_ATTRIBUTES``
    ``resource_attributes``       ``OTEL_RESOURCE_ATTRIBUTES`` (explicit keys win)
    ``sampler``/``sampler_ratio`` ``OTEL_TRACES_SAMPLER``, ``OTEL_TRACES_SAMPLER_ARG``;
                                  ``jaeger_remote`` / ``parentbased_jaeger_remote`` set
                                  ``remote_sampler`` (arg ``endpoint=…,
                                  pollingIntervalMs=…,initialSamplingRate=…``)
    ``propagators``               ``OTEL_PROPAGATORS``
    ============================  ====================================================

    ``env`` defaults to ``os.environ``. A present but invalid value raises
    :class:`ConfigError` naming the variable, never its value. The result is not validated
    (``init_tracing`` does that); ``service_name`` is ``""`` when nothing sets it.
    """
    explicit = _explicit(config, overrides)
    found = _env_values(os.environ if env is None else env, explicit.get("protocol"))
    special = ("export_timeout_s", "tls")
    values: dict[str, Any] = {k: v for k, v in found.items() if k not in special}
    if "sampler" in explicit and "remote_sampler" not in explicit:
        values.pop("remote_sampler", None)
    if "remote_sampler" in explicit:
        values.pop("sampler", None)
    env_resource = values.pop("resource_attributes", {})
    values.update(explicit)
    if env_resource:
        values["resource_attributes"] = {
            **env_resource,
            **dict(explicit.get("resource_attributes") or {}),
        }
    if "export_timeout_s" in found:
        batch: SpanBatchOptions = values.get("batch", SpanBatchOptions())
        if batch.export_timeout_s == SpanBatchOptions().export_timeout_s:
            values["batch"] = dataclasses.replace(batch, export_timeout_s=found["export_timeout_s"])
    if "tls" in found:
        merged: dict[str, Any] = dict(found["tls"])
        merged.update({k: v for k, v in _tls_fields(values.get("tls")).items() if v})
        values["tls"] = TlsConfig(**merged)
    values.setdefault("service_name", "")
    return JaegerTracingConfig(**values)


def _tls_fields(tls: TlsConfig | None) -> dict[str, Any]:
    return {} if tls is None else dataclasses.asdict(tls)


# --------------------------------------------------------------------------- exporter


_PEM_FILES: list[str] = []


def _cleanup_pem_files() -> None:
    for path in _PEM_FILES:
        with contextlib.suppress(OSError):
            os.unlink(path)


atexit.register(_cleanup_pem_files)


def _pem_file(pem: str) -> str:
    fd, path = tempfile.mkstemp(prefix="grow-jaeger-ca-", suffix=".pem")
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        fh.write(pem)
    _PEM_FILES.append(path)
    return path


def _read(path: str | None) -> bytes | None:
    if path is None:
        return None
    try:
        with open(path, "rb") as fh:
            return fh.read()
    except OSError:
        raise ConfigError("tracing: cannot read a TLS file (check the configured path)") from None


def _grpc_exporter(config: TracingConfig, endpoint: str) -> SpanExporter:
    try:
        import grpc  # noqa: PLC0415 - optional extra
        from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import (  # noqa: PLC0415
            OTLPSpanExporter as GrpcSpanExporter,
        )
    except ImportError as exc:  # pragma: no cover - depends on the installed extras
        raise ConfigError(
            "tracing: protocol grpc needs the 'grpc' extra (pip install 'grow-jaeger[grpc]')"
        ) from exc
    tls = config.tls or TlsConfig()
    if tls.insecure_skip_verify:
        raise ConfigError("tracing: insecure_skip_verify is not supported by the gRPC exporter")
    ca = _read(tls.ca_cert_path) or (tls.ca_cert_pem.encode() if tls.ca_cert_pem else None)
    cert = _read(tls.client_cert_path)
    key = _read(tls.client_key_path)
    secure = endpoint.startswith("https:") or ca is not None or cert is not None
    credentials = (
        grpc.ssl_channel_credentials(root_certificates=ca, private_key=key, certificate_chain=cert)
        if secure
        else None
    )
    return GrpcSpanExporter(
        endpoint=endpoint,
        insecure=not secure,
        credentials=credentials,
        headers={k.lower(): v for k, v in config.headers.items()},
        timeout=config.batch.export_timeout_s,
        compression=grpc.Compression.Gzip
        if config.compression == "gzip"
        else grpc.Compression.NoCompression,
    )


def _http_exporter(config: TracingConfig, endpoint: str) -> SpanExporter:
    import requests  # noqa: PLC0415 - dependency of the OTLP/HTTP exporter
    from opentelemetry.exporter.otlp.proto.http import Compression  # noqa: PLC0415
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import (  # noqa: PLC0415
        OTLPSpanExporter as HttpSpanExporter,
    )

    tls = config.tls or TlsConfig()
    ca_path = tls.ca_cert_path or (_pem_file(tls.ca_cert_pem) if tls.ca_cert_pem else None)
    session = None
    if tls.insecure_skip_verify:
        session = requests.Session()
        session.verify = False
    return HttpSpanExporter(
        endpoint=endpoint,
        certificate_file=ca_path,
        client_key_file=tls.client_key_path,
        client_certificate_file=tls.client_cert_path,
        headers=dict(config.headers),
        timeout=config.batch.export_timeout_s,
        compression=Compression.Gzip if config.compression == "gzip" else Compression.NoCompression,
        session=session,
    )


def jaeger_exporter_factory(config: TracingConfig) -> SpanExporter:
    """The OTLP span exporter for a config (the exporter factory of ``init_tracing_with``).

    ``http/protobuf`` → ``opentelemetry-exporter-otlp-proto-http``; ``grpc`` →
    ``opentelemetry-exporter-otlp-proto-grpc`` (extra ``grow-jaeger[grpc]``). Headers,
    compression, export timeout and TLS come from the config. ``native`` raises
    :class:`ConfigError` (Jaeger ingests OTLP).
    """
    if config.protocol == "native":
        raise ConfigError("tracing: Jaeger has no native protocol; use grpc or http/protobuf")
    endpoint = resolve_jaeger_endpoint(config.protocol, config.endpoint)
    if config.protocol == "grpc":
        return _grpc_exporter(config, endpoint)
    return _http_exporter(config, endpoint)


# --------------------------------------------------------------------------- init_tracing


class JaegerTracingHandle(TracingHandle):
    """A running Jaeger tracing setup: :class:`TracingHandle` plus ``endpoint`` and the
    ``remote_sampler`` in use. :meth:`shutdown` also closes a sampler the handle created."""

    def __init__(
        self,
        base: TracingHandle,
        *,
        endpoint: str,
        remote_sampler: JaegerRemoteSampler | None = None,
        owns_sampler: bool = False,
    ) -> None:
        super().__init__(
            base.config, base.tracer_provider, base.propagator, is_global=base.is_global
        )
        self.endpoint = endpoint
        self.remote_sampler = remote_sampler
        self._owns_sampler = owns_sampler

    def shutdown(self) -> None:
        """Flush and stop the provider (and an owned remote sampler); idempotent."""
        first = not self.is_shut_down
        try:
            super().shutdown()
        finally:
            if first and self._owns_sampler and self.remote_sampler is not None:
                self.remote_sampler.close()

    #: Alias so a handle can live in a client's resource registry.
    stop = shutdown


def _remote_sampler(
    setting: JaegerRemoteSampler | RemoteSamplerOptions | bool,
    service_name: str,
    env: Mapping[str, str] | Literal[False] | None,
) -> tuple[JaegerRemoteSampler, bool, bool]:
    """``(sampler, owned, parent_based)``."""
    if isinstance(setting, JaegerRemoteSampler):
        return setting, False, True
    options = setting if isinstance(setting, RemoteSamplerOptions) else RemoteSamplerOptions()
    config = options.config
    if config is None:
        config = JaegerConfig() if env is False else JaegerConfig.from_env(env)
    sampler = JaegerRemoteSampler(
        options.sampling_url,
        options.service_name or service_name,
        options.poll_interval_s,
        initial_sampling_rate=options.initial_sampling_rate,
        config=config,
    )
    return sampler, True, options.parent_based


def init_tracing(
    config: TracingConfig | None = None,
    *,
    env: Mapping[str, str] | Literal[False] | None = None,
    propagator_factories: Mapping[str, PropagatorFactory] | None = None,
    span_processors: Sequence[SpanProcessor] = (),
    id_generator: IdGenerator | None = None,
    **overrides: Any,
) -> JaegerTracingHandle:
    """Set up OpenTelemetry tracing for Jaeger (SPEC §5).

    OTLP ``http/protobuf`` to ``http://127.0.0.1:4318/v1/traces`` by default, or ``grpc`` to
    ``http://127.0.0.1:4317``. Pass a :class:`TracingConfig` / :class:`JaegerTracingConfig`
    and/or its fields as keywords (``service_name="checkout"``). The environment fills every
    field left unset (see :func:`tracing_config_from_env`; ``env`` defaults to
    ``os.environ``, ``env=False`` ignores it). With ``remote_sampler`` the Jaeger remote
    strategy decides sampling (as the root of a ``ParentBased`` sampler unless
    ``parent_based=False``).

    Raises :class:`ConfigError` for invalid settings.
    """
    if env is False:
        effective = tracing_config_from_env(config, env={}, **overrides)
    else:
        effective = tracing_config_from_env(config, env=env, **overrides)
    endpoint = resolve_jaeger_endpoint(effective.protocol, effective.endpoint)
    effective = dataclasses.replace(effective, endpoint=endpoint)
    remote = effective.remote_sampler
    base = init_tracing_with(
        effective,
        jaeger_exporter_factory,
        propagator_factories=propagator_factories,
        span_processors=span_processors,
        id_generator=id_generator,
    )
    if remote is None or remote is False:
        return JaegerTracingHandle(base, endpoint=endpoint)
    try:
        sampler, owned, parent_based = _remote_sampler(remote, effective.service_name, env)
    except BaseException:
        base.shutdown()
        raise
    root: Sampler = ParentBased(root=sampler) if parent_based else sampler
    # init_tracing_with has no sampler parameter; tracers read the provider's sampler when
    # they are created, and none exists yet, so replacing it here affects every tracer.
    base.tracer_provider.sampler = root
    return JaegerTracingHandle(base, endpoint=endpoint, remote_sampler=sampler, owns_sampler=owned)
