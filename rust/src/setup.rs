// Service code (Jaeger): tracing setup with the Jaeger OTLP preset and remote sampling.
//! `init_tracing` for Jaeger: OTLP `http/protobuf` to `http://127.0.0.1:4318/v1/traces` by
//! default (or OTLP gRPC to `http://127.0.0.1:4317`), environment resolution, the exporter
//! factory, sampling presets and the remote-sampled pipeline.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::InstrumentationScope;
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfigBuilder, Sampler, SdkTracer, SdkTracerProvider};

use crate::config::JaegerConfig;
use crate::core::{EnvReader, Error, Result, TlsConfig};
use crate::sampler::{JaegerRemoteSampler, RemoteSamplerOptions};
use crate::tracing::{
    build_propagator, build_resource, init_tracing_with, AttrValue, Compression, PropagatorKind,
    Protocol, SamplerKind, TracingConfig, TracingHandle,
};

/// Default OTLP/HTTP traces endpoint of a Jaeger all-in-one or collector.
pub const DEFAULT_OTLP_HTTP_ENDPOINT: &str = "http://127.0.0.1:4318/v1/traces";
/// Default OTLP/gRPC endpoint of a Jaeger all-in-one or collector.
pub const DEFAULT_OTLP_GRPC_ENDPOINT: &str = "http://127.0.0.1:4317";
/// Protocol of the Jaeger preset: OTLP over HTTP/protobuf (works through proxies).
pub const DEFAULT_TRACING_PROTOCOL: Protocol = Protocol::HttpProtobuf;

/// A named head-sampling choice for [`TracingConfig`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplingPreset {
    /// Sampler.
    pub sampler: SamplerKind,
    /// Ratio for the ratio samplers.
    pub ratio: f64,
}

impl SamplingPreset {
    /// Records every trace (`always_on`).
    pub const DEVELOPMENT: SamplingPreset = SamplingPreset {
        sampler: SamplerKind::AlwaysOn,
        ratio: 1.0,
    };
    /// Keeps 10 % of new traces and follows the parent decision (`parentbased_traceidratio`
    /// 0.1).
    pub const PRODUCTION: SamplingPreset = SamplingPreset {
        sampler: SamplerKind::ParentBasedTraceIdRatio,
        ratio: 0.1,
    };
    /// Records new traces and follows the parent decision (`parentbased_always_on`, the
    /// OpenTelemetry default).
    pub const FOLLOW_PARENT: SamplingPreset = SamplingPreset {
        sampler: SamplerKind::ParentBasedAlwaysOn,
        ratio: 1.0,
    };
    /// Records nothing (`always_off`).
    pub const OFF: SamplingPreset = SamplingPreset {
        sampler: SamplerKind::AlwaysOff,
        ratio: 1.0,
    };

    /// A `parentbased_traceidratio` preset; a ratio ≤ 0 gives `parentbased_always_off`, ≥ 1
    /// gives [`FOLLOW_PARENT`](Self::FOLLOW_PARENT).
    pub fn ratio(ratio: f64) -> SamplingPreset {
        if ratio <= 0.0 || ratio.is_nan() {
            SamplingPreset {
                sampler: SamplerKind::ParentBasedAlwaysOff,
                ratio: 1.0,
            }
        } else if ratio >= 1.0 {
            Self::FOLLOW_PARENT
        } else {
            SamplingPreset {
                sampler: SamplerKind::ParentBasedTraceIdRatio,
                ratio,
            }
        }
    }

    /// `config` with this preset's sampler and ratio.
    pub fn apply(self, config: TracingConfig) -> TracingConfig {
        config.with_sampler(self.sampler, self.ratio)
    }
}

/// A `jaeger_remote` sampler requested through `OTEL_TRACES_SAMPLER`.
#[derive(Debug, Clone, PartialEq)]
struct EnvRemote {
    parent_based: bool,
    endpoint: Option<String>,
    poll_interval: Option<Duration>,
    initial_rate: Option<f64>,
}

fn otel(env: &EnvReader, names: &[&str]) -> Option<(String, String)> {
    // The empty suffix maps to the bare prefix, which is never set.
    env.lookup("", names)
}

