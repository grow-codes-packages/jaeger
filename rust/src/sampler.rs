// Service code (Jaeger): remote sampling.
//! Jaeger remote sampling: [`SamplingStrategy`] (the `/sampling?service=` response),
//! [`parse_sampling_strategy`] and [`JaegerRemoteSampler`], an OpenTelemetry head sampler that
//! polls the strategy.
//!
//! Rule (the same in every language): a per-operation strategy wins for a listed operation (span
//! name); every other operation uses the service-level rate limiter when `rateLimitingSampling`
//! is present, else the `probabilisticSampling` rate (else `defaultSamplingProbability`).
//! Probabilistic decisions are deterministic per trace id. Until the first strategy arrives the
//! initial rate applies; a failed or malformed poll keeps the last good strategy.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use opentelemetry::trace::{
    Link, SamplingDecision, SamplingResult, SpanKind as OtelSpanKind, TraceContextExt, TraceId,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{Sampler, ShouldSample};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::core::redact::redact_url;
use crate::core::{CommonConfig, Error, HttpTransport, Result, Secret};

/// How often a [`JaegerRemoteSampler`] polls by default (Jaeger default: 1 minute).
pub const DEFAULT_SAMPLING_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Probability applied until the first strategy arrives (Jaeger and OpenTelemetry default).
pub const DEFAULT_INITIAL_SAMPLING_RATE: f64 = 0.001;

/// Strategy type of a [`SamplingStrategy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrategyType {
    /// `PROBABILISTIC`.
    Probabilistic,
    /// `RATE_LIMITING`.
    RateLimiting,
}

impl StrategyType {
    /// Wire value (`PROBABILISTIC` / `RATE_LIMITING`).
    pub fn as_str(self) -> &'static str {
        match self {
            StrategyType::Probabilistic => "PROBABILISTIC",
            StrategyType::RateLimiting => "RATE_LIMITING",
        }
    }
}

/// Sampling probability of one operation (span name).
#[derive(Debug, Clone, PartialEq)]
pub struct OperationStrategy {
    /// Span name.
    pub operation: String,
    /// Probability, 0..1.
    pub sampling_rate: f64,
}

/// A Jaeger remote sampling strategy (the `/sampling?service=` response).
#[derive(Debug, Clone, PartialEq)]
pub struct SamplingStrategy {
    /// `strategyType`.
    pub strategy_type: StrategyType,
    /// Service-level probability (`probabilisticSampling.samplingRate`, else
    /// `operationSampling.defaultSamplingProbability`), 0..1.
    pub sampling_rate: f64,
    /// `rateLimitingSampling.maxTracesPerSecond` when the response carries a rate limiter.
    pub max_traces_per_second: Option<f64>,
    /// `operationSampling.defaultSamplingProbability` when present.
    pub default_sampling_probability: Option<f64>,
    /// `operationSampling.defaultLowerBoundTracesPerSecond` (reported only, not enforced).
    pub default_lower_bound_traces_per_second: f64,
    /// Per-operation probabilities (`operationSampling.perOperationStrategies`).
    pub operations: Vec<OperationStrategy>,
}

impl SamplingStrategy {
    /// A probabilistic strategy with one rate and no operations.
    pub fn probabilistic(rate: f64) -> Self {
        SamplingStrategy {
            strategy_type: StrategyType::Probabilistic,
            sampling_rate: rate,
            max_traces_per_second: None,
            default_sampling_probability: None,
            default_lower_bound_traces_per_second: 0.0,
            operations: Vec::new(),
        }
    }

    /// Checks rates (0..1) and the traces-per-second limits (≥ 0); `VALIDATION` otherwise.
    pub fn validate(&self) -> Result<()> {
        const OP: &str = "sampling.strategy";
        let rate_ok = |r: f64| (0.0..=1.0).contains(&r);
        if !rate_ok(self.sampling_rate)
            || !self.default_sampling_probability.is_none_or(rate_ok)
            || !self.operations.iter().all(|o| rate_ok(o.sampling_rate))
        {
            return Err(Error::validation(format!(
                "{OP}: samplingRate must be between 0 and 1"
            )));
        }
        let tps_ok = |v: f64| v >= 0.0 && v.is_finite();
        if !self.max_traces_per_second.is_none_or(tps_ok)
            || !tps_ok(self.default_lower_bound_traces_per_second)
        {
            return Err(Error::validation(format!(
                "{OP}: maxTracesPerSecond must not be negative"
            )));
        }
        Ok(())
    }

