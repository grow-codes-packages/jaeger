//! Remote sampling: strategy parsing, the sampler rule, polling and lifecycle.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use grow_jaeger as sdk;
use opentelemetry::trace::{
    SamplingDecision, SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId,
    TraceState,
};
use opentelemetry::Context;
use opentelemetry_sdk::trace::ShouldSample;
use sdk::core::ErrorCode;
use sdk::{parse_sampling_strategy, JaegerRemoteSampler, RemoteSamplerOptions, StrategyType};
use support::{fixture, respond, text};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn decide(s: &JaegerRemoteSampler, id: u128, name: &str) -> bool {
    s.should_sample(None, TraceId::from(id), name, &SpanKind::Internal, &[], &[])
        .decision
        == SamplingDecision::RecordAndSample
}

#[test]
fn parse_recorded_strategies() {
    let ops = parse_sampling_strategy(&fixture("sampling.per_operation").body).unwrap();
    assert_eq!(ops.strategy_type, StrategyType::Probabilistic);
    assert_eq!(ops.sampling_rate, 0.8);
    assert_eq!(ops.default_sampling_probability, Some(0.8));
    assert_eq!(ops.operation_rate("GET /fast"), Some(0.2));
    assert_eq!(ops.operation_rate("/health"), Some(0.0));
    assert_eq!(ops.operation_rate("nope"), None);
    assert!(!ops.uses_rate_limiting());

    let rate = parse_sampling_strategy(&fixture("sampling.rate_limiting").body).unwrap();
    assert_eq!(rate.strategy_type, StrategyType::RateLimiting);
    assert_eq!(rate.max_traces_per_second, Some(5.0));
    assert!(rate.uses_rate_limiting());
    assert_eq!(rate.strategy_type.as_str(), "RATE_LIMITING");

    let prob = parse_sampling_strategy(&fixture("sampling.probabilistic").body).unwrap();
    assert_eq!(prob.sampling_rate, 0.25);
    assert_eq!(StrategyType::Probabilistic.as_str(), "PROBABILISTIC");
    for name in [
        "sampling.default_stock",
        "sampling.default_strategy",
        "sampling.after_reload",
        "sampling.post",
    ] {
        parse_sampling_strategy(&fixture(name).body).unwrap();
    }
}

