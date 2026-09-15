//! `GET /api/oauth/usage` — request + response mapping.
//!
//! Every response field is `Option` + `#[serde(default)]`, and unknown keys
//! are ignored (ordinary serde behavior, no `deny_unknown_fields`): the
//! endpoint is not a documented public contract, so a field Anthropic adds,
//! renames, or nulls out later must degrade to "missing", never a hard parse
//! failure that takes usage metering down crate-wide.
//!
//! Mapping (spec "실측으로 확정된 사실"): `limits[]` is authoritative when
//! present — `kind == "session"` for the session section, `"weekly_all"` for
//! the all-tiers weekly section, `"weekly_scoped"` (with a
//! `scope.model.display_name`) for the separately-capped model tier. When
//! `limits[]` doesn't carry a given kind, `five_hour`/`seven_day` are the
//! fallback for session/week_all respectively — there is no fallback source
//! for `weekly_scoped`, since only `limits[]` carries per-model scoping.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::usage::model::{ProfileUsage, UsageSection};

use super::display;

/// The compiled-in default API base. `pub` so `local::collect` can compare
/// `resolve_base()`'s result against it and warn when an override is active
/// (see [`fetch_usage`]'s doc on `CSM_USAGE_API_BASE`).
pub const DEFAULT_BASE: &str = "https://api.anthropic.com";

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// HTTP 429 — this token is over its request budget for the window.
    #[error("rate limited (429)")]
    RateLimited,
    /// HTTP 401/403 — the token is invalid (expired, revoked, wrong scope).
    #[error("unauthorized")]
    Unauthorized,
    /// Any other non-2xx status.
    #[error("HTTP {0}")]
    Http(u16),
    /// Connection/transport failure (DNS, timeout, TLS, …). The message is a
    /// diagnostic string from `reqwest::Error`'s `Display` — never the token.
    #[error("network error: {0}")]
    Network(String),
    /// The response body was not valid `OauthUsage` JSON.
    #[error("response parse error: {0}")]
    Parse(String),
    /// `CSM_USAGE_API_BASE` resolved to something other than `https://` or a
    /// loopback host. Every profile's live OAuth access token is about to be
    /// sent as a Bearer credential to whatever this resolves to — an
    /// unvalidated env var pointing that at an attacker-controlled
    /// `http://` host would exfiltrate it in cleartext, silently. See
    /// [`fetch_usage`]'s doc.
    #[error(
        "unsafe API base '{0}' — must be https:// or a loopback host (localhost/127.0.0.1/::1)"
    )]
    UnsafeBase(String),
}

// ─── response shape ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelScope {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub model: Option<ModelScope>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub scope: Option<Scope>,
}

/// Deserialized `/api/oauth/usage` response. Only the fields this crate uses
/// are modeled; every other top-level key (`extra_usage`, `spend`,
/// `seven_day_opus`, …) is dropped silently by serde.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OauthUsage {
    #[serde(default)]
    pub five_hour: Option<Window>,
    #[serde(default)]
    pub seven_day: Option<Window>,
    #[serde(default)]
    pub limits: Vec<Limit>,
}

// ─── request ────────────────────────────────────────────────────────────────

/// Fetch `/api/oauth/usage` for the account behind `token`.
///
/// Timeouts: 3s connect / 8s total — the same order of magnitude as
/// `transport.rs`'s hub HTTP path, generous enough for a real network round
/// trip but bounded so a hung request never blocks `collect()` across every
/// other profile indefinitely.
///
/// `base` is validated ([`validate_base`]) before anything is sent: `token`
/// — every configured profile's live OAuth access token — is attached as a
/// Bearer credential to `{base}/api/oauth/usage`, and `base` is
/// operator-overridable via `CSM_USAGE_API_BASE` (mainly for tests). An
/// unvalidated override pointing that at an attacker-controlled `http://`
/// host would exfiltrate every profile's token in cleartext, silently — the
/// table would still render normally, or show a generic network error.
pub fn fetch_usage(token: &str, base: &str) -> Result<OauthUsage, ApiError> {
    use std::time::Duration;

    validate_base(base)?;

    let url = format!("{}/api/oauth/usage", base.trim_end_matches('/'));

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| ApiError::Network(e.to_string()))?;

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|e| ApiError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    if status == 429 {
        return Err(ApiError::RateLimited);
    }
    if status == 401 || status == 403 {
        return Err(ApiError::Unauthorized);
    }
    if !(200..300).contains(&status) {
        return Err(ApiError::Http(status));
    }

    let body = resp.text().map_err(|e| ApiError::Network(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| ApiError::Parse(e.to_string()))
}

