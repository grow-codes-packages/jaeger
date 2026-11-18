// Skeleton created by sdk-kit 0.1.0; owned by this repository (edit freely).
//! Blocking wrapper (feature `blocking`). Each client owns a current-thread tokio runtime.
//!
//! Do not use it from inside an async runtime (calls would panic); use the async client there.

use std::future::Future;
use std::time::Duration;

use crate::core::{Error, HealthStatus, Result};
use crate::tracing::{
    DependencyLink, DependencyOptions, Trace, TraceSearchQuery, TraceSummary, WaitForTraceOptions,
};
use crate::{
    GetTraceOptions, JaegerClient as AsyncClient, JaegerConfig, ListOperationsOptions, Operation,
    SamplingStrategy,
};

/// Blocking client for Jaeger with the same methods as the async client.
#[derive(Debug)]
pub struct JaegerClient {
    inner: AsyncClient,
    runtime: tokio::runtime::Runtime,
}

impl JaegerClient {
    /// Creates a client from a configuration.
    pub fn new(config: JaegerConfig) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::config("cannot start the tokio runtime").with_source(e))?;
        let inner = {
            let _guard = runtime.enter();
            AsyncClient::new(config)?
        };
        Ok(JaegerClient { inner, runtime })
    }

    /// Creates a client from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::new(JaegerConfig::from_env()?)
    }

    /// The wrapped async client.
    pub fn async_client(&self) -> &AsyncClient {
        &self.inner
    }

    /// Runs a future of the async API to completion on this client's runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// See [`AsyncClient::health`].
    pub fn health(&self) -> Result<HealthStatus> {
        self.block_on(self.inner.health())
    }

    /// See [`AsyncClient::ping`].
    pub fn ping(&self) -> Result<Duration> {
        self.block_on(self.inner.ping())
    }

    /// See [`AsyncClient::server_version`].
    pub fn server_version(&self) -> Result<String> {
        self.block_on(self.inner.server_version())
    }

    /// See [`AsyncClient::sampling_strategy`].
    pub fn sampling_strategy(&self, service: &str) -> Result<SamplingStrategy> {
        self.block_on(self.inner.sampling_strategy(service))
    }

    /// See [`JaegerQueryClient::get_trace`](crate::JaegerQueryClient::get_trace).
    pub fn get_trace(&self, trace_id: &str) -> Result<Option<Trace>> {
        self.block_on(self.inner.query().get_trace(trace_id))
    }

    /// See [`JaegerQueryClient::get_trace_with`](crate::JaegerQueryClient::get_trace_with).
    pub fn get_trace_with(&self, trace_id: &str, opts: GetTraceOptions) -> Result<Option<Trace>> {
        self.block_on(self.inner.query().get_trace_with(trace_id, opts))
    }

    /// See [`JaegerQueryClient::find_traces`](crate::JaegerQueryClient::find_traces).
    pub fn find_traces(&self, query: &TraceSearchQuery) -> Result<Vec<Trace>> {
        self.block_on(self.inner.query().find_traces(query))
    }

    /// See [`JaegerQueryClient::search_traces`](crate::JaegerQueryClient::search_traces).
    pub fn search_traces(&self, query: &TraceSearchQuery) -> Result<Vec<TraceSummary>> {
        self.block_on(self.inner.query().search_traces(query))
    }

    /// See [`JaegerQueryClient::list_services`](crate::JaegerQueryClient::list_services).
    pub fn list_services(&self) -> Result<Vec<String>> {
        self.block_on(self.inner.query().list_services())
    }

    /// See [`JaegerQueryClient::list_operations`](crate::JaegerQueryClient::list_operations).
    pub fn list_operations(
        &self,
        service: &str,
        opts: ListOperationsOptions,
    ) -> Result<Vec<String>> {
        self.block_on(self.inner.query().list_operations(service, opts))
    }

    /// See [`JaegerQueryClient::get_operations`](crate::JaegerQueryClient::get_operations).
    pub fn get_operations(
        &self,
        service: &str,
        opts: ListOperationsOptions,
    ) -> Result<Vec<Operation>> {
        self.block_on(self.inner.query().get_operations(service, opts))
    }

    /// See [`JaegerQueryClient::get_dependencies`](crate::JaegerQueryClient::get_dependencies).
    pub fn get_dependencies(&self, opts: DependencyOptions) -> Result<Vec<DependencyLink>> {
        self.block_on(self.inner.query().get_dependencies(opts))
    }

    /// See [`JaegerQueryClient::wait_for_trace`](crate::JaegerQueryClient::wait_for_trace).
    pub fn wait_for_trace(&self, trace_id: &str, opts: WaitForTraceOptions) -> Result<Trace> {
        self.block_on(self.inner.query().wait_for_trace(trace_id, opts))
    }

    /// See [`AsyncClient::close`]. Idempotent.
    pub fn close(&self) {
        self.block_on(self.inner.close());
    }
}

impl Drop for JaegerClient {
    fn drop(&mut self) {
        self.inner.transport().close();
    }
}
