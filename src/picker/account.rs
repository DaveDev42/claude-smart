//! The stale-usage account picker: opened when local usage collection fails
//! or returns nothing scorable, so csm cannot auto-pick. Rows show
//! last-known usage from `.usage-cache.json` with a stale-age annotation.
//!
//! When an *interactive* proactive-pick context encounters a usage fetch miss
//! (`Err(FetchError)`) or negative-cache active, the binary opens a picker over
//! configured profiles showing last-known stale usage from `.usage-cache.json`.
//!
//! Trigger: proactive-pick + interactive (isatty(0)&&isatty(1)) + fetch miss.
//! NOT triggered when: non-interactive / hook / `--profile` pin / `--no-pick`.
//!
//! Row format (tab-delimited):
//!   col1 = profile_name (hidden recovery key)
//!   col2+ = display text (session%, week%, resets, stale-age annotation)
//!
//! Picker: display fields 2.. (profile name hidden), tab delimiter,
//! `account > ` prompt; single select, best match on top.
//!
//! Degrade path (no usable terminal):
//!   Print to stderr: `csm: usage collection failed and no interactive terminal — keeping current profile`
//!   Return `Unavailable` (caller falls back to current profile).
//!
//! Escape / Ctrl-C → `Cancelled` (caller aborts the launch).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::picker::engine::{self, PickerOpts, PickerOutcome};
use crate::usage::model::{Attention, AttentionKind};

// ─── types ────────────────────────────────────────────────────────────────────

/// Per-profile usage data for stale-cache rendering.
///
/// All fields are `Option` because a stale-usage picker may only have partial data
/// (or none at all for profiles absent from the cache).
#[derive(Debug, Clone)]
pub struct StaleProfileData {
    /// Session usage percentage (0–100), or `None` if absent/null in cache.
    pub session_pct: Option<i64>,
    /// Weekly (all-models) usage percentage (0–100), or `None`.
    pub week_all_pct: Option<i64>,
    /// Raw reset string as stored in cache (e.g. `"Jun 18 at 9pm (Asia/Seoul)"`).
    /// `None` if absent/null.
    pub resets: Option<String>,
    /// Machine-native reset epoch (`week_all.resets_at`), when the cache was
    /// written by the local collector. `None` for an older cache that predates
    /// the field, or when the section itself is absent. Ranking prefers this
    /// over re-parsing `resets` — see `cmd::run::account_row_rank`.
    pub resets_at: Option<i64>,
    /// Weekly PER-MODEL-TIER (`week_fable`) usage percentage, or `None` when
    /// this profile carries no model-scoped weekly cap.
    /// `cmd::run::account_row_rank` (via `scoring::is_viable_pcts`) no longer
    /// lets this field constrain viability — a model-scoped-only cap still
    /// leaves the row usable on another model, so it never sinks the row on
    /// its own. Still feeds the row's displayed `model NN%` reading and the
    /// effective-reset ranking (`scoring::effective_reset_epoch`).
    pub week_fable_pct: Option<i64>,
    /// Machine-native reset epoch for the `week_fable` section
    /// (`week_fable.resets_at`), when known. `None` when absent (no cap for
    /// this profile, or an older cache record).
    pub week_fable_resets_at: Option<i64>,
    /// Error string if the cache recorded an error for this profile.
    pub error: Option<String>,
    /// This profile's `ProfileUsage::attention`, carried straight through
    /// from the cache. In practice only [`AttentionKind::NeedsRefresh`]
    /// reaches this field with `error` still `None` — a `NeedsLogin`
    /// profile is always also recorded in `UsageData::errors` (see
    /// `usage::local::mod`'s module doc), so [`render_display`] never needs
    /// to render this alongside an error.
    pub attention: Option<Attention>,
}

/// Star marker prefixed to the recommended row's display (what `pick_best`
/// would auto-select). Non-recommended rows get an equal-width blank prefix so
/// the usage columns stay aligned.
pub const RECOMMENDED_MARKER: &str = "★ ";
/// Blank prefix (same display width as [`RECOMMENDED_MARKER`]) for the rows that
/// are not the recommendation, keeping every row's columns aligned.
pub const PLAIN_MARKER: &str = "  ";

/// One row of the account picker display.
#[derive(Debug, Clone)]
pub struct AccountRow {
    /// Profile name — the hidden col1 recovery key.
    pub profile: String,
    /// Pre-rendered display string (everything after the tab).
    pub display: String,
    /// Whether this is the recommended row (the one `pick_best` would
    /// auto-select). Exactly one row is recommended when a viable candidate
    /// exists; the display gets a leading `★` so the user can see the pick.
    pub recommended: bool,
}

