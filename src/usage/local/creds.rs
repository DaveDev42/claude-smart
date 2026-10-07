//! Credential lookup for the local usage collector.
//!
//! Two sources, both Orca's:
//! - the account `D` holds: [`lookup_runtime`] reads the runtime grant in
//!   Orca's order (M:239969-239999): on macOS the scoped Keychain item for
//!   `D`, then the unscoped (legacy) one; then `D/.credentials.json`. Every
//!   Keychain read goes through [`crate::orca::keychain`], whose runner
//!   refuses the real `/usr/bin/security` under `cfg(test)`.
//! - every other account: its Orca stash, read by the collector through
//!   [`crate::orca::stash`] and parsed here with [`parse_blob`].
//!
//! **This module never writes and never refreshes.** The active grant is
//! claude's and Orca's to rotate; an inactive stash is refreshed only by the
//! collector's gated offline path (`super::collect`, design section 8).
//!
//! The access token is never logged, printed, or embedded in an error string
//! anywhere in this module. [`OauthToken`] hand-writes its `Debug` impl to
//! redact it — never derive `Debug` on that struct. Parse errors name the
//! category only, never the text that failed to parse.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::orca::HostOs;
use crate::orca::keychain::{self, KeychainUser};
use crate::orca::runtime::RuntimePaths;

// ─── public types ───────────────────────────────────────────────────────────

/// A live OAuth token read from local credential storage.
///
/// Deliberately does NOT derive `Clone`/`Debug` from the compiler — `Debug` is
/// hand-written below to redact `access_token`, and callers that need the
/// token pass it straight to [`super::api::fetch_usage`] rather than storing
/// copies.
pub struct OauthToken {
    pub access_token: String,
    pub expires_at_ms: Option<i64>,
    /// `claudeAiOauth.refreshTokenExpiresAt` — when present, the refresh
    /// token itself (not the access token) stops being usable. Carried
    /// through on a *live* token purely for completeness; the field that
    /// actually drives the NeedsRefresh/NeedsLogin split is
    /// [`CredError::Expired`]'s `refresh_alive`, computed once at parse time
    /// against this same value (see [`parse_blob`]).
    pub refresh_expires_at_ms: Option<i64>,
    pub subscription_type: Option<String>,
}

impl std::fmt::Debug for OauthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthToken")
            .field("access_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("refresh_expires_at_ms", &self.refresh_expires_at_ms)
            .field("subscription_type", &self.subscription_type)
            .finish()
    }
}

/// Why credential lookup failed for a profile.
#[derive(Debug, thiserror::Error)]
pub enum CredError {
    /// No credential entry exists at all (keychain miss + no `.credentials.json`,
    /// or the file/entry exists but carries no `claudeAiOauth.accessToken`).
    #[error("no credentials found")]
    NotFound,
    /// A token was found but its `expiresAt` is at or before `now`.
    ///
    /// `refresh_alive` is `true` iff `refreshTokenExpiresAt` was present AND
    /// still in the future at parse time — i.e. Claude Code itself would
    /// silently mint a new access token on its next run under this profile
    /// (NeedsRefresh). `false` (absent OR already past) means the profile is
    /// logged out in every way that matters (NeedsLogin) — see
    /// `local::mod::resolve`'s three-way classification.
    #[error("token expired")]
    Expired {
        refresh_alive: bool,
        expired_at_ms: i64,
    },
    /// The entry exists but couldn't be read/parsed (permission error,
    /// corrupt JSON, keychain command failure, etc). The message is a
    /// diagnostic string — NEVER the token itself.
    #[error("credentials unreadable: {0}")]
    Unreadable(String),
}

// ─── credentials blob shape (`claudeAiOauth`) ──────────────────────────────
//
// Written by Claude Code itself, both to the macOS Keychain (as the `-w`
// secret) and to `<config_dir>/.credentials.json` on every other platform.
// We only read the subset we need; unknown sibling keys (`mcpOAuth`,
// `refreshToken`, `scopes`, …) are ignored by ordinary serde behavior (no
// `deny_unknown_fields`).

