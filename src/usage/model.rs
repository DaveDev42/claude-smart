//! Serde model shared by the local usage store (`<smart_dir>/usage/<profile>.json`,
//! wrapped in `local::store::StoreRecord`) and `.usage-cache.json` (the
//! positive TTL cache `transport.rs` writes on top of whatever produced a
//! `UsageData` — the local collector today; historically a hub scrape).
//!
//! Shape:
//!
//! ```json
//! {
//!   "captured_at": "2026-06-17T07:13:19Z",
//!   "profiles": {
//!     "<profile_name>": {
//!       "captured_at": "<ISO-8601>",
//!       "session":     { "pct": <int>, "resets": <string|null>, "resets_at": <epoch|null> } | null,
//!       "week_all":    { "pct": <int>, "resets": <string|null>, "resets_at": <epoch|null> } | null,
//!       "week_fable":  { "pct": <int>, "resets": <string|null>, "resets_at": <epoch|null> } | null,
//!       "week_model_label": "<string>" | null,
//!       "session_stats": ["<string>", ...],
//!       "source": "api" | "statusline" | "cmd" | null
//!     }
//!   },
//!   "errors": { "<profile_name>": "<error string>" }
//! }
//! ```
//!
//! Design choices:
//! - Each section (`session`/`week_all`/`week_fable`) is `Option<UsageSection>`.
//!   An absent/failed section is `None` (serde null).
//! - `resets` inside a present section is the human-readable display string
//!   (`Option<String>`, may be null); `resets_at` is the machine-native unix
//!   epoch the local collector actually computed the display string from.
//!   [`UsageSection::reset_instant`] is the one place callers should read a
//!   section's reset time from — it prefers `resets_at` and only falls back to
//!   re-parsing `resets` for a record written before this field existed.
//! - `source` records which local collection path (`local::api`,
//!   `local::statusline`, or `CSM_USAGE_CMD`) produced a `ProfileUsage`, so a
//!   caller merging a live API reading with a fresher statusline sample (or
//!   vice versa) can tell them apart. `None` for any older record.
//! - `errors` key is absent when all profiles succeeded → `Option<HashMap<…>>`.
//! - `#[serde(default)]` throughout for forward-compatible tolerance — in
//!   particular, a `UsageSection` written before `resets_at` existed parses
//!   cleanly with `resets_at: None`, and `reset_instant` falls back correctly.
//!
//! `week_fable` is the *separately-capped model tier*, whose name tracks
//! whichever tier Anthropic meters on its own — the `claude /usage` row was
//! "Current week (Sonnet only)" until 2026-07 and is "Current week (Fable)"
//! now. The key must stay in lockstep with the producer
//! (`local::api::to_profile_usage`'s `weekly_scoped` mapping): `serde(default)`
//! means a producer-side rename does not error here, it silently reads `None`
//! forever.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── top-level ────────────────────────────────────────────────────────────────

/// Deserialized form of `.usage-cache.json` (positive TTL cache) and of
/// whatever `local::collect`/an operator's `CSM_USAGE_CMD` produces (same
/// shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageData {
    /// ISO-8601 timestamp of the newest per-profile `captured_at` actually
    /// placed into `profiles` this collection round (see `local::collect`'s
    /// `track_newest`) — the true freshness signal `scoring::newest_captured_at`
    /// reads. May be absent on older cache files or a `CSM_USAGE_CMD` payload
    /// that omits it.
    #[serde(default)]
    pub captured_at: Option<String>,

    /// Per-profile usage.  Key = profile name (e.g. `"home"`, `"work"`).
    #[serde(default)]
    pub profiles: HashMap<String, ProfileUsage>,

    /// Profiles that produced no usable data this round.  Key = profile name,
    /// value = error string.  The `errors` key is **absent** when all
    /// profiles succeeded.
    #[serde(default)]
    pub errors: Option<HashMap<String, String>>,

    /// `true` when at least one profile actually reached a live probe
    /// (`Event::ApiOk`/`Resolution::Persist`) during this `local::collect`
    /// call. Never serialized — a same-call signal for `transport.rs`'s
    /// negative-cooldown decision, which must not key off `profiles` being
    /// non-empty (a `ServeStale`-only round populates `profiles` too, even
    /// though every live attempt failed). Always `false` for data that did
    /// not come from `local::collect` (a cache read, a `CSM_USAGE_CMD`
    /// payload) — those callers have no live-probe concept.
    #[serde(skip)]
    pub any_probe_attempted: bool,

    /// `true` when at least one profile's live probe this round succeeded
    /// (paired with [`Self::any_probe_attempted`] — see its doc). Never
    /// serialized.
    #[serde(skip)]
    pub any_probe_succeeded: bool,
}

