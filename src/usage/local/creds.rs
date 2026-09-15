//! Credential lookup for the local usage collector — the macOS Keychain
//! (mirroring Claude Code's own storage scheme exactly, since we read the
//! same entry it writes) on macOS, or `<config_dir>/.credentials.json`
//! everywhere else (and as the macOS fallback when the keychain entry is
//! missing).
//!
//! **This read path never performs a `refresh_token` grant.** Refreshing
//! rotates the token, and racing that rotation against Claude Code's own
//! refresh can log the user out from under them. An expired token surfaces
//! as [`CredError::Expired`]; the caller (`local::mod::collect`) falls back
//! to the last stored reading rather than trying to mint a new one — that is
//! still the default for every caller.
//!
//! The one exception lives in [`super::refresh`], not here: an explicitly
//! opted-in headless collector (`csm usage --refresh-oauth` /
//! `CSM_OAUTH_REFRESH=1`) may mint a new access token for a profile whose
//! access token has expired while its refresh token is still alive — and
//! only when no live Claude Code session exists for that profile, an
//! exclusive lock is held, and the platform stores credentials in the file
//! rather than the macOS Keychain. See that module's gate list. Nothing in
//! this module writes.
//!
//! The access token is never logged, printed, or embedded in an error string
//! anywhere in this module. [`OauthToken`] hand-writes its `Debug` impl to
//! redact it — never derive `Debug` on that struct.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

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

// ─── public entry-point ─────────────────────────────────────────────────────

/// Look up the live OAuth token for the profile whose Claude Code config
/// directory is `dir`.
///
/// macOS: tries the Keychain first (`security find-generic-password`,
/// service derived from `dir` — see [`service_name`]), falling back to
/// `<dir>/.credentials.json` on a keychain miss. Every other OS: reads
/// `<dir>/.credentials.json` only.
pub fn lookup(dir: &Path, now: DateTime<Utc>) -> Result<OauthToken, CredError> {
    #[cfg(target_os = "macos")]
    {
        match lookup_keychain(dir, now) {
            Ok(tok) => return Ok(tok),
            Err(CredError::NotFound) => {} // fall through to the file on disk
            Err(e) => return Err(e),
        }
    }
    lookup_file(dir, now)
}

/// Hard cap on `.credentials.json` reads — a real credentials blob is a few
/// KB; this matches the 256 KiB ceiling `csm usage capture`/`csm statusline`
/// already apply to their own external-input reads (`CAPTURE_STDIN_CAP_BYTES`
/// in `main.rs`/`statusline.rs`), so this is the one unbounded external-input
/// path left in the module otherwise.
const CREDENTIALS_FILE_CAP_BYTES: u64 = 256 * 1024;

fn lookup_file(dir: &Path, now: DateTime<Utc>) -> Result<OauthToken, CredError> {
    use std::io::Read;

    let path = dir.join(".credentials.json");
    let file = std::fs::File::open(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CredError::NotFound
        } else {
            CredError::Unreadable(format!("{}: {e}", path.display()))
        }
    })?;
    // Reject anything that isn't a regular file up front — a FIFO or a
    // device-node symlink here would otherwise let `read_to_end` block
    // forever (no writer) or stream unbounded data (`/dev/zero`) on
    // `collect()`'s critical launch path. `metadata()` follows symlinks, so
    // this also catches a symlink pointing at a non-regular target.
    let meta = file
        .metadata()
        .map_err(|e| CredError::Unreadable(format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(CredError::Unreadable(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    let mut buf = Vec::new();
    file.take(CREDENTIALS_FILE_CAP_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| CredError::Unreadable(format!("{}: {e}", path.display())))?;
    let text = String::from_utf8(buf)
        .map_err(|_| CredError::Unreadable(format!("{}: not valid UTF-8", path.display())))?;
    parse_blob(&text, now)
}

// ─── macOS Keychain path ────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn lookup_keychain(dir: &Path, now: DateTime<Utc>) -> Result<OauthToken, CredError> {
    let dir_str = dir.to_string_lossy().to_string();
    let account = account_name(&std::env::var("USER").unwrap_or_default());
    let svc = service_name(&dir_str);

    match run_security(&account, &svc) {
        Ok(text) => return parse_blob(&text, now),
        Err(CredError::NotFound) => {} // try the unsuffixed default-profile service below
        Err(e) => return Err(e),
    }

    // The unsuffixed service `"Claude Code-credentials"` is what a login with
    // CLAUDE_CONFIG_DIR unset writes to — only worth trying when `dir` IS that
    // default profile dir (`$HOME/.claude`); trying it for every other
    // profile would silently read the wrong account's token.
    if is_default_claude_dir(dir) {
        let text = run_security(&account, "Claude Code-credentials")?;
        return parse_blob(&text, now);
    }

    Err(CredError::NotFound)
}

#[cfg(target_os = "macos")]
fn is_default_claude_dir(dir: &Path) -> bool {
    match dirs::home_dir() {
        Some(home) => home.join(".claude") == dir,
        None => false,
    }
}

/// Run `security find-generic-password -a <account> -w -s <service>` with a
/// 5s hard deadline.
///
/// Mirrors `transport.rs::run_usage_command`'s spawn/dedicated-reader-thread/kill
/// pattern exactly: the reader thread owns the stdout pipe so the child never
/// blocks on a full pipe buffer while we poll `try_wait`, and on
/// timeout/wait-failure we detach (never join) the reader — a surviving
/// grandchild could otherwise keep the pipe open past the deadline.
#[cfg(target_os = "macos")]
fn run_security(account: &str, service: &str) -> Result<String, CredError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-a", account, "-w", "-s", service])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| CredError::Unreadable(format!("spawn `security` failed: {e}")))?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| CredError::Unreadable("security: stdout pipe missing".into()))?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut pipe = stdout_pipe;
        std::io::Read::read_to_end(&mut pipe, &mut buf).map(|_| buf)
    });

    let start = Instant::now();
    let deadline = Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap so we don't leave a zombie
                    drop(reader); // detach — see module note above
                    return Err(CredError::Unreadable("security: timed out after 5s".into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return Err(CredError::Unreadable(format!("security: wait failed: {e}")));
            }
        }
    };

    // errSecItemNotFound.
    if status.code() == Some(44) {
        return Err(CredError::NotFound);
    }
    if !status.success() {
        return Err(CredError::Unreadable(format!(
            "security exited with status {status}"
        )));
    }

    let bytes = match reader.join() {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(CredError::Unreadable(format!("security: read failed: {e}"))),
        Err(_) => {
            return Err(CredError::Unreadable(
                "security: reader thread panicked".into(),
            ))
        }
    };
    String::from_utf8(bytes)
        .map_err(|_| CredError::Unreadable("security: output is not valid UTF-8".into()))
}

