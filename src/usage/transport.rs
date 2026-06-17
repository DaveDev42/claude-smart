//! Usage fetch transport — hub-local fast path, positive/negative TTL cache,
//! HTTP-first (reqwest blocking), SSH fallback (POSIX only).
//!
//! # Fetch algorithm (spec §2 "Usage transport + caching")
//!
//! 1. **Hub-local fast path** (`hostname == "workstation"` → read
//!    `hub_local_cache()` directly, skip all network).
//! 2. **Positive TTL check**: if `.usage-cache.json` mtime < 60 s → return
//!    cached `UsageData` immediately.
//! 3. **Negative cooldown check**: if `.usage-fetch-failed` mtime < 120 s →
//!    return `Err(FetchError::NegativeCacheActive)`.
//! 4. **HTTP fetch** (reqwest blocking; connect-timeout 1 s / max-time 2 s).
//! 5. **SSH fallback** (`#[cfg(unix)]` only; ControlMaster reuse via `ssh`
//!    shell-out; see `ssh_fetch`).  On Windows, HTTP is the only path.
//! 6. On success: validate JSON, write cache atomically (tmp + rename).
//! 7. On failure: stamp `.usage-fetch-failed`; return `Err`.

use super::model::UsageData;
use super::FetchError;
use crate::paths;

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
    if is_hub_local() {
        return read_hub_local();
    }

    // Step 2 — positive TTL cache (< 60 s).
    if let Some(data) = try_positive_cache()? {
        return Ok(data);
    }

    // Step 3 — negative cooldown (< 120 s).
    if negative_cache_active() {
        return Err(FetchError::NegativeCacheActive);
    }

    // Step 4 — HTTP fetch.
    match http_fetch() {
        Ok(data) => {
            write_positive_cache(&data)?;
            return Ok(data);
        }
        Err(e) => {
            // HTTP failed; try SSH fallback (POSIX only).
            #[cfg(unix)]
            {
                match ssh_fetch() {
                    Ok(data) => {
                        write_positive_cache(&data)?;
                        return Ok(data);
                    }
                    Err(_ssh_err) => {
                        // Both paths failed.
                        stamp_negative_cache();
                        return Err(e);
                    }
                }
            }

            // On Windows there is no SSH fallback.
            #[cfg(not(unix))]
            {
                stamp_negative_cache();
                return Err(e);
            }
        }
    }
}

// ─── hub-local fast path ─────────────────────────────────────────────────────

/// True when running on Workstation itself (the hub machine).
fn is_hub_local() -> bool {
    hostname() == "workstation"
}

/// Read the hub's own `usage-limits.json` (no network needed).
fn read_hub_local() -> Result<UsageData, FetchError> {
    let path = paths::hub_local_cache();
    if !path.exists() {
        return Err(FetchError::EmptyPayload);
    }
    let raw = std::fs::read_to_string(&path)?;
    let data: UsageData = serde_json::from_str(&raw)?;
    Ok(data)
}

/// Return the short hostname (no domain suffix).
fn hostname() -> String {
    // `hostname::get()` is not a dep; use `std::process::Command` or the `nix`
    // crate on POSIX, or `GetComputerNameW` on Windows.  For Phase 0 this is
    // a simple env/uname shell-out stub.
    todo!("hostname(): read gethostname via nix (unix) / GetComputerNameW (windows)")
}

// ─── positive TTL cache ───────────────────────────────────────────────────────

const POSITIVE_TTL_SECS: u64 = 60;

/// Return `Ok(Some(data))` if the cache file is fresh (< `POSITIVE_TTL_SECS`),
/// `Ok(None)` if absent/stale, or `Err` on a parse failure of a fresh file.
fn try_positive_cache() -> Result<Option<UsageData>, FetchError> {
    let path = paths::usage_cache();
    if !path.exists() {
        return Ok(None);
    }

    let age = file_age_secs(&path)?;
    if age >= POSITIVE_TTL_SECS {
        return Ok(None);
    }

    // Fresh — parse and return.
    let raw = std::fs::read_to_string(&path)?;
    let data: UsageData = serde_json::from_str(&raw)?;
    Ok(Some(data))
}

// ─── negative cooldown cache ──────────────────────────────────────────────────

const NEGATIVE_COOLDOWN_SECS: u64 = 120;

/// True if the negative-cooldown file is recent (< `NEGATIVE_COOLDOWN_SECS`).
fn negative_cache_active() -> bool {
    let path = paths::fetch_failed();
    if !path.exists() {
        return false;
    }
    file_age_secs(&path).map(|age| age < NEGATIVE_COOLDOWN_SECS).unwrap_or(false)
}

/// Write (or update) the negative-cooldown sentinel.
fn stamp_negative_cache() {
    let path = paths::fetch_failed();
    // Best-effort; ignore errors (a missing stamp just means one extra fetch).
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(&path, epoch.to_string());
}

// ─── HTTP fetch ───────────────────────────────────────────────────────────────

/// Hub usage endpoint (Tailscale Serve on Workstation).
const HUB_USAGE_URL: &str = "http://workstation.example-tnet.ts.net/cc-usage/api/data/limits";

/// Blocking HTTP fetch with tight timeouts.
fn http_fetch() -> Result<UsageData, FetchError> {
    todo!(
        "http_fetch(): reqwest::blocking::Client with connect_timeout(1s) / timeout(2s) \
         GET {HUB_USAGE_URL}; validate JSON before returning"
    )
}

// ─── SSH fallback (POSIX only) ────────────────────────────────────────────────

/// SSH fallback path — POSIX only (ControlMaster socket reuse).
///
/// This function is compiled only on `cfg(unix)`.  On Windows the HTTP path is
/// the sole transport, per spec §5 #5.
#[cfg(unix)]
fn ssh_fetch() -> Result<UsageData, FetchError> {
    todo!(
        "ssh_fetch(): shell-out to `ssh hub.example-tnet.ts.net \
         cat ~/claude-code-usage/cache/usage-limits.json` \
         (ControlMaster reuse, wrapped in `timeout 3`); \
         parse stdout as JSON"
    )
}

// ─── cache write ─────────────────────────────────────────────────────────────

/// Atomically write `data` to `.usage-cache.json` (tmp + rename).
///
/// Validates that the data re-serializes before writing so a partial write
/// never corrupts the cache.
fn write_positive_cache(data: &UsageData) -> Result<(), FetchError> {
    todo!(
        "write_positive_cache(): serde_json::to_string(data) → write to \
         .usage-cache.json.tmp → std::fs::rename to .usage-cache.json (atomic)"
    )
}

// ─── utility ─────────────────────────────────────────────────────────────────

/// Return the age of `path` in seconds (wall clock now − mtime).
/// Returns `Err(FetchError::Io)` if the metadata cannot be read.
fn file_age_secs(path: &std::path::Path) -> Result<u64, FetchError> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let now = std::time::SystemTime::now();
    Ok(now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0))
}
