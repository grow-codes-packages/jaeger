// Created by sdk-kit 0.1.0; owned by this repository.
//! Integration suite against the real service (Docker): client round trips and the
//! conformance scenarios for `core` and this service's categories.

use std::sync::Arc;
use std::time::Duration;

use grow_jaeger as sdk;
use grow_jaeger_integration as support;
use sdk::testing::tracing::{TracingHandler, TracingTarget};
use sdk::testing::{ConformanceRunner, CoreHandler};
use sdk::tracing::{TraceQueryClient, TracingConfig};
use sdk::ErrorCode;
use support::{conformance_dir, load_bootstrap_env};

use sdk::blocking::JaegerClient as BlockingClient;

use sdk::JaegerClient as Client;
use sdk::JaegerConfig as Config;

fn client() -> Client {
    load_bootstrap_env();
    Client::new(Config::from_env().expect("config from env")).expect("client")
}

#[tokio::test]
async fn client_round_trip() {
    let c = client();
    let health = c.health().await.expect("health");
    assert!(health.healthy, "service not healthy: {health:?}");
    c.ping().await.expect("authenticated ping");
    assert_eq!(c.server_version().await.expect("version"), "2.23.0");
    // unreachable services report unhealthy instead of failing
    let down = Client::new(
        Config::builder()
            .url("http://127.0.0.1:9")
            .max_retries(0)
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(!down.health().await.unwrap().healthy);
    assert_eq!(
        down.ping().await.unwrap_err().code(),
        ErrorCode::Unavailable
    );
    c.close().await;
    c.close().await;
}

#[test]
fn blocking_round_trip() {
    load_bootstrap_env();
    let c = BlockingClient::from_env().expect("client");
    assert!(c.health().expect("health").healthy);
    c.ping().expect("ping");
    c.close();
}

#[tokio::test]
async fn conformance() {
    load_bootstrap_env();
    let service = format!(
        "grow-conformance-rust-{}",
        std::env::var("GROW_TEST_RUN_ID").unwrap_or_else(|_| "local".into())
    );
    let factory_service = service.clone();
    #[allow(unused_mut)]
    let mut runner = ConformanceRunner::new(conformance_dir())
        // Jaeger serves every tracing capability: query (v3 API), services and dependencies.
        .capabilities(["version", "query", "services", "dependencies"])
        .var("serviceName", service.clone())
        .handler(CoreHandler::new(Client::from_env))
        .handler(TracingHandler::new(move || {
            let service = factory_service.clone();
            async move {
                let client = Client::from_env()?;
                let mut config = TracingConfig::new(service).with_set_global(false);
                config.batch.scheduled_delay = Duration::from_millis(100);
                let handle = sdk::init_tracing(config)?;
                let query: Arc<dyn TraceQueryClient> = Arc::new(client.query().clone());
                Ok(TracingTarget {
                    handle: handle.tracing_handle().expect("standard pipeline").clone(),
                    query: Some(query),
                })
            }
        }));
    let report = runner.run().await.expect("scenarios load");
    println!("{report}");
    report.assert_success();
}
