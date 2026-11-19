"""Let Jaeger's remote sampling strategy decide which traces are recorded.

Environment: GROW_JAEGER_SAMPLING_URL (default http://127.0.0.1:5778/sampling) plus the
tracing variables of tracing_query.py.
"""

from __future__ import annotations

import grow_jaeger as sdk


def main() -> None:
    with sdk.JaegerClient.from_env() as client:
        strategy = client.sampling_strategy("checkout")
        print(f"strategy: {strategy.type} rate={strategy.sampling_rate}")
        for op in strategy.operations:
            print(f"  {op.operation}: {op.sampling_rate}")

        # The handle owns the sampler: it polls every 30 s and stops on shutdown.
        tracing = client.init_tracing(
            service_name="checkout",
            remote_sampler=sdk.RemoteSamplerOptions(poll_interval_s=30),
        )
        sampler = tracing.remote_sampler
        assert sampler is not None
        sampler.ready(5)
        with tracing.tracer("example").start_as_current_span("GET /health") as span:
            print("sampled:", span.get_span_context().trace_flags.sampled)


if __name__ == "__main__":
    main()
