// Skeleton created by sdk-kit 0.1.0; owned by this repository (edit freely).
//! `JaegerClient`: entry point of the SDK (SPEC §2.4): health, version, the query API
//! ([`JaegerClient::query`]) and remote sampling.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use serde_json::Value;

use crate::config::JaegerConfig;
use crate::core::lifecycle::CloseGroup;
use crate::core::{Error, ErrorCode, HealthStatus, HttpTransport, Response, Result};
use crate::query::JaegerQueryClient;
use crate::sampler::{fetch_strategy, JaegerRemoteSampler, RemoteSamplerOptions, SamplingStrategy};

/// Async client for Jaeger. Cheap to clone (clones share one connection pool).
#[derive(Debug, Clone)]
pub struct JaegerClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: JaegerConfig,
    transport: Arc<HttpTransport>,
    query: JaegerQueryClient,
    on_close: CloseGroup,
    samplers: Mutex<Vec<JaegerRemoteSampler>>,
}

/// Words in a health body's `status` that mean "not healthy".
const UNHEALTHY_WORDS: &[&str] = &["unhealthy", "down", "error", "fail", "failed", "degraded"];

impl JaegerClient {
    /// Creates a client from a configuration. It makes no network call.
    pub fn new(config: JaegerConfig) -> Result<Self> {
        config.validate()?;
        let transport = Arc::new(HttpTransport::new(config.common())?);
        let query = JaegerQueryClient::new(Arc::clone(&transport), config.token().cloned());
        Ok(JaegerClient {
            inner: Arc::new(Inner {
                config,
                transport,
                query,
                on_close: CloseGroup::new(),
                samplers: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Creates a client from the process environment (see [`JaegerConfig::from_env`]).
    pub fn from_env() -> Result<Self> {
        Self::new(JaegerConfig::from_env()?)
    }

    /// The configuration in use.
    pub fn config(&self) -> &JaegerConfig {
        &self.inner.config
    }

    /// The HTTP transport (for service modules).
    pub fn transport(&self) -> &HttpTransport {
        &self.inner.transport
    }

    /// The query client for the Jaeger HTTP query API; it implements
    /// [`TraceQueryClient`](crate::tracing::TraceQueryClient).
    pub fn query(&self) -> &JaegerQueryClient {
        &self.inner.query
    }

    /// Registers work that [`JaegerClient::close`] runs (flush, stop watchers, …).
    pub fn on_close<F>(&self, f: F)
    where
        F: FnOnce() -> BoxFuture<'static, ()> + Send + 'static,
    {
        self.inner.on_close.register(f);
    }

    /// One health probe: GET without credentials, never retried. Unreachable or HTTP errors
    /// become an unhealthy status; only `CANCELLED` and `CONFIG` are errors.
    async fn probe(&self, url: &str, accept: &str) -> Result<HealthStatus> {
        let mut req = self
            .transport()
            .get(url)
            .operation("health")
            .header("accept", accept);
        for status in [400, 401, 403, 404, 405, 429, 500, 502, 503, 504] {
            req = req.ok_status(status);
        }
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(err) => return HealthStatus::from_error(err),
        };
        let ok = (200..300).contains(&resp.status());
        let mut status: Option<String> = None;
        let mut details = BTreeMap::new();
        details.insert("httpStatus".to_owned(), resp.status().to_string());
        let mut healthy_flag = true;
        if let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(resp.bytes()) {
            for (k, v) in body {
                match (k.as_str(), v) {
                    ("status", Value::String(s)) => status = Some(s),
                    ("healthy", Value::Bool(b)) => {
                        healthy_flag = b;
                        details.insert(k, b.to_string());
                    }
                    (_, Value::String(s)) => {
                        details.insert(k, self.transport().redact(&s));
                    }
                    (_, v @ (Value::Bool(_) | Value::Number(_))) => {
                        details.insert(k, v.to_string());
                    }
                    _ => {}
                }
            }
        }
        let status = status.unwrap_or_else(|| if ok { "ok" } else { "unhealthy" }.to_owned());
        let word_ok = !UNHEALTHY_WORDS.contains(&status.to_ascii_lowercase().as_str());
        Ok(HealthStatus {
            healthy: ok && word_ok && healthy_flag,
            status,
            details,
        })
    }

    /// Liveness check without credentials: GET on the query root (the UI, which is also the
    /// stack catalog health target). When [`JaegerConfig::health_url`] is set, the
    /// healthcheckv2 result is added to `details` with the prefix `healthcheck.`, and an
    /// unhealthy result there turns the status into `degraded`. Never fails for an unhealthy or
    /// unreachable service; only `CANCELLED` and `CONFIG` are returned as errors.
    pub async fn health(&self) -> Result<HealthStatus> {
        let mut hs = self.probe("/", "text/html").await?;
        let Some(url) = self.config().health_url() else {
            return Ok(hs);
        };
        if !hs.healthy {
            return Ok(hs);
        }
        let detail = self.probe(url.as_str(), "application/json").await?;
        hs.details
            .insert("healthcheck.status".to_owned(), detail.status.clone());
        for (k, v) in detail.details {
            hs.details.insert(format!("healthcheck.{k}"), v);
        }
        if !detail.healthy {
            hs.healthy = false;
            hs.status = "degraded".to_owned();
        }
        Ok(hs)
    }

    /// Authenticated round trip (`GET /api/v3/services`); returns the latency.
    /// `AUTHENTICATION` on bad credentials behind an auth proxy.
    pub async fn ping(&self) -> Result<Duration> {
        let started = Instant::now();
        let mut req = self.transport().get("/api/v3/services").operation("ping");
        if let Some(token) = self.config().token() {
            req = req.bearer(token);
        }
        req.send().await?;
        Ok(started.elapsed())
    }

    async fn get_optional(&self, url: &str, accept: &str) -> Result<Option<Response>> {
        let mut req = self
            .transport()
            .get(url)
            .operation("serverVersion")
            .header("accept", accept);
        if let Some(token) = self.config().token() {
            req = req.bearer(token);
        }
        match req.send().await {
            Ok(resp) => Ok(Some(resp)),
            Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The Jaeger version without the leading `v` (for example `2.23.0`). It reads
    /// `JAEGER_VERSION` from the UI index page and falls back to the `target_info` metric of
    /// [`JaegerConfig::metrics_url`]. `UNSUPPORTED` when neither exposes a version (for example
    /// when the UI is disabled); a 404 is not `NOT_FOUND`.
    pub async fn server_version(&self) -> Result<String> {
        if let Some(resp) = self.get_optional("/", "text/html").await? {
            if let Some(v) = parse_ui_version(&String::from_utf8_lossy(resp.bytes())) {
                return Ok(v);
            }
        }
        if let Some(url) = self.config().metrics_url() {
            if let Some(resp) = self.get_optional(url.as_str(), "text/plain").await? {
                if let Some(v) = parse_target_info_version(&String::from_utf8_lossy(resp.bytes())) {
                    return Ok(v);
                }
            }
        }
        Err(Error::unsupported(
            "serverVersion: the server exposes no version",
        ))
    }

    /// The remote sampling strategy of `service` from [`JaegerConfig::sampling_url`]
    /// (`GET <sampling_url>?service=<service>`). An empty service is `VALIDATION`.
    pub async fn sampling_strategy(&self, service: &str) -> Result<SamplingStrategy> {
        fetch_strategy(
            self.transport(),
            self.config().sampling_url(),
            self.config().token(),
            service,
        )
        .await
    }

    /// A [`JaegerRemoteSampler`] for `service` that polls [`JaegerConfig::sampling_url`] every
    /// `poll_interval` (zero = 1 minute) with this client's transport settings. Polling starts
    /// at once; [`JaegerClient::close`] stops every sampler created here.
    pub fn remote_sampler(
        &self,
        service: &str,
        poll_interval: Duration,
    ) -> Result<JaegerRemoteSampler> {
        self.remote_sampler_with(RemoteSamplerOptions::new(service).poll_interval(poll_interval))
    }

    /// Like [`remote_sampler`](Self::remote_sampler) with full options; the sampling URL,
    /// token and transport settings default to the client's.
    pub fn remote_sampler_with(&self, opts: RemoteSamplerOptions) -> Result<JaegerRemoteSampler> {
        if self.is_closed() {
            return Err(Error::cancelled("sampling: client is closed"));
        }
        let mut opts = opts;
        if opts.token.is_none() {
            opts.token = self.config().token().cloned();
        }
        let url = match &opts.sampling_url {
            Some(raw) => url::Url::parse(raw)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
                .ok_or_else(|| {
                    Error::config("sampling: samplingUrl must be an absolute http or https URL")
                })?,
            None => self.config().sampling_url().clone(),
        };
        let sampler = match opts.http_config.clone() {
            Some(common) => {
                let transport = Arc::new(HttpTransport::new(&common)?);
                JaegerRemoteSampler::create(transport, true, url, opts)?
            }
            None => {
                JaegerRemoteSampler::create(Arc::clone(&self.inner.transport), false, url, opts)?
            }
        };
        self.inner
            .samplers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sampler.clone());
        Ok(sampler)
    }

    /// Stops every remote sampler created by the client, runs the registered close work and
    /// closes the transport. Idempotent.
    pub async fn close(&self) {
        let samplers: Vec<JaegerRemoteSampler> = std::mem::take(
            &mut *self
                .inner
                .samplers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for s in samplers {
            s.close().await;
        }
        self.inner.on_close.close().await;
        self.inner.transport.close();
    }

    /// Whether [`JaegerClient::close`] was called.
    pub fn is_closed(&self) -> bool {
        self.inner.transport.is_closed()
    }
}

/// Parses `JAEGER_VERSION = {"gitVersion":"v2.23.0",…};` from the UI index page.
pub(crate) fn parse_ui_version(html: &str) -> Option<String> {
    html.match_indices("JAEGER_VERSION").find_map(|(idx, key)| {
        let rest = html[idx + key.len()..].trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        if !rest.starts_with('{') {
            return None;
        }
        let obj = rest[..rest.find(';')?].trim_end();
        let info: Value = serde_json::from_str(obj).ok()?;
        let v = info.get("gitVersion")?.as_str()?;
        let v = v.strip_prefix('v').unwrap_or(v);
        (!v.is_empty()).then(|| v.to_owned())
    })
}

/// Parses `target_info{…,service_version="v2.23.0",…}` from Prometheus text.
pub(crate) fn parse_target_info_version(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let labels = line.strip_prefix("target_info{")?;
        let labels = &labels[..labels.find('}')?];
        let start = labels.find("service_version=\"")? + "service_version=\"".len();
        let value = &labels[start..];
        let value = &value[..value.find('"')?];
        let value = value.strip_prefix('v').unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsers() {
        assert_eq!(
            parse_ui_version(r#"<script>JAEGER_VERSION = {"gitVersion":"v2.23.0"};</script>"#),
            Some("2.23.0".to_owned())
        );
        assert_eq!(parse_ui_version("JAEGER_VERSION = DEFAULT;"), None);
        assert_eq!(parse_ui_version("JAEGER_VERSION {}"), None);
        assert_eq!(
            parse_ui_version(r#"JAEGER_VERSION = {"gitVersion":""};"#),
            None
        );
        assert_eq!(parse_ui_version(r#"JAEGER_VERSION = {"x":1}"#), None);
        assert_eq!(parse_ui_version("nothing"), None);
        assert_eq!(
            parse_target_info_version(
                "# HELP x\ntarget_info{service_name=\"jaeger\",service_version=\"v2.23.0\"} 1\n"
            ),
            Some("2.23.0".to_owned())
        );
        assert_eq!(parse_target_info_version("target_info{a=\"b\"} 1"), None);
        assert_eq!(parse_target_info_version("up 1"), None);
    }
}