#[derive(Debug, Deserialize)]
struct CredentialsBlob {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Debug, Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

// ─── public entry-points ────────────────────────────────────────────────────

/// Look up the runtime grant: the account `D` holds. See the module doc for
/// the order. A Keychain error other than "not found" is reported as
/// unreadable rather than falling through to the file, so a locked Keychain
/// never makes csm probe with a stale file copy.
pub fn lookup_runtime(
    os: HostOs,
    paths: &RuntimePaths,
    user: &KeychainUser,
    now: DateTime<Utc>,
) -> Result<OauthToken, CredError> {
    if os == HostOs::MacOs {
        let dir = paths.config_dir.to_string_lossy().into_owned();
        match keychain::read_runtime_aggregate(Some(&dir), user) {
            Ok(Some(secret)) => return parse_blob(secret.expose(), now),
            Ok(None) => {}
            Err(e) => return Err(CredError::Unreadable(format!("Keychain: {e}"))),
        }
    }
    lookup_file(&paths.credentials_path, now)
}

/// Hard cap on `.credentials.json` reads — a real credentials blob is a few
/// KB; this matches the 256 KiB ceiling `csm usage capture`/`csm statusline`
/// already apply to their own external-input reads.
const CREDENTIALS_FILE_CAP_BYTES: u64 = 256 * 1024;

/// Read and parse a `.credentials.json` at `path`.
pub fn lookup_file(path: &Path, now: DateTime<Utc>) -> Result<OauthToken, CredError> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CredError::NotFound
        } else {
            CredError::Unreadable(format!("{}: {}", path.display(), e.kind()))
        }
    })?;
    // Reject anything that isn't a regular file up front — a FIFO or a
    // device-node symlink here would otherwise let `read_to_end` block
    // forever (no writer) or stream unbounded data (`/dev/zero`).
    let meta = file
        .metadata()
        .map_err(|e| CredError::Unreadable(format!("{}: {}", path.display(), e.kind())))?;
    if !meta.is_file() {
        return Err(CredError::Unreadable(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    let mut buf = Vec::new();
    file.take(CREDENTIALS_FILE_CAP_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| CredError::Unreadable(format!("{}: {}", path.display(), e.kind())))?;
    let text = String::from_utf8(buf)
        .map_err(|_| CredError::Unreadable(format!("{}: not valid UTF-8", path.display())))?;
    parse_blob(&text, now)
}

// ─── pure helpers (unit-tested without touching the real keychain/disk) ────

/// Parse a credentials blob — either `security -w`'s stdout or
/// `.credentials.json`'s content — into an [`OauthToken`], applying the
/// hex-decode defensive fallback and the expiry check against `now`.
pub fn parse_blob(text: &str, now: DateTime<Utc>) -> Result<OauthToken, CredError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CredError::Unreadable("empty credentials blob".into()));
    }

    // Defensive: some keychain configurations return the secret as a hex
    // string rather than the raw bytes; if the entire body is hex digits,
    // decode it before treating it as JSON.
    let json_text: String = if trimmed.len().is_multiple_of(2) && is_all_hex(trimmed) {
        match hex_decode(trimmed).and_then(|bytes| String::from_utf8(bytes).ok()) {
            Some(decoded) => decoded,
            None => trimmed.to_string(), // not actually hex-encoded JSON; parse as-is
        }
    } else {
        trimmed.to_string()
    };

    // The category only: a serde message can quote the value it choked on.
    let blob: CredentialsBlob = serde_json::from_str(&json_text).map_err(|e| {
        CredError::Unreadable(format!("credentials JSON parse error ({:?})", e.classify()))
    })?;
    let oauth = blob.claude_ai_oauth.ok_or(CredError::NotFound)?;
    let access_token = oauth.access_token.ok_or(CredError::NotFound)?;

    if let Some(expires_at_ms) = oauth.expires_at
        && expires_at_ms <= now.timestamp_millis()
    {
        let refresh_alive = oauth
            .refresh_token_expires_at
            .is_some_and(|r| r > now.timestamp_millis());
        return Err(CredError::Expired {
            refresh_alive,
            expired_at_ms: expires_at_ms,
        });
    }

    Ok(OauthToken {
        access_token,
        expires_at_ms: oauth.expires_at,
        refresh_expires_at_ms: oauth.refresh_token_expires_at,
        subscription_type: oauth.subscription_type,
    })
}

