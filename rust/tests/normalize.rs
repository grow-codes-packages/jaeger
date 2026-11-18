//! Wire-format conversions (`grow_jaeger::normalize`).

use grow_jaeger::normalize::{
    any_value, dependencies_from_body, key_values, legacy_to_traces, nanos_to_micros,
    otlp_span_to_span, otlp_to_traces, summaries_from_body,
};
use grow_jaeger::tracing::{SpanKind, StatusCode};
use serde_json::json;

#[test]
fn any_values() {
    assert_eq!(any_value(&json!({"stringValue": "s"})), Some(json!("s")));
    assert_eq!(any_value(&json!({"boolValue": true})), Some(json!(true)));
    assert_eq!(any_value(&json!({"boolValue": "true"})), Some(json!(true)));
    assert_eq!(any_value(&json!({"boolValue": 1})), Some(json!(false)));
    assert_eq!(any_value(&json!({"intValue": "42"})), Some(json!(42)));
    assert_eq!(any_value(&json!({"intValue": 7})), Some(json!(7)));
    assert_eq!(any_value(&json!({"intValue": "x"})), Some(json!("x")));
    assert_eq!(any_value(&json!({"intValue": null})), Some(json!("")));
    assert_eq!(any_value(&json!({"doubleValue": 1.5})), Some(json!(1.5)));
    assert_eq!(any_value(&json!({"doubleValue": "2.5"})), Some(json!(2.5)));
    assert_eq!(
        any_value(&json!({"doubleValue": "NaN"})),
        Some(json!("NaN"))
    );
    assert_eq!(any_value(&json!({"doubleValue": true})), Some(json!("")));
    assert_eq!(
        any_value(&json!({"bytesValue": "AQI="})),
        Some(json!("AQI="))
    );
    assert_eq!(
        any_value(&json!({"arrayValue": {"values": [{"stringValue": "a"}, {}]}})),
        Some(json!(["a", null]))
    );
    assert_eq!(any_value(&json!({"arrayValue": {}})), Some(json!([])));
    assert_eq!(
        any_value(&json!({"kvlistValue": {"values": [{"key": "k", "value": {"intValue": "1"}}]}})),
        Some(json!({"k": 1}))
    );
    assert_eq!(any_value(&json!({})), None);
    assert_eq!(any_value(&json!("x")), None);
    let kv = key_values(
        &json!([{"key": "a"}, {"value": {}}, {"key": "b", "value": {"stringValue": "v"}}]),
    );
    assert_eq!(kv["a"], json!(null));
    assert_eq!(kv["b"], json!("v"));
    assert_eq!(kv.len(), 2);
    assert_eq!(nanos_to_micros(&json!("1500")), 1);
    assert_eq!(nanos_to_micros(&json!(3000)), 3);
    assert_eq!(nanos_to_micros(&json!(3000.0)), 3);
    assert_eq!(nanos_to_micros(&json!(-1.0)), 0);
    assert_eq!(nanos_to_micros(&json!(null)), 0);
}

#[test]
fn otlp_spans() {
    let span = otlp_span_to_span(
        &json!({
            "traceId": "ABCD", "spanId": "1", "parentSpanId": "",
            "name": "n", "kind": "SPAN_KIND_PRODUCER",
            "startTimeUnixNano": "5000", "endTimeUnixNano": "2000",
            "status": {"code": "STATUS_CODE_OK", "message": ""}
        }),
        "svc",
    );
    assert_eq!(span.trace_id, "0000000000000000000000000000abcd");
    assert_eq!(span.span_id, "0000000000000001");
    assert_eq!(span.parent_span_id, None);
    assert_eq!(span.kind, SpanKind::Producer);
    assert_eq!(span.duration_micros, 0);
    assert_eq!(span.status.code, StatusCode::Ok);
    assert_eq!(span.status.message, None);
    let span = otlp_span_to_span(
        &json!({"traceId": "not-hex", "kind": null, "status": {"code": 9}}),
        "",
    );
    assert_eq!(span.trace_id, "not-hex");
    assert_eq!(span.kind, SpanKind::Unspecified);
    assert_eq!(span.status.code, StatusCode::Unset);
    let span = otlp_span_to_span(
        &json!({"kind": 5, "status": {"code": "STATUS_CODE_ERROR"}}),
        "",
    );
    assert_eq!(span.kind, SpanKind::Consumer);
    assert_eq!(span.status.code, StatusCode::Error);

    // bare TracesData, two traces in first-seen order, resource without service.name
    let traces = otlp_to_traces(&json!({"resourceSpans": [
        {"resource": {}, "scopeSpans": [{"spans": [
            {"traceId": "b", "spanId": "2", "startTimeUnixNano": "2000"},
            {"traceId": "a", "spanId": "1"},
            {"traceId": "b", "spanId": "1", "startTimeUnixNano": "1000"}
        ]}]}
    ]}));
    assert_eq!(traces.len(), 2);
    assert!(traces[0].trace_id.ends_with('b'));
    assert_eq!(traces[0].spans[0].span_id, "0000000000000001");
    assert_eq!(traces[0].spans[0].service_name, "");
    assert!(otlp_to_traces(&json!({})).is_empty());
}

