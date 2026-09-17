//! Opt-in OAuth access-token refresh for **headless collectors** — the one
//! path in this crate that writes a profile's credential file.
//!
//! # Why this exists
//!
//! [`super::creds`] is deliberately read-only: an access token lives ~8h and
//! only a running Claude Code process refreshes it, so racing that rotation
//! from the collector can log the user out. That stance is still the DEFAULT
//! and nothing here runs unless it is explicitly asked for.
//!
//! It breaks down in exactly one deployment: a headless host that collects
//! usage for profiles no Claude Code ever runs under (a container that only
//! runs `csm usage`). There, 8h after login every profile flips to
//! NeedsRefresh and collection stops for good — nothing on that box will ever
//! mint a new access token. This module is the narrow, gated escape hatch for
//! that case.
//!
//! # Gates — every one of them must hold before a single byte is sent
//!
//! 1. **Opt-in** — `csm usage --refresh-oauth`, or `CSM_OAUTH_REFRESH=1`.
//!    Threaded as an explicit parameter from `main::cmd_usage` all the way to
//!    [`super::probe`]; every other caller (statusline, picker, sidecar,
//!    hook) passes `false` and is behaviorally unchanged.
//! 2. **State** — only [`CredState::Refreshable`] (access token expired AND
//!    `refreshTokenExpiresAt` still in the future). A live access token is
//!    never rotated; a dead refresh token is never spent. This is checked
//!    before the platform gate on purpose: it keeps the loud macOS refusal
//!    scoped to the profiles that would actually have been refreshed instead
//!    of firing for every profile of every run.
//! 3. **Platform** — the credential file is only the live copy on
//!    Linux/Windows. On macOS the live copy is the login Keychain (which
//!    `creds` reads first), so writing the file there would be ignored at
//!    best and desynchronizing at worst: macOS returns
//!    [`RefreshOutcome::Unsupported`] and the file is never written.
//! 4. **No live session** — `<profile_dir>/sessions/*.json` is Claude Code's
//!    own session registry; if any recorded pid is a live claude/node
//!    process, Claude Code itself will refresh and we stand down.
//! 5. **Single writer** — an `O_EXCL` lock file next to the credentials,
//!    with a 60s staleness takeover, released by a [`Drop`] guard.
//!
//! # Secrecy
//!
//! No token — access or refresh — is ever printed, logged, or placed in an
//! error string. Response bodies are never echoed: a non-2xx failure carries
//! the status plus, at most, the short `error` code (`invalid_grant`), and
//! only after it passes a strict character/length filter. [`TokenResponse`]
//! hand-writes its `Debug` impl to redact both tokens — never derive it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::api;

// ─── constants ──────────────────────────────────────────────────────────────

/// Claude Code's own token endpoint. Overridable by `CSM_OAUTH_TOKEN_URL`
/// (tests only — see [`resolve_token_url`]).
pub const DEFAULT_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Claude Code's public OAuth client id — a fixed, non-secret identifier the
/// token endpoint requires; it is not a credential.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// A lock file older than this is treated as abandoned by a crashed process.
const LOCK_STALE_SECS: i64 = 60;

/// Same bounded-read ceiling `creds::lookup_file` applies to a credentials
/// blob (a real one is a few KB).
const CREDENTIALS_FILE_CAP_BYTES: u64 = 256 * 1024;

/// Session-registry entries are tiny JSON objects; cap the read anyway so a
/// hostile/broken file in that directory can't stream unbounded data.
const SESSION_FILE_CAP_BYTES: u64 = 64 * 1024;

// ─── gate types (pure) ──────────────────────────────────────────────────────

/// What [`super::creds::lookup`] just said about this profile, reduced to the
/// only distinction refreshing cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredState {
    /// Access token still valid — nothing to do (and rotating it would be a
    /// gratuitous race).
    Live,
    /// Access token expired, refresh token still alive — the only refreshable
    /// state.
    Refreshable,
    /// Access token expired and the refresh token is dead/absent — only
    /// `claude auth login` recovers this.
    RefreshDead,
    /// No credentials at all, or unreadable — nothing to refresh from.
    Unusable,
}

/// Why a refresh was not attempted. Every variant is a short, fixed string —
/// never derived from a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NotOptedIn,
    UnsupportedPlatform,
    TokenStillValid,
    RefreshTokenDead,
    NoRefreshableCredentials,
    LiveSession,
    LockHeld,
}

impl SkipReason {
    /// The stderr wording for this skip.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NotOptedIn => "not opted in",
            SkipReason::UnsupportedPlatform => {
                "unsupported on macOS (credentials live in the Keychain)"
            }
            SkipReason::TokenStillValid => "access token still valid",
            SkipReason::RefreshTokenDead => "refresh token dead",
            SkipReason::NoRefreshableCredentials => "no refreshable credentials",
            SkipReason::LiveSession => "live session present",
            SkipReason::LockHeld => "lock held",
        }
    }

    /// Whether this skip is worth a stderr line. "Not opted in" and "token
    /// still valid" are the overwhelmingly common cases and must stay silent
    /// (they would fire on every profile of every run); a dead refresh token
    /// is already reported loudly as NeedsLogin by `local::resolve`.
    ///
    /// The three loud ones are all reachable only *after* the credential
    /// state matched [`CredState::Refreshable`] (see [`gate`]'s order), so
    /// each of them speaks about a profile that genuinely wanted a refresh
    /// and did not get one — one line per such profile, not per profile.
    pub fn is_diagnostic(self) -> bool {
        matches!(
            self,
            SkipReason::UnsupportedPlatform | SkipReason::LiveSession | SkipReason::LockHeld
        )
    }
}

/// The pure gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Attempt,
    Skip(SkipReason),
}

/// Everything the gate decides from — no clock, no I/O. The I/O shell
/// ([`maybe_refresh`]) fills these in progressively, cheapest first, so a
/// profile that fails an early gate never pays for the session scan or the
/// lock.
#[derive(Debug, Clone, Copy)]
pub struct GateInputs {
    pub opt_in: bool,
    pub platform_supported: bool,
    pub creds: CredState,
    pub live_session: bool,
    pub lock_held: bool,
}

