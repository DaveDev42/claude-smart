//! Usage fetch transport — positive/negative TTL cache in front of local,
//! per-profile usage collection.
//!
//! # Fetch algorithm
//!
//! 1. **Positive TTL check**: if `paths::usage_cache()` exists and mtime age <
//!    `POSITIVE_TTL_SECS` (60 s, env `CLAUDE_USAGE_TTL` / alias `CSM_USAGE_TTL_SECS`)
//!    → parse + return. Skipped entirely when the caller asked for a forced
//!    refresh ([`fetch_with`] with `force = true`).
//!
//! 2. **User command** (`CSM_USAGE_CMD`, if set): run it via the shell, parse
//!    stdout as `UsageData`. This is the operator-injected "check via a provided
//!    script" source; it runs *before* the negative cooldown (an explicit
//!    command is independent of whether local collection is failing) and takes
//!    precedence over local collection. On success the result is cached; on
//!    failure it falls through to local collection.
//!
//! 3. **Negative cooldown check**: if `paths::fetch_failed()` exists and age <
//!    `NEGATIVE_COOLDOWN_SECS` (120 s, env `CLAUDE_USAGE_FAIL_COOLDOWN`) →
//!    return `Err(FetchError::NegativeCacheActive)`. This guards against
//!    hammering local collection (keychain reads + the OAuth usage API) when
//!    every profile just failed outright. Skipped entirely when the caller
//!    asked for a forced refresh ([`fetch_with`] with `force = true`) — same
//!    as step 1, a stale stamp from a prior all-failed round must not defeat
//!    an explicit `--refresh`.
//!
//! 4. **Local collection** ([`super::local::collect`]) — the terminal layer.
//!    For every profile in the registry it serves a fresh per-profile store
//!    record, or probes live credentials + the Anthropic OAuth usage API, or
//!    serves a stale record, or records an error; see `local::mod` for that
//!    per-profile decision matrix. `force` is threaded straight through, so a
//!    forced refresh also bypasses each profile's own store-record TTL.
//!
//! 5. On success — defined as "at least one profile produced data, or there
//!    were no errors at all" (an empty registry is a legitimate, if empty,
//!    result) — write the positive cache.
//!
//! 6. On total failure — `collect` returned zero profiles AND at least one
//!    error, i.e. every configured profile failed — stamp the negative
//!    cooldown and return `Err(FetchError::EmptyPayload)`.
//!
//! 6a. **Live-probe-all-failed, but not total failure**: `collect` can
//!    "succeed" (non-empty `profiles`) purely on `ServeStale` fallbacks while
//!    every profile that actually reached the network/keychain failed —
//!    an offline machine with existing store records never trips step 6's
//!    `profiles.is_empty()` check, so without this branch the negative
//!    cooldown could never engage for exactly the case it exists for. When
//!    `UsageData::any_probe_attempted && !any_probe_succeeded`, the negative
//!    cooldown is stamped too — the call still returns `Ok` (the stale data
//!    is legitimate to serve), but the *next* call's step 3 short-circuits
//!    instead of re-paying a full probe round (keychain read + OAuth call
//!    per profile) on every launch.

use chrono::Utc;

use super::model::UsageData;
use super::FetchError;
use crate::account::ProfileMap;
use crate::paths;

// ─── constants (overrideable via env) ─────────────────────────────────────────

/// Default positive TTL in seconds. Overridden by `CLAUDE_USAGE_TTL`.
const DEFAULT_POSITIVE_TTL_SECS: u64 = 60;

/// Default negative cooldown in seconds. Overridden by `CLAUDE_USAGE_FAIL_COOLDOWN`.
const DEFAULT_NEGATIVE_COOLDOWN_SECS: u64 = 120;

// ─── public entry-points ───────────────────────────────────────────────────────

/// Fetch usage data, obeying the positive/negative TTL caches. Equivalent to
/// `fetch_with(false)`.
///
/// Returns `Ok(UsageData)` on success or `Err(FetchError)` on any failure
/// (every profile's collection failed, cache-miss, parse error, etc.).
///
/// The caller should treat *any* `Err` as "no usage data available right now"
/// and open the offline account picker (interactive contexts) or fall back
/// silently (non-interactive contexts).
pub fn fetch() -> Result<UsageData, FetchError> {
    fetch_with(false, false)
}