// ─── per-profile ──────────────────────────────────────────────────────────────

/// Usage data for a single profile.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileUsage {
    /// ISO-8601 capture timestamp for this profile's slice.
    #[serde(default)]
    pub captured_at: Option<String>,

    /// Session-level quota (resets more frequently than weekly).
    /// `None` when the local collector's mapping (`local::api::to_profile_usage`)
    /// found no percent for this section.
    #[serde(default)]
    pub session: Option<UsageSection>,

    /// Weekly aggregate quota across all model tiers.
    /// `None` when absent or null.
    #[serde(default)]
    pub week_all: Option<UsageSection>,

    /// Weekly per-model-tier quota (currently Fable).
    /// `None` when absent or null.
    #[serde(default)]
    pub week_fable: Option<UsageSection>,

    /// Which tier `week_fable` was actually read from, verbatim from the
    /// OAuth usage API's `limits[kind=weekly_scoped].scope.model.display_name`
    /// (e.g. `"Fable"`). `local::api` derives this instead of assuming a
    /// name, so a tier rename relabels the column rather than leaving it
    /// advertising a tier that is no longer metered.
    #[serde(default)]
    pub week_model_label: Option<String>,

    /// Raw stat strings from the local collector (e.g. token counts).
    /// Optional; not used for scoring but preserved for debugging.
    #[serde(default)]
    pub session_stats: Vec<String>,

    /// Which local collection path produced this reading: `"api"`
    /// (`local::api::to_profile_usage`), `"statusline"`
    /// (`local::statusline::to_profile_usage`), or `"cmd"` (an operator's
    /// `CSM_USAGE_CMD`). `None` for a record that predates this field, or one
    /// this crate did not itself produce (a foreign `CSM_USAGE_CMD` payload
    /// that omits it). Not used for scoring — purely diagnostic/merge context
    /// (e.g. the statusline recorder's throttle compares this to decide
    /// whether two consecutive captures are "the same source").
    #[serde(default)]
    pub source: Option<String>,

    /// Set when this profile's credentials need user action — a dead/expired
    /// token (design spec "맛이 간 프로필은 로그인하라고 경고"). `None` for a
    /// healthy profile, or a record written before this field existed.
    /// Deliberately lives on the DATA (not just printed to stderr once) so it
    /// survives the positive cache: `local::collect` doesn't run again on a
    /// cache hit, so stderr-only warnings would silently disappear for
    /// however long the cache stays fresh. `local::mod::resolve` is the only
    /// producer; every render surface (`report::render_table`'s STATUS
    /// column + footer, `report::attention_lines`, the account picker) reads
    /// it from here rather than recomputing anything about credential state.
    #[serde(default)]
    pub attention: Option<Attention>,
}

// ─── attention (expired/dead credentials) ──────────────────────────────────

/// A user-actionable warning attached to one profile's [`ProfileUsage`].
///
/// `message` is a stable, age-free description (e.g. `"credentials
/// expired"`) — never a baked-in relative time like `"3d ago"`, because this
/// struct is what gets cached to `.usage-cache.json` and a relative-time
/// string written at collection time would silently go stale the longer the
/// cache (or a `ServeStale` round) keeps serving it. Renderers combine
/// `message` with a freshly-computed age from `since_epoch` at DISPLAY time
/// instead (see `report::attention_lines`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub kind: AttentionKind,
    /// Stable, age-free description — see the struct doc.
    pub message: String,
    /// A complete, copy-pasteable shell command that resolves this warning:
    /// `csm --profile <name>` for [`AttentionKind::NeedsRefresh`],
    /// `CLAUDE_CONFIG_DIR=<dir> claude auth login` for
    /// [`AttentionKind::NeedsLogin`].
    pub action: String,
    /// Unix epoch (seconds) the credential expired at, when known — absent
    /// for `NotFound` (never logged in — nothing "expired") and for an API
    /// 401/403 (the server rejected the token; the local clock never saw it
    /// as expired, so there is no local expiry instant to report).
    #[serde(default)]
    pub since_epoch: Option<i64>,
}