impl AccountRow {
    /// Render to a tab-delimited picker input line: `profile\t<marker>display`.
    ///
    /// The marker (`★ ` when recommended, two spaces otherwise) leads the
    /// display column so the recommendation is visible AND the usage columns
    /// stay aligned across rows. col1 (`profile`) is the hidden recovery key and
    /// is never decorated.
    pub fn to_tsv(&self) -> String {
        let marker = if self.recommended {
            RECOMMENDED_MARKER
        } else {
            PLAIN_MARKER
        };
        format!("{}\t{}{}", self.profile, marker, self.display)
    }

    /// Build an `AccountRow` from a profile name and its stale data.
    ///
    /// Stale-age annotation is appended as `(stale Nm ago)` when `cache_mtime_secs`
    /// is `Some`.
    ///
    /// `recommended` marks this as the row `pick_best` would auto-select; it gets
    /// a leading `★` at render time (see [`AccountRow::to_tsv`]).
    ///
    /// Row format / rendered examples:
    /// ```text
    /// home   session 3%   week 32%   resets Jun 18 9pm   (stale 4m ago)
    /// work   [error: no credentials]                      (stale 4m ago)
    /// home   (no usage data)
    /// ```
    pub fn build(
        profile: &str,
        data: &StaleProfileData,
        cache_mtime_secs: Option<u64>,
        recommended: bool,
    ) -> Self {
        let stale_annotation = cache_mtime_secs.map(|mtime| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let age_secs = now.saturating_sub(mtime);
            format_stale_age(age_secs)
        });

        // The display column (col2, shown via --with-nth=2..) MUST lead with the
        // profile name — col1 is hidden for recovery, so without this the user
        // could not tell which account each row is. Matches the example above
        // (`home   session 3%   …`). Left-pad to a fixed width so the usage
        // columns line up across rows.
        let usage = render_display(data, stale_annotation.as_deref());
        let display = format!("{:<10} {usage}", profile);

        AccountRow {
            profile: profile.to_string(),
            display,
            recommended,
        }
    }
}

/// Render the usage portion (everything after the profile-name column) for a row.
///
/// Rendering rules:
/// - error present → `[error: <string>]  (stale Nm ago)`
/// - no data at all → `(no usage data)` (no stale annotation either way for
///   no-data; but stale annotation is still appended if we have a cache mtime)
/// - otherwise: `session <N>%   week <N>%   resets <str>   (stale Nm ago)`
fn render_display(data: &StaleProfileData, stale: Option<&str>) -> String {
    let stale_suffix = stale.map(|s| format!("   ({s})")).unwrap_or_default();

    if let Some(ref err) = data.error {
        return format!("[error: {err}]{stale_suffix}");
    }

    // Build the usage part from whatever sections are available.
    let mut parts = Vec::new();

    if let Some(pct) = data.session_pct {
        parts.push(format!("session {pct}%"))
    }
    if let Some(pct) = data.week_all_pct {
        parts.push(format!("week {pct}%"))
    }
    // Model-scoped weekly cap — only shown when this profile actually carries
    // one; `None` means no such limit for it (see `StaleProfileData` doc).
    if let Some(pct) = data.week_fable_pct {
        parts.push(format!("model {pct}%"))
    }
    if let Some(ref resets) = data.resets {
        parts.push(format!("resets {resets}"));
    }
    // Distinct annotation for a token that will silently self-heal on next
    // use — NOT an error, so it renders alongside the usage numbers rather
    // than replacing them the way `[error: …]` does above.
    if matches!(
        data.attention,
        Some(Attention {
            kind: AttentionKind::NeedsRefresh,
            ..
        })
    ) {
        parts.push("[needs refresh]".to_string());
    }

    if parts.is_empty() {
        format!("(no usage data){stale_suffix}")
    } else {
        format!("{}   {}", parts.join("   "), stale_suffix.trim_start())
            .trim_end()
            .to_string()
    }
}

/// Format a stale age in seconds into a human-readable git-relative-style string.
///
/// Stale-age computation:
/// - Minutes (up to 60): `Nm ago`, where N = ceil(age / 60).
/// - Hours: `Nh ago`.
/// - Days: `Nd ago`.
pub fn format_stale_age(age_secs: u64) -> String {
    if age_secs < 60 * 60 {
        let minutes = age_secs.div_ceil(60).max(1);
        format!("stale {minutes}m ago")
    } else if age_secs < 60 * 60 * 24 {
        let hours = age_secs / 3600;
        format!("stale {hours}h ago")
    } else {
        let days = age_secs / 86400;
        format!("stale {days}d ago")
    }
}

// ─── AccountPicker ────────────────────────────────────────────────────────────

/// Interactive account picker shown when local usage collection fails.
///
/// Build with `AccountPicker::new(rows)`, then call `AccountPicker::pick()`.
///
/// Degrade: when there is no usable terminal, `pick()` prints a stderr warning
/// and returns `Unavailable` (caller keeps current profile).
pub struct AccountPicker {
    rows: Vec<AccountRow>,
}

