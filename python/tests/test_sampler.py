"""Remote sampling: strategy parsing, rate limiter, decision rule and polling."""

from __future__ import annotations

import threading
import time
from typing import Any

import httpx
import pytest
from opentelemetry import trace as otel_trace
from opentelemetry.sdk.trace.sampling import Decision
from opentelemetry.trace import NonRecordingSpan, SpanContext, TraceFlags, TraceState

import grow_jaeger as sdk
from _fixtures import Router, body, config, response
from _support import MockServer, Reply

LOW = 1  # sampled by any ratio > 0
HIGH = (1 << 64) - 1  # sampled only by ratio 1


def sampler(router: Router, **kw: Any) -> sdk.JaegerRemoteSampler:
    kw.setdefault("auto_start", False)
    return sdk.JaegerRemoteSampler(
        "http://jaeger:5778/sampling",
        kw.pop("service", "svc"),
        kw.pop("poll_interval_s", 60.0),
        config=config(),
        http_transport=router.transport(),
        **kw,
    )


def decide(s: sdk.JaegerRemoteSampler, name: str, trace_id: int = LOW) -> Decision:
    return s.should_sample(None, trace_id, name).decision


# --------------------------------------------------------------------------- parsing


def test_parse_recorded_strategies() -> None:
    ops = sdk.parse_sampling_strategy(body("sampling.per_operation"))
    assert ops.type == "probabilistic" and ops.sampling_rate == 0.8
    assert ops.operation_rate("GET /fast") == 0.2
    assert ops.operation_rate("/health") == 0.0
    assert ops.operation_rate("other") is None
    rate = sdk.parse_sampling_strategy(body("sampling.rate_limiting"))
    assert rate.type == "ratelimiting" and rate.max_traces_per_second == 5
    assert rate.sampling_rate is None
    stock = sdk.parse_sampling_strategy(body("sampling.default_stock"))
    assert stock.sampling_rate == 1 and stock.operations == ()


def test_parse_defaults_and_numeric_types() -> None:
    parsed = sdk.parse_sampling_strategy(
        {
            "strategyType": 1,
            "rateLimitingSampling": {},
            "probabilisticSampling": {},
            "operationSampling": {
                "perOperationStrategies": [
                    {"operation": "a", "probabilisticSampling": {}},
                    {"operation": "b"},
                    {"nope": 1},
                    "x",
                ]
            },
        }
    )
    assert parsed.type == "ratelimiting"
    assert parsed.max_traces_per_second == 0.0 and parsed.sampling_rate == 0.0
    assert parsed.default_sampling_probability == 0.0
    assert [(o.operation, o.sampling_rate) for o in parsed.operations] == [("a", 0.0), ("b", 0.0)]
    assert sdk.parse_sampling_strategy({}).type == "probabilistic"
    assert sdk.parse_sampling_strategy({"strategyType": 0}).type == "probabilistic"


@pytest.mark.parametrize(
    "bad",
    [
        None,
        [],
        {"strategyType": "ADAPTIVE"},
        {"probabilisticSampling": {"samplingRate": 1.5}},
        {"probabilisticSampling": {"samplingRate": "0.5"}},
        {"probabilisticSampling": {"samplingRate": True}},
        {"rateLimitingSampling": {"maxTracesPerSecond": -1}},
        {"operationSampling": {"defaultLowerBoundTracesPerSecond": -1}},
        {"operationSampling": {"defaultSamplingProbability": 2}},
    ],
)
def test_parse_rejects_malformed(bad: Any) -> None:
    with pytest.raises(sdk.ValidationError):
        sdk.parse_sampling_strategy(bad)


def test_rate_limiter_token_bucket() -> None:
    now = [0.0]
    limiter = sdk.RateLimiter(2.0, clock=lambda: now[0])
    assert limiter.max_balance == 2.0
    assert [limiter.check() for _ in range(3)] == [True, True, False]
    now[0] += 0.5
    assert limiter.check() is True
    assert limiter.check() is False
    zero = sdk.RateLimiter(0.0, clock=lambda: now[0])
    assert zero.check() is True  # Jaeger semantics: the bucket starts with one credit
    now[0] += 100
    assert zero.check() is False


# --------------------------------------------------------------------------- decision rule


def test_initial_rate_before_first_strategy() -> None:
    s = sampler(Router(), initial_sampling_rate=0.5)
    assert s.strategy is None
    assert decide(s, "x", LOW) == Decision.RECORD_AND_SAMPLE
    assert decide(s, "x", HIGH) == Decision.DROP


def test_per_operation_wins_then_service_rate() -> None:
    router = Router().add("/sampling", response("sampling.per_operation"))
    s = sampler(router)
    strategy = s.refresh()
    assert s.strategy == strategy
    assert router.params("/sampling") == {"service": "svc"}
    assert decide(s, "/health", LOW) == Decision.DROP  # per-operation rate 0
    assert decide(s, "GET /fast", LOW) == Decision.RECORD_AND_SAMPLE
    assert decide(s, "GET /fast", HIGH) == Decision.DROP
    assert decide(s, "other", LOW) == Decision.RECORD_AND_SAMPLE


