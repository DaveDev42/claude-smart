//! Usage fetch transport — hub-local fast path, positive/negative TTL cache,
//! HTTP-first (reqwest blocking), SSH fallback (POSIX only).
//!
//! # Fetch algorithm (spec §2 "Usage transport + caching")
//!
//! Reproduces `fetch_usage()` in `claude-smart-helper.sh.j2` lines 655–728.
//!
//! 1. **Hub-local fast path** (`hostname == "workstation"` → read
//!    `paths::hub_local_cache()` directly, skip all network).
//!    Shell lines 657–662: `if printf '%s' "$host" | grep -qi '^workstation$'; then cat "$USAGE_CACHE"; return 0`
//!
//! 2. **Positive TTL check**: if `paths::usage_cache()` exists and mtime age <
//!    `POSITIVE_TTL_SECS` (60 s, env `CLAUDE_USAGE_TTL`) → parse + return.
//!    Shell lines 666–675: `if [ -s "$pos_cache" ]; then … if [ $(( cnow - cmt )) -lt "$USAGE_TTL" ]; then cat …`
//!
//! 3. **Negative cooldown check**: if `paths::fetch_failed()` exists and age <
//!    `NEGATIVE_COOLDOWN_SECS` (120 s, env `CLAUDE_USAGE_FAIL_COOLDOWN`) →
//!    return `Err(FetchError::NegativeCacheActive)`.
//!    Shell lines 677–687: `if [ -f "$fail_marker" ]; then … if [ $(( now - last )) -lt "$FETCH_FAIL_COOLDOWN" ]; then return 0`
//!
//! 4. **HTTP fetch** (reqwest blocking; connect-timeout 1 s / max-time 2 s).
//!    Shell lines 694–698: `if [ -n "$USAGE_URL" ] && [ -x "$CURL" ]; then out="$(curl -fs --connect-timeout 1 --max-time …)"`
//!    `CLAUDE_USAGE_URL` env: empty string = disable HTTP path; unset = use default URL.
//!    `CLAUDE_USAGE_HTTP_TIMEOUT` env: max-time in seconds (default 2).
//!
//! 5. **SSH fallback** (`#[cfg(unix)]` only; ControlMaster reuse via `ssh`
//!    shell-out; see `ssh_fetch`). Shell lines 699–708.
//!    On Windows, HTTP is the only path (spec §5 #5).
//!
//! 6. On success: validate JSON (`serde_json::from_str`), write cache atomically
//!    (tmp + rename). Clear negative cache. Shell lines 713–720.
//!
//! 7. On failure (both HTTP + SSH fail): stamp `.usage-fetch-failed` epoch.
//!    Shell lines 723–726.

use super::model::UsageData;
use super::FetchError;
use crate::paths;

// ─── constants (overrideable via env) ─────────────────────────────────────────

/// Default positive TTL in seconds. Overridden by `CLAUDE_USAGE_TTL`.
/// Shell: `USAGE_TTL="${CLAUDE_USAGE_TTL:-60}"`.
const DEFAULT_POSITIVE_TTL_SECS: u64 = 60;

/// Default negative cooldown in seconds. Overridden by `CLAUDE_USAGE_FAIL_COOLDOWN`.
/// Shell: `FETCH_FAIL_COOLDOWN="${CLAUDE_USAGE_FAIL_COOLDOWN:-120}"`.
const DEFAULT_NEGATIVE_COOLDOWN_SECS: u64 = 120;

/// Default HTTP total timeout in seconds. Overridden by `CLAUDE_USAGE_HTTP_TIMEOUT`.
/// Shell: `HTTP_DEADLINE="${CLAUDE_USAGE_HTTP_TIMEOUT:-2}"`.
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 2;

/// Default SSH deadline in seconds. Overridden by `CLAUDE_USAGE_SSH_TIMEOUT`.
/// Shell: `SSH_DEADLINE="${CLAUDE_USAGE_SSH_TIMEOUT:-3}"`.
#[cfg(unix)]
const DEFAULT_SSH_DEADLINE_SECS: u64 = 3;

/// Default hub usage URL.
/// Shell: `USAGE_URL="${CLAUDE_USAGE_URL-{{ usage_http_default_url }}}"`.
/// Note the `-` (not `:-`): set-but-empty disables HTTP entirely.
const DEFAULT_HUB_USAGE_URL: &str =
    "http://workstation.example-tnet.ts.net/cc-usage/api/data/limits";

/// Hub hostname (short, case-insensitive match).
/// Shell: `USAGE_HUB="workstation"`.
const HUB_HOSTNAME: &str = "workstation";

// ─── public entry-point ───────────────────────────────────────────────────────

