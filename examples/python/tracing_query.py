"""Send a small trace to Jaeger over OTLP, then read it back through the query API.

Environment: GROW_JAEGER_URL (query, default http://127.0.0.1:16686) and the OTLP endpoint
(GROW_JAEGER_OTLP_HTTP_ENDPOINT or OTEL_EXPORTER_OTLP_ENDPOINT, default
http://127.0.0.1:4318/v1/traces).
"""

from __future__ import annotations

import uuid

import grow_jaeger as sdk


def main() -> None:
    service = f"example-{uuid.uuid4().hex[:8]}"
    with sdk.JaegerClient.from_env() as client:
        # OTLP http/protobuf to :4318 by default; protocol="grpc" uses :4317.
        tracing = client.init_tracing(service_name=service, **sdk.SAMPLING_DEVELOPMENT.as_kwargs())
        tracer = tracing.tracer("example")
        with tracer.start_as_current_span("checkout", attributes={"order.id": "42"}) as root:
            with sdk.with_span("charge-card", kind="client", tracer=tracer):
                sdk.add_event("card.charged", {"amount": 9.99})
        tracing.force_flush()
        trace_id = format(root.get_span_context().trace_id, "032x")

        trace = client.query.wait_for_trace(trace_id, min_spans=2, timeout_s=30)
        for node in trace.tree():
            print(f"{node.span.name} ({node.span.duration_micros} µs)")
            for child in node.children:
                print(f"  └─ {child.span.name} events={[e.name for e in child.span.events]}")

        print("services:", client.query.list_services())
        print("operations:", client.query.list_operations(service))
        for summary in client.query.search_traces(service=service, tags={"order.id": "42"}):
            print("found", summary.trace_id, summary.span_count, "spans")
        print("dependencies:", client.query.get_dependencies())
        print("server version:", client.server_version())


if __name__ == "__main__":
    main()
