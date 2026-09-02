//! Usage fetch — public surface.
//!
//! ```text
//! fetch() -> Result<UsageData, FetchError>
//! ```
//!
//! The transport layer is split into `transport.rs`; the serde model lives in
//! `model.rs`.  This module re-exports the types callers need and wires the
//! `fetch()` entry-point.

pub mod local;
pub mod model;
pub mod report;
mod transport;

pub use model::UsageData;
pub use transport::{fetch, fetch_with};

/// Errors that can occur when fetching usage data.
///
/// Callers treat *any* `Err(FetchError)` as "no usage data available right
/// now" and open the offline account picker (interactive contexts) or fall
/// back to the current profile silently (non-interactive / hook contexts).
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The negative-cooldown file (`$SMART_DIR/.usage-fetch-failed`) is recent
    /// (< 120 s) — local collection failed for every profile within the
    /// cooldown window; treat identically to a fresh total failure.
    #[error(
        "negative cache active — usage collection failed for every profile within cooldown window"
    )]
    NegativeCacheActive,

    /// The user-supplied usage command (`CSM_USAGE_CMD`) failed to run, exited
    /// non-zero, or produced output that did not parse as `UsageData`.
    #[error("usage command failed: {0}")]
    Command(String),

    /// Local, per-profile usage collection failed in a way that isn't already
    /// captured per-profile in `UsageData::errors` — e.g. an I/O error writing
    /// the local store. See `local::LocalError`.
    #[error("local usage collection error: {0}")]
    Local(#[from] local::LocalError),

    /// The JSON payload (cache file or `CSM_USAGE_CMD` output) was unparseable.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O error when reading/writing the cache files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Every configured profile failed to produce usage data (zero profiles,
    /// at least one error) — local collection's terminal failure mode.
    #[error("usage collection produced no usable data for any profile")]
    EmptyPayload,
}
