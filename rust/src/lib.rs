// Skeleton created by sdk-kit 0.1.0; owned by this repository (edit freely).
//! # grow-jaeger
//!
//! Rust SDK for [Jaeger](https://grow-codes-packages.github.io/jaeger/) by Grow Codes.
//!
//! ```no_run
//! # async fn demo() -> grow_jaeger::Result<()> {
//! use grow_jaeger::tracing::{traced, TracingConfig, TraceSearchQuery};
//! use grow_jaeger::{init_tracing, JaegerClient, JaegerConfig};
//!
//! // Export spans to Jaeger over OTLP (http/protobuf on 4318 unless configured otherwise).
//! let tracing = init_tracing(TracingConfig::new("checkout"))?;
//! traced("charge-card", async { Ok::<_, std::io::Error>(()) }).await.ok();
//! tracing.shutdown().await?;
//!
//! // Read them back through the query API (16686).
//! let client = JaegerClient::new(JaegerConfig::from_env()?)?;
//! let summaries = client.query().search_traces(&TraceSearchQuery::service("checkout")).await?;
//! println!("{} traces", summaries.len());
//! client.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! Modules: [`JaegerClient`] (health, version, sampling strategy), [`JaegerQueryClient`]
//! (`get_trace`, `find_traces`, `search_traces`, `list_services`, `list_operations`,
//! `get_operations`, `get_dependencies`), [`init_tracing`] (OTLP preset, presets, remote
//! sampling), [`JaegerRemoteSampler`] and the [`tracing`] category helpers.
//!
//! Features: `blocking` (a blocking client), `testing` (in-memory fakes and the conformance
//! runner)
//! , `tower` and `reqwest-middleware` (tracing instrumentation)
//! .
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod core;

mod client;
mod config;
pub mod normalize;
mod query;
mod sampler;
mod setup;

#[cfg(feature = "blocking")]
#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
pub mod blocking;

#[cfg(feature = "testing")]
#[cfg_attr(docsrs, doc(cfg(feature = "testing")))]
pub mod testing;

pub mod tracing;

pub use crate::core::{Error, ErrorCode, HealthStatus, Result, RetryPolicy, Secret};

pub use crate::config::{DEFAULT_SAMPLING_URL, DEFAULT_URL};

pub use crate::query::{
    GetTraceOptions, JaegerQueryClient, ListOperationsOptions, Operation,
    DEFAULT_DEPENDENCY_LOOKBACK, DEFAULT_GET_TRACE_WINDOW, DEFAULT_SEARCH_WINDOW,
};

pub use crate::sampler::{
    parse_sampling_strategy, JaegerRemoteSampler, OperationStrategy, RateLimiter,
    RemoteSamplerOptions, SamplingStrategy, StrategyType, DEFAULT_INITIAL_SAMPLING_RATE,
    DEFAULT_SAMPLING_POLL_INTERVAL,
};

pub use crate::setup::{
    init_tracing, init_tracing_with_options, init_tracing_with_remote_sampler, jaeger_exporter,
    resolve_endpoint, tracing_config_from_env, JaegerTracingHandle, JaegerTracingOptions,
    RemoteSampling, SamplingPreset, DEFAULT_OTLP_GRPC_ENDPOINT, DEFAULT_OTLP_HTTP_ENDPOINT,
    DEFAULT_TRACING_PROTOCOL,
};

pub use crate::client::{JaegerBuildInfo, JaegerClient};

pub use crate::config::JaegerConfig;

pub use crate::config::JaegerConfigBuilder;

/// Service-prefixed alias of [`Error`].
pub type JaegerError = Error;
