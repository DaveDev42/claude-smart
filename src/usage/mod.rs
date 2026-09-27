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
pub use transport::{
    LIMIT_PICK_TIMEOUT, fetch, fetch_cached, fetch_cached_with, fetch_for_limit_pick, fetch_with,
};

// ─── warnings ─────────────────────────────────────────────────────────────────

/// Where the collector's warnings go. They print on stderr, except inside
/// [`capture_warnings`], which hands them to its caller instead: the limit
/// switch runs the collector from the supervisor of an Orca pane, where
/// only fatal errors and the one relaunch line may reach the pane (design
/// §5), so it routes them to csm's log there.
pub(crate) fn warn(line: String) {
    let unclaimed = WARN_SINK.with(|s| match s.borrow_mut().as_mut() {
        Some(v) => {
            v.push(line);
            None
        }
        None => Some(line),
    });
    if let Some(line) = unclaimed {
        eprintln!("{line}");
    }
}

thread_local! {
    static WARN_SINK: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with every collector warning on this thread collected instead of
/// printed. The collector runs on the calling thread, so nothing escapes.
pub fn capture_warnings<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let prev = WARN_SINK.with(|s| s.borrow_mut().replace(Vec::new()));
    let r = f();
    let got = WARN_SINK.with(|s| std::mem::replace(&mut *s.borrow_mut(), prev));
    (r, got.unwrap_or_default())
}

// ─── reach probe (test-only) ────────────────────────────────────────────────

/// Marks the steps a usage read may never reach from `csm hook` or the
/// statusline (design decision 8): the operator command, the usage API,
/// Orca's socket, the Keychain, and Orca's process-table sweep. Each of
/// those entry points calls [`reach::note`]; under `cfg(test)` the call is
/// recorded per thread so a test can assert the hook reached none of them.
/// In a release build `note` compiles to nothing.
pub(crate) mod reach {
    #[cfg(test)]
    thread_local! {
        static SEEN: std::cell::RefCell<Vec<&'static str>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Record that `step` was reached on this thread.
    #[inline]
    pub(crate) fn note(step: &'static str) {
        #[cfg(test)]
        SEEN.with(|s| s.borrow_mut().push(step));
        #[cfg(not(test))]
        let _ = step;
    }

    /// Drain this thread's record.
    #[cfg(test)]
    pub(crate) fn take() -> Vec<&'static str> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }
}

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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_warnings_collects_and_restores_the_outer_scope() {
        let (inner, outer) = capture_warnings(|| {
            warn("outer-1".into());
            let ((), inner) = capture_warnings(|| warn("inner".into()));
            warn("outer-2".into());
            inner
        });
        assert_eq!(inner, vec!["inner".to_owned()]);
        assert_eq!(outer, vec!["outer-1".to_owned(), "outer-2".to_owned()]);
        // Outside any scope nothing is collected.
        assert!(WARN_SINK.with(|s| s.borrow().is_none()));
    }
}