    /// The probability of a listed operation, `None` when the operation is not listed.
    pub fn operation_rate(&self, operation: &str) -> Option<f64> {
        self.operations
            .iter()
            .find(|o| o.operation == operation)
            .map(|o| o.sampling_rate)
    }

    /// Whether operations that are not listed are rate limited (`rateLimitingSampling` present).
    pub fn uses_rate_limiting(&self) -> bool {
        self.max_traces_per_second.is_some()
    }
}

fn number(v: Option<&Value>, what: &str) -> Result<f64> {
    match v {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
        Some(_) => Err(Error::validation(format!(
            "sampling.strategy: {what} must be a number"
        ))),
    }
}

/// Parses and validates a `/sampling` response body. Missing numbers count as 0 (proto3 JSON
/// omits zero values); a missing `strategyType` means `PROBABILISTIC`; a `RATE_LIMITING`
/// strategy without `rateLimitingSampling` or `probabilisticSampling` rate-limits at 0 traces
/// per second. `VALIDATION` for malformed JSON, an unknown type or values out of range.
pub fn parse_sampling_strategy(body: &[u8]) -> Result<SamplingStrategy> {
    const OP: &str = "sampling.strategy";
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| Error::validation(format!("{OP}: invalid JSON")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| Error::validation(format!("{OP}: expected a JSON object")))?;
    let strategy_type = match obj.get("strategyType") {
        None | Some(Value::Null) => StrategyType::Probabilistic,
        Some(t) => match t
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| t.to_string())
            .as_str()
        {
            "" | "0" | "PROBABILISTIC" => StrategyType::Probabilistic,
            "1" | "RATE_LIMITING" => StrategyType::RateLimiting,
            _ => return Err(Error::validation(format!("{OP}: unknown strategy type"))),
        },
    };
    let prob = obj.get("probabilisticSampling").filter(|p| p.is_object());
    let rl = obj.get("rateLimitingSampling").filter(|p| p.is_object());
    let ops = obj.get("operationSampling").filter(|p| p.is_object());
    let mut s = SamplingStrategy::probabilistic(0.0);
    s.strategy_type = strategy_type;
    if let Some(p) = prob {
        s.sampling_rate = number(p.get("samplingRate"), "samplingRate")?;
    }
    if let Some(r) = rl {
        s.max_traces_per_second = Some(number(r.get("maxTracesPerSecond"), "maxTracesPerSecond")?);
    } else if strategy_type == StrategyType::RateLimiting && prob.is_none() {
        s.max_traces_per_second = Some(0.0);
    }
    if let Some(o) = ops {
        let default = number(
            o.get("defaultSamplingProbability"),
            "defaultSamplingProbability",
        )?;
        s.default_sampling_probability = Some(default);
        s.default_lower_bound_traces_per_second = number(
            o.get("defaultLowerBoundTracesPerSecond"),
            "defaultLowerBoundTracesPerSecond",
        )?;
        if prob.is_none() {
            s.sampling_rate = default;
        }
        for p in o
            .get("perOperationStrategies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            s.operations.push(OperationStrategy {
                operation: p
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                sampling_rate: number(
                    p.get("probabilisticSampling")
                        .and_then(|x| x.get("samplingRate")),
                    "samplingRate",
                )?,
            });
        }
    }
    s.validate()?;
    Ok(s)
}

/// `GET <sampling_url>?service=<service>` with `transport` (the URL is absolute).
pub(crate) async fn fetch_strategy(
    transport: &HttpTransport,
    sampling_url: &url::Url,
    token: Option<&Secret>,
    service: &str,
) -> Result<SamplingStrategy> {
    const OP: &str = "sampling.fetch";
    if service.trim().is_empty() {
        return Err(Error::validation(format!("{OP}: service is required")));
    }
    let mut req = transport
        .get(sampling_url.as_str())
        .operation(OP)
        .query("service", service);
    if let Some(t) = token {
        req = req.bearer(t);
    }
    let resp = req.send().await?;
    parse_sampling_strategy(resp.bytes()).map_err(|e| {
        let err = Error::unavailable(format!("{OP}: invalid strategy response"))
            .with_status(resp.status())
            .with_retryable(true)
            .with_source(e);
        match resp.request_id() {
            Some(id) => err.with_request_id(id),
            None => err,
        }
    })
}

/// Token bucket that lets at most `credits_per_second` traces through per second, with a burst
/// of `max(credits_per_second, 1)`. It starts full, so a rate of 0 lets the first trace through
/// and then none (Jaeger behaviour).
#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    rate: f64,
    burst: f64,
    balance: f64,
    last: Instant,
}