impl AccountPicker {
    /// Create a picker from pre-built account rows.
    ///
    /// The list should cover *all* configured profiles (not just those in
    /// the cache).
    pub fn new(rows: Vec<AccountRow>) -> Self {
        Self { rows }
    }

    /// Run the picker and return a [`PickerOutcome`]:
    /// - `Selected(profile_name)` — user selected a profile.
    /// - `Cancelled` — user pressed Escape / Ctrl-C (caller aborts the launch).
    /// - `Unavailable` — empty rows or no usable terminal (caller keeps current).
    ///
    /// No usable terminal → stderr warning + `Unavailable` (degrade).
    pub fn pick(&self) -> PickerOutcome {
        if self.rows.is_empty() {
            return PickerOutcome::Unavailable;
        }
        if !engine::terminal_available() {
            eprintln!(
                "csm: usage collection failed and no interactive terminal — keeping current profile"
            );
            return PickerOutcome::Unavailable;
        }
        let lines = self.build_picker_input();
        engine::run_picker(&lines, &Self::picker_opts())
    }

    /// Build the TSV lines for the picker.
    pub fn build_picker_input(&self) -> Vec<String> {
        self.rows.iter().map(AccountRow::to_tsv).collect()
    }

    /// Picker opts for the account picker.
    ///
    /// Display fields 2.. (profile name hidden), tab delimiter, `account > ` prompt.
    pub fn picker_opts() -> PickerOpts {
        PickerOpts {
            prompt: "account > ".to_string(),
            display_from: 2,
            delimiter: '\t',
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_stale_age_under_one_hour() {
        // 0 s → 1m ago (ceil(0/60)=0, but max(1))
        assert_eq!(format_stale_age(0), "stale 1m ago");
        // 60 s → 1m ago
        assert_eq!(format_stale_age(60), "stale 1m ago");
        // 61 s → 2m ago (ceil(61/60)=2)
        assert_eq!(format_stale_age(61), "stale 2m ago");
        // 119 s → 2m ago
        assert_eq!(format_stale_age(119), "stale 2m ago");
        // 120 s → 2m ago
        assert_eq!(format_stale_age(120), "stale 2m ago");
        // 3540 s (59 min) → 59m ago
        assert_eq!(format_stale_age(3540), "stale 59m ago");
        // 3599 s → 60m ago (one minute short of one hour, rounds up to 60)
        assert_eq!(format_stale_age(3599), "stale 60m ago");
    }

    #[test]
    fn format_stale_age_hours() {
        // 3600 s = 1h
        assert_eq!(format_stale_age(3600), "stale 1h ago");
        // 7200 s = 2h
        assert_eq!(format_stale_age(7200), "stale 2h ago");
        // Just under 24h
        assert_eq!(format_stale_age(86399), "stale 23h ago");
    }

    #[test]
    fn format_stale_age_days() {
        assert_eq!(format_stale_age(86400), "stale 1d ago");
        assert_eq!(format_stale_age(86400 * 2), "stale 2d ago");
    }

    #[test]
    fn account_row_to_tsv_col1_is_profile() {
        let row = AccountRow {
            profile: "home".to_string(),
            display: "session 3%   week 32%".to_string(),
            recommended: false,
        };
        let tsv = row.to_tsv();
        let col1 = tsv.split('\t').next().unwrap();
        assert_eq!(col1, "home");
    }

    #[test]
    fn to_tsv_recommended_row_gets_star_marker() {
        let row = AccountRow {
            profile: "home".to_string(),
            display: "session 3%   week 32%".to_string(),
            recommended: true,
        };
        let tsv = row.to_tsv();
        // col1 (recovery key) must NOT be decorated.
        assert_eq!(tsv.split('\t').next().unwrap(), "home");
        // The display column (col2) leads with the ★ marker.
        let col2 = tsv.split('\t').nth(1).unwrap();
        assert!(col2.starts_with(RECOMMENDED_MARKER), "got: {col2}");
        assert!(col2.contains("session 3%"), "got: {col2}");
    }

    #[test]
    fn to_tsv_plain_row_gets_blank_marker_same_width() {
        let row = AccountRow {
            profile: "home".to_string(),
            display: "session 3%".to_string(),
            recommended: false,
        };
        let col2 = row.to_tsv().split('\t').nth(1).unwrap().to_string();
        assert!(col2.starts_with(PLAIN_MARKER), "got: {col2}");
        assert!(
            !col2.contains('★'),
            "plain row must not have a star: {col2}"
        );
        // Same display width as the star marker keeps columns aligned.
        assert_eq!(
            RECOMMENDED_MARKER.chars().count(),
            PLAIN_MARKER.chars().count()
        );
    }

    #[test]
    fn render_display_error() {
        let data = StaleProfileData {
            session_pct: None,
            week_all_pct: None,
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: Some("no credentials".to_string()),
            attention: None,
        };
        let d = render_display(&data, Some("stale 4m ago"));
        assert!(d.starts_with("[error: no credentials]"), "got: {d}");
        assert!(d.contains("stale 4m ago"), "got: {d}");
    }

    #[test]
    fn render_display_no_data() {
        let data = StaleProfileData {
            session_pct: None,
            week_all_pct: None,
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        };
        let d = render_display(&data, None);
        assert_eq!(d, "(no usage data)");
    }

    #[test]
    fn render_display_full() {
        let data = StaleProfileData {
            session_pct: Some(3),
            week_all_pct: Some(32),
            resets: Some("Jun 18 9pm".to_string()),
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        };
        let d = render_display(&data, Some("stale 4m ago"));
        assert!(d.contains("session 3%"), "got: {d}");
        assert!(d.contains("week 32%"), "got: {d}");
        assert!(d.contains("resets Jun 18 9pm"), "got: {d}");
        assert!(d.contains("stale 4m ago"), "got: {d}");
    }

    #[test]
    fn render_display_needs_refresh_shown_alongside_usage() {
        // A NeedsRefresh profile still carries usable percentages (the
        // access token will self-heal on next use) — the annotation must
        // render alongside them, not replace them the way `[error: …]` does.
        let data = StaleProfileData {
            session_pct: Some(3),
            week_all_pct: Some(32),
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: Some(Attention {
                kind: AttentionKind::NeedsRefresh,
                message: "credentials expired".to_string(),
                action: "csm --profile home".to_string(),
                since_epoch: Some(1_000),
            }),
        };
        let d = render_display(&data, Some("stale 4m ago"));
        assert!(d.contains("session 3%"), "got: {d}");
        assert!(d.contains("[needs refresh]"), "got: {d}");
        assert!(d.contains("stale 4m ago"), "got: {d}");
    }

    #[test]
    fn render_display_partial_no_resets() {
        let data = StaleProfileData {
            session_pct: Some(50),
            week_all_pct: None,
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        };
        let d = render_display(&data, None);
        assert!(d.contains("session 50%"), "got: {d}");
        assert!(!d.contains("week"), "should not include week: {d}");
        assert!(!d.contains("resets"), "should not include resets: {d}");
    }

    #[test]
    fn account_row_build_stale_annotation() {
        // Build a row with a known cache mtime far in the past (1000 seconds ago).
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let old_mtime = now.saturating_sub(300); // 5 minutes ago
        let data = StaleProfileData {
            session_pct: Some(10),
            week_all_pct: Some(20),
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        };
        let row = AccountRow::build("home", &data, Some(old_mtime), false);
        assert_eq!(row.profile, "home");
        // Display should contain the stale annotation (approximately 5m ago).
        assert!(row.display.contains("stale"), "got: {}", row.display);
        // …and MUST lead with the profile name (col1 is hidden via --with-nth=2..,
        // so the name only appears to the user if it is in the display column).
        assert!(
            row.display.starts_with("home"),
            "display must start with profile name, got: {}",
            row.display
        );
        assert!(row.display.contains("session 10%"), "got: {}", row.display);
    }

    #[test]
    fn build_display_leads_with_profile_name_even_on_error() {
        let data = StaleProfileData {
            session_pct: None,
            week_all_pct: None,
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: Some("no credentials".to_string()),
            attention: None,
        };
        let row = AccountRow::build("work", &data, None, false);
        assert!(row.display.starts_with("work"), "got: {}", row.display);
        assert!(
            row.display.contains("[error: no credentials]"),
            "got: {}",
            row.display
        );
        // col1 (recovery key) is the bare profile name, no padding.
        assert_eq!(row.profile, "work");
        let tsv = row.to_tsv();
        assert_eq!(tsv.split('\t').next().unwrap(), "work");
    }

    #[test]
    fn picker_opts_account_picker() {
        let opts = AccountPicker::picker_opts();
        assert_eq!(opts.prompt, "account > ");
        assert_eq!(opts.display_from, 2);
        assert_eq!(opts.delimiter, '\t');
    }

    #[test]
    fn build_picker_input_col1_is_profile() {
        let rows = vec![
            AccountRow {
                profile: "home".to_string(),
                display: "session 5%".to_string(),
                recommended: true,
            },
            AccountRow {
                profile: "work".to_string(),
                display: "session 80%".to_string(),
                recommended: false,
            },
        ];
        let picker = AccountPicker::new(rows);
        let lines = picker.build_picker_input();
        assert_eq!(lines.len(), 2);
        let profiles: Vec<&str> = lines
            .iter()
            .map(|l| l.split('\t').next().unwrap())
            .collect();
        assert_eq!(profiles[0], "home");
        assert_eq!(profiles[1], "work");
    }
}
