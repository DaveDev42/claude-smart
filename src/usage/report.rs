//! `csm usage` — multi-profile usage report.
//!
//! Joins the **registry** ([`ProfileMap`]) with the local per-profile
//! [`UsageData`] store into one view, one row per profile (registry ∪ store).
//! A registered profile with no local data shows `—`/`no data`; a profile
//! present in the store but not in the registry is appended with an
//! `(unregistered)` tag (visibility over silent drop).
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
//! - `NoData`   — registered, but absent from the local store (or no sections).
//! - `NearLimit`— any of session/week_all/week_fable pct ≥ `WARN_PCT`.
//! - `Ok`       — otherwise.
//!
//! # Spec reference
//! `dave-environment docs/superpowers/specs/2026-06-19-csm-usage-and-interactive-cas-edit.md` §1.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::account::profiles::ProfileMap;
use crate::account::scoring::SATURATION_PCT;
use crate::usage::model::{Attention, AttentionKind, UsageData};

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
    /// Local collection reported an error for this profile.
    Errored,
    /// Registered but the local store has no row (or no section data) for it.
    NoData,
    /// Access token expired but the refresh token is alive — see
    /// [`AttentionKind::NeedsRefresh`]. Takes priority over every other
    /// status: `join_one` checks `attention` before `errors`/section data.
    RefreshNeeded,
    /// Credentials are dead (refresh token dead/absent, never logged in, or
    /// the server rejected the token) — see [`AttentionKind::NeedsLogin`].
    LoginRequired,
}

impl Status {
    /// The compact glyph+word used in the human table. `RefreshNeeded`'s exact
    /// text (with the stale age) is built by [`status_cell`] instead — this
    /// bare label is only ever seen for it in a context with no `Row` at hand.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NearLimit => "\u{26a0} near-limit",
            Status::Errored => "\u{2716} errored",
            Status::NoData => "\u{00b7} no data",
            Status::RefreshNeeded => "REFRESH NEEDED",
            Status::LoginRequired => "LOGIN REQUIRED",
        }
    }
}

/// One joined row: a profile's registry membership + its local usage slice.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Profile name (registry key, or a store-only name).
    pub name: String,
    /// `true` iff the profile is in the registry (`profiles.json`).
    pub registered: bool,
    /// Session quota pct, or `None` when absent.
    pub session_pct: Option<i64>,
    /// Weekly (all tiers) quota pct, or `None` when absent.
    pub week_all_pct: Option<i64>,
    /// Weekly per-model-tier (Fable) quota pct, or `None` when absent.
    pub week_fable_pct: Option<i64>,
    /// The session (5h block) reset hint — a bare local time such as
    /// `11:50pm (Asia/Seoul)` (the upstream `/usage` gauge never dates it), or
    /// `None`.
    pub session_resets: Option<String>,
    /// Machine-native epoch backing `session_resets` (`UsageSection::resets_at`),
    /// when the local collector recorded one. `None` for an older cache record
    /// or an absent session section.
    pub session_resets_at: Option<i64>,
    /// The weekly (all tiers) reset hint — carries the date, e.g.
    /// `Jul 9 at 9pm (Asia/Seoul)`, or `None`. Shown separately from the
    /// session hint: collapsing the two into one column made a same-day 5h
    /// reset read as the *weekly* reset.
    pub week_all_resets: Option<String>,
    /// Machine-native epoch backing `week_all_resets`. `None` for an older
    /// cache record or an absent week_all section.
    pub week_all_resets_at: Option<i64>,
    /// Classified status.
    pub status: Status,
    /// Error message when `status == Errored`.
    pub error: Option<String>,
    /// Set when `status` is `RefreshNeeded`/`LoginRequired` — the full
    /// warning, consumed by [`status_cell`] (STATUS cell text) and
    /// [`attention_lines`] (the footer block) and serialized verbatim into
    /// `--json`.
    pub attention: Option<Attention>,
}

/// The full report: rows + freshness/config metadata for the header/footer.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Joined rows, sorted: registered (by name) first, then unregistered.
    pub rows: Vec<Row>,
    /// Age of the served data in seconds, when known — derived from the
    /// data's own per-profile `captured_at` timestamps (the least-fresh
    /// served profile), never from a cache file's mtime (see
    /// `local::oldest_profile_age_secs`). `None` when the data is live-fresh
    /// (just collected) or there is no usable data.
    pub stale_secs: Option<u64>,
    /// `true` when metering is configured — the registry is non-empty. The
    /// real caller (`main::cmd_usage`) only ever produces `false` alongside
    /// an empty registry, in which case `rows` is also empty and the
    /// `rows.is_empty()` branch in `render_table` already explains why
    /// ("no profiles configured — `csm profiles add <name>`"); `configured`
    /// itself drives no separate footer. `build_report`'s pure signature does
    /// accept `configured=false` with a non-empty `rows` (exercised directly
    /// by this module's own tests) — that combination renders the usage
    /// columns as `—` with no further explanation, since it is not a shape
    /// any real call site produces.
    pub configured: bool,
    /// `true` when we have NO usage data at all (fetch failed + no cache, or
    /// an empty registry) — the table shows registry info only.
    pub no_usage: bool,
    /// Top-level capture timestamp from local collection, when present.
    pub captured_at: Option<String>,
    /// Which model tier the `week_fable_pct` column actually measures, as
    /// reported by the OAuth usage API (e.g. `"Fable"`). `None` when the
    /// record predates the field or carried no label; the column then falls
    /// back to its baked-in name.
    pub week_model_label: Option<String>,
}

