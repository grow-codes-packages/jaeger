// Service-owned integration suite: every public function of the Jaeger package against the real
// `jaegertracing/jaeger:2.23.0` from docker/compose.yml, with round-trip assertions.
//! Integration tests against the real Jaeger (run through `make test-docker LANGS=rust`).

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use grow_jaeger as sdk;
use grow_jaeger_integration::{load_bootstrap_env, required_env, unique};
use opentelemetry::trace::{
    Link, Span as _, SpanKind as OtelKind, TraceContextExt, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::ShouldSample;
use sdk::core::{EnvReader, ErrorCode};
use sdk::tracing::{
    self as tr, DependencyOptions, Protocol, SamplerKind, SpanKind, SpanOptions, StatusCode,
    TraceQueryClient, TraceSearchQuery, TracingConfig, WaitForTraceOptions,
};
use sdk::{
    GetTraceOptions, JaegerClient, JaegerConfig, JaegerRemoteSampler, JaegerTracingHandle,
    JaegerTracingOptions, ListOperationsOptions, SamplingPreset, StrategyType,
};

fn client() -> JaegerClient {
    load_bootstrap_env();
    JaegerClient::from_env().expect("client from env")
}

fn wait() -> WaitForTraceOptions {
    WaitForTraceOptions::default()
        .timeout(Duration::from_secs(30))
        .interval(Duration::from_millis(250))
}

fn start(service: &str, protocol: Protocol, set_global: bool) -> JaegerTracingHandle {
    let mut config = TracingConfig::new(service)
        .with_protocol(protocol)
        .with_set_global(set_global)
        .with_service_version("1.2.3")
        .with_environment("sdk-it");
    config.batch.scheduled_delay = Duration::from_millis(100);
    sdk::init_tracing(config).expect("init_tracing")
}

#[tokio::test]
async fn client_core() {
    let c = client();
    let h = c.health().await.unwrap();
    assert!(h.healthy, "{h:?}");
    assert_eq!(h.details["healthcheck.status"], "StatusOK");
    assert!(c.ping().await.unwrap() < Duration::from_secs(10));
    assert_eq!(c.server_version().await.unwrap(), "2.23.0");
    let info = c.build_info().await.unwrap().expect("build info");
    assert_eq!(info.git_version.as_deref(), Some("v2.23.0"));
    // client.init_tracing: the handle exports to Jaeger and close() shuts it down
    let service = unique("sdk-rust-client-tracing");
    let handle = c
        .init_tracing(
            TracingConfig::new(service.clone()).with_set_global(false),
            JaegerTracingOptions::default(),
        )
        .unwrap();
    let mut span = handle
        .tracer("t", None)
        .start_with_context("client-op", &Context::new());
    let trace_id = span.span_context().trace_id().to_string();
    span.end();
    handle.force_flush(None).await.unwrap();
    let trace = c.query().wait_for_trace(&trace_id, wait()).await.unwrap();
    assert_eq!(trace.spans[0].service_name, service);
    // metrics fallback alone (no UI version on a non-UI URL): point the query URL at metrics
    let metrics_only = JaegerClient::new(
        JaegerConfig::builder()
            .url(required_env("GROW_JAEGER_METRICS_URL").replace("/metrics", "/nothing"))
            .metrics_url(required_env("GROW_JAEGER_METRICS_URL"))
            .build()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(metrics_only.server_version().await.unwrap(), "2.23.0");
    c.close().await;
    c.close().await;
    assert!(c.is_closed());
    assert!(handle.is_shutdown());
}

#[tokio::test]
async fn trace_round_trip_http_and_grpc() {
    for protocol in [Protocol::HttpProtobuf, Protocol::Grpc] {
        round_trip(protocol).await;
    }
}

async fn round_trip(protocol: Protocol) {
    let c = client();
    let q = c.query();
    let service = unique(match protocol {
        Protocol::Grpc => "sdk-rust-grpc",
        _ => "sdk-rust-http",
    });
    let h = start(&service, protocol, false);
    assert!(h.endpoint().starts_with("http://"));
    let tracer = h.tracer("integration", Some("0.1.0"));

    let mut other = tracer.start_with_context("linked", &Context::new());
    let other_cx = other.span_context().clone();
    other.end();

    let root_name = format!("GET /orders {service}");
    let run = service.clone();
    let root = tracer
        .span_builder(root_name.clone())
        .with_kind(OtelKind::Server)
        .with_attributes(vec![
            KeyValue::new("order.id", run.clone()),
            KeyValue::new("int.attr", 42i64),
            KeyValue::new("double.attr", 1.5),
            KeyValue::new("bool.attr", true),
        ])
        .start_with_context(&tracer, &Context::new());
    let cx = Context::new().with_span(root);
    let trace_id = cx.span().span_context().trace_id().to_string();
    {
        let _g = cx.clone().attach();
        assert_eq!(tr::current_trace_id().as_deref(), Some(trace_id.as_str()));
        assert_eq!(
            tr::trace_fields().trace_id.as_deref(),
            Some(trace_id.as_str())
        );
        assert!(tr::current_span_id().is_some());
    }
    let res: Result<(), String> = tr::with_span(
        "db.query",
        SpanOptions::default()
            .tracer(tracer.clone())
            .parent(cx.clone())
            .kind(OtelKind::Client)
            .link(Link::with_context(other_cx.clone())),
        async {
            tr::add_event("cache.miss", vec![KeyValue::new("cache.key", "k1")]);
            Err("boom".to_owned())
        },
    )
    .await;
    assert!(res.is_err());
    let ok: Result<(), String> = tr::with_span_sync(
        "set-error",
        SpanOptions::default()
            .tracer(tracer.clone())
            .parent(cx.clone()),
        || {
            tr::record_exception("recorded");
            tr::set_error("explicit");
            Ok(())
        },
    );
    assert!(ok.is_ok());
    cx.span().end();
    h.force_flush(Some(Duration::from_secs(10))).await.unwrap();

    let trace = q
        .wait_for_trace(&trace_id, wait().min_spans(3))
        .await
        .unwrap();
    assert_eq!(trace.trace_id, trace_id);
    assert_eq!(trace.spans.len(), 3);
    let r = trace.spans.iter().find(|s| s.name == root_name).unwrap();
    assert_eq!(r.parent_span_id, None);
    assert_eq!(r.kind, SpanKind::Server);
    assert_eq!(r.service_name, service);
    assert_eq!(r.attributes["order.id"], serde_json::json!(run));
    assert_eq!(r.attributes["int.attr"], serde_json::json!(42));
    assert_eq!(r.attributes["double.attr"], serde_json::json!(1.5));
    assert_eq!(r.attributes["bool.attr"], serde_json::json!(true));
    let db = trace.spans.iter().find(|s| s.name == "db.query").unwrap();
    assert_eq!(db.parent_span_id.as_deref(), Some(r.span_id.as_str()));
    assert_eq!(db.kind, SpanKind::Client);
    assert_eq!(db.status.code, StatusCode::Error);
    assert!(db.events.iter().any(|e| e.name == "cache.miss"));
    assert_eq!(db.links[0].trace_id, other_cx.trace_id().to_string());
    let se = trace.spans.iter().find(|s| s.name == "set-error").unwrap();
    assert_eq!(se.status.code, StatusCode::Error);
    assert_eq!(se.status.message.as_deref(), Some("explicit"));
    assert_eq!(trace.tree().len(), 1);

    let same = q.get_trace(&trace_id).await.unwrap().unwrap();
    assert_eq!(same.spans.len(), 3);
    let raw = q
        .get_trace_with(
            &trace_id,
            GetTraceOptions {
                start: Some(SystemTime::now() - Duration::from_secs(3600)),
                end: Some(SystemTime::now() + Duration::from_secs(60)),
                raw_traces: true,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.spans.len(), 3);
    assert_eq!(q.require_trace(&trace_id).await.unwrap().trace_id, trace_id);
    assert!(q
        .get_trace("ffffffffffffffffffffffffffffff01")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        q.require_trace("ffffffffffffffffffffffffffffff01")
            .await
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );

    let query = TraceSearchQuery {
        min_duration: Some(Duration::from_nanos(1)),
        max_duration: Some(Duration::from_secs(3600)),
        ..TraceSearchQuery::service(service.clone())
            .operation(root_name.clone())
            .tag("order.id", run.clone())
    };
    let found = q.find_traces(&query).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].trace_id, trace_id);
    assert_eq!(found[0].spans.len(), 3);
    assert!(q
        .find_traces(&TraceSearchQuery::service(service.clone()).operation("nope"))
        .await
        .unwrap()
        .is_empty());
    let sums = q
        .search_traces(&TraceSearchQuery::service(service.clone()).limit(10))
        .await
        .unwrap();
    let s = sums
        .iter()
        .find(|s| s.trace_id == trace_id)
        .expect("summary");
    assert_eq!(s.root_name.as_deref(), Some(root_name.as_str()));
    assert_eq!(s.root_service_name.as_deref(), Some(service.as_str()));
    assert_eq!(s.span_count, 3);
    assert!(s.duration_micros > 0);
    assert!(q
        .search_traces(&TraceSearchQuery::service(unique("nobody")))
        .await
        .unwrap()
        .is_empty());

    let services = q.list_services().await.unwrap();
    assert!(services.contains(&service) && services.contains(&"jaeger".to_owned()));
    let ops = q
        .list_operations(&service, ListOperationsOptions::default())
        .await
        .unwrap();
    assert!(ops.contains(&root_name) && ops.contains(&"db.query".to_owned()));
    let server_ops = q
        .get_operations(&service, ListOperationsOptions::span_kind(SpanKind::Server))
        .await
        .unwrap();
    assert_eq!(server_ops.len(), 1);
    assert_eq!(server_ops[0].name, root_name);
    assert_eq!(server_ops[0].span_kind, SpanKind::Server);
    let as_trait: &dyn TraceQueryClient = q;
    assert!(as_trait
        .list_operations(&service)
        .await
        .unwrap()
        .contains(&root_name));
    assert_eq!(
        as_trait
            .get_trace(&trace_id)
            .await
            .unwrap()
            .unwrap()
            .spans
            .len(),
        3
    );
    h.shutdown().await.unwrap();
    h.shutdown().await.unwrap();
    c.close().await;
}

#[tokio::test]
async fn dependencies_and_propagation() {
    let c = client();
    let gw = unique("sdk-rust-gw");
    let wk = unique("sdk-rust-wk");
    let gw_h = start(&gw, Protocol::HttpProtobuf, true);
    let wk_h = start(&wk, Protocol::HttpProtobuf, false);
    let gw_t = gw_h.tracer("gw", None);
    let client_span = gw_t
        .span_builder("call worker")
        .with_kind(OtelKind::Client)
        .start_with_context(&gw_t, &Context::new());
    let cx = Context::new().with_span(client_span);
    let trace_id = cx.span().span_context().trace_id().to_string();
    // propagate over a carrier (global propagator installed by the gateway handle)
    let mut carrier = HashMap::new();
    tr::inject_context(&cx, &mut carrier);
    assert!(carrier.contains_key("traceparent"));
    let parent = tr::extract(&carrier);
    let wk_t = wk_h.tracer("wk", None);
    let mut server_span = wk_t
        .span_builder("handle")
        .with_kind(OtelKind::Server)
        .start_with_context(&wk_t, &parent);
    server_span.end();
    cx.span().end();
    gw_h.force_flush(None).await.unwrap();
    wk_h.force_flush(None).await.unwrap();
    let trace = c
        .query()
        .wait_for_trace(&trace_id, wait().min_spans(2))
        .await
        .unwrap();
    let services: Vec<&str> = trace
        .spans
        .iter()
        .map(|s| s.service_name.as_str())
        .collect();
    assert!(services.contains(&gw.as_str()) && services.contains(&wk.as_str()));

    let links = c
        .query()
        .get_dependencies(DependencyOptions {
            start: Some(SystemTime::now() - Duration::from_secs(3600)),
            end: Some(SystemTime::now() + Duration::from_secs(60)),
        })
        .await
        .unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.parent == gw && l.child == wk && l.call_count >= 1),
        "{links:?}"
    );
    let at = c
        .query()
        .get_dependencies_at(None, Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    assert!(at.iter().any(|l| l.parent == gw && l.child == wk));
    gw_h.shutdown().await.unwrap();
    wk_h.shutdown().await.unwrap();
}

