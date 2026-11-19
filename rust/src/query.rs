// Service code (Jaeger): the HTTP query client.
//! [`JaegerQueryClient`]: reads traces from the Jaeger HTTP query API (port 16686).
//!
//! It uses the v3 API (`/api/v3/traces`, `/api/v3/trace-summaries`, `/api/v3/services`,
//! `/api/v3/operations`), the legacy `/api/traces/{id}` as a fallback for
//! [`get_trace`](JaegerQueryClient::get_trace) when the v3 route is missing, and the legacy
//! `/api/dependencies` (Jaeger has no v3 HTTP dependencies route).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::{Error, HttpTransport, Response, Result, Secret};
use crate::normalize::{
    dependencies_from_body, legacy_to_traces, otlp_to_traces, summaries_from_body,
};
use crate::tracing::{
    normalize_trace_id, wait_for_trace, DependencyLink, DependencyOptions, SpanKind, Trace,
    TraceQueryClient, TraceSearchQuery, TraceSummary, WaitForTraceOptions,
};

/// Search window when a [`TraceSearchQuery`] has no `start`: the hour before `end` (Jaeger
/// requires both bounds).
pub const DEFAULT_SEARCH_WINDOW: Duration = Duration::from_secs(3600);
/// Time hint [`JaegerQueryClient::get_trace`] sends: the last 7 days.
pub const DEFAULT_GET_TRACE_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);
/// Dependency lookback when no start is given: 24 hours.
pub const DEFAULT_DEPENDENCY_LOOKBACK: Duration = Duration::from_secs(24 * 3600);

/// Options of [`JaegerQueryClient::get_trace_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GetTraceOptions {
    /// Start of the time hint for the storage (default: `end` minus 7 days). The in-memory
    /// store ignores it.
    pub start: Option<SystemTime>,
    /// End of the time hint (default: now).
    pub end: Option<SystemTime>,
    /// Skips Jaeger's span adjusters (clock skew, deduplication).
    pub raw_traces: bool,
}

/// One operation of a service, as returned by [`JaegerQueryClient::get_operations`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    /// Span name.
    pub name: String,
    /// Span kind (`Unspecified` when Jaeger reports none).
    pub span_kind: SpanKind,
}

/// Options of [`JaegerQueryClient::list_operations`] and [`JaegerQueryClient::get_operations`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListOperationsOptions {
    /// Keep only operations of this kind (`Server`, `Client`, `Producer`, `Consumer`,
    /// `Internal`; `None` = all). Sent lower-case (Jaeger is case-sensitive).
    pub span_kind: Option<SpanKind>,
}

impl ListOperationsOptions {
    /// Options filtering by span kind.
    pub fn span_kind(kind: SpanKind) -> Self {
        ListOperationsOptions {
            span_kind: Some(kind),
        }
    }
}

/// Outcome of a query call that may legitimately answer 404.
enum Outcome {
    /// 2xx response.
    Ok(Response),
    /// 404 with Jaeger's JSON error envelope: the record does not exist.
    NotFound,
    /// 404 without the envelope (`404 page not found`, proxy page): the route is missing.
    RouteMissing,
}

/// Whether a 404 body is Jaeger's JSON error envelope (v3 `{"error":{…}}` or legacy
/// `{"errors":[…]}`) rather than a missing route. Jaeger sends it as `text/plain`, so the body
/// is parsed whatever the content type.
fn is_json_envelope(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.as_object()
                .map(|o| o.contains_key("error") || o.contains_key("errors"))
        })
        .unwrap_or(false)
}

fn route_missing(op: &str, route: &str) -> Error {
    Error::unsupported(format!("{op}: the query API has no {route} route"))
}

fn invalid_json(op: &str, resp: &Response) -> Error {
    let err = Error::unavailable(format!("{op}: invalid JSON response"))
        .with_status(resp.status())
        .with_retryable(true);
    match resp.request_id() {
        Some(id) => err.with_request_id(id),
        None => err,
    }
}