impl RateLimiter {
    /// A limiter for `credits_per_second` (≥ 0).
    pub fn new(credits_per_second: f64) -> Self {
        Self::starting_at(credits_per_second, Instant::now())
    }

    fn starting_at(rate: f64, now: Instant) -> Self {
        let burst = rate.max(1.0);
        RateLimiter {
            state: Mutex::new(Bucket {
                rate,
                burst,
                balance: burst,
                last: now,
            }),
        }
    }

    /// The configured rate.
    pub fn rate(&self) -> f64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).rate
    }

    /// Takes one credit if available.
    pub fn check(&self) -> bool {
        self.check_at(Instant::now())
    }

    fn check_at(&self, now: Instant) -> bool {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.balance = (b.balance + elapsed * b.rate).min(b.burst);
        b.last = now;
        if b.balance >= 1.0 {
            b.balance -= 1.0;
            true
        } else {
            false
        }
    }
}

type ErrorCallback = Arc<dyn Fn(&Error) + Send + Sync>;
type UpdateCallback = Arc<dyn Fn(&SamplingStrategy) + Send + Sync>;

/// Options of [`JaegerRemoteSampler::with_options`].
#[derive(Clone)]
pub struct RemoteSamplerOptions {
    /// Service whose strategy is fetched (`?service=`). Required.
    pub service_name: String,
    /// Sampling endpoint (default [`DEFAULT_SAMPLING_URL`](crate::DEFAULT_SAMPLING_URL)).
    pub sampling_url: Option<String>,
    /// Poll interval (default [`DEFAULT_SAMPLING_POLL_INTERVAL`]).
    pub poll_interval: Duration,
    /// Probability used until the first strategy arrives (default
    /// [`DEFAULT_INITIAL_SAMPLING_RATE`]).
    pub initial_sampling_rate: f64,
    /// Transport settings (timeouts, retries, TLS, proxy, headers, hooks); its URL is ignored.
    pub http_config: Option<CommonConfig>,
    /// Bearer token for the sampling endpoint.
    pub token: Option<Secret>,
    /// Start polling at once (first fetch immediately, needs a tokio runtime). Default `true`.
    pub auto_start: bool,
    /// Called when a poll fails; the last good strategy stays in use.
    pub on_error: Option<ErrorCallback>,
    /// Called after each successful poll.
    pub on_update: Option<UpdateCallback>,
}

impl fmt::Debug for RemoteSamplerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSamplerOptions")
            .field("service_name", &self.service_name)
            .field(
                "sampling_url",
                &self.sampling_url.as_deref().map(redact_url),
            )
            .field("poll_interval", &self.poll_interval)
            .field("initial_sampling_rate", &self.initial_sampling_rate)
            .field("token", &self.token)
            .field("auto_start", &self.auto_start)
            .finish_non_exhaustive()
    }
}

impl RemoteSamplerOptions {
    /// Defaults for a service.
    pub fn new(service_name: impl Into<String>) -> Self {
        RemoteSamplerOptions {
            service_name: service_name.into(),
            sampling_url: None,
            poll_interval: DEFAULT_SAMPLING_POLL_INTERVAL,
            initial_sampling_rate: DEFAULT_INITIAL_SAMPLING_RATE,
            http_config: None,
            token: None,
            auto_start: true,
            on_error: None,
            on_update: None,
        }
    }
    /// Sets the sampling endpoint.
    pub fn sampling_url(mut self, url: impl Into<String>) -> Self {
        self.sampling_url = Some(url.into());
        self
    }
    /// Sets the poll interval.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
    /// Sets the initial sampling rate.
    pub fn initial_sampling_rate(mut self, rate: f64) -> Self {
        self.initial_sampling_rate = rate;
        self
    }
    /// Sets the transport settings.
    pub fn http_config(mut self, config: CommonConfig) -> Self {
        self.http_config = Some(config);
        self
    }
    /// Sets the bearer token.
    pub fn token(mut self, token: impl Into<Secret>) -> Self {
        self.token = Some(token.into());
        self
    }
    /// Sets `auto_start`.
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }
    /// Sets the poll error callback.
    pub fn on_error(mut self, f: impl Fn(&Error) + Send + Sync + 'static) -> Self {
        self.on_error = Some(Arc::new(f));
        self
    }
    /// Sets the update callback.
    pub fn on_update(mut self, f: impl Fn(&SamplingStrategy) + Send + Sync + 'static) -> Self {
        self.on_update = Some(Arc::new(f));
        self
    }
}

