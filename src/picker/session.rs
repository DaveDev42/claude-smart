//! Session picker — fzf-based interactive session selector.
//!
//! Spec §2 Picker (session-select):
//! - Synthetic sentinel rows `__NEW__` (mtime 9999999999) and `__CONTINUE__`
//!   (mtime 9999999998) are prepended to the fzf input.
//! - Live sessions are annotated with `● live in another pane · …`; col1 keeps
//!   the real UUID.
//! - fzf flags: `--with-nth=3.. --delimiter='\t' --prompt='session > '
//!   --height=40% --reverse --no-multi`; col1 recovered by field split.
//! - Exit 130 / empty selection → `None` (caller exits cleanly).
//! - When fzf is absent or no TTY → degrade to newest-free-sid or fresh.
//!
//! The `__NEW__` sentinel resolves to `PickedSession::Fresh`.
//! The `__CONTINUE__` sentinel resolves to `PickedSession::Continue`.
//! A real UUID resolves to `PickedSession::Resume(session_id)`.

use crate::picker::fzf::FzfOpts;

// ─── sentinel constants ───────────────────────────────────────────────────────

/// Synthetic "start a new session" sentinel row (mtime field = 9999999999).
pub const SENTINEL_NEW: &str = "__NEW__";
/// Synthetic "continue the newest session" sentinel row (mtime field = 9999999998).
pub const SENTINEL_CONTINUE: &str = "__CONTINUE__";

// ─── types ────────────────────────────────────────────────────────────────────

/// A single row the session picker shows in fzf.
///
/// TSV format (matches `session/scan.rs` output contract):
/// `sid \t mtime \t human_ts \t mode \t label(≤80)`
///
/// Col1 (`sid`) is hidden from display (`--with-nth=3..`); it is the recovery key.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Session UUID (or sentinel constant). The hidden fzf col1.
    pub sid: String,
    /// Unix mtime in seconds (u64 as string in TSV). Sentinels use 9999999999 /
    /// 9999999998 so they sort to the top with `--reverse`.
    pub mtime: u64,
    /// Human timestamp `MM-DD HH:MM`, or empty for sentinels.
    pub human_ts: String,
    /// Last `permissionMode` from the transcript (e.g. `"default"`, `"bypassPermissions"`).
    pub mode: String,
    /// Display label (≤80 chars). Sentinels carry a fixed description.
    pub label: String,
    /// Whether the session is currently live in another pane.
    pub is_live: bool,
}

impl SessionRow {
    /// Render to a tab-delimited line for fzf stdin.
    ///
    /// Format: `sid\tmtime\thuman_ts\tmode\tdisplay_label`
    /// where `display_label` is the label, optionally prefixed with the live
    /// annotation `● live in another pane · ` when `is_live == true`.
    pub fn to_tsv(&self) -> String {
        let display_label = if self.is_live {
            format!("● live in another pane · {}", self.label)
        } else {
            self.label.clone()
        };
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.sid, self.mtime, self.human_ts, self.mode, display_label
        )
    }

    /// Construct the `__NEW__` sentinel row.
    pub fn new_session_sentinel() -> Self {
        Self {
            sid: SENTINEL_NEW.to_string(),
            mtime: 9_999_999_999,
            human_ts: String::new(),
            mode: String::new(),
            label: "[ start a new session ]".to_string(),
            is_live: false,
        }
    }

    /// Construct the `__CONTINUE__` sentinel row.
    ///
    /// `newest_live` — if `Some(label)`, the label is incorporated into the
    /// display text to reflect the newest live session.
    pub fn continue_sentinel(newest_live: Option<&str>) -> Self {
        let label = match newest_live {
            Some(live) => format!("[ continue · {live} ]"),
            None => "[ continue newest session ]".to_string(),
        };
        Self {
            sid: SENTINEL_CONTINUE.to_string(),
            mtime: 9_999_999_998,
            human_ts: String::new(),
            mode: String::new(),
            label,
            is_live: false,
        }
    }
}