/// Parses `k=v,k2=v2` (OTel list format; keys and values are URL-decoded and trimmed).
fn parse_key_values(raw: &str, variable: &str) -> Result<Vec<(String, String)>> {
    let bad = || Error::config(format!("{variable} must be a list of key=value pairs"));
    let mut out = Vec::new();
    for part in raw.split(',').filter(|p| !p.trim().is_empty()) {
        let (k, v) = part.split_once('=').ok_or_else(bad)?;
        let decode = |s: &str| {
            url::form_urlencoded::parse(format!("x={}", s.trim()).as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let key = decode(k);
        if key.is_empty() {
            return Err(bad());
        }
        out.push((key, decode(v)));
    }
    Ok(out)
}

fn has_path(endpoint: &str) -> bool {
    url::Url::parse(endpoint)
        .map(|u| u.path() != "/" && !u.path().is_empty())
        .unwrap_or(true)
}

/// The exporter URL for a protocol: for `http/protobuf`, an endpoint without a path (a base such
/// as `http://jaeger:4318`) gets `/v1/traces`; any other endpoint is used as is. Without an
/// endpoint, the Jaeger default for the protocol.
pub fn resolve_endpoint(protocol: Protocol, endpoint: Option<&str>) -> String {
    match (protocol, endpoint.filter(|e| !e.is_empty())) {
        (Protocol::Grpc, Some(e)) => e.to_owned(),
        (Protocol::Grpc, None) => DEFAULT_OTLP_GRPC_ENDPOINT.to_owned(),
        (_, Some(e)) if has_path(e) => e.to_owned(),
        (_, Some(e)) => format!("{}/v1/traces", e.trim_end_matches('/')),
        (_, None) => DEFAULT_OTLP_HTTP_ENDPOINT.to_owned(),
    }
}

fn env_endpoint(env: &EnvReader, protocol: Protocol) -> Option<String> {
    let specific = if protocol == Protocol::Grpc {
        "OTLP_GRPC_ENDPOINT"
    } else {
        "OTLP_HTTP_ENDPOINT"
    };
    if let Some(v) = env.string(specific, &[]) {
        return Some(v);
    }
    if let Some(v) = env.string("OTLP_ENDPOINT", &["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"]) {
        return Some(v);
    }
    let (_, base) = otel(env, &["OTEL_EXPORTER_OTLP_ENDPOINT"])?;
    Some(if protocol == Protocol::Grpc {
        base
    } else {
        format!("{}/v1/traces", base.trim_end_matches('/'))
    })
}

fn sampler_from_env(env: &EnvReader, config: &mut TracingConfig) -> Result<Option<EnvRemote>> {
    let Some((variable, raw)) = otel(env, &["OTEL_TRACES_SAMPLER"]) else {
        return Ok(None);
    };
    let name = raw.trim().to_ascii_lowercase();
    if name == "jaeger_remote" || name == "parentbased_jaeger_remote" {
        let mut remote = EnvRemote {
            parent_based: name.starts_with("parentbased_"),
            endpoint: None,
            poll_interval: None,
            initial_rate: None,
        };
        if let Some((arg_var, arg)) = otel(env, &["OTEL_TRACES_SAMPLER_ARG"]) {
            let bad = || {
                Error::config(format!(
                    "{arg_var} must be endpoint=<url>,pollingIntervalMs=<ms>,initialSamplingRate=<0..1>"
                ))
            };
            for (k, v) in parse_key_values(&arg, &arg_var)? {
                match k.as_str() {
                    "endpoint" => remote.endpoint = Some(v),
                    "pollingIntervalMs" => {
                        let ms: u64 = v.parse().map_err(|_| bad())?;
                        remote.poll_interval = Some(Duration::from_millis(ms));
                    }
                    "initialSamplingRate" => {
                        let r: f64 = v.parse().map_err(|_| bad())?;
                        if !(0.0..=1.0).contains(&r) {
                            return Err(bad());
                        }
                        remote.initial_rate = Some(r);
                    }
                    _ => {}
                }
            }
        }
        return Ok(Some(remote));
    }
    let kind: SamplerKind = name
        .parse()
        .map_err(|_| Error::config(format!("{variable} names an unknown sampler")))?;
    let mut ratio = 1.0;
    if matches!(
        kind,
        SamplerKind::TraceIdRatio | SamplerKind::ParentBasedTraceIdRatio
    ) {
        if let Some((arg_var, arg)) = otel(env, &["OTEL_TRACES_SAMPLER_ARG"]) {
            ratio = arg
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or_else(|| {
                    Error::config(format!("{arg_var} must be a number between 0 and 1"))
                })?;
        }
    }
    config.sampler = kind;
    config.sampler_ratio = ratio;
    Ok(None)
}

fn resolve(config: TracingConfig, env: &EnvReader) -> Result<(TracingConfig, Option<EnvRemote>)> {
    let mut c = config;
    if c.protocol != Protocol::Grpc {
        c.protocol = match env.lookup(
            "OTLP_PROTOCOL",
            &[
                "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
                "OTEL_EXPORTER_OTLP_PROTOCOL",
            ],
        ) {
            Some((variable, v)) => match v.trim().to_ascii_lowercase().as_str() {
                "grpc" => Protocol::Grpc,
                "http/protobuf" => Protocol::HttpProtobuf,
                _ => {
                    return Err(Error::config(format!(
                        "{variable} must be grpc or http/protobuf"
                    )))
                }
            },
            None => DEFAULT_TRACING_PROTOCOL,
        };
    }
    let endpoint = c.endpoint.clone().or_else(|| env_endpoint(env, c.protocol));
    c.endpoint = Some(resolve_endpoint(c.protocol, endpoint.as_deref()));
    if c.headers.is_empty() {
        if let Some((variable, v)) = env.lookup(
            "OTLP_HEADERS",
            &[
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
                "OTEL_EXPORTER_OTLP_HEADERS",
            ],
        ) {
            c.headers = parse_key_values(&v, &variable)?.into_iter().collect();
        }
    }
    if c.compression == Compression::Gzip {
        if let Some((variable, v)) = env.lookup(
            "OTLP_COMPRESSION",
            &[
                "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
                "OTEL_EXPORTER_OTLP_COMPRESSION",
            ],
        ) {
            c.compression = match v.trim().to_ascii_lowercase().as_str() {
                "gzip" => Compression::Gzip,
                "none" => Compression::None,
                _ => return Err(Error::config(format!("{variable} must be gzip or none"))),
            };
        }
    }
    if c.tls.is_none() {
        let path = |names: (&str, &[&str])| env.string(names.0, names.1).map(PathBuf::from);
        let ca = path((
            "OTLP_CERTIFICATE",
            &[
                "GROW_JAEGER_OTLP_CA_CERT",
                "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE",
                "OTEL_EXPORTER_OTLP_CERTIFICATE",
            ],
        ));
        let cert = path((
            "OTLP_CLIENT_CERTIFICATE",
            &[
                "OTEL_EXPORTER_OTLP_TRACES_CLIENT_CERTIFICATE",
                "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE",
            ],
        ));
        let key = path((
            "OTLP_CLIENT_KEY",
            &[
                "OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY",
                "OTEL_EXPORTER_OTLP_CLIENT_KEY",
            ],
        ));
        if ca.is_some() || cert.is_some() || key.is_some() {
            c.tls = Some(TlsConfig {
                ca_cert_path: ca,
                client_cert_path: cert,
                client_key_path: key,
                ..TlsConfig::default()
            });
        }
    }
    let mut resource_service_name = None;
    if let Some((variable, v)) = otel(env, &["OTEL_RESOURCE_ATTRIBUTES"]) {
        for (k, val) in parse_key_values(&v, &variable)? {
            if k == "service.name" {
                resource_service_name = Some(val);
                continue;
            }
            c.resource_attributes
                .entry(k)
                .or_insert(AttrValue::String(val));
        }
    }
    if c.service_name.trim().is_empty() {
        if let Some(name) = env
            .string("SERVICE_NAME", &["OTEL_SERVICE_NAME"])
            .or(resource_service_name)
        {
            c.service_name = name;
        }
    }
    let mut remote = None;
    if c.sampler == SamplerKind::ParentBasedAlwaysOn && c.sampler_ratio == 1.0 {
        remote = sampler_from_env(env, &mut c)?;
    }
    if c.propagators == [PropagatorKind::TraceContext, PropagatorKind::Baggage] {
        if let Some((_, v)) = otel(env, &["OTEL_PROPAGATORS"]) {
            let list: Vec<PropagatorKind> = v
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty() && *p != "none")
                .map(str::parse)
                .collect::<Result<_>>()?;
            c.propagators = list;
        }
    }
    Ok((c, remote))
}

/// Fills every unset field of `config` from `env` and applies the Jaeger preset (protocol
/// `http/protobuf`, endpoint per protocol, always set in the result). Precedence: explicit >
/// `GROW_JAEGER_OTLP_*` > `OTEL_EXPORTER_OTLP_TRACES_*` > `OTEL_EXPORTER_OTLP_*` > default.
/// A field still at its [`TracingConfig::new`] default counts as unset.
///
/// | Field | Variables |
/// | - | - |
/// | protocol | `GROW_JAEGER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, `OTEL_EXPORTER_OTLP_PROTOCOL` |
/// | endpoint | `GROW_JAEGER_OTLP_GRPC_ENDPOINT` or `GROW_JAEGER_OTLP_HTTP_ENDPOINT` (by protocol), `GROW_JAEGER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (as is), `OTEL_EXPORTER_OTLP_ENDPOINT` (HTTP: `/v1/traces` appended) |
/// | headers | `GROW_JAEGER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, `OTEL_EXPORTER_OTLP_HEADERS` |
/// | compression | `GROW_JAEGER_OTLP_COMPRESSION`, `OTEL_EXPORTER_OTLP_TRACES_COMPRESSION`, `OTEL_EXPORTER_OTLP_COMPRESSION` |
/// | TLS CA | `GROW_JAEGER_OTLP_CERTIFICATE`, `GROW_JAEGER_OTLP_CA_CERT`, `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CERTIFICATE` |
/// | TLS client | `GROW_JAEGER_OTLP_CLIENT_CERTIFICATE` / `_CLIENT_KEY`, then the `OTEL_EXPORTER_OTLP_(TRACES_)CLIENT_*` names |
/// | service name | `GROW_JAEGER_SERVICE_NAME`, `OTEL_SERVICE_NAME` |
/// | resource | `OTEL_RESOURCE_ATTRIBUTES` (explicit keys win) |
/// | sampler | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` (`jaeger_remote` is handled by [`init_tracing`]) |
/// | propagators | `OTEL_PROPAGATORS` |
///
/// A present but invalid value is `CONFIG` naming the variable, never its value.
pub fn tracing_config_from_env(config: TracingConfig, env: &EnvReader) -> Result<TracingConfig> {
    resolve(config, env).map(|(c, _)| c)
}

fn read_pem(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        Error::config(format!(
            "tracing: TLS file {} cannot be read",
            path.display()
        ))
        .with_source(e)
    })
}

