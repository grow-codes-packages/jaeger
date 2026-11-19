// Service code (Jaeger): converts Jaeger query responses into the normalized trace model.
//! Conversions from Jaeger's wire formats to the normalized model of the tracing category:
//! OTLP/JSON `TracesData` (the v3 API), the legacy UI model (`/api/traces/{id}`), trace
//! summaries (`/api/v3/trace-summaries`) and dependency links (`/api/dependencies`).
//!
//! The functions are lenient: unknown fields are ignored, missing numbers count as 0 and ids are
//! lower-cased and left-padded. They never fail on well-formed JSON.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

use crate::tracing::{
    normalize_span_id, normalize_trace_id, AttrValue, Attributes, DependencyLink, Span, SpanEvent,
    SpanKind, SpanLink, SpanStatus, StatusCode, Trace, TraceSummary,
};

/// Converts a decimal nanosecond value (string or number) to microseconds; 0 when invalid.
pub fn nanos_to_micros(value: &Value) -> u64 {
    as_u64(value) / 1000
}

fn as_u64(value: &Value) -> u64 {
    match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

/// Converts an OTLP/JSON `AnyValue` (`{"stringValue": …}`, `{"intValue": "42"}`, …) to an
/// attribute value; `None` for an empty value.
pub fn any_value(value: &Value) -> Option<AttrValue> {
    let obj = value.as_object()?;
    if let Some(v) = obj.get("stringValue") {
        return Some(Value::String(v.as_str().unwrap_or_default().to_owned()));
    }
    if let Some(v) = obj.get("boolValue") {
        return Some(Value::Bool(match v {
            Value::Bool(b) => *b,
            Value::String(s) => s == "true",
            _ => false,
        }));
    }
    if let Some(v) = obj.get("intValue") {
        let n = match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        return Some(match n {
            Some(i) => Value::Number(i.into()),
            None => Value::String(v.as_str().unwrap_or_default().to_owned()),
        });
    }
    if let Some(v) = obj.get("doubleValue") {
        let f = match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        return Some(
            f.and_then(Number::from_f64)
                .map(Value::Number)
                .unwrap_or_else(|| Value::String(v.as_str().unwrap_or_default().to_owned())),
        );
    }
    if let Some(v) = obj.get("bytesValue") {
        return Some(Value::String(v.as_str().unwrap_or_default().to_owned()));
    }
    if let Some(v) = obj.get("arrayValue") {
        let items = v
            .get("values")
            .and_then(Value::as_array)
            .map(|vals| {
                vals.iter()
                    .map(|e| any_value(e).unwrap_or(Value::Null))
                    .collect()
            })
            .unwrap_or_default();
        return Some(Value::Array(items));
    }
    if let Some(v) = obj.get("kvlistValue") {
        let map: Map<String, Value> = key_values(v.get("values").unwrap_or(&Value::Null))
            .into_iter()
            .collect();
        return Some(Value::Object(map));
    }
    None
}

/// Converts an OTLP/JSON `KeyValue` list to an attribute map (later keys win).
pub fn key_values(values: &Value) -> Attributes {
    let mut out = Attributes::new();
    for kv in values.as_array().into_iter().flatten() {
        let Some(key) = kv.get("key").and_then(Value::as_str) else {
            continue;
        };
        let value = kv.get("value").and_then(any_value).unwrap_or(Value::Null);
        out.insert(key.to_owned(), value);
    }
    out
}

/// Lower-cases and left-pads a hex id; ids that are not hex are returned lower-cased as is (the
/// server is trusted to send valid ids). Empty stays empty.
fn loose_id(value: Option<&Value>, trace: bool) -> String {
    let raw = value.and_then(Value::as_str).unwrap_or_default();
    if raw.is_empty() {
        return String::new();
    }
    let normalized = if trace {
        normalize_trace_id(raw)
    } else {
        normalize_span_id(raw)
    };
    normalized.unwrap_or_else(|_| raw.to_ascii_lowercase())
}

fn otlp_kind(value: Option<&Value>) -> SpanKind {
    match value {
        Some(Value::Number(n)) => SpanKind::parse(&n.to_string()),
        Some(Value::String(s)) => SpanKind::parse(s),
        _ => SpanKind::Unspecified,
    }
}

fn otlp_status(value: Option<&Value>) -> SpanStatus {
    let code = match value.and_then(|s| s.get("code")) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let code = match code.as_str() {
        "1" | "STATUS_CODE_OK" => StatusCode::Ok,
        "2" | "STATUS_CODE_ERROR" => StatusCode::Error,
        _ => StatusCode::Unset,
    };
    let message = value
        .and_then(|s| s.get("message"))
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    SpanStatus { code, message }
}

/// Converts one OTLP/JSON span; `service_name` comes from the enclosing resource.
pub fn otlp_span_to_span(span: &Value, service_name: &str) -> Span {
    let start = nanos_to_micros(span.get("startTimeUnixNano").unwrap_or(&Value::Null));
    let end = nanos_to_micros(span.get("endTimeUnixNano").unwrap_or(&Value::Null));
    let parent = loose_id(span.get("parentSpanId"), false);
    let events = span
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| SpanEvent {
            name: e
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            time_micros: nanos_to_micros(e.get("timeUnixNano").unwrap_or(&Value::Null)),
            attributes: key_values(e.get("attributes").unwrap_or(&Value::Null)),
        })
        .collect();
    let links = span
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|l| SpanLink {
            trace_id: loose_id(l.get("traceId"), true),
            span_id: loose_id(l.get("spanId"), false),
            attributes: key_values(l.get("attributes").unwrap_or(&Value::Null)),
        })
        .collect();
    Span {
        trace_id: loose_id(span.get("traceId"), true),
        span_id: loose_id(span.get("spanId"), false),
        parent_span_id: (!parent.is_empty()).then_some(parent),
        name: span
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: otlp_kind(span.get("kind")),
        service_name: service_name.to_owned(),
        start_micros: start,
        end_micros: end,
        duration_micros: end.saturating_sub(start),
        status: otlp_status(span.get("status")),
        attributes: key_values(span.get("attributes").unwrap_or(&Value::Null)),
        events,
        links,
    }
}