/// Which kind of credential trouble a profile's [`Attention`] describes.
///
/// `NeedsRefresh` — the access token expired but the refresh token is still
/// alive, so Claude Code itself will silently mint a new access token the
/// next time it runs under this profile. `NeedsLogin` — the refresh token is
/// dead or absent (or the API outright rejected the token), so nothing but
/// an interactive `claude auth login` recovers the profile.
///
/// `NeedsLogin` profiles are additionally recorded in `UsageData::errors` —
/// see `local::mod`'s module doc for why that (not a new field checked by
/// `scoring::pick_best_at`) is the exclusion mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    NeedsRefresh,
    NeedsLogin,
}

// ─── per-section ──────────────────────────────────────────────────────────────

/// A single usage quota section (session, week_all, or week_fable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSection {
    /// Percentage consumed (0–100; may exceed 100 on burst).
    pub pct: i64,

    /// Human-readable reset time string, e.g. `"9pm (Asia/Seoul)"` or
    /// `"Jun 18 at 9pm (Asia/Seoul)"`.  `None` when the producer omitted it.
    #[serde(default)]
    pub resets: Option<String>,

    /// Machine-native reset instant, unix epoch seconds. Populated by the
    /// local collector (`local::api`/`local::statusline`) from the API's
    /// RFC-3339 `resets_at` / the statusline's already-epoch `resets_at`.
    /// `None` for a record written before this field existed — callers use
    /// [`Self::reset_instant`] rather than reading this directly so that case
    /// falls back to re-parsing `resets`.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