fn identity_pem(tls: &TlsConfig) -> Result<Option<Vec<u8>>> {
    match (&tls.client_cert_path, &tls.client_key_path) {
        (Some(cert), Some(key)) => {
            let mut pem = read_pem(cert)?;
            pem.push(b'\n');
            pem.extend(read_pem(key)?);
            Ok(Some(pem))
        }
        (None, None) => Ok(None),
        _ => Err(Error::config(
            "tracing: client certificate and client key must be given together",
        )),
    }
}

fn ca_pem(tls: &TlsConfig) -> Result<Option<Vec<u8>>> {
    match (&tls.ca_cert_path, &tls.ca_cert_pem) {
        (Some(path), _) => read_pem(path).map(Some),
        (None, Some(pem)) => Ok(Some(pem.as_bytes().to_vec())),
        (None, None) => Ok(None),
    }
}

/// Builds the official OTLP span exporter for Jaeger from a config (the exporter factory of
/// [`init_tracing_with`]): `http/protobuf` posts protobuf to the resolved endpoint (see
/// [`resolve_endpoint`]), `grpc` uses tonic. Headers, compression, timeout and TLS (CA, client
/// certificate, `insecure_skip_verify` over HTTP) come from the config. `native` is `CONFIG`
/// (Jaeger ingests OTLP), and so is `insecure_skip_verify` over gRPC (tonic cannot skip
/// verification). The environment is not read here; see [`tracing_config_from_env`].
pub fn jaeger_exporter(config: &TracingConfig) -> Result<SpanExporter> {
    let endpoint = resolve_endpoint(config.protocol, config.endpoint.as_deref());
    let tls = config.tls.clone().unwrap_or_default();
    let built = match config.protocol {
        Protocol::Native => {
            return Err(Error::config(
                "tracing: Jaeger has no native protocol; use grpc or http/protobuf",
            ))
        }
        Protocol::HttpProtobuf => {
            let mut b = SpanExporter::builder()
                .with_http()
                .with_endpoint(endpoint)
                .with_timeout(config.batch.export_timeout)
                .with_headers(
                    config
                        .headers
                        .clone()
                        .into_iter()
                        .collect::<HashMap<_, _>>(),
                );
            if config.compression == Compression::Gzip {
                b = b.with_compression(opentelemetry_otlp::Compression::Gzip);
            }
            if tls != TlsConfig::default() {
                let mut client = reqwest::Client::builder().timeout(config.batch.export_timeout);
                if let Some(ca) = ca_pem(&tls)? {
                    for cert in reqwest::Certificate::from_pem_bundle(&ca).map_err(|e| {
                        Error::config("tracing: the CA certificate is not valid PEM").with_source(e)
                    })? {
                        client = client.add_root_certificate(cert);
                    }
                }
                if let Some(pem) = identity_pem(&tls)? {
                    let id = reqwest::Identity::from_pem(&pem).map_err(|e| {
                        Error::config("tracing: the client certificate or key is not valid PEM")
                            .with_source(e)
                    })?;
                    client = client.identity(id);
                }
                if tls.insecure_skip_verify {
                    client = client.danger_accept_invalid_certs(true);
                }
                let client = client.build().map_err(|e| {
                    Error::config("tracing: cannot build the HTTP client").with_source(e)
                })?;
                b = b.with_http_client(client);
            }
            b.build()
        }
        Protocol::Grpc => {
            use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
            if tls.insecure_skip_verify {
                return Err(Error::config(
                    "tracing: insecureSkipVerify is not supported with OTLP gRPC in Rust; use http/protobuf",
                ));
            }
            let mut headers = http::HeaderMap::new();
            for (k, v) in &config.headers {
                let name = http::header::HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes())
                    .map_err(|_| Error::config(format!("invalid tracing header name {k:?}")))?;
                let value = http::header::HeaderValue::from_str(v).map_err(|_| {
                    Error::config(format!("invalid value for tracing header {k:?}"))
                })?;
                headers.insert(name, value);
            }
            let mut b = SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint.clone())
                .with_timeout(config.batch.export_timeout)
                .with_metadata(MetadataMap::from_headers(headers));
            if config.compression == Compression::Gzip {
                b = b.with_compression(opentelemetry_otlp::Compression::Gzip);
            }
            let ca = ca_pem(&tls)?;
            let identity = identity_pem(&tls)?;
            if endpoint.starts_with("https:") || ca.is_some() || identity.is_some() {
                b = grpc_tls(b, &endpoint, &tls, ca)?;
            }
            b.build()
        }
    };
    built.map_err(|e| Error::config(format!("tracing: cannot build the OTLP exporter: {e}")))
}

