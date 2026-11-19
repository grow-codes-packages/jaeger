# Python examples

Install the package, point it at a running Jaeger and run an example:

```bash
pip install grow-jaeger
export GROW_JAEGER_URL=http://127.0.0.1:16686
export GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318   # OTLP/HTTP (default)
python examples/python/basic.py
```

| File | Shows |
| - | - |
| `basic.py` | sync client: health, ping, server version, error handling |
| `async_basic.py` | async client, cancellation by task |
| `hooks_and_logging.py` | request hooks, the `grow_jaeger` logger, retries and timeouts |
| `tracing_query.py` | `init_tracing` (OTLP to Jaeger), span helpers, then `query`: `wait_for_trace`, `list_services`, `list_operations`, `search_traces`, `get_dependencies` |
| `remote_sampling.py` | `sampling_strategy`, `init_tracing(remote_sampler=…)` with Jaeger remote sampling |
