//! Serde model for `.usage-cache.json`.
//!
//! Cache shape (from spec §4a):
//!
//! ```json
//! {
//!   "captured_at": "2026-06-17T07:13:19Z",
//!   "profiles": {
//!     "<profile_name>": {
//!       "captured_at": "<ISO-8601>",
//!       "session":     { "pct": <int>, "resets": <string|null> } | null,
//!       "week_all":    { "pct": <int>, "resets": <string|null> } | null,
//!       "week_sonnet": { "pct": <int>, "resets": <string|null> } | null,
//!       "session_stats": ["<string>", ...]
//!     }
//!   },
//!   "errors": { "<profile_name>": "<error string>" }
//! }
//! ```
//!
//! Design choices (spec mandated):
//! - Each section (`session`/`week_all`/`week_sonnet`) is `Option<UsageSection>`.
//!   `parse-usage.py` returns `None` for an absent section → serde null → `None`.
//! - `resets` inside a present section is `Option<String>` (may be null).
//! - `errors` key is absent when all profiles succeeded → `Option<HashMap<…>>`.
//! - `#[serde(default)]` throughout for forward-compatible tolerance.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── top-level ────────────────────────────────────────────────────────────────

/// Deserialized form of `.usage-cache.json` (positive TTL cache) and the hub's
/// `/cc-usage/api/data/limits` JSON response (same shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageData {
    /// ISO-8601 timestamp at which the hub collected this data.
    /// May be absent on older cache files; callers use the *file mtime* for
    /// freshness, NOT this field (spec §4a).
    #[serde(default)]
    pub captured_at: Option<String>,

    /// Per-profile usage.  Key = profile name (e.g. `"personal"`, `"work"`).
    #[serde(default)]
    pub profiles: HashMap<String, ProfileUsage>,

    /// Profiles that could not be scraped.  Key = profile name, value = error
    /// string.  The `errors` key is **absent** when all profiles succeeded.
    #[serde(default)]
    pub errors: Option<HashMap<String, String>>,
}

// ─── per-profile ──────────────────────────────────────────────────────────────

/// Usage data for a single profile.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileUsage {
    /// ISO-8601 capture timestamp for this profile's slice.
    #[serde(default)]
    pub captured_at: Option<String>,

    /// Session-level quota (resets more frequently than weekly).
    /// `None` when the hub returned `null` for this section.
    #[serde(default)]
    pub session: Option<UsageSection>,

    /// Weekly aggregate quota across all model tiers.
    /// `None` when absent or null.
    #[serde(default)]
    pub week_all: Option<UsageSection>,

    /// Weekly Sonnet-tier quota.
    /// `None` when absent or null.
    #[serde(default)]
    pub week_sonnet: Option<UsageSection>,

    /// Raw stat strings from the hub (e.g. token counts).  Optional; not used
    /// for scoring but preserved for debugging.
    #[serde(default)]
    pub session_stats: Vec<String>,
}

// ─── per-section ──────────────────────────────────────────────────────────────

/// A single usage quota section (session, week_all, or week_sonnet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSection {
    /// Percentage consumed (0–100; may exceed 100 on burst).
    pub pct: i64,

    /// Human-readable reset time string, e.g. `"9pm (Asia/Seoul)"` or
    /// `"Jun 18 at 9pm (Asia/Seoul)"`.  `None` when the hub omitted it.
    #[serde(default)]
    pub resets: Option<String>,
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

    /// A representative `.usage-cache.json` payload with:
    /// - `personal`: all three sections present with real values
    /// - `work`: `week_sonnet` absent (null), `session.resets` null
    /// - `errors`: one errored profile
    /// - top-level `captured_at` present
    const SAMPLE_CACHE_JSON: &str = r#"
    {
      "captured_at": "2026-06-17T07:13:19Z",
      "profiles": {
        "personal": {
          "captured_at": "2026-06-17T07:13:17Z",
          "session": {
            "pct": 42,
            "resets": "9pm (Asia/Seoul)"
          },
          "week_all": {
            "pct": 31,
            "resets": "Jun 18 at 9pm (Asia/Seoul)"
          },
          "week_sonnet": {
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
          "week_sonnet": null,
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

        // personal profile
        let personal = data.profiles.get("personal").expect("personal profile");
        let sess = personal.session.as_ref().expect("personal.session");
        assert_eq!(sess.pct, 42);
        assert_eq!(sess.resets.as_deref(), Some("9pm (Asia/Seoul)"));

        let week_all = personal.week_all.as_ref().expect("personal.week_all");
        assert_eq!(week_all.pct, 31);
        assert_eq!(
            week_all.resets.as_deref(),
            Some("Jun 18 at 9pm (Asia/Seoul)")
        );

        let week_sonnet = personal.week_sonnet.as_ref().expect("personal.week_sonnet");
        assert_eq!(week_sonnet.pct, 15);

        assert_eq!(personal.session_stats.len(), 2);

        // work profile — week_sonnet is null → None
        let work = data.profiles.get("work").expect("work profile");
        assert!(
            work.week_sonnet.is_none(),
            "work.week_sonnet should be None (null in JSON)"
        );
        // session.resets is null → None
        let esess = work.session.as_ref().expect("work.session");
        assert_eq!(esess.pct, 5);
        assert!(
            esess.resets.is_none(),
            "work.session.resets should be None"
        );
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
        let json = r#"{"profiles": {"personal": {"session": {"pct": 10}, "week_all": {"pct": 20}}}}"#;
        let data: UsageData = serde_json::from_str(json).expect("minimal JSON");
        assert!(data.errors.is_none(), "errors should be None when key absent");
        let p = data.profiles.get("personal").expect("personal");
        assert!(p.week_sonnet.is_none());
    }

    #[test]
    fn current_usage_returns_correct_pcts() {
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("parse for current_usage test");

        let (sess, week) = data.current_usage("personal").expect("personal present");
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
        let data: UsageData =
            serde_json::from_str(SAMPLE_CACHE_JSON).expect("initial parse");
        let serialized = serde_json::to_string(&data).expect("serialize");
        let data2: UsageData = serde_json::from_str(&serialized).expect("re-parse");

        // Spot-check a field to verify the roundtrip.
        assert_eq!(
            data.profiles["personal"].session.as_ref().unwrap().pct,
            data2.profiles["personal"].session.as_ref().unwrap().pct
        );
    }
}