// ─── pure core: join ─────────────────────────────────────────────────────────

/// Build the joined report from the registry and an optional usage blob.
///
/// `usage = None` means "no usable usage data" (fetch failed with no cache, or
/// an empty registry) — every row gets `NoData`/`—`. `configured` and
/// `stale_secs` are passed in by the caller (which owns the fetch and derives
/// `stale_secs` from the served data's own `captured_at` timestamps).
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

    // 2. Append store profiles NOT in the registry, tagged unregistered.
    if let Some(u) = usage {
        let mut extra: Vec<&str> = u
            .profiles
            .keys()
            .map(String::as_str)
            .filter(|n| !profiles.contains(n))
            .collect();
        // Also surface error-only store profiles that never produced a row.
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
        week_model_label: tier_label(usage),
    }
}

/// The tier the per-model weekly column measures, e.g. `"Fable"`.
///
/// Taken from the OAuth usage API response rather than baked in here:
/// Anthropic renames the separately-capped tier (the `claude /usage` row read
/// "Sonnet only" until 2026-07 and "Fable" after), and a hardcoded header
/// would go on advertising a tier that is no longer the one being metered —
/// the same silent-staleness this field exists to end. Every profile is
/// metered under the same tier, so the first label wins; sorted-name order
/// keeps that pick stable across `HashMap` iteration orders.
fn tier_label(usage: Option<&UsageData>) -> Option<String> {
    let u = usage?;
    let mut names: Vec<&str> = u.profiles.keys().map(String::as_str).collect();
    names.sort_unstable();
    names
        .into_iter()
        .find_map(|n| u.profiles[n].week_model_label.clone())
}

/// Join a single profile name against the usage blob.
///
/// `attention` is checked BEFORE `errors` — a `NeedsLogin` profile is
/// recorded in both (see `local::mod`'s module doc, "exclusion mechanism"),
/// and if `errors` were checked first this row would render as a bare
/// `Errored` row with no numbers, losing exactly the stale numbers the design
/// spec requires the table to keep showing. A profile carrying `attention`
/// therefore always renders as ONE row with whatever numbers it has (real or
/// none), never a separate `Errored` row.
fn join_one(
    name: &str,
    registered: bool,
    usage: Option<&UsageData>,
    errors: Option<&std::collections::HashMap<String, String>>,
) -> Row {
    let pu = usage.and_then(|u| u.profiles.get(name));

    if let Some(attention) = pu.and_then(|p| p.attention.clone()) {
        let status = match attention.kind {
            AttentionKind::NeedsRefresh => Status::RefreshNeeded,
            AttentionKind::NeedsLogin => Status::LoginRequired,
        };
        let session_pct = pu.and_then(|p| p.session.as_ref()).map(|s| s.pct);
        let week_all_pct = pu.and_then(|p| p.week_all.as_ref()).map(|s| s.pct);
        let week_fable_pct = pu.and_then(|p| p.week_fable.as_ref()).map(|s| s.pct);
        let session_resets = pu
            .and_then(|p| p.session.as_ref())
            .and_then(|s| s.resets.clone());
        let session_resets_at = pu
            .and_then(|p| p.session.as_ref())
            .and_then(|s| s.resets_at);
        let week_all_resets = pu
            .and_then(|p| p.week_all.as_ref())
            .and_then(|s| s.resets.clone());
        let week_all_resets_at = pu
            .and_then(|p| p.week_all.as_ref())
            .and_then(|s| s.resets_at);
        return Row {
            name: name.to_owned(),
            registered,
            session_pct,
            week_all_pct,
            week_fable_pct,
            session_resets,
            session_resets_at,
            week_all_resets,
            week_all_resets_at,
            status,
            error: None,
            attention: Some(attention),
        };
    }

    // Errored profile (no attention — a plain fetch/rate-limit failure):
    // short-circuit, no section data trustworthy.
    if let Some(err) = errors.and_then(|e| e.get(name)) {
        return Row {
            name: name.to_owned(),
            registered,
            session_pct: None,
            week_all_pct: None,
            week_fable_pct: None,
            session_resets: None,
            session_resets_at: None,
            week_all_resets: None,
            week_all_resets_at: None,
            status: Status::Errored,
            error: Some(err.clone()),
            attention: None,
        };
    }

    let session_pct = pu.and_then(|p| p.session.as_ref()).map(|s| s.pct);
    let week_all_pct = pu.and_then(|p| p.week_all.as_ref()).map(|s| s.pct);
    let week_fable_pct = pu.and_then(|p| p.week_fable.as_ref()).map(|s| s.pct);

    // Reset hints: session and weekly are separate facts — never collapse them
    // into one field (a dateless session time masquerading as the weekly reset
    // is exactly the misread the split exists to prevent).
    let session_resets = pu
        .and_then(|p| p.session.as_ref())
        .and_then(|s| s.resets.clone());
    let session_resets_at = pu
        .and_then(|p| p.session.as_ref())
        .and_then(|s| s.resets_at);
    let week_all_resets = pu
        .and_then(|p| p.week_all.as_ref())
        .and_then(|s| s.resets.clone());
    let week_all_resets_at = pu
        .and_then(|p| p.week_all.as_ref())
        .and_then(|s| s.resets_at);

    let has_any = session_pct.is_some() || week_all_pct.is_some() || week_fable_pct.is_some();
    let status = if !has_any {
        Status::NoData
    } else if [session_pct, week_all_pct, week_fable_pct]
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
        week_fable_pct,
        session_resets,
        session_resets_at,
        week_all_resets,
        week_all_resets_at,
        status,
        error: None,
        attention: None,
    }
}