/// Collects spans per trace id in first-seen order.
#[derive(Default)]
struct Grouper {
    order: Vec<String>,
    spans: BTreeMap<String, Vec<Span>>,
}

impl Grouper {
    fn add(&mut self, span: Span) {
        if !self.spans.contains_key(&span.trace_id) {
            self.order.push(span.trace_id.clone());
        }
        self.spans
            .entry(span.trace_id.clone())
            .or_default()
            .push(span);
    }

    fn traces(mut self) -> Vec<Trace> {
        self.order
            .iter()
            .map(|id| Trace::new(id.clone(), self.spans.remove(id).unwrap_or_default()))
            .collect()
    }
}

/// Groups the spans of an OTLP/JSON `TracesData` body (`{"result": {"resourceSpans": …}}` or
/// the bare `{"resourceSpans": …}`) into traces, in first-seen order with sorted spans.
pub fn otlp_to_traces(data: &Value) -> Vec<Trace> {
    let root = data.get("result").unwrap_or(data);
    let mut grouper = Grouper::default();
    for rs in root
        .get("resourceSpans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let resource = key_values(
            rs.get("resource")
                .and_then(|r| r.get("attributes"))
                .unwrap_or(&Value::Null),
        );
        let service = resource
            .get("service.name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        for ss in rs
            .get("scopeSpans")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for span in ss
                .get("spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                grouper.add(otlp_span_to_span(span, &service));
            }
        }
    }
    grouper.traces()
}

/// Converts a typed legacy tag value (`{"key","type","value"}`).
fn legacy_value(kv: &Value) -> AttrValue {
    let value = kv.get("value").cloned().unwrap_or(Value::Null);
    let ty = kv.get("type").and_then(Value::as_str).unwrap_or_default();
    match (&value, ty) {
        (Value::Number(n), "float64") => n
            .as_f64()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(value.clone()),
        _ => value,
    }
}

fn legacy_kind(kind: &str) -> SpanKind {
    match kind.to_ascii_lowercase().as_str() {
        "server" => SpanKind::Server,
        "client" => SpanKind::Client,
        "producer" => SpanKind::Producer,
        "consumer" => SpanKind::Consumer,
        "internal" => SpanKind::Internal,
        _ => SpanKind::Unspecified,
    }
}

