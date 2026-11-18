# grow-jaeger (Python)

Python SDK for [Jaeger](https://grow-codes-packages.github.io/jaeger/) by Grow Codes: OTLP tracing
preset, trace query client, remote sampling. Tested against `jaegertracing/jaeger:2.23.0`.
Unofficial and community maintained; "Jaeger" is a trademark of The Linux Foundation (Jaeger is a CNCF project).

```bash
pip install grow-jaeger            # OTLP/HTTP
pip install "grow-jaeger[grpc]"    # + OTLP/gRPC exporter
```

```python
from grow_jaeger import JaegerClient

# Reads GROW_JAEGER_URL (or JAEGER_QUERY_URL), GROW_JAEGER_SAMPLING_URL, ... from the environment.
with JaegerClient.from_env() as client:
    tracing = client.init_tracing(service_name="checkout")  # OTLP http/protobuf to :4318
    with tracing.tracer("app").start_as_current_span("charge") as span:
        trace_id = format(span.get_span_context().trace_id, "032x")
    tracing.force_flush()

    trace = client.query.wait_for_trace(trace_id)            # GET /api/v3/traces/{id}
    print([s.name for s in trace.spans], client.server_version())
    print(client.query.search_traces(service="checkout", limit=5))
```

The async client has the same methods:

```python
import asyncio
from grow_jaeger import AsyncJaegerClient

async def main() -> None:
    async with AsyncJaegerClient.from_env() as client:
        print(await client.health())
        print(await client.query.list_services())

asyncio.run(main())
```

| Module | Functions |
| - | - |
| `JaegerClient` / `AsyncJaegerClient` | `health`, `ping`, `server_version`, `build_info`, `sampling_strategy`, `query`, `init_tracing`, `remote_sampler`, `close` |
| `JaegerQueryClient` | `get_trace`, `require_trace`, `find_traces`, `search_traces`, `list_services`, `get_operations`, `list_operations`, `get_dependencies`, `wait_for_trace` |
| tracing | `init_tracing`, `tracing_config_from_env`, `jaeger_exporter_factory`, sampling presets (`SAMPLING_PRODUCTION`, `sampling_ratio`, …), `with_span`, `traced`, `inject`/`extract`, ASGI/WSGI middleware, httpx/requests hooks |
| `JaegerRemoteSampler` | OpenTelemetry sampler polling `GET /sampling?service=` |

- Python 3.10 or newer, typed (`py.typed`), `httpx` based, environment proxies ignored unless
  configured.
- Retries with full-jitter backoff and `Retry-After`, connect/request/total timeouts,
  cancellation, hooks and a silent-by-default `logging` logger named `grow_jaeger`.
- Credentials are wrapped in `Secret` and never appear in `repr()`, logs or error messages.
- `grow_jaeger.testing` ships in-memory fakes and the conformance runner.

Documentation: https://grow-codes-packages.github.io/jaeger/ ·
Source: https://github.com/grow-codes-packages/jaeger · Licence: Apache-2.0