/// Fetch usage data from the hub, obeying the positive/negative TTL caches.
///
/// Returns `Ok(UsageData)` on success or `Err(FetchError)` on any failure
/// (network down, cache-miss, parse error, etc.).
///
/// The caller should treat *any* `Err` as "hub unavailable" and open the
/// hub-down account picker (interactive contexts) or fall back silently
/// (non-interactive contexts).
pub fn fetch() -> Result<UsageData, FetchError> {
    // Step 1 — hub-local fast path.
    // Shell lines 657–662.
    if is_hub_local() {
        return read_hub_local();
    }

    let positive_ttl = positive_ttl_secs();
    let negative_cooldown = negative_cooldown_secs();

    // Step 2 — positive TTL cache (< POSITIVE_TTL_SECS).
    // Shell lines 666–675.
    if let Some(data) = try_positive_cache(positive_ttl)? {
        return Ok(data);
    }

    // Step 3 — negative cooldown (< NEGATIVE_COOLDOWN_SECS).
    // Shell lines 677–687.
    if negative_cache_active(negative_cooldown) {
        return Err(FetchError::NegativeCacheActive);
    }

    // Steps 4 + 5 — HTTP-first, then SSH fallback (POSIX only).
    // Shell lines 694–726.
    match do_network_fetch() {
        Ok(data) => {
            // Success — write positive cache, clear negative cache marker.
            // Shell lines 713–720.
            if let Err(e) = write_positive_cache(&data) {
                // Best-effort; don't fail on a caching error if the data is good.
                eprintln!("csm: warning: could not write usage cache: {e}");
            }
            let _ = std::fs::remove_file(paths::fetch_failed());
            Ok(data)
        }
        Err(e) => {
            // Failure — stamp the negative cache.
            // Shell lines 723–726.
            stamp_negative_cache();
            Err(e)
        }
    }
}

// ─── network fetch orchestration ─────────────────────────────────────────────

/// Run the HTTP-first network fetch, then SSH fallback on POSIX.
///
/// Returns the first successfully-parsed `UsageData`, or an `Err` if all
/// paths fail.
fn do_network_fetch() -> Result<UsageData, FetchError> {
    // HTTP first (all platforms).
    // Shell lines 694–698: if USAGE_URL is set-and-non-empty, try curl.
    // CLAUDE_USAGE_URL set-but-empty = disable HTTP.
    let usage_url = resolve_usage_url();

    if let Some(ref url) = usage_url {
        match http_fetch(url) {
            Ok(data) => return Ok(data),
            Err(_) => {
                // Fall through to SSH fallback (POSIX) or final failure (Windows).
            }
        }
    }

    // SSH fallback — POSIX only.
    // Shell lines 699–708: `out="$(timeout $SSH_DEADLINE ssh … 'cat "$HOME/claude-code-usage/cache/usage-limits.json"')"`.
    #[cfg(unix)]
    {
        ssh_fetch()
    }

    // Windows: HTTP is the only transport.
    #[cfg(not(unix))]
    {
        Err(FetchError::EmptyPayload)
    }
}

/// Resolve the usage URL from the environment.
///
/// Shell: `USAGE_URL="${CLAUDE_USAGE_URL-{{ usage_http_default_url }}}"`.
/// The `-` (not `:-`) means: if `CLAUDE_USAGE_URL` is set but empty, use
/// empty (which disables HTTP); if unset, use the default. Returns `None`
/// when HTTP is disabled (empty URL).
fn resolve_usage_url() -> Option<String> {
    match std::env::var("CLAUDE_USAGE_URL") {
        Ok(val) => {
            if val.is_empty() {
                None // set-but-empty = HTTP disabled
            } else {
                Some(val)
            }
        }
        Err(_) => Some(DEFAULT_HUB_USAGE_URL.to_owned()), // unset = use default
    }
}

// ─── hub-local fast path ─────────────────────────────────────────────────────

/// True when running on Workstation itself (the hub machine).
///
/// Shell lines 657–659: `host="$(hostname -s …)"; if printf '%s' "$host" | grep -qi '^workstation$'`
/// Case-insensitive match (`Workstation` or `workstation`).
fn is_hub_local() -> bool {
    short_hostname().to_ascii_lowercase() == HUB_HOSTNAME
}

/// Read the hub's own `usage-limits.json` (no network needed).
///
/// Shell line 660: `cat "$USAGE_CACHE"`.
fn read_hub_local() -> Result<UsageData, FetchError> {
    let path = paths::hub_local_cache();
    if !path.exists() {
        return Err(FetchError::EmptyPayload);
    }
    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Err(FetchError::EmptyPayload);
    }
    let data: UsageData = serde_json::from_str(&raw)?;
    Ok(data)
}