fn legacy_span(span: &Value, service_name: &str) -> Span {
    let trace_id = loose_id(span.get("traceID"), true);
    let start = as_u64(span.get("startTime").unwrap_or(&Value::Null));
    let duration = as_u64(span.get("duration").unwrap_or(&Value::Null));
    let refs: Vec<&Value> = span
        .get("references")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let same_trace = |r: &Value| loose_id(r.get("traceID"), true) == trace_id;
    let parent = refs
        .iter()
        .position(|r| r.get("refType").and_then(Value::as_str) == Some("CHILD_OF") && same_trace(r))
        .or_else(|| refs.iter().position(|r| same_trace(r)));
    let mut out = Span {
        trace_id: trace_id.clone(),
        span_id: loose_id(span.get("spanID"), false),
        parent_span_id: None,
        name: span
            .get("operationName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: SpanKind::Unspecified,
        service_name: service_name.to_owned(),
        start_micros: start,
        end_micros: start + duration,
        duration_micros: duration,
        status: SpanStatus::default(),
        attributes: Attributes::new(),
        events: Vec::new(),
        links: Vec::new(),
    };
    for (i, r) in refs.iter().enumerate() {
        let span_id = loose_id(r.get("spanID"), false);
        if Some(i) == parent {
            out.parent_span_id = (!span_id.is_empty()).then_some(span_id);
        } else {
            out.links.push(SpanLink {
                trace_id: loose_id(r.get("traceID"), true),
                span_id,
                attributes: Attributes::new(),
            });
        }
    }
    let mut error_tag = false;
    for kv in span
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = kv.get("key").and_then(Value::as_str).unwrap_or_default();
        let value = legacy_value(kv);
        match key {
            "span.kind" => out.kind = legacy_kind(value.as_str().unwrap_or_default()),
            "otel.status_code" => match value.as_str() {
                Some("OK") => out.status.code = StatusCode::Ok,
                Some("ERROR") => out.status.code = StatusCode::Error,
                _ => {}
            },
            "otel.status_description" => {
                out.status.message = value.as_str().map(str::to_owned);
            }
            "error" => error_tag = value == Value::Bool(true) || value.as_str() == Some("true"),
            k if k.starts_with("otel.scope.") => {}
            k => {
                out.attributes.insert(k.to_owned(), value);
            }
        }
    }
    if error_tag && out.status.code == StatusCode::Unset {
        out.status.code = StatusCode::Error;
    }
    for log in span
        .get("logs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut event = SpanEvent {
            name: String::new(),
            time_micros: as_u64(log.get("timestamp").unwrap_or(&Value::Null)),
            attributes: Attributes::new(),
        };
        for field in log
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = field.get("key").and_then(Value::as_str).unwrap_or_default();
            let value = legacy_value(field);
            match (key, value) {
                ("event", Value::String(name)) => event.name = name,
                (k, v) => {
                    event.attributes.insert(k.to_owned(), v);
                }
            }
        }
        out.events.push(event);
    }
    out
}

/// Converts the legacy `{"data": [{"traceID", "spans", "processes"}]}` body into traces: the
/// parent is the first `CHILD_OF` reference in the same trace (else the first reference in the
/// same trace), other references become links, `span.kind` / `otel.status_*` / `error` tags
/// become kind and status, logs become events.
pub fn legacy_to_traces(body: &Value) -> Vec<Trace> {
    let mut grouper = Grouper::default();
    for trace in body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let processes = trace.get("processes");
        for span in trace
            .get("spans")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let service = span
                .get("processID")
                .and_then(Value::as_str)
                .and_then(|p| processes.and_then(|ps| ps.get(p)))
                .and_then(|p| p.get("serviceName"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            grouper.add(legacy_span(span, service));
        }
    }
    grouper.traces()
}

/// Converts a `/api/v3/trace-summaries` body (`{"summaries": […]}`) into summaries.
pub fn summaries_from_body(body: &Value) -> Vec<TraceSummary> {
    body.get("summaries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let start = nanos_to_micros(s.get("minStartTimeUnixNano").unwrap_or(&Value::Null));
            let end = nanos_to_micros(s.get("maxEndTimeUnixNano").unwrap_or(&Value::Null));
            let text = |k: &str| {
                s.get(k)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            };
            TraceSummary {
                trace_id: loose_id(s.get("traceId"), true),
                root_service_name: text("rootServiceName"),
                root_name: text("rootOperationName"),
                start_micros: start,
                duration_micros: end.saturating_sub(start),
                span_count: as_u64(s.get("spanCount").unwrap_or(&Value::Null)) as usize,
            }
        })
        .collect()
}

/// Converts a `/api/dependencies` body (`{"data": [{"parent", "child", "callCount"}]}`).
pub fn dependencies_from_body(body: &Value) -> Vec<DependencyLink> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|d| DependencyLink {
            parent: d
                .get("parent")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            child: d
                .get("child")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            call_count: as_u64(d.get("callCount").unwrap_or(&Value::Null)),
        })
        .collect()
}