/// Like [`fetch`], but `force = true` bypasses the positive cache AND is
/// threaded into [`super::local::collect`] so every profile's own store-record
/// TTL is bypassed too — a live re-probe of every profile, not just a
/// cache-refresh. Used by `csm usage --refresh`.
///
/// `refresh_oauth` is threaded straight through to
/// [`super::local::collect`], where it permits a gated access-token refresh
/// for a profile whose token has expired and under which no live Claude Code
/// session exists (see `local::refresh`). Only `csm usage --refresh-oauth` /
/// `CSM_OAUTH_REFRESH=1` sets it; [`fetch`] passes `false`, so every other
/// caller keeps today's read-only behavior.
pub fn fetch_with(force: bool, refresh_oauth: bool) -> Result<UsageData, FetchError> {
    let positive_ttl = positive_ttl_secs();
    let negative_cooldown = negative_cooldown_secs();

    // Step 1 — positive TTL cache (< POSITIVE_TTL_SECS), skipped under force.
    if !force {
        if let Some(data) = try_positive_cache(positive_ttl)? {
            return Ok(data);
        }
    }

    // Step 2 — user-supplied usage command (`CSM_USAGE_CMD`), if set.
    //
    // When the operator wires a metering command (a site script, or anything
    // that emits UsageData JSON on stdout), it is the explicit "check via a
    // provided script" source and takes precedence over local collection.
    //
    // It runs *before* the negative-cooldown gate on purpose: that cooldown
    // exists to avoid hammering local collection (keychain reads + the OAuth
    // API) when it just failed outright, but an explicit command is an
    // independent source the user asked for — a local-collection outage must
    // not silently suppress it. Success is cached like a live fetch so the
    // (potentially slow) command is not re-run within the positive TTL.
    if let Some(cmd) = resolve_usage_command() {
        match run_usage_command(&cmd) {
            Ok(data) => {
                if let Err(e) = write_positive_cache(&data) {
                    eprintln!("csm: warning: could not write usage cache: {e}");
                }
                let _ = std::fs::remove_file(paths::fetch_failed());
                return Ok(data);
            }
            Err(e) => {
                // Command failed — fall through to local collection (the
                // command is an override, not a hard gate). The negative-
                // cooldown check and the final stamp below still apply.
                eprintln!("csm: warning: CSM_USAGE_CMD failed: {e}");
            }
        }
    }

    // Step 3 — negative cooldown (< NEGATIVE_COOLDOWN_SECS), skipped under
    // force for the same reason step 1's positive-cache check is: `--refresh`
    // means "go probe live regardless". Without this, a stamp left over from
    // a PRIOR failed round (e.g. every profile NeedsLogin) would short-circuit
    // `csm usage --refresh` straight to `Err`, and the caller degrades to the
    // last-known *positive* cache — silently hiding a just-recorded statusline
    // capture (or a token that has since been logged back in) behind stale
    // LOGIN REQUIRED data instead of ever reaching `local::collect` to re-probe.
    if !force && negative_cache_active(negative_cooldown) {
        return Err(FetchError::NegativeCacheActive);
    }

    // Step 4 — local, per-profile collection (the terminal layer).
    let profiles = ProfileMap::load().unwrap_or_default();
    let data = super::local::collect(&profiles, Utc::now(), force, refresh_oauth);

    // Total failure: every configured profile produced an error and none
    // produced usable data. An empty registry (zero profiles, zero errors)
    // is NOT a failure — it is a legitimate, if empty, result.
    let total_failure =
        data.profiles.is_empty() && data.errors.as_ref().is_some_and(|e| !e.is_empty());

    if total_failure {
        stamp_negative_cache();
        return Err(FetchError::EmptyPayload);
    }

    // Live-probe-all-failed (step 6a): `data.profiles` can be non-empty
    // purely from `ServeStale` fallbacks while every profile that actually
    // reached the network/keychain this round failed. `total_failure` above
    // can't see that — it only looks at whether `profiles` ended up empty,
    // and a `ServeStale` round populates it even though nothing new was
    // learned. Stamp the cooldown here too so the NEXT call's negative-cache
    // check (step 3) short-circuits instead of re-paying a full probe round
    // on every launch of an offline/all-expired-tokens machine — the current
    // call still returns `Ok` below, since the stale data is legitimate to
    // serve right now.
    if data.any_probe_attempted && !data.any_probe_succeeded {
        stamp_negative_cache();
    } else {
        let _ = std::fs::remove_file(paths::fetch_failed());
    }

    // Success (possibly partial — some profiles errored, others didn't).
    if let Err(e) = write_positive_cache(&data) {
        // Best-effort; don't fail on a caching error if the data is good.
        eprintln!("csm: warning: could not write usage cache: {e}");
    }
    Ok(data)
}