/// Reject an API base that isn't `https://` and isn't a loopback host
/// (`localhost`/`127.0.0.1`/`::1`). No `url` crate dependency — this is a
/// deliberately minimal parse of exactly the two things that matter for the
/// security question ("does this leak the Bearer token off-host over
/// cleartext"), not a general URL validator: scheme, then the host portion of
/// the authority (userinfo and port stripped, `[...]` IPv6-literal syntax
/// unwrapped). A base that doesn't even parse as `scheme://authority` is
/// rejected outright. `pub(crate)` so [`super::refresh`] validates its own
/// (env-overridable) token endpoint against exactly the same rule.
pub(crate) fn validate_base(base: &str) -> Result<(), ApiError> {
    let Some((scheme, rest)) = base.split_once("://") else {
        return Err(ApiError::UnsafeBase(base.to_string()));
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(after_bracket) = host_port.strip_prefix('[') {
        after_bracket.split(']').next().unwrap_or(after_bracket)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    let host_lc = host.to_ascii_lowercase();
    let is_loopback = matches!(host_lc.as_str(), "localhost" | "127.0.0.1" | "::1");
    if scheme.eq_ignore_ascii_case("https") || is_loopback {
        Ok(())
    } else {
        Err(ApiError::UnsafeBase(base.to_string()))
    }
}

/// Resolve the API base URL: `CSM_USAGE_API_BASE` if set (trailing slash
/// trimmed), else the compiled-in default. Unlike the removed hub URL, this
/// default IS safe to compile in — `api.anthropic.com` is Anthropic's public
/// endpoint, not site-private infrastructure.
pub fn resolve_base() -> String {
    let base = std::env::var("CSM_USAGE_API_BASE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    base.trim_end_matches('/').to_string()
}

// ─── mapping: OauthUsage → ProfileUsage ────────────────────────────────────

/// Map a raw API response to the crate's [`ProfileUsage`] shape, per the
/// module-level mapping doc. `source` is always `"api"`; `captured_at` is
/// `now` in RFC-3339.
pub fn to_profile_usage(u: &OauthUsage, now: DateTime<Utc>) -> ProfileUsage {
    let session = section_from(find_limit(&u.limits, "session"), u.five_hour.as_ref());
    let week_all = section_from(find_limit(&u.limits, "weekly_all"), u.seven_day.as_ref());
    let (week_fable, week_model_label) = weekly_scoped(&u.limits);

    ProfileUsage {
        captured_at: Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        session,
        week_all,
        week_fable,
        week_model_label,
        session_stats: Vec::new(),
        source: Some("api".to_string()),
        // A successful api probe means the credentials are healthy right
        // now — any attention a prior probe attached is stale and must be
        // cleared, not carried forward (unlike the statusline merge path,
        // which never re-checks credentials and so must preserve it).
        attention: None,
    }
}

fn find_limit<'a>(limits: &'a [Limit], kind: &str) -> Option<&'a Limit> {
    limits.iter().find(|l| l.kind.as_deref() == Some(kind))
}

/// Build a [`UsageSection`] from a `limits[]` entry when it carries a
/// percent, else fall back to the corresponding `five_hour`/`seven_day`
/// window. `None` when neither source has a percent to report.
///
/// A `limits[]` entry that MATCHES `kind` but carries `percent: null` must
/// still fall through to `window` — the endpoint is not a stable contract
/// (module doc), so a transient null or a future reshape must degrade to the
/// window fallback, never short-circuit the whole function via `?` and
/// silently drop a percent that's sitting right there in `five_hour`/
/// `seven_day`. Losing this section entirely can make a saturated profile
/// invisible to `report::join_one`'s `has_any` check and drop it from
/// scoring's candidate list outright.
fn section_from(limit: Option<&Limit>, window: Option<&Window>) -> Option<UsageSection> {
    if let Some(pct) = limit.and_then(|l| l.percent) {
        let resets_at = limit
            .and_then(|l| l.resets_at.as_deref())
            .and_then(parse_rfc3339_epoch);
        return Some(build_section(pct.round() as i64, resets_at));
    }
    let w = window?;
    let pct = w.utilization?.round() as i64;
    let resets_at = w.resets_at.as_deref().and_then(parse_rfc3339_epoch);
    Some(build_section(pct, resets_at))
}

/// The separately-capped model-tier weekly limit: the `limits[]` entry whose
/// `kind == "weekly_scoped"` AND whose `scope.model.display_name` is present.
/// There is no `five_hour`/`seven_day`-style fallback for this one — only
/// `limits[]` carries per-model scoping at all.
fn weekly_scoped(limits: &[Limit]) -> (Option<UsageSection>, Option<String>) {
    let entry = limits.iter().find(|l| {
        l.kind.as_deref() == Some("weekly_scoped")
            && l.scope
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .and_then(|m| m.display_name.as_ref())
                .is_some()
    });
    let Some(l) = entry else {
        return (None, None);
    };
    let Some(pct) = l.percent else {
        return (None, None);
    };
    let resets_at = l.resets_at.as_deref().and_then(parse_rfc3339_epoch);
    let label = l
        .scope
        .as_ref()
        .and_then(|s| s.model.as_ref())
        .and_then(|m| m.display_name.clone());
    (Some(build_section(pct.round() as i64, resets_at)), label)
}

fn build_section(pct: i64, resets_at: Option<i64>) -> UsageSection {
    UsageSection {
        pct,
        resets: resets_at.map(display::format_resets),
        resets_at,
    }
}

fn parse_rfc3339_epoch(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// The exact fixture at
    /// `/private/tmp/.../scratchpad/oauth_usage_fixture.json` — a real
    /// (neutrally-renamed) `/api/oauth/usage` response captured during the
    /// spec's live probe. No private identifiers.
    const FIXTURE: &str = r#"
    {
      "five_hour": {
        "utilization": 42.0,
        "resets_at": "2026-09-02T08:59:59.626084+00:00",
        "limit_dollars": null,
        "used_dollars": null,
        "remaining_dollars": null,
        "locked_reason": null
      },
      "seven_day": {
        "utilization": 31.0,
        "resets_at": "2026-09-02T08:59:59.626105+00:00",
        "limit_dollars": null,
        "used_dollars": null,
        "remaining_dollars": null,
        "locked_reason": null
      },
      "seven_day_oauth_apps": null,
      "seven_day_opus": null,
      "seven_day_sonnet": null,
      "seven_day_cowork": null,
      "seven_day_omelette": null,
      "tangelo": null,
      "iguana_necktie": null,
      "omelette_promotional": null,
      "nimbus_quill": {
        "utilization": 0.0,
        "resets_at": null,
        "limit_dollars": null,
        "used_dollars": null,
        "remaining_dollars": null,
        "locked_reason": null
      },
      "cinder_cove": null,
      "amber_ladder": null,
      "juniper_tide": null,
      "extra_usage": {
        "is_enabled": false,
        "monthly_limit": null,
        "used_credits": null,
        "utilization": null,
        "currency": null,
        "decimal_places": null,
        "disabled_reason": null,
        "user_disabled": false,
        "spend_limit_reached": false,
        "credits_ever_enabled": false,
        "daily": null,
        "weekly": null
      },
      "limits": [
        {
          "kind": "session",
          "group": "session",
          "percent": 42,
          "severity": "normal",
          "resets_at": "2026-09-02T08:59:59.626084+00:00",
          "scope": null,
          "is_active": true
        },
        {
          "kind": "weekly_all",
          "group": "weekly",
          "percent": 31,
          "severity": "normal",
          "resets_at": "2026-09-02T08:59:59.626105+00:00",
          "scope": null,
          "is_active": false
        },
        {
          "kind": "weekly_scoped",
          "group": "weekly",
          "percent": 15,
          "severity": "normal",
          "resets_at": "2026-09-02T08:59:59.626334+00:00",
          "scope": {
            "model": {
              "id": null,
              "display_name": "Fable"
            },
            "surface": null
          },
          "is_active": false
        }
      ],
      "spend": {
        "used": { "amount_minor": 0, "currency": "USD", "exponent": 2 },
        "limit": null,
        "percent": 0,
        "severity": "normal",
        "enabled": false,
        "disabled_reason": null,
        "cap": null,
        "balance": null,
        "auto_reload": null,
        "disclaimer": "...",
        "can_purchase_credits": false,
        "can_toggle": false
      },
      "member_dashboard_available": false
    }
    "#;

    /// All three `resets_at` timestamps in the fixture truncate (whole
    /// seconds) to this same epoch — independently verified.
    const FIXTURE_EPOCH: i64 = 1_788_339_599;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    #[test]
    fn fixture_parses() {
        let parsed: OauthUsage = serde_json::from_str(FIXTURE).expect("fixture must parse");
        assert_eq!(parsed.limits.len(), 3);
    }

    #[test]
    fn fixture_maps_session_week_all_week_fable() {
        let parsed: OauthUsage = serde_json::from_str(FIXTURE).unwrap();
        let pu = to_profile_usage(&parsed, now());

        let session = pu.session.expect("session");
        assert_eq!(session.pct, 42);
        assert_eq!(session.resets_at, Some(FIXTURE_EPOCH));

        let week_all = pu.week_all.expect("week_all");
        assert_eq!(week_all.pct, 31);
        assert_eq!(week_all.resets_at, Some(FIXTURE_EPOCH));

        let week_fable = pu.week_fable.expect("week_fable");
        assert_eq!(week_fable.pct, 15);
        assert_eq!(week_fable.resets_at, Some(FIXTURE_EPOCH));

        assert_eq!(pu.week_model_label.as_deref(), Some("Fable"));
        assert_eq!(pu.source.as_deref(), Some("api"));
        assert!(pu.captured_at.is_some());
    }

    #[test]
    fn limit_present_but_percent_null_falls_back_to_window() {
        // A `limits[]` entry that MATCHES `kind == "session"` but carries a
        // null `percent` must not short-circuit to `None` — it must fall
        // through to `five_hour`, the same as if the entry were absent
        // entirely. Regression: `l.percent?` used to `return`  from the
        // whole function on this shape, dropping a percent sitting right
        // there in `five_hour`.
        let json = r#"{
          "five_hour": { "utilization": 80.0, "resets_at": "2026-09-02T08:59:59+00:00" },
          "limits": [
            { "kind": "session", "percent": null, "resets_at": "2026-09-02T08:59:59+00:00" }
          ]
        }"#;
        let parsed: OauthUsage = serde_json::from_str(json).unwrap();
        let pu = to_profile_usage(&parsed, now());
        assert_eq!(
            pu.session.expect("must fall back to five_hour").pct,
            80,
            "a percentless limits[] entry must not hide the five_hour reading"
        );
    }

    #[test]
    fn limits_absent_falls_back_to_five_hour_seven_day() {
        let json = r#"{
          "five_hour": { "utilization": 7.0, "resets_at": "2026-09-02T08:59:59+00:00" },
          "seven_day": { "utilization": 88.0, "resets_at": "2026-09-03T08:59:59+00:00" },
          "limits": []
        }"#;
        let parsed: OauthUsage = serde_json::from_str(json).unwrap();
        let pu = to_profile_usage(&parsed, now());

        assert_eq!(pu.session.expect("session from five_hour").pct, 7);
        assert_eq!(pu.week_all.expect("week_all from seven_day").pct, 88);
        // No weekly_scoped limit at all → no fallback source exists for it.
        assert!(pu.week_fable.is_none());
        assert!(pu.week_model_label.is_none());
    }

    #[test]
    fn empty_response_maps_to_all_none() {
        let parsed: OauthUsage = serde_json::from_str("{}").unwrap();
        let pu = to_profile_usage(&parsed, now());
        assert!(pu.session.is_none());
        assert!(pu.week_all.is_none());
        assert!(pu.week_fable.is_none());
        assert!(pu.week_model_label.is_none());
        assert_eq!(pu.source.as_deref(), Some("api"));
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        // The fixture itself has a pile of unrelated keys (extra_usage,
        // spend, tangelo, …) — this just asserts parsing doesn't choke on an
        // even stranger unrecognised key.
        let json = r#"{"some_future_field": {"nested": [1,2,3]}, "limits": []}"#;
        let result: Result<OauthUsage, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn weekly_scoped_without_display_name_is_ignored() {
        // A weekly_scoped limit with no scope.model.display_name has nothing
        // to label the column with, so it must not populate week_fable.
        let json = r#"{
          "limits": [
            { "kind": "weekly_scoped", "percent": 50, "resets_at": null, "scope": { "model": { "display_name": null } } }
          ]
        }"#;
        let parsed: OauthUsage = serde_json::from_str(json).unwrap();
        let pu = to_profile_usage(&parsed, now());
        assert!(pu.week_fable.is_none());
        assert!(pu.week_model_label.is_none());
    }

    #[test]
    fn percent_rounds_to_nearest_int() {
        let json = r#"{"limits": [{"kind": "session", "percent": 42.6}]}"#;
        let parsed: OauthUsage = serde_json::from_str(json).unwrap();
        let pu = to_profile_usage(&parsed, now());
        assert_eq!(pu.session.unwrap().pct, 43);
    }

    // ── validate_base ────────────────────────────────────────────────────────

    #[test]
    fn validate_base_accepts_https() {
        assert!(validate_base("https://api.anthropic.com").is_ok());
        assert!(validate_base("https://custom.example:8443").is_ok());
    }

    #[test]
    fn validate_base_accepts_loopback_over_http() {
        assert!(validate_base("http://localhost:8080").is_ok());
        assert!(validate_base("http://127.0.0.1:8080").is_ok());
        assert!(validate_base("http://[::1]:8080").is_ok());
        assert!(
            validate_base("http://LOCALHOST").is_ok(),
            "host match is case-insensitive"
        );
    }

    #[test]
    fn validate_base_rejects_http_to_a_non_loopback_host() {
        // The exfiltration scenario this guards against: a redirected token
        // destination over cleartext to an attacker-controlled host.
        let err = validate_base("http://collector.example:8080").unwrap_err();
        assert!(matches!(err, ApiError::UnsafeBase(b) if b == "http://collector.example:8080"));
    }

    #[test]
    fn validate_base_rejects_unparseable_base() {
        assert!(validate_base("not-a-url").is_err());
    }

    #[test]
    fn fetch_usage_rejects_unsafe_base_before_any_network_call() {
        // A fake token is fine here — validate_base short-circuits before the
        // token is ever attached to a request.
        let err = fetch_usage("tok_example", "http://collector.example").unwrap_err();
        assert!(matches!(err, ApiError::UnsafeBase(_)));
    }

    #[test]
    fn resolve_base_defaults_and_trims_trailing_slash() {
        let saved = std::env::var("CSM_USAGE_API_BASE").ok();
        std::env::remove_var("CSM_USAGE_API_BASE");
        assert_eq!(resolve_base(), DEFAULT_BASE);

        std::env::set_var("CSM_USAGE_API_BASE", "https://custom.example/");
        assert_eq!(resolve_base(), "https://custom.example");

        match saved {
            Some(v) => std::env::set_var("CSM_USAGE_API_BASE", v),
            None => std::env::remove_var("CSM_USAGE_API_BASE"),
        }
    }
}