#[test]
fn parse_edge_cases() {
    // proto3 omits zeros; missing type means probabilistic; numeric enum values
    let s = parse_sampling_strategy(br#"{"probabilisticSampling":{}}"#).unwrap();
    assert_eq!(s.sampling_rate, 0.0);
    let s = parse_sampling_strategy(br#"{"strategyType":1,"rateLimitingSampling":{}}"#).unwrap();
    assert_eq!(s.max_traces_per_second, Some(0.0));
    let s = parse_sampling_strategy(br#"{"strategyType":"RATE_LIMITING"}"#).unwrap();
    assert_eq!(s.max_traces_per_second, Some(0.0));
    let s = parse_sampling_strategy(
        br#"{"operationSampling":{"defaultSamplingProbability":0.3,"perOperationStrategies":[{"operation":"x","probabilisticSampling":{}}]}}"#,
    )
    .unwrap();
    assert_eq!(s.sampling_rate, 0.3);
    assert_eq!(s.operation_rate("x"), Some(0.0));
    let s = parse_sampling_strategy(
        br#"{"strategyType":null,"probabilisticSampling":{"samplingRate":null}}"#,
    )
    .unwrap();
    assert_eq!(s.sampling_rate, 0.0);
    for bad in [
        &b"not json"[..],
        b"[]",
        br#"{"strategyType":"ADAPTIVE"}"#,
        br#"{"probabilisticSampling":{"samplingRate":1.5}}"#,
        br#"{"probabilisticSampling":{"samplingRate":"x"}}"#,
        br#"{"rateLimitingSampling":{"maxTracesPerSecond":-1}}"#,
        br#"{"operationSampling":{"defaultLowerBoundTracesPerSecond":-1}}"#,
        br#"{"operationSampling":{"perOperationStrategies":[{"operation":"x","probabilisticSampling":{"samplingRate":2}}]}}"#,
    ] {
        assert_eq!(
            parse_sampling_strategy(bad).unwrap_err().code(),
            ErrorCode::Validation,
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

async fn strategy_server() -> MockServer {
    let server = MockServer::start().await;
    for (svc, name) in [
        ("ops", "sampling.per_operation"),
        ("rate", "sampling.rate_limiting"),
        ("prob", "sampling.probabilistic"),
    ] {
        Mock::given(path("/sampling"))
            .and(query_param("service", svc))
            .respond_with(respond(name))
            .mount(&server)
            .await;
    }
    Mock::given(path("/sampling"))
        .and(query_param("service", "zero"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"strategyType":"PROBABILISTIC","probabilisticSampling":{"samplingRate":0}}"#,
        ))
        .mount(&server)
        .await;
    server
}

fn opts(server: &MockServer, svc: &str) -> RemoteSamplerOptions {
    RemoteSamplerOptions::new(svc)
        .sampling_url(format!("{}/sampling", server.uri()))
        .poll_interval(Duration::from_secs(3600))
        .auto_start(false)
}

#[tokio::test]
async fn sampler_rule() {
    let server = strategy_server().await;
    // initial rate applies before the first strategy
    let s =
        JaegerRemoteSampler::with_options(opts(&server, "ops").initial_sampling_rate(0.0)).unwrap();
    assert!(s.strategy().is_none());
    assert!(!decide(&s, 1, "anything"));
    s.ready().await.unwrap();
    s.ready().await.unwrap();
    // per-operation: /health rate 0 dropped, others use 0.8 (deterministic per trace id)
    assert!(!decide(&s, u128::MAX, "/health"));
    let low_id = 1u128; // smallest ids are always sampled by ratio samplers > 0
    assert!(decide(&s, low_id, "other-op"));
    assert!(decide(&s, low_id, "GET /fast"));
    assert!(format!("{s:?}").contains("service=ops"));

    // rate limiting: listed operations still use their probability; others at most 5/s
    let r = JaegerRemoteSampler::with_options(opts(&server, "rate")).unwrap();
    r.refresh().await.unwrap();
    let mut n = 0;
    for i in 0..50 {
        if decide(&r, i + 1, "op") {
            n += 1;
        }
    }
    assert!((5..=7).contains(&n), "rate limited {n} of 50");
    assert!(!decide(&r, 1, "/health"));
    // refresh with the same rate keeps the bucket (still empty)
    r.refresh().await.unwrap();
    assert!(!decide(&r, 1, "op"));

    let p = JaegerRemoteSampler::with_options(opts(&server, "prob")).unwrap();
    assert_eq!(p.refresh().await.unwrap().sampling_rate, 0.25);
    let z = JaegerRemoteSampler::with_options(opts(&server, "zero")).unwrap();
    z.refresh().await.unwrap();
    assert!(!decide(&z, 1, "x"));

    // trace state of a sampled parent is kept by the rate limiter
    let parent = Context::new().with_remote_span_context(SpanContext::new(
        TraceId::from(9u128),
        SpanId::from(1u64),
        TraceFlags::SAMPLED,
        true,
        TraceState::from_key_value([("k", "v")]).unwrap(),
    ));
    let rate0 = MockServer::start().await;
    Mock::given(path("/sampling"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"strategyType":"RATE_LIMITING","rateLimitingSampling":{"maxTracesPerSecond":0}}"#,
        ))
        .mount(&rate0)
        .await;
    let r0 = JaegerRemoteSampler::with_options(opts(&rate0, "x")).unwrap();
    r0.refresh().await.unwrap();
    let res = r0.should_sample(
        Some(&parent),
        TraceId::from(9u128),
        "x",
        &SpanKind::Server,
        &[],
        &[],
    );
    // rate 0: the first trace goes through, then none
    assert_eq!(res.decision, SamplingDecision::RecordAndSample);
    assert_eq!(res.trace_state.get("k"), Some("v"));
    assert!(!decide(&r0, 2, "x"));
    for s in [s, r, p, z, r0] {
        s.close().await;
    }
}

#[tokio::test]
async fn polling_keeps_last_good_and_reports() {
    let server = MockServer::start().await;
    Mock::given(path("/sampling"))
        .respond_with(respond("sampling.probabilistic"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/sampling"))
        .respond_with(text(200, "{broken"))
        .mount(&server)
        .await;
    let errors = Arc::new(AtomicU32::new(0));
    let updates = Arc::new(AtomicU32::new(0));
    let (e, u) = (errors.clone(), updates.clone());
    let s = JaegerRemoteSampler::with_options(
        RemoteSamplerOptions::new("svc")
            .sampling_url(format!("{}/sampling", server.uri()))
            .poll_interval(Duration::from_millis(20))
            .token("sampler-secret")
            .http_config(
                sdk::core::CommonConfig::builder()
                    .retry(support::fast_retry().with_max_retries(0))
                    .build("http://ignored")
                    .unwrap(),
            )
            .on_error(move |_| {
                e.fetch_add(1, Ordering::SeqCst);
            })
            .on_update(move |_| {
                u.fetch_add(1, Ordering::SeqCst);
            }),
    )
    .unwrap();
    s.start(); // already started: no-op
    for _ in 0..200 {
        if errors.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(updates.load(Ordering::SeqCst), 1);
    assert!(errors.load(Ordering::SeqCst) >= 2);
    assert_eq!(s.strategy().unwrap().sampling_rate, 0.25);
    let dbg = format!("{:?}", opts(&server, "x").token("abc-secret"));
    assert!(!dbg.contains("abc-secret"));
    let clone = s.clone();
    s.close().await;
    clone.close().await;
    assert!(clone.is_closed());
    assert_eq!(s.refresh().await.unwrap_err().code(), ErrorCode::Cancelled);
    s.start(); // closed: no-op
}

#[tokio::test]
async fn constructor_validation() {
    let ok =
        JaegerRemoteSampler::new("http://127.0.0.1:1/sampling", "svc", Duration::ZERO).unwrap();
    ok.close().await;
    for o in [
        RemoteSamplerOptions::new(" "),
        RemoteSamplerOptions::new("svc").initial_sampling_rate(2.0),
        RemoteSamplerOptions::new("svc").sampling_url("ftp://x"),
        RemoteSamplerOptions::new("svc").sampling_url("relative"),
    ] {
        assert_eq!(
            JaegerRemoteSampler::with_options(o).unwrap_err().code(),
            ErrorCode::Config
        );
    }
    // default URL, no auto start
    let s = JaegerRemoteSampler::with_options(RemoteSamplerOptions::new("svc").auto_start(false))
        .unwrap();
    assert!(s.to_string().contains("127.0.0.1:5778"));
    drop(s);
}

#[test]
fn needs_runtime_for_auto_start() {
    let err =
        JaegerRemoteSampler::new("http://127.0.0.1:1/sampling", "svc", Duration::ZERO).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Config);
    let limiter = sdk::RateLimiter::new(1.0);
    assert!(limiter.check());
    assert!(!limiter.check());
}