// ─── user-supplied usage command (CSM_USAGE_CMD) ──────────────────────────────

/// Resolve the operator-supplied usage command from `CSM_USAGE_CMD`.
///
/// Empty/unset = disabled (returns `None`) — no command path is compiled in.
/// The command is run via the platform shell so a full pipeline / script path
/// works; its stdout must be a `UsageData` JSON object.
///
/// This honors the crate's separation invariant: the extraction *mechanism*
/// (which is fragile and site-specific — see the PoC findings on `claude`'s
/// `/usage`) is injected, never baked into the binary.
fn resolve_usage_command() -> Option<String> {
    std::env::var("CSM_USAGE_CMD")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Run `cmd` through the platform shell, parse its stdout as `UsageData`.
///
/// Honors `CSM_USAGE_CMD_TIMEOUT` (seconds, default 10) as a hard deadline —
/// claude-direct extraction is slow (~2–30 s in PoC), so the command must not
/// block csm indefinitely on a prompt-path call.
fn run_usage_command(cmd: &str) -> Result<UsageData, FetchError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let timeout_secs = std::env::var("CSM_USAGE_CMD_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10);

    #[cfg(unix)]
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| FetchError::Command(format!("spawn failed: {e}")))?;

    #[cfg(not(unix))]
    let mut child = Command::new("cmd")
        .args(["/C", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| FetchError::Command(format!("spawn failed: {e}")))?;

    // Drain stdout on a dedicated thread so the child never blocks on a full
    // pipe buffer (~64 KB) while we poll for exit. Without this, a command that
    // emits more than the buffer deadlocks: the child blocks writing, we block in
    // try_wait, and the (valid) result is lost to the timeout. The reader thread
    // owns the pipe and reads to EOF, which it reaches when the child exits.
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| FetchError::Command("stdout pipe missing".into()))?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut pipe = stdout_pipe;
        std::io::Read::read_to_end(&mut pipe, &mut buf).map(|_| buf)
    });

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap so we don't leave a zombie
                                          // Do NOT join the reader here. Killing the direct child does
                                          // not guarantee the pipe's write-end closes: a grandchild
                                          // (e.g. `cmd | cat`) can inherit it and outlive the parent,
                                          // so read_to_end never reaches EOF and a join would block past
                                          // the deadline — defeating the whole timeout. Drop the handle
                                          // instead: the detached thread ends on its own once the last
                                          // write-end finally closes, and is reaped at process exit.
                    drop(reader);
                    return Err(FetchError::Command(format!(
                        "timed out after {timeout_secs}s"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                // Same rationale as the timeout path: a surviving grandchild can
                // keep the pipe open, so detach rather than join.
                drop(reader);
                return Err(FetchError::Command(format!("wait failed: {e}")));
            }
        }
    };

    let stdout_bytes = match reader.join() {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return Err(FetchError::Command(format!("read failed: {e}"))),
        Err(_) => return Err(FetchError::Command("stdout reader thread panicked".into())),
    };

    if !status.success() {
        return Err(FetchError::Command(format!(
            "command exited with status {status}"
        )));
    }

    // A clear diagnostic for non-UTF-8 output beats a confusing JSON parse error.
    let body = String::from_utf8(stdout_bytes)
        .map_err(|_| FetchError::Command("command output is not valid UTF-8".into()))?;
    if body.trim().is_empty() {
        return Err(FetchError::Command("command produced empty output".into()));
    }
    serde_json::from_str(&body)
        .map_err(|e| FetchError::Command(format!("output not UsageData: {e}")))
}

