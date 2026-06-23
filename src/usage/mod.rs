//! Usage fetch — public surface.
//!
//! ```text
//! fetch() -> Result<UsageData, FetchError>
//! ```
//!
//! The transport layer is split into `transport.rs`; the serde model lives in
//! `model.rs`.  This module re-exports the types callers need and wires the
//! `fetch()` entry-point.

pub mod model;
pub mod report;
mod transport;

pub use model::UsageData;
pub use transport::{cache_age_secs, fetch, hub_hostname, is_configured};

/// Errors that can occur when fetching usage data from the hub.
///
/// Callers treat *any* `Err(FetchError)` as "hub down / no data available" and
/// open the hub-down account picker (interactive contexts) or fall back to the
/// current profile silently (non-interactive / hook contexts).
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The negative-cooldown file (`$SMART_DIR/.usage-fetch-failed`) is recent
    /// (< 120 s); treat identically to a live fetch failure.
    #[error("negative cache active — hub confirmed down within cooldown window")]
    NegativeCacheActive,

    /// The HTTP fetch failed (connect timeout, non-2xx, etc.).
    #[error("HTTP fetch failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The SSH fallback failed (POSIX only; compiled away on Windows — the
    /// `ssh_fetch` constructor is `#[cfg(unix)]`, so this variant must be too).
    #[cfg(unix)]
    #[error("SSH fallback failed: {0}")]
    Ssh(String),

    /// The user-supplied usage command (`CSM_USAGE_CMD`) failed to run, exited
    /// non-zero, or produced output that did not parse as `UsageData`.
    #[error("usage command failed: {0}")]
    Command(String),

    /// The JSON payload from the hub was unparseable.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O error when reading/writing the cache files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The hub returned an empty or null payload.
    #[error("hub returned empty payload")]
    EmptyPayload,
}