fn sampled(s: &JaegerRemoteSampler, name: &str) -> bool {
    s.should_sample(
        None,
        opentelemetry::trace::TraceId::from(0x0001_u128),
        name,
        &OtelKind::Internal,
        &[],
        &[],
    )
    .decision
        == opentelemetry::trace::SamplingDecision::RecordAndSample
}

#[tokio::test]
async fn remote_sampling() {
    let c = client();
    let fixture = required_env("GROW_JAEGER_SAMPLING_FIXTURE_URL");
    // stock all-in-one: probability 1 for every service
    let stock = c
        .sampling_strategy(&unique("sdk-rust-stock"))
        .await
        .unwrap();
    assert_eq!(stock.strategy_type, StrategyType::Probabilistic);
    assert_eq!(stock.sampling_rate, 1.0);
    assert_eq!(
        c.sampling_strategy(" ").await.unwrap_err().code(),
        ErrorCode::Validation
    );

    let ops = JaegerRemoteSampler::new(
        fixture.clone(),
        "sdk-sampling-ops",
        Duration::from_secs(3600),
    )
    .unwrap();
    let st = ops.refresh().await.unwrap();
    assert_eq!(st.sampling_rate, 0.8);
    assert_eq!(st.operation_rate("GET /fast"), Some(0.2));
    let rate = JaegerRemoteSampler::new(
        fixture.clone(),
        "sdk-sampling-rate",
        Duration::from_secs(3600),
    )
    .unwrap();
    rate.ready().await.unwrap();
    let st = rate.strategy().unwrap();
    assert_eq!(st.strategy_type, StrategyType::RateLimiting);
    assert_eq!(st.max_traces_per_second, Some(5.0));
    let n = (0..50).filter(|_| sampled(&rate, "op")).count();
    assert!((5..=7).contains(&n), "rate limiting sampled {n} of 50");
    assert!(!sampled(&rate, "/health"));
    let prob = JaegerRemoteSampler::new(
        fixture.clone(),
        "sdk-sampling-prob",
        Duration::from_secs(3600),
    )
    .unwrap();
    assert_eq!(prob.refresh().await.unwrap().sampling_rate, 0.25);

    // the remote sampler drives init_tracing: "/health" (rate 0) is dropped, others kept
    let service = unique("sdk-rust-remote");
    let mut config = TracingConfig::new(service.clone()).with_set_global(false);
    config.batch.scheduled_delay = Duration::from_millis(100);
    let h = sdk::init_tracing_with_remote_sampler(config, ops.clone()).unwrap();
    let t = h.tracer("t", None);
    assert!(!t.start("/health").span_context().is_sampled());
    // "sampled-op" uses the service rate 0.8: find a trace id the sampler keeps
    let mut trace_id = None;
    for _ in 0..50 {
        let mut span = t.start_with_context("sampled-op", &Context::new());
        let sampled = span.span_context().is_sampled();
        let id = span.span_context().trace_id().to_string();
        span.end();
        if sampled {
            trace_id = Some(id);
            break;
        }
    }
    let trace_id = trace_id.expect("no span was sampled at rate 0.8");
    h.force_flush(None).await.unwrap();
    let trace = c.query().wait_for_trace(&trace_id, wait()).await.unwrap();
    assert_eq!(trace.spans[0].service_name, service);
    h.shutdown().await.unwrap();

    // client-made sampler (stock strategy) stops with the client
    let s = c
        .remote_sampler(&service, Duration::from_secs(3600))
        .unwrap();
    s.ready().await.unwrap();
    assert_eq!(s.strategy().unwrap().sampling_rate, 1.0);
    for x in [&ops, &rate, &prob] {
        x.close().await;
    }
    c.close().await;
    assert!(s.is_closed());

    // OTEL_TRACES_SAMPLER=parentbased_jaeger_remote with the URL from the environment
    let env = EnvReader::from_map([
        (
            "OTEL_TRACES_SAMPLER".to_owned(),
            "parentbased_jaeger_remote".to_owned(),
        ),
        ("GROW_JAEGER_SAMPLING_URL".to_owned(), fixture.clone()),
        (
            "GROW_JAEGER_OTLP_HTTP_ENDPOINT".to_owned(),
            required_env("GROW_JAEGER_OTLP_HTTP_ENDPOINT"),
        ),
    ]);
    let h = sdk::init_tracing_with_options(
        TracingConfig::new("sdk-sampling-ops").with_set_global(false),
        JaegerTracingOptions {
            env: Some(env),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    let remote = h.remote_sampler().unwrap().clone();
    remote.ready().await.unwrap();
    assert_eq!(
        remote.strategy().unwrap().operation_rate("/health"),
        Some(0.0)
    );
    h.shutdown().await.unwrap();
    assert!(remote.is_closed());
}

#[tokio::test]
async fn presets_config_and_exporter() {
    load_bootstrap_env();
    let cfg = sdk::tracing_config_from_env(TracingConfig::new("x"), &EnvReader::new()).unwrap();
    assert_eq!(cfg.protocol, sdk::DEFAULT_TRACING_PROTOCOL);
    assert_ne!(
        cfg.endpoint.as_deref(),
        Some(sdk::DEFAULT_OTLP_HTTP_ENDPOINT)
    );
    let service = unique("sdk-rust-exporter");
    let config = SamplingPreset::DEVELOPMENT
        .apply(cfg.clone())
        .with_set_global(false);
    let mut config = config;
    config.service_name = service.clone();
    assert_eq!(config.sampler, SamplerKind::AlwaysOn);
    // the exporter factory with the category init
    let h = tr::init_tracing_with(config, sdk::jaeger_exporter).unwrap();
    let t = h.tracer("t", None);
    let mut s = t.start_with_context("preset-op", &Context::new());
    let trace_id = s.span_context().trace_id().to_string();
    s.end();
    h.shutdown().await.unwrap();
    let c = client();
    let trace = c.query().wait_for_trace(&trace_id, wait()).await.unwrap();
    assert_eq!(trace.spans[0].name, "preset-op");
    assert_eq!(trace.spans[0].service_name, service);
    let p = SamplingPreset::ratio(0.5).apply(TracingConfig::new("x"));
    p.validate().unwrap();
    assert_eq!(SamplingPreset::OFF.sampler, SamplerKind::AlwaysOff);
    assert_eq!(SamplingPreset::PRODUCTION.ratio, 0.1);
    assert_eq!(
        SamplingPreset::FOLLOW_PARENT.sampler,
        SamplerKind::ParentBasedAlwaysOn
    );
    assert!(
        sdk::parse_sampling_strategy(br#"{"probabilisticSampling":{"samplingRate":0.5}}"#).is_ok()
    );
    assert_eq!(
        sdk::resolve_endpoint(Protocol::HttpProtobuf, Some("http://jaeger:4318")),
        "http://jaeger:4318/v1/traces"
    );
    // the provider is a normal OpenTelemetry provider
    let h = start(&unique("sdk-rust-provider"), Protocol::HttpProtobuf, false);
    let _ = h.tracer_provider().tracer("x");
    let _ = h.propagator();
    assert!(h.tracing_handle().is_some());
    h.shutdown().await.unwrap();
}

#[test]
fn blocking_query_round_trip() {
    load_bootstrap_env();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let service = unique("sdk-rust-blocking");
    let trace_id = rt.block_on(async {
        let h = start(&service, Protocol::HttpProtobuf, false);
        let t = h.tracer("t", None);
        let mut s = t.start_with_context("blocking-op", &Context::new());
        let id = s.span_context().trace_id().to_string();
        s.end();
        h.shutdown().await.unwrap();
        id
    });
    drop(rt);
    let c = sdk::blocking::JaegerClient::from_env().unwrap();
    assert!(c.health().unwrap().healthy);
    c.ping().unwrap();
    assert_eq!(c.server_version().unwrap(), "2.23.0");
    assert!(c.build_info().unwrap().is_some());
    let trace = c.wait_for_trace(&trace_id, wait()).unwrap();
    assert_eq!(c.require_trace(&trace_id).unwrap().trace_id, trace_id);
    assert_eq!(trace.spans[0].service_name, service);
    assert!(c.get_trace(&trace_id).unwrap().is_some());
    assert!(c
        .get_trace_with(&trace_id, GetTraceOptions::default())
        .unwrap()
        .is_some());
    assert_eq!(
        c.find_traces(&TraceSearchQuery::service(service.clone()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        c.search_traces(&TraceSearchQuery::service(service.clone()))
            .unwrap()
            .len(),
        1
    );
    assert!(c.list_services().unwrap().contains(&service));
    assert_eq!(
        c.list_operations(&service, ListOperationsOptions::default())
            .unwrap(),
        vec!["blocking-op".to_owned()]
    );
    assert_eq!(
        c.get_operations(&service, ListOperationsOptions::default())
            .unwrap()
            .len(),
        1
    );
    let _ = c.get_dependencies(DependencyOptions::default()).unwrap();
    assert_eq!(c.sampling_strategy(&service).unwrap().sampling_rate, 1.0);
    c.close();
}