def test_rate_limiting_for_unlisted_operations() -> None:
    router = Router().add(
        "/sampling",
        response("sampling.rate_limiting"),
        httpx.Response(200, json={"strategyType": "RATE_LIMITING", "rateLimitingSampling": {}}),
    )
    now = [0.0]
    s = sampler(router, clock=lambda: now[0])
    s.refresh()
    assert decide(s, "/health") == Decision.DROP  # listed operation keeps its rate
    results = [decide(s, "work", HIGH) for _ in range(6)]
    assert results.count(Decision.RECORD_AND_SAMPLE) == 5
    s.refresh()  # max 0: a new limiter
    assert decide(s, "work") == Decision.RECORD_AND_SAMPLE
    assert decide(s, "work") == Decision.DROP


def test_rate_limiter_kept_when_unchanged_and_parent_trace_state() -> None:
    router = Router().add("/sampling", response("sampling.rate_limiting"))
    s = sampler(router)
    s.refresh()
    limiter = s._limiter
    s.refresh()
    assert s._limiter is limiter
    parent = SpanContext(
        0xABC,
        0xDEF,
        is_remote=True,
        trace_flags=TraceFlags(1),
        trace_state=TraceState([("k", "v")]),
    )
    ctx = otel_trace.set_span_in_context(NonRecordingSpan(parent))
    result = s.should_sample(ctx, 0xABC, "work")
    assert result.trace_state is not None and result.trace_state.get("k") == "v"


def test_default_probability_fallbacks() -> None:
    router = Router().add(
        "/sampling",
        httpx.Response(200, json={"operationSampling": {"defaultSamplingProbability": 1}}),
        httpx.Response(200, json={"strategyType": "PROBABILISTIC"}),
    )
    s = sampler(router, initial_sampling_rate=0.0)
    s.refresh()
    assert decide(s, "x", HIGH) == Decision.RECORD_AND_SAMPLE
    s.refresh()
    assert decide(s, "x", LOW) == Decision.DROP  # nothing given: initial rate 0


def test_refresh_errors_keep_the_last_strategy() -> None:
    router = Router().add(
        "/sampling",
        response("sampling.probabilistic"),
        response("sampling.missing_service"),
        httpx.Response(200, json={"probabilisticSampling": {"samplingRate": 7}}),
    )
    s = sampler(router)
    good = s.refresh()
    with pytest.raises(sdk.ValidationError) as info:
        s.refresh()
    assert "'service' parameter" in info.value.message
    with pytest.raises(sdk.ValidationError):
        s.refresh()
    assert s.strategy == good
    assert "svc" in s.get_description() and "svc" in repr(s)


@pytest.mark.parametrize(
    "kw",
    [{"service": ""}, {"poll_interval_s": 0}, {"initial_sampling_rate": 2.0}],
)
def test_constructor_validation(kw: dict[str, Any]) -> None:
    with pytest.raises(sdk.ConfigError):
        sampler(Router(), **kw)


def test_sampling_url_defaults_to_config(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("JAEGER_SAMPLING_ENDPOINT", "http://env:5778/sampling")
    s = sdk.JaegerRemoteSampler(None, "svc", auto_start=False)
    assert s.sampling_url == "http://env:5778/sampling"
    s.close()


# --------------------------------------------------------------------------- polling


def test_polling_updates_and_reports_errors(server: MockServer) -> None:
    strategies = [
        Reply(200, body("sampling.per_operation")),
        Reply(500, "boom"),
        Reply(200, body("sampling.after_reload")),
    ]
    server.enqueue("/sampling", *strategies)
    server.defaults["/sampling"] = Reply(200, body("sampling.after_reload"))
    updates: list[sdk.JaegerSamplingStrategy] = []
    errors: list[BaseException] = []
    done = threading.Event()

    def on_update(strategy: sdk.JaegerSamplingStrategy) -> None:
        updates.append(strategy)
        if len(updates) >= 2:
            done.set()
        raise RuntimeError("callback errors are ignored")

    s = sdk.JaegerRemoteSampler(
        f"{server.url}/sampling",
        "sdk-sampling-live",
        0.02,
        config=config(server.url, retry=sdk.RetryPolicy(max_retries=0)),
        on_update=on_update,
        on_error=errors.append,
    )
    assert s.ready(5)
    assert done.wait(5)
    s.close()
    s.close()
    s.start()  # no-op after close
    assert s.closed
    assert updates[0].operation_rate("GET /fast") == 0.2
    assert updates[1].sampling_rate == 0.0
    assert errors and isinstance(errors[0], sdk.UnavailableError)
    with pytest.raises(sdk.CancelledError):
        s.refresh()


def test_close_before_first_poll_and_ready_timeout() -> None:
    s = sampler(Router())
    assert s.ready(0.01) is False
    s.close()
    assert s.ready(0) is True


def test_close_from_callback_does_not_deadlock(server: MockServer) -> None:
    server.defaults["/sampling"] = Reply(200, body("sampling.probabilistic"))
    holder: dict[str, sdk.JaegerRemoteSampler] = {}
    closed = threading.Event()

    def on_update(_: sdk.JaegerSamplingStrategy) -> None:
        while "s" not in holder:
            time.sleep(0.001)
        holder["s"].close()
        closed.set()

    holder["s"] = sdk.JaegerRemoteSampler(
        f"{server.url}/sampling", "svc", 1.0, config=config(server.url), on_update=on_update
    )
    assert closed.wait(5)
