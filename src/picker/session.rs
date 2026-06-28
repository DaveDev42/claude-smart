//! Session picker — interactive session selector (in-process fuzzy picker).
//!
//! Spec §2 Picker (session-select):
//! - Synthetic sentinel rows `__NEW__` (mtime 9999999999) and `__CONTINUE__`
//!   (mtime 9999999998) are prepended to the picker input.
//! - Live sessions are annotated with `● live in another pane · …`; col1 keeps
//!   the real UUID.
//! - Display fields 3.. (full-UUID col1 + mtime col2 hidden) show
//!   `human_ts short_id mode label`, where `short_id` is the first 8 UUID chars,
//!   so the user can see WHICH session a row is without the full id; tab
//!   delimiter, `session > ` prompt; single select, best match on top; col1
//!   recovered by field split.
//! - Escape / Ctrl-C → `PickedSession::Cancel` (caller aborts the launch).
//! - When there is no usable terminal → degrade to newest-free-sid or fresh.
//!
//! The `__NEW__` sentinel resolves to `PickedSession::Fresh`.
//! The `__CONTINUE__` sentinel resolves to `PickedSession::Continue`.
//! A real UUID resolves to `PickedSession::Resume(session_id)`.

use crate::picker::engine::{self, PickerOpts};

// ─── sentinel constants ───────────────────────────────────────────────────────

/// Synthetic "start a new session" sentinel row (mtime field = 9999999999).
pub const SENTINEL_NEW: &str = "__NEW__";
/// Synthetic "continue the newest session" sentinel row (mtime field = 9999999998).
pub const SENTINEL_CONTINUE: &str = "__CONTINUE__";

// ─── types ────────────────────────────────────────────────────────────────────

/// A single row the session picker shows.
///
/// TSV format (matches `session/scan.rs` output contract):
/// `sid \t mtime \t human_ts \t mode \t label(≤80)`
///
/// Col1 (`sid`) is hidden from display (`--with-nth=3..`); it is the recovery key.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Session UUID (or sentinel constant). The hidden col1 recovery key.
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
    /// Render to a tab-delimited picker input line.
    ///
    /// Format: `sid\tmtime\thuman_ts\tshort_id\tmode\tdisplay_label`
    /// where `short_id` is the first 8 chars of the UUID (empty for sentinels)
    /// so the user can see WHICH session each row is without the full UUID eating
    /// the line, and `display_label` is the label, optionally prefixed with the
    /// live annotation `● live in another pane · ` when `is_live == true`.
    ///
    /// col1 (`sid`) stays the full UUID — the hidden recovery key; the displayed
    /// short id is a separate column (`--with-nth=3..` shows it, col1 is hidden).
    pub fn to_tsv(&self) -> String {
        let display_label = if self.is_live {
            format!("● live in another pane · {}", self.label)
        } else {
            self.label.clone()
        };
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.sid,
            self.mtime,
            self.human_ts,
            self.short_id(),
            self.mode,
            display_label
        )
    }

    /// The short, human-facing session id: the first 8 chars of the UUID. Empty
    /// for sentinel rows (`__NEW__` / `__CONTINUE__`), which have no real id.
    pub fn short_id(&self) -> String {
        if self.sid == SENTINEL_NEW || self.sid == SENTINEL_CONTINUE {
            String::new()
        } else {
            self.sid.chars().take(8).collect()
        }
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
    /// User chose `__NEW__` or picker was skipped (no sessions / no usable terminal).
    Fresh,
    /// User chose `__CONTINUE__` — resume the newest (free) session.
    Continue,
    /// User chose a real session; `session_id` is the UUID.
    Resume(String),
    /// User pressed Escape / Ctrl-C — abort the launch entirely (do NOT fall
    /// back to a fresh session). Distinct from `Fresh`, which is an explicit
    /// "start new" choice or a graceful degrade when no picker can run.
    Cancel,
}

// ─── SessionPicker ────────────────────────────────────────────────────────────

