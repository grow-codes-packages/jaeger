// Taken over from sdk-kit 0.1.0 (header removed): the skeleton tests targeted the demo
// service; these test the Jaeger client surface against recorded Jaeger responses.
//! `JaegerClient` and `JaegerConfig`: health, ping, server version, sampling, close.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use grow_jaeger as sdk;
use sdk::core::{EnvReader, ErrorCode};
use sdk::{JaegerClient as Client, JaegerConfig as Config, StrategyType};
use serde_json::json;
use support::{client, closed_port, fast_retry, fixture, mount, respond, text};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn html(version: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!(
            "<html><script>// <!-- JAEGER_VERSION -->\nJAEGER_VERSION = {{\"gitVersion\":\"{version}\"}};</script></html>"
        ),
        "text/html; charset=utf-8",
    )
}

#[test]
fn config_from_env_and_builder() {
    let env = EnvReader::from_map([
        ("JAEGER_QUERY_URL", "http://query:16686"),
        ("GROW_JAEGER_TOKEN", "env-token-123"),
        ("JAEGER_SAMPLING_ENDPOINT", "http://agent:5778/sampling"),
        ("GROW_JAEGER_HEALTH_URL", "http://j:13133/status"),
        ("GROW_JAEGER_METRICS_URL", "http://j:8888/metrics"),
    ]);
    let cfg = Config::builder().build_with_env(&env).unwrap();
    assert_eq!(cfg.url().as_str(), "http://query:16686/");
    assert_eq!(cfg.token().unwrap().expose_secret(), "env-token-123");
    assert_eq!(cfg.sampling_url().as_str(), "http://agent:5778/sampling");
    assert_eq!(cfg.health_url().unwrap().as_str(), "http://j:13133/status");
    assert_eq!(cfg.metrics_url().unwrap().as_str(), "http://j:8888/metrics");
    assert!(cfg.common().headers().is_empty());
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains("env-token-123"));

    // GROW_* wins over vendor variables, explicit wins over both
    let env = EnvReader::from_map([
        ("GROW_JAEGER_URL", "http://grow:16686"),
        ("JAEGER_QUERY_URL", "http://vendor:16686"),
        ("GROW_JAEGER_SAMPLING_URL", "http://grow:5778/sampling"),
    ]);
    let cfg = Config::builder().build_with_env(&env).unwrap();
    assert_eq!(cfg.url().host_str(), Some("grow"));
    assert_eq!(cfg.sampling_url().host_str(), Some("grow"));
    let cfg = Config::builder()
        .url("http://explicit:1")
        .sampling_url("http://explicit:2/sampling")
        .build_with_env(&env)
        .unwrap();
    assert_eq!(cfg.url().host_str(), Some("explicit"));
    assert_eq!(cfg.sampling_url().port(), Some(2));

    // defaults
    let cfg = Config::builder()
        .build_with_env(&EnvReader::from_map(Vec::<(String, String)>::new()))
        .unwrap();
    assert_eq!(cfg.url().as_str(), "http://127.0.0.1:16686/");
    assert_eq!(cfg.sampling_url().as_str(), sdk::DEFAULT_SAMPLING_URL);
    assert!(cfg.health_url().is_none() && cfg.metrics_url().is_none() && cfg.token().is_none());

    // invalid values
    for b in [
        Config::builder().sampling_url("ftp://x"),
        Config::builder().health_url("not a url"),
        Config::builder().metrics_url("/relative"),
        Config::builder().url("mailto:x"),
        Config::builder().token(""),
    ] {
        assert_eq!(b.build().unwrap_err().code(), ErrorCode::Config);
    }
    let cfg = Config::builder()
        .ca_cert_path("/nonexistent/ca.pem")
        .build()
        .unwrap();
    assert_eq!(Client::new(cfg).unwrap_err().code(), ErrorCode::Config);
    let _ = Client::from_env();
    let _ = Config::from_env();
    // builder setters
    let cfg = Config::builder()
        .connect_timeout(Duration::from_secs(1))
        .request_timeout(Duration::from_secs(2))
        .total_timeout(Duration::from_secs(3))
        .max_retries(1)
        .insecure_skip_verify(true)
        .proxy("http://proxy:3128")
        .header("x-a", "b")
        .user_agent_suffix("test")
        .tls(sdk::core::TlsConfig::default())
        .common(|c| c.header("x-c", "d"))
        .hooks(Arc::new(NoHooks))
        .build()
        .unwrap();
    assert_eq!(cfg.common().request_timeout(), Duration::from_secs(2));
}

struct NoHooks;
impl sdk::core::Hooks for NoHooks {}

