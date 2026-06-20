//! Session module — scan, liveness, and alias resolution.
//!
//! Public surface:
//! - [`SessionRow`] — one row of the TSV output from a session scan.
//! - [`scan`] — scan `cwd` and return sorted `Vec<SessionRow>`.
//! - [`resolve_alias`] — look up a non-UUID alias in `titles.tsv`, returning
//!   the canonical UUID `sid`.  An unknown alias is a hard error (spec N6).

pub mod alias;
pub mod liveness;
pub mod scan;

pub use alias::resolve_alias;
pub use liveness::sid_live;

use std::path::Path;

/// One row of the session-scan TSV output.
///
/// TSV contract (spec §2 / `session/scan.rs`):
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

impl SessionRow {
    /// Render as a tab-delimited line (no trailing newline).
    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.sid, self.mtime, self.human_ts, self.mode, self.label
        )
    }

    /// Parse a TSV line produced by [`to_tsv`].  Returns `None` on malformed input.
    /// The symmetric complement of `to_tsv`; kept for any future TSV-based session
    /// IPC/cache reader (no production consumer reads the 5-column form yet — the
    /// scan index uses its own 4-column parser).
    #[allow(dead_code)]
    pub fn from_tsv(line: &str) -> Option<Self> {
        let mut cols = line.splitn(5, '\t');
        let sid = cols.next()?.to_owned();
        let mtime: i64 = cols.next()?.parse().ok()?;
        let human_ts = cols.next()?.to_owned();
        let mode = cols.next()?.to_owned();
        let label = cols.next()?.to_owned();
        Some(SessionRow {
            sid,
            mtime,
            human_ts,
            mode,
            label,
        })
    }
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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_row_tsv_roundtrip() {
        let row = SessionRow {
            sid: "01234567-89ab-cdef-0123-456789abcdef".to_owned(),
            mtime: 1_718_000_000,
            human_ts: "06-10 14:32".to_owned(),
            mode: "default".to_owned(),
            label: "Some conversation label".to_owned(),
        };
        let tsv = row.to_tsv();
        let parsed = SessionRow::from_tsv(&tsv).expect("round-trip should succeed");
        assert_eq!(row, parsed);
    }

    #[test]
    fn session_row_tsv_roundtrip_empty_mode() {
        let row = SessionRow {
            sid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            mtime: 0,
            human_ts: "01-01 00:00".to_owned(),
            mode: String::new(),
            label: "label with\ttab in it? no — label is last col so tab OK".to_owned(),
        };
        let tsv = row.to_tsv();
        let parsed = SessionRow::from_tsv(&tsv).expect("round-trip should succeed");
        assert_eq!(row, parsed);
    }

    #[test]
    fn session_row_from_tsv_rejects_short_line() {
        // Fewer than 5 fields → None
        assert!(SessionRow::from_tsv("only-three\t1234\thuman").is_none());
    }

    #[test]
    fn session_row_from_tsv_rejects_bad_mtime() {
        assert!(SessionRow::from_tsv("sid\tnot-a-number\thuman\tmode\tlabel").is_none());
    }
}