type TonicBuilder =
    opentelemetry_otlp::SpanExporterBuilder<opentelemetry_otlp::TonicExporterBuilderSet>;

/// TLS for the gRPC exporter (crate feature `otlp-tls`, on by default).
#[cfg(feature = "otlp-tls")]
fn grpc_tls(
    b: TonicBuilder,
    endpoint: &str,
    tls: &TlsConfig,
    ca: Option<Vec<u8>>,
) -> Result<TonicBuilder> {
    use opentelemetry_otlp::tonic_types::transport::{Certificate, ClientTlsConfig, Identity};
    let mut t = ClientTlsConfig::new().with_native_roots();
    if let Some(ca) = ca {
        t = t.ca_certificate(Certificate::from_pem(ca));
    }
    if let (Some(cert), Some(key)) = (&tls.client_cert_path, &tls.client_key_path) {
        t = t.identity(Identity::from_pem(read_pem(cert)?, read_pem(key)?));
    }
    if let Ok(u) = url::Url::parse(endpoint) {
        if let Some(host) = u.host_str() {
            t = t.domain_name(host.to_owned());
        }
    }
    Ok(b.with_tls_config(t))
}

/// Without the crate feature `otlp-tls` the gRPC exporter cannot use TLS: `CONFIG`.
#[cfg(not(feature = "otlp-tls"))]
fn grpc_tls(
    _b: TonicBuilder,
    _endpoint: &str,
    _tls: &TlsConfig,
    _ca: Option<Vec<u8>>,
) -> Result<TonicBuilder> {
    Err(Error::config(
        "tracing: TLS settings for grpc need the crate feature \"otlp-tls\"; use http/protobuf",
    ))
}