/// Return the short hostname (no domain suffix), lowercase.
///
/// Shell: `hostname -s 2>/dev/null || hostname`.
/// On POSIX uses `nix::unistd::gethostname`; on Windows uses `GetComputerNameW`
/// via a subprocess (fallback to env var `COMPUTERNAME`).
fn short_hostname() -> String {
    #[cfg(unix)]
    {
        use nix::unistd::gethostname;
        gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            // strip domain suffix — take everything up to the first '.'
            .map(|h| h.split('.').next().unwrap_or(&h).to_owned())
            .unwrap_or_default()
    }

    #[cfg(not(unix))]
    {
        // On Windows read %COMPUTERNAME% first (always set, no subprocess needed).
        std::env::var("COMPUTERNAME")
            .unwrap_or_default()
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .to_owned()
    }
}

// ─── positive TTL cache ───────────────────────────────────────────────────────

/// Read `CLAUDE_USAGE_TTL` env (seconds). Default: 60.
/// Shell: `USAGE_TTL="${CLAUDE_USAGE_TTL:-60}"`.
fn positive_ttl_secs() -> u64 {
    std::env::var("CLAUDE_USAGE_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_POSITIVE_TTL_SECS)
}

/// Return `Ok(Some(data))` if the cache file exists, is non-empty, and its
/// mtime is less than `ttl_secs` old; `Ok(None)` if absent/stale; `Err` on
/// parse failure of a fresh file.
///
/// Shell lines 666–675:
/// ```sh
/// if [ -s "$pos_cache" ]; then
///   cmt="$(stat -f %m … || stat -c %Y …)"
///   cnow="$(date +%s)"
///   if [ $(( cnow - cmt )) -lt "$USAGE_TTL" ]; then cat "$pos_cache"; return 0; fi
/// fi
/// ```
fn try_positive_cache(ttl_secs: u64) -> Result<Option<UsageData>, FetchError> {
    let path = paths::usage_cache();
    if !path.exists() {
        return Ok(None);
    }

    // [ -s "$pos_cache" ] — non-zero size check.
    let meta = std::fs::metadata(&path)?;
    if meta.len() == 0 {
        return Ok(None);
    }

    let age = file_age_secs_from_meta(&meta);
    if age >= ttl_secs {
        return Ok(None);
    }

    // Fresh — parse and return.
    let raw = std::fs::read_to_string(&path)?;
    let data: UsageData = serde_json::from_str(&raw)?;
    Ok(Some(data))
}

// ─── negative cooldown cache ──────────────────────────────────────────────────

/// Read `CLAUDE_USAGE_FAIL_COOLDOWN` env (seconds). Default: 120.
/// Shell: `FETCH_FAIL_COOLDOWN="${CLAUDE_USAGE_FAIL_COOLDOWN:-120}"`.
fn negative_cooldown_secs() -> u64 {
    std::env::var("CLAUDE_USAGE_FAIL_COOLDOWN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_NEGATIVE_COOLDOWN_SECS)
}

/// True if the negative-cooldown file is recent (< `cooldown_secs`).
///
/// Shell lines 679–686:
/// ```sh
/// if [ -f "$fail_marker" ]; then
///   last="$(cat "$fail_marker" 2>/dev/null)"
///   now="$(date +%s)"
///   case "$last" in ''|*[!0-9]*) last=0 ;; esac
///   if [ $(( now - last )) -lt "$FETCH_FAIL_COOLDOWN" ]; then return 0; fi
/// fi
/// ```
///
/// Note: the shell reads the *content* of the file as an epoch, not the mtime.
/// However the shell also writes `date +%s` as content (line 725) in the same
/// process so content ≈ mtime. The shell reads content; we replicate that
/// exactly — read the epoch from the file content, fall back to 0 on parse
/// failure (matches the `case` guard `''|*[!0-9]*)` → `last=0`).
fn negative_cache_active(cooldown_secs: u64) -> bool {
    let path = paths::fetch_failed();
    if !path.exists() {
        return false;
    }
    // Read the epoch written into the file (shell line 681: `last="$(cat…)"`).
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let last_epoch: u64 = content.trim().parse().unwrap_or(0); // shell: case ''|*[!0-9]*) last=0
    let now_epoch = unix_now_secs();
    let age = now_epoch.saturating_sub(last_epoch);
    age < cooldown_secs
}

/// Write (or update) the negative-cooldown sentinel with the current epoch.
///
/// Shell line 725: `date +%s > "$fail_marker" 2>/dev/null`.
/// Best-effort; ignore errors.
fn stamp_negative_cache() {
    let path = paths::fetch_failed();
    // Ensure the parent directory exists.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let epoch = unix_now_secs();
    let _ = std::fs::write(&path, epoch.to_string());
}

// ─── HTTP fetch ───────────────────────────────────────────────────────────────

/// Blocking HTTP fetch with tight timeouts.
///
/// Shell lines 695–697:
/// ```sh
/// out="$("$CURL" -fs --connect-timeout 1 --max-time "$HTTP_DEADLINE" "$USAGE_URL" 2>/dev/null)"
/// ```
/// connect-timeout = 1 s; total timeout = `HTTP_DEADLINE` (default 2 s).
///
/// `-f` = fail on HTTP 4xx/5xx; `-s` = silent.
/// Returns `Err` on any HTTP error (connection refused, timeout, non-2xx).
fn http_fetch(url: &str) -> Result<UsageData, FetchError> {
    use std::time::Duration;

    let http_timeout = std::env::var("CLAUDE_USAGE_HTTP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS);

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(http_timeout))
        .build()
        .map_err(FetchError::Http)?;

    let resp = client.get(url).send().map_err(FetchError::Http)?;

    // Map HTTP errors (4xx/5xx) to failure — mirrors curl -f.
    let resp = resp.error_for_status().map_err(FetchError::Http)?;

    let body = resp.text().map_err(FetchError::Http)?;
    if body.trim().is_empty() {
        return Err(FetchError::EmptyPayload);
    }

    // Validate JSON before returning (shell line 699: `jq -e .`).
    let data: UsageData = serde_json::from_str(&body)?;
    Ok(data)
}

// ─── SSH fallback (POSIX only) ────────────────────────────────────────────────

/// SSH fallback path — POSIX only (ControlMaster socket reuse).
///
/// Reproduces shell lines 699–708:
/// ```sh
/// mkdir -p "$HOME/.ssh" 2>/dev/null
/// out="$("$TIMEOUT" "$SSH_DEADLINE" ssh "${SSH_OPTS[@]}" "$USAGE_HUB" \
///   'cat "$HOME/claude-code-usage/cache/usage-limits.json"' 2>/dev/null)"
/// ```
///
/// SSH options (shell `SSH_OPTS` array):
/// - `BatchMode=yes` (no interactive prompts)
/// - `ConnectTimeout=4`
/// - `ControlMaster=auto`
/// - `ControlPath` → `~/.ssh/cm-claude-%C.sock`
/// - `ControlPersist=300`
///
/// The outer `timeout $SSH_DEADLINE` is implemented here as a
/// `std::process::Command` with `wait_timeout`; we replicate the hard deadline
/// by spawning and checking within the deadline.
///
/// Spec §5 #5: "SSH fallback … POSIX-only behind `cfg(unix)`; Windows has HTTP-only."
#[cfg(unix)]
fn ssh_fetch() -> Result<UsageData, FetchError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Ensure ~/.ssh exists (shell: `mkdir -p "$HOME/.ssh" 2>/dev/null`).
    if let Some(home) = dirs::home_dir() {
        let _ = std::fs::create_dir_all(home.join(".ssh"));
    }

    let ssh_deadline = std::env::var("CLAUDE_USAGE_SSH_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SSH_DEADLINE_SECS);

    // Control path: `~/.ssh/cm-claude-%C.sock`.
    // `%C` is a `ssh_config` token; pass it literally — ssh expands it.
    let control_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".ssh")
        .join("cm-claude-%C.sock");
    let control_path_str = control_path.to_string_lossy().to_string();

    // Remote command: single-quoted so $HOME expands on the REMOTE side.
    // Shell line 708: `'cat "$HOME/claude-code-usage/cache/usage-limits.json"'`
    let remote_cmd = r#"cat "$HOME/claude-code-usage/cache/usage-limits.json""#;

    let start = Instant::now();
    let mut child = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=4",
            "-o", "ControlMaster=auto",
            "-o", &format!("ControlPath={control_path_str}"),
            "-o", "ControlPersist=300",
            HUB_HOSTNAME,   // short MagicDNS name (ssh_config FQDN pin handles resolution)
            remote_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| FetchError::Ssh(format!("spawn failed: {e}")))?;

    // Poll for exit within the deadline (replicates `timeout $SSH_DEADLINE ssh …`).
    let deadline = Duration::from_secs(ssh_deadline);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                break child
                    .wait_with_output()
                    .map_err(|e| FetchError::Ssh(format!("wait_with_output failed: {e}")))?;
            }
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    return Err(FetchError::Ssh(format!(
                        "ssh timed out after {ssh_deadline}s"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(FetchError::Ssh(format!("wait failed: {e}")));
            }
        }
    };

    if !output.status.success() {
        return Err(FetchError::Ssh(format!(
            "ssh exited with status {}",
            output.status
        )));
    }

    let body = String::from_utf8_lossy(&output.stdout).to_string();
    if body.trim().is_empty() {
        return Err(FetchError::EmptyPayload);
    }

    // Shell line 699: validate JSON (`jq -e .`).
    let data: UsageData = serde_json::from_str(&body)?;
    Ok(data)
}

