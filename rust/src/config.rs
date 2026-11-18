// Skeleton created by sdk-kit 0.1.0; owned by this repository (edit freely).
//! `JaegerConfig`: common settings (SPEC §2.2) plus the Jaeger endpoints (query, remote
//! sampling, health detail, version metrics) and the optional bearer token.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::core::redact::redact_url;
use crate::core::{
    CommonConfig, CommonConfigBuilder, EnvReader, Error, Hooks, Result, RetryPolicy, Secret,
    TlsConfig,
};

/// Query endpoint (UI and HTTP query API, port 16686) used when neither the builder nor the
/// environment sets one.
pub const DEFAULT_URL: &str = "http://127.0.0.1:16686";

/// Remote sampling endpoint (port 5778) used when neither the builder nor the environment sets
/// one.
pub const DEFAULT_SAMPLING_URL: &str = "http://127.0.0.1:5778/sampling";

/// Vendor variables checked (in order) after `GROW_JAEGER_URL`.
const VENDOR_URL_VARS: &[&str] = &["JAEGER_QUERY_URL"];
/// Vendor variables checked (in order) after `GROW_JAEGER_TOKEN`.
const VENDOR_TOKEN_VARS: &[&str] = &[];
/// Vendor variables checked (in order) after `GROW_JAEGER_SAMPLING_URL` (the legacy Jaeger
/// client name).
const VENDOR_SAMPLING_VARS: &[&str] = &["JAEGER_SAMPLING_ENDPOINT"];

/// Immutable client configuration; build it with [`JaegerConfig::builder`] or
/// [`JaegerConfig::from_env`]. `Debug` output redacts every credential.
///
/// Environment variables read by [`JaegerConfig::from_env`] (explicit value > `GROW_JAEGER_*` >
/// vendor variables > default):
///
/// | Setting | Variables | Default |
/// | - | - | - |
/// | `url` | `GROW_JAEGER_URL`, `JAEGER_QUERY_URL` | [`DEFAULT_URL`] |
/// | `token` | `GROW_JAEGER_TOKEN` | none |
/// | `sampling_url` | `GROW_JAEGER_SAMPLING_URL`, `JAEGER_SAMPLING_ENDPOINT` | [`DEFAULT_SAMPLING_URL`] |
/// | `health_url` | `GROW_JAEGER_HEALTH_URL` | none |
/// | `metrics_url` | `GROW_JAEGER_METRICS_URL` | none |
///
/// plus the common `GROW_JAEGER_CONNECT_TIMEOUT`, `REQUEST_TIMEOUT`, `TOTAL_TIMEOUT`,
/// `MAX_RETRIES`, `CA_CERT`, `CLIENT_CERT`, `CLIENT_KEY`, `INSECURE_SKIP_VERIFY` and `PROXY`. The
/// span exporter has its own settings, see
/// [`tracing_config_from_env`](crate::tracing_config_from_env).
#[derive(Clone)]
pub struct JaegerConfig {
    common: CommonConfig,
    token: Option<Secret>,
    sampling_url: url::Url,
    health_url: Option<url::Url>,
    metrics_url: Option<url::Url>,
}

impl std::fmt::Debug for JaegerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |u: &url::Url| redact_url(u.as_str());
        f.debug_struct("JaegerConfig")
            .field("common", &self.common)
            .field("token", &self.token)
            .field("sampling_url", &redact(&self.sampling_url))
            .field("health_url", &self.health_url.as_ref().map(redact))
            .field("metrics_url", &self.metrics_url.as_ref().map(redact))
            .finish()
    }
}

fn parse_endpoint(name: &str, raw: &str) -> Result<url::Url> {
    url::Url::parse(raw)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
        .ok_or_else(|| Error::config(format!("{name} must be an absolute http or https URL")))
}

impl JaegerConfig {
    /// A builder with nothing set.
    pub fn builder() -> JaegerConfigBuilder {
        JaegerConfigBuilder::default()
    }

    /// Configuration from the process environment
    /// (explicit > `GROW_JAEGER_*` > vendor variables > defaults).
    pub fn from_env() -> Result<Self> {
        Self::builder().build_from_env()
    }

    /// Settings shared by every Grow Codes SDK.
    pub fn common(&self) -> &CommonConfig {
        &self.common
    }

    /// Endpoint base URL.
    pub fn url(&self) -> &url::Url {
        self.common.url()
    }

    /// Bearer token sent on query and sampling requests, when configured. Stock Jaeger has no
    /// auth; use it behind a reverse proxy or with bearer token propagation.
    pub fn token(&self) -> Option<&Secret> {
        self.token.as_ref()
    }

    /// Remote sampling endpoint (ends in `/sampling`).
    pub fn sampling_url(&self) -> &url::Url {
        &self.sampling_url
    }

    /// Optional healthcheckv2 endpoint (port 13133, `/status`) that `health()` adds as detail.
    pub fn health_url(&self) -> Option<&url::Url> {
        self.health_url.as_ref()
    }

    /// Optional Prometheus endpoint (port 8888, `/metrics`) that `server_version()` falls back to.
    pub fn metrics_url(&self) -> Option<&url::Url> {
        self.metrics_url.as_ref()
    }

