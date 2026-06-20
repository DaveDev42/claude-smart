//! `csm usage` — multi-profile usage report.
//!
//! Joins the **registry** ([`ProfileMap`]) with the hub's [`UsageData`] blob
//! into one view, one row per profile (registry ∪ hub). A registered profile
//! with no hub data shows `—`/`no data`; a hub profile not in the registry is
//! appended with an `(unregistered)` tag (visibility over silent drop).
//!
//! ## Layering (testability)
//!
//! The pure core — [`build_report`] (join) + [`render_table`] / [`render_json`]
//! (format) — takes a `ProfileMap` + `Option<UsageData>` + freshness/config
//! flags and never touches the network. `fetch()` and stdout live only in
//! `main::cmd_usage`. Every formatting branch is unit-tested against fixtures.
//!
//! ## Status column
//!
//! Reuses the scoring thresholds (`SATURATION_PCT` = 95) so "near-limit" in the
//! table matches what the picker would refuse to launch under:
//! - `Errored`  — the profile is in `UsageData.errors`.
//! - `NoData`   — registered, but absent from the hub (or no sections).
//! - `NearLimit`— any of session/week_all/week_sonnet pct ≥ `WARN_PCT`.
//! - `Ok`       — otherwise.
//!
//! # Spec reference
//! `docs/2026-06-19-csm-usage-and-interactive-cas-edit.md` §1.

use serde::Serialize;

use crate::account::profiles::ProfileMap;
use crate::account::scoring::SATURATION_PCT;
use crate::usage::model::UsageData;

/// pct ≥ this → `NearLimit`. Matches the picker saturation gate so the table's
/// warning and the launcher's refusal agree.
pub const WARN_PCT: i64 = SATURATION_PCT;

// ─── view model ────────────────────────────────────────────────────────────────

/// Per-profile status classification for the STATUS column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Healthy — no section at/over the warn threshold.
    Ok,
    /// At least one quota section is at/over [`WARN_PCT`].
    NearLimit,
    /// The hub reported an error scraping this profile.
    Errored,
    /// Registered but the hub has no row (or no section data) for it.
    NoData,
}

impl Status {
    /// The compact glyph+word used in the human table.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NearLimit => "\u{26a0} near-limit",
            Status::Errored => "\u{2716} errored",
            Status::NoData => "\u{00b7} no data",
        }
    }
}

/// One joined row: a profile's registry membership + its hub usage slice.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Profile name (registry key, or a hub-only name).
    pub name: String,
    /// `true` iff the profile is in the registry (`profiles.json`).
    pub registered: bool,
    /// Session quota pct, or `None` when absent.
    pub session_pct: Option<i64>,
    /// Weekly (all tiers) quota pct, or `None` when absent.
    pub week_all_pct: Option<i64>,
    /// Weekly Sonnet quota pct, or `None` when absent.
    pub week_sonnet_pct: Option<i64>,
    /// A human reset hint (the soonest meaningful `resets` string), or `None`.
    pub resets: Option<String>,
    /// Classified status.
    pub status: Status,
    /// Error message when `status == Errored`.
    pub error: Option<String>,
}

/// The full report: rows + freshness/config metadata for the header/footer.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Joined rows, sorted: registered (by name) first, then unregistered.
    pub rows: Vec<Row>,
    /// Age of the served data in seconds, when known (cache mtime). `None` when
    /// the data is live-fresh (hub-local) or there is no usable data.
    pub stale_secs: Option<u64>,
    /// `true` when metering is configured (hub env present). When `false`, the
    /// usage columns are all `—` and the report explains how to enable it.
    pub configured: bool,
    /// `true` when we have NO usage data at all (fetch failed + no cache, or
    /// unconfigured) — the table shows registry info only.
    pub no_usage: bool,
    /// Top-level capture timestamp from the hub, when present.
    pub captured_at: Option<String>,
}

// ─── pure core: join ─────────────────────────────────────────────────────────