/// The whole gate, in priority order.
///
/// The credential state is examined before the platform, even though the
/// platform check is the cheaper of the two: [`SkipReason::UnsupportedPlatform`]
/// is a loud skip, and ordering it after the state match keeps it scoped to
/// the profiles that would really have been refreshed — otherwise every macOS
/// run with the opt-in on would print one line per profile per tick,
/// including profiles whose access token is perfectly valid.
pub fn gate(i: GateInputs) -> GateDecision {
    if !i.opt_in {
        return GateDecision::Skip(SkipReason::NotOptedIn);
    }
    match i.creds {
        CredState::Live => return GateDecision::Skip(SkipReason::TokenStillValid),
        CredState::RefreshDead => return GateDecision::Skip(SkipReason::RefreshTokenDead),
        CredState::Unusable => return GateDecision::Skip(SkipReason::NoRefreshableCredentials),
        CredState::Refreshable => {}
    }
    if !i.platform_supported {
        return GateDecision::Skip(SkipReason::UnsupportedPlatform);
    }
    if i.live_session {
        return GateDecision::Skip(SkipReason::LiveSession);
    }
    if i.lock_held {
        return GateDecision::Skip(SkipReason::LockHeld);
    }
    GateDecision::Attempt
}

/// `true` on every platform where `<profile_dir>/.credentials.json` is the
/// live credential copy — i.e. everywhere except macOS (Keychain).
fn platform_supported() -> bool {
    !cfg!(target_os = "macos")
}

// ─── outcome ────────────────────────────────────────────────────────────────

/// What one refresh attempt did. Failures and skips are informational: the
/// caller leaves the profile exactly as today's read-only path would.
#[derive(Debug)]
pub enum RefreshOutcome {
    /// A new access token was written; it is good for `expires_in_secs`.
    Refreshed { expires_in_secs: i64 },
    /// A gate said no.
    Skipped(SkipReason),
    /// macOS — the credential file is not the live copy here.
    Unsupported,
    /// The attempt ran and failed. The message is a short diagnostic
    /// (status + sanitized error code, or a local I/O reason) — never a token
    /// and never a response body.
    Failed(String),
}

// ─── opt-in ─────────────────────────────────────────────────────────────────

/// Env-side opt-in: `CSM_OAUTH_REFRESH=1` (also accepts `true`/`yes`,
/// case-insensitive). The `--refresh-oauth` flag ORs with this in
/// `main::cmd_usage`; nothing else reads it, so no other code path can be
/// switched on by the environment alone.
pub fn opt_in_from_env() -> bool {
    matches!(
        std::env::var("CSM_OAUTH_REFRESH")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

// ─── the I/O shell ──────────────────────────────────────────────────────────

/// Refresh this profile's access token if — and only if — every gate holds.
///
/// Called unconditionally from [`super::probe`]; with `opt_in == false` it
/// returns on the first gate having touched nothing. Reporting belongs to the
/// caller: this returns the outcome and prints nothing itself (the sole
/// exception is [`resolve_token_url`]'s override warning, which has to fire
/// where the redirected endpoint is actually used).
pub fn maybe_refresh(
    dir: &Path,
    now: DateTime<Utc>,
    opt_in: bool,
    creds: CredState,
) -> RefreshOutcome {
    maybe_refresh_on(dir, now, opt_in, creds, platform_supported())
}

/// [`maybe_refresh`] with the platform verdict injected, so the whole
/// gate → lock → read/POST/merge/write assembly is exercisable by tests on
/// every host (macOS included, where the real verdict short-circuits it).
fn maybe_refresh_on(
    dir: &Path,
    now: DateTime<Utc>,
    opt_in: bool,
    creds: CredState,
    platform_supported: bool,
) -> RefreshOutcome {
    let inputs = GateInputs {
        opt_in,
        platform_supported,
        creds,
        live_session: false,
        lock_held: false,
    };

    // Cheap gates first — nothing below this point runs for a profile that
    // isn't opted in / isn't refreshable / is on macOS.
    if let GateDecision::Skip(reason) = gate(inputs) {
        return skipped(reason);
    }

    // Gate 4 — Claude Code's own session registry.
    let live = has_live_session(&dir.join("sessions"), |pid| {
        use crate::platform::proc_check::ProcCheck;
        crate::platform::PlatformProcCheck::is_live_claude_or_node(pid)
    });
    if let GateDecision::Skip(reason) = gate(GateInputs {
        live_session: live,
        ..inputs
    }) {
        return skipped(reason);
    }

    // Gate 5 — single writer. The guard removes the lock on every exit path.
    let lock_path = dir.join(".credentials.json.csm-refresh.lock");
    let Some(_lock) = acquire_lock(&lock_path, now.timestamp()) else {
        return skipped(SkipReason::LockHeld);
    };

    match do_refresh(dir, now) {
        Ok(expires_in_secs) => RefreshOutcome::Refreshed { expires_in_secs },
        Err(e) => RefreshOutcome::Failed(e.to_string()),
    }
}

/// Map a gate refusal to an outcome — macOS gets its own variant so the
/// caller can word that one as a platform limit rather than a skip.
fn skipped(reason: SkipReason) -> RefreshOutcome {
    if reason == SkipReason::UnsupportedPlatform {
        return RefreshOutcome::Unsupported;
    }
    RefreshOutcome::Skipped(reason)
}

/// Read → request → merge → atomic write. Every failure is "no change": the
/// credential file is only ever replaced by a fully-formed merged blob.
fn do_refresh(dir: &Path, now: DateTime<Utc>) -> Result<i64, RefreshError> {
    let path = dir.join(".credentials.json");
    let text = read_capped(&path, CREDENTIALS_FILE_CAP_BYTES)
        .map_err(|e| RefreshError::Local(format!("credentials unreadable: {e}")))?;
    let existing: Value = serde_json::from_str(&text)
        .map_err(|_| RefreshError::Local("credentials JSON parse error".into()))?;
    let oauth = existing
        .get("claudeAiOauth")
        .ok_or_else(|| RefreshError::Local("no claudeAiOauth object".into()))?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RefreshError::Local("no stored refresh token".into()))?;
    let scope = stored_scope(oauth);

    let resp = request_refresh(&resolve_token_url(), refresh_token, scope.as_deref())?;

    let merged = merge_refreshed(&existing, &resp, now.timestamp_millis());
    write_credentials_atomic(&path, &merged)
        .map_err(|e| RefreshError::Local(format!("credentials write failed: {e}")))?;
    Ok(resp.expires_in)
}

// ─── HTTP ───────────────────────────────────────────────────────────────────

/// Failure modes of one refresh attempt. Every `Display` form is a short
/// diagnostic — no token, no response body.
#[derive(Debug, thiserror::Error)]
enum RefreshError {
    #[error("unsafe token URL '{0}' — must be https:// or a loopback host")]
    UnsafeUrl(String),
    /// `"400 invalid_grant"`, or just `"500"` — see [`error_code_from_body`].
    #[error("refresh HTTP {0}")]
    Http(String),
    #[error("refresh network error: {0}")]
    Network(String),
    #[error("refresh response parse error")]
    Parse,
    #[error("{0}")]
    Local(String),
}

/// The 200 body. Hand-written `Debug` (below) redacts both tokens — never
/// derive it.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field("refresh_token_expires_in", &self.refresh_token_expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Resolve the token endpoint. An override is loud on stderr for the same
/// reason `CSM_USAGE_API_BASE`'s is: the profile's refresh token — a
/// longer-lived credential than the access token — is about to be POSTed
/// there.
fn resolve_token_url() -> String {
    match std::env::var("CSM_OAUTH_TOKEN_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(url) => {
            eprintln!(
                "csm: warning: CSM_OAUTH_TOKEN_URL overrides the OAuth token endpoint to \
                 {url} — this profile's refresh token will be sent there; verify this \
                 host is trusted"
            );
            url
        }
        None => DEFAULT_TOKEN_URL.to_string(),
    }
}