/// Interactive session picker.
///
/// Build with `SessionPicker::new(rows)`, then call `SessionPicker::pick()`.
///
/// Spec §2 Picker:
/// - When `rows` is empty, returns `PickedSession::Fresh` immediately (no picker).
/// - When there is no usable terminal, degrades to newest-free-sid
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
    /// - No usable terminal → `None` (caller degrades to newest-free-sid / fresh).
    /// - Escape / Ctrl-C → `Some(PickedSession::Cancel)` (caller aborts the launch).
    /// - `__NEW__` → `Fresh`, `__CONTINUE__` → `Continue`, UUID → `Resume(uuid)`.
    pub fn pick(&self, newest_live_label: Option<&str>) -> Option<PickedSession> {
        if self.rows.is_empty() {
            return Some(PickedSession::Fresh);
        }
        let lines = self.build_picker_input(newest_live_label);
        match engine::run_picker(&lines, &Self::picker_opts()) {
            engine::PickerOutcome::Selected(col1) => Some(Self::resolve(&col1)),
            // Escape / Ctrl-C → explicit cancel: surface it so the caller aborts
            // instead of silently starting a fresh session.
            engine::PickerOutcome::Cancelled => Some(PickedSession::Cancel),
            // No usable terminal → degrade (None) per the existing contract.
            engine::PickerOutcome::Unavailable => None,
        }
    }

    /// Map a recovered col1 (sentinel constant or UUID) to a `PickedSession`.
    pub fn resolve(col1: &str) -> PickedSession {
        match col1 {
            SENTINEL_NEW => PickedSession::Fresh,
            SENTINEL_CONTINUE => PickedSession::Continue,
            uuid => PickedSession::Resume(uuid.to_string()),
        }
    }

    /// Build the TSV lines for the picker, including sentinel rows at the top.
    ///
    /// Returns the lines in display order: `__NEW__`, `__CONTINUE__`, then the
    /// real session rows newest-first.
    pub fn build_picker_input(&self, newest_live_label: Option<&str>) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        lines.push(SessionRow::new_session_sentinel().to_tsv());
        lines.push(SessionRow::continue_sentinel(newest_live_label).to_tsv());
        for row in &self.rows {
            lines.push(row.to_tsv());
        }
        lines
    }

    /// Picker opts for the session picker.
    ///
    /// Display fields 3.. (full-UUID col1 + mtime col2 hidden), tab delimiter,
    /// `session > ` prompt. The shown columns are `human_ts short_id mode label`
    /// — the short id (first 8 UUID chars) is visible while the full UUID stays
    /// hidden as the recovery key. Fuzzy matching also runs over these columns,
    /// so the user can type a partial id, a date, or words from the label.
    pub fn picker_opts() -> PickerOpts {
        PickerOpts {
            prompt: "session > ".to_string(),
            display_from: 3,
            delimiter: '\t',
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
        // display label (col6, index 5) must have the live prefix
        let label_col = tsv.split('\t').nth(5).unwrap();
        assert!(
            label_col.starts_with("● live in another pane · "),
            "got: {label_col}"
        );
        assert!(label_col.contains("My session"));

        // Non-live row must NOT have the annotation.
        row.is_live = false;
        let tsv2 = row.to_tsv();
        let label_col_2 = tsv2.split('\t').nth(5).unwrap();
        assert!(!label_col_2.starts_with("●"), "got: {label_col_2}");
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
        let cols: Vec<&str> = tsv.split('\t').collect();
        // col1 (index 0) is the full UUID recovery key.
        assert_eq!(cols[0], "deadbeef-0000-0000-0000-000000000000");
        // col4 (index 3) is the displayed short id: first 8 UUID chars.
        assert_eq!(cols[3], "deadbeef");
        // The full UUID must NOT appear in any DISPLAYED column (index >= 2).
        assert!(
            !cols[2..].iter().any(|c| c.contains("0000-0000")),
            "full UUID leaked into a displayed column: {cols:?}"
        );
    }

    #[test]
    fn short_id_is_first_8_chars_and_empty_for_sentinels() {
        let row = SessionRow {
            sid: "abcd1234-5678-90ab-cdef-000000000000".to_string(),
            mtime: 1,
            human_ts: String::new(),
            mode: String::new(),
            label: "x".to_string(),
            is_live: false,
        };
        assert_eq!(row.short_id(), "abcd1234");
        // Sentinels carry no real id → empty short id (no stray "__NEW__" text).
        assert_eq!(SessionRow::new_session_sentinel().short_id(), "");
        assert_eq!(SessionRow::continue_sentinel(None).short_id(), "");
    }

    #[test]
    fn build_picker_input_sentinel_order() {
        let picker = SessionPicker::new(vec![]);
        let lines = picker.build_picker_input(None);
        // Sentinels must be first two lines.
        assert_eq!(lines.len(), 2);
        let col1_0 = lines[0].split('\t').next().unwrap();
        let col1_1 = lines[1].split('\t').next().unwrap();
        assert_eq!(col1_0, SENTINEL_NEW);
        assert_eq!(col1_1, SENTINEL_CONTINUE);
    }

    #[test]
    fn build_picker_input_rows_follow_sentinels() {
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
        let lines = picker.build_picker_input(None);
        assert_eq!(lines.len(), 4);
        let sids: Vec<&str> = lines
            .iter()
            .map(|l| l.split('\t').next().unwrap())
            .collect();
        assert_eq!(sids[0], SENTINEL_NEW);
        assert_eq!(sids[1], SENTINEL_CONTINUE);
        assert_eq!(sids[2], "aaa");
        assert_eq!(sids[3], "bbb");
    }

    #[test]
    fn picker_opts_session_picker() {
        let opts = SessionPicker::picker_opts();
        assert_eq!(opts.prompt, "session > ");
        assert_eq!(opts.display_from, 3);
        assert_eq!(opts.delimiter, '\t');
    }

    /// `resolve` never produces `Cancel`: a recovered col1 is always a real
    /// choice (NEW/CONTINUE/uuid). `Cancel` arises ONLY from a picker
    /// Escape/Ctrl-C in `pick`, so it must stay a distinct variant from `Fresh`
    /// — collapsing the two would resurrect "Escape silently starts fresh".
    #[test]
    fn resolve_never_yields_cancel_and_cancel_is_distinct_from_fresh() {
        assert_eq!(SessionPicker::resolve(SENTINEL_NEW), PickedSession::Fresh);
        assert_eq!(
            SessionPicker::resolve(SENTINEL_CONTINUE),
            PickedSession::Continue
        );
        assert_eq!(
            SessionPicker::resolve("dead-beef"),
            PickedSession::Resume("dead-beef".to_string())
        );
        // The type-level guarantee the caller relies on:
        assert_ne!(PickedSession::Cancel, PickedSession::Fresh);
    }

    #[test]
    fn continue_sentinel_with_live_label() {
        let row = SessionRow::continue_sentinel(Some("my-session-label"));
        assert!(row.label.contains("my-session-label"), "got: {}", row.label);
    }
}
