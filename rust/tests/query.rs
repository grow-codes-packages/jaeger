//! `JaegerQueryClient` against recorded Jaeger responses.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use grow_jaeger as sdk;
use sdk::tracing::{
    DependencyOptions, SpanKind, StatusCode, TraceQueryClient, TraceSearchQuery,
    WaitForTraceOptions,
};
use sdk::{ErrorCode, GetTraceOptions, ListOperationsOptions};
use serde_json::json;
use support::{client, fixture, mount, respond, text};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TID: &str = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1";

fn fixed_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_795_034_983)
}

#[tokio::test]
async fn get_trace_v3() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v3/traces/{TID}")))
        .and(query_param("startTime", "2026-11-18T20:49:43.000000000Z"))
        .and(query_param("endTime", "2026-11-25T20:49:43.000000000Z"))
        .respond_with(respond("getTrace.v3_ok"))
        .mount(&server)
        .await;
    let q = client(&server).query().clone().with_clock(fixed_now);
    let end = fixed_now() + Duration::from_secs(7 * 24 * 3600);
    // upper-case and 0x-prefixed ids are normalized
    let trace = q
        .get_trace_with(
            &TID.to_uppercase(),
            GetTraceOptions {
                end: Some(end),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("trace");
    assert_eq!(trace.trace_id, TID);
    assert_eq!(trace.spans.len(), 3);
    let root = &trace.spans[0];
    assert_eq!(root.name, "GET /orders");
    assert_eq!(root.parent_span_id, None);
    assert_eq!(root.service_name, "sdk-gw-86837b62");
    let server_span = trace
        .spans
        .iter()
        .find(|s| s.name == "handle /orders")
        .unwrap();
    assert_eq!(server_span.kind, SpanKind::Server);
    assert_eq!(server_span.status.code, StatusCode::Error);
    assert_eq!(server_span.status.message.as_deref(), Some("boom"));
    assert_eq!(server_span.attributes["int.attr"], json!(42));
    assert_eq!(server_span.attributes["double.attr"], json!(1.5));
    assert_eq!(server_span.attributes["bool.attr"], json!(true));
    assert_eq!(server_span.events[0].name, "cache.miss");
    assert_eq!(server_span.links[0].span_id, "00f067aa0ba902b7");
    assert_eq!(server_span.duration_micros, 1_000_000);
    let db = trace.spans.iter().find(|s| s.name == "db.query").unwrap();
    assert_eq!(db.kind, SpanKind::Internal);
    assert_eq!(db.status.code, StatusCode::Unset);
    assert_eq!(
        db.parent_span_id.as_deref(),
        Some(server_span.span_id.as_str())
    );

    // a v3 404 envelope means "no such trace"
    let other = "0000000000000000000000000000abcd";
    mount(
        &server,
        &format!("/api/v3/traces/{other}"),
        "getTrace.v3_not_found",
    )
    .await;
    assert!(q.get_trace("abcd").await.unwrap().is_none());
    let err = q.require_trace("abcd").await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);
    mount(&server, &format!("/api/v3/traces/{TID}"), "getTrace.v3_ok").await;
    assert!(q.require_trace(TID).await.is_ok());
}

