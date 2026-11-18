//! Helpers for the Jaeger service tests: recorded fixtures (`tests/fixtures/jaeger`, real
//! responses of `jaegertracing/jaeger:2.23.0`) mounted on a wiremock server.
#![allow(dead_code)]

use std::time::Duration;

use grow_jaeger as sdk;
use sdk::core::{RandomSource, RetryPolicy};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A recorded fixture.
pub struct Fixture {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub json: Value,
}

/// Loads `tests/fixtures/jaeger/<name>.json`.
pub fn fixture(name: &str) -> Fixture {
    let path = format!(
        "{}/tests/fixtures/jaeger/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let resp = &doc["response"];
    let status = match &resp["status"] {
        Value::Number(n) => n.as_u64().unwrap() as u16,
        Value::String(s) => s.parse().unwrap(),
        _ => 200,
    };
    let content_type = resp["headers"]
        .get("content-type")
        .and_then(Value::as_str)
        .unwrap_or("application/json")
        .to_owned();
    let json = resp.get("body").cloned().unwrap_or(Value::Null);
    let body = match &json {
        Value::String(s) => s.clone().into_bytes(),
        Value::Null => resp
            .get("bodyExcerpt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
        other => serde_json::to_vec(other).unwrap(),
    };
    Fixture {
        status,
        content_type,
        body,
        json,
    }
}

/// The response of a fixture.
pub fn respond(name: &str) -> ResponseTemplate {
    let f = fixture(name);
    ResponseTemplate::new(f.status).set_body_raw(f.body, &f.content_type)
}

/// Mounts a fixture for `GET <p>`.
pub async fn mount(server: &MockServer, p: &str, name: &str) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(respond(name))
        .mount(server)
        .await;
}

/// Plain-text response.
pub fn text(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_raw(body.as_bytes().to_vec(), "text/plain; charset=utf-8")
}

/// Retry policy with short deterministic backoffs.
pub fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
        ..RetryPolicy::default()
    }
    .with_random(RandomSource::fixed(0.0))
}

/// Client for a mock server.
pub fn client(server: &MockServer) -> sdk::JaegerClient {
    client_at(&server.uri())
}

/// Client for a URL.
pub fn client_at(url: &str) -> sdk::JaegerClient {
    sdk::JaegerClient::new(
        sdk::JaegerConfig::builder()
            .url(url)
            .retry(fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap()
}

/// A port with nothing listening.
pub fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