    /// Checks the service rules (common rules and URLs are checked when building).
    pub fn validate(&self) -> Result<()> {
        if self.token.as_ref().is_some_and(|t| t.is_empty()) {
            return Err(Error::config("token must not be empty when set"));
        }
        Ok(())
    }
}

/// Builder for [`JaegerConfig`]. Values set here win over the environment.
#[derive(Debug, Clone, Default)]
pub struct JaegerConfigBuilder {
    common: CommonConfigBuilder,
    token: Option<Secret>,
    sampling_url: Option<String>,
    health_url: Option<String>,
    metrics_url: Option<String>,
}

impl JaegerConfigBuilder {
    /// Query endpoint base URL (default [`DEFAULT_URL`]). A reverse-proxy base path is kept
    /// (`https://host/jaeger` calls `https://host/jaeger/api/v3/...`).
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.common = self.common.url(url);
        self
    }
    /// Bearer token for query and sampling requests (redacted everywhere).
    pub fn token(mut self, token: impl Into<Secret>) -> Self {
        self.token = Some(token.into());
        self
    }
    /// Remote sampling endpoint (default [`DEFAULT_SAMPLING_URL`]).
    pub fn sampling_url(mut self, url: impl Into<String>) -> Self {
        self.sampling_url = Some(url.into());
        self
    }
    /// healthcheckv2 endpoint (for example `http://jaeger:13133/status`) added to `health()`.
    pub fn health_url(mut self, url: impl Into<String>) -> Self {
        self.health_url = Some(url.into());
        self
    }
    /// Prometheus endpoint (for example `http://jaeger:8888/metrics`) used by
    /// `server_version()` when the UI exposes no version.
    pub fn metrics_url(mut self, url: impl Into<String>) -> Self {
        self.metrics_url = Some(url.into());
        self
    }
    /// Connect timeout (default 10 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.common = self.common.connect_timeout(timeout);
        self
    }
    /// Per-attempt timeout (default 30 s).
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.common = self.common.request_timeout(timeout);
        self
    }
    /// Whole-call timeout including retries (default 120 s).
    pub fn total_timeout(mut self, timeout: Duration) -> Self {
        self.common = self.common.total_timeout(timeout);
        self
    }
    /// Retry policy.
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.common = self.common.retry(retry);
        self
    }
    /// Maximum retries (default 3).
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.common = self.common.max_retries(max_retries);
        self
    }
    /// TLS settings.
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.common = self.common.tls(tls);
        self
    }
    /// Extra trusted CA certificates (PEM file).
    pub fn ca_cert_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.common = self.common.ca_cert_path(path);
        self
    }
    /// Disables TLS verification (tests only).
    pub fn insecure_skip_verify(mut self, skip: bool) -> Self {
        self.common = self.common.insecure_skip_verify(skip);
        self
    }
    /// Proxy URL (environment proxies are ignored).
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.common = self.common.proxy(proxy);
        self
    }
    /// Extra request header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.common = self.common.header(name, value);
        self
    }
    /// Text appended to the User-Agent.
    pub fn user_agent_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.common = self.common.user_agent_suffix(suffix);
        self
    }
    /// Request hooks.
    pub fn hooks(mut self, hooks: Arc<dyn Hooks>) -> Self {
        self.common = self.common.hooks(hooks);
        self
    }
    /// Edits any other common setting.
    pub fn common(mut self, f: impl FnOnce(CommonConfigBuilder) -> CommonConfigBuilder) -> Self {
        self.common = f(self.common);
        self
    }

    /// Builds without reading the environment.
    pub fn build(self) -> Result<JaegerConfig> {
        let common = self.common.build(DEFAULT_URL)?;
        let sampling_url = parse_endpoint(
            "samplingUrl",
            self.sampling_url.as_deref().unwrap_or(DEFAULT_SAMPLING_URL),
        )?;
        let health_url = self
            .health_url
            .as_deref()
            .map(|u| parse_endpoint("healthUrl", u))
            .transpose()?;
        let metrics_url = self
            .metrics_url
            .as_deref()
            .map(|u| parse_endpoint("metricsUrl", u))
            .transpose()?;
        let config = JaegerConfig {
            common,
            token: self.token,
            sampling_url,
            health_url,
            metrics_url,
        };
        config.validate()?;
        Ok(config)
    }

    /// Fills unset values from the process environment, then builds.
    pub fn build_from_env(self) -> Result<JaegerConfig> {
        self.build_with_env(&EnvReader::new())
    }

    /// Fills unset values from `env`, then builds.
    pub fn build_with_env(mut self, env: &EnvReader) -> Result<JaegerConfig> {
        self.common = self.common.apply_env(env, VENDOR_URL_VARS)?;
        if self.token.is_none() {
            self.token = env.secret("TOKEN", VENDOR_TOKEN_VARS);
        }
        if self.sampling_url.is_none() {
            self.sampling_url = env.string("SAMPLING_URL", VENDOR_SAMPLING_VARS);
        }
        if self.health_url.is_none() {
            self.health_url = env.string("HEALTH_URL", &[]);
        }
        if self.metrics_url.is_none() {
            self.metrics_url = env.string("METRICS_URL", &[]);
        }
        self.build()
    }
}