fn body_json(op: &str, resp: &Response) -> Result<Value> {
    serde_json::from_slice(resp.bytes()).map_err(|_| invalid_json(op, resp))
}

/// Formats a time as RFC 3339 with nanoseconds in UTC (Jaeger's v3 time format).
pub(crate) fn format_time(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_nanos()
    )
}

/// Formats a duration in Go duration syntax as whole microseconds (`1500us`, at least `1us`).
pub(crate) fn format_duration(d: Duration) -> String {
    format!("{}us", d.as_micros().max(1))
}

fn unix_millis(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

fn unix_micros(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_micros()
}

/// Query client for the Jaeger HTTP query API. Get it with
/// [`JaegerClient::query`](crate::JaegerClient::query) or build it with
/// [`JaegerQueryClient::new`]. Cheap to clone; implements [`TraceQueryClient`].
#[derive(Clone)]
pub struct JaegerQueryClient {
    transport: Arc<HttpTransport>,
    token: Option<Secret>,
    now: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl std::fmt::Debug for JaegerQueryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JaegerQueryClient")
            .field(
                "url",
                &crate::core::redact::redact_url(self.transport.config().url().as_str()),
            )
            .field("token", &self.token)
            .finish()
    }
}

impl JaegerQueryClient {
    /// A query client over a transport (its base URL is the query endpoint) with an optional
    /// bearer token.
    pub fn new(transport: Arc<HttpTransport>, token: Option<Secret>) -> Self {
        if let Some(t) = &token {
            transport.add_known_secret(t);
        }
        JaegerQueryClient {
            transport,
            token,
            now: Arc::new(SystemTime::now),
        }
    }

    /// Replaces the clock used for default time windows (tests).
    pub fn with_clock(mut self, now: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }

    /// The transport in use.
    pub fn transport(&self) -> &HttpTransport {
        &self.transport
    }

    async fn call(&self, op: &str, path: &str, query: Vec<(String, String)>) -> Result<Outcome> {
        let mut req = self.transport.get(path).operation(op).ok_status(404);
        for (k, v) in query {
            req = req.query(k, v);
        }
        if let Some(token) = &self.token {
            req = req.bearer(token);
        }
        let resp = req.send().await?;
        if resp.status() != 404 {
            return Ok(Outcome::Ok(resp));
        }
        Ok(if is_json_envelope(resp.bytes()) {
            Outcome::NotFound
        } else {
            Outcome::RouteMissing
        })
    }

    /// The trace with all spans sorted, or `None` when it does not exist. The id is 1–32 hex
    /// characters (64-bit ids are left-padded); anything else is `VALIDATION`. Calls
    /// `/api/v3/traces/{id}` and falls back to the legacy `/api/traces/{id}` when the v3 route
    /// is missing.
    pub async fn get_trace(&self, trace_id: &str) -> Result<Option<Trace>> {
        self.get_trace_with(trace_id, GetTraceOptions::default())
            .await
    }

