// Taken over from sdk-kit 0.1.0 (header removed): the skeleton test used the demo service's
// routes; this one serves Jaeger responses.
//! Blocking client (feature `blocking`).
#![cfg(feature = "blocking")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use grow_jaeger as sdk;
use sdk::blocking::JaegerClient as Client;
use sdk::core::ErrorCode;
use sdk::tracing::{DependencyOptions, TraceSearchQuery, WaitForTraceOptions};
use sdk::{GetTraceOptions, JaegerConfig as Config, ListOperationsOptions};

const TID: &str = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";

fn otlp_body() -> String {
    format!(
        r#"{{"result":{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"svc"}}}}]}},"scopeSpans":[{{"spans":[{{"traceId":"{TID}","spanId":"55bc22d6caba571a","name":"op","kind":2,"startTimeUnixNano":"1000","endTimeUnixNano":"3000"}}]}}]}}]}}}}"#
    )
}

/// Minimal HTTP server on a thread that answers by path prefix (no async runtime involved).
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, ctype, body) = if target.starts_with("/api/v3/services") {
                (
                    "200",
                    "application/json",
                    r#"{"services":["svc","jaeger"]}"#.to_owned(),
                )
            } else if target.starts_with("/api/v3/operations") {
                (
                    "200",
                    "application/json",
                    r#"{"operations":[{"name":"op","spanKind":"server"}]}"#.to_owned(),
                )
            } else if target.starts_with("/api/v3/trace-summaries") {
                ("200", "application/json", r#"{"summaries":[]}"#.to_owned())
            } else if target.starts_with("/api/v3/traces") {
                ("200", "application/json", otlp_body())
            } else if target.starts_with("/api/dependencies") {
                ("200", "application/json", r#"{"data":[]}"#.to_owned())
            } else if target.starts_with("/sampling") {
                (
                    "200",
                    "application/json",
                    r#"{"strategyType":"PROBABILISTIC","probabilisticSampling":{"samplingRate":1}}"#
                        .to_owned(),
                )
            } else {
                (
                    "200",
                    "text/html",
                    r#"<script>JAEGER_VERSION = {"gitVersion":"v2.23.0"};</script>"#.to_owned(),
                )
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[test]
fn blocking_client() {
    let url = serve();
    let cfg = Config::builder()
        .url(url.clone())
        .sampling_url(format!("{url}/sampling"))
        .token("t-1234")
        .build()
        .unwrap();
    let c = Client::new(cfg).unwrap();
    assert!(c.health().unwrap().healthy);
    c.ping().unwrap();
    assert_eq!(c.server_version().unwrap(), "2.23.0");
    assert_eq!(c.sampling_strategy("svc").unwrap().sampling_rate, 1.0);
    assert_eq!(c.get_trace(TID).unwrap().unwrap().spans.len(), 1);
    assert!(c
        .get_trace_with(TID, GetTraceOptions::default())
        .unwrap()
        .is_some());
    assert_eq!(
        c.find_traces(&TraceSearchQuery::service("svc"))
            .unwrap()
            .len(),
        1
    );
    assert!(c
        .search_traces(&TraceSearchQuery::service("svc"))
        .unwrap()
        .is_empty());
    assert_eq!(c.list_services().unwrap(), vec!["jaeger", "svc"]);
    assert_eq!(
        c.list_operations("svc", ListOperationsOptions::default())
            .unwrap(),
        vec!["op"]
    );
    assert_eq!(
        c.get_operations("svc", ListOperationsOptions::default())
            .unwrap()
            .len(),
        1
    );
    assert!(c
        .get_dependencies(DependencyOptions::default())
        .unwrap()
        .is_empty());
    let opts = WaitForTraceOptions::default().interval(Duration::from_millis(10));
    assert_eq!(c.wait_for_trace(TID, opts).unwrap().trace_id, TID);
    assert!(!c.async_client().is_closed());
    assert_eq!(c.block_on(async { 7 }), 7);
    c.close();
    c.close();
    assert_eq!(c.ping().unwrap_err().code(), ErrorCode::Cancelled);
    let _ = Client::from_env();
    let bad = Config::builder()
        .ca_cert_path("/nonexistent")
        .build()
        .unwrap();
    assert_eq!(Client::new(bad).unwrap_err().code(), ErrorCode::Config);
}