/// Remote sampling for [`JaegerTracingOptions`].
#[derive(Debug, Clone)]
pub enum RemoteSampling {
    /// An existing sampler (not closed by the handle).
    Sampler(JaegerRemoteSampler),
    /// Options for a sampler the handle creates and closes on shutdown. An empty
    /// `service_name` defaults to the tracing service name.
    Options(Box<RemoteSamplerOptions>),
}

/// Options of [`init_tracing_with_options`].
#[derive(Debug, Clone)]
pub struct JaegerTracingOptions {
    /// Environment beneath the explicit config (default: the process environment).
    pub env: Option<EnvReader>,
    /// Ignore the environment completely (default `false`).
    pub ignore_env: bool,
    /// Use Jaeger remote sampling instead of `config.sampler`.
    pub remote_sampling: Option<RemoteSampling>,
    /// Wrap the remote sampler in a parent-based sampler (default `true`).
    pub parent_based: bool,
    /// Settings (sampling URL, token, TLS, timeouts) of a remote sampler started for
    /// `OTEL_TRACES_SAMPLER=jaeger_remote`. Default: [`JaegerConfig`] from the same environment
    /// (`GROW_JAEGER_SAMPLING_URL`, `JAEGER_SAMPLING_ENDPOINT`, `GROW_JAEGER_TOKEN`, …).
    pub sampler_config: Option<JaegerConfig>,
}

