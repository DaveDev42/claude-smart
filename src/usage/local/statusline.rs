//! Parse the statusLine command's stdin `rate_limits` payload into a
//! [`ProfileUsage`] update.
//!
//! Claude Code's statusLine JSON only ever carries `five_hour`/`seven_day`
//! (subscriber-only, and only once the first API response of the session has
//! come back) — never the per-model-tier scoped limit. So a payload here maps
//! to `session`/`week_all`; `week_fable`/`week_model_label` are carried
//! forward unchanged from whatever the store already knew (typically from the
//! last `local::api` fetch), which is why [`to_profile_usage`] takes the prior
//! record as an explicit parameter rather than reading the store itself — it
//! stays a pure mapping function, testable without touching disk.
//!
//! Each of `session`/`week_all` is ALSO carried forward from `prior` when its
//! own window is absent from this payload — the spec notes the two windows
//! are present only "윈도우가 살아있는 동안만" (only while the window is
//! live), so a `five_hour`-only tick is expected and must not be read as
//! "week_all is now unknown". Without the fallback, a payload that happens to
//! omit `seven_day` would blank out `week_all` and drop the profile from
//! scoring's candidate list entirely (it requires a `week_all` section) —
//! purely additive statusline capture must never regress the single most
//! important auto-pick signal.

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[cfg(test)]
use crate::usage::model::{Attention, AttentionKind};
use crate::usage::model::{ProfileUsage, UsageSection};

use super::display;

/// The subset of statusLine stdin this crate reads. Every field is `Option` +
/// `#[serde(default)]` and unknown sibling keys (`model`, `workspace`,
/// `cost`, …) are ignored — this is one small slice of a much larger stdin
/// payload documented in the Claude Code binary, not something this crate
/// owns the schema of.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatuslinePayload {
    #[serde(default)]
    pub rate_limits: Option<RateLimits>,
    /// Kept loose (`Value`) so an unexpected shape never fails the parse.
    #[serde(default)]
    pub session_id: Option<serde_json::Value>,
    #[serde(default)]
    pub cost: Option<serde_json::Value>,
    /// Claude Code's own prompt-cache state, read by `idle_compact`. Kept
    /// loose (`Value`), exactly like `cost` above, so a shape this crate
    /// does not expect (a future engine change, a non-object value) never
    /// fails the parse of the rest of the payload; the typed sub-fields
    /// (`warm`/`expires_at`/`recache_tokens_if_cold`) are read back through
    /// the `prompt_cache_*` accessors below, each individually optional.
    #[serde(default)]
    pub prompt_cache: Option<serde_json::Value>,
    /// The context window size `idle_compact`'s log line reports alongside
    /// the re-write estimate. Kept as a loose `Value` (undocumented
    /// shape/unit) — never interpreted here beyond the bare-number case
    /// (see `idle_compact::context_window_summary`), only carried through.
    #[serde(default)]
    pub context_window: Option<serde_json::Value>,
    /// The `.jsonl` transcript path for this session, read by
    /// `idle_compact`'s busy check (its mtime vs. the `<sid>.idle` marker).
    /// Kept loose for the same reason as `session_id` above.
    #[serde(default)]
    pub transcript_path: Option<serde_json::Value>,
    /// Claude Code's vim-mode indicator, read by `idle_compact::request` so
    /// a hand-off request can tell the supervisor whether it will need to
    /// leave NORMAL mode before typing. Kept loose for the same reason as
    /// `prompt_cache` above; the typed value is read back through
    /// [`Self::vim_mode`].
    #[serde(default)]
    pub vim: Option<serde_json::Value>,
}

impl StatuslinePayload {
    /// The payload's `session_id`, when it is a non-empty string.
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// `cost.total_duration_ms` in whole seconds, when it is a sane number.
    pub fn duration_secs(&self) -> Option<i64> {
        let ms = self.cost.as_ref()?.get("total_duration_ms")?.as_f64()?;
        (ms.is_finite() && ms >= 0.0).then(|| (ms / 1000.0) as i64)
    }

