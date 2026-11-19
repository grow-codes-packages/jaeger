//! `init_tracing` for Jaeger: environment resolution, the exporter, presets and remote sampling.

mod support;

use std::time::Duration;

use grow_jaeger as sdk;
use opentelemetry::trace::{Span as _, TraceContextExt, Tracer as _};
use sdk::core::{EnvReader, ErrorCode, TlsConfig};
use sdk::tracing::{Compression, PropagatorKind, Protocol, SamplerKind, TracingConfig};
use sdk::{
    init_tracing_with_options, jaeger_exporter, tracing_config_from_env, JaegerConfig,
    JaegerRemoteSampler, JaegerTracingOptions, RemoteSamplerOptions, RemoteSampling,
    SamplingPreset,
};
use support::respond;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn env(vars: &[(&str, &str)]) -> EnvReader {
    EnvReader::from_map(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")).into()
}

#[test]
fn config_from_env_precedence() {
    let e = env(&[
        ("GROW_JAEGER_OTLP_PROTOCOL", "grpc"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
        ("GROW_JAEGER_OTLP_GRPC_ENDPOINT", "http://j:4317"),
        (
            "GROW_JAEGER_OTLP_HEADERS",
            "authorization=Bearer%20abc,x-a=1",
        ),
        ("OTEL_EXPORTER_OTLP_COMPRESSION", "none"),
        ("GROW_JAEGER_OTLP_CA_CERT", "/ca.pem"),
        ("OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE", "/c.pem"),
        ("OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY", "/k.pem"),
        ("GROW_JAEGER_SERVICE_NAME", "from-grow"),
        ("OTEL_SERVICE_NAME", "from-otel"),
        ("OTEL_RESOURCE_ATTRIBUTES", "team=core,region=eu"),
        ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
        ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
        ("OTEL_PROPAGATORS", "tracecontext,b3,none"),
    ]);
    let c = tracing_config_from_env(
        TracingConfig::new("").with_resource_attribute("team", "explicit"),
        &e,
    )
    .unwrap();
    assert_eq!(c.protocol, Protocol::Grpc);
    assert_eq!(c.endpoint.as_deref(), Some("http://j:4317"));
    assert_eq!(c.headers["authorization"], "Bearer abc");
    assert_eq!(c.compression, Compression::None);
    let tls = c.tls.clone().unwrap();
    assert_eq!(tls.ca_cert_path.unwrap().to_str(), Some("/ca.pem"));
    assert_eq!(tls.client_cert_path.unwrap().to_str(), Some("/c.pem"));
    assert_eq!(tls.client_key_path.unwrap().to_str(), Some("/k.pem"));
    assert_eq!(c.service_name, "from-grow");
    assert_eq!(c.resource_attributes["team"], "explicit");
    assert_eq!(c.resource_attributes["region"], "eu");
    assert_eq!(
        (c.sampler, c.sampler_ratio),
        (SamplerKind::ParentBasedTraceIdRatio, 0.25)
    );
    assert_eq!(
        c.propagators,
        vec![PropagatorKind::TraceContext, PropagatorKind::B3]
    );
    assert!(!format!("{c:?}").contains("Bearer abc"));

    // defaults: http/protobuf with the Jaeger endpoint
    let c = tracing_config_from_env(TracingConfig::new("x"), &env(&[])).unwrap();
    assert_eq!(c.protocol, Protocol::HttpProtobuf);
    assert_eq!(c.endpoint.as_deref(), Some(sdk::DEFAULT_OTLP_HTTP_ENDPOINT));
    let c = tracing_config_from_env(
        TracingConfig::new("x").with_protocol(Protocol::Grpc),
        &env(&[]),
    )
    .unwrap();
    assert_eq!(c.endpoint.as_deref(), Some(sdk::DEFAULT_OTLP_GRPC_ENDPOINT));

    // endpoint chain for HTTP
    let cases: [(&[(&str, &str)], &str); 4] = [
        (
            &[("GROW_JAEGER_OTLP_HTTP_ENDPOINT", "http://j:4318")],
            "http://j:4318/v1/traces",
        ),
        (
            &[("GROW_JAEGER_OTLP_ENDPOINT", "http://g:4318/x")],
            "http://g:4318/x",
        ),
        (
            &[(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://t:4318/v1/traces",
            )],
            "http://t:4318/v1/traces",
        ),
        (
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://b:4318/")],
            "http://b:4318/v1/traces",
        ),
    ];
    for (vars, want) in cases {
        let c = tracing_config_from_env(TracingConfig::new("x"), &env(vars)).unwrap();
        assert_eq!(c.endpoint.as_deref(), Some(want));
    }
    let c = tracing_config_from_env(
        TracingConfig::new("x").with_protocol(Protocol::Grpc),
        &env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://b:4317")]),
    )
    .unwrap();
    assert_eq!(c.endpoint.as_deref(), Some("http://b:4317"));
    // explicit wins
    let c = tracing_config_from_env(
        TracingConfig::new("x")
            .with_endpoint("http://explicit:4318")
            .with_header("x", "1")
            .with_sampler(SamplerKind::AlwaysOn, 1.0),
        &env(&[
            ("GROW_JAEGER_OTLP_HTTP_ENDPOINT", "http://env:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "y=2"),
            ("OTEL_TRACES_SAMPLER", "always_off"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
            ("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.5"),
        ]),
    )
    .unwrap();
    assert_eq!(
        c.endpoint.as_deref(),
        Some("http://explicit:4318/v1/traces")
    );
    assert!(c.headers.contains_key("x") && !c.headers.contains_key("y"));
    assert_eq!(c.sampler, SamplerKind::AlwaysOn);
    let c = tracing_config_from_env(
        TracingConfig::new("x"),
        &env(&[
            ("OTEL_TRACES_SAMPLER", "always_off"),
            ("OTEL_TRACES_SAMPLER_ARG", "x"),
        ]),
    )
    .unwrap();
    assert_eq!(c.sampler, SamplerKind::AlwaysOff);
}

#[test]
fn config_from_env_errors_name_the_variable() {
    for (var, value) in [
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ("GROW_JAEGER_OTLP_HEADERS", "nope"),
        ("OTEL_EXPORTER_OTLP_COMPRESSION", "zstd"),
        ("OTEL_RESOURCE_ATTRIBUTES", "=x"),
        ("OTEL_TRACES_SAMPLER", "sometimes"),
        ("OTEL_PROPAGATORS", "tracecontext,,"),
    ] {
        let r = tracing_config_from_env(TracingConfig::new("x"), &env(&[(var, value)]));
        if var == "OTEL_PROPAGATORS" {
            assert!(r.is_ok());
            continue;
        }
        let err = r.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Config);
        assert!(err.message().contains(var), "{err}");
        assert!(!err.message().contains(value) || value.len() < 3);
    }
    let err = tracing_config_from_env(
        TracingConfig::new("x"),
        &env(&[
            ("OTEL_TRACES_SAMPLER", "traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "2"),
        ]),
    )
    .unwrap_err();
    assert!(err.message().contains("OTEL_TRACES_SAMPLER_ARG"));
}

#[test]
fn presets() {
    let base = TracingConfig::new("x");
    assert_eq!(
        SamplingPreset::DEVELOPMENT.apply(base.clone()).sampler,
        SamplerKind::AlwaysOn
    );
    assert_eq!(SamplingPreset::PRODUCTION.ratio, 0.1);
    assert_eq!(
        SamplingPreset::FOLLOW_PARENT.sampler,
        SamplerKind::ParentBasedAlwaysOn
    );
    assert_eq!(SamplingPreset::OFF.sampler, SamplerKind::AlwaysOff);
    assert_eq!(
        SamplingPreset::ratio(0.0).sampler,
        SamplerKind::ParentBasedAlwaysOff
    );
    assert_eq!(
        SamplingPreset::ratio(f64::NAN).sampler,
        SamplerKind::ParentBasedAlwaysOff
    );
    assert_eq!(SamplingPreset::ratio(1.5), SamplingPreset::FOLLOW_PARENT);
    let p = SamplingPreset::ratio(0.5).apply(base);
    assert_eq!(
        (p.sampler, p.sampler_ratio),
        (SamplerKind::ParentBasedTraceIdRatio, 0.5)
    );
    p.validate().unwrap();
}

#[tokio::test]
async fn exporter_variants() {
    let http = TracingConfig::new("x").with_endpoint("http://127.0.0.1:4318");
    jaeger_exporter(&http).unwrap();
    jaeger_exporter(&http.clone().with_compression(Compression::None)).unwrap();
    let tls = TlsConfig {
        ca_cert_path: Some(fixture_path("ca.pem")),
        client_cert_path: Some(fixture_path("client.pem")),
        client_key_path: Some(fixture_path("client.key")),
        insecure_skip_verify: true,
        ..TlsConfig::default()
    };
    jaeger_exporter(&http.clone().with_tls(tls.clone())).unwrap();
    let pem = std::fs::read_to_string(fixture_path("ca.pem")).unwrap();
    jaeger_exporter(&http.clone().with_tls(TlsConfig {
        ca_cert_pem: Some(pem.clone()),
        ..TlsConfig::default()
    }))
    .unwrap();
    for bad in [
        TlsConfig {
            ca_cert_path: Some("/nonexistent/ca.pem".into()),
            ..TlsConfig::default()
        },
        TlsConfig {
            client_cert_path: Some(fixture_path("client.pem")),
            ..TlsConfig::default()
        },
        TlsConfig {
            client_cert_path: Some(fixture_path("ca.pem")),
            client_key_path: Some(fixture_path("ca.pem")),
            ..TlsConfig::default()
        },
    ] {
        let err = jaeger_exporter(&http.clone().with_tls(bad)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Config);
    }

    let grpc = TracingConfig::new("x")
        .with_protocol(Protocol::Grpc)
        .with_endpoint("http://127.0.0.1:4317")
        .with_header("X-Team", "core");
    jaeger_exporter(&grpc).unwrap();
    jaeger_exporter(&grpc.clone().with_compression(Compression::None)).unwrap();
    let grpc_tls = jaeger_exporter(
        &grpc
            .clone()
            .with_endpoint("https://127.0.0.1:4317")
            .with_tls(TlsConfig {
                insecure_skip_verify: false,
                ..tls.clone()
            }),
    );
    if cfg!(feature = "otlp-tls") {
        grpc_tls.unwrap();
    } else {
        // Without the crate feature `otlp-tls` the gRPC exporter cannot use TLS.
        let err = grpc_tls.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Config);
        assert!(err.message().contains("otlp-tls"), "{err}");
    }
    for bad in [
        grpc.clone().with_tls(tls),
        grpc.clone().with_header("bad header", "x"),
        grpc.clone().with_header("x", "bad\nvalue"),
        TracingConfig::new("x").with_protocol(Protocol::Native),
    ] {
        assert_eq!(jaeger_exporter(&bad).unwrap_err().code(), ErrorCode::Config);
    }
}

fn no_env() -> JaegerTracingOptions {
    JaegerTracingOptions {
        ignore_env: true,
        ..JaegerTracingOptions::default()
    }
}

#[tokio::test]
async fn init_tracing_exports_otlp_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .and(header("content-type", "application/x-protobuf"))
        .respond_with(respond("otlpHttpProtobuf.ok"))
        .mount(&server)
        .await;
    let config = TracingConfig::new("sdk-rust-unit")
        .with_endpoint(server.uri())
        .with_compression(Compression::None)
        .with_set_global(false);
    let h = init_tracing_with_options(config, no_env()).unwrap();
    assert!(h.tracing_handle().is_some());
    assert!(h.remote_sampler().is_none());
    assert_eq!(h.endpoint(), format!("{}/v1/traces", server.uri()));
    assert_eq!(h.service_name(), "sdk-rust-unit");
    let tracer = h.tracer("unit", Some("1.0"));
    tracer.in_span("op", |cx| {
        cx.span()
            .set_attribute(opentelemetry::KeyValue::new("k", "v"));
    });
    h.force_flush(Some(Duration::from_secs(5))).await.unwrap();
    let _ = h.propagator();
    let _ = h.tracer_provider();
    assert!(!format!("{h:?}").is_empty());
    h.shutdown().await.unwrap();
    h.shutdown().await.unwrap();
    assert!(h.is_shutdown());
    let received = server.received_requests().await.unwrap();
    assert!(!received.is_empty());

    // process environment path and validation
    assert_eq!(
        sdk::init_tracing(TracingConfig::new(" ").with_set_global(false))
            .unwrap_err()
            .code(),
        ErrorCode::Config
    );
}

#[tokio::test]
async fn init_tracing_with_remote_sampler() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/sampling"))
        .respond_with(respond("sampling.per_operation"))
        .mount(&server)
        .await;
    let sampler = JaegerRemoteSampler::with_options(
        RemoteSamplerOptions::new("sdk-sampling-ops")
            .sampling_url(format!("{}/sampling", server.uri()))
            .auto_start(false),
    )
    .unwrap();
    sampler.refresh().await.unwrap();
    let config = TracingConfig::new("sdk-remote")
        .with_endpoint(server.uri())
        .with_set_global(true);
    let h = sdk::init_tracing_with_remote_sampler(config, sampler.clone()).unwrap();
    assert!(h.tracing_handle().is_none());
    assert_eq!(h.remote_sampler().unwrap().service(), "sdk-sampling-ops");
    let tracer = h.tracer("t", None);
    let dropped = tracer.start("/health");
    assert!(!dropped.span_context().is_sampled());
    let mut kept = tracer.start_with_context("other", &opentelemetry::Context::new());
    kept.end();
    h.force_flush(None).await.unwrap();
    h.force_flush(Some(Duration::from_secs(5))).await.unwrap();
    let _ = (h.propagator(), h.tracer_provider(), h.service_name());
    assert!(format!("{h:?}").contains("sdk-sampling-ops"));
    h.shutdown().await.unwrap();
    h.shutdown().await.unwrap();
    assert!(h.is_shutdown());
    // the handle does not close a sampler it was given
    assert!(!sampler.is_closed());
    sampler.close().await;

    // options-created sampler, not parent-based; closed on shutdown; service defaults
    let h = init_tracing_with_options(
        TracingConfig::new("sdk-sampling-ops")
            .with_endpoint(server.uri())
            .with_set_global(false),
        JaegerTracingOptions {
            ignore_env: true,
            parent_based: false,
            remote_sampling: Some(RemoteSampling::Options(Box::new(
                RemoteSamplerOptions::new("")
                    .sampling_url(format!("{}/sampling", server.uri()))
                    .auto_start(false),
            ))),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    let s = h.remote_sampler().unwrap().clone();
    assert_eq!(s.service(), "sdk-sampling-ops");
    h.shutdown().await.unwrap();
    assert!(s.is_closed());

    // drop without shutdown shuts down in the background
    let h = init_tracing_with_options(
        TracingConfig::new("x")
            .with_endpoint(server.uri())
            .with_set_global(false),
        JaegerTracingOptions {
            ignore_env: true,
            remote_sampling: Some(RemoteSampling::Options(Box::new(
                RemoteSamplerOptions::new("x")
                    .sampling_url(format!("{}/sampling", server.uri()))
                    .auto_start(false),
            ))),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    drop(h);

    // a failing pipeline closes the sampler it created
    let err = init_tracing_with_options(
        TracingConfig::new("x")
            .with_protocol(Protocol::Grpc)
            .with_header("bad header", "v"),
        JaegerTracingOptions {
            ignore_env: true,
            remote_sampling: Some(RemoteSampling::Options(Box::new(
                RemoteSamplerOptions::new("x").auto_start(false),
            ))),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Config);
    let err = init_tracing_with_options(
        TracingConfig::new("x"),
        JaegerTracingOptions {
            ignore_env: true,
            remote_sampling: Some(RemoteSampling::Options(Box::new(
                RemoteSamplerOptions::new("x").sampling_url("bad"),
            ))),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Config);
}

#[tokio::test]
async fn jaeger_remote_from_env() {
    let server = MockServer::start().await;
    Mock::given(path("/sampling"))
        .respond_with(respond("sampling.probabilistic"))
        .mount(&server)
        .await;
    let sampling = format!("{}/sampling", server.uri());
    let arg = format!("endpoint={sampling},pollingIntervalMs=50,initialSamplingRate=0.5,other=1");
    let e = env(&[
        ("OTEL_TRACES_SAMPLER", "parentbased_jaeger_remote"),
        ("OTEL_TRACES_SAMPLER_ARG", &arg),
    ]);
    let h = init_tracing_with_options(
        TracingConfig::new("sdk-env-remote")
            .with_endpoint(server.uri())
            .with_set_global(false),
        JaegerTracingOptions {
            env: Some(e),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    let s = h.remote_sampler().unwrap().clone();
    s.ready().await.unwrap();
    assert_eq!(s.strategy().unwrap().sampling_rate, 0.25);
    h.shutdown().await.unwrap();
    assert!(s.is_closed());

    // URL from GROW_JAEGER_SAMPLING_URL, root sampler (jaeger_remote)
    let e = env(&[
        ("OTEL_TRACES_SAMPLER", "jaeger_remote"),
        ("GROW_JAEGER_SAMPLING_URL", &sampling),
    ]);
    let h = init_tracing_with_options(
        TracingConfig::new("sdk-env-remote")
            .with_endpoint(server.uri())
            .with_set_global(false),
        JaegerTracingOptions {
            env: Some(e),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    assert!(h
        .remote_sampler()
        .unwrap()
        .to_string()
        .contains("/sampling"));
    h.shutdown().await.unwrap();

    // WithSamplerConfig equivalent: the token and URL come from sampler_config.
    let e = env(&[("OTEL_TRACES_SAMPLER", "jaeger_remote")]);
    let h = init_tracing_with_options(
        TracingConfig::new("sdk-env-remote")
            .with_endpoint(server.uri())
            .with_set_global(false),
        JaegerTracingOptions {
            env: Some(e),
            sampler_config: Some(
                JaegerConfig::builder()
                    .sampling_url(sampling.clone())
                    .token("s3cret-token")
                    .build()
                    .unwrap(),
            ),
            ..JaegerTracingOptions::default()
        },
    )
    .unwrap();
    let s = h.remote_sampler().unwrap().clone();
    s.ready().await.unwrap();
    assert_eq!(s.strategy().unwrap().sampling_rate, 0.25);
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().any(|r| r
        .headers
        .get("authorization")
        .is_some_and(|v| v == "Bearer s3cret-token")));
    h.shutdown().await.unwrap();

    for bad in [
        "pollingIntervalMs=x",
        "initialSamplingRate=2",
        "initialSamplingRate=x",
        "novalue",
    ] {
        let e = env(&[
            ("OTEL_TRACES_SAMPLER", "jaeger_remote"),
            ("OTEL_TRACES_SAMPLER_ARG", bad),
        ]);
        let err = init_tracing_with_options(
            TracingConfig::new("x").with_set_global(false),
            JaegerTracingOptions {
                env: Some(e),
                ..JaegerTracingOptions::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Config, "{bad}");
    }
}

#[test]
fn init_outside_runtime_fails() {
    let err = init_tracing_with_options(TracingConfig::new("x").with_set_global(false), no_env())
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Config);
}

#[test]
fn service_name_falls_back_to_resource_attributes() {
    let c = tracing_config_from_env(
        TracingConfig::new(""),
        &env(&[("OTEL_RESOURCE_ATTRIBUTES", "service.name=from-res,team=env")]),
    )
    .unwrap();
    assert_eq!(c.service_name, "from-res");
    assert!(!c.resource_attributes.contains_key("service.name"));
    assert!(c.resource_attributes.contains_key("team"));
}