/// The resolved result of the session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickedSession {
    /// User chose `__NEW__` or picker was skipped (no sessions / no TTY / no fzf).
    Fresh,
    /// User chose `__CONTINUE__` — resume the newest (free) session.
    Continue,
    /// User chose a real session; `session_id` is the UUID.
    Resume(String),
}

// ─── SessionPicker ────────────────────────────────────────────────────────────

/// Interactive fzf session picker.
///
/// Build with `SessionPicker::new(rows)`, then call `SessionPicker::pick()`.
///
/// Spec §2 Picker:
/// - When `rows` is empty, returns `PickedSession::Fresh` immediately (no picker).
/// - When fzf is unavailable or there is no TTY, degrades to newest-free-sid
///   auto-selection (callers interpret `PickedSession::Continue` accordingly).
/// - `__NEW__` and `__CONTINUE__` sentinels are prepended automatically; callers
///   must not include them in the input `rows`.
pub struct SessionPicker {
    rows: Vec<SessionRow>,
}

impl SessionPicker {
    /// Create a new picker from pre-built session rows.
    ///
    /// `rows` must already be sorted by mtime descending (newest first) and
    /// deduplicated by sid. Sentinels are added by `pick()`.
    pub fn new(rows: Vec<SessionRow>) -> Self {
        Self { rows }
    }

    /// Run the picker and return the user's selection.
    ///
    /// - Empty `rows` → `PickedSession::Fresh` (no picker shown).
    /// - fzf unavailable → `None` (caller degrades to newest-free-sid / fresh).
    /// - Escape / Ctrl-C / empty selection → `None`.
    /// - `__NEW__` → `Fresh`, `__CONTINUE__` → `Continue`, UUID → `Resume(uuid)`.
    pub fn pick(&self, newest_live_label: Option<&str>) -> Option<PickedSession> {
        if self.rows.is_empty() {
            return Some(PickedSession::Fresh);
        }
        if !crate::picker::fzf::fzf_available() {
            return None;
        }
        let lines = self.build_fzf_input(newest_live_label);
        let col1 = crate::picker::fzf::run_fzf(&lines, &Self::fzf_opts())?;
        Some(Self::resolve(&col1))
    }

    /// Map a recovered col1 (sentinel constant or UUID) to a `PickedSession`.
    pub fn resolve(col1: &str) -> PickedSession {
        match col1 {
            SENTINEL_NEW => PickedSession::Fresh,
            SENTINEL_CONTINUE => PickedSession::Continue,
            uuid => PickedSession::Resume(uuid.to_string()),
        }
    }