fn is_all_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn parse_blob_plain_json_ok() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999,"subscriptionType":"pro"}}"#;
        let tok = parse_blob(json, now()).expect("should parse");
        assert_eq!(tok.access_token, "tok_example");
        assert_eq!(tok.subscription_type.as_deref(), Some("pro"));
        assert_eq!(tok.expires_at_ms, Some(9_999_999_999_999));
        assert_eq!(tok.refresh_expires_at_ms, None);
    }

    #[test]
    fn parse_blob_carries_refresh_expires_at_ms_on_live_token() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999,"refreshTokenExpiresAt":8888888888888}}"#;
        let tok = parse_blob(json, now()).expect("should parse");
        assert_eq!(tok.refresh_expires_at_ms, Some(8_888_888_888_888));
    }

    #[test]
    fn parse_blob_hex_encoded_decodes_then_parses() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999}}"#;
        let hex: String = json.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let tok = parse_blob(&hex, now()).expect("hex blob should decode then parse");
        assert_eq!(tok.access_token, "tok_example");
    }

    #[test]
    fn parse_blob_expires_at_zero_is_expired() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":0}}"#;
        match parse_blob(json, now()) {
            Err(CredError::Expired { expired_at_ms, .. }) => assert_eq!(expired_at_ms, 0),
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_expired_token_is_expired_error() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":1}}"#;
        let result = parse_blob(json, now());
        match result {
            Err(CredError::Expired {
                refresh_alive,
                expired_at_ms,
            }) => {
                assert!(!refresh_alive, "no refreshTokenExpiresAt at all → dead");
                assert_eq!(expired_at_ms, 1);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_exactly_at_expiry_is_expired() {
        // expiresAt == now → Expired (spec: "expiresAt(ms) <= now → Expired").
        let now_ms = now().timestamp_millis();
        let json =
            format!(r#"{{"claudeAiOauth":{{"accessToken":"tok_example","expiresAt":{now_ms}}}}}"#);
        let result = parse_blob(&json, now());
        assert!(
            matches!(result, Err(CredError::Expired { .. })),
            "{result:?}"
        );
    }

    // ── refresh_alive classification ───────────────────────────────────────

    #[test]
    fn parse_blob_expired_with_live_refresh_token_is_refresh_alive() {
        let now_ms = now().timestamp_millis();
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"tok_example","expiresAt":1,"refreshTokenExpiresAt":{}}}}}"#,
            now_ms + 1_000_000
        );
        let result = parse_blob(&json, now());
        match result {
            Err(CredError::Expired {
                refresh_alive,
                expired_at_ms,
            }) => {
                assert!(refresh_alive, "future refreshTokenExpiresAt → alive");
                assert_eq!(expired_at_ms, 1);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_expired_with_expired_refresh_token_is_not_refresh_alive() {
        let now_ms = now().timestamp_millis();
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"tok_example","expiresAt":1,"refreshTokenExpiresAt":{}}}}}"#,
            now_ms - 1_000_000
        );
        let result = parse_blob(&json, now());
        match result {
            Err(CredError::Expired { refresh_alive, .. }) => {
                assert!(!refresh_alive, "past refreshTokenExpiresAt → dead");
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_expired_with_absent_refresh_token_expiry_is_not_refresh_alive() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":1}}"#;
        let result = parse_blob(json, now());
        match result {
            Err(CredError::Expired { refresh_alive, .. }) => {
                assert!(!refresh_alive, "absent refreshTokenExpiresAt → dead");
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_expired_with_refresh_token_expiring_exactly_at_now_is_not_alive() {
        // `> now`, not `>= now` — an equal-to-now refresh token is treated as
        // already dead, mirroring the access-token `<=` rule's conservatism.
        let now_ms = now().timestamp_millis();
        let json = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"tok_example","expiresAt":1,"refreshTokenExpiresAt":{now_ms}}}}}"#
        );
        let result = parse_blob(&json, now());
        match result {
            Err(CredError::Expired { refresh_alive, .. }) => assert!(!refresh_alive),
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_no_expiry_never_expires() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok_example"}}"#;
        let tok = parse_blob(json, now()).expect("no expiresAt = never expired");
        assert_eq!(tok.access_token, "tok_example");
        assert!(tok.expires_at_ms.is_none());
    }

    #[test]
    fn parse_blob_missing_oauth_key_is_not_found() {
        let result = parse_blob("{}", now());
        assert!(matches!(result, Err(CredError::NotFound)), "{result:?}");
    }

    #[test]
    fn parse_blob_missing_access_token_is_not_found() {
        let json = r#"{"claudeAiOauth":{"subscriptionType":"pro"}}"#;
        let result = parse_blob(json, now());
        assert!(matches!(result, Err(CredError::NotFound)), "{result:?}");
    }

    #[test]
    fn parse_blob_empty_string_is_unreadable() {
        let result = parse_blob("", now());
        assert!(
            matches!(result, Err(CredError::Unreadable(_))),
            "{result:?}"
        );
    }

    #[test]
    fn parse_blob_garbage_json_is_unreadable() {
        let result = parse_blob("not json at all", now());
        assert!(
            matches!(result, Err(CredError::Unreadable(_))),
            "{result:?}"
        );
    }

    #[test]
    fn parse_blob_tolerates_sibling_keys() {
        // mcpOAuth and extra claudeAiOauth fields (refreshToken, scopes, …)
        // must not break parsing — we only read the three fields we need.
        let json = r#"{
          "claudeAiOauth": {
            "accessToken": "tok_example",
            "refreshToken": "rtok_example",
            "expiresAt": 9999999999999,
            "refreshTokenExpiresAt": 9999999999999,
            "scopes": ["user:inference"],
            "subscriptionType": "pro",
            "rateLimitTier": "default",
            "clientId": "abc"
          },
          "mcpOAuth": {}
        }"#;
        let tok = parse_blob(json, now()).expect("sibling keys must be ignored");
        assert_eq!(tok.access_token, "tok_example");
    }

    // ── OauthToken::Debug redaction ────────────────────────────────────────────

    #[test]
    fn debug_impl_redacts_access_token() {
        let tok = OauthToken {
            access_token: "tok_example_super_secret_value".into(),
            expires_at_ms: Some(123),
            refresh_expires_at_ms: Some(456),
            subscription_type: Some("pro".into()),
        };
        let rendered = format!("{tok:?}");
        assert!(
            !rendered.contains("tok_example_super_secret_value"),
            "Debug output must never contain the raw token: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        assert!(
            rendered.contains("pro"),
            "non-secret fields should still show"
        );
    }

    // ── lookup_file / lookup_runtime ──────────────────────────────────────────

    #[test]
    fn lookup_file_reads_credentials_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999}}"#,
        )
        .unwrap();
        let tok = lookup_file(&dir.path().join(".credentials.json"), now())
            .expect("should read the file");
        assert_eq!(tok.access_token, "tok_example");
    }

    #[test]
    fn lookup_file_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = lookup_file(&dir.path().join(".credentials.json"), now());
        assert!(matches!(result, Err(CredError::NotFound)), "{result:?}");
    }

    #[test]
    fn lookup_file_larger_than_cap_is_bounded_not_oomed() {
        // A credentials blob far past the 256 KiB cap must be truncated (and
        // so fail to parse as JSON) rather than read in full — this is the
        // observable behavior of the bounded read; the point is that
        // `lookup_file` returns promptly with an error instead of allocating
        // unboundedly for a huge/streaming file.
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(CREDENTIALS_FILE_CAP_BYTES as usize + 1024);
        std::fs::write(dir.path().join(".credentials.json"), huge).unwrap();
        let result = lookup_file(&dir.path().join(".credentials.json"), now());
        assert!(
            matches!(result, Err(CredError::Unreadable(_))),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lookup_file_rejects_non_regular_file() {
        // A symlink to a character device (`/dev/null`) is a safe, instant
        // stand-in for the FIFO-blocks-forever scenario this guard exists
        // for: `metadata()` follows the symlink and must see "not a regular
        // file" before any read is attempted.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join(".credentials.json");
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        let result = lookup_file(&dir.path().join(".credentials.json"), now());
        assert!(
            matches!(result, Err(CredError::Unreadable(_))),
            "{result:?}"
        );
    }

    fn runtime_paths_in(dir: &Path) -> RuntimePaths {
        crate::orca::runtime::runtime_paths(Some(dir.to_str().unwrap()), dir, |p| p.exists())
    }

    fn user() -> KeychainUser {
        KeychainUser {
            acct: "example".into(),
            delete_accts: vec!["example".into()],
        }
    }

    /// Off macOS the runtime grant is `D/.credentials.json` only.
    #[test]
    fn lookup_runtime_reads_the_file_off_macos() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999}}"#,
        )
        .unwrap();
        let paths = runtime_paths_in(dir.path());
        let tok = lookup_runtime(HostOs::Linux, &paths, &user(), now()).unwrap();
        assert_eq!(tok.access_token, "tok_example");
    }

    /// Guard: on macOS the runtime lookup goes through the Keychain seam,
    /// which refuses to run anything without a fake `security` installed
    /// (never the real binary), and reads the fake's scoped item first.
    /// Unix only: the fake runs under `/usr/bin/perl`.
    #[cfg(unix)]
    #[test]
    fn lookup_runtime_uses_only_the_fake_security() {
        let dir = tempfile::tempdir().unwrap();
        let paths = runtime_paths_in(dir.path());
        let refused = lookup_runtime(HostOs::MacOs, &paths, &user(), now());
        assert!(
            matches!(&refused, Err(CredError::Unreadable(m)) if m.contains("cfg(test)")),
            "{refused:?}"
        );

        let fake = crate::orca::testsupport::FakeSecurity::install();
        let svc = keychain::runtime_service(Some(dir.path().to_str().unwrap()));
        fake.put(
            &svc,
            "example",
            br#"{"claudeAiOauth":{"accessToken":"tok_scoped","expiresAt":9999999999999}}"#,
        );
        let tok = lookup_runtime(HostOs::MacOs, &paths, &user(), now()).unwrap();
        assert_eq!(tok.access_token, "tok_scoped");
    }

    #[test]
    fn parse_errors_never_quote_the_text() {
        let err = parse_blob(
            r#"{"claudeAiOauth":{"accessToken":12345678,"expiresAt":"tok_leak"}}"#,
            now(),
        )
        .err()
        .unwrap();
        assert!(!format!("{err}").contains("tok_leak"), "{err}");
    }
}
