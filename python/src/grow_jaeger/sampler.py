"""Jaeger remote sampling (``GET {sampling_url}?service=<name>``, port 5778) as an
OpenTelemetry :class:`~opentelemetry.sdk.trace.sampling.Sampler`."""

from __future__ import annotations

import contextlib
import dataclasses
import math
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from datetime import timedelta
from typing import Any

import httpx
from opentelemetry import trace as otel_trace
from opentelemetry.context import Context
from opentelemetry.sdk.trace.sampling import (
    Decision,
    Sampler,
    SamplingResult,
    TraceIdRatioBased,
)
from opentelemetry.trace import Link, SpanKind
from opentelemetry.trace.span import TraceState
from opentelemetry.util.types import Attributes

from . import _http
from ._core import CancelToken, ConfigError, ValidationError, redact_url, to_seconds
from .config import JaegerConfig

__all__ = [
    "DEFAULT_INITIAL_SAMPLING_RATE",
    "DEFAULT_SAMPLING_POLL_INTERVAL_S",
    "JaegerRemoteSampler",
    "JaegerSamplingStrategy",
    "OperationStrategy",
    "RateLimiter",
    "parse_sampling_strategy",
]

#: Default poll interval of :class:`JaegerRemoteSampler` (the Jaeger client default).
DEFAULT_SAMPLING_POLL_INTERVAL_S = 60.0
#: Probability applied until the first strategy arrives (the Jaeger client default).
DEFAULT_INITIAL_SAMPLING_RATE = 0.001

STRATEGY_PROBABILISTIC = "probabilistic"
STRATEGY_RATE_LIMITING = "ratelimiting"


@dataclasses.dataclass(frozen=True)
class OperationStrategy:
    """A per-operation probabilistic rate (``operationSampling.perOperationStrategies``)."""

    operation: str
    sampling_rate: float


@dataclasses.dataclass(frozen=True)
class JaegerSamplingStrategy:
    """A sampling strategy as served by Jaeger, normalized.

    Attributes:
        type: ``"probabilistic"`` or ``"ratelimiting"`` (Jaeger ``strategyType``).
        sampling_rate: service-level ``probabilisticSampling.samplingRate`` (0..1).
        max_traces_per_second: service-level ``rateLimitingSampling.maxTracesPerSecond``.
        default_sampling_probability: ``operationSampling.defaultSamplingProbability``.
        default_lower_bound_traces_per_second:
            ``operationSampling.defaultLowerBoundTracesPerSecond``.
        operations: per-operation probabilistic rates.
    """

    type: str
    sampling_rate: float | None = None
    max_traces_per_second: float | None = None
    default_sampling_probability: float | None = None
    default_lower_bound_traces_per_second: float | None = None
    operations: Sequence[OperationStrategy] = ()

    def operation_rate(self, operation: str) -> float | None:
        """Rate of a listed operation, ``None`` when the operation has no own strategy."""
        for item in self.operations:
            if item.operation == operation:
                return item.sampling_rate
        return None


def _number(value: Any, what: str) -> float:
    # proto3 JSON omits zero values, so a missing number means 0.
    n = 0 if value is None else value
    if isinstance(n, bool) or not isinstance(n, (int, float)) or not math.isfinite(n):
        raise ValidationError(f"jaeger.sampling: {what} must be a number")
    return float(n)


def _rate(value: Any, what: str) -> float:
    n = _number(value, what)
    if not 0.0 <= n <= 1.0:
        raise ValidationError(f"jaeger.sampling: {what} must be between 0 and 1")
    return n


def _obj(value: Any) -> Mapping[str, Any] | None:
    return value if isinstance(value, Mapping) else None