/// POST the refresh grant. Timeouts mirror `api::fetch_usage`'s shape (3s
/// connect) with a longer 30s ceiling — a token mint is a write the endpoint
/// may take longer to settle, and unlike the usage probe there is no stale
/// value to fall back to if we give up early.
fn request_refresh(
    url: &str,
    refresh_token: &str,
    scope: Option<&str>,
) -> Result<TokenResponse, RefreshError> {
    use std::time::Duration;

    api::validate_base(url).map_err(|_| RefreshError::UnsafeUrl(url.to_string()))?;

    let mut body = Map::new();
    body.insert("grant_type".into(), json!("refresh_token"));
    body.insert("refresh_token".into(), json!(refresh_token));
    body.insert("client_id".into(), json!(CLIENT_ID));
    if let Some(scope) = scope {
        body.insert("scope".into(), json!(scope));
    }

    let client = api::http_client(Duration::from_secs(30))
        .map_err(|e| RefreshError::Network(e.to_string()))?;

    let resp = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            concat!("csm/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(serde_json::Value::Object(body).to_string())
        .send()
        .map_err(|e| RefreshError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    let text = resp
        .text()
        .map_err(|e| RefreshError::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(RefreshError::Http(match error_code_from_body(&text) {
            Some(code) => format!("{status} {code}"),
            None => status.to_string(),
        }));
    }
    serde_json::from_str(&text).map_err(|_| RefreshError::Parse)
}

/// Pull `error` out of an RFC-6749 error body, but only when it looks like a
/// bare code (`invalid_grant`): short, and made of the characters an OAuth
/// error code is allowed to use. Anything else is dropped, so no server-chosen
/// prose ever reaches a log line.
fn error_code_from_body(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let code = v.get("error")?.as_str()?;
    let ok = !code.is_empty()
        && code.len() <= 40
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| code.to_string())
}

// ─── pure helpers ───────────────────────────────────────────────────────────

/// The stored `scopes` array joined with single spaces, or `None` when absent
/// or empty (the protocol omits `scope` entirely in that case).
fn stored_scope(oauth: &Value) -> Option<String> {
    let joined = oauth
        .get("scopes")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Merge a 200 response into the existing credentials blob, exactly as Claude
/// Code does:
///
/// - `accessToken` ← `access_token`
/// - `refreshToken` ← `refresh_token` when present, else the stored one
/// - `expiresAt` ← `now_ms + expires_in * 1000`
/// - `refreshTokenExpiresAt` ← `now_ms + refresh_token_expires_in * 1000`
///   when present, else the stored one
/// - `scopes` ← `scope` split on `' '` when present, else the stored ones
///
/// Every other key of `claudeAiOauth` (`subscriptionType`, `rateLimitTier`,
/// `clientId`, `tokenAccount`, …) and every other top-level key (`mcpOAuth`,
/// …) is carried through verbatim.
fn merge_refreshed(existing: &Value, resp: &TokenResponse, now_ms: i64) -> Value {
    let mut root = match existing {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let mut oauth = match root.get("claudeAiOauth") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };

    oauth.insert("accessToken".into(), json!(resp.access_token));
    if let Some(rt) = &resp.refresh_token {
        oauth.insert("refreshToken".into(), json!(rt));
    }
    oauth.insert(
        "expiresAt".into(),
        json!(now_ms + resp.expires_in.saturating_mul(1000)),
    );
    if let Some(secs) = resp.refresh_token_expires_in {
        oauth.insert(
            "refreshTokenExpiresAt".into(),
            json!(now_ms + secs.saturating_mul(1000)),
        );
    }
    if let Some(scope) = resp.scope.as_deref() {
        let scopes: Vec<&str> = scope.split(' ').filter(|s| !s.is_empty()).collect();
        // An empty/whitespace `scope` is treated as absent rather than as
        // "the account now has no scopes" — clearing them would be a
        // destructive read of an ambiguous field.
        if !scopes.is_empty() {
            oauth.insert("scopes".into(), json!(scopes));
        }
    }

    root.insert("claudeAiOauth".into(), Value::Object(oauth));
    Value::Object(root)
}

/// Does `sessions_dir` (Claude Code's own `<profile_dir>/sessions/`) hold a
/// registry entry whose pid is a live claude/node process?
///
/// `is_live` is injected so the whole scan is unit-testable over a temp dir.
/// A missing directory, a non-`.json` name, and an entry with no usable pid
/// all count as "no live session" — the conservative direction is the other
/// one (an unparseable file whose *name* is a numeric pid still gets probed,
/// so a half-written registry entry can only ever suppress a refresh, never
/// license one).
fn has_live_session(sessions_dir: &Path, mut is_live: impl FnMut(u32) -> bool) -> bool {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = pid_from_session_file(&path) else {
            continue;
        };
        if is_live(pid) {
            return true;
        }
    }
    false
}

/// The pid a session-registry entry refers to: its `pid` field, falling back
/// to the numeric file stem (`<pid>.json`) when the body is missing the field
/// or doesn't parse.
fn pid_from_session_file(path: &Path) -> Option<u32> {
    if let Ok(text) = read_capped(path, SESSION_FILE_CAP_BYTES)
        && let Ok(v) = serde_json::from_str::<Value>(&text)
        && let Some(pid) = v.get("pid").and_then(|p| p.as_u64())
    {
        return u32::try_from(pid).ok();
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
}

// ─── lock ───────────────────────────────────────────────────────────────────

/// Holds `<profile_dir>/.credentials.json.csm-refresh.lock` for the duration
/// of one attempt; [`Drop`] removes it on every exit path (including a panic
/// unwinding through the caller).
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `O_EXCL`-create the lock, taking over one that is older than
/// [`LOCK_STALE_SECS`] (a crashed writer). `now_epoch` is injected so the
/// takeover is testable without sleeping. A lock whose age can't be read is
/// treated as held — the safe direction.
fn acquire_lock(path: &Path, now_epoch: i64) -> Option<LockGuard> {
    match create_lock(path) {
        Ok(guard) => Some(guard),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let stale = lock_age_secs(path, now_epoch).is_some_and(|age| age > LOCK_STALE_SECS);
            if !stale {
                return None;
            }
            let _ = std::fs::remove_file(path);
            create_lock(path).ok()
        }
        Err(_) => None,
    }
}

fn create_lock(path: &Path) -> std::io::Result<LockGuard> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    // Best-effort provenance for a human staring at a stuck lock.
    let _ = writeln!(file, "{}", std::process::id());
    Ok(LockGuard {
        path: path.to_path_buf(),
    })
}