impl Default for JaegerTracingOptions {
    fn default() -> Self {
        JaegerTracingOptions {
            env: None,
            ignore_env: false,
            remote_sampling: None,
            parent_based: true,
            sampler_config: None,
        }
    }
}

/// Sets up OpenTelemetry tracing for Jaeger (SPEC §5): the environment fills every field the
/// config leaves unset (see [`tracing_config_from_env`]), the exporter is [`jaeger_exporter`].
/// `OTEL_TRACES_SAMPLER=jaeger_remote` (or `parentbased_jaeger_remote`) turns on remote
/// sampling with the URL from `OTEL_TRACES_SAMPLER_ARG` `endpoint=`, else
/// `GROW_JAEGER_SAMPLING_URL` / `JAEGER_SAMPLING_ENDPOINT` / the default. Must be called inside
/// a tokio runtime. Call [`JaegerTracingHandle::shutdown`] before exit.
pub fn init_tracing(config: TracingConfig) -> Result<JaegerTracingHandle> {
    init_tracing_with_options(config, JaegerTracingOptions::default())
}

/// [`init_tracing`] with a [`JaegerRemoteSampler`] deciding sampling (parent-based; the sampler
/// is not closed by the handle).
pub fn init_tracing_with_remote_sampler(
    config: TracingConfig,
    sampler: JaegerRemoteSampler,
) -> Result<JaegerTracingHandle> {
    init_tracing_with_options(
        config,
        JaegerTracingOptions {
            remote_sampling: Some(RemoteSampling::Sampler(sampler)),
            ..JaegerTracingOptions::default()
        },
    )
}

/// [`init_tracing`] with options.
pub fn init_tracing_with_options(
    config: TracingConfig,
    options: JaegerTracingOptions,
) -> Result<JaegerTracingHandle> {
    let env = if options.ignore_env {
        EnvReader::from_map(Vec::<(String, String)>::new())
    } else {
        options.env.clone().unwrap_or_default()
    };
    let (config, env_remote) = resolve(config, &env)?;
    config.validate()?;
    let endpoint = config.endpoint.clone().unwrap_or_default();
    let mut parent_based = options.parent_based;
    let (sampler, owned) = match (options.remote_sampling, env_remote) {
        (Some(RemoteSampling::Sampler(s)), _) => (Some(s), false),
        (Some(RemoteSampling::Options(mut o)), _) => {
            if o.service_name.trim().is_empty() {
                o.service_name = config.service_name.clone();
            }
            (Some(JaegerRemoteSampler::with_options(*o)?), true)
        }
        (None, Some(r)) => {
            parent_based = r.parent_based;
            // Sampling settings (URL, token, TLS, timeouts) from the same environment.
            let sampling = match options.sampler_config {
                Some(c) => c,
                None => JaegerConfig::builder().build_with_env(&env)?,
            };
            let mut o = RemoteSamplerOptions::new(config.service_name.clone())
                .http_config(sampling.common().clone());
            o.sampling_url = Some(
                r.endpoint
                    .unwrap_or_else(|| sampling.sampling_url().to_string()),
            );
            if let Some(token) = sampling.token() {
                o = o.token(token.clone());
            }
            if let Some(i) = r.poll_interval {
                o.poll_interval = i;
            }
            if let Some(rate) = r.initial_rate {
                o.initial_sampling_rate = rate;
            }
            (Some(JaegerRemoteSampler::with_options(o)?), true)
        }
        (None, None) => (None, false),
    };
    let Some(sampler) = sampler else {
        let handle = init_tracing_with(config, jaeger_exporter)?;
        return Ok(JaegerTracingHandle {
            pipeline: Pipeline::Standard(handle),
            endpoint,
        });
    };
    let built = build_remote_pipeline(config, sampler.clone(), parent_based);
    match built {
        Ok(p) => Ok(JaegerTracingHandle {
            pipeline: Pipeline::Remote(Arc::new(RemotePipeline {
                provider: Some(p.0),
                propagator: p.1,
                service_name: p.2,
                sampler,
                owns_sampler: owned,
                shutdown: AtomicBool::new(false),
            })),
            endpoint,
        }),
        Err(e) => {
            if owned {
                let s = sampler.clone();
                tokio::spawn(async move { s.close().await });
            }
            Err(e)
        }
    }
}

