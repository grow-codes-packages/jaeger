// Created by sdk-kit 0.1.0; owned by this repository.
//! Helpers for the integration tests: bootstrap credentials and the conformance directory.

use std::path::PathBuf;

use grow_jaeger as sdk;

/// Loads `KEY=value` lines from `$GROW_BOOTSTRAP_ENV` (default `/bootstrap/env`) into the
/// process environment; variables that are already set win. A missing file is not an error.
pub fn load_bootstrap_env() {
    let path = std::env::var("GROW_BOOTSTRAP_ENV").unwrap_or_else(|_| "/bootstrap/env".to_owned());
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().trim_start_matches("export ").trim();
            let value = value.trim().trim_matches('"');
            if !key.is_empty() && std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// A short id unique per call (run id from `GROW_TEST_RUN_ID` plus time and a counter), for
/// unique service names (Jaeger reports itself as `jaeger` and keeps every service).
pub fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!(
        "{prefix}-{:08x}{}",
        nanos ^ std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

/// Environment variable that the bootstrap writes; panics with a clear message when missing.
pub fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} missing from /bootstrap/env"))
}

/// `$GROW_CONFORMANCE_DIR`, else `<repo>/conformance/scenarios`.
pub fn conformance_dir() -> PathBuf {
    sdk::testing::scenarios_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}
