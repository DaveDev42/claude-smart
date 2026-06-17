//! Hub-down account picker — interactive fzf account selector.
//!
//! Spec §4a "Hub-down account picker" (Decision #1):
//!
//! When an *interactive* proactive-pick context encounters a usage fetch miss
//! (`Err(FetchError)`) or negative-cache active, the binary opens an fzf picker
//! over configured profiles showing last-known stale usage from `.usage-cache.json`.
//!
//! Trigger: proactive-pick + interactive (isatty(0)&&isatty(1)) + fetch miss.
//! NOT triggered when: non-interactive / hook / `--profile` pin / `--no-pick`.
//!
//! Row format (tab-delimited):
//!   col1 = profile_name (hidden recovery key)
//!   col2+ = display text (session%, week%, resets, stale-age annotation)
//!
//! fzf flags: `--with-nth=2.. --delimiter='\t' --prompt='account > '
//!             --height=40% --reverse --no-multi`
//!
//! Degrade path (Windows / no fzf):
//!   Print to stderr: `csm: hub usage fetch failed and fzf not available — keeping current profile`
//!   Return `None` (caller falls back to current profile).
//!
//! Empty selection (Escape / Ctrl-C, exit 130): return `None`, stderr note.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::picker::fzf::{FzfOpts, fzf_available};

// ─── types ────────────────────────────────────────────────────────────────────

/// Per-profile usage data for stale-cache rendering.
///
/// All fields are `Option` because a hub-down picker may only have partial data
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
    /// Error string if the cache recorded an error for this profile.
    pub error: Option<String>,
}

/// One row of the account picker display.
#[derive(Debug, Clone)]
pub struct AccountRow {
    /// Profile name — the hidden fzf col1 recovery key.
    pub profile: String,
    /// Pre-rendered display string (everything after the tab).
    pub display: String,
}

impl AccountRow {
    /// Render to a tab-delimited fzf input line: `profile\tdisplay`.
    pub fn to_tsv(&self) -> String {
        format!("{}\t{}", self.profile, self.display)
    }

    /// Build an `AccountRow` from a profile name and its stale data.
    ///
    /// Stale-age annotation is appended as `(stale Nm ago)` when `cache_mtime_secs`
    /// is `Some`.
    ///
    /// Spec §4a "Row format / Rendered examples":
    /// ```text
    /// personal   session 3%   week 32%   resets Jun 18 9pm   (stale 4m ago)
    /// work    [error: no credentials]                      (stale 4m ago)
    /// personal   (no usage data)
    /// ```
    pub fn build(
        profile: &str,
        data: &StaleProfileData,
        cache_mtime_secs: Option<u64>,
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
        // could not tell which account each row is. Matches the spec §4a example
        // (`personal   session 3%   …`). Left-pad to a fixed width so the usage
        // columns line up across rows.
        let usage = render_display(data, stale_annotation.as_deref());
        let display = format!("{:<10} {usage}", profile);

        AccountRow {
            profile: profile.to_string(),
            display,
        }
    }
}