def parse_sampling_strategy(body: Any) -> JaegerSamplingStrategy:
    """Parse a ``/sampling?service=`` response.

    Missing numbers default to 0 (proto3 JSON), a missing ``strategyType`` means
    PROBABILISTIC. Raises :class:`ValidationError` for a malformed body, a rate outside 0..1
    or a negative ``maxTracesPerSecond``.
    """
    root = _obj(body)
    if root is None:
        raise ValidationError("jaeger.sampling: response is not an object")
    type_text = str(root.get("strategyType", "PROBABILISTIC")).upper()
    if type_text in ("PROBABILISTIC", "0"):
        kind = STRATEGY_PROBABILISTIC
    elif type_text in ("RATE_LIMITING", "1"):
        kind = STRATEGY_RATE_LIMITING
    else:
        raise ValidationError("jaeger.sampling: unknown strategyType")
    values: dict[str, Any] = {"type": kind}
    prob = _obj(root.get("probabilisticSampling"))
    if prob is not None:
        values["sampling_rate"] = _rate(prob.get("samplingRate"), "samplingRate")
    limit = _obj(root.get("rateLimitingSampling"))
    if limit is not None:
        max_tps = _number(limit.get("maxTracesPerSecond"), "maxTracesPerSecond")
        if max_tps < 0:
            raise ValidationError("jaeger.sampling: maxTracesPerSecond must be >= 0")
        values["max_traces_per_second"] = max_tps
    ops = _obj(root.get("operationSampling"))
    if ops is not None:
        values["default_sampling_probability"] = _rate(
            ops.get("defaultSamplingProbability"), "defaultSamplingProbability"
        )
        lower = _number(
            ops.get("defaultLowerBoundTracesPerSecond"), "defaultLowerBoundTracesPerSecond"
        )
        if lower < 0:
            raise ValidationError("jaeger.sampling: defaultLowerBoundTracesPerSecond must be >= 0")
        values["default_lower_bound_traces_per_second"] = lower
        entries = ops.get("perOperationStrategies")
        operations = []
        for entry in entries if isinstance(entries, list) else []:
            item = _obj(entry)
            if item is None or not isinstance(item.get("operation"), str):
                continue
            prob_op = _obj(item.get("probabilisticSampling")) or {}
            operations.append(
                OperationStrategy(item["operation"], _rate(prob_op.get("samplingRate"), "rate"))
            )
        values["operations"] = tuple(operations)
    return JaegerSamplingStrategy(**values)


class RateLimiter:
    """Token bucket with Jaeger semantics: the balance starts full at ``max(1, rate)`` credits."""

    def __init__(
        self, credits_per_second: float, clock: Callable[[], float] = time.monotonic
    ) -> None:
        self.credits_per_second = credits_per_second
        self.max_balance = max(1.0, credits_per_second)
        self._balance = self.max_balance
        self._clock = clock
        self._last = clock()
        self._lock = threading.Lock()

    def check(self) -> bool:
        """Take one credit; ``False`` when none is available."""
        with self._lock:
            now = self._clock()
            elapsed = max(0.0, now - self._last)
            self._last = now
            self._balance = min(self.max_balance, self._balance + elapsed * self.credits_per_second)
            if self._balance >= 1.0:
                self._balance -= 1.0
                return True
            return False


def _parent_trace_state(parent_context: Context | None) -> TraceState | None:
    ctx = otel_trace.get_current_span(parent_context).get_span_context()
    return ctx.trace_state if ctx.is_valid else None