/// Build the joined report from the registry and an optional usage blob.
///
/// `usage = None` means "no usable usage data" (fetch failed with no cache, or
/// metering disabled) — every row gets `NoData`/`—`. `configured` and
/// `stale_secs` are passed in by the caller (which owns the fetch + cache mtime).
///
/// Pure: no I/O, no network, no clock. Fully unit-testable.
pub fn build_report(
    profiles: &ProfileMap,
    usage: Option<&UsageData>,
    configured: bool,
    stale_secs: Option<u64>,
) -> Report {
    let mut rows: Vec<Row> = Vec::new();

    let errors = usage.and_then(|u| u.errors.as_ref());

    // 1. One row per registered profile (so a registered-but-unused profile is
    //    visible as `no data`, never silently dropped).
    for name in profiles.names_sorted() {
        rows.push(join_one(name, true, usage, errors));
    }

    // 2. Append hub profiles NOT in the registry, tagged unregistered.
    if let Some(u) = usage {
        let mut extra: Vec<&str> = u
            .profiles
            .keys()
            .map(String::as_str)
            .filter(|n| !profiles.contains(n))
            .collect();
        // Also surface error-only hub profiles that never produced a row.
        if let Some(errs) = errors {
            for n in errs.keys() {
                if !profiles.contains(n) && !u.profiles.contains_key(n) {
                    extra.push(n.as_str());
                }
            }
        }
        extra.sort_unstable();
        extra.dedup();
        for name in extra {
            rows.push(join_one(name, false, usage, errors));
        }
    }

    let no_usage = usage.is_none();

    Report {
        rows,
        stale_secs,
        configured,
        no_usage,
        captured_at: usage.and_then(|u| u.captured_at.clone()),
    }
}

/// Join a single profile name against the usage blob.
fn join_one(
    name: &str,
    registered: bool,
    usage: Option<&UsageData>,
    errors: Option<&std::collections::HashMap<String, String>>,
) -> Row {
    // Errored profile: short-circuit (no section data trustworthy).
    if let Some(err) = errors.and_then(|e| e.get(name)) {
        return Row {
            name: name.to_owned(),
            registered,
            session_pct: None,
            week_all_pct: None,
            week_sonnet_pct: None,
            resets: None,
            status: Status::Errored,
            error: Some(err.clone()),
        };
    }

    let pu = usage.and_then(|u| u.profiles.get(name));
    let session_pct = pu.and_then(|p| p.session.as_ref()).map(|s| s.pct);
    let week_all_pct = pu.and_then(|p| p.week_all.as_ref()).map(|s| s.pct);
    let week_sonnet_pct = pu.and_then(|p| p.week_sonnet.as_ref()).map(|s| s.pct);

    // Reset hint: prefer the session reset, else the weekly one.
    let resets = pu.and_then(|p| {
        p.session
            .as_ref()
            .and_then(|s| s.resets.clone())
            .or_else(|| p.week_all.as_ref().and_then(|s| s.resets.clone()))
    });

    let has_any = session_pct.is_some() || week_all_pct.is_some() || week_sonnet_pct.is_some();
    let status = if !has_any {
        Status::NoData
    } else if [session_pct, week_all_pct, week_sonnet_pct]
        .into_iter()
        .flatten()
        .any(|p| p >= WARN_PCT)
    {
        Status::NearLimit
    } else {
        Status::Ok
    };

    Row {
        name: name.to_owned(),
        registered,
        session_pct,
        week_all_pct,
        week_sonnet_pct,
        resets,
        status,
        error: None,
    }
}

// ─── pure core: human table render ──────────────────────────────────────────

/// Render the human-readable table (with header/footer lines) to a `String`.
///
/// Pure: returns the full multi-line block; the caller prints it. This keeps
/// every formatting branch unit-testable.
pub fn render_table(report: &Report) -> String {
    let mut out = String::new();

    // Stale header (offline degrade — first-class, per spec §1).
    if let Some(age) = report.stale_secs {
        // Only warn once the data is meaningfully old (> one positive-TTL window).
        if age >= 60 {
            out.push_str(&format!(
                "\u{26a0} hub data is {} old (showing last-known cache)\n",
                humanize_age(age)
            ));
        }
    }

    if !report.configured {
        out.push_str(
            "usage metering disabled — set CLAUDE_USAGE_URL + CLAUDE_HUB_HOSTNAME to enable\n",
        );
    }

    if report.rows.is_empty() {
        out.push_str("(no profiles configured — `csm profiles add <name>`)\n");
        return out;
    }

    // Column widths (name column sizes to content, min 8).
    let name_w = report
        .rows
        .iter()
        .map(|r| display_name(r).len())
        .max()
        .unwrap_or(8)
        .max(8);

    out.push_str(&format!(
        "{:<nw$}  {:>7}  {:>9}  {:>9}  {:<20}  {}\n",
        "PROFILE",
        "SESSION",
        "WEEK(all)",
        "WK(sonnet)",
        "RESETS",
        "STATUS",
        nw = name_w,
    ));

    for r in &report.rows {
        out.push_str(&format!(
            "{:<nw$}  {:>7}  {:>9}  {:>9}  {:<20}  {}\n",
            display_name(r),
            pct(r.session_pct),
            pct(r.week_all_pct),
            pct(r.week_sonnet_pct),
            truncate(r.resets.as_deref().unwrap_or("\u{2014}"), 20),
            status_cell(r),
            nw = name_w,
        ));
    }

    if report.no_usage && report.configured {
        out.push_str("usage metering unavailable (hub unreachable, no cache)\n");
    }

    out
}