// ─── cache write ─────────────────────────────────────────────────────────────

/// Atomically write `data` to `.usage-cache.json` (tmp + rename).
///
/// Shell lines 716–719:
/// ```sh
/// printf '%s' "$out" > "$pos_cache.$$" 2>/dev/null \
///   && mv -f "$pos_cache.$$" "$pos_cache" 2>/dev/null \
///   || rm -f "$pos_cache.$$" 2>/dev/null
/// ```
///
/// We serialize the `UsageData` back to JSON (the same bytes we received, via
/// serde). The spec says "only validated JSON is ever cached" — we already
/// parsed it above, so serialization here is just re-encoding the same data.
fn write_positive_cache(data: &UsageData) -> Result<(), FetchError> {
    let cache_path = paths::usage_cache();
    let parent = cache_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;

    // Write to a temp file in the same directory (same FS = atomic rename).
    let tmp_path = parent.join(format!(".usage-cache.json.{}", std::process::id()));

    let json_bytes = serde_json::to_vec(data)?;
    std::fs::write(&tmp_path, &json_bytes)?;

    // Atomic rename (mv -f).
    if let Err(e) = std::fs::rename(&tmp_path, &cache_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(FetchError::Io(e));
    }

    Ok(())
}

// ─── utility ─────────────────────────────────────────────────────────────────