// ─── pure helpers (unit-tested without touching the real keychain/disk) ────

/// Derive the macOS Keychain service name Claude Code itself uses for a
/// profile's config directory: `"Claude Code-credentials-" +
/// sha256(NFC(dir)).hex[..8]`.
///
/// `dir` must be the directory string exactly as Claude Code saw it (i.e.
/// `CLAUDE_CONFIG_DIR`'s value, or the resolved default) — NFC-normalizing
/// here only protects against a path containing decomposed Unicode; it does
/// not resolve symlinks or relative components, because neither does Claude
/// Code's own hasher.
///
/// Only called from `lookup_keychain` (macOS-only); kept `pub` and testable
/// on every platform so the fixed-sha256 vector below guards the derivation
/// even when CI isn't running on macOS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn service_name(dir: &str) -> String {
    let normalized: String = dir.nfc().collect();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("Claude Code-credentials-{}", &hex[..8])
}

/// Derive the macOS Keychain account name from `$USER`: the literal username
/// when it matches `^[a-zA-Z0-9._-]+$`, else Claude Code's own fallback for an
/// unusual username.
///
/// Only called from `lookup_keychain` (macOS-only); kept `pub` and testable
/// on every platform — see [`service_name`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn account_name(user: &str) -> String {
    let valid = !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        user.to_string()
    } else {
        "claude-code-user".to_string()
    }
}

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

    let blob: CredentialsBlob = serde_json::from_str(&json_text)
        .map_err(|e| CredError::Unreadable(format!("credentials JSON parse error: {e}")))?;
    let oauth = blob.claude_ai_oauth.ok_or(CredError::NotFound)?;
    let access_token = oauth.access_token.ok_or(CredError::NotFound)?;

    if let Some(expires_at_ms) = oauth.expires_at {
        if expires_at_ms <= now.timestamp_millis() {
            let refresh_alive = oauth
                .refresh_token_expires_at
                .is_some_and(|r| r > now.timestamp_millis());
            return Err(CredError::Expired {
                refresh_alive,
                expired_at_ms: expires_at_ms,
            });
        }
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

    // ── service_name — fixed sha256 vector ─────────────────────────────────
    //
    // sha256(NFC("/Users/example/.claude.work")) =
    //   a343d19d04e5a6a6544b23b71bcc92c53fe55a2214b794f87f31bdbc0aea9565
    // (computed independently, first 8 hex chars = "a343d19d"). A neutral
    // example path per the crate's leak-guard invariant — never a real home.

    #[test]
    fn service_name_matches_fixed_sha256_vector() {
        assert_eq!(
            service_name("/Users/example/.claude.work"),
            "Claude Code-credentials-a343d19d"
        );
    }

    #[test]
    fn service_name_is_stable_and_dir_sensitive() {
        let a = service_name("/Users/example/.claude.work");
        let b = service_name("/Users/example/.claude.home");
        assert_ne!(a, b, "different dirs must hash to different service names");
        assert_eq!(
            a,
            service_name("/Users/example/.claude.work"),
            "deterministic"
        );
    }

    #[test]
    fn service_name_nfc_normalizes_before_hashing() {
        // "é" as a precomposed codepoint (U+00E9) vs. "e" + combining acute
        // (U+0065 U+0301) must hash identically once both are NFC-normalized.
        let precomposed = "/Users/example/.claude.caf\u{00e9}";
        let decomposed = "/Users/example/.claude.cafe\u{0301}";
        assert_eq!(service_name(precomposed), service_name(decomposed));
    }

    // ── account_name ─────────────────────────────────────────────────────────

    #[test]
    fn account_name_rules() {
        assert_eq!(account_name("example"), "example");
        assert_eq!(account_name("example.user-1_x"), "example.user-1_x");
        assert_eq!(account_name(""), "claude-code-user");
        assert_eq!(account_name("has space"), "claude-code-user");
        assert_eq!(account_name("has/slash"), "claude-code-user");
    }

    // ── parse_blob ────────────────────────────────────────────────────────────

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

    // ── lookup_file (works on every OS; the keychain path is macOS-only) ──────

    #[test]
    fn lookup_file_reads_credentials_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok_example","expiresAt":9999999999999}}"#,
        )
        .unwrap();
        let tok = lookup_file(dir.path(), now()).expect("should read the file");
        assert_eq!(tok.access_token, "tok_example");
    }

    #[test]
    fn lookup_file_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = lookup_file(dir.path(), now());
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
        let result = lookup_file(dir.path(), now());
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
        let result = lookup_file(dir.path(), now());
        assert!(
            matches!(result, Err(CredError::Unreadable(_))),
            "{result:?}"
        );
    }
}