impl UsageSection {
    /// Resolve this section's reset time to a concrete instant, relative to
    /// `now` (used only by the `resets`-string fallback, which — like
    /// [`crate::account::reset::resets_to_epoch_at`] — needs a reference
    /// instant to fill in an implied date and to decide next-year rollover).
    ///
    /// Prefers `resets_at` (the machine-native epoch the local collector
    /// computed the display string from) and only falls back to re-parsing
    /// the human-readable `resets` string when `resets_at` is absent — the
    /// backward-compat path for a store/cache record written before this
    /// field existed.
    ///
    /// Called from `scoring::pick_best_at`'s reset-time ranking (see
    /// `src/account/scoring.rs`).
    pub fn reset_instant(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if let Some(from_epoch) = self
            .resets_at
            .and_then(|epoch| DateTime::<Utc>::from_timestamp(epoch, 0))
        {
            return Some(from_epoch);
        }
        self.resets
            .as_deref()
            .and_then(|s| crate::account::reset::resets_to_epoch_at(s, now).ok())
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

impl UsageData {
    /// Return `(session_pct, week_all_pct)` for `profile`, or `None` if the
    /// profile is in `errors`, absent, or has no section data.
    ///
    /// Absent `session.pct` is encoded as `-1` in the scoring logic (spec §2).
    pub fn current_usage(&self, profile: &str) -> Option<(i64, i64)> {
        // If this profile is in the errors map, it has no usable data.
        if let Some(errors) = &self.errors {
            if errors.contains_key(profile) {
                return None;
            }
        }
        let pu = self.profiles.get(profile)?;
        let sess_pct = pu.session.as_ref().map(|s| s.pct).unwrap_or(-1);
        let week_pct = pu.week_all.as_ref().map(|s| s.pct).unwrap_or(-1);
        Some((sess_pct, week_pct))
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A representative `.usage-cache.json` payload with:
    /// - `home`: all three sections present with real values
    /// - `work`: `week_fable` absent (null), `session.resets` null
    /// - `errors`: one errored profile
    /// - top-level `captured_at` present
    const SAMPLE_CACHE_JSON: &str = r#"
    {
      "captured_at": "2026-06-17T07:13:19Z",
      "profiles": {
        "home": {
          "captured_at": "2026-06-17T07:13:17Z",
          "session": {
            "pct": 42,
            "resets": "9pm (Asia/Seoul)"
          },
          "week_all": {
            "pct": 31,
            "resets": "Jun 18 at 9pm (Asia/Seoul)"
          },
          "week_fable": {
            "pct": 15,
            "resets": "Jun 18 at 9pm (Asia/Seoul)"
          },
          "session_stats": ["12000 tokens used", "88000 remaining"]
        },
        "work": {
          "captured_at": "2026-06-17T07:13:18Z",
          "session": {
            "pct": 5,
            "resets": null
          },
          "week_all": {
            "pct": 67,
            "resets": "Jun 20 at 8:20pm (Asia/Seoul)"
          },
          "week_fable": null,
          "session_stats": []
        }
      },
      "errors": {
        "broken_profile": "HTTP 401: no credentials"
      }
    }
    "#;

    #[test]
    fn deserialize_sample_cache_json() {
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("should parse sample cache JSON");

        // top-level
        assert_eq!(
            data.captured_at.as_deref(),
            Some("2026-06-17T07:13:19Z"),
            "top-level captured_at"
        );

        // home profile
        let home = data.profiles.get("home").expect("home profile");
        let sess = home.session.as_ref().expect("home.session");
        assert_eq!(sess.pct, 42);
        assert_eq!(sess.resets.as_deref(), Some("9pm (Asia/Seoul)"));

        let week_all = home.week_all.as_ref().expect("home.week_all");
        assert_eq!(week_all.pct, 31);
        assert_eq!(
            week_all.resets.as_deref(),
            Some("Jun 18 at 9pm (Asia/Seoul)")
        );

        let week_fable = home.week_fable.as_ref().expect("home.week_fable");
        assert_eq!(week_fable.pct, 15);

        assert_eq!(home.session_stats.len(), 2);

        // work profile — week_fable is null → None
        let work = data.profiles.get("work").expect("work profile");
        assert!(
            work.week_fable.is_none(),
            "work.week_fable should be None (null in JSON)"
        );
        // session.resets is null → None
        let esess = work.session.as_ref().expect("work.session");
        assert_eq!(esess.pct, 5);
        assert!(esess.resets.is_none(), "work.session.resets should be None");
        assert!(work.session_stats.is_empty());

        // errors map
        let errors = data.errors.as_ref().expect("errors map");
        assert!(errors.contains_key("broken_profile"));
        assert!(
            errors["broken_profile"].contains("401"),
            "error message should mention 401"
        );
    }

    #[test]
    fn deserialize_minimal_json_no_errors_key() {
        // The `errors` key is absent when all profiles succeeded.
        let json = r#"{"profiles": {"home": {"session": {"pct": 10}, "week_all": {"pct": 20}}}}"#;
        let data: UsageData = serde_json::from_str(json).expect("minimal JSON");
        assert!(
            data.errors.is_none(),
            "errors should be None when key absent"
        );
        let p = data.profiles.get("home").expect("home");
        assert!(p.week_fable.is_none());
    }

    #[test]
    fn current_usage_returns_correct_pcts() {
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("parse for current_usage test");

        let (sess, week) = data.current_usage("home").expect("home present");
        assert_eq!(sess, 42);
        assert_eq!(week, 31);

        let (sess, week) = data.current_usage("work").expect("work present");
        assert_eq!(sess, 5);
        assert_eq!(week, 67);
    }

    #[test]
    fn current_usage_none_for_errored_profile() {
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("parse for error test");
        assert!(
            data.current_usage("broken_profile").is_none(),
            "errored profile must return None"
        );
    }

    #[test]
    fn current_usage_none_for_absent_profile() {
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("parse for absent test");
        assert!(
            data.current_usage("no_such_profile").is_none(),
            "absent profile must return None"
        );
    }

    #[test]
    fn absent_session_encodes_as_minus_one() {
        // A profile with session=null → current_usage returns (-1, week_pct).
        let json = r#"{"profiles": {"p": {"session": null, "week_all": {"pct": 55}}}}"#;
        let data: UsageData = serde_json::from_str(json).expect("parse");
        let (sess, week) = data.current_usage("p").expect("p present");
        assert_eq!(sess, -1, "absent session.pct must encode as -1");
        assert_eq!(week, 55);
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let data: UsageData = serde_json::from_str(SAMPLE_CACHE_JSON).expect("initial parse");
        let serialized = serde_json::to_string(&data).expect("serialize");
        let data2: UsageData = serde_json::from_str(&serialized).expect("re-parse");

        // Spot-check a field to verify the roundtrip.
        assert_eq!(
            data.profiles["home"].session.as_ref().unwrap().pct,
            data2.profiles["home"].session.as_ref().unwrap().pct
        );
    }

    // ── resets_at / source (added for local collection) ───────────────────────

    #[test]
    fn old_json_without_resets_at_or_source_parses_backward_compat() {
        // SAMPLE_CACHE_JSON predates both fields entirely — must still parse,
        // with resets_at/source defaulting to None everywhere.
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("old-shape JSON must still parse");
        let home = data.profiles.get("home").expect("home profile");
        assert!(
            home.session.as_ref().unwrap().resets_at.is_none(),
            "resets_at absent in old JSON must default to None"
        );
        assert!(
            home.source.is_none(),
            "source absent in old JSON must default to None"
        );
    }

    #[test]
    fn deserialize_section_with_resets_at_and_source() {
        let json = r#"{
          "profiles": {
            "home": {
              "source": "api",
              "session": { "pct": 42, "resets": "9pm (Asia/Seoul)", "resets_at": 1788339599 }
            }
          }
        }"#;
        let data: UsageData = serde_json::from_str(json).expect("parse");
        let home = data.profiles.get("home").expect("home");
        assert_eq!(home.source.as_deref(), Some("api"));
        let sess = home.session.as_ref().expect("session");
        assert_eq!(sess.resets_at, Some(1_788_339_599));
    }

    #[test]
    fn reset_instant_prefers_resets_at_over_resets_string() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let section = UsageSection {
            pct: 10,
            // A resets string that would parse to something else entirely —
            // reset_instant must ignore it because resets_at is present.
            resets: Some("9pm (Asia/Seoul)".to_string()),
            resets_at: Some(1_788_339_599),
        };
        let instant = section.reset_instant(now).expect("resets_at must resolve");
        assert_eq!(instant.timestamp(), 1_788_339_599);
    }

    #[test]
    fn reset_instant_falls_back_to_resets_string_when_resets_at_absent() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        let section = UsageSection {
            pct: 10,
            resets: Some("9pm (Asia/Seoul)".to_string()),
            resets_at: None,
        };
        let instant = section
            .reset_instant(now)
            .expect("must fall back to parsing the resets string");
        // Matches account::reset's own fixture expectation for this exact input/now.
        let expected = chrono::Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        assert_eq!(instant, expected);
    }