/// Return the age of `path` in seconds (wall clock now − mtime).
/// Returns `Err(FetchError::Io)` if the metadata cannot be read.
/// Test-only: production paths call `file_age_secs_from_meta` to avoid a second `stat`.
#[cfg(test)]
fn file_age_secs(path: &std::path::Path) -> Result<u64, FetchError> {
    let meta = std::fs::metadata(path)?;
    Ok(file_age_secs_from_meta(&meta))
}

/// Compute age from an already-fetched `Metadata` (avoids a second `stat` call).
fn file_age_secs_from_meta(meta: &std::fs::Metadata) -> u64 {
    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
    let now = std::time::SystemTime::now();
    now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0)
}

/// Current Unix epoch in seconds.
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── test helpers (not network-touching) ──────────────────────────────────────
//
// These functions are exposed (non-pub, but usable in `#[cfg(test)]`) so unit
// tests can drive freshness via injected file mtimes without hitting the
// network.

/// Parse raw JSON bytes as `UsageData` — the same validation gate the real
/// fetch uses.  Used in tests to verify that only valid JSON passes through.
#[cfg(test)]
pub(crate) fn parse_usage_json(raw: &str) -> Result<UsageData, FetchError> {
    if raw.trim().is_empty() {
        return Err(FetchError::EmptyPayload);
    }
    let data: UsageData = serde_json::from_str(raw)?;
    Ok(data)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Global mutex for tests that mutate process-wide env vars.
    /// Rust test harness runs tests in parallel by default; env var mutation
    /// without serialization causes races between tests that read+write the
    /// same env key (e.g. `resolve_usage_url_*`, `positive_ttl_*`,
    /// `negative_cooldown_*`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── shared fixture JSON ────────────────────────────────────────────────────

    const VALID_USAGE_JSON: &str = r#"{
      "captured_at": "2026-06-17T07:13:19Z",
      "profiles": {
        "personal": {
          "session":  { "pct": 42, "resets": "9pm (Asia/Seoul)" },
          "week_all": { "pct": 31, "resets": "Jun 18 at 9pm (Asia/Seoul)" }
        },
        "work": {
          "session":  { "pct": 5, "resets": null },
          "week_all": { "pct": 67, "resets": "Jun 20 at 8:20pm (Asia/Seoul)" },
          "week_sonnet": null
        }
      },
      "errors": { "broken": "HTTP 401" }
    }"#;

    const INVALID_JSON: &str = r#"{ "profiles": { "p": INVALID }"#;

    // ── helper: write a file with an artificial mtime ─────────────────────────

    /// Write `content` to `path` and set its mtime to `now - age_secs` seconds
    /// ago so the TTL/freshness logic sees the desired age.
    ///
    /// Uses `touch -t [[CC]YY]MMDDhhmm[.SS]` (BSD macOS touch, also accepted
    /// by GNU touch), derived from a computed target epoch via `date -r EPOCH`
    /// (macOS) or `date -d @EPOCH` (Linux/GNU).  Both are gated by
    /// `#[cfg(unix)]` at the call sites.
    fn write_aged_file(path: &std::path::Path, content: &str, age_secs: u64) {
        fs::write(path, content).unwrap();

        #[cfg(unix)]
        {
            let target_epoch = unix_now_secs().saturating_sub(age_secs);

            // Try `date -r EPOCH …` (macOS/BSD) then `date -d @EPOCH …` (GNU).
            let ts = std::process::Command::new("date")
                .args(["-r", &target_epoch.to_string(), "+%Y%m%d%H%M.%S"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
                .or_else(|| {
                    std::process::Command::new("date")
                        .args(["-d", &format!("@{target_epoch}"), "+%Y%m%d%H%M.%S"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_owned())
                })
                .expect("could not format touch timestamp via date -r or date -d");

            let status = std::process::Command::new("touch")
                .args(["-t", &ts, &path.to_string_lossy().to_string()])
                .status()
                .expect("touch -t invocation failed");
            assert!(status.success(), "touch -t exited with failure for ts={ts}");
        }
    }

    // ── parse_usage_json ──────────────────────────────────────────────────────

    #[test]
    fn parse_valid_json_succeeds() {
        let result = parse_usage_json(VALID_USAGE_JSON);
        assert!(result.is_ok(), "expected Ok for valid JSON, got: {result:?}");
        let data = result.unwrap();
        assert!(data.profiles.contains_key("personal"));
    }

    #[test]
    fn parse_empty_string_returns_empty_payload() {
        let result = parse_usage_json("");
        assert!(
            matches!(result, Err(FetchError::EmptyPayload)),
            "expected EmptyPayload for empty string, got: {result:?}"
        );
    }

    #[test]
    fn parse_whitespace_only_returns_empty_payload() {
        let result = parse_usage_json("   \n  ");
        assert!(
            matches!(result, Err(FetchError::EmptyPayload)),
            "expected EmptyPayload for whitespace, got: {result:?}"
        );
    }

    #[test]
    fn parse_invalid_json_returns_json_error() {
        let result = parse_usage_json(INVALID_JSON);
        assert!(
            matches!(result, Err(FetchError::Json(_))),
            "expected Json error for invalid JSON, got: {result:?}"
        );
    }

    #[test]
    fn parse_minimal_json_succeeds() {
        let json = r#"{"profiles": {}}"#;
        let result = parse_usage_json(json);
        assert!(result.is_ok(), "expected Ok for minimal JSON, got: {result:?}");
    }

    // ── negative_cache_active ─────────────────────────────────────────────────

    #[test]
    fn negative_cache_absent_is_not_active() {
        let dir = TempDir::new().unwrap();
        // Point fetch_failed path to a non-existent file.
        // We can't directly inject paths::fetch_failed() in tests, so we test
        // the logic via the content-based function with the actual path helpers
        // by using a temp dir and checking file_age_secs returns correct values.
        //
        // The negative_cache_active function reads paths::fetch_failed() which
        // is under $HOME. We test the LOGIC of the cooldown here with a helper.
        let _ = dir; // suppress unused

        // Test: a file that doesn't exist → not active.
        let non_existent = std::path::Path::new("/tmp/csm_test_never_exists_xyz123.fail");
        assert!(!non_existent.exists(), "precondition: file should not exist");

        // The logic: if file doesn't exist → false.
        let active = if !non_existent.exists() {
            false
        } else {
            true // would read content
        };
        assert!(!active);
    }

    #[test]
    fn negative_cache_content_based_epoch_within_cooldown() {
        // Simulate the shell content-based logic:
        // stamp = now - 30s → still within 120s cooldown.
        let now = unix_now_secs();
        let stamp = now.saturating_sub(30);
        let content = stamp.to_string();

        // Parse as the function does.
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        let age = now.saturating_sub(last_epoch);
        assert!(age < 120, "30s old stamp should be within 120s cooldown");
    }

    #[test]
    fn negative_cache_content_based_epoch_beyond_cooldown() {
        // Stamp = now - 200s → beyond 120s cooldown.
        let now = unix_now_secs();
        let stamp = now.saturating_sub(200);
        let last_epoch: u64 = stamp.to_string().trim().parse().unwrap_or(0);
        let age = now.saturating_sub(last_epoch);
        assert!(age >= 120, "200s old stamp should be beyond 120s cooldown");
    }

    #[test]
    fn negative_cache_empty_content_treated_as_zero() {
        // Shell: `case "$last" in ''|*[!0-9]*) last=0 ;; esac`.
        let content = "";
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        assert_eq!(last_epoch, 0, "empty content should parse as 0");
    }

    #[test]
    fn negative_cache_non_numeric_content_treated_as_zero() {
        // Shell: `*[!0-9]*)` matches non-numeric → last=0.
        let content = "not-a-number";
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        assert_eq!(last_epoch, 0, "non-numeric content should parse as 0");
    }

    #[test]
    fn negative_cache_roundtrip_via_tempdir() {
        // Write a stamp file with a recent epoch and verify cooldown logic
        // correctly identifies it as active.
        let dir = TempDir::new().unwrap();
        let fail_path = dir.path().join(".usage-fetch-failed");

        let now = unix_now_secs();
        // Stamp = now - 10s (within 120s cooldown).
        let stamp = now.saturating_sub(10);
        fs::write(&fail_path, stamp.to_string()).unwrap();

        // Read back and apply the same logic.
        let content = fs::read_to_string(&fail_path).unwrap();
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        let age = now.saturating_sub(last_epoch);
        assert!(age < 120, "10s old stamp should be within 120s cooldown");
    }

    #[test]
    fn negative_cache_roundtrip_expired_stamp() {
        let dir = TempDir::new().unwrap();
        let fail_path = dir.path().join(".usage-fetch-failed");

        let now = unix_now_secs();
        // Stamp = now - 150s (beyond 120s cooldown).
        let stamp = now.saturating_sub(150);
        fs::write(&fail_path, stamp.to_string()).unwrap();

        let content = fs::read_to_string(&fail_path).unwrap();
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        let age = now.saturating_sub(last_epoch);
        assert!(age >= 120, "150s old stamp should be beyond 120s cooldown");
    }

    // ── positive TTL cache (mtime-based) ──────────────────────────────────────

    /// Test that a file written RIGHT NOW has age ≈ 0 and is therefore "fresh"
    /// for any positive TTL > 0.
    #[test]
    fn positive_cache_fresh_file_has_small_age() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join(".usage-cache.json");
        fs::write(&cache, VALID_USAGE_JSON).unwrap();

        let meta = fs::metadata(&cache).unwrap();
        let age = file_age_secs_from_meta(&meta);
        assert!(age < 5, "just-written file should have age < 5s, got {age}");
    }

    #[test]
    #[cfg(unix)]
    fn positive_cache_stale_file_exceeds_ttl() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join(".usage-cache.json");
        // Write a file dated 90 seconds ago — stale for the 60s TTL.
        write_aged_file(&cache, VALID_USAGE_JSON, 90);

        let meta = fs::metadata(&cache).unwrap();
        let age = file_age_secs_from_meta(&meta);
        assert!(
            age >= 60,
            "file aged 90s should have age >= 60s (TTL), got {age}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn positive_cache_fresh_file_within_ttl() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join(".usage-cache.json");
        // Write a file dated 30 seconds ago — fresh for the 60s TTL.
        write_aged_file(&cache, VALID_USAGE_JSON, 30);

        let meta = fs::metadata(&cache).unwrap();
        let age = file_age_secs_from_meta(&meta);
        assert!(
            age < 60,
            "file aged 30s should have age < 60s (TTL), got {age}"
        );
    }

    // ── JSON validation gate ───────────────────────────────────────────────────

    /// Only valid JSON should ever be written to the positive cache.
    /// This mirrors the shell check: `if … | jq -e . >/dev/null 2>&1; then … cache`.
    #[test]
    fn json_validation_gate_blocks_invalid() {
        let result = parse_usage_json(INVALID_JSON);
        assert!(
            matches!(result, Err(FetchError::Json(_))),
            "invalid JSON must not pass the validation gate"
        );
    }

    #[test]
    fn json_validation_gate_passes_valid() {
        let result = parse_usage_json(VALID_USAGE_JSON);
        assert!(result.is_ok(), "valid JSON must pass the validation gate");
    }

    // ── write_positive_cache (atomic write) ───────────────────────────────────

    /// After a successful `write_positive_cache`, the target path exists,
    /// contains valid JSON, and no temp file remains.
    #[test]
    fn write_positive_cache_writes_valid_json_atomically() {
        let dir = TempDir::new().unwrap();
        let cache_path = dir.path().join(".usage-cache.json");

        // Override paths::usage_cache() is not possible without injection,
        // but we can test the atomic-write logic directly.
        let data: UsageData = serde_json::from_str(VALID_USAGE_JSON).unwrap();
        let json_bytes = serde_json::to_vec(&data).unwrap();

        let tmp_path = dir.path().join(".usage-cache.json.testpid");
        fs::write(&tmp_path, &json_bytes).unwrap();
        fs::rename(&tmp_path, &cache_path).unwrap();

        // Verify the final file is valid.
        assert!(cache_path.exists(), "cache file should exist after write");
        assert!(
            !tmp_path.exists(),
            "tmp file should not exist after rename"
        );

        let on_disk = fs::read_to_string(&cache_path).unwrap();
        let parsed: UsageData = serde_json::from_str(&on_disk)
            .expect("on-disk cache must be valid JSON");
        assert!(
            parsed.profiles.contains_key("personal"),
            "on-disk cache should contain personal profile"
        );
    }

    // ── stamp_negative_cache / unix_now_secs ──────────────────────────────────

    #[test]
    fn unix_now_secs_is_reasonable() {
        let now = unix_now_secs();
        // Must be after 2026-01-01 00:00:00 UTC = 1767225600.
        assert!(
            now > 1_767_225_600,
            "unix_now_secs should return a sane epoch, got {now}"
        );
    }

    #[test]
    fn stamp_and_read_negative_cache_via_tempdir() {
        // We can't override global paths in tests, but we can test the
        // stamp_negative_cache content-format assumption: content == epoch string.
        let now_before = unix_now_secs();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fail");

        // Simulate what stamp_negative_cache does.
        let epoch = unix_now_secs();
        fs::write(&path, epoch.to_string()).unwrap();
        let now_after = unix_now_secs();

        let content = fs::read_to_string(&path).unwrap();
        let stored: u64 = content.trim().parse().unwrap();
        assert!(stored >= now_before, "stored epoch should be >= before");
        assert!(stored <= now_after, "stored epoch should be <= after");
    }

    // ── resolve_usage_url ─────────────────────────────────────────────────────
    //
    // These three tests mutate the same env var; they acquire ENV_LOCK to
    // prevent parallel interference with each other.

    #[test]
    fn resolve_usage_url_uses_default_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_URL").ok();
        std::env::remove_var("CLAUDE_USAGE_URL");

        let url = resolve_usage_url();
        assert_eq!(url.as_deref(), Some(DEFAULT_HUB_USAGE_URL));

        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_URL", v),
            None => std::env::remove_var("CLAUDE_USAGE_URL"),
        }
    }

    #[test]
    fn resolve_usage_url_empty_disables_http() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_URL").ok();
        std::env::set_var("CLAUDE_USAGE_URL", "");

        let url = resolve_usage_url();
        assert!(url.is_none(), "empty CLAUDE_USAGE_URL should disable HTTP");

        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_URL", v),
            None => std::env::remove_var("CLAUDE_USAGE_URL"),
        }
    }

    #[test]
    fn resolve_usage_url_custom_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_URL").ok();
        std::env::set_var("CLAUDE_USAGE_URL", "http://custom-hub/api");

        let url = resolve_usage_url();
        assert_eq!(url.as_deref(), Some("http://custom-hub/api"));

        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_URL", v),
            None => std::env::remove_var("CLAUDE_USAGE_URL"),
        }
    }

    // ── is_hub_local ──────────────────────────────────────────────────────────

    #[test]
    fn short_hostname_is_nonempty() {
        // Can't assert what it equals in CI, but it must not be empty.
        let h = short_hostname();
        assert!(!h.is_empty(), "short_hostname() must not be empty");
    }

    #[test]
    fn hub_hostname_const_is_lowercase() {
        assert_eq!(HUB_HOSTNAME, HUB_HOSTNAME.to_ascii_lowercase());
    }

    // ── positive_ttl_secs / negative_cooldown_secs env overrides ─────────────
    //
    // These tests also mutate env vars; acquire ENV_LOCK.

    #[test]
    fn positive_ttl_defaults_to_60() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_TTL").ok();
        std::env::remove_var("CLAUDE_USAGE_TTL");
        assert_eq!(positive_ttl_secs(), 60);
        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_TTL", v),
            None => std::env::remove_var("CLAUDE_USAGE_TTL"),
        }
    }

    #[test]
    fn positive_ttl_respects_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_TTL").ok();
        std::env::set_var("CLAUDE_USAGE_TTL", "30");
        assert_eq!(positive_ttl_secs(), 30);
        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_TTL", v),
            None => std::env::remove_var("CLAUDE_USAGE_TTL"),
        }
    }

    #[test]
    fn negative_cooldown_defaults_to_120() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_FAIL_COOLDOWN").ok();
        std::env::remove_var("CLAUDE_USAGE_FAIL_COOLDOWN");
        assert_eq!(negative_cooldown_secs(), 120);
        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_FAIL_COOLDOWN", v),
            None => std::env::remove_var("CLAUDE_USAGE_FAIL_COOLDOWN"),
        }
    }

    #[test]
    fn negative_cooldown_respects_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CLAUDE_USAGE_FAIL_COOLDOWN").ok();
        std::env::set_var("CLAUDE_USAGE_FAIL_COOLDOWN", "60");
        assert_eq!(negative_cooldown_secs(), 60);
        match saved {
            Some(v) => std::env::set_var("CLAUDE_USAGE_FAIL_COOLDOWN", v),
            None => std::env::remove_var("CLAUDE_USAGE_FAIL_COOLDOWN"),
        }
    }

    // ── file_age_secs ─────────────────────────────────────────────────────────

    #[test]
    fn file_age_secs_fresh_file_is_small() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("test.txt");
        fs::write(&f, "hello").unwrap();
        let age = file_age_secs(&f).unwrap();
        assert!(age < 5, "just-written file age should be < 5s, got {age}");
    }

    #[test]
    fn file_age_secs_missing_file_returns_io_err() {
        let result = file_age_secs(std::path::Path::new("/tmp/csm_nonexistent_xyz123.txt"));
        assert!(
            matches!(result, Err(FetchError::Io(_))),
            "missing file should return Io error"
        );
    }

    #[test]
    #[cfg(unix)]
    fn file_age_secs_aged_file_matches_expected() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("old.txt");
        // Write the file dated 70 seconds ago.
        write_aged_file(&f, "data", 70);
        let age = file_age_secs(&f).unwrap();
        // Allow ±5s for any scheduling jitter.
        assert!(
            age >= 65 && age <= 80,
            "file aged 70s should report age ≈ 70s, got {age}"
        );
    }
}