/// Decision state built from one strategy.
#[derive(Debug)]
struct State {
    strategy: SamplingStrategy,
    fetched: bool,
    ops: HashMap<String, Sampler>,
    limiter: Option<Arc<RateLimiter>>,
    fallback: Sampler,
}

impl State {
    fn new(strategy: SamplingStrategy, fetched: bool, previous: Option<&State>) -> Self {
        let ops = strategy
            .operations
            .iter()
            .map(|o| {
                (
                    o.operation.clone(),
                    Sampler::TraceIdRatioBased(o.sampling_rate),
                )
            })
            .collect();
        let limiter = strategy.max_traces_per_second.map(|rate| {
            match previous.and_then(|p| p.limiter.as_ref()) {
                Some(old) if old.rate() == rate => Arc::clone(old),
                _ => Arc::new(RateLimiter::new(rate)),
            }
        });
        let fallback = Sampler::TraceIdRatioBased(strategy.sampling_rate);
        State {
            strategy,
            fetched,
            ops,
            limiter,
            fallback,
        }
    }
}

struct SamplerInner {
    transport: Arc<HttpTransport>,
    owns_transport: bool,
    url: url::Url,
    token: Option<Secret>,
    service: String,
    interval: Duration,
    on_error: Option<ErrorCallback>,
    on_update: Option<UpdateCallback>,
    state: RwLock<State>,
    closed: AtomicBool,
    cancel: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for SamplerInner {
    fn drop(&mut self) {
        self.cancel.cancel();
        if self.owns_transport {
            self.transport.close();
        }
    }
}

/// OpenTelemetry head sampler driven by Jaeger remote sampling: it polls
/// `GET <sampling_url>?service=<service>` every poll interval and applies the strategy (see the
/// module docs for the rule). Use it with
/// [`init_tracing_with_sampler`](crate::init_tracing_with_sampler) (which wraps it in a
/// parent-based sampler) or `SdkTracerProvider::builder().with_sampler(…)`. Cheap to clone
/// (clones share the strategy and the poller). [`close`](Self::close) stops polling.
#[derive(Clone)]
pub struct JaegerRemoteSampler {
    inner: Arc<SamplerInner>,
}

impl fmt::Debug for JaegerRemoteSampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for JaegerRemoteSampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JaegerRemoteSampler{{service={},url={}}}",
            self.inner.service,
            redact_url(self.inner.url.as_str())
        )
    }
}

impl JaegerRemoteSampler {
    /// A sampler for `service` that polls `sampling_url` (for example
    /// `http://127.0.0.1:5778/sampling`) every `poll_interval` (zero = default). Polling starts
    /// at once in the background, so it must be called inside a tokio runtime.
    pub fn new(
        sampling_url: impl Into<String>,
        service: impl Into<String>,
        poll_interval: Duration,
    ) -> Result<Self> {
        Self::with_options(
            RemoteSamplerOptions::new(service)
                .sampling_url(sampling_url)
                .poll_interval(poll_interval),
        )
    }

    /// A sampler from options.
    pub fn with_options(opts: RemoteSamplerOptions) -> Result<Self> {
        let raw = opts
            .sampling_url
            .clone()
            .unwrap_or_else(|| crate::config::DEFAULT_SAMPLING_URL.to_owned());
        let url = url::Url::parse(&raw)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
            .ok_or_else(|| {
                Error::config("sampling: samplingUrl must be an absolute http or https URL")
            })?;
        let common = match opts.http_config.clone() {
            Some(c) => c,
            None => CommonConfig::builder().build(url.as_str())?,
        };
        let transport = Arc::new(HttpTransport::new(&common)?);
        Self::create(transport, true, url, opts)
    }