    #[test]
    fn reset_instant_none_when_both_absent() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        let section = UsageSection {
            pct: 10,
            resets: None,
            resets_at: None,
        };
        assert!(section.reset_instant(now).is_none());
    }

    // ── attention ────────────────────────────────────────────────────────────

    #[test]
    fn old_json_without_attention_field_still_parses() {
        // SAMPLE_CACHE_JSON predates the `attention` field entirely.
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("old-shape JSON must still parse");
        let home = data.profiles.get("home").expect("home profile");
        assert!(
            home.attention.is_none(),
            "attention absent in old JSON must default to None"
        );
    }

    #[test]
    fn attention_survives_cache_round_trip() {
        let mut data = UsageData::default();
        data.profiles.insert(
            "work".to_string(),
            ProfileUsage {
                captured_at: Some("2026-08-30T00:00:00Z".to_string()),
                session: Some(UsageSection {
                    pct: 12,
                    resets: None,
                    resets_at: None,
                }),
                attention: Some(Attention {
                    kind: AttentionKind::NeedsLogin,
                    message: "credentials expired".to_string(),
                    action: "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"
                        .to_string(),
                    since_epoch: Some(1_756_000_000),
                }),
                ..Default::default()
            },
        );

        let serialized = serde_json::to_string(&data).expect("serialize");
        let round_tripped: UsageData = serde_json::from_str(&serialized).expect("re-parse");

        let att = round_tripped.profiles["work"]
            .attention
            .as_ref()
            .expect("attention must survive the round trip");
        assert_eq!(att.kind, AttentionKind::NeedsLogin);
        assert_eq!(att.message, "credentials expired");
        assert_eq!(
            att.action,
            "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"
        );
        assert_eq!(att.since_epoch, Some(1_756_000_000));
    }

    #[test]
    fn attention_kind_serializes_snake_case() {
        let att = Attention {
            kind: AttentionKind::NeedsRefresh,
            message: "access token expired".to_string(),
            action: "csm --profile home".to_string(),
            since_epoch: None,
        };
        let json = serde_json::to_string(&att).unwrap();
        assert!(json.contains("\"kind\":\"needs_refresh\""), "{json}");
        assert!(
            json.contains("since_epoch"),
            "since_epoch key is present even when null (no skip_serializing_if): {json}"
        );

        let att2 = Attention {
            kind: AttentionKind::NeedsLogin,
            ..att
        };
        let json2 = serde_json::to_string(&att2).unwrap();
        assert!(json2.contains("\"kind\":\"needs_login\""), "{json2}");
    }
}