    /// `prompt_cache.warm`, when present and a boolean.
    pub fn prompt_cache_warm(&self) -> Option<bool> {
        self.prompt_cache.as_ref()?.get("warm")?.as_bool()
    }

    /// `prompt_cache.expires_at` (unix epoch seconds), when present and a
    /// sane integer.
    pub fn prompt_cache_expires_at(&self) -> Option<i64> {
        self.prompt_cache.as_ref()?.get("expires_at")?.as_i64()
    }

    /// `prompt_cache.recache_tokens_if_cold`, when present and a sane
    /// integer.
    pub fn prompt_cache_recache_tokens_if_cold(&self) -> Option<i64> {
        self.prompt_cache
            .as_ref()?
            .get("recache_tokens_if_cold")?
            .as_i64()
    }

    /// The payload's `transcript_path`, when it is a non-empty string.
    pub fn transcript_path(&self) -> Option<String> {
        self.transcript_path
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// `vim.mode`, when present and a non-empty string (for example
    /// `"insert"`/`"normal"`). `None` when the payload carries no `vim`
    /// section at all (vim mode is off, or claude has not reported one yet).
    pub fn vim_mode(&self) -> Option<String> {
        self.vim
            .as_ref()?
            .get("mode")?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimits {
    #[serde(default)]
    pub five_hour: Option<Window>,
    #[serde(default)]
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub used_percentage: Option<f64>,
    /// Already a unix epoch (unlike the OAuth API's RFC-3339 strings) —
    /// statusLine's own documented shape.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// Map a statusline payload to a [`ProfileUsage`], merging in `prior`'s
/// `week_fable`/`week_model_label`.
///
/// Returns `None` when there is nothing usable: `rate_limits` absent
/// entirely, or present but both windows absent (a session before the first
/// API response, or a non-subscriber payload) — the caller (`record_statusline_payload`)
/// treats `None` as "no-op, do not touch the store".
pub fn to_profile_usage(
    payload: &StatuslinePayload,
    prior: Option<&ProfileUsage>,
    now: DateTime<Utc>,
) -> Option<ProfileUsage> {
    let rl = payload.rate_limits.as_ref()?;
    if rl.five_hour.is_none() && rl.seven_day.is_none() {
        return None;
    }

    Some(ProfileUsage {
        captured_at: Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        session: rl
            .five_hour
            .as_ref()
            .and_then(window_to_section)
            .or_else(|| prior.and_then(|p| p.session.clone())),
        week_all: rl
            .seven_day
            .as_ref()
            .and_then(window_to_section)
            .or_else(|| prior.and_then(|p| p.week_all.clone())),
        week_fable: prior.and_then(|p| p.week_fable.clone()),
        week_model_label: prior.and_then(|p| p.week_model_label.clone()),
        session_stats: Vec::new(),
        source: Some("statusline".to_string()),
        // A statusline capture never touches `creds`/the API — it can't
        // learn anything new about credential health, so it must carry the
        // prior probe's `attention` forward rather than silently clearing
        // it. Without this, a ~1/s statusline capture would erase a
        // NeedsRefresh/NeedsLogin warning within a second of a real probe
        // setting it, and it would stay erased until the next api probe
        // (up to `CSM_USAGE_PROFILE_TTL` later).
        attention: prior.and_then(|p| p.attention.clone()),
    })
}

fn window_to_section(w: &Window) -> Option<UsageSection> {
    let pct = w.used_percentage?.round() as i64;
    Some(UsageSection {
        pct,
        resets: w.resets_at.map(display::format_resets),
        resets_at: w.resets_at,
    })
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    const SAMPLE_JSON: &str = r#"{
      "rate_limits": {
        "five_hour": { "used_percentage": 42.0, "resets_at": 1788339599 },
        "seven_day": { "used_percentage": 31.0, "resets_at": 1788339599 }
      }
    }"#;

    #[test]
    fn parses_both_windows() {
        let payload: StatuslinePayload = serde_json::from_str(SAMPLE_JSON).unwrap();
        let pu = to_profile_usage(&payload, None, now()).expect("both windows present");
        assert_eq!(pu.session.unwrap().pct, 42);
        assert_eq!(pu.week_all.unwrap().pct, 31);
        assert_eq!(pu.source.as_deref(), Some("statusline"));
    }

    #[test]
    fn rate_limits_absent_is_none() {
        let payload: StatuslinePayload = serde_json::from_str(r#"{}"#).unwrap();
        assert!(to_profile_usage(&payload, None, now()).is_none());
    }

    #[test]
    fn rate_limits_present_but_both_windows_absent_is_none() {
        let payload: StatuslinePayload = serde_json::from_str(r#"{"rate_limits": {}}"#).unwrap();
        assert!(to_profile_usage(&payload, None, now()).is_none());
    }

    #[test]
    fn one_window_present_is_still_some() {
        let payload: StatuslinePayload = serde_json::from_str(
            r#"{"rate_limits": {"five_hour": {"used_percentage": 10.0, "resets_at": 1}}}"#,
        )
        .unwrap();
        let pu = to_profile_usage(&payload, None, now()).expect("one window is enough");
        assert_eq!(pu.session.unwrap().pct, 10);
        assert!(pu.week_all.is_none());
    }

    #[test]
    fn merge_preserves_prior_week_fable_and_label() {
        let prior = ProfileUsage {
            captured_at: Some("2026-09-01T00:00:00Z".to_string()),
            session: None,
            week_all: None,
            week_fable: Some(UsageSection {
                pct: 15,
                resets: None,
                resets_at: Some(1_788_339_599),
            }),
            week_model_label: Some("Fable".to_string()),
            session_stats: vec![],
            source: Some("api".to_string()),
            attention: None,
        };
        let payload: StatuslinePayload = serde_json::from_str(SAMPLE_JSON).unwrap();
        let pu = to_profile_usage(&payload, Some(&prior), now()).unwrap();

        assert_eq!(pu.week_fable.unwrap().pct, 15);
        assert_eq!(pu.week_model_label.as_deref(), Some("Fable"));
        // session/week_all come from THIS payload, not the prior record.
        assert_eq!(pu.session.unwrap().pct, 42);
    }

    #[test]
    fn merge_preserves_prior_attention() {
        // A statusline capture never touches creds/the API — it must never
        // silently erase a warning a real probe attached to the prior record.
        let prior = ProfileUsage {
            attention: Some(Attention {
                kind: AttentionKind::NeedsRefresh,
                message: "access token expired".to_string(),
                action: "csm accounts use home".to_string(),
                since_epoch: Some(1),
            }),
            ..Default::default()
        };
        let payload: StatuslinePayload = serde_json::from_str(SAMPLE_JSON).unwrap();
        let pu = to_profile_usage(&payload, Some(&prior), now()).unwrap();
        assert_eq!(
            pu.attention.map(|a| a.kind),
            Some(AttentionKind::NeedsRefresh)
        );
    }

    #[test]
    fn no_prior_leaves_attention_none() {
        let payload: StatuslinePayload = serde_json::from_str(SAMPLE_JSON).unwrap();
        let pu = to_profile_usage(&payload, None, now()).unwrap();
        assert!(pu.attention.is_none());
    }

    #[test]
    fn merge_preserves_prior_week_all_when_payload_omits_seven_day() {
        // Regression: a payload carrying only `five_hour` (the window rolled
        // out of `seven_day`'s presence, or a session before the weekly probe
        // landed) used to blank `week_all` entirely, dropping the profile
        // from scoring's candidate list (it requires `week_all`).
        let prior = ProfileUsage {
            captured_at: Some("2026-09-01T00:00:00Z".to_string()),
            session: Some(UsageSection {
                pct: 20,
                resets: None,
                resets_at: None,
            }),
            week_all: Some(UsageSection {
                pct: 31,
                resets: None,
                resets_at: Some(1_788_339_599),
            }),
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: Some("api".to_string()),
            attention: None,
        };
        let payload: StatuslinePayload = serde_json::from_str(
            r#"{"rate_limits": {"five_hour": {"used_percentage": 42.0, "resets_at": 1788339599}}}"#,
        )
        .unwrap();
        let pu = to_profile_usage(&payload, Some(&prior), now()).unwrap();

        // session comes from THIS payload's five_hour window ...
        assert_eq!(pu.session.unwrap().pct, 42);
        // ... but week_all, absent from this payload, is carried forward
        // from the prior record rather than dropped.
        assert_eq!(pu.week_all.unwrap().pct, 31);
    }

    #[test]
    fn merge_preserves_prior_session_when_payload_omits_five_hour() {
        let prior = ProfileUsage {
            captured_at: Some("2026-09-01T00:00:00Z".to_string()),
            session: Some(UsageSection {
                pct: 20,
                resets: None,
                resets_at: None,
            }),
            week_all: None,
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: Some("api".to_string()),
            attention: None,
        };
        let payload: StatuslinePayload = serde_json::from_str(
            r#"{"rate_limits": {"seven_day": {"used_percentage": 31.0, "resets_at": 1788339599}}}"#,
        )
        .unwrap();
        let pu = to_profile_usage(&payload, Some(&prior), now()).unwrap();

        assert_eq!(pu.week_all.unwrap().pct, 31);
        // session, absent from this payload, is carried forward.
        assert_eq!(pu.session.unwrap().pct, 20);
    }

    #[test]
    fn no_prior_leaves_week_fable_none() {
        let payload: StatuslinePayload = serde_json::from_str(SAMPLE_JSON).unwrap();
        let pu = to_profile_usage(&payload, None, now()).unwrap();
        assert!(pu.week_fable.is_none());
        assert!(pu.week_model_label.is_none());
    }

    #[test]
    fn window_to_section_rounds_percentage() {
        let w = Window {
            used_percentage: Some(10.5),
            resets_at: Some(1),
        };
        assert_eq!(window_to_section(&w).unwrap().pct, 11); // round-half-away-from-zero
    }

    #[test]
    fn unknown_sibling_keys_are_tolerated() {
        let json = r#"{
          "rate_limits": { "five_hour": {"used_percentage": 5.0, "resets_at": 1}, "spend_limit": {"anything": true} },
          "model": {"id": "whatever"},
          "workspace": {"current_dir": "/x"}
        }"#;
        let payload: StatuslinePayload = serde_json::from_str(json).expect("must tolerate extras");
        assert!(to_profile_usage(&payload, None, now()).is_some());
    }

    // ── prompt_cache / context_window (idle_compact's inputs) ────────────────

    #[test]
    fn prompt_cache_parses_full_shape() {
        let json = r#"{
          "prompt_cache": {"warm": true, "expires_at": 1788339599, "recache_tokens_if_cold": 213000},
          "context_window": 200000
        }"#;
        let payload: StatuslinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.prompt_cache_warm(), Some(true));
        assert_eq!(payload.prompt_cache_expires_at(), Some(1_788_339_599));
        assert_eq!(payload.prompt_cache_recache_tokens_if_cold(), Some(213_000));
        assert_eq!(payload.context_window, Some(serde_json::json!(200000)));
    }

    #[test]
    fn prompt_cache_absent_is_none() {
        let payload: StatuslinePayload = serde_json::from_str(r#"{}"#).unwrap();
        assert!(payload.prompt_cache_warm().is_none());
        assert!(payload.prompt_cache_expires_at().is_none());
        assert!(payload.prompt_cache_recache_tokens_if_cold().is_none());
        assert!(payload.context_window.is_none());
    }

    #[test]
    fn prompt_cache_partial_fields_are_individually_optional() {
        let payload: StatuslinePayload =
            serde_json::from_str(r#"{"prompt_cache": {"warm": true}}"#).unwrap();
        assert_eq!(payload.prompt_cache_warm(), Some(true));
        assert!(payload.prompt_cache_expires_at().is_none());
        assert!(payload.prompt_cache_recache_tokens_if_cold().is_none());
    }

    #[test]
    fn prompt_cache_unexpected_shape_never_fails_the_whole_parse() {
        // A future engine change reshapes prompt_cache (e.g. a string instead
        // of an object) — the rest of the payload must still parse, with
        // every prompt_cache accessor coming back None rather than an error.
        let json = r#"{
          "prompt_cache": "unexpected",
          "rate_limits": {"five_hour": {"used_percentage": 5.0, "resets_at": 1}}
        }"#;
        let payload: StatuslinePayload =
            serde_json::from_str(json).expect("must tolerate a reshaped prompt_cache");
        assert!(payload.prompt_cache_warm().is_none());
        assert!(payload.prompt_cache_expires_at().is_none());
        assert!(payload.prompt_cache_recache_tokens_if_cold().is_none());
        assert!(to_profile_usage(&payload, None, now()).is_some());
    }

    #[test]
    fn context_window_tolerates_any_shape() {
        let payload: StatuslinePayload =
            serde_json::from_str(r#"{"context_window": {"used": 1, "max": 2}}"#).unwrap();
        assert!(payload.context_window.is_some());
    }

    // ── transcript_path (idle_compact's busy check) ──────────────────────────

    #[test]
    fn transcript_path_parses_a_string() {
        let json = r#"{"transcript_path": "/home/example/.claude/projects/foo/sid.jsonl"}"#;
        let payload: StatuslinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(
            payload.transcript_path().as_deref(),
            Some("/home/example/.claude/projects/foo/sid.jsonl")
        );
    }

    #[test]
    fn transcript_path_absent_is_none() {
        let payload: StatuslinePayload = serde_json::from_str(r#"{}"#).unwrap();
        assert!(payload.transcript_path().is_none());
    }

    #[test]
    fn transcript_path_blank_or_wrong_shape_is_none() {
        let blank: StatuslinePayload =
            serde_json::from_str(r#"{"transcript_path": "  "}"#).unwrap();
        assert!(blank.transcript_path().is_none());
        let wrong_shape: StatuslinePayload = serde_json::from_str(
            r#"{"transcript_path": 42, "rate_limits": {"five_hour": {"used_percentage": 5.0, "resets_at": 1}}}"#,
        )
        .unwrap();
        assert!(wrong_shape.transcript_path().is_none());
        assert!(
            to_profile_usage(&wrong_shape, None, now()).is_some(),
            "a wrong-shaped transcript_path must not fail the rest of the parse"
        );
    }

    // ── vim (idle_compact's request hand-off) ─────────────────────────────────

    #[test]
    fn vim_mode_parses_a_string() {
        let payload: StatuslinePayload =
            serde_json::from_str(r#"{"vim": {"mode": "insert"}}"#).unwrap();
        assert_eq!(payload.vim_mode().as_deref(), Some("insert"));
    }

    #[test]
    fn vim_mode_absent_is_none() {
        let payload: StatuslinePayload = serde_json::from_str(r#"{}"#).unwrap();
        assert!(payload.vim_mode().is_none());
    }

    #[test]
    fn vim_mode_blank_or_wrong_shape_is_none() {
        let blank: StatuslinePayload = serde_json::from_str(r#"{"vim": {"mode": "  "}}"#).unwrap();
        assert!(blank.vim_mode().is_none());
        let wrong_shape: StatuslinePayload = serde_json::from_str(
            r#"{"vim": "unexpected", "rate_limits": {"five_hour": {"used_percentage": 5.0, "resets_at": 1}}}"#,
        )
        .unwrap();
        assert!(wrong_shape.vim_mode().is_none());
        assert!(
            to_profile_usage(&wrong_shape, None, now()).is_some(),
            "a wrong-shaped vim must not fail the rest of the parse"
        );
    }
}
