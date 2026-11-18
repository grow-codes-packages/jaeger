"""Grow Codes SDK for Jaeger.

>>> from grow_jaeger import JaegerClient, JaegerConfig
>>> client = JaegerClient.from_env()  # doctest: +SKIP

Modules: :class:`JaegerClient` / :class:`AsyncJaegerClient` (health, ping, version),
:class:`JaegerQueryClient` (trace query API), :func:`init_tracing` (OTLP export preset) and
:class:`JaegerRemoteSampler` (remote sampling), plus the shared tracing helpers.
"""

from . import _core
from ._core import *  # noqa: F403
from ._version import __version__
from .client import AsyncJaegerClient, JaegerBuildInfo, JaegerClient
from .config import DEFAULT_SAMPLING_URL, DEFAULT_URL, JaegerConfig

__all__ = [
    "__version__",
    "JaegerClient",
    "AsyncJaegerClient",
    "JaegerBuildInfo",
    "JaegerConfig",
    "DEFAULT_URL",
    "DEFAULT_SAMPLING_URL",
]
__all__ += _core.__all__

from . import _tracing  # noqa: E402
from ._tracing import *  # noqa: E402, F403

__all__ += _tracing.__all__

from . import normalize, query, sampler, setup  # noqa: E402
from .normalize import *  # noqa: E402, F403
from .query import *  # noqa: E402, F403
from .sampler import *  # noqa: E402, F403
from .setup import *  # noqa: E402, F403

__all__ += normalize.__all__ + query.__all__ + sampler.__all__ + setup.__all__