type Built = (SdkTracerProvider, Arc<TextMapCompositePropagator>, String);

fn build_remote_pipeline(
    config: TracingConfig,
    sampler: JaegerRemoteSampler,
    parent_based: bool,
) -> Result<Built> {
    tokio::runtime::Handle::try_current()
        .map_err(|_| Error::config("init_tracing must be called inside a tokio runtime"))?;
    let propagator = Arc::new(build_propagator(&config.propagators, HashMap::new())?);
    let exporter = jaeger_exporter(&config)?;
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(config.batch.max_queue_size)
        .with_max_export_batch_size(config.batch.max_export_batch_size)
        .with_scheduled_delay(config.batch.scheduled_delay)
        .with_max_export_timeout(config.batch.export_timeout)
        .build();
    let processor = BatchSpanProcessor::builder(exporter, Tokio)
        .with_batch_config(batch)
        .build();
    let builder = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(build_resource(&config));
    let provider = if parent_based {
        builder.with_sampler(Sampler::ParentBased(Box::new(sampler)))
    } else {
        builder.with_sampler(sampler)
    }
    .build();
    if config.set_global {
        opentelemetry::global::set_tracer_provider(provider.clone());
        opentelemetry::global::set_text_map_propagator(SharedPropagator(Arc::clone(&propagator)));
    }
    Ok((provider, propagator, config.service_name))
}

/// Lets the global propagator share the handle's composite propagator.
#[derive(Debug)]
struct SharedPropagator(Arc<TextMapCompositePropagator>);

impl opentelemetry::propagation::TextMapPropagator for SharedPropagator {
    fn inject_context(
        &self,
        cx: &opentelemetry::Context,
        injector: &mut dyn opentelemetry::propagation::Injector,
    ) {
        self.0.inject_context(cx, injector)
    }
    fn extract_with_context(
        &self,
        cx: &opentelemetry::Context,
        extractor: &dyn opentelemetry::propagation::Extractor,
    ) -> opentelemetry::Context {
        self.0.extract_with_context(cx, extractor)
    }
    fn fields(&self) -> opentelemetry::propagation::text_map_propagator::FieldIter<'_> {
        self.0.fields()
    }
}

struct RemotePipeline {
    provider: Option<SdkTracerProvider>,
    propagator: Arc<TextMapCompositePropagator>,
    service_name: String,
    sampler: JaegerRemoteSampler,
    owns_sampler: bool,
    shutdown: AtomicBool,
}

impl Drop for RemotePipeline {
    fn drop(&mut self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        if let Some(provider) = self.provider.take() {
            let _ = std::thread::Builder::new()
                .name("grow-tracing-shutdown".into())
                .spawn(move || {
                    let _ = provider.shutdown();
                });
        }
    }
}

impl RemotePipeline {
    fn provider(&self) -> &SdkTracerProvider {
        self.provider.as_ref().expect("provider present until drop")
    }
}

#[derive(Clone)]
enum Pipeline {
    Standard(TracingHandle),
    Remote(Arc<RemotePipeline>),
}

/// A running Jaeger tracing setup (SPEC §5 `TracingHandle` plus the endpoint and the remote
/// sampler). Cheap to clone. Without remote sampling it wraps the category
/// [`TracingHandle`] ([`tracing_handle`](Self::tracing_handle)).
#[derive(Clone)]
pub struct JaegerTracingHandle {
    pipeline: Pipeline,
    endpoint: String,
}

impl fmt::Debug for JaegerTracingHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JaegerTracingHandle")
            .field("service_name", &self.service_name())
            .field("endpoint", &crate::core::redact::redact_url(&self.endpoint))
            .field("remote_sampler", &self.remote_sampler())
            .field("shutdown", &self.is_shutdown())
            .finish()
    }
}

fn flatten<E: fmt::Display>(
    what: &str,
    joined: std::result::Result<std::result::Result<(), E>, tokio::task::JoinError>,
) -> Result<()> {
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(Error::unavailable(format!("{what} failed: {e}"))),
        Err(e) => Err(Error::unavailable(format!("{what} failed: {e}"))),
    }
}

impl JaegerTracingHandle {
    /// The category handle (`None` when remote sampling is on: that pipeline is built here).
    pub fn tracing_handle(&self) -> Option<&TracingHandle> {
        match &self.pipeline {
            Pipeline::Standard(h) => Some(h),
            Pipeline::Remote(_) => None,
        }
    }