    /// [`get_trace`](Self::get_trace) with a time hint and the raw-traces switch.
    pub async fn get_trace_with(
        &self,
        trace_id: &str,
        opts: GetTraceOptions,
    ) -> Result<Option<Trace>> {
        const OP: &str = "traces.getTrace";
        let id = normalize_trace_id(trace_id)?;
        let end = opts.end.unwrap_or_else(|| (self.now)());
        let start = opts.start.unwrap_or_else(|| {
            end.checked_sub(DEFAULT_GET_TRACE_WINDOW)
                .unwrap_or(UNIX_EPOCH)
        });
        if start > end {
            return Err(Error::validation(format!(
                "{OP}: start must not be after end"
            )));
        }
        let mut q = vec![
            ("startTime".to_owned(), format_time(start)),
            ("endTime".to_owned(), format_time(end)),
        ];
        if opts.raw_traces {
            q.push(("rawTraces".to_owned(), "true".to_owned()));
        }
        let pick = |traces: Vec<Trace>| traces.into_iter().find(|t| t.trace_id == id);
        match self.call(OP, &format!("/api/v3/traces/{id}"), q).await? {
            Outcome::NotFound => return Ok(None),
            Outcome::Ok(resp) => return Ok(pick(otlp_to_traces(&body_json(OP, &resp)?))),
            Outcome::RouteMissing => {}
        }
        let mut q = vec![
            ("start".to_owned(), unix_micros(start).to_string()),
            ("end".to_owned(), unix_micros(end).to_string()),
        ];
        if opts.raw_traces {
            q.push(("raw".to_owned(), "true".to_owned()));
        }
        match self.call(OP, &format!("/api/traces/{id}"), q).await? {
            Outcome::NotFound => Ok(None),
            Outcome::RouteMissing => Err(route_missing(OP, "/api/v3/traces or /api/traces")),
            Outcome::Ok(resp) => Ok(pick(legacy_to_traces(&body_json(OP, &resp)?))),
        }
    }

    /// Like [`get_trace`](Self::get_trace), but a missing trace is `NOT_FOUND`.
    pub async fn require_trace(&self, trace_id: &str) -> Result<Trace> {
        self.get_trace(trace_id).await?.ok_or_else(|| {
            Error::not_found(format!("traces.getTrace: trace {trace_id} does not exist"))
        })
    }

    /// The v3 `query.*` parameters (defaults: `end` now, `start` = `end` − 1 h, `limit` 20).
    fn search_params(&self, op: &str, q: &TraceSearchQuery) -> Result<Vec<(String, String)>> {
        if let (Some(min), Some(max)) = (q.min_duration, q.max_duration) {
            if min > max {
                return Err(Error::validation(format!(
                    "{op}: minDuration must not exceed maxDuration"
                )));
            }
        }
        let end = q.end.unwrap_or_else(|| (self.now)());
        let start = q
            .start
            .unwrap_or_else(|| end.checked_sub(DEFAULT_SEARCH_WINDOW).unwrap_or(UNIX_EPOCH));
        if start > end {
            return Err(Error::validation(format!(
                "{op}: start must not be after end"
            )));
        }
        let limit = if q.limit == 0 { 20 } else { q.limit };
        let mut v = vec![
            ("query.startTimeMin".to_owned(), format_time(start)),
            ("query.startTimeMax".to_owned(), format_time(end)),
            ("query.searchDepth".to_owned(), limit.to_string()),
        ];
        if let Some(s) = q.service.as_deref().filter(|s| !s.is_empty()) {
            v.push(("query.serviceName".to_owned(), s.to_owned()));
        }
        if let Some(o) = q.operation.as_deref().filter(|o| !o.is_empty()) {
            v.push(("query.operationName".to_owned(), o.to_owned()));
        }
        if !q.tags.is_empty() {
            // A BTreeMap<String, String> always serializes.
            let tags = serde_json::to_string(&q.tags).unwrap_or_default();
            v.push(("query.attributes".to_owned(), tags));
        }
        if let Some(d) = q.min_duration.filter(|d| !d.is_zero()) {
            v.push(("query.durationMin".to_owned(), format_duration(d)));
        }
        if let Some(d) = q.max_duration.filter(|d| !d.is_zero()) {
            v.push(("query.durationMax".to_owned(), format_duration(d)));
        }
        Ok(v)
    }

    /// Full traces matching the query (`GET /api/v3/traces`), each with sorted spans, in the
    /// order Jaeger returns them. Tags match attributes on any span of the trace; durations are
    /// inclusive. No match returns an empty list. Defaults: `end` now, `start` = `end` − 1 h,
    /// `limit` 20.
    pub async fn find_traces(&self, query: &TraceSearchQuery) -> Result<Vec<Trace>> {
        const OP: &str = "traces.findTraces";
        let params = self.search_params(OP, query)?;
        match self.call(OP, "/api/v3/traces", params).await? {
            Outcome::NotFound => Ok(Vec::new()),
            Outcome::RouteMissing => Err(route_missing(OP, "/api/v3/traces")),
            Outcome::Ok(resp) => Ok(otlp_to_traces(&body_json(OP, &resp)?)),
        }
    }

