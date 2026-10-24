//! Session module — scan, liveness, and alias resolution.
//!
//! Public surface:
//! - [`SessionRow`] — one row of the TSV output from a session scan.
//! - [`scan`] — scan `cwd` and return sorted `Vec<SessionRow>`.
//! - [`resolve_alias`] — look up a non-UUID alias in `titles.tsv`, returning
//!   the canonical UUID `sid`.  An unknown alias is a hard error.

pub mod alias;
pub mod liveness;
pub mod scan;

pub use alias::resolve_alias;
pub use liveness::sid_live;

use std::path::Path;

/// One row of the session-scan TSV output.
///
/// TSV contract (see `session/scan.rs`):
/// ```text
/// sid \t mtime \t human_ts \t mode \t label(≤80)
/// ```
/// Rows are newest-first, sid-deduplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Canonical session UUID (lowercase, 36-char).
    pub sid: String,

    /// Unix timestamp (seconds) of the most-recent transcript modification.
    /// Used for sort order and the incremental-index freshness check.
    pub mtime: i64,

    /// Human-readable timestamp, `MM-DD HH:MM` (chrono, locale-independent).
    pub human_ts: String,

    /// Last `permissionMode` value seen in the transcript, or an empty string
    /// when no `permissionMode` field was recorded.
    pub mode: String,

    /// Display label: ai-title → last-prompt → first-user-prompt fallback,
    /// truncated to ≤ 80 chars.
    pub label: String,
}

/// Scan `cwd` for Claude Code sessions and return rows sorted newest-first.
///
/// Uses the dual cwd encoding (`paths::encode_cwd`) to union both the current
/// and legacy project-dir names.  Results are deduplicated by `sid` (newest
/// `mtime` wins, `>=` tiebreak).
///
/// See `session/scan.rs` for the full incremental-index implementation.
pub fn scan(cwd: &Path) -> Vec<SessionRow> {
    scan::scan_sessions(cwd)
}