    /// The remote sampler in use, if any.
    pub fn remote_sampler(&self) -> Option<&JaegerRemoteSampler> {
        match &self.pipeline {
            Pipeline::Standard(_) => None,
            Pipeline::Remote(p) => Some(&p.sampler),
        }
    }

    /// The exporter endpoint in use.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The OpenTelemetry tracer provider.
    pub fn tracer_provider(&self) -> SdkTracerProvider {
        match &self.pipeline {
            Pipeline::Standard(h) => h.tracer_provider(),
            Pipeline::Remote(p) => p.provider().clone(),
        }
    }

    /// The composite propagator built from the config.
    pub fn propagator(&self) -> Arc<TextMapCompositePropagator> {
        match &self.pipeline {
            Pipeline::Standard(h) => h.propagator(),
            Pipeline::Remote(p) => Arc::clone(&p.propagator),
        }
    }

    /// The configured `service.name`.
    pub fn service_name(&self) -> &str {
        match &self.pipeline {
            Pipeline::Standard(h) => h.service_name(),
            Pipeline::Remote(p) => &p.service_name,
        }
    }

    /// A tracer for an instrumentation scope.
    pub fn tracer(&self, name: impl Into<Cow<'static, str>>, version: Option<&str>) -> SdkTracer {
        match &self.pipeline {
            Pipeline::Standard(h) => h.tracer(name, version),
            Pipeline::Remote(p) => {
                let mut scope = InstrumentationScope::builder(name.into());
                if let Some(v) = version {
                    scope = scope.with_version(v.to_string());
                }
                p.provider().tracer_with_scope(scope.build())
            }
        }
    }

    /// Exports every finished span now. `None` waits without a limit.
    pub async fn force_flush(&self, timeout: Option<Duration>) -> Result<()> {
        let p = match &self.pipeline {
            Pipeline::Standard(h) => return h.force_flush(timeout).await,
            Pipeline::Remote(p) => p.provider().clone(),
        };
        let task = tokio::task::spawn_blocking(move || p.force_flush());
        let joined = match timeout {
            None => task.await,
            Some(t) => tokio::time::timeout(t, task).await.map_err(|_| {
                Error::timeout(format!("tracing flush did not finish within {t:?}"))
            })?,
        };
        flatten("tracing flush", joined)
    }

    /// Flushes and shuts the provider down, then closes a remote sampler the handle created;
    /// idempotent.
    pub async fn shutdown(&self) -> Result<()> {
        let p = match &self.pipeline {
            Pipeline::Standard(h) => return h.shutdown().await,
            Pipeline::Remote(p) => p,
        };
        if p.shutdown.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let provider = p.provider().clone();
        let joined = tokio::task::spawn_blocking(move || provider.shutdown()).await;
        if p.owns_sampler {
            p.sampler.close().await;
        }
        flatten("tracing shutdown", joined)
    }

    /// Whether [`shutdown`](Self::shutdown) was called.
    pub fn is_shutdown(&self) -> bool {
        match &self.pipeline {
            Pipeline::Standard(h) => h.is_shutdown(),
            Pipeline::Remote(p) => p.shutdown.load(Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_value_lists() {
        let kv = parse_key_values("a=1, b = x%20y ,,", "V").unwrap();
        assert_eq!(
            kv,
            vec![("a".into(), "1".into()), ("b".into(), "x y".into())]
        );
        assert!(parse_key_values("novalue", "V").is_err());
        assert!(parse_key_values("=v", "V").is_err());
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            resolve_endpoint(Protocol::HttpProtobuf, Some("http://j:4318/")),
            "http://j:4318/v1/traces"
        );
        assert_eq!(
            resolve_endpoint(Protocol::HttpProtobuf, Some("http://j:4318/custom")),
            "http://j:4318/custom"
        );
        assert_eq!(
            resolve_endpoint(Protocol::HttpProtobuf, Some("not a url")),
            "not a url"
        );
        assert_eq!(
            resolve_endpoint(Protocol::HttpProtobuf, None),
            DEFAULT_OTLP_HTTP_ENDPOINT
        );
        assert_eq!(
            resolve_endpoint(Protocol::Grpc, Some("")),
            DEFAULT_OTLP_GRPC_ENDPOINT
        );
        assert_eq!(
            resolve_endpoint(Protocol::Grpc, Some("http://j:4317")),
            "http://j:4317"
        );
    }
}