    /// Trace summaries matching the query (`GET /api/v3/trace-summaries`), in the order Jaeger
    /// returns them. Same filters and defaults as [`find_traces`](Self::find_traces).
    pub async fn search_traces(&self, query: &TraceSearchQuery) -> Result<Vec<TraceSummary>> {
        const OP: &str = "traces.searchTraces";
        let params = self.search_params(OP, query)?;
        match self.call(OP, "/api/v3/trace-summaries", params).await? {
            Outcome::NotFound => Ok(Vec::new()),
            Outcome::RouteMissing => Err(route_missing(OP, "/api/v3/trace-summaries")),
            Outcome::Ok(resp) => Ok(summaries_from_body(&body_json(OP, &resp)?)),
        }
    }

    /// Names of all services with stored spans, sorted (`GET /api/v3/services`). The
    /// all-in-one always lists itself as `jaeger`.
    pub async fn list_services(&self) -> Result<Vec<String>> {
        const OP: &str = "traces.listServices";
        let resp = match self.call(OP, "/api/v3/services", Vec::new()).await? {
            Outcome::Ok(resp) => resp,
            _ => return Err(route_missing(OP, "/api/v3/services")),
        };
        let body = body_json(OP, &resp)?;
        let mut out: Vec<String> = body
            .get("services")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        out.sort();
        Ok(out)
    }

    /// Operations of a service with their span kinds (`GET /api/v3/operations`), optionally
    /// filtered by kind, sorted by name then kind. An empty service is `VALIDATION`; an unknown
    /// service returns an empty list.
    pub async fn get_operations(
        &self,
        service: &str,
        opts: ListOperationsOptions,
    ) -> Result<Vec<Operation>> {
        const OP: &str = "traces.listOperations";
        if service.trim().is_empty() {
            return Err(Error::validation(format!("{OP}: service is required")));
        }
        let mut params = vec![("service".to_owned(), service.to_owned())];
        if let Some(kind) = opts.span_kind {
            let wire = match kind {
                SpanKind::Server => "server",
                SpanKind::Client => "client",
                SpanKind::Producer => "producer",
                SpanKind::Consumer => "consumer",
                SpanKind::Internal => "internal",
                SpanKind::Unspecified => {
                    return Err(Error::validation(format!(
                        "{OP}: spanKind must be server, client, producer, consumer or internal"
                    )))
                }
            };
            params.push(("spanKind".to_owned(), wire.to_owned()));
        }
        let resp = match self.call(OP, "/api/v3/operations", params).await? {
            Outcome::Ok(resp) => resp,
            _ => return Err(route_missing(OP, "/api/v3/operations")),
        };
        let body = body_json(OP, &resp)?;
        let mut out: Vec<Operation> = body
            .get("operations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|o| Operation {
                name: o
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                span_kind: o
                    .get("spanKind")
                    .and_then(Value::as_str)
                    .map(SpanKind::parse)
                    .unwrap_or(SpanKind::Unspecified),
            })
            .collect();
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{:?}", a.span_kind).cmp(&format!("{:?}", b.span_kind)))
        });
        Ok(out)
    }

    /// Operation (span) names of a service, deduplicated across span kinds and sorted; see
    /// [`get_operations`](Self::get_operations).
    pub async fn list_operations(
        &self,
        service: &str,
        opts: ListOperationsOptions,
    ) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .get_operations(service, opts)
            .await?
            .into_iter()
            .map(|o| o.name)
            .collect();
        names.dedup();
        Ok(names)
    }

    /// Dependency links of spans in `[start, end]` (defaults: `end` now, `start` = `end` −
    /// 24 h) from the legacy `/api/dependencies`. The in-memory store derives them on the fly;
    /// production stores need the `spark-dependencies` job, so an empty result is normal there.
    pub async fn get_dependencies(&self, opts: DependencyOptions) -> Result<Vec<DependencyLink>> {
        let end = opts.end.unwrap_or_else(|| (self.now)());
        let start = opts.start.unwrap_or_else(|| {
            end.checked_sub(DEFAULT_DEPENDENCY_LOOKBACK)
                .unwrap_or(UNIX_EPOCH)
        });
        let lookback = end.duration_since(start).map_err(|_| {
            Error::validation("traces.getDependencies: start must not be after end")
        })?;
        self.get_dependencies_at(Some(end), Some(lookback)).await
    }

    /// Dependency links of the `lookback` window that ends at `end_ts` (Jaeger's
    /// `endTs`/`lookback` form). `None` means now and 24 h; a zero lookback means 24 h.
    pub async fn get_dependencies_at(
        &self,
        end_ts: Option<SystemTime>,
        lookback: Option<Duration>,
    ) -> Result<Vec<DependencyLink>> {
        const OP: &str = "traces.getDependencies";
        let end = end_ts.unwrap_or_else(|| (self.now)());
        let lookback = lookback
            .filter(|d| !d.is_zero())
            .unwrap_or(DEFAULT_DEPENDENCY_LOOKBACK);
        let params = vec![
            ("endTs".to_owned(), unix_millis(end).to_string()),
            (
                "lookback".to_owned(),
                lookback.as_millis().max(1).to_string(),
            ),
        ];
        match self.call(OP, "/api/dependencies", params).await? {
            Outcome::RouteMissing => Err(route_missing(OP, "/api/dependencies")),
            Outcome::NotFound => Ok(Vec::new()),
            Outcome::Ok(resp) => Ok(dependencies_from_body(&body_json(OP, &resp)?)),
        }
    }

    /// Polls [`get_trace`](Self::get_trace) until the trace has at least `opts.min_spans`
    /// spans (test helper); `TIMEOUT` otherwise.
    pub async fn wait_for_trace(&self, trace_id: &str, opts: WaitForTraceOptions) -> Result<Trace> {
        wait_for_trace(self, trace_id, opts).await
    }
}