#[test]
fn legacy_traces() {
    let body = json!({"data": [{
        "traceID": "a",
        "processes": {"p1": {"serviceName": "svc"}},
        "spans": [
            {"traceID": "a", "spanID": "2", "operationName": "child",
             "references": [
                {"refType": "FOLLOWS_FROM", "traceID": "a", "spanID": "1"},
                {"refType": "FOLLOWS_FROM", "traceID": "b", "spanID": "9"}
             ],
             "startTime": 10, "duration": 5, "processID": "p1",
             "tags": [
                {"key": "span.kind", "type": "string", "value": "producer"},
                {"key": "error", "type": "bool", "value": true},
                {"key": "otel.status_description", "type": "string", "value": "bad"},
                {"key": "big", "type": "float64", "value": 3}
             ],
             "logs": [{"timestamp": 11, "fields": [
                {"key": "event", "type": "string", "value": "e1"},
                {"key": "event", "type": "int64", "value": 1},
                {"key": "k", "type": "string", "value": "v"}
             ]}]},
            {"traceID": "a", "spanID": "3", "references": [{"refType": "CHILD_OF", "traceID": "a", "spanID": ""}],
             "tags": [{"key": "span.kind", "type": "string", "value": "weird"},
                      {"key": "otel.status_code", "type": "string", "value": "UNSET"},
                      {"key": "error", "type": "string", "value": "true"}], "processID": "nope"},
            {"traceID": "a", "spanID": "4",
             "tags": [{"key": "span.kind", "type": "string", "value": "INTERNAL"},
                      {"key": "otel.status_code", "type": "string", "value": "OK"},
                      {"key": "error", "type": "bool", "value": true}]},
            {"traceID": "a", "spanID": "5", "tags": [
                {"key": "span.kind", "type": "string", "value": "consumer"},
                {"key": "otel.status_code", "type": "string", "value": "ERROR"}]},
            {"traceID": "a", "spanID": "6", "tags": [{"key": "span.kind", "value": "server"}]},
            {"traceID": "a", "spanID": "7", "tags": [{"key": "span.kind", "value": "client"}]}
        ]
    }]});
    let traces = legacy_to_traces(&body);
    let spans = &traces[0].spans;
    let child = spans.iter().find(|s| s.name == "child").unwrap();
    assert_eq!(child.parent_span_id.as_deref(), Some("0000000000000001"));
    assert_eq!(child.links.len(), 1);
    assert_eq!(child.kind, SpanKind::Producer);
    assert_eq!(child.status.code, StatusCode::Error);
    assert_eq!(child.status.message.as_deref(), Some("bad"));
    assert_eq!(child.attributes["big"], json!(3.0));
    assert_eq!(child.events[0].name, "e1");
    assert_eq!(child.events[0].attributes["event"], json!(1));
    assert_eq!(child.service_name, "svc");
    assert_eq!(child.end_micros, 15);
    let s3 = spans.iter().find(|s| s.span_id.ends_with('3')).unwrap();
    assert_eq!(s3.parent_span_id, None);
    assert_eq!(s3.kind, SpanKind::Unspecified);
    assert_eq!(s3.status.code, StatusCode::Error);
    assert_eq!(s3.service_name, "");
    let s4 = spans.iter().find(|s| s.span_id.ends_with('4')).unwrap();
    assert_eq!(
        (s4.kind, s4.status.code),
        (SpanKind::Internal, StatusCode::Ok)
    );
    let s5 = spans.iter().find(|s| s.span_id.ends_with('5')).unwrap();
    assert_eq!(
        (s5.kind, s5.status.code),
        (SpanKind::Consumer, StatusCode::Error)
    );
    assert!(spans.iter().any(|s| s.kind == SpanKind::Server));
    assert!(spans.iter().any(|s| s.kind == SpanKind::Client));
    assert!(legacy_to_traces(&json!({"data": null})).is_empty());
}

#[test]
fn summaries_and_dependencies() {
    let s = summaries_from_body(&json!({"summaries": [
        {"traceId": "A", "rootServiceName": "svc", "rootOperationName": "",
         "minStartTimeUnixNano": "1000000", "maxEndTimeUnixNano": "3000000", "spanCount": 4}
    ]}));
    assert_eq!(s[0].trace_id, "0000000000000000000000000000000a");
    assert_eq!(s[0].root_service_name.as_deref(), Some("svc"));
    assert_eq!(s[0].root_name, None);
    assert_eq!(
        (s[0].start_micros, s[0].duration_micros, s[0].span_count),
        (1000, 2000, 4)
    );
    let d = dependencies_from_body(
        &json!({"data": [{"parent": "a", "child": "b", "callCount": 3}, {}]}),
    );
    assert_eq!(
        (d[0].parent.as_str(), d[0].child.as_str(), d[0].call_count),
        ("a", "b", 3)
    );
    assert_eq!(d[1].call_count, 0);
}