fn lock_age_secs(path: &Path, now_epoch: i64) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = crate::epoch::from_systemtime(modified);
    Some(now_epoch - i64::try_from(secs).ok()?)
}

// ─── file I/O ───────────────────────────────────────────────────────────────

/// Bounded read of a regular file (the same shape `creds::lookup_file` uses:
/// a FIFO or device node here would otherwise block forever or stream
/// unbounded data).
fn read_capped(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf)?;
    String::from_utf8(buf)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8"))
}

/// Write `value` over `path` atomically: a `0600` temp file next to it,
/// fsynced, then renamed. The temp file is removed on any failure, so a
/// partial write can never be observed as the credential file and the
/// original is left untouched.
fn write_credentials_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".credentials.json");
    let tmp = parent.join(format!("{base}.tmp-{}", std::process::id()));

    let result = write_tmp_then_rename(&tmp, path, value);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_tmp_then_rename(tmp: &Path, path: &Path, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

    // `.mode()` only applies at creation — re-assert 0600 in case the temp
    // file survived a crashed earlier run with a laxer mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(tmp, path)
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Sets `CSM_OAUTH_TOKEN_URL` for as long as it is alive, then restores
    /// whatever was there before. Caller must hold
    /// `crate::testenv::lock_for("CSM_OAUTH_TOKEN_URL")` for its whole
    /// lifetime.
    struct TokenUrlEnv {
        saved: Option<String>,
    }

    impl TokenUrlEnv {
        fn set(url: &str) -> Self {
            let saved = std::env::var("CSM_OAUTH_TOKEN_URL").ok();
            crate::testenv::set_var("CSM_OAUTH_TOKEN_URL", url);
            Self { saved }
        }
    }

    impl Drop for TokenUrlEnv {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => crate::testenv::set_var("CSM_OAUTH_TOKEN_URL", &v),
                None => crate::testenv::remove_var("CSM_OAUTH_TOKEN_URL"),
            }
        }
    }

    /// Every fixture token below is a made-up placeholder string, never a
    /// real-shaped credential.
    fn resp(
        access: &str,
        refresh: Option<&str>,
        expires_in: i64,
        refresh_expires_in: Option<i64>,
        scope: Option<&str>,
    ) -> TokenResponse {
        TokenResponse {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_in,
            refresh_token_expires_in: refresh_expires_in,
            scope: scope.map(str::to_string),
        }
    }

    fn existing_blob() -> Value {
        json!({
            "claudeAiOauth": {
                "accessToken": "tok_example_old",
                "refreshToken": "rtok_example_old",
                "expiresAt": 1_000_000_000_000i64,
                "refreshTokenExpiresAt": 2_000_000_000_000i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "pro",
                "rateLimitTier": "default",
                "clientId": "client-example",
                "tokenAccount": {"nested": true}
            },
            "mcpOAuth": {"some-server": {"token": "tok_example_mcp"}},
            "otherTopLevel": 42
        })
    }

    fn now_ms() -> i64 {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    // ── (a) blob merge ──────────────────────────────────────────────────────

    #[test]
    fn merge_sets_access_token_and_expiry() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 28_800, None, None),
            now_ms(),
        );
        let oauth = &merged["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], json!("tok_example_new"));
        assert_eq!(oauth["expiresAt"], json!(now_ms() + 28_800 * 1000));
    }

    #[test]
    fn merge_keeps_old_refresh_token_when_response_omits_it() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, None, None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["refreshToken"],
            json!("rtok_example_old")
        );
    }

    #[test]
    fn merge_rotates_refresh_token_when_response_carries_one() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", Some("rtok_example_new"), 100, None, None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["refreshToken"],
            json!("rtok_example_new")
        );
    }

    #[test]
    fn merge_keeps_old_refresh_expiry_when_response_omits_it() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, None, None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["refreshTokenExpiresAt"],
            json!(2_000_000_000_000i64)
        );
    }

    #[test]
    fn merge_sets_refresh_expiry_when_response_carries_it() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, Some(7_776_000), None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["refreshTokenExpiresAt"],
            json!(now_ms() + 7_776_000 * 1000)
        );
    }

    #[test]
    fn merge_keeps_old_scopes_when_response_omits_scope() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, None, None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["scopes"],
            json!(["user:inference", "user:profile"])
        );
    }

    #[test]
    fn merge_splits_scope_on_single_spaces() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp(
                "tok_example_new",
                None,
                100,
                None,
                Some("a:one b:two c:three"),
            ),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["scopes"],
            json!(["a:one", "b:two", "c:three"])
        );
    }

    #[test]
    fn merge_treats_blank_scope_as_absent() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, None, Some("   ")),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["scopes"],
            json!(["user:inference", "user:profile"]),
            "a blank scope must never clear the stored scopes"
        );
    }

    #[test]
    fn merge_preserves_unknown_and_non_oauth_keys() {
        let merged = merge_refreshed(
            &existing_blob(),
            &resp("tok_example_new", None, 100, None, None),
            now_ms(),
        );
        let oauth = &merged["claudeAiOauth"];
        assert_eq!(oauth["subscriptionType"], json!("pro"));
        assert_eq!(oauth["rateLimitTier"], json!("default"));
        assert_eq!(oauth["clientId"], json!("client-example"));
        assert_eq!(oauth["tokenAccount"], json!({"nested": true}));
        assert_eq!(
            merged["mcpOAuth"],
            json!({"some-server": {"token": "tok_example_mcp"}})
        );
        assert_eq!(merged["otherTopLevel"], json!(42));
    }

    #[test]
    fn merge_creates_the_oauth_object_when_the_blob_has_none() {
        let merged = merge_refreshed(
            &json!({"mcpOAuth": {}}),
            &resp("tok_example_new", Some("rtok_example_new"), 100, None, None),
            now_ms(),
        );
        assert_eq!(
            merged["claudeAiOauth"]["accessToken"],
            json!("tok_example_new")
        );
        assert_eq!(merged["mcpOAuth"], json!({}));
    }

    #[test]
    fn debug_impl_redacts_both_tokens() {
        let rendered = format!(
            "{:?}",
            resp(
                "tok_example_secret",
                Some("rtok_example_secret"),
                1,
                None,
                None
            )
        );
        assert!(!rendered.contains("tok_example_secret"), "{rendered}");
        assert!(rendered.contains("redacted"));
    }

    // ── stored scope ────────────────────────────────────────────────────────

    #[test]
    fn stored_scope_joins_with_single_spaces() {
        let oauth = json!({"scopes": ["a:one", "b:two"]});
        assert_eq!(stored_scope(&oauth).as_deref(), Some("a:one b:two"));
    }

    #[test]
    fn stored_scope_absent_or_empty_is_none() {
        assert!(stored_scope(&json!({})).is_none());
        assert!(stored_scope(&json!({"scopes": []})).is_none());
        assert!(stored_scope(&json!({"scopes": [""]})).is_none());
    }

    // ── (b) gate decision matrix ────────────────────────────────────────────

    fn inputs() -> GateInputs {
        GateInputs {
            opt_in: true,
            platform_supported: true,
            creds: CredState::Refreshable,
            live_session: false,
            lock_held: false,
        }
    }

    #[test]
    fn gate_attempts_only_when_every_condition_holds() {
        assert_eq!(gate(inputs()), GateDecision::Attempt);
    }

    #[test]
    fn gate_skips_when_not_opted_in() {
        assert_eq!(
            gate(GateInputs {
                opt_in: false,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::NotOptedIn)
        );
    }

    #[test]
    fn gate_skips_on_unsupported_platform() {
        // The macOS case: the Keychain, not the file, is the live copy.
        assert_eq!(
            gate(GateInputs {
                platform_supported: false,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::UnsupportedPlatform)
        );
    }

    #[test]
    fn gate_skips_a_still_valid_token() {
        assert_eq!(
            gate(GateInputs {
                creds: CredState::Live,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::TokenStillValid)
        );
    }

    #[test]
    fn gate_skips_a_dead_refresh_token() {
        assert_eq!(
            gate(GateInputs {
                creds: CredState::RefreshDead,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::RefreshTokenDead)
        );
        assert_eq!(
            gate(GateInputs {
                creds: CredState::Unusable,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::NoRefreshableCredentials)
        );
    }

    #[test]
    fn gate_skips_when_a_live_session_exists() {
        assert_eq!(
            gate(GateInputs {
                live_session: true,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::LiveSession)
        );
    }

    #[test]
    fn gate_skips_when_the_lock_is_held() {
        assert_eq!(
            gate(GateInputs {
                lock_held: true,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::LockHeld)
        );
    }

    #[test]
    fn gate_priority_is_cheapest_first() {
        // Opt-in loses to nothing: with every other gate also failing, the
        // reported reason is still "not opted in" — the shell must be able to
        // return before any I/O.
        assert_eq!(
            gate(GateInputs {
                opt_in: false,
                platform_supported: false,
                creds: CredState::Live,
                live_session: true,
                lock_held: true,
            }),
            GateDecision::Skip(SkipReason::NotOptedIn)
        );
    }

    #[test]
    fn gate_reports_a_valid_token_before_the_platform_on_macos() {
        // The loud UnsupportedPlatform skip must stay scoped to profiles that
        // really wanted a refresh: an opted-in macOS run with a still-valid
        // token reports the silent TokenStillValid, not one stderr line per
        // profile per tick.
        assert_eq!(
            gate(GateInputs {
                platform_supported: false,
                creds: CredState::Live,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::TokenStillValid)
        );
        assert_eq!(
            gate(GateInputs {
                platform_supported: false,
                creds: CredState::RefreshDead,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::RefreshTokenDead)
        );
        // …but a refreshable profile on macOS still says so, loudly.
        assert_eq!(
            gate(GateInputs {
                platform_supported: false,
                ..inputs()
            }),
            GateDecision::Skip(SkipReason::UnsupportedPlatform)
        );
    }

    #[test]
    fn only_the_actionable_skips_are_diagnostics() {
        assert!(!SkipReason::NotOptedIn.is_diagnostic());
        assert!(!SkipReason::TokenStillValid.is_diagnostic());
        assert!(!SkipReason::RefreshTokenDead.is_diagnostic());
        assert!(SkipReason::LiveSession.is_diagnostic());
        assert!(SkipReason::LockHeld.is_diagnostic());
    }

    // ── (c) session-registry scan ───────────────────────────────────────────

    #[test]
    fn scan_finds_a_live_pid_from_the_json_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("4242.json"), r#"{"pid":4242}"#).unwrap();
        assert!(has_live_session(dir.path(), |pid| pid == 4242));
    }

    #[test]
    fn scan_reports_no_live_session_when_every_pid_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("11.json"), r#"{"pid":11}"#).unwrap();
        std::fs::write(dir.path().join("12.json"), r#"{"pid":12}"#).unwrap();
        assert!(!has_live_session(dir.path(), |_| false));
    }

    #[test]
    fn scan_ignores_non_json_and_unusable_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Not a .json file at all.
        std::fs::write(dir.path().join("notes.txt"), "1").unwrap();
        // Non-numeric name AND no usable pid in the body.
        std::fs::write(dir.path().join("scratch.json"), r#"{"no":"pid"}"#).unwrap();
        // Unparseable body, non-numeric name — nothing to fall back to.
        std::fs::write(dir.path().join("broken.json"), "{not json").unwrap();
        let mut seen: Vec<u32> = Vec::new();
        let live = has_live_session(dir.path(), |pid| {
            seen.push(pid);
            true // even a permissive closure must never be called here
        });
        assert!(!live);
        assert!(seen.is_empty(), "no pid should have been probed: {seen:?}");
    }

    #[test]
    fn scan_falls_back_to_the_numeric_file_stem_when_the_body_is_unparseable() {
        // Conservative direction: a half-written registry entry can suppress
        // a refresh, never license one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("777.json"), "{ truncated").unwrap();
        assert!(has_live_session(dir.path(), |pid| pid == 777));
    }

    #[test]
    fn scan_of_a_missing_directory_is_no_live_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_live_session(&dir.path().join("sessions"), |_| true));
    }

    // ── (d) atomic write ────────────────────────────────────────────────────

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"tok_example_old"}}"#,
        )
        .unwrap();

        let merged = merge_refreshed(
            &json!({"claudeAiOauth": {"accessToken": "tok_example_old"}}),
            &resp("tok_example_new", None, 100, None, None),
            now_ms(),
        );
        write_credentials_atomic(&path, &merged).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            back["claudeAiOauth"]["accessToken"],
            json!("tok_example_new")
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_result_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        write_credentials_atomic(&path, &json!({"claudeAiOauth": {}})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_failure_leaves_the_original_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let original = r#"{"claudeAiOauth":{"accessToken":"tok_example_old"}}"#;
        std::fs::write(&path, original).unwrap();

        // Read-only directory → the temp file can't be created at all.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_credentials_atomic(&path, &json!({"claudeAiOauth": {"a": 1}}));
        let restored = std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700));

        if result.is_ok() {
            // Running as root (or a filesystem that ignores the mode) — the
            // premise of the test doesn't hold, so assert nothing.
            restored.unwrap();
            return;
        }
        restored.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    // ── (f) lock guard ──────────────────────────────────────────────────────

    /// The lock file's real mtime, as an epoch — the reference point the
    /// injected clock has to be expressed against (`acquire_lock` compares
    /// `now_epoch` to that mtime).
    fn lock_mtime_epoch(path: &Path) -> i64 {
        lock_age_secs(path, 0).map(|age| -age).expect("lock mtime")
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json.csm-refresh.lock");

        let guard = acquire_lock(&path, 0).expect("first acquisition must win");
        assert!(path.exists());
        let now = lock_mtime_epoch(&path); // still "just written"
        assert!(
            acquire_lock(&path, now).is_none(),
            "a fresh lock must not be taken over"
        );
        drop(guard);
        assert!(!path.exists(), "the guard must remove the lock on drop");
        assert!(acquire_lock(&path, now).is_some(), "released lock reusable");
    }

    #[test]
    fn stale_lock_is_taken_over_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json.csm-refresh.lock");

        let guard = acquire_lock(&path, 0).expect("first acquisition");
        std::mem::forget(guard); // simulate a crashed writer: lock left behind

        let written_at = lock_mtime_epoch(&path);
        // Exactly at the grace period → still held.
        assert!(acquire_lock(&path, written_at + LOCK_STALE_SECS).is_none());
        // Past it → taken over.
        let taken = acquire_lock(&path, written_at + LOCK_STALE_SECS + 1);
        assert!(taken.is_some(), "a stale lock must be taken over");
        drop(taken);
        assert!(!path.exists());
    }

    // ── error-code sanitizing ───────────────────────────────────────────────

    #[test]
    fn error_code_is_extracted_only_when_it_looks_like_a_code() {
        assert_eq!(
            error_code_from_body(r#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        // Prose, whitespace, punctuation, or an over-long value is dropped —
        // no server-chosen text ever reaches a log line.
        assert!(error_code_from_body(r#"{"error":"bad token: tok_example"}"#).is_none());
        assert!(error_code_from_body(r#"{"error":""}"#).is_none());
        assert!(error_code_from_body(r#"{"error_description":"nope"}"#).is_none());
        assert!(error_code_from_body("not json").is_none());
        let long = "x".repeat(41);
        assert!(error_code_from_body(&format!(r#"{{"error":"{long}"}}"#)).is_none());
    }

    // ── (e) HTTP against an in-process mock ─────────────────────────────────

    /// Serve exactly one request from a loopback listener and hand the raw
    /// request text back. No external network is involved.
    fn spawn_mock(status_line: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .ok();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        req.extend_from_slice(&buf[..n]);
                        if request_is_complete(&req) {
                            break;
                        }
                    }
                }
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            String::from_utf8_lossy(&req).to_string()
        });

        (format!("http://127.0.0.1:{port}/v1/oauth/token"), handle)
    }

    /// Headers terminated and the whole declared body read.
    fn request_is_complete(req: &[u8]) -> bool {
        let text = String::from_utf8_lossy(req);
        let Some(head_end) = text.find("\r\n\r\n") else {
            return false;
        };
        let declared = text[..head_end]
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        req.len() >= head_end + 4 + declared
    }

    #[test]
    fn http_200_is_parsed_and_the_request_matches_the_protocol() {
        let (url, server) = spawn_mock(
            "200 OK",
            r#"{"access_token":"tok_example_new","refresh_token":"rtok_example_new",
                "expires_in":28800,"refresh_token_expires_in":7776000,
                "scope":"a:one b:two","account":{"ignored":true}}"#,
        );

        let got =
            request_refresh(&url, "rtok_example_old", Some("a:one b:two")).expect("200 must parse");
        assert_eq!(got.access_token, "tok_example_new");
        assert_eq!(got.refresh_token.as_deref(), Some("rtok_example_new"));
        assert_eq!(got.expires_in, 28_800);
        assert_eq!(got.refresh_token_expires_in, Some(7_776_000));
        assert_eq!(got.scope.as_deref(), Some("a:one b:two"));

        let request = server.join().expect("mock server");
        assert!(request.starts_with("POST /v1/oauth/token"), "{request}");
        assert!(request
            .to_lowercase()
            .contains("content-type: application/json"));
        assert!(request.to_lowercase().contains("user-agent: csm/"));
        assert!(
            request.contains(r#""grant_type":"refresh_token""#),
            "{request}"
        );
        assert!(request.contains(CLIENT_ID), "{request}");
        assert!(request.contains(r#""scope":"a:one b:two""#), "{request}");
    }

    #[test]
    fn http_200_without_optional_fields_still_parses() {
        let (url, server) = spawn_mock(
            "200 OK",
            r#"{"access_token":"tok_example_new","expires_in":3600}"#,
        );
        let got = request_refresh(&url, "rtok_example_old", None).expect("200 must parse");
        assert_eq!(got.refresh_token, None);
        assert_eq!(got.refresh_token_expires_in, None);
        assert_eq!(got.scope, None);

        let request = server.join().expect("mock server");
        assert!(
            !request.contains(r#""scope""#),
            "scope must be omitted when there are no stored scopes: {request}"
        );
    }

    #[test]
    fn http_400_invalid_grant_is_a_short_diagnostic_failure() {
        let (url, server) = spawn_mock(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
        );
        let err = request_refresh(&url, "rtok_example_dead", None).expect_err("400 must fail");
        let rendered = err.to_string();
        assert_eq!(rendered, "refresh HTTP 400 invalid_grant");
        assert!(
            !rendered.contains("revoked"),
            "the response body must never be echoed: {rendered}"
        );
        server.join().expect("mock server");
    }

    #[test]
    fn http_malformed_200_body_is_a_parse_error() {
        let (url, server) = spawn_mock("200 OK", r#"{"unexpected":true}"#);
        let err = request_refresh(&url, "rtok_example_old", None).expect_err("must fail");
        assert_eq!(err.to_string(), "refresh response parse error");
        server.join().expect("mock server");
    }

    #[test]
    fn an_unsafe_token_url_is_rejected_before_anything_is_sent() {
        let err = request_refresh(
            "http://collector.example/v1/oauth/token",
            "rtok_example",
            None,
        )
        .expect_err("cleartext off-host must be refused");
        assert!(matches!(err, RefreshError::UnsafeUrl(_)), "{err}");
    }

    // ── opt-in env ──────────────────────────────────────────────────────────

    #[test]
    fn opt_in_env_accepts_only_affirmative_values() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("TRUE", true),
            ("yes", true),
            ("0", false),
            ("", false),
            ("maybe", false),
        ] {
            crate::testenv::with_env_var("CSM_OAUTH_REFRESH", Some(value), || {
                assert_eq!(opt_in_from_env(), expected, "value {value:?}");
            });
        }
        crate::testenv::with_env_var("CSM_OAUTH_REFRESH", None, || {
            assert!(!opt_in_from_env());
        });
    }

    // ── (g) the token-URL override and the full assembly around it ──────────

    #[test]
    fn resolve_token_url_defaults_and_honours_the_override() {
        crate::testenv::with_env_var("CSM_OAUTH_TOKEN_URL", None, || {
            assert_eq!(resolve_token_url(), DEFAULT_TOKEN_URL);
        });

        crate::testenv::with_env_var(
            "CSM_OAUTH_TOKEN_URL",
            Some("  http://127.0.0.1:9/v1/oauth/token  "),
            || {
                assert_eq!(
                    resolve_token_url(),
                    "http://127.0.0.1:9/v1/oauth/token",
                    "surrounding whitespace is trimmed"
                );
            },
        );

        crate::testenv::with_env_var("CSM_OAUTH_TOKEN_URL", Some("   "), || {
            assert_eq!(
                resolve_token_url(),
                DEFAULT_TOKEN_URL,
                "a blank override is no override"
            );
        });
    }

    /// A credentials file holding [`existing_blob`], the shape the merge
    /// tests already assert against.
    fn credentials_fixture(dir: &Path) -> PathBuf {
        let path = dir.join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&existing_blob()).unwrap(),
        )
        .unwrap();
        path
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
    }

    fn tmp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect()
    }

    #[test]
    fn do_refresh_reads_posts_merges_and_writes_through_the_env_override() {
        let _lock = crate::testenv::lock_for("CSM_OAUTH_TOKEN_URL");
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());

        let (url, server) = spawn_mock(
            "200 OK",
            r#"{"access_token":"tok_example_new","expires_in":28800}"#,
        );
        let _env = TokenUrlEnv::set(&url);

        // The whole assembly: resolve_token_url → read → POST → merge → write.
        let expires_in = do_refresh(dir.path(), fixed_now()).expect("refresh must succeed");
        assert_eq!(expires_in, 28_800);

        // The override really is where the grant went, carrying the stored
        // scopes joined with single spaces.
        let request = server.join().expect("mock server");
        assert!(request.starts_with("POST /v1/oauth/token"), "{request}");
        assert!(
            request.contains(r#""grant_type":"refresh_token""#),
            "{request}"
        );
        assert!(request.contains(CLIENT_ID), "{request}");
        assert!(
            request.contains(r#""scope":"user:inference user:profile""#),
            "{request}"
        );

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let oauth = &back["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], json!("tok_example_new"));
        assert_eq!(
            oauth["refreshToken"],
            json!("rtok_example_old"),
            "a response without a refresh token keeps the stored one"
        );
        assert_eq!(oauth["expiresAt"], json!(now_ms() + 28_800 * 1000));
        assert_eq!(oauth["subscriptionType"], json!("pro"));
        assert_eq!(back["otherTopLevel"], json!(42));
        assert!(tmp_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn do_refresh_changes_nothing_when_the_endpoint_refuses() {
        let _lock = crate::testenv::lock_for("CSM_OAUTH_TOKEN_URL");
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        let (url, server) = spawn_mock("400 Bad Request", r#"{"error":"invalid_grant"}"#);
        let _env = TokenUrlEnv::set(&url);

        let err = do_refresh(dir.path(), fixed_now()).expect_err("400 must fail");
        assert_eq!(err.to_string(), "refresh HTTP 400 invalid_grant");
        server.join().expect("mock server");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(tmp_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn maybe_refresh_returns_on_the_first_gate_without_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        // Opt-in off: the default for every caller but `csm usage
        // --refresh-oauth`. No endpoint is resolved, so no override is needed.
        let outcome = maybe_refresh(dir.path(), fixed_now(), false, CredState::Refreshable);
        assert!(
            matches!(outcome, RefreshOutcome::Skipped(SkipReason::NotOptedIn)),
            "{outcome:?}"
        );

        // Opted in, but the access token is still good — silent everywhere,
        // macOS included (the state gate runs before the platform gate).
        let outcome = maybe_refresh(dir.path(), fixed_now(), true, CredState::Live);
        assert!(
            matches!(
                outcome,
                RefreshOutcome::Skipped(SkipReason::TokenStillValid)
            ),
            "{outcome:?}"
        );

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn maybe_refresh_writes_the_profile_and_releases_the_lock() {
        let _lock = crate::testenv::lock_for("CSM_OAUTH_TOKEN_URL");
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());

        let (url, server) = spawn_mock(
            "200 OK",
            r#"{"access_token":"tok_example_new","expires_in":3600}"#,
        );
        let _env = TokenUrlEnv::set(&url);

        // The platform verdict is injected so this runs on macOS too; every
        // other input is the real thing (no `sessions/` directory at all →
        // no live session; a real lock file; a real atomic write).
        let outcome = maybe_refresh_on(
            dir.path(),
            fixed_now(),
            true,
            CredState::Refreshable,
            /* platform_supported */ true,
        );
        assert!(
            matches!(
                outcome,
                RefreshOutcome::Refreshed {
                    expires_in_secs: 3600
                }
            ),
            "{outcome:?}"
        );
        server.join().expect("mock server");

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            back["claudeAiOauth"]["accessToken"],
            json!("tok_example_new")
        );
        assert!(
            !dir.path()
                .join(".credentials.json.csm-refresh.lock")
                .exists(),
            "the lock guard must release on the success path too"
        );
        assert!(tmp_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn maybe_refresh_reports_a_held_lock_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        // Another writer is mid-refresh (the lock is fresh, so it is honoured
        // rather than taken over) — no endpoint is contacted.
        let lock = dir.path().join(".credentials.json.csm-refresh.lock");
        std::fs::write(&lock, "12345\n").unwrap();

        let outcome = maybe_refresh_on(
            dir.path(),
            fixed_now(),
            true,
            CredState::Refreshable,
            /* platform_supported */ true,
        );
        assert!(
            matches!(outcome, RefreshOutcome::Skipped(SkipReason::LockHeld)),
            "{outcome:?}"
        );
        assert!(lock.exists(), "someone else's lock must survive");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn maybe_refresh_on_an_unsupported_platform_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        let outcome = maybe_refresh_on(
            dir.path(),
            fixed_now(),
            true,
            CredState::Refreshable,
            /* platform_supported */ false,
        );
        assert!(
            matches!(outcome, RefreshOutcome::Unsupported),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(tmp_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn only_macos_is_an_unsupported_platform() {
        assert_eq!(platform_supported(), !cfg!(target_os = "macos"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn maybe_refresh_is_unsupported_on_macos_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_fixture(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        let outcome = maybe_refresh(dir.path(), fixed_now(), true, CredState::Refreshable);
        assert!(
            matches!(outcome, RefreshOutcome::Unsupported),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(tmp_leftovers(dir.path()).is_empty());
    }
}