    /// A sampler sharing a client's transport (see
    /// [`JaegerClient::remote_sampler`](crate::JaegerClient::remote_sampler)).
    pub(crate) fn create(
        transport: Arc<HttpTransport>,
        owns_transport: bool,
        url: url::Url,
        opts: RemoteSamplerOptions,
    ) -> Result<Self> {
        if opts.service_name.trim().is_empty() {
            return Err(Error::config("sampling: service is required"));
        }
        if !(0.0..=1.0).contains(&opts.initial_sampling_rate) {
            return Err(Error::config(
                "sampling: initial sampling rate must be between 0 and 1",
            ));
        }
        let runtime = tokio::runtime::Handle::try_current().ok();
        if opts.auto_start && runtime.is_none() {
            return Err(Error::config(
                "sampling: the remote sampler must be created inside a tokio runtime",
            ));
        }
        if let Some(t) = &opts.token {
            transport.add_known_secret(t);
        }
        let interval = if opts.poll_interval.is_zero() {
            DEFAULT_SAMPLING_POLL_INTERVAL
        } else {
            opts.poll_interval
        };
        let sampler = JaegerRemoteSampler {
            inner: Arc::new(SamplerInner {
                transport,
                owns_transport,
                url,
                token: opts.token,
                service: opts.service_name,
                interval,
                on_error: opts.on_error,
                on_update: opts.on_update,
                state: RwLock::new(State::new(
                    SamplingStrategy::probabilistic(opts.initial_sampling_rate),
                    false,
                    None,
                )),
                closed: AtomicBool::new(false),
                cancel: CancellationToken::new(),
                task: Mutex::new(None),
            }),
        };
        if opts.auto_start {
            sampler.start();
        }
        Ok(sampler)
    }

    /// Starts the background poller (idempotent; a no-op after [`close`](Self::close)). Must be
    /// called inside a tokio runtime.
    pub fn start(&self) {
        let mut task = self.inner.task.lock().unwrap_or_else(|e| e.into_inner());
        if task.is_some() || self.is_closed() {
            return;
        }
        let weak: Weak<SamplerInner> = Arc::downgrade(&self.inner);
        let cancel = self.inner.cancel.clone();
        let interval = self.inner.interval;
        *task = Some(tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let sampler = JaegerRemoteSampler { inner };
                let refreshed = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    r = sampler.refresh() => r,
                };
                if let (Err(err), Some(cb)) = (refreshed, sampler.inner.on_error.clone()) {
                    if !cancel.is_cancelled() {
                        cb(&err);
                    }
                }
                drop(sampler);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        }));
    }

    /// Fetches the strategy now and applies it. On error the previous strategy stays active
    /// and the error is returned.
    pub async fn refresh(&self) -> Result<SamplingStrategy> {
        if self.is_closed() {
            return Err(Error::cancelled("sampling.fetch: sampler is closed"));
        }
        let strategy = fetch_strategy(
            &self.inner.transport,
            &self.inner.url,
            self.inner.token.as_ref(),
            &self.inner.service,
        )
        .await?;
        {
            let mut state = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
            let next = State::new(strategy.clone(), true, Some(&state));
            *state = next;
        }
        if let Some(cb) = &self.inner.on_update {
            cb(&strategy);
        }
        Ok(strategy)
    }

    /// Waits until a strategy from the server is active (fetches one now if none arrived yet).
    pub async fn ready(&self) -> Result<()> {
        if self.strategy().is_some() {
            return Ok(());
        }
        self.refresh().await.map(|_| ())
    }

    /// The active strategy from the server; `None` while only the initial rate applies.
    pub fn strategy(&self) -> Option<SamplingStrategy> {
        let state = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        state.fetched.then(|| state.strategy.clone())
    }

    /// The service whose strategy is polled.
    pub fn service(&self) -> &str {
        &self.inner.service
    }

    /// Stops polling; idempotent.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.cancel.cancel();
        let task = self
            .inner
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        if self.inner.owns_transport {
            self.inner.transport.close();
        }
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

impl ShouldSample for JaegerRemoteSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &OtelSpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let state = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        let ratio = match (state.ops.get(name), &state.limiter) {
            (Some(op), _) => op,
            (None, Some(limiter)) => {
                let decision = if limiter.check() {
                    SamplingDecision::RecordAndSample
                } else {
                    SamplingDecision::Drop
                };
                let trace_state = parent_context
                    .filter(|cx| cx.has_active_span())
                    .map(|cx| cx.span().span_context().trace_state().clone())
                    .unwrap_or_default();
                return SamplingResult {
                    decision,
                    attributes: Vec::new(),
                    trace_state,
                };
            }
            (None, None) => &state.fallback,
        };
        ratio.should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_bucket() {
        let t0 = Instant::now();
        let zero = RateLimiter::starting_at(0.0, t0);
        assert!(zero.check_at(t0));
        assert!(!zero.check_at(t0 + Duration::from_secs(10)));
        let two = RateLimiter::starting_at(2.0, t0);
        assert!(two.check_at(t0));
        assert!(two.check_at(t0));
        assert!(!two.check_at(t0));
        assert!(two.check_at(t0 + Duration::from_millis(500)));
        assert!(!two.check_at(t0 + Duration::from_millis(500)));
        // time going backwards adds nothing
        assert!(!two.check_at(t0));
        assert_eq!(two.rate(), 2.0);
    }
}