/// Render the usage portion (everything after the profile-name column) for a row.
///
/// Spec §4a rendering rules:
/// - error present → `[error: <string>]  (stale Nm ago)`
/// - no data at all → `(no usage data)` (no stale annotation either way for
///   no-data; but stale annotation is still appended if we have a cache mtime)
/// - otherwise: `session <N>%   week <N>%   resets <str>   (stale Nm ago)`
fn render_display(data: &StaleProfileData, stale: Option<&str>) -> String {
    let stale_suffix = stale
        .map(|s| format!("   ({s})"))
        .unwrap_or_default();

    if let Some(ref err) = data.error {
        return format!("[error: {err}]{stale_suffix}");
    }

    // Build the usage part from whatever sections are available.
    let mut parts = Vec::new();

    if let Some(pct) = data.session_pct { parts.push(format!("session {pct}%")) }
    if let Some(pct) = data.week_all_pct { parts.push(format!("week {pct}%")) }
    if let Some(ref resets) = data.resets {
        parts.push(format!("resets {resets}"));
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
/// Spec §4a "Stale-age computation":
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

/// Interactive fzf account picker shown when the hub usage fetch fails.
///
/// Spec §4a "Hub-down account picker".
///
/// Build with `AccountPicker::new(rows)`, then call `AccountPicker::pick()`.
///
/// Degrade: when `fzf_available()` is `false`, `pick()` prints a stderr warning
/// and returns `None` (caller keeps current profile).
pub struct AccountPicker {
    rows: Vec<AccountRow>,
}

impl AccountPicker {
    /// Create a picker from pre-built account rows.
    ///
    /// The list should cover *all* configured profiles (not just those in the
    /// cache), per spec §4a "Profile enumeration".
    pub fn new(rows: Vec<AccountRow>) -> Self {
        Self { rows }
    }

    /// Run the picker and return the selected profile name, or `None`.
    ///
    /// Returns:
    /// - `Some(profile_name)` — user selected a profile.
    /// - `None` — empty/cancelled selection OR fzf unavailable (degraded).
    ///
    /// When `fzf_available()` is false → stderr warning + `None` (degrade).
    /// Escape / Ctrl-C / empty selection → `None`. Otherwise the chosen profile.
    pub fn pick(&self) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        if !fzf_available() {
            eprintln!(
                "csm: hub usage fetch failed and fzf not available — keeping current profile"
            );
            return None;
        }
        let lines = self.build_fzf_input();
        crate::picker::fzf::run_fzf(&lines, &Self::fzf_opts())
    }

    /// Build the TSV lines to pipe to fzf.
    pub fn build_fzf_input(&self) -> Vec<String> {
        self.rows.iter().map(AccountRow::to_tsv).collect()
    }

    /// fzf opts for the account picker.
    ///
    /// Spec: `--with-nth=2.. --delimiter='\t' --prompt='account > ' --height=40%
    /// --reverse --no-multi`
    pub fn fzf_opts() -> FzfOpts {
        FzfOpts {
            prompt: "account > ".to_string(),
            with_nth: "2..".to_string(),
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
            profile: "personal".to_string(),
            display: "session 3%   week 32%".to_string(),
        };
        let tsv = row.to_tsv();
        let col1 = tsv.split('\t').next().unwrap();
        assert_eq!(col1, "personal");
    }

    #[test]
    fn render_display_error() {
        let data = StaleProfileData {
            session_pct: None,
            week_all_pct: None,
            resets: None,
            error: Some("no credentials".to_string()),
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
            error: None,
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
            error: None,
        };
        let d = render_display(&data, Some("stale 4m ago"));
        assert!(d.contains("session 3%"), "got: {d}");
        assert!(d.contains("week 32%"), "got: {d}");
        assert!(d.contains("resets Jun 18 9pm"), "got: {d}");
        assert!(d.contains("stale 4m ago"), "got: {d}");
    }

    #[test]
    fn render_display_partial_no_resets() {
        let data = StaleProfileData {
            session_pct: Some(50),
            week_all_pct: None,
            resets: None,
            error: None,
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
            error: None,
        };
        let row = AccountRow::build("personal", &data, Some(old_mtime));
        assert_eq!(row.profile, "personal");
        // Display should contain the stale annotation (approximately 5m ago).
        assert!(row.display.contains("stale"), "got: {}", row.display);
        // …and MUST lead with the profile name (col1 is hidden via --with-nth=2..,
        // so the name only appears to the user if it is in the display column).
        assert!(
            row.display.starts_with("personal"),
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
            error: Some("no credentials".to_string()),
        };
        let row = AccountRow::build("work", &data, None);
        assert!(
            row.display.starts_with("work"),
            "got: {}",
            row.display
        );
        assert!(row.display.contains("[error: no credentials]"), "got: {}", row.display);
        // col1 (recovery key) is the bare profile name, no padding.
        assert_eq!(row.profile, "work");
        let tsv = row.to_tsv();
        assert_eq!(tsv.split('\t').next().unwrap(), "work");
    }

    #[test]
    fn fzf_opts_account_picker() {
        let opts = AccountPicker::fzf_opts();
        assert_eq!(opts.prompt, "account > ");
        assert_eq!(opts.with_nth, "2..");
        assert_eq!(opts.delimiter, "\t");
    }

    #[test]
    fn build_fzf_input_col1_is_profile() {
        let rows = vec![
            AccountRow {
                profile: "personal".to_string(),
                display: "session 5%".to_string(),
            },
            AccountRow {
                profile: "work".to_string(),
                display: "session 80%".to_string(),
            },
        ];
        let picker = AccountPicker::new(rows);
        let lines = picker.build_fzf_input();
        assert_eq!(lines.len(), 2);
        let profiles: Vec<&str> = lines
            .iter()
            .map(|l| l.split('\t').next().unwrap())
            .collect();
        assert_eq!(profiles[0], "personal");
        assert_eq!(profiles[1], "work");
    }
}
