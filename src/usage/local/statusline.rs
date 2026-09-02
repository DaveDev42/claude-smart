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
                action: "csm --profile home".to_string(),
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
}