/// The name as shown in the table (unregistered profiles get a tag).
fn display_name(r: &Row) -> String {
    if r.registered {
        r.name.clone()
    } else {
        format!("{} (unreg)", r.name)
    }
}

/// Status cell: the label, with the error message appended for errored rows.
fn status_cell(r: &Row) -> String {
    match (&r.status, &r.error) {
        (Status::Errored, Some(msg)) => format!("{}: {}", Status::Errored.label(), msg),
        (s, _) => s.label().to_owned(),
    }
}

/// Format an optional percent as `NN%` or the em-dash placeholder.
fn pct(v: Option<i64>) -> String {
    match v {
        Some(p) => format!("{p}%"),
        None => "\u{2014}".to_owned(),
    }
}

/// Truncate `s` to `max` display chars (ASCII-safe; resets strings are ASCII).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

/// Humanize an age in seconds → `"7m"`, `"3h"`, `"2d"`, `"45s"`.
fn humanize_age(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

// ─── pure core: JSON render ────────────────────────────────────────────────────

/// The stable `--json` wire shape (decoupled from the internal `Report`/`Row`
/// structs so renames there don't silently break the JSON contract).
#[derive(Debug, Serialize)]
struct JsonReport<'a> {
    captured_at: Option<&'a str>,
    stale_secs: Option<u64>,
    configured: bool,
    profiles: std::collections::BTreeMap<&'a str, JsonRow<'a>>,
}

#[derive(Debug, Serialize)]
struct JsonRow<'a> {
    registered: bool,
    session_pct: Option<i64>,
    week_all_pct: Option<i64>,
    week_sonnet_pct: Option<i64>,
    resets: Option<&'a str>,
    status: Status,
    error: Option<&'a str>,
}