    /// Build the TSV lines to pipe to fzf, including sentinel rows at the top.
    ///
    /// Returns the lines in display order: `__NEW__`, `__CONTINUE__`, then the
    /// real session rows newest-first.
    pub fn build_fzf_input(&self, newest_live_label: Option<&str>) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        lines.push(SessionRow::new_session_sentinel().to_tsv());
        lines.push(SessionRow::continue_sentinel(newest_live_label).to_tsv());
        for row in &self.rows {
            lines.push(row.to_tsv());
        }
        lines
    }

    /// fzf opts for the session picker.
    ///
    /// Spec: `--with-nth=3.. --delimiter='\t' --prompt='session > ' --height=40%
    /// --reverse --no-multi`
    pub fn fzf_opts() -> FzfOpts {
        FzfOpts {
            prompt: "session > ".to_string(),
            with_nth: "3..".to_string(),
            delimiter: "\t".to_string(),
            height: "40%".to_string(),
            extra_args: vec!["--reverse".to_string(), "--no-multi".to_string()],
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_mtime_ordering() {
        // NEW > CONTINUE > any real session mtime
        assert!(
            SessionRow::new_session_sentinel().mtime > SessionRow::continue_sentinel(None).mtime
        );
        // Any real session mtime should be well below 9_999_999_998.
        // (Real sessions have Unix timestamps in the ~1.7×10^9 range as of 2026.)
        let real_mtime: u64 = 1_750_000_000;
        assert!(SessionRow::continue_sentinel(None).mtime > real_mtime);
    }

    #[test]
    fn sentinel_sid_constants() {
        assert_eq!(SessionRow::new_session_sentinel().sid, SENTINEL_NEW);
        assert_eq!(SessionRow::continue_sentinel(None).sid, SENTINEL_CONTINUE);
    }

    #[test]
    fn live_annotation_prefix() {
        let mut row = SessionRow {
            sid: "abc".to_string(),
            mtime: 1_000,
            human_ts: "06-18 12:00".to_string(),
            mode: "default".to_string(),
            label: "My session".to_string(),
            is_live: true,
        };
        let tsv = row.to_tsv();
        // display label (col5) must have the live prefix
        let col5 = tsv.split('\t').nth(4).unwrap();
        assert!(col5.starts_with("● live in another pane · "), "got: {col5}");
        assert!(col5.contains("My session"));

        // Non-live row must NOT have the annotation.
        row.is_live = false;
        let tsv2 = row.to_tsv();
        let col5_2 = tsv2.split('\t').nth(4).unwrap();
        assert!(!col5_2.starts_with("●"), "got: {col5_2}");
    }

    #[test]
    fn to_tsv_col1_is_sid() {
        let row = SessionRow {
            sid: "deadbeef-0000-0000-0000-000000000000".to_string(),
            mtime: 1_700_000_000,
            human_ts: "01-01 00:00".to_string(),
            mode: "bypassPermissions".to_string(),
            label: "label text".to_string(),
            is_live: false,
        };
        let tsv = row.to_tsv();
        let col1 = tsv.split('\t').next().unwrap();
        assert_eq!(col1, "deadbeef-0000-0000-0000-000000000000");
    }

    #[test]
    fn build_fzf_input_sentinel_order() {
        let picker = SessionPicker::new(vec![]);
        let lines = picker.build_fzf_input(None);
        // Sentinels must be first two lines.
        assert_eq!(lines.len(), 2);
        let col1_0 = lines[0].split('\t').next().unwrap();
        let col1_1 = lines[1].split('\t').next().unwrap();
        assert_eq!(col1_0, SENTINEL_NEW);
        assert_eq!(col1_1, SENTINEL_CONTINUE);
    }

    #[test]
    fn build_fzf_input_rows_follow_sentinels() {
        let rows = vec![
            SessionRow {
                sid: "aaa".to_string(),
                mtime: 2,
                human_ts: String::new(),
                mode: String::new(),
                label: "a".to_string(),
                is_live: false,
            },
            SessionRow {
                sid: "bbb".to_string(),
                mtime: 1,
                human_ts: String::new(),
                mode: String::new(),
                label: "b".to_string(),
                is_live: false,
            },
        ];
        let picker = SessionPicker::new(rows);
        let lines = picker.build_fzf_input(None);
        assert_eq!(lines.len(), 4);
        let sids: Vec<&str> = lines.iter().map(|l| l.split('\t').next().unwrap()).collect();
        assert_eq!(sids[0], SENTINEL_NEW);
        assert_eq!(sids[1], SENTINEL_CONTINUE);
        assert_eq!(sids[2], "aaa");
        assert_eq!(sids[3], "bbb");
    }

    #[test]
    fn fzf_opts_session_picker() {
        let opts = SessionPicker::fzf_opts();
        assert_eq!(opts.prompt, "session > ");
        assert_eq!(opts.with_nth, "3..");
        assert_eq!(opts.delimiter, "\t");
    }

    #[test]
    fn continue_sentinel_with_live_label() {
        let row = SessionRow::continue_sentinel(Some("my-session-label"));
        assert!(row.label.contains("my-session-label"), "got: {}", row.label);
    }
}