#[tokio::test]
async fn get_trace_options_and_errors() {
    let server = MockServer::start().await;
    Mock::given(path(format!("/api/v3/traces/{TID}")))
        .and(query_param("rawTraces", "true"))
        .respond_with(respond("getTrace.v3_ok"))
        .mount(&server)
        .await;
    let bad = "00000000000000000000000000000bad";
    mount(
        &server,
        &format!("/api/v3/traces/{bad}"),
        "getTrace.v3_bad_time",
    )
    .await;
    let garbage = "0000000000000000000000000000beef";
    Mock::given(path(format!("/api/v3/traces/{garbage}")))
        .respond_with(text(200, "not json"))
        .mount(&server)
        .await;
    let other = "0000000000000000000000000000cafe";
    Mock::given(path(format!("/api/v3/traces/{other}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {}})))
        .mount(&server)
        .await;
    let q = client(&server).query().clone();
    let opts = GetTraceOptions {
        start: Some(fixed_now() - Duration::from_secs(60)),
        end: Some(fixed_now()),
        raw_traces: true,
    };
    assert!(q.get_trace_with(TID, opts).await.unwrap().is_some());
    let err = q.get_trace(bad).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Validation);
    assert_eq!(err.http_status(), Some(400));
    let err = q.get_trace(garbage).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unavailable);
    assert!(q.get_trace(other).await.unwrap().is_none());
    for id in ["", "xyz", "0000", &"a".repeat(33)] {
        assert_eq!(
            q.get_trace(id).await.unwrap_err().code(),
            ErrorCode::Validation
        );
    }
    let reversed = GetTraceOptions {
        start: Some(fixed_now()),
        end: Some(fixed_now() - Duration::from_secs(1)),
        raw_traces: false,
    };
    assert_eq!(
        q.get_trace_with(TID, reversed).await.unwrap_err().code(),
        ErrorCode::Validation
    );
}

#[tokio::test]
async fn get_trace_legacy_fallback() {
    let server = MockServer::start().await;
    // v3 route missing (plain-text 404), legacy present
    Mock::given(path_regex_v3())
        .respond_with(respond("api.unknown_route"))
        .mount(&server)
        .await;
    Mock::given(path(format!("/api/traces/{TID}")))
        .and(query_param("raw", "true"))
        .respond_with(respond("getTraceLegacy.ok"))
        .mount(&server)
        .await;
    let missing = "0000000000000000000000000000abcd";
    mount(
        &server,
        &format!("/api/traces/{missing}"),
        "getTraceLegacy.not_found",
    )
    .await;
    let q = client(&server).query().clone();
    let trace = q
        .get_trace_with(
            TID,
            GetTraceOptions {
                raw_traces: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("legacy trace");
    assert_eq!(trace.spans.len(), 3);
    let root = &trace.spans[0];
    assert_eq!(root.kind, SpanKind::Client);
    assert_eq!(root.status.code, StatusCode::Ok);
    assert!(!root.attributes.contains_key("otel.scope.name"));
    let srv = trace
        .spans
        .iter()
        .find(|s| s.name == "handle /orders")
        .unwrap();
    assert_eq!(srv.parent_span_id.as_deref(), Some(root.span_id.as_str()));
    assert_eq!(srv.links.len(), 1);
    assert_eq!(srv.status.code, StatusCode::Error);
    assert_eq!(srv.attributes["double.attr"], json!(1.5));
    assert_eq!(srv.attributes["int.attr"], json!(42));
    assert_eq!(srv.events[0].name, "cache.miss");
    assert_eq!(srv.service_name, "sdk-wk-86837b62");
    assert!(q.get_trace(missing).await.unwrap().is_none());

    // both routes missing → UNSUPPORTED
    let none = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(text(404, "404 page not found\n"))
        .mount(&none)
        .await;
    let err = client(&none).query().get_trace(TID).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unsupported);
}

fn path_regex_v3() -> wiremock::matchers::PathRegexMatcher {
    wiremock::matchers::path_regex("^/api/v3/")
}

#[tokio::test]
async fn find_and_search_traces() {
    let server = MockServer::start().await;
    Mock::given(path("/api/v3/traces"))
        .and(query_param("query.serviceName", "sdk-wk-86837b62"))
        .and(query_param("query.operationName", "handle /orders"))
        .and(query_param(
            "query.attributes",
            r#"{"order.id":"86837b62"}"#,
        ))
        .and(query_param("query.durationMin", "1000us"))
        .and(query_param("query.durationMax", "5000000us"))
        .and(query_param("query.searchDepth", "5"))
        .and(query_param(
            "query.startTimeMin",
            "2026-11-18T19:49:43.000000000Z",
        ))
        .and(query_param(
            "query.startTimeMax",
            "2026-11-18T20:49:43.000000000Z",
        ))
        .respond_with(respond("findTraces.by_service"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/traces"))
        .and(query_param("query.serviceName", "nobody"))
        .respond_with(respond("findTraces.no_match"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/traces"))
        .and(query_param("query.serviceName", "bad"))
        .respond_with(respond("findTraces.bad_attributes"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/trace-summaries"))
        .and(query_param("query.serviceName", "sdk-gw-86837b62"))
        .and(query_param("query.searchDepth", "20"))
        .respond_with(respond("findTraceSummaries.ok"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/trace-summaries"))
        .and(query_param("query.serviceName", "nobody"))
        .respond_with(respond("findTraceSummaries.empty"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/trace-summaries"))
        .and(query_param("query.serviceName", "envelope"))
        .respond_with(respond("findTraces.no_match"))
        .mount(&server)
        .await;
    let q = client(&server).query().clone().with_clock(fixed_now);
    let query = TraceSearchQuery {
        min_duration: Some(Duration::from_millis(1)),
        max_duration: Some(Duration::from_secs(5)),
        ..TraceSearchQuery::service("sdk-wk-86837b62")
            .operation("handle /orders")
            .tag("order.id", "86837b62")
            .limit(5)
    };
    let traces = q.find_traces(&query).await.unwrap();
    assert!(!traces.is_empty());
    assert!(traces
        .iter()
        .all(|t| t.spans.iter().any(|s| s.service_name == "sdk-wk-86837b62")));
    assert!(q
        .find_traces(&TraceSearchQuery::service("nobody"))
        .await
        .unwrap()
        .is_empty());
    let err = q
        .find_traces(&TraceSearchQuery::service("bad"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Validation);

    let summaries = q
        .search_traces(&TraceSearchQuery::service("sdk-gw-86837b62"))
        .await
        .unwrap();
    let expected = &fixture("findTraceSummaries.ok").json["summaries"][0];
    assert_eq!(summaries[0].trace_id, expected["traceId"].as_str().unwrap());
    assert_eq!(
        summaries[0].root_name.as_deref(),
        expected["rootOperationName"].as_str()
    );
    assert!(summaries[0].span_count >= 1);
    assert!(summaries[0].duration_micros > 0);
    assert!(q
        .search_traces(&TraceSearchQuery::service("nobody"))
        .await
        .unwrap()
        .is_empty());
    assert!(q
        .search_traces(&TraceSearchQuery::service("envelope"))
        .await
        .unwrap()
        .is_empty());

    // validation before any request
    let bad = TraceSearchQuery {
        min_duration: Some(Duration::from_secs(2)),
        max_duration: Some(Duration::from_secs(1)),
        ..Default::default()
    };
    assert_eq!(
        q.find_traces(&bad).await.unwrap_err().code(),
        ErrorCode::Validation
    );
    let reversed = TraceSearchQuery {
        start: Some(fixed_now()),
        end: Some(fixed_now() - Duration::from_secs(1)),
        ..Default::default()
    };
    assert_eq!(
        q.search_traces(&reversed).await.unwrap_err().code(),
        ErrorCode::Validation
    );
}

#[tokio::test]
async fn search_defaults_and_routes_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |req: &Request| {
            let q: BTreeMap<String, String> = req.url.query_pairs().into_owned().collect();
            if !req.url.path().contains("trace") {
                return text(404, "404 page not found\n");
            }
            // limit 0 → default 20, empty service/operation omitted, zero durations omitted
            assert_eq!(q["query.searchDepth"], "20");
            assert!(!q.contains_key("query.serviceName"));
            assert!(!q.contains_key("query.operationName"));
            assert!(!q.contains_key("query.durationMin"));
            text(404, "404 page not found\n")
        })
        .mount(&server)
        .await;
    let q = client(&server).query().clone();
    let query = TraceSearchQuery {
        service: Some(String::new()),
        operation: Some(String::new()),
        min_duration: Some(Duration::ZERO),
        limit: 0,
        ..Default::default()
    };
    assert_eq!(
        q.find_traces(&query).await.unwrap_err().code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        q.search_traces(&query).await.unwrap_err().code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        q.list_services().await.unwrap_err().code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        q.list_operations("svc", ListOperationsOptions::default())
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        q.get_dependencies(DependencyOptions::default())
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
}

#[tokio::test]
async fn services_and_operations() {
    let server = MockServer::start().await;
    mount(&server, "/api/v3/services", "listServices.ok").await;
    Mock::given(path("/api/v3/operations"))
        .and(query_param("service", "sdk-wk-86837b62"))
        .and(query_param("spanKind", "server"))
        .respond_with(respond("listOperations.span_kind_server"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/operations"))
        .and(query_param("service", "sdk-wk-86837b62"))
        .respond_with(respond("listOperations.ok"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/operations"))
        .and(query_param("service", "dup"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"operations": [
                {"name": "b", "spanKind": "client"},
                {"name": "a"},
                {"name": "b", "spanKind": "server"}
            ]})),
        )
        .mount(&server)
        .await;
    mount(
        &server,
        "/api/v3/operations",
        "listOperations.unknown_service",
    )
    .await;
    let q = client(&server).query().clone();
    let services = q.list_services().await.unwrap();
    assert!(services.contains(&"jaeger".to_owned()));
    let mut sorted = services.clone();
    sorted.sort();
    assert_eq!(services, sorted);

    let ops = q
        .get_operations("sdk-wk-86837b62", ListOperationsOptions::default())
        .await
        .unwrap();
    assert!(ops
        .iter()
        .any(|o| o.name == "handle /orders" && o.span_kind == SpanKind::Server));
    let server_ops = q
        .list_operations(
            "sdk-wk-86837b62",
            ListOperationsOptions::span_kind(SpanKind::Server),
        )
        .await
        .unwrap();
    assert_eq!(server_ops, vec!["handle /orders".to_owned()]);
    let dup = q
        .list_operations("dup", ListOperationsOptions::default())
        .await
        .unwrap();
    assert_eq!(dup, vec!["a".to_owned(), "b".to_owned()]);
    let kinds = q
        .get_operations("dup", ListOperationsOptions::default())
        .await
        .unwrap();
    assert_eq!(kinds[0].span_kind, SpanKind::Unspecified);
    assert!(q
        .list_operations("unknown", ListOperationsOptions::default())
        .await
        .unwrap()
        .is_empty());
    for kind in [
        SpanKind::Client,
        SpanKind::Producer,
        SpanKind::Consumer,
        SpanKind::Internal,
    ] {
        let _ = q
            .get_operations("unknown", ListOperationsOptions::span_kind(kind))
            .await
            .unwrap();
    }
    assert_eq!(
        q.get_operations("  ", ListOperationsOptions::default())
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Validation
    );
    assert_eq!(
        q.get_operations("x", ListOperationsOptions::span_kind(SpanKind::Unspecified))
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Validation
    );
    let as_trait: Arc<dyn TraceQueryClient> = Arc::new(q.clone());
    assert!(as_trait
        .list_services()
        .await
        .unwrap()
        .contains(&"jaeger".to_owned()));
    assert!(!as_trait
        .list_operations("sdk-wk-86837b62")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn dependencies() {
    let server = MockServer::start().await;
    Mock::given(path("/api/dependencies"))
        .and(query_param("endTs", "1795034983000"))
        .and(query_param("lookback", "3600000"))
        .respond_with(respond("getDependencies.ok"))
        .mount(&server)
        .await;
    Mock::given(path("/api/dependencies"))
        .and(query_param("lookback", "86400000"))
        .respond_with(respond("getDependencies.outside_window"))
        .mount(&server)
        .await;
    Mock::given(path("/api/dependencies"))
        .and(query_param("lookback", "1"))
        .respond_with(respond("getDependencies.bad_lookback"))
        .mount(&server)
        .await;
    Mock::given(path("/api/dependencies"))
        .and(query_param("lookback", "2"))
        .respond_with(respond("getTraceLegacy.not_found"))
        .mount(&server)
        .await;
    let q = client(&server).query().clone().with_clock(fixed_now);
    let links = q
        .get_dependencies(DependencyOptions {
            start: Some(fixed_now() - Duration::from_secs(3600)),
            end: Some(fixed_now()),
        })
        .await
        .unwrap();
    let expected = &fixture("getDependencies.ok").json["data"][0];
    assert_eq!(links[0].parent, expected["parent"].as_str().unwrap());
    assert_eq!(links[0].child, expected["child"].as_str().unwrap());
    assert!(links[0].call_count >= 1);
    let at = q
        .get_dependencies_at(Some(fixed_now()), Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    assert_eq!(at, links);
    // defaults: 24 h window
    assert!(q
        .get_dependencies(DependencyOptions::default())
        .await
        .unwrap()
        .is_empty());
    assert!(q
        .get_dependencies_at(None, Some(Duration::ZERO))
        .await
        .unwrap()
        .is_empty());
    let as_trait: &dyn TraceQueryClient = &q;
    assert!(as_trait
        .get_dependencies(DependencyOptions::default())
        .await
        .unwrap()
        .is_empty());
    let err = q
        .get_dependencies_at(None, Some(Duration::from_millis(1)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Validation);
    assert!(q
        .get_dependencies_at(None, Some(Duration::from_millis(2)))
        .await
        .unwrap()
        .is_empty());
    let err = q
        .get_dependencies(DependencyOptions {
            start: Some(fixed_now()),
            end: Some(fixed_now() - Duration::from_secs(1)),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Validation);
}

#[tokio::test]
async fn status_mapping_retries_and_token() {
    let server = MockServer::start().await;
    Mock::given(path("/api/v3/services"))
        .and(header("authorization", "Bearer q-token-1"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/services"))
        .and(header("authorization", "Bearer q-token-1"))
        .respond_with(respond("listServices.ok"))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/services"))
        .respond_with(ResponseTemplate::new(403).set_body_raw(
            r#"{"error":{"httpCode":403,"message":"denied"}}"#,
            "text/plain",
        ))
        .mount(&server)
        .await;
    Mock::given(path("/api/v3/operations"))
        .respond_with(ResponseTemplate::new(501).set_body_raw(
            r#"{"error":{"httpCode":501,"message":"unsupported"}}"#,
            "text/plain",
        ))
        .mount(&server)
        .await;
    let authed = sdk::JaegerClient::new(
        sdk::JaegerConfig::builder()
            .url(server.uri())
            .token("q-token-1")
            .retry(support::fast_retry())
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(authed
        .query()
        .list_services()
        .await
        .unwrap()
        .contains(&"jaeger".to_owned()));
    assert!(!format!("{:?}", authed.query()).contains("q-token-1"));
    let anon = client(&server);
    let err = anon.query().list_services().await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::PermissionDenied);
    assert!(err.message().contains("denied"));
    let err = anon
        .query()
        .list_operations("svc", ListOperationsOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), Some(501));
    let bad_json = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(text(200, "<html>"))
        .mount(&bad_json)
        .await;
    let c = client(&bad_json);
    assert_eq!(
        c.query().list_services().await.unwrap_err().code(),
        ErrorCode::Unavailable
    );
    assert_eq!(
        c.query()
            .get_operations("s", ListOperationsOptions::default())
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unavailable
    );
    assert_eq!(
        c.query()
            .search_traces(&TraceSearchQuery::default())
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Unavailable
    );
}

#[tokio::test]
async fn wait_for_trace_polls() {
    let server = MockServer::start().await;
    Mock::given(path(format!("/api/v3/traces/{TID}")))
        .respond_with(respond("getTrace.v3_not_found"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    mount(&server, &format!("/api/v3/traces/{TID}"), "getTrace.v3_ok").await;
    let q = client(&server).query().clone();
    let opts = WaitForTraceOptions::default()
        .interval(Duration::from_millis(10))
        .timeout(Duration::from_secs(5))
        .min_spans(3);
    let trace = q.wait_for_trace(TID, opts).await.unwrap();
    assert_eq!(trace.spans.len(), 3);
    let err = q
        .wait_for_trace(TID, opts.min_spans(4).timeout(Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Timeout);
}

#[tokio::test]
async fn base_path_is_kept() {
    let server = MockServer::start().await;
    Mock::given(path("/jaeger/api/v3/services"))
        .respond_with(respond("listServices.ok"))
        .mount(&server)
        .await;
    let c = support::client_at(&format!("{}/jaeger", server.uri()));
    assert!(!c.query().list_services().await.unwrap().is_empty());
    let transport = sdk::core::HttpTransport::new(c.config().common()).unwrap();
    let standalone = sdk::JaegerQueryClient::new(Arc::new(transport), Some("t-1".into()));
    assert!(!standalone.list_services().await.unwrap().is_empty());
    assert!(standalone
        .transport()
        .config()
        .url()
        .path()
        .starts_with("/jaeger"));
}