/// Render the report as pretty JSON (stable key order via `BTreeMap`).
pub fn render_json(report: &Report) -> Result<String, serde_json::Error> {
    let profiles: std::collections::BTreeMap<&str, JsonRow> = report
        .rows
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                JsonRow {
                    registered: r.registered,
                    session_pct: r.session_pct,
                    week_all_pct: r.week_all_pct,
                    week_sonnet_pct: r.week_sonnet_pct,
                    resets: r.resets.as_deref(),
                    status: r.status,
                    error: r.error.as_deref(),
                },
            )
        })
        .collect();

    let wire = JsonReport {
        captured_at: report.captured_at.as_deref(),
        stale_secs: report.stale_secs,
        configured: report.configured,
        profiles,
    };
    serde_json::to_string_pretty(&wire)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn registry(names: &[&str]) -> ProfileMap {
        let mut m = HashMap::new();
        for n in names {
            m.insert((*n).to_owned(), format!("/tmp/.claude.{n}"));
        }
        ProfileMap(m)
    }

    /// A usage blob: `home` ok, `work` near-limit (week_all=96), one
    /// errored profile, plus a hub-only `ghost` profile.
    fn sample_usage() -> UsageData {
        serde_json::from_str(
            r#"{
              "captured_at": "2026-06-19T07:00:00Z",
              "profiles": {
                "home": {
                  "session": {"pct": 12, "resets": "9pm (Asia/Seoul)"},
                  "week_all": {"pct": 34, "resets": "Jun 22"},
                  "week_sonnet": {"pct": 8}
                },
                "work": {
                  "session": {"pct": 40},
                  "week_all": {"pct": 96, "resets": "Jun 22"},
                  "week_sonnet": null
                },
                "ghost": {
                  "session": {"pct": 3},
                  "week_all": {"pct": 5}
                }
              },
              "errors": { "errored-acct": "HTTP 401: no credentials" }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn join_classifies_status_per_profile() {
        let reg = registry(&["home", "work", "errored-acct"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, Some(5));

        let by = |n: &str| report.rows.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(by("home").status, Status::Ok);
        assert_eq!(by("home").session_pct, Some(12));
        assert_eq!(by("work").status, Status::NearLimit); // week_all=96 ≥ 95
        assert_eq!(by("errored-acct").status, Status::Errored);
        assert_eq!(
            by("errored-acct").error.as_deref(),
            Some("HTTP 401: no credentials")
        );
    }

    #[test]
    fn registered_without_hub_data_is_no_data() {
        let reg = registry(&["home", "lonely"]);
        let u = sample_usage(); // has home but not "lonely"
        let report = build_report(&reg, Some(&u), true, None);
        let lonely = report.rows.iter().find(|r| r.name == "lonely").unwrap();
        assert_eq!(lonely.status, Status::NoData);
        assert!(lonely.registered);
        assert_eq!(lonely.session_pct, None);
    }

    #[test]
    fn unregistered_hub_profile_is_appended_and_tagged() {
        let reg = registry(&["home", "work"]); // ghost not registered
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let ghost = report.rows.iter().find(|r| r.name == "ghost").unwrap();
        assert!(!ghost.registered);
        // Registered rows come first, unregistered after.
        let names: Vec<&str> = report.rows.iter().map(|r| r.name.as_str()).collect();
        let ghost_idx = names.iter().position(|n| *n == "ghost").unwrap();
        let home_idx = names.iter().position(|n| *n == "home").unwrap();
        assert!(
            ghost_idx > home_idx,
            "unregistered must sort after registered: {names:?}"
        );
        // And the rendered name carries the tag.
        let table = render_table(&report);
        assert!(table.contains("ghost (unreg)"), "table:\n{table}");
    }

    #[test]
    fn error_only_hub_profile_surfaces_as_row() {
        // "errored-acct" is errored and has NO profiles entry — it must still appear.
        let reg = registry(&["home"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let errored = report.rows.iter().find(|r| r.name == "errored-acct");
        assert!(errored.is_some(), "error-only hub profile must surface");
        assert_eq!(errored.unwrap().status, Status::Errored);
    }

    #[test]
    fn no_usage_blob_makes_every_registered_row_no_data() {
        let reg = registry(&["home", "work"]);
        let report = build_report(&reg, None, true, None);
        assert!(report.no_usage);
        assert_eq!(report.rows.len(), 2);
        assert!(report.rows.iter().all(|r| r.status == Status::NoData));
        let table = render_table(&report);
        assert!(
            table.contains("usage metering unavailable"),
            "table:\n{table}"
        );
    }

    #[test]
    fn unconfigured_shows_disabled_message_and_registry() {
        let reg = registry(&["home"]);
        let report = build_report(&reg, None, /*configured=*/ false, None);
        let table = render_table(&report);
        assert!(table.contains("usage metering disabled"), "table:\n{table}");
        assert!(
            table.contains("home"),
            "registry must still render: {table}"
        );
        // Disabled never claims the hub is "unavailable" (that's a different state).
        assert!(!table.contains("unavailable"), "table:\n{table}");
    }

    #[test]
    fn empty_registry_renders_hint() {
        let reg = registry(&[]);
        let report = build_report(&reg, None, false, None);
        let table = render_table(&report);
        assert!(table.contains("no profiles configured"), "table:\n{table}");
    }

    #[test]
    fn stale_header_only_when_old() {
        let reg = registry(&["home"]);
        let u = sample_usage();
        // Fresh (5s): no stale header.
        let fresh = render_table(&build_report(&reg, Some(&u), true, Some(5)));
        assert!(
            !fresh.contains("hub data is"),
            "fresh should not warn: {fresh}"
        );
        // Old (7m): stale header present.
        let stale = render_table(&build_report(&reg, Some(&u), true, Some(420)));
        assert!(stale.contains("hub data is 7m old"), "stale:\n{stale}");
    }

    #[test]
    fn humanize_age_units() {
        assert_eq!(humanize_age(45), "45s");
        assert_eq!(humanize_age(420), "7m");
        assert_eq!(humanize_age(7_200), "2h");
        assert_eq!(humanize_age(172_800), "2d");
    }

    #[test]
    fn json_shape_is_stable_and_sorted() {
        let reg = registry(&["home", "work"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, Some(30));
        let json = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(v["configured"], serde_json::json!(true));
        assert_eq!(v["stale_secs"], serde_json::json!(30));
        assert_eq!(v["captured_at"], serde_json::json!("2026-06-19T07:00:00Z"));
        // BTreeMap → keys sorted: errored-acct before ghost before home before work.
        let keys: Vec<&str> = v["profiles"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "json profile keys must be sorted: {keys:?}");
        // Status serializes snake_case.
        assert_eq!(
            v["profiles"]["work"]["status"],
            serde_json::json!("near_limit")
        );
        assert_eq!(v["profiles"]["home"]["session_pct"], serde_json::json!(12));
        assert_eq!(
            v["profiles"]["errored-acct"]["status"],
            serde_json::json!("errored")
        );
        assert_eq!(
            v["profiles"]["errored-acct"]["error"],
            serde_json::json!("HTTP 401: no credentials")
        );
    }

    #[test]
    fn truncate_respects_max() {
        assert_eq!(truncate("short", 20), "short");
        let long = "this is a very long reset string indeed";
        let t = truncate(long, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('\u{2026}'));
    }
}