#[tokio::test]
async fn health_variants() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(header("accept", "text/html"))
        .respond_with(html("v2.23.0"))
        .mount(&server)
        .await;
    mount(&server, "/status", "health.status_ok").await;
    Mock::given(path("/bad/status"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(
                json!({"healthy": false, "status": "StatusRecoverableError", "n": 1}),
            ),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    let h = c.health().await.unwrap();
    assert!(h.healthy);
    assert_eq!(h.status, "ok");
    assert_eq!(h.details["httpStatus"], "200");

    let with_detail = Client::new(
        Config::builder()
            .url(server.uri())
            .health_url(format!("{}/status", server.uri()))
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    let h = with_detail.health().await.unwrap();
    assert!(h.healthy, "{h:?}");
    assert_eq!(h.details["healthcheck.status"], "StatusOK");
    assert_eq!(h.details["healthcheck.healthy"], "true");

    let degraded = Client::new(
        Config::builder()
            .url(server.uri())
            .health_url(format!("{}/bad/status", server.uri()))
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    let h = degraded.health().await.unwrap();
    assert!(!h.healthy);
    assert_eq!(h.status, "degraded");
    assert_eq!(h.details["healthcheck.n"], "1");

    // query root down → unhealthy, detail endpoint not called
    let down = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&down)
        .await;
    let c = Client::new(
        Config::builder()
            .url(down.uri())
            .health_url(format!("{}/status", server.uri()))
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    let h = c.health().await.unwrap();
    assert_eq!((h.healthy, h.status.as_str()), (false, "unhealthy"));
    assert_eq!(h.details["httpStatus"], "502");

    // unreachable, then closed
    let c = support::client_at(&format!("http://127.0.0.1:{}", closed_port()));
    let h = c.health().await.unwrap();
    assert_eq!((h.healthy, h.status.as_str()), (false, "unreachable"));
    c.close().await;
    assert_eq!(c.health().await.unwrap_err().code(), ErrorCode::Cancelled);
}

#[tokio::test]
async fn ping_with_token() {
    let server = MockServer::start().await;
    Mock::given(path("/api/v3/services"))
        .and(header("authorization", "Bearer ping-token-9"))
        .respond_with(respond("listServices.ok"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/services"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "unauthorized"})))
        .mount(&server)
        .await;
    let c = Client::new(
        Config::builder()
            .url(server.uri())
            .token("ping-token-9")
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(c.ping().await.unwrap() < Duration::from_secs(5));
    assert!(!format!("{c:?}").contains("ping-token-9"));
    let err = client(&server).ping().await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Authentication);
}

#[tokio::test]
async fn server_version_sources() {
    let ui = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(html("v2.23.0"))
        .mount(&ui)
        .await;
    assert_eq!(client(&ui).server_version().await.unwrap(), "2.23.0");
    let info = client(&ui).build_info().await.unwrap().unwrap();
    assert_eq!(info.git_version.as_deref(), Some("v2.23.0"));

    // UI without a version (404 is not NOT_FOUND) → metrics fallback
    let metrics = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(text(404, "404 page not found\n"))
        .mount(&metrics)
        .await;
    Mock::given(path("/metrics"))
        .respond_with(text(
            200,
            "# TYPE target_info gauge\ntarget_info{service_name=\"jaeger\",service_version=\"v2.23.0\"} 1\n",
        ))
        .mount(&metrics)
        .await;
    let with_metrics = |uri: String| {
        Client::new(
            Config::builder()
                .url(uri.clone())
                .metrics_url(format!("{uri}/metrics"))
                .retry(fast_retry())
                .build()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        with_metrics(metrics.uri()).server_version().await.unwrap(),
        "2.23.0"
    );
    // neither source → UNSUPPORTED
    assert_eq!(client(&metrics).build_info().await.unwrap(), None);
    let err = client(&metrics).server_version().await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unsupported);
    let empty = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
        .mount(&empty)
        .await;
    Mock::given(path("/metrics"))
        .respond_with(text(200, "up 1\n"))
        .mount(&empty)
        .await;
    assert_eq!(
        with_metrics(empty.uri())
            .server_version()
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    let gone = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(text(404, "nope"))
        .mount(&gone)
        .await;
    assert_eq!(
        with_metrics(gone.uri())
            .server_version()
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    // other errors propagate
    let denied = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&denied)
        .await;
    assert_eq!(
        client(&denied).server_version().await.unwrap_err().code(),
        ErrorCode::PermissionDenied
    );
    let metrics_denied = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&metrics_denied)
        .await;
    Mock::given(path("/metrics"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&metrics_denied)
        .await;
    assert_eq!(
        with_metrics(metrics_denied.uri())
            .server_version()
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Authentication
    );
    // the recorded index page excerpt parses too
    let rec = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("serverVersion.index_html").body, "text/html"),
        )
        .mount(&rec)
        .await;
    assert_eq!(client(&rec).server_version().await.unwrap(), "2.23.0");
}

#[tokio::test]
async fn sampling_strategy_and_samplers() {
    let server = MockServer::start().await;
    Mock::given(path("/sampling"))
        .and(query_param("service", "svc-a"))
        .respond_with(respond("sampling.default_stock"))
        .mount(&server)
        .await;
    Mock::given(path("/sampling"))
        .and(query_param("service", "garbage"))
        .respond_with(text(200, "not json"))
        .mount(&server)
        .await;
    Mock::given(path("/sampling"))
        .respond_with(respond("sampling.missing_service"))
        .mount(&server)
        .await;
    let c = Client::new(
        Config::builder()
            .url(server.uri())
            .sampling_url(format!("{}/sampling", server.uri()))
            .token("samp-token")
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    let s = c.sampling_strategy("svc-a").await.unwrap();
    assert_eq!(s.strategy_type, StrategyType::Probabilistic);
    assert_eq!(s.sampling_rate, 1.0);
    assert_eq!(
        c.sampling_strategy(" ").await.unwrap_err().code(),
        ErrorCode::Validation
    );
    let err = c.sampling_strategy("garbage").await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unavailable);
    assert_eq!(
        c.sampling_strategy("other").await.unwrap_err().code(),
        ErrorCode::Validation
    );

    let sampler = c
        .remote_sampler("svc-a", Duration::from_secs(3600))
        .unwrap();
    sampler.ready().await.unwrap();
    assert_eq!(sampler.strategy().unwrap().sampling_rate, 1.0);
    assert_eq!(sampler.service(), "svc-a");
    // own transport + explicit URL
    let common = c.config().common().clone();
    let own = c
        .remote_sampler_with(
            sdk::RemoteSamplerOptions::new("svc-a")
                .sampling_url(format!("{}/sampling", server.uri()))
                .http_config(common)
                .auto_start(false),
        )
        .unwrap();
    own.refresh().await.unwrap();
    assert_eq!(
        c.remote_sampler_with(sdk::RemoteSamplerOptions::new("x").sampling_url("nope"))
            .unwrap_err()
            .code(),
        ErrorCode::Config
    );
    c.close().await;
    assert!(sampler.is_closed() && own.is_closed());
    assert_eq!(
        c.remote_sampler("svc-a", Duration::ZERO)
            .unwrap_err()
            .code(),
        ErrorCode::Cancelled
    );
}

#[tokio::test]
async fn close_idempotent() {
    let server = MockServer::start().await;
    let c = client(&server);
    let closed = Arc::new(AtomicU32::new(0));
    let counter = closed.clone();
    c.on_close(move || {
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    });
    assert!(!c.is_closed());
    c.close().await;
    c.close().await;
    assert!(c.is_closed());
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(c.ping().await.unwrap_err().code(), ErrorCode::Cancelled);
    assert!(c.transport().is_closed());
}

#[tokio::test]
async fn init_tracing_uses_client_config_and_close_shuts_down() {
    let server = MockServer::start().await;
    Mock::given(path("/sampling"))
        .respond_with(respond("sampling.probabilistic"))
        .mount(&server)
        .await;
    let c = Client::new(
        Config::builder()
            .url(server.uri())
            .sampling_url(format!("{}/sampling", server.uri()))
            .token("t0ken-1234")
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    let env = EnvReader::from_map([("OTEL_TRACES_SAMPLER", "jaeger_remote")]);
    let h = c
        .init_tracing(
            sdk::tracing::TracingConfig::new("svc-client")
                .with_endpoint(server.uri())
                .with_set_global(false),
            sdk::JaegerTracingOptions {
                env: Some(env),
                ..sdk::JaegerTracingOptions::default()
            },
        )
        .unwrap();
    let sampler = h.remote_sampler().unwrap().clone();
    sampler.ready().await.unwrap();
    assert_eq!(sampler.strategy().unwrap().sampling_rate, 0.25);
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().any(|r| r
        .headers
        .get("authorization")
        .is_some_and(|v| v == "Bearer t0ken-1234")));
    c.close().await;
    assert!(h.is_shutdown());
    let err = c
        .init_tracing(
            sdk::tracing::TracingConfig::new("x"),
            sdk::JaegerTracingOptions::default(),
        )
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Cancelled);
}