// ─── pure core: human table render ──────────────────────────────────────────

/// Render the human-readable table (with header/footer lines) to a `String`.
///
/// Pure: returns the full multi-line block; the caller prints it. This keeps
/// every formatting branch unit-testable.
///
/// `now` is used only to turn each attentive row's `since_epoch` into a
/// freshly-computed relative age (`status_cell`'s `(stale 3d)`,
/// `attention_lines`'s `3d ago`) — never baked into the `Row`/`Attention`
/// data itself, so a cached `Report` renders its true current age however
/// long it's been sitting in `.usage-cache.json` (see
/// `model::Attention`'s doc). This is a deliberate signature addition beyond
/// the design spec's plain `render_table(report)` — the alternative (baking
/// a rendered age string into `Attention` at collection time) would go stale
/// exactly like the one-line stderr hint this feature replaces.
pub fn render_table(report: &Report, now: DateTime<Utc>) -> String {
    let mut out = String::new();

    // Stale header (offline degrade — first-class, per spec §1).
    if let Some(age) = report.stale_secs {
        // Only warn once the data is meaningfully old (> one positive-TTL window).
        if age >= 60 {
            out.push_str(&format!(
                "\u{26a0} usage data is {} old (showing last-known cache)\n",
                humanize_age(age)
            ));
        }
    }

    if report.rows.is_empty() {
        out.push_str("(no profiles configured — `csm profiles add <name>`)\n");
        return out;
    }

    // Column widths (name and both resets columns size to content; resets are
    // capped so one pathological string cannot blow the table apart, and
    // floored at the header width).
    const RESETS_MAX_W: usize = 30;
    let name_w = report
        .rows
        .iter()
        .map(|r| display_name(r).len())
        .max()
        .unwrap_or(8)
        .max(8);
    let resets_w = |f: fn(&Row) -> Option<&str>| {
        report
            .rows
            .iter()
            .map(|r| f(r).unwrap_or("\u{2014}").chars().count())
            .max()
            .unwrap_or(0)
            .clamp("RESETS(sess)".len(), RESETS_MAX_W)
    };
    let sess_w = resets_w(|r| r.session_resets.as_deref());
    let week_w = resets_w(|r| r.week_all_resets.as_deref());

    // The per-model weekly column is headed with the tier the API actually
    // reported, so a tier rename retitles the column instead of mislabelling it.
    // Truncated because the label is free text off a TUI row, and floored at the
    // legacy width so the table keeps its shape for the common short names.
    const TIER_MAX_W: usize = 14;
    let tier_head = format!(
        "WK({})",
        truncate(
            report.week_model_label.as_deref().unwrap_or("fable"),
            TIER_MAX_W
        )
    );
    let tier_w = tier_head.chars().count().max(9);

    out.push_str(&format!(
        "{:<nw$}  {:>7}  {:>9}  {:>tw$}  {:<sw$}  {:<ww$}  {}\n",
        "PROFILE",
        "SESSION",
        "WEEK(all)",
        tier_head,
        "RESETS(sess)",
        "RESETS(week)",
        "STATUS",
        nw = name_w,
        tw = tier_w,
        sw = sess_w,
        ww = week_w,
    ));

    for r in &report.rows {
        out.push_str(&format!(
            "{:<nw$}  {:>7}  {:>9}  {:>tw$}  {:<sw$}  {:<ww$}  {}\n",
            display_name(r),
            pct(r.session_pct),
            pct(r.week_all_pct),
            pct(r.week_fable_pct),
            truncate(r.session_resets.as_deref().unwrap_or("\u{2014}"), sess_w),
            truncate(r.week_all_resets.as_deref().unwrap_or("\u{2014}"), week_w),
            status_cell(r, now),
            nw = name_w,
            tw = tier_w,
            sw = sess_w,
            ww = week_w,
        ));
    }

    if report.no_usage && report.configured {
        out.push_str("usage data unavailable (no readable credentials, no cache)\n");
    }

    // Footer block: one profile's warning would otherwise be truncated by the
    // STATUS column's width, or (for LoginRequired, whose cell carries no
    // age/action) not shown at all outside the bare label — the footer is
    // where the full message + the exact copy-pasteable command live.
    for line in attention_lines(&report.rows, now) {
        out.push_str(&line);
        out.push('\n');
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

/// Status cell: the label, with the error message appended for errored rows,
/// or a freshly-computed stale age appended for `RefreshNeeded` rows (e.g.
/// `REFRESH NEEDED (stale 3d)`). `LoginRequired` renders as the bare label —
/// its full message/age/action live in the footer (`attention_lines`) since
/// `CLAUDE_CONFIG_DIR=<dir> claude auth login` is far too long for a column.
fn status_cell(r: &Row, now: DateTime<Utc>) -> String {
    match (&r.status, &r.error, &r.attention) {
        (Status::RefreshNeeded, _, Some(att)) => {
            format!(
                "{} (stale {})",
                Status::RefreshNeeded.label(),
                humanize_age(attention_age_secs(att, now))
            )
        }
        (Status::Errored, Some(msg), _) => format!("{}: {}", Status::Errored.label(), msg),
        (s, _, _) => s.label().to_owned(),
    }
}

/// Seconds elapsed since `attention.since_epoch`, clamped at 0. `0` when
/// `since_epoch` is absent (an API 401/403 or `NotFound` carries no local
/// expiry instant — see `model::Attention::since_epoch`'s doc) rather than
/// panicking or fabricating an age; callers needing to distinguish "no known
/// age" should check `attention.since_epoch` directly.
fn attention_age_secs(attention: &Attention, now: DateTime<Utc>) -> u64 {
    attention
        .since_epoch
        .map(|epoch| (now.timestamp() - epoch).max(0) as u64)
        .unwrap_or(0)
}

/// The two-line footer block for one profile's `attention`:
/// ```text
/// ⚠ work: credentials expired 3d ago — login required
///   CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login
/// ```
/// The age suffix (` 3d ago`) is only appended when `since_epoch` is known —
/// a `NotFound`/401-403 warning has no local expiry instant to report (see
/// `Attention::since_epoch`'s doc), so its line reads `not logged in — login
/// required` with no age.
///
/// Public (not just `report::render_table`'s internal footer builder) so
/// `main`'s launch-time surface (design spec "csm run 실행 시점") can render
/// the identical block straight from a cached `UsageData`'s per-profile
/// `attention`, without needing a full `Row`/`Report` join.
pub fn attention_block_lines(name: &str, attention: &Attention, now: DateTime<Utc>) -> [String; 2] {
    let age_part = attention
        .since_epoch
        .map(|_| format!(" {} ago", humanize_age(attention_age_secs(attention, now))))
        .unwrap_or_default();
    let action_word = match attention.kind {
        AttentionKind::NeedsLogin => "login required",
        AttentionKind::NeedsRefresh => "run once to refresh",
    };
    let first = format!(
        "\u{26a0} {name}: {}{age_part} \u{2014} {action_word}",
        attention.message
    );
    let second = format!("  {}", attention.action);
    [first, second]
}

/// `None` when the row carries no `attention`.
fn format_attention_block(row: &Row, now: DateTime<Utc>) -> Option<[String; 2]> {
    let attention = row.attention.as_ref()?;
    Some(attention_block_lines(&row.name, attention, now))
}

/// The full footer block across every row that carries an `attention` —
/// `render_table` appends this after the table body.
pub fn attention_lines(rows: &[Row], now: DateTime<Utc>) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows {
        if let Some(lines) = format_attention_block(row, now) {
            out.extend(lines);
        }
    }
    out
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
    /// Which tier every row's `week_fable_pct` measures, as reported by the
    /// local collector.
    week_model_label: Option<&'a str>,
    profiles: std::collections::BTreeMap<&'a str, JsonRow<'a>>,
}

#[derive(Debug, Serialize)]
struct JsonRow<'a> {
    registered: bool,
    session_pct: Option<i64>,
    week_all_pct: Option<i64>,
    week_fable_pct: Option<i64>,
    session_resets: Option<&'a str>,
    /// Machine-native epoch backing `session_resets`. Omitted (not `null`)
    /// when absent — an older cache record predates this field, and a JSON
    /// consumer distinguishing "field never existed" from "field is null" is
    /// exactly what `skip_serializing_if` avoids forcing on them.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_resets_at: Option<i64>,
    week_all_resets: Option<&'a str>,
    /// Machine-native epoch backing `week_all_resets`. Same omit-when-absent
    /// rationale as `session_resets_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    week_all_resets_at: Option<i64>,
    status: Status,
    error: Option<&'a str>,
    /// Credential warning, verbatim — see [`Attention`]. Omitted (not `null`)
    /// for a healthy row, so an older consumer that has never heard of this
    /// field sees exactly the JSON shape it always has.
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<&'a Attention>,
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
                    week_fable_pct: r.week_fable_pct,
                    session_resets: r.session_resets.as_deref(),
                    session_resets_at: r.session_resets_at,
                    week_all_resets: r.week_all_resets.as_deref(),
                    week_all_resets_at: r.week_all_resets_at,
                    status: r.status,
                    error: r.error.as_deref(),
                    attention: r.attention.as_ref(),
                },
            )
        })
        .collect();

    let wire = JsonReport {
        captured_at: report.captured_at.as_deref(),
        stale_secs: report.stale_secs,
        configured: report.configured,
        week_model_label: report.week_model_label.as_deref(),
        profiles,
    };
    serde_json::to_string_pretty(&wire)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn now() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    fn registry(names: &[&str]) -> ProfileMap {
        let mut m = HashMap::new();
        for n in names {
            m.insert((*n).to_owned(), format!("/tmp/.claude.{n}"));
        }
        ProfileMap(m)
    }

    /// A usage blob: `home` ok, `work` near-limit (week_all=96), one
    /// errored profile, plus a store-only `ghost` profile.
    fn sample_usage() -> UsageData {
        serde_json::from_str(
            r#"{
              "captured_at": "2026-06-19T07:00:00Z",
              "profiles": {
                "home": {
                  "session": {"pct": 12, "resets": "9pm (Asia/Seoul)"},
                  "week_all": {"pct": 34, "resets": "Jun 22"},
                  "week_fable": {"pct": 8}
                },
                "work": {
                  "session": {"pct": 40},
                  "week_all": {"pct": 96, "resets": "Jun 22"},
                  "week_fable": null
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
    fn resets_columns_are_separate_session_and_week_facts() {
        let reg = registry(&["home", "work"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);

        let by = |n: &str| report.rows.iter().find(|r| r.name == n).unwrap().clone();
        // home carries both hints; they must land in their own fields.
        assert_eq!(
            by("home").session_resets.as_deref(),
            Some("9pm (Asia/Seoul)")
        );
        assert_eq!(by("home").week_all_resets.as_deref(), Some("Jun 22"));
        // work has a weekly hint but NO session hint — the weekly string must
        // not leak into the session slot (and vice versa).
        assert_eq!(by("work").session_resets, None);
        assert_eq!(by("work").week_all_resets.as_deref(), Some("Jun 22"));

        let table = render_table(&report, now());
        assert!(table.contains("RESETS(sess)"), "table:\n{table}");
        assert!(table.contains("RESETS(week)"), "table:\n{table}");
        assert!(table.contains("9pm (Asia/Seoul)"), "table:\n{table}");
        assert!(table.contains("Jun 22"), "table:\n{table}");
        // work's missing session hint renders as the em-dash placeholder.
        let work_line = table.lines().find(|l| l.starts_with("work")).unwrap();
        assert!(work_line.contains('\u{2014}'), "work line: {work_line}");
    }

    #[test]
    fn registered_without_usage_data_is_no_data() {
        let reg = registry(&["home", "lonely"]);
        let u = sample_usage(); // has home but not "lonely"
        let report = build_report(&reg, Some(&u), true, None);
        let lonely = report.rows.iter().find(|r| r.name == "lonely").unwrap();
        assert_eq!(lonely.status, Status::NoData);
        assert!(lonely.registered);
        assert_eq!(lonely.session_pct, None);
    }

    #[test]
    fn unregistered_usage_profile_is_appended_and_tagged() {
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
        let table = render_table(&report, now());
        assert!(table.contains("ghost (unreg)"), "table:\n{table}");
    }

    #[test]
    fn error_only_usage_profile_surfaces_as_row() {
        // "errored-acct" is errored and has NO profiles entry — it must still appear.
        let reg = registry(&["home"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let errored = report.rows.iter().find(|r| r.name == "errored-acct");
        assert!(errored.is_some(), "error-only usage profile must surface");
        assert_eq!(errored.unwrap().status, Status::Errored);
    }

    #[test]
    fn no_usage_blob_makes_every_registered_row_no_data() {
        let reg = registry(&["home", "work"]);
        let report = build_report(&reg, None, true, None);
        assert!(report.no_usage);
        assert_eq!(report.rows.len(), 2);
        assert!(report.rows.iter().all(|r| r.status == Status::NoData));
        let table = render_table(&report, now());
        assert!(
            table.contains("usage data unavailable (no readable credentials, no cache)"),
            "table:\n{table}"
        );
    }

    #[test]
    fn unconfigured_with_nonempty_registry_renders_rows_with_no_footer() {
        // `configured=false` is only ever produced by `main::cmd_usage` for an
        // EMPTY registry (see `empty_registry_renders_hint` for that realistic
        // case) — this pins the pure core's defensive behavior for the
        // otherwise-unreachable combination of a non-empty registry passed
        // alongside `configured=false`: the registry still renders (never
        // silently dropped), but neither footer message fires, since
        // `no_usage && configured` is false and rows is non-empty.
        let reg = registry(&["home"]);
        let report = build_report(&reg, None, /*configured=*/ false, None);
        let table = render_table(&report, now());
        assert!(
            table.contains("home"),
            "registry must still render: {table}"
        );
        assert!(!table.contains("unavailable"), "table:\n{table}");
        assert!(!table.contains("no profiles configured"), "table:\n{table}");
    }

    #[test]
    fn empty_registry_renders_hint() {
        let reg = registry(&[]);
        let report = build_report(&reg, None, false, None);
        let table = render_table(&report, now());
        assert!(table.contains("no profiles configured"), "table:\n{table}");
    }

    #[test]
    fn stale_header_only_when_old() {
        let reg = registry(&["home"]);
        let u = sample_usage();
        // Fresh (5s): no stale header.
        let fresh = render_table(&build_report(&reg, Some(&u), true, Some(5)), now());
        assert!(
            !fresh.contains("usage data is"),
            "fresh should not warn: {fresh}"
        );
        // Old (7m): stale header present.
        let stale = render_table(&build_report(&reg, Some(&u), true, Some(420)), now());
        assert!(
            stale.contains("usage data is 7m old (showing last-known cache)"),
            "stale:\n{stale}"
        );
    }

    #[test]
    fn stale_header_absent_when_stale_secs_is_none() {
        // `stale_secs: None` — e.g. every profile is NeedsLogin/NotFound with
        // no store record ever captured (see
        // `local::oldest_profile_age_secs_none_when_no_profile_ever_captured`)
        // — must render no "⚠ usage data is … old" footer at all, and the
        // JSON `stale_secs` field must be null rather than some derived
        // number.
        let reg = registry(&["home"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let table = render_table(&report, now());
        assert!(
            !table.contains("usage data is"),
            "stale_secs: None must not print a staleness footer: {table}"
        );
        let json = render_json(&report).unwrap();
        assert!(
            json.contains("\"stale_secs\": null"),
            "stale_secs: None must serialize as JSON null: {json}"
        );
    }

    /// A single-profile blob carrying an explicit tier label, as the local
    /// usage collector reports which row was read.
    fn labelled_usage(label: &str) -> UsageData {
        serde_json::from_str(&format!(
            r#"{{
              "captured_at": "2026-07-26T07:00:00Z",
              "profiles": {{
                "home": {{
                  "session": {{"pct": 12}},
                  "week_all": {{"pct": 34}},
                  "week_fable": {{"pct": 80}},
                  "week_model_label": "{label}"
                }}
              }}
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn tier_column_is_headed_with_the_reported_tier() {
        let reg = registry(&["home"]);
        let u = labelled_usage("Fable");
        let report = build_report(&reg, Some(&u), true, None);
        assert_eq!(report.week_model_label.as_deref(), Some("Fable"));
        let table = render_table(&report, now());
        assert!(table.contains("WK(Fable)"), "table:\n{table}");
        // And the label reaches `--json` consumers too.
        let json = render_json(&report).unwrap();
        assert!(json.contains("\"week_model_label\": \"Fable\""), "{json}");
    }

    #[test]
    fn tier_column_falls_back_when_no_label_is_sent() {
        // Pre-label records (and cache files written before the field existed)
        // omit it entirely;
        // the column keeps its baked-in name rather than rendering `WK()`.
        let reg = registry(&["home", "work"]);
        let u = sample_usage();
        let report = build_report(&reg, Some(&u), true, None);
        assert_eq!(report.week_model_label, None);
        assert!(render_table(&report, now()).contains("WK(fable)"));
    }

    #[test]
    fn renamed_tier_retitles_the_column_and_keeps_it_aligned() {
        // The whole point of the field: when Anthropic meters a different tier,
        // the header follows the data instead of lying about it — and the wider
        // header must widen the column, not shove later columns out of line.
        let reg = registry(&["home"]);
        let u = labelled_usage("Sonnet only");
        let report = build_report(&reg, Some(&u), true, None);
        let table = render_table(&report, now());
        assert!(table.contains("WK(Sonnet only)"), "table:\n{table}");
        assert!(!table.contains("WK(fable)"), "stale label:\n{table}");

        let mut lines = table.lines();
        let header = lines.next().unwrap();
        let row = lines.next().unwrap();
        // STATUS is the one unpadded column, so equal start offsets prove the
        // widened tier column moved every intervening column consistently.
        assert_eq!(
            header.chars().count() - "STATUS".len(),
            row.chars().count() - status_cell(&report.rows[0], now()).chars().count(),
            "columns misaligned:\n{table}"
        );
    }

    #[test]
    fn tier_label_pick_is_deterministic_and_bounded() {
        // Sorted-name order, so a `HashMap` reshuffle cannot flip the header.
        let u: UsageData = serde_json::from_str(
            r#"{"profiles": {
                 "zzz": {"week_all": {"pct": 1}, "week_model_label": "Zeta"},
                 "aaa": {"week_all": {"pct": 1}, "week_model_label": "Alpha"}
               }}"#,
        )
        .unwrap();
        let reg = registry(&["aaa", "zzz"]);
        for _ in 0..8 {
            let report = build_report(&reg, Some(&u), true, None);
            assert_eq!(report.week_model_label.as_deref(), Some("Alpha"));
        }

        // A pathological label is truncated, never left to blow the table apart.
        let long = labelled_usage("Supercalifragilistic");
        let table = render_table(
            &build_report(&registry(&["home"]), Some(&long), true, None),
            now(),
        );
        let header = table.lines().next().unwrap();
        assert!(
            header.contains("WK(Supercalifrag\u{2026})"),
            "header: {header}"
        );
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
            v["profiles"]["home"]["session_resets"],
            serde_json::json!("9pm (Asia/Seoul)")
        );
        assert_eq!(
            v["profiles"]["home"]["week_all_resets"],
            serde_json::json!("Jun 22")
        );
        assert_eq!(
            v["profiles"]["work"]["session_resets"],
            serde_json::json!(null)
        );
        assert_eq!(
            v["profiles"]["errored-acct"]["status"],
            serde_json::json!("errored")
        );
        assert_eq!(
            v["profiles"]["errored-acct"]["error"],
            serde_json::json!("HTTP 401: no credentials")
        );
        // `sample_usage()`'s sections carry no `resets_at` (an older-shape
        // fixture) — the JSON keys must be omitted entirely, not `null`.
        assert!(
            !v["profiles"]["home"]
                .as_object()
                .unwrap()
                .contains_key("session_resets_at"),
            "absent resets_at must be omitted, not null: {json}"
        );
    }

    #[test]
    fn json_includes_resets_at_when_present() {
        let reg = registry(&["home"]);
        let u: UsageData = serde_json::from_str(
            r#"{
              "profiles": {
                "home": {
                  "session":  { "pct": 42, "resets_at": 1788339599 },
                  "week_all": { "pct": 31, "resets_at": 1788944399 }
                }
              }
            }"#,
        )
        .unwrap();
        let report = build_report(&reg, Some(&u), true, None);
        let json = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["profiles"]["home"]["session_resets_at"],
            serde_json::json!(1_788_339_599)
        );
        assert_eq!(
            v["profiles"]["home"]["week_all_resets_at"],
            serde_json::json!(1_788_944_399)
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

    // ── attention (design spec "맛이 간 프로필은 로그인하라고 경고") ─────────

    /// A usage blob with one `work` profile that's `LoginRequired` (dead
    /// creds, refresh token also dead) still carrying its last-known stale
    /// numbers, and one `home` profile that's `RefreshNeeded` (access token
    /// expired, refresh alive) also carrying stale numbers. `since_epoch`s
    /// are chosen so `now()` (2026-09-02T00:00:00Z) puts them at exactly 3
    /// days and 2 hours ago — the exact figures in the design spec's own
    /// footer example.
    fn attention_usage() -> UsageData {
        serde_json::from_str(
            r#"{
              "captured_at": "2026-08-30T00:00:00Z",
              "profiles": {
                "work": {
                  "captured_at": "2026-08-30T00:00:00Z",
                  "session": {"pct": 40},
                  "week_all": {"pct": 55},
                  "attention": {
                    "kind": "needs_login",
                    "message": "credentials expired",
                    "action": "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login",
                    "since_epoch": 1788048000
                  }
                },
                "home": {
                  "captured_at": "2026-09-01T22:00:00Z",
                  "session": {"pct": 12},
                  "week_all": {"pct": 34},
                  "attention": {
                    "kind": "needs_refresh",
                    "message": "access token expired",
                    "action": "csm --profile home",
                    "since_epoch": 1788300000
                  }
                }
              },
              "errors": { "work": "credentials expired — login required" }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn join_one_attention_wins_over_errors_membership_and_keeps_numbers() {
        // "work" is in BOTH `attention` (on the ProfileUsage) and `errors`
        // (the exclusion mechanism) — it must still render as exactly ONE
        // row, carrying its stale numbers, not a bare `Errored` row.
        let reg = registry(&["work", "home"]);
        let u = attention_usage();
        let report = build_report(&reg, Some(&u), true, None);
        assert_eq!(report.rows.len(), 2, "one row per profile, never split");

        let work = report.rows.iter().find(|r| r.name == "work").unwrap();
        assert_eq!(work.status, Status::LoginRequired);
        assert_eq!(work.session_pct, Some(40));
        assert_eq!(work.week_all_pct, Some(55));
        assert!(work.attention.is_some());
        assert!(work.error.is_none(), "attention wins — error is not set");

        let home = report.rows.iter().find(|r| r.name == "home").unwrap();
        assert_eq!(home.status, Status::RefreshNeeded);
        assert_eq!(home.session_pct, Some(12));
        assert!(home.attention.is_some());
    }

    #[test]
    fn status_cell_login_required_is_bare_label() {
        let reg = registry(&["work"]);
        let mut u = attention_usage();
        u.profiles.remove("home");
        let report = build_report(&reg, Some(&u), true, None);
        let row = report.rows.iter().find(|r| r.name == "work").unwrap();
        assert_eq!(status_cell(row, now()), "LOGIN REQUIRED");
    }

    #[test]
    fn status_cell_refresh_needed_shows_stale_age() {
        let reg = registry(&["home"]);
        let mut u = attention_usage();
        u.profiles.remove("work");
        let report = build_report(&reg, Some(&u), true, None);
        let row = report.rows.iter().find(|r| r.name == "home").unwrap();
        assert_eq!(status_cell(row, now()), "REFRESH NEEDED (stale 2h)");
    }

    #[test]
    fn attention_footer_block_matches_exact_design_spec_strings() {
        let reg = registry(&["work", "home"]);
        let u = attention_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let lines = attention_lines(&report.rows, now());
        assert_eq!(
            lines,
            vec![
                "\u{26a0} home: access token expired 2h ago \u{2014} run once to refresh"
                    .to_string(),
                "  csm --profile home".to_string(),
                "\u{26a0} work: credentials expired 3d ago \u{2014} login required".to_string(),
                "  CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login".to_string(),
            ],
            "footer lines: {lines:#?}"
        );
    }

    #[test]
    fn render_table_appends_the_attention_footer_block() {
        let reg = registry(&["work", "home"]);
        let u = attention_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let table = render_table(&report, now());
        assert!(
            table.contains("\u{26a0} work: credentials expired 3d ago \u{2014} login required"),
            "table:\n{table}"
        );
        assert!(
            table.contains("  CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"),
            "table:\n{table}"
        );
        assert!(table.contains("LOGIN REQUIRED"), "table:\n{table}");
        assert!(
            table.contains("REFRESH NEEDED (stale 2h)"),
            "table:\n{table}"
        );
    }

    #[test]
    fn attention_lines_empty_when_no_row_carries_attention() {
        let reg = registry(&["home"]);
        let u = sample_usage(); // no attention anywhere
        let report = build_report(&reg, Some(&u), true, None);
        assert!(attention_lines(&report.rows, now()).is_empty());
    }

    #[test]
    fn needs_login_without_age_omits_the_ago_suffix() {
        // NotFound/401-403 carry no local expiry instant (`since_epoch: None`)
        // — the line must read "not logged in — login required", no age.
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "ghost".to_string(),
            serde_json::from_str::<crate::usage::model::ProfileUsage>(
                r#"{
                  "attention": {
                    "kind": "needs_login",
                    "message": "not logged in",
                    "action": "CLAUDE_CONFIG_DIR=/Users/example/.claude.ghost claude auth login"
                  }
                }"#,
            )
            .unwrap(),
        );
        let u = UsageData {
            profiles,
            errors: Some(std::collections::HashMap::from([(
                "ghost".to_string(),
                "not logged in — login required".to_string(),
            )])),
            ..Default::default()
        };
        let reg = registry(&["ghost"]);
        let report = build_report(&reg, Some(&u), true, None);
        let lines = attention_lines(&report.rows, now());
        assert_eq!(
            lines[0],
            "\u{26a0} ghost: not logged in \u{2014} login required"
        );
    }

    #[test]
    fn json_carries_attention_object_verbatim() {
        let reg = registry(&["work", "home"]);
        let u = attention_usage();
        let report = build_report(&reg, Some(&u), true, None);
        let json = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            v["profiles"]["work"]["attention"]["kind"],
            serde_json::json!("needs_login")
        );
        assert_eq!(
            v["profiles"]["work"]["attention"]["action"],
            serde_json::json!("CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login")
        );
        assert_eq!(
            v["profiles"]["home"]["attention"]["kind"],
            serde_json::json!("needs_refresh")
        );
        // A healthy profile's JSON omits `attention` entirely (not `null`) —
        // `sample_usage()`'s "home" carries no attention at all.
        let healthy_data = sample_usage();
        let healthy_report = build_report(&registry(&["home"]), Some(&healthy_data), true, None);
        let healthy_json = render_json(&healthy_report).unwrap();
        let hv: serde_json::Value = serde_json::from_str(&healthy_json).unwrap();
        assert!(
            !hv["profiles"]["home"]
                .as_object()
                .unwrap()
                .contains_key("attention"),
            "healthy row must omit attention, not null: {healthy_json}"
        );
    }
}