#[async_trait]
impl TraceQueryClient for JaegerQueryClient {
    async fn get_trace(&self, trace_id: &str) -> Result<Option<Trace>> {
        JaegerQueryClient::get_trace(self, trace_id).await
    }
    async fn search_traces(&self, query: &TraceSearchQuery) -> Result<Vec<TraceSummary>> {
        JaegerQueryClient::search_traces(self, query).await
    }
    async fn list_services(&self) -> Result<Vec<String>> {
        JaegerQueryClient::list_services(self).await
    }
    async fn list_operations(&self, service: &str) -> Result<Vec<String>> {
        JaegerQueryClient::list_operations(self, service, ListOperationsOptions::default()).await
    }
    async fn get_dependencies(&self, opts: DependencyOptions) -> Result<Vec<DependencyLink>> {
        JaegerQueryClient::get_dependencies(self, opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_and_duration_format() {
        let t = UNIX_EPOCH + Duration::new(1_763_000_000, 5);
        assert_eq!(format_time(t), "2025-11-13T02:13:20.000000005Z");
        assert_eq!(format_time(UNIX_EPOCH), "1970-01-01T00:00:00.000000000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(format_time(leap), "2000-02-29T00:00:00.000000000Z");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500000us");
        assert_eq!(format_duration(Duration::from_nanos(10)), "1us");
    }

    #[test]
    fn envelope_detection() {
        assert!(is_json_envelope(br#"{"error":{"httpCode":404}}"#));
        assert!(is_json_envelope(
            br#"{"data":null,"errors":[{"code":404}]}"#
        ));
        assert!(!is_json_envelope(b"404 page not found\n"));
        assert!(!is_json_envelope(b"[1]"));
    }
}