class JaegerRemoteSampler(Sampler):
    """OpenTelemetry sampler driven by Jaeger's remote sampling endpoint.

    Rule (the same in every language): a per-operation strategy wins for a listed operation
    (span name); every other operation uses the service-level ``rateLimitingSampling`` when
    present, else ``probabilisticSampling`` (else ``defaultSamplingProbability``).
    Probabilistic decisions are deterministic per trace id. Until the first strategy
    arrives, ``initial_sampling_rate`` applies; a failed or malformed poll keeps the last good
    strategy.

    Polling runs on a daemon thread (first fetch immediately) until :meth:`close`. Use the
    sampler as the root of a ``ParentBased`` sampler (what ``init_tracing(remote_sampler=…)``
    does).

    Args:
        sampling_url: the endpoint (``…:5778/sampling``); ``None`` uses
            ``config.sampling_url``.
        service: the service whose strategy is fetched (``?service=``).
        poll_interval_s: seconds (or ``timedelta``) between polls; default 60.
        initial_sampling_rate: probability until the first strategy arrives; default 0.001.
        config: transport settings (timeouts, retries, TLS, proxy, headers, token); default
            :meth:`JaegerConfig.from_env`.
        auto_start: start polling at once (default ``True``).
        on_error: called with the error when a poll fails.
        on_update: called with the strategy after each successful poll.
    """

    def __init__(
        self,
        sampling_url: str | None,
        service: str,
        poll_interval_s: float | timedelta = DEFAULT_SAMPLING_POLL_INTERVAL_S,
        *,
        initial_sampling_rate: float = DEFAULT_INITIAL_SAMPLING_RATE,
        config: JaegerConfig | None = None,
        auto_start: bool = True,
        on_error: Callable[[BaseException], None] | None = None,
        on_update: Callable[[JaegerSamplingStrategy], None] | None = None,
        http_transport: httpx.BaseTransport | None = None,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        if not isinstance(service, str) or not service.strip():
            raise ConfigError("jaeger.remote_sampler: service is required")
        interval = to_seconds(poll_interval_s)
        if not interval > 0:
            raise ConfigError("jaeger.remote_sampler: poll_interval_s must be positive")
        if not 0.0 <= initial_sampling_rate <= 1.0:
            raise ConfigError("jaeger.remote_sampler: initial_sampling_rate must be within 0..1")
        base = config if config is not None else JaegerConfig.from_env()
        if sampling_url is not None:
            base = base.with_changes(sampling_url=sampling_url)
        self.config = base.validate()
        self.service = service
        self.sampling_url = self.config.sampling_url
        self.poll_interval_s = interval
        self.initial_sampling_rate = initial_sampling_rate
        self._on_error = on_error
        self._on_update = on_update
        self._clock = clock
        self._pool = _http.TransportPool(self.config, http_transport=http_transport)
        self._ratios: dict[float, TraceIdRatioBased] = {}
        self._strategy: JaegerSamplingStrategy | None = None
        self._limiter: RateLimiter | None = None
        self._lock = threading.Lock()
        self._stop = CancelToken()
        self._first = threading.Event()
        self._thread: threading.Thread | None = None
        if auto_start:
            self.start()

    @property
    def strategy(self) -> JaegerSamplingStrategy | None:
        """The strategy in use, ``None`` before the first successful poll."""
        return self._strategy

    @property
    def closed(self) -> bool:
        return self._stop.cancelled

    def refresh(self, *, cancel: CancelToken | None = None) -> JaegerSamplingStrategy:
        """Fetch the strategy once and install it; raises on failure (the previous strategy
        stays in use)."""
        transport, path = self._pool.for_url(self.sampling_url)
        op = f"jaeger.sampling {self.service}"
        req = _http.get(path, op, self.config, params={"service": self.service})
        resp = transport.request(req, cancel=cancel or self._stop)
        if not 200 <= resp.status < 300:
            raise _http.status_error(resp, op, self.config)
        strategy = parse_sampling_strategy(resp.json())
        self._install(strategy)
        return strategy

    def _install(self, strategy: JaegerSamplingStrategy) -> None:
        with self._lock:
            previous = self._strategy.max_traces_per_second if self._strategy else None
            self._strategy = strategy
            if strategy.max_traces_per_second is None:
                self._limiter = None
            elif self._limiter is None or previous != strategy.max_traces_per_second:
                self._limiter = RateLimiter(strategy.max_traces_per_second, self._clock)

    def start(self) -> None:
        """Start polling (first fetch immediately). Idempotent; no-op after :meth:`close`."""
        with self._lock:
            if self._thread is not None or self._stop.cancelled:
                return
            self._thread = threading.Thread(
                target=self._loop, name="grow-jaeger-sampler", daemon=True
            )
        self._thread.start()

    def ready(self, timeout_s: float | timedelta | None = None) -> bool:
        """Wait until the first poll since :meth:`start` finished (successfully or not)."""
        return self._first.wait(None if timeout_s is None else to_seconds(timeout_s))

    def _poll_once(self) -> None:
        try:
            strategy = self.refresh()
        except Exception as err:  # noqa: BLE001 - reported, polling continues
            _safe(self._on_error, err)
        else:
            _safe(self._on_update, strategy)

    def _loop(self) -> None:
        try:
            if not self._stop.cancelled:
                self._poll_once()
        finally:
            self._first.set()
        while not self._stop.wait(self.poll_interval_s):
            self._poll_once()

    def close(self) -> None:
        """Stop polling and release connections. Idempotent."""
        self._stop.cancel()
        thread = self._thread
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout=5.0)
        self._first.set()
        self._pool.close()

    stop = close
    shutdown = close

    def _ratio(self, rate: float) -> TraceIdRatioBased:
        sampler = self._ratios.get(rate)
        if sampler is None:
            sampler = self._ratios[rate] = TraceIdRatioBased(rate)
        return sampler

    def should_sample(
        self,
        parent_context: Context | None,
        trace_id: int,
        name: str,
        kind: SpanKind | None = None,
        attributes: Attributes = None,
        links: Sequence[Link] | None = None,
        trace_state: TraceState | None = None,
    ) -> SamplingResult:
        """The decision for a new span (see the class description for the rule)."""
        strategy, limiter = self._strategy, self._limiter
        if strategy is None:
            rate = self.initial_sampling_rate
        else:
            op_rate = strategy.operation_rate(name)
            if op_rate is not None:
                rate = op_rate
            elif limiter is not None:
                decision = Decision.RECORD_AND_SAMPLE if limiter.check() else Decision.DROP
                return SamplingResult(decision, None, _parent_trace_state(parent_context))
            elif strategy.sampling_rate is not None:
                rate = strategy.sampling_rate
            elif strategy.default_sampling_probability is not None:
                rate = strategy.default_sampling_probability
            else:
                rate = self.initial_sampling_rate
        return self._ratio(rate).should_sample(
            parent_context, trace_id, name, kind, attributes, links, trace_state
        )

    def get_description(self) -> str:
        return f"JaegerRemoteSampler{{service={self.service}}}"

    def __repr__(self) -> str:
        return (
            f"JaegerRemoteSampler(service={self.service!r}, "
            f"sampling_url={redact_url(self.sampling_url)!r})"
        )


def _safe(callback: Callable[[Any], None] | None, value: Any) -> None:
    if callback is None:
        return
    with contextlib.suppress(Exception):  # callback errors never stop polling
        callback(value)
