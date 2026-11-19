"""Configuration for the Jaeger client.

This file is owned by the service package (generated once by sdk-kit).
"""

from __future__ import annotations

import dataclasses
from collections.abc import Mapping
from typing import Any, ClassVar, TypeVar
from urllib.parse import urlsplit

from ._core import BaseConfig, ConfigError, EnvReader, Secret

__all__ = ["DEFAULT_SAMPLING_URL", "DEFAULT_URL", "JaegerConfig"]

#: Default query endpoint (UI and HTTP query API, port 16686).
DEFAULT_URL = "http://127.0.0.1:16686"
#: Default remote sampling endpoint (port 5778).
DEFAULT_SAMPLING_URL = "http://127.0.0.1:5778/sampling"

_C = TypeVar("_C", bound="JaegerConfig")


def _is_http_url(value: str) -> bool:
    try:
        parts = urlsplit(value)
    except ValueError:
        return False
    return parts.scheme in ("http", "https") and bool(parts.hostname)


@dataclasses.dataclass(frozen=True, repr=False)
class JaegerConfig(BaseConfig):
    """Immutable settings for :class:`~grow_jaeger.JaegerClient`.

    Common fields (``url``, timeouts, ``retry``, ``tls``, ``proxy``, ``headers``,
    ``user_agent_suffix``, ``hooks``, ``logger``) come from :class:`BaseConfig`. Durations are
    float seconds; ``datetime.timedelta`` is accepted. ``token`` accepts a ``str`` and is stored
    as a :class:`Secret`; ``repr()`` never shows it.

    Attributes:
        url: query API base URL (port 16686). A reverse-proxy base path is kept
            (``https://host/jaeger`` → ``https://host/jaeger/api/v3/...``).
        token: optional bearer token sent as ``Authorization: Bearer <token>`` (stock Jaeger has
            no auth; use it for a reverse proxy or ``bearer_token_propagation``).
        sampling_url: remote sampling endpoint (port 5778, path ``/sampling``).
        health_url: optional healthcheckv2 ``/status`` URL (port 13133); when set,
            :meth:`~grow_jaeger.JaegerClient.health` adds its detail.
        metrics_url: optional Prometheus ``/metrics`` URL (port 8888);
            :meth:`~grow_jaeger.JaegerClient.server_version` falls back to its ``target_info``.

    Environment (``from_env``), prefix ``GROW_JAEGER_``: ``URL`` (then ``JAEGER_QUERY_URL``),
    ``TOKEN``, ``SAMPLING_URL`` (then ``JAEGER_SAMPLING_ENDPOINT``), ``HEALTH_URL``,
    ``METRICS_URL``, ``CONNECT_TIMEOUT``, ``REQUEST_TIMEOUT``, ``TOTAL_TIMEOUT``,
    ``MAX_RETRIES``, ``CA_CERT``, ``CLIENT_CERT``, ``CLIENT_KEY``, ``INSECURE_SKIP_VERIFY``,
    ``PROXY``.
    """

    #: Vendor variables per GROW_* suffix, in precedence order.
    ENV_VENDOR: ClassVar[Mapping[str, tuple[str, ...]]] = {
        "URL": ("JAEGER_QUERY_URL",),
        "SAMPLING_URL": ("JAEGER_SAMPLING_ENDPOINT",),
    }

    url: str = DEFAULT_URL
    token: Secret | None = None
    sampling_url: str = DEFAULT_SAMPLING_URL
    health_url: str | None = None
    metrics_url: str | None = None

    @classmethod
    def _env_values(cls, reader: EnvReader) -> dict[str, Any]:
        values = super()._env_values(reader)
        token = reader.string("TOKEN", cls.ENV_VENDOR.get("TOKEN", ()))
        if token is not None:
            values["token"] = Secret(token)
        for field, suffix in (
            ("sampling_url", "SAMPLING_URL"),
            ("health_url", "HEALTH_URL"),
            ("metrics_url", "METRICS_URL"),
        ):
            value = reader.string(suffix, cls.ENV_VENDOR.get(suffix, ()))
            if value is not None:
                values[field] = value
        return values

    def validate(self: _C) -> _C:
        """Common checks plus the Jaeger rules; raises :class:`ConfigError`."""
        super().validate()
        if self.token is not None and not self.token.reveal().strip():
            raise ConfigError("config: token must not be blank")
        for name in ("sampling_url", "health_url", "metrics_url"):
            value = getattr(self, name)
            if value is not None and not _is_http_url(value):
                raise ConfigError(f"config: {name} must be an absolute http or https URL")
        return self

    def with_changes(self: _C, **changes: Any) -> _C:
        """Copy with some fields replaced (validated)."""
        return dataclasses.replace(self, **changes).validate()
