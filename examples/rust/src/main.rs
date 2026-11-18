// Created by sdk-kit 0.1.0; owned by this repository.
//! The Jaeger Rust SDK end to end: export spans to Jaeger over OTLP, read the trace back through
//! the query API, list services and operations, and fetch the remote sampling strategy.
//!
//! ```sh
//! # Jaeger all-in-one: docker run --rm -p 16686:16686 -p 4318:4318 -p 5778:5778 jaegertracing/jaeger:2.23.0
//! GROW_JAEGER_URL=http://127.0.0.1:16686 cargo run
//! ```

use std::sync::Arc;
use std::time::Duration;

use grow_jaeger as sdk;
use opentelemetry::trace::{TraceContextExt, Tracer as _};
use opentelemetry::{Context, KeyValue};
use sdk::core::{ErrorInfo, Hooks};
use sdk::tracing::{with_span, SpanOptions, TraceSearchQuery, TracingConfig, WaitForTraceOptions};
use sdk::{init_tracing, JaegerClient, JaegerConfig, ListOperationsOptions, SamplingPreset};

/// Prints every retry.
struct RetryLogger;

impl Hooks for RetryLogger {
    fn on_error(&self, info: &ErrorInfo) {
        if info.will_retry {
            eprintln!(
                "retrying {} {} (attempt {})",
                info.method, info.url, info.attempt
            );
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error [{}]: {err}", err.code());
        std::process::exit(1);
    }
}

async fn run() -> sdk::Result<()> {
    // Query client: explicit > GROW_JAEGER_* > JAEGER_QUERY_URL > http://127.0.0.1:16686.
    let config = JaegerConfig::builder()
        .hooks(Arc::new(RetryLogger))
        .build_from_env()?;
    let client = JaegerClient::new(config)?;
    let health = client.health().await?;
    println!("health: {} ({})", health.healthy, health.status);
    println!("jaeger version: {}", client.server_version().await?);

    // Tracing: OTLP http/protobuf to :4318 (GROW_JAEGER_OTLP_* / OTEL_EXPORTER_OTLP_* override).
    let service = format!("grow-example-{}", std::process::id());
    let tracing = init_tracing(SamplingPreset::DEVELOPMENT.apply(TracingConfig::new(&service)))?;
    let tracer = tracing.tracer("example", None);
    let root = tracer
        .span_builder("GET /orders")
        .with_attributes(vec![KeyValue::new("order.id", "42")])
        .start_with_context(&tracer, &Context::new());
    let cx = Context::new().with_span(root);
    let trace_id = cx.span().span_context().trace_id().to_string();
    let _: Result<(), String> = with_span(
        "db.query",
        SpanOptions::default()
            .tracer(tracer.clone())
            .parent(cx.clone()),
        async { Ok(()) },
    )
    .await;
    cx.span().end();
    tracing.shutdown().await?; // flushes

    // Read it back.
    let query = client.query();
    let trace = query
        .wait_for_trace(
            &trace_id,
            WaitForTraceOptions::default()
                .min_spans(2)
                .interval(Duration::from_millis(250)),
        )
        .await?;
    for span in &trace.spans {
        println!(
            "span {} {} ({} µs)",
            span.span_id, span.name, span.duration_micros
        );
    }
    let summaries = query
        .search_traces(&TraceSearchQuery::service(&service).tag("order.id", "42"))
        .await?;
    println!("search: {} trace(s)", summaries.len());
    println!("services: {:?}", query.list_services().await?);
    println!(
        "operations: {:?}",
        query
            .list_operations(&service, ListOperationsOptions::default())
            .await?
    );

    // Remote sampling strategy (GROW_JAEGER_SAMPLING_URL, default http://127.0.0.1:5778/sampling).
    match client.sampling_strategy(&service).await {
        Ok(s) => println!("sampling: {:?} rate {}", s.strategy_type, s.sampling_rate),
        Err(e) => println!("sampling endpoint not reachable: {e}"),
    }
    client.close().await;
    Ok(())
}