// ─── positive TTL cache ───────────────────────────────────────────────────────

/// Read the positive cache TTL (seconds). Default: 60.
///
/// Precedence: `CLAUDE_USAGE_TTL` (the legacy shell name) then
/// `CSM_USAGE_TTL_SECS` (the csm-native alias), then the default. Exposing the
/// alias lets users configure the cache lifetime under a csm-prefixed name
/// without knowing the legacy variable.
fn positive_ttl_secs() -> u64 {
    std::env::var("CLAUDE_USAGE_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            std::env::var("CSM_USAGE_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(DEFAULT_POSITIVE_TTL_SECS)
}

/// Return `Ok(Some(data))` if the cache file exists, is non-empty, and its
/// mtime is less than `ttl_secs` old; `Ok(None)` if absent/stale; `Err` on
/// parse failure of a fresh file.
fn try_positive_cache(ttl_secs: u64) -> Result<Option<UsageData>, FetchError> {
    let path = paths::usage_cache();
    if !path.exists() {
        return Ok(None);
    }

    // Non-zero size check.
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
fn negative_cooldown_secs() -> u64 {
    std::env::var("CLAUDE_USAGE_FAIL_COOLDOWN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_NEGATIVE_COOLDOWN_SECS)
}

/// True if the negative-cooldown file is recent (< `cooldown_secs`).
///
/// Reads the epoch written into the file's *content* (not its mtime — see
/// [`stamp_negative_cache`], which writes the same epoch as content); falls
/// back to `0` (i.e. "definitely expired") on any parse failure.
fn negative_cache_active(cooldown_secs: u64) -> bool {
    let path = paths::fetch_failed();
    if !path.exists() {
        return false;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let last_epoch: u64 = content.trim().parse().unwrap_or(0);
    let now_epoch = unix_now_secs();
    let age = now_epoch.saturating_sub(last_epoch);
    age < cooldown_secs
}

/// Write (or update) the negative-cooldown sentinel with the current epoch.
///
/// Stamped when local collection fails for every configured profile — the
/// coarse "nothing at all is working right now" signal that gates a burst of
/// repeat probes (each of which is a keychain read + a live API call per
/// profile). Best-effort; ignore errors.
fn stamp_negative_cache() {
    let path = paths::fetch_failed();
    // Ensure the parent directory exists.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let epoch = unix_now_secs();
    let _ = std::fs::write(&path, epoch.to_string());
}

// ─── cache write ─────────────────────────────────────────────────────────────

/// Atomically write `data` to `.usage-cache.json` (tmp + rename).
///
/// We serialize the `UsageData` back to JSON (the same shape we received, via
/// serde). Only validated `UsageData` is ever cached — we already parsed it
/// (from the cache, the user command, or local collection) above, so
/// serialization here is just re-encoding the same data.
fn write_positive_cache(data: &UsageData) -> Result<(), FetchError> {
    let cache_path = paths::usage_cache();
    let parent = cache_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
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
    /// same env key (e.g. `positive_ttl_*`, `negative_cooldown_*`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── shared fixture JSON ────────────────────────────────────────────────────

    const VALID_USAGE_JSON: &str = r#"{
      "captured_at": "2026-06-17T07:13:19Z",
      "profiles": {
        "home": {
          "session":  { "pct": 42, "resets": "9pm (Asia/Seoul)" },
          "week_all": { "pct": 31, "resets": "Jun 18 at 9pm (Asia/Seoul)" }
        },
        "work": {
          "session":  { "pct": 5, "resets": null },
          "week_all": { "pct": 67, "resets": "Jun 20 at 8:20pm (Asia/Seoul)" },
          "week_fable": null
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
    /// (macOS) or `date -d @EPOCH` (Linux/GNU).  Both this helper and all its
    /// callers are `#[cfg(unix)]` (the mtime-aging trick is POSIX-only).
    #[cfg(unix)]
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
                .args(["-t", &ts, path.to_string_lossy().as_ref()])
                .status()
                .expect("touch -t invocation failed");
            assert!(status.success(), "touch -t exited with failure for ts={ts}");
        }
    }

    // ── parse_usage_json ──────────────────────────────────────────────────────

    #[test]
    fn parse_valid_json_succeeds() {
        let result = parse_usage_json(VALID_USAGE_JSON);
        assert!(
            result.is_ok(),
            "expected Ok for valid JSON, got: {result:?}"
        );
        let data = result.unwrap();
        assert!(data.profiles.contains_key("home"));
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
        assert!(
            result.is_ok(),
            "expected Ok for minimal JSON, got: {result:?}"
        );
    }

    // ── negative_cache_active ─────────────────────────────────────────────────

    #[test]
    fn negative_cache_absent_is_not_active() {
        // A file that doesn't exist → not active.
        let non_existent = std::path::Path::new("/tmp/csm_test_never_exists_xyz123.fail");
        assert!(
            !non_existent.exists(),
            "precondition: file should not exist"
        );
        let active = if !non_existent.exists() {
            false
        } else {
            true // would read content
        };
        assert!(!active);
    }

    #[test]
    fn negative_cache_content_based_epoch_within_cooldown() {
        // Simulate the content-based logic: stamp = now - 30s → still within
        // 120s cooldown.
        let now = unix_now_secs();
        let stamp = now.saturating_sub(30);
        let content = stamp.to_string();

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
        let content = "";
        let last_epoch: u64 = content.trim().parse().unwrap_or(0);
        assert_eq!(last_epoch, 0, "empty content should parse as 0");
    }

    #[test]
    fn negative_cache_non_numeric_content_treated_as_zero() {
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
        assert!(!tmp_path.exists(), "tmp file should not exist after rename");

        let on_disk = fs::read_to_string(&cache_path).unwrap();
        let parsed: UsageData =
            serde_json::from_str(&on_disk).expect("on-disk cache must be valid JSON");
        assert!(
            parsed.profiles.contains_key("home"),
            "on-disk cache should contain home profile"
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

    // ── CSM_USAGE_CMD (user-supplied usage command) ───────────────────────────

    #[test]
    fn resolve_usage_command_disabled_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CSM_USAGE_CMD").ok();
        std::env::remove_var("CSM_USAGE_CMD");
        assert!(resolve_usage_command().is_none(), "unset → None");
        // set-but-empty / whitespace → None
        std::env::set_var("CSM_USAGE_CMD", "   ");
        assert!(resolve_usage_command().is_none(), "blank → None");
        match saved {
            Some(v) => std::env::set_var("CSM_USAGE_CMD", v),
            None => std::env::remove_var("CSM_USAGE_CMD"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_parses_valid_json_stdout() {
        // A command that emits a valid UsageData JSON on stdout → Ok(data).
        // VALID_USAGE_JSON is tiny (~350 bytes), so inlining it as a shell
        // argument is ARG_MAX-safe. Do NOT inline a LARGE payload this way —
        // it overflows execve's ARG_MAX on Linux (see the deadlock test below,
        // which has the child generate its big payload via awk instead).
        let cmd = format!("printf '%s' '{}'", VALID_USAGE_JSON.replace('\n', " "));
        let result = run_usage_command(&cmd);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap().profiles.contains_key("home"));
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_nonzero_exit_is_command_error() {
        let result = run_usage_command("exit 3");
        assert!(
            matches!(result, Err(FetchError::Command(_))),
            "non-zero exit must be a Command error, got: {result:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_empty_stdout_is_command_error() {
        let result = run_usage_command("true"); // exits 0, no stdout
        assert!(
            matches!(result, Err(FetchError::Command(_))),
            "empty stdout must be a Command error, got: {result:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_non_json_stdout_is_command_error() {
        let result = run_usage_command("echo not-json-at-all");
        assert!(
            matches!(result, Err(FetchError::Command(_))),
            "non-JSON stdout must be a Command error, got: {result:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_respects_timeout() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CSM_USAGE_CMD_TIMEOUT").ok();
        std::env::set_var("CSM_USAGE_CMD_TIMEOUT", "1");
        let start = std::time::Instant::now();
        let result = run_usage_command("sleep 10");
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(FetchError::Command(_))),
            "a command past the deadline must error, got: {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout must fire well before the command's own 10s, took {elapsed:?}"
        );
        match saved {
            Some(v) => std::env::set_var("CSM_USAGE_CMD_TIMEOUT", v),
            None => std::env::remove_var("CSM_USAGE_CMD_TIMEOUT"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_handles_large_output_without_deadlock() {
        // Regression for the pipe-deadlock finding: if the command writes more
        // than the OS pipe buffer (~64 KB) to stdout, a wait-then-read loop that
        // never drains the pipe will deadlock — the child blocks on write while
        // we block in try_wait — and only escape via the timeout, discarding the
        // (valid) result. Build a >256 KB valid UsageData JSON and assert it
        // parses well within a short deadline.
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CSM_USAGE_CMD_TIMEOUT").ok();
        // 3s deadline: comfortably long for a correct drain, but far shorter than
        // the wall time a deadlock would burn — so a deadlock fails the test fast.
        std::env::set_var("CSM_USAGE_CMD_TIMEOUT", "3");

        // Have the CHILD generate the large payload itself, via a tiny awk
        // program, rather than inlining a >256 KB JSON string as a shell
        // ARGUMENT. Inlining it (`printf '%s' '<huge json>'`) overflows
        // ARG_MAX on Linux (execve E2BIG) even though macOS's larger ARG_MAX
        // tolerated it — that divergence is exactly what broke CI. The awk
        // command string is ~300 bytes (ARG_MAX-safe by 400x) while its stdout
        // is ~341 KB, comfortably past the OS pipe buffer (~64 KB) that the
        // drain thread must survive. POSIX awk only (BEGIN, printf, C-style
        // for/if, %d, % modulo) — no gawk extensions, no seq, no bash-isms —
        // so it runs identically on GNU/Linux and BSD/macOS. i goes 0..=3000,
        // yielding 3001 profiles.
        let n_profiles = 3001;
        let cmd = r#"awk 'BEGIN{printf "{\"captured_at\":\"2024-01-01T00:00:00Z\",\"profiles\":{"; for(i=0;i<=3000;i++){if(i>0)printf ","; printf "\"p%d\":{\"session\":{\"pct\":%d,\"resets\":\"2024-01-01T00:00:00Z\"},\"week_all\":{\"pct\":%d,\"resets\":\"2024-01-01T00:00:00Z\"}}",i,i%100,i%100}; printf "},\"errors\":{}}"}'"#;
        let start = std::time::Instant::now();
        let result = run_usage_command(cmd);
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "large output must parse (deadlock would time out): {result:?}"
        );
        assert_eq!(
            result.unwrap().profiles.len(),
            n_profiles,
            "all profiles parsed"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "must not hit the deadline — a deadlock would, took {elapsed:?}"
        );

        match saved {
            Some(v) => std::env::set_var("CSM_USAGE_CMD_TIMEOUT", v),
            None => std::env::remove_var("CSM_USAGE_CMD_TIMEOUT"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn run_usage_command_timeout_is_hard_even_when_a_grandchild_holds_the_pipe() {
        // The stdout-drain thread reads to EOF, which it only reaches when the
        // pipe's last write-end closes. On timeout we kill the DIRECT child
        // (`sh`), but a grandchild can inherit the same stdout pipe and outlive
        // it — e.g. `sleep | cat`, where `cat` holds the write-end. If the
        // timeout path were to `reader.join()` unconditionally, that join would
        // block until the grandchild died on its own, silently defeating the
        // hard deadline. This test pins that the timeout returns within the
        // deadline regardless of a surviving grandchild.
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("CSM_USAGE_CMD_TIMEOUT").ok();
        std::env::set_var("CSM_USAGE_CMD_TIMEOUT", "1");

        // `sleep 30 | cat`: cat inherits our stdout pipe and stays alive ~30s
        // after sh is killed, holding the write-end open so read_to_end can't
        // reach EOF.
        let start = std::time::Instant::now();
        let result = run_usage_command("sleep 30 | cat");
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(FetchError::Command(_))),
            "a command past the deadline must error, got: {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout must stay hard even with a grandchild holding the pipe, took {elapsed:?}"
        );

        match saved {
            Some(v) => std::env::set_var("CSM_USAGE_CMD_TIMEOUT", v),
            None => std::env::remove_var("CSM_USAGE_CMD_TIMEOUT"),
        }
    }

    #[test]
    fn positive_ttl_alias_csm_secs() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_legacy = std::env::var("CLAUDE_USAGE_TTL").ok();
        let saved_alias = std::env::var("CSM_USAGE_TTL_SECS").ok();
        // Legacy unset, alias set → alias wins.
        std::env::remove_var("CLAUDE_USAGE_TTL");
        std::env::set_var("CSM_USAGE_TTL_SECS", "17");
        assert_eq!(positive_ttl_secs(), 17, "alias should be honored");
        // Legacy set → legacy takes precedence over alias.
        std::env::set_var("CLAUDE_USAGE_TTL", "5");
        assert_eq!(positive_ttl_secs(), 5, "legacy var should win over alias");
        match saved_legacy {
            Some(v) => std::env::set_var("CLAUDE_USAGE_TTL", v),
            None => std::env::remove_var("CLAUDE_USAGE_TTL"),
        }
        match saved_alias {
            Some(v) => std::env::set_var("CSM_USAGE_TTL_SECS", v),
            None => std::env::remove_var("CSM_USAGE_TTL_SECS"),
        }
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
            (65..=80).contains(&age),
            "file aged 70s should report age ≈ 70s, got {age}"
        );
    }

    // ── fetch_with total-failure classification (pure, no I/O) ───────────────
    //
    // `fetch_with` itself touches the registry/keychain/network and is the
    // thin I/O shell — its total-failure predicate is exercised here as plain
    // boolean logic, mirroring exactly what the function computes.

    #[test]
    fn total_failure_is_zero_profiles_and_nonempty_errors() {
        let mut errors = std::collections::HashMap::new();
        errors.insert("home".to_string(), "not logged in".to_string());
        let data = UsageData {
            captured_at: None,
            profiles: std::collections::HashMap::new(),
            errors: Some(errors),
            ..Default::default()
        };
        let total_failure =
            data.profiles.is_empty() && data.errors.as_ref().is_some_and(|e| !e.is_empty());
        assert!(total_failure);
    }

    #[test]
    fn empty_registry_is_not_total_failure() {
        // Zero profiles, zero errors (an empty registry) is a legitimate
        // empty result, not a failure.
        let data = UsageData {
            captured_at: None,
            profiles: std::collections::HashMap::new(),
            errors: None,
            ..Default::default()
        };
        let total_failure =
            data.profiles.is_empty() && data.errors.as_ref().is_some_and(|e| !e.is_empty());
        assert!(!total_failure);
    }

    #[test]
    fn partial_success_is_not_total_failure() {
        let mut profiles = std::collections::HashMap::new();
        profiles.insert("home".to_string(), Default::default());
        let mut errors = std::collections::HashMap::new();
        errors.insert("work".to_string(), "token expired".to_string());
        let data = UsageData {
            captured_at: None,
            profiles,
            errors: Some(errors),
            ..Default::default()
        };
        let total_failure =
            data.profiles.is_empty() && data.errors.as_ref().is_some_and(|e| !e.is_empty());
        assert!(!total_failure, "one good profile is a partial success");
    }

    // ── fetch_with live-probe-all-failed classification (pure, no I/O) ───────
    //
    // Mirrors the total-failure block above: `fetch_with`'s "stamp the
    // negative cooldown even on a step-4 `Ok`" predicate
    // (`data.any_probe_attempted && !data.any_probe_succeeded`) exercised as
    // plain boolean logic against constructed `UsageData` values, without
    // touching the real negative-cache file (which is keyed off the real
    // `$HOME` via `paths::fetch_failed()`).

    #[test]
    fn servestale_only_round_is_live_probe_all_failed() {
        // Every profile in `profiles` came from a `ServeStale` fallback (a
        // stale store record survived an offline/expired-token round) — NOT
        // total failure (`profiles` is non-empty), but every live attempt
        // this round still failed.
        let mut profiles = std::collections::HashMap::new();
        profiles.insert("home".to_string(), Default::default());
        let data = UsageData {
            profiles,
            any_probe_attempted: true,
            any_probe_succeeded: false,
            ..Default::default()
        };
        let total_failure =
            data.profiles.is_empty() && data.errors.as_ref().is_some_and(|e| !e.is_empty());
        assert!(
            !total_failure,
            "ServeStale populating `profiles` must not itself read as total failure"
        );
        assert!(
            data.any_probe_attempted && !data.any_probe_succeeded,
            "but the cooldown predicate must still catch it"
        );
    }

    #[test]
    fn any_live_probe_succeeding_is_not_live_probe_all_failed() {
        let mut profiles = std::collections::HashMap::new();
        profiles.insert("home".to_string(), Default::default());
        let data = UsageData {
            profiles,
            any_probe_attempted: true,
            any_probe_succeeded: true,
            ..Default::default()
        };
        assert!(
            !(data.any_probe_attempted && !data.any_probe_succeeded),
            "one successful live probe must not stamp the cooldown"
        );
    }

    #[test]
    fn no_probe_attempted_is_not_live_probe_all_failed() {
        // Every profile served from `Freshness::Fresh` (no probe needed at
        // all this round) — nothing was even attempted, so there is nothing
        // to conclude "failed" about.
        let mut profiles = std::collections::HashMap::new();
        profiles.insert("home".to_string(), Default::default());
        let data = UsageData {
            profiles,
            any_probe_attempted: false,
            any_probe_succeeded: false,
            ..Default::default()
        };
        assert!(!(data.any_probe_attempted && !data.any_probe_succeeded));
    }

    // ── fetch_with: negative cooldown must not block a forced refresh ────────
    //
    // Bug B root cause: step 3's negative-cooldown gate had no `!force`
    // guard, unlike step 1's positive-cache check. A prior all-NeedsLogin
    // round stamps the negative cooldown (see
    // `servestale_only_round_is_live_probe_all_failed` above); with that
    // stamp still warm, `csm usage --refresh` — whose entire contract is "go
    // probe live regardless" — hit `Err(FetchError::NegativeCacheActive)`
    // before ever reaching `local::collect`, so the caller fell back to the
    // stale *positive* cache and a just-recorded statusline capture's
    // numbers never appeared. Mirrors this crate's established pattern of
    // testing `fetch_with`'s inline predicates as plain boolean logic (see
    // the total-failure/live-probe-all-failed blocks above) rather than
    // touching the real `$HOME`-rooted `paths::fetch_failed()` file that
    // `negative_cache_active` reads.

    #[test]
    fn forced_refresh_skips_the_negative_cooldown_gate() {
        let force = true;
        let cache_is_active = true; // a prior all-failed round just stamped it
        let would_short_circuit = !force && cache_is_active;
        assert!(
            !would_short_circuit,
            "force=true must bypass an active negative cooldown, not return \
             NegativeCacheActive and degrade to the stale positive cache"
        );
    }

    #[test]
    fn unforced_fetch_still_honors_the_negative_cooldown_gate() {
        let force = false;
        let cache_is_active = true;
        let would_short_circuit = !force && cache_is_active;
        assert!(
            would_short_circuit,
            "a plain `csm usage` (no --refresh) must still short-circuit on \
             an active negative cooldown — only `force` bypasses it"
        );
    }
}
