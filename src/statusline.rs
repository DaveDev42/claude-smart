//! `csm statusline` — print `<profile>@<host>` for shell prompt integration.
//!
//! Ships **DORMANT (N5)**: `csm statusline` is built but the `settings.json`
//! `statuslineCommand` entry still points at `statusline-command.sh` until the
//! explicit cutover described in §7 of the design spec.
//!
//! ## Output contract (reproduced from `statusline-command.sh.j2` lines 159–171
//! and `statusline-command.ps1.j2` lines 141–157)
//!
//! The `<profile>@<host>` segment is the **host field** that appears first in
//! the rendered status line; `run()` emits this segment alone (with a trailing
//! newline) for the dormant / testing use-case.
//!
//! ## Piggyback statusLine capture (local usage collection)
//!
//! Claude Code's own `statusLine` command runs with the full statusLine JSON
//! (which carries `rate_limits` for the *active* profile, refreshed roughly
//! once a second) on stdin — a shell prompt or manual invocation, by contrast,
//! inherits a TTY stdin with nothing on it. `run()` tells the two apart with
//! `stdin().is_terminal()`: when stdin is NOT a terminal (the statusLine case),
//! it reads stdin (capped, see [`CAPTURE_STDIN_CAP_BYTES`]) and feeds it to
//! [`usage::local::record_statusline_payload`] best-effort — every error is
//! swallowed, since a malformed/partial payload must never turn the segment
//! this command exists to print into an error. `CSM_STATUSLINE_NO_CAPTURE=1`
//! (or `true`) disables the read entirely, e.g. for a caller that pipes
//! something else into a piped, non-interactive `csm statusline` and does not
//! want its stdin consumed for capture.
//!
//! ### Profile resolution
//!
//! 1. Take the **basename** of `$CLAUDE_CONFIG_DIR`.
//! 2. If it starts with `.claude.`, strip that prefix →
//!    `/home/you/.claude.work` → `"work"`.
//! 3. Otherwise the host field shows **only** the hostname (no profile prefix).
//! 4. If `CLAUDE_CONFIG_DIR` is unset, no profile prefix is prepended.
//!
//! ### Host display
//!
//! The short hostname (first DNS label) is read at runtime and optionally
//! rewritten by the `CSM_HOST_REPLACE` rule (see [`apply_host_replace`]) — the
//! binary hardcodes no naming convention. With `CSM_HOST_REPLACE=Acme-/` a host
//! `Acme-Laptop` renders as `Laptop`; with no rule it renders verbatim.
//!
//! ### Personal-machine gate
//!
//! The host field gains a profile prefix only when a profile registry is
//! present — the *presence* of `~/.config/claude-as/profiles.json`. When that
//! file is absent the host field is rendered without a profile prefix.
//!
//! ## Segment format
//!
//! | Condition | Output |
//! |-----------|--------|
//! | registry present, `CLAUDE_CONFIG_DIR` = `…/.claude.home` | `home@Laptop` |
//! | registry present, `CLAUDE_CONFIG_DIR` = `…/.claude.work` | `work@Laptop` |
//! | registry present, `CLAUDE_CONFIG_DIR` unset or bare dir name | `Laptop` |
//! | no registry | `Laptop` |

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;

use crate::usage;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Hard cap on how much of a piggybacked statusLine stdin payload `run()` will
/// ever read — mirrors `main::CAPTURE_STDIN_CAP_BYTES`; kept as a separate
/// constant since `csm usage capture` and `csm statusline` are different
/// entry points that happen to share the same defensive ceiling.
const CAPTURE_STDIN_CAP_BYTES: u64 = 256 * 1024;

/// Subcommand handler: print `<profile>@<host>` (or just `<host>`) to stdout.
///
/// Mirrors the host-display block from `statusline-command.sh.j2` lines 164–171:
///
/// ```sh
/// host=$(hostname -s)
/// host="${host#Acme-}"
/// if [ "$IS_PERSONAL_MACHINE" = "1" ] && [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then
///   profile=$(basename "$CLAUDE_CONFIG_DIR")
///   case "$profile" in
///     .claude.*) host="${profile#.claude.}@${host}" ;;
///   esac
/// fi
/// ```
///
/// Also piggybacks the statusLine-stdin usage capture (see the module doc)
/// before rendering: when stdin is not a terminal and the capture gate is not
/// disabled, stdin is read and handed to
/// [`usage::local::record_statusline_payload`], best-effort. This never
/// affects the printed segment or the exit code — a capture failure is
/// invisible to whatever renders this command's output in the prompt.
pub fn run(_args: &[OsString]) -> Result<()> {
    if should_capture_stdin() {
        let raw = read_stdin_capped(CAPTURE_STDIN_CAP_BYTES);
        let _ = usage::local::record_statusline_payload(&raw);
    }
    let segment = render_segment()?;
    println!("{segment}");
    Ok(())
}

/// `true` when `run()` should read stdin for the piggyback capture: stdin is
/// not a terminal (a real TTY-backed shell-prompt invocation never has a
/// payload to read) AND `CSM_STATUSLINE_NO_CAPTURE` is not `1`/`true`.
///
/// Split from `run()` so the env-gate logic is testable without needing to
/// control the process's actual stdin.
fn should_capture_stdin() -> bool {
    if std::io::stdin().is_terminal() {
        return false;
    }
    !capture_disabled_by_env()
}

/// `true` iff `CSM_STATUSLINE_NO_CAPTURE` is set to `1` or `true` (any case).
fn capture_disabled_by_env() -> bool {
    match std::env::var("CSM_STATUSLINE_NO_CAPTURE") {
        Ok(v) => matches!(v.trim(), "1" | "true" | "True" | "TRUE"),
        Err(_) => false,
    }
}

/// Hard deadline on the piggybacked stdin read (see [`read_stdin_capped`]).
/// The statusLine case (this command's actual reason to read stdin at all)
/// writes its payload and closes stdin essentially immediately; this is
/// generous for that case while still bounding the failure mode below.
const CAPTURE_STDIN_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

/// Read stdin up to `max_bytes`, lossily decoding as UTF-8, bounded by
/// [`CAPTURE_STDIN_DEADLINE`] as well as `max_bytes`.
///
/// The byte cap alone does not bound *time*: `should_capture_stdin`'s gate is
/// "stdin is not a terminal", which is broader than "invoked as a statusLine
/// command" — a caller that runs `csm statusline` with an inherited pipe it
/// does not itself own (e.g. `producer | while read -r line; do prompt="$(csm
/// statusline)"; done`) hands this a stdin that may deliver under `max_bytes`
/// and never close, and a plain `read_to_end` would then block forever,
/// hanging the caller's loop. The read runs on a background thread instead;
/// this function waits only up to the deadline and returns whatever arrived
/// (empty string on timeout — `record_statusline_payload` treats that as a
/// no-op, same as any other unusable payload). The spawned thread is not
/// joined on timeout — nothing here can force a blocking `Read` to return
/// early — but it is harmless to leak until the process exits shortly after
/// this call returns (`run()` prints the segment next and returns).
///
/// Mirrors `main::read_stdin_capped`'s byte-cap contract; duplicated rather
/// than shared because `main` is a binary crate root, not a library other
/// modules import from.
fn read_stdin_capped(max_bytes: u64) -> String {
    use std::io::Read;
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::stdin().take(max_bytes).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(CAPTURE_STDIN_DEADLINE) {
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => String::new(),
    }
}

/// Compute the `<profile>@<host>` (or bare `<host>`) segment.
///
/// Separated from `run()` so tests can call it without spawning a process.
pub fn render_segment() -> Result<String> {
    let host = short_hostname()?;
    let segment = format_segment(host, is_personal_machine());
    Ok(segment)
}

/// Build the host segment from a pre-computed short hostname.
///
/// Factored out so tests can inject both the hostname and the personal-flag
/// without touching process environment or the filesystem.
///
/// Logic (mirrors sh lines 166–171):
/// - If `personal` is false → return `host` as-is.
/// - Read `CLAUDE_CONFIG_DIR`; if absent → return `host`.
/// - Take basename; if it starts with `.claude.` → `"{label}@{host}"`.
/// - Otherwise → `host`.
pub fn format_segment(host: String, personal: bool) -> String {
    if !personal {
        return host;
    }
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        None => host,
        Some(dir) => {
            let base = Path::new(&dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Only a `.claude.<profile>` dir gets a profile label; any other dir
            // shows the bare host. `strip_claude_prefix` returns the input unchanged
            // when the prefix is absent, so detect that case to preserve the
            // label-only-for-managed-profiles behavior.
            let label = strip_claude_prefix(&base);
            if label != base {
                format!("{label}@{host}")
            } else {
                host
            }
        }
    }
}

// ─── Profile resolution ───────────────────────────────────────────────────────

/// Derive the profile label from `$CLAUDE_CONFIG_DIR`.
///
/// Rules (mirrors `statusline-command.sh.j2` lines 167–169):
/// - Absent env var → `"unknown"`.
/// - Take `basename($CLAUDE_CONFIG_DIR)`.
/// - If the basename starts with `.claude.`, strip that prefix.
/// - Otherwise return the raw basename.
///
/// Tested helper; `format_segment` is the live statusline path. Reserved for a
/// future profile-name display (e.g. `csm cas` status) that wants the raw label
/// without the `@host` suffix.
#[allow(dead_code)]
pub fn current_profile() -> Result<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR");
    match dir {
        None => Ok("unknown".to_owned()),
        Some(path) => {
            let p = Path::new(&path);
            match p.file_name() {
                Some(name) => {
                    let base = name.to_string_lossy();
                    let label = strip_claude_prefix(&base);
                    Ok(label.to_owned())
                }
                None => {
                    // Path ends in a root or is somehow empty; fall back to
                    // the raw string rather than erroring (defensive).
                    Ok(path.to_string_lossy().into_owned())
                }
            }
        }
    }
}

/// Strip the `.claude.` prefix if present, otherwise return `s` as-is.
///
/// Examples:
/// ```text
/// ".claude.home"  →  "home"
/// ".claude.work"   →  "work"
/// ".claude."          →  ""   (degenerate — prefix present but suffix empty)
/// "myprofile"         →  "myprofile"
/// ```
pub fn strip_claude_prefix(s: &str) -> &str {
    s.strip_prefix(".claude.").unwrap_or(s)
}

// ─── Hostname ─────────────────────────────────────────────────────────────────

/// Environment variable carrying an optional hostname-rewrite rule, so the
/// binary never hardcodes any site-specific naming convention. Format is a
/// single `find/replace` pair (literal, case-insensitive find, first match):
/// e.g. `Acme-/` turns `Acme-Laptop` into `Laptop`. Unset/empty → no rewrite.
pub const HOST_REPLACE_ENV: &str = "CSM_HOST_REPLACE";

/// Return the **short** hostname, applying the optional `CSM_HOST_REPLACE`
/// rewrite if one is configured. With no rule set, the raw short hostname is
/// returned unchanged — the binary carries no built-in naming convention.
pub fn short_hostname() -> Result<String> {
    let raw = hostname()?;
    Ok(apply_host_replace(
        raw,
        std::env::var(HOST_REPLACE_ENV).ok().as_deref(),
    ))
}

/// Apply a `find/replace` rewrite rule to a hostname.
///
/// `rule` is `Some("find/replace")` (e.g. `"Acme-/"` to drop an `Acme-`
/// prefix); the find is matched case-insensitively at the first occurrence and
/// replaced literally. `None`, an empty rule, or a rule without a `/` separator
/// leaves the hostname untouched. This keeps every site-specific convention out
/// of the binary — the rule is injected from the environment (the
/// dave-environment deployment sets `CSM_HOST_REPLACE=Acme-/`).
pub fn apply_host_replace(s: String, rule: Option<&str>) -> String {
    let Some(rule) = rule.filter(|r| !r.is_empty()) else {
        return s;
    };
    let Some((find, replace)) = rule.split_once('/') else {
        return s;
    };
    if find.is_empty() {
        return s;
    }
    // Case-insensitive search for the first occurrence of `find`.
    let lower_s = s.to_ascii_lowercase();
    let lower_find = find.to_ascii_lowercase();
    match lower_s.find(&lower_find) {
        Some(idx) => format!("{}{}{}", &s[..idx], replace, &s[idx + find.len()..]),
        None => s,
    }
}

/// Return the system short hostname (first DNS label; no domain suffix).
pub fn hostname() -> Result<String> {
    hostname_impl()
}

#[cfg(unix)]
fn hostname_impl() -> Result<String> {
    use nix::unistd::gethostname;
    let name = gethostname()?;
    let raw = name.to_string_lossy();
    // Strip FQDN suffix (keep first label only), matching `hostname -s`.
    let short = raw.split('.').next().unwrap_or(&raw);
    Ok(short.to_owned())
}

#[cfg(windows)]
fn hostname_impl() -> Result<String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::{ComputerNameNetBIOS, GetComputerNameExW};

    // First call: pass null + &mut size to obtain required buffer length.
    let mut size: u32 = 0;
    // SAFETY: querying buffer size with null pointer — documented Windows API pattern.
    unsafe { GetComputerNameExW(ComputerNameNetBIOS, std::ptr::null_mut(), &mut size) };

    let mut buf: Vec<u16> = vec![0u16; size as usize];
    let ok = unsafe { GetComputerNameExW(ComputerNameNetBIOS, buf.as_mut_ptr(), &mut size) };
    if ok == 0 {
        anyhow::bail!("GetComputerNameExW failed");
    }
    buf.truncate(size as usize);
    Ok(OsString::from_wide(&buf).to_string_lossy().into_owned())
}

// ─── Personal machine detection ───────────────────────────────────────────────

/// Return `true` if this appears to be a personal machine.
///
/// The sh/ps1 scripts bake `IS_PERSONAL_MACHINE` at Ansible deploy time. A single
/// cross-platform binary cannot bake a compile-time constant, so the signal is,
/// in priority order:
///
/// 1. The `IS_PERSONAL_MACHINE` env var, if set to `1`/`true`/`0`/`false`
///    (the deploy may still export it via settings.json env — honour it first,
///    matching the sh script's source-of-truth exactly).
/// 2. Otherwise, **delegate to the `.claude.` match itself**: on a toss/non-managed
///    box `CLAUDE_CONFIG_DIR` is never a `.claude.<profile>` path (toss uses bare
///    `~/.claude` or leaves it unset), so `format_segment`'s prefix match already
///    encodes the gate. Returning `true` here is safe — the profile prefix only
///    appears when the dir genuinely is `.claude.<profile>`.
///
/// The old `profiles.json`-presence heuristic was dropped: it produced a false
/// negative whenever the binary ran before ansible had deployed that file
/// (e.g. fresh checkout / dev box), hiding the `home@host` prefix.
pub fn is_personal_machine() -> bool {
    match std::env::var("IS_PERSONAL_MACHINE") {
        Ok(v) => matches!(v.trim(), "1" | "true" | "True" | "TRUE" | "yes"),
        // Unset → delegate to the .claude.<profile> match in format_segment.
        Err(_) => true,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialise all tests that touch CLAUDE_CONFIG_DIR (process-global env
    // var) — shared across module boundaries with `usage::local`'s own
    // CLAUDE_CONFIG_DIR-mutating tests, since a module-local lock cannot
    // protect against a different module's test interleaving on the same
    // process-global variable. See `crate::testenv` for why.
    use crate::testenv::CLAUDE_CONFIG_DIR_ENV_LOCK as ENV_LOCK;

    /// Set `CLAUDE_CONFIG_DIR`, call `current_profile()`, then restore original.
    fn profile_with_dir(dir: &str) -> String {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
        let result = current_profile().expect("current_profile() must not fail");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        result
    }

    /// Call `current_profile()` with `CLAUDE_CONFIG_DIR` absent.
    fn profile_with_no_var() -> String {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        current_profile().expect("current_profile() must not fail when var is absent")
    }

    /// Call `format_segment` with a specific `CLAUDE_CONFIG_DIR` (personal machine).
    fn segment_personal(dir: &str) -> String {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
        let result = format_segment("Laptop".to_owned(), true);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        result
    }

    /// Call `format_segment` without `CLAUDE_CONFIG_DIR` (personal machine, no dir set).
    fn segment_personal_no_dir() -> String {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        format_segment("Laptop".to_owned(), true)
    }

    // ── strip_claude_prefix ───────────────────────────────────────────────────

    #[test]
    fn strip_prefix_personal() {
        assert_eq!(strip_claude_prefix(".claude.home"), "home");
    }

    #[test]
    fn strip_prefix_work() {
        assert_eq!(strip_claude_prefix(".claude.work"), "work");
    }

    #[test]
    fn strip_prefix_no_prefix() {
        assert_eq!(strip_claude_prefix("myprofile"), "myprofile");
    }

    #[test]
    fn strip_prefix_degenerate_empty_suffix() {
        // ".claude." → strip gives "" (degenerate but should not panic)
        assert_eq!(strip_claude_prefix(".claude."), "");
    }

    #[test]
    fn strip_prefix_bare_claude_no_dot() {
        // ".claude" (no trailing dot) → no strip
        assert_eq!(strip_claude_prefix(".claude"), ".claude");
    }

    // ── current_profile — env var extraction ─────────────────────────────────

    #[test]
    fn leaf_personal_path() {
        // /home/you/.claude.home  →  basename ".claude.home"  →  "home"
        assert_eq!(profile_with_dir("/home/you/.claude.home"), "home");
    }

    #[test]
    fn leaf_work_path() {
        // /home/you/.claude.work  →  basename ".claude.work"  →  "work"
        assert_eq!(profile_with_dir("/home/you/.claude.work"), "work");
    }

    #[test]
    fn leaf_macos_style_path() {
        // /Users/example/.claude.home  →  "home"
        assert_eq!(profile_with_dir("/Users/example/.claude.home"), "home");
    }

    #[test]
    fn leaf_bare_profile_no_dot_prefix() {
        // A dir with no .claude. prefix is returned as-is.
        assert_eq!(profile_with_dir("myprofile"), "myprofile");
    }

    #[test]
    fn leaf_trailing_slash_stripped() {
        // std::path::Path normalises trailing slashes; basename is still correct.
        assert_eq!(profile_with_dir("/home/you/.claude.home/"), "home");
    }

    #[test]
    fn absent_env_var_returns_unknown() {
        assert_eq!(profile_with_no_var(), "unknown");
    }

    #[test]
    fn leaf_windows_style_path_does_not_panic() {
        // On POSIX, Path::file_name on a Windows-style path treats the whole
        // string as the basename (no separator recognised).  Must not panic.
        let result = profile_with_dir(r"C:\Users\example\.claude.home");
        assert!(!result.is_empty());
    }

    // ── apply_host_replace (CSM_HOST_REPLACE rewrite rule) ────────────────────

    const PREFIX_RULE: Option<&str> = Some("Acme-/");

    #[test]
    fn replace_prefix_basic() {
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), PREFIX_RULE),
            "Laptop"
        );
    }

    #[test]
    fn replace_prefix_workstation() {
        assert_eq!(
            apply_host_replace("Acme-Workstation".to_owned(), PREFIX_RULE),
            "Workstation"
        );
    }

    #[test]
    fn replace_prefix_case_insensitive() {
        // Windows NetBIOS uppercased: "ACME-WINDOWS" still matches "Acme-/".
        assert_eq!(
            apply_host_replace("ACME-WINDOWS".to_owned(), PREFIX_RULE),
            "WINDOWS"
        );
    }

    #[test]
    fn replace_no_match_unchanged() {
        // Hostname without the configured prefix is returned unchanged.
        assert_eq!(
            apply_host_replace("myhostname".to_owned(), PREFIX_RULE),
            "myhostname"
        );
    }

    #[test]
    fn replace_no_rule_unchanged() {
        // With no rule (None / empty) the hostname is never rewritten.
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), None),
            "Acme-Laptop"
        );
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), Some("")),
            "Acme-Laptop"
        );
    }

    #[test]
    fn replace_short_string_no_panic() {
        // Strings shorter than the find pattern must not panic.
        assert_eq!(apply_host_replace("abc".to_owned(), PREFIX_RULE), "abc");
    }

    #[test]
    fn replace_malformed_rule_unchanged() {
        // A rule without a '/' separator is ignored.
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), Some("noseparator")),
            "Acme-Laptop"
        );
    }

    // ── format_segment — the full host-display logic ──────────────────────────

    #[test]
    fn segment_personal_with_home_dir() {
        // personal machine + CLAUDE_CONFIG_DIR = …/.claude.home  →  "home@Laptop"
        let seg = segment_personal("/Users/example/.claude.home");
        assert_eq!(seg, "home@Laptop");
    }

    #[test]
    fn segment_personal_with_work_dir() {
        // personal machine + CLAUDE_CONFIG_DIR = …/.claude.work  →  "work@Laptop"
        let seg = segment_personal("/Users/example/.claude.work");
        assert_eq!(seg, "work@Laptop");
    }

    #[test]
    fn segment_personal_with_bare_dir_no_prefix() {
        // personal machine + CLAUDE_CONFIG_DIR = dir with no .claude.* prefix → no @ prefix
        let seg = segment_personal("/Users/example/.claude");
        // ".claude" does not start with ".claude." (note the trailing dot) → host only
        assert_eq!(seg, "Laptop");
    }

    #[test]
    fn segment_personal_no_claude_config_dir() {
        // personal machine but CLAUDE_CONFIG_DIR unset → host only
        let seg = segment_personal_no_dir();
        assert_eq!(seg, "Laptop");
    }

    #[test]
    fn segment_toss_machine_ignores_dir() {
        // toss / non-personal machine: profile prefix must NOT appear
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/some/.claude.home");
        let seg = format_segment("Laptop".to_owned(), false /* personal=false */);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(seg, "Laptop");
    }

    #[test]
    fn segment_workstation_personal() {
        // Acme- prefix stripped + work profile → "work@Workstation"
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/Users/example/.claude.work");
        let host = apply_host_replace("Acme-Workstation".to_owned(), Some("Acme-/"));
        let seg = format_segment(host, true);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(seg, "work@Workstation");
    }

    #[test]
    fn segment_windows_personal() {
        // ACME-WINDOWS (NetBIOS uppercased) + personal  →  "home@WINDOWS"
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", r"C:\Users\example\.claude.home");
        let host = apply_host_replace("ACME-WINDOWS".to_owned(), Some("Acme-/"));
        // On POSIX, Path::file_name treats the Windows-style path as one token;
        // the basename is the whole string.  We only assert the Acme- strip here.
        let seg = format_segment(host, true);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        // On POSIX the Windows-path basename won't match .claude.* after
        // file_name(), so the host-only branch fires — just verify no panic.
        assert!(!seg.is_empty());
    }

    // ── short_hostname ────────────────────────────────────────────────────────

    #[test]
    fn short_hostname_returns_nonempty() {
        let h = short_hostname().expect("short_hostname() must not error");
        assert!(!h.is_empty());
    }

    #[test]
    fn short_hostname_no_domain_suffix() {
        let h = short_hostname().unwrap();
        assert!(
            !h.contains('.') || h.starts_with('.'),
            "short hostname should have no interior FQDN dot: {h}"
        );
    }

    #[test]
    fn short_hostname_applies_env_rule() {
        // short_hostname() honors CSM_HOST_REPLACE: with a rule matching the
        // raw hostname's first label, that prefix is rewritten away. The binary
        // hardcodes no convention — the rule comes entirely from the env.
        let _guard = ENV_LOCK.lock().unwrap();
        let raw = hostname().unwrap();
        // Build a rule that strips the raw hostname's leading char as a prefix,
        // so the assertion holds on any host without depending on a real name.
        if let Some(first) = raw.chars().next() {
            std::env::set_var(HOST_REPLACE_ENV, format!("{first}/"));
            let h = short_hostname().unwrap();
            std::env::remove_var(HOST_REPLACE_ENV);
            // The first occurrence of `first` is removed → result is shorter or
            // equal, and never starts with that exact char at index 0 unless it
            // repeated. Just assert the rewrite ran (length strictly decreased).
            assert!(
                h.len() < raw.len(),
                "rule should have removed one char: {raw} → {h}"
            );
        }
    }

    #[test]
    fn short_hostname_no_rule_is_raw() {
        // With no CSM_HOST_REPLACE set, short_hostname() == raw short hostname.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(HOST_REPLACE_ENV);
        let h = short_hostname().unwrap();
        let raw = hostname().unwrap();
        assert_eq!(h, raw, "no rule → hostname unchanged");
    }

    // ── hostname (raw) ────────────────────────────────────────────────────────

    #[test]
    fn hostname_returns_nonempty_string() {
        let h = hostname().expect("hostname() must not error");
        assert!(!h.is_empty(), "hostname must be non-empty");
        assert!(
            !h.contains('\n'),
            "hostname must not contain a newline, got {h:?}"
        );
    }

    #[test]
    fn hostname_no_domain_suffix() {
        let h = hostname().unwrap();
        assert!(!h.starts_with('.'), "hostname must not start with a dot");
        assert!(!h.ends_with('.'), "hostname must not end with a dot");
        if let Some(first) = h.split('.').next() {
            assert!(!first.is_empty(), "first hostname label must be non-empty");
        }
    }

    // ── run() smoke ───────────────────────────────────────────────────────────

    #[test]
    fn run_does_not_panic_or_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/.claude.test");
        let result = run(&[]);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert!(
            result.is_ok(),
            "run() returned Err: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn render_segment_returns_nonempty() {
        let seg = render_segment().expect("render_segment must not error");
        assert!(!seg.is_empty());
    }

    // ── is_personal_machine env gate ──────────────────────────────────────────

    #[test]
    fn personal_gate_env_explicit_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("IS_PERSONAL_MACHINE", "0");
        assert!(!is_personal_machine(), "IS_PERSONAL_MACHINE=0 → false");
        std::env::set_var("IS_PERSONAL_MACHINE", "false");
        assert!(!is_personal_machine(), "IS_PERSONAL_MACHINE=false → false");
        std::env::remove_var("IS_PERSONAL_MACHINE");
    }

    #[test]
    fn personal_gate_env_explicit_true() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("IS_PERSONAL_MACHINE", "1");
        assert!(is_personal_machine(), "IS_PERSONAL_MACHINE=1 → true");
        std::env::remove_var("IS_PERSONAL_MACHINE");
    }

    #[test]
    fn personal_gate_unset_delegates_true() {
        // Unset → delegate to the .claude. match (returns true; the prefix match
        // in format_segment is the real gate). This is the fix for the
        // profiles.json-absent false-negative.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("IS_PERSONAL_MACHINE");
        assert!(is_personal_machine());
    }

    // ── capture_disabled_by_env (CSM_STATUSLINE_NO_CAPTURE gate) ─────────────

    static CAPTURE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn capture_disabled_by_env_unset_is_false() {
        let _guard = CAPTURE_ENV_LOCK.lock().unwrap();
        std::env::remove_var("CSM_STATUSLINE_NO_CAPTURE");
        assert!(!capture_disabled_by_env());
    }

    #[test]
    fn capture_disabled_by_env_recognizes_1_and_true() {
        let _guard = CAPTURE_ENV_LOCK.lock().unwrap();
        for v in ["1", "true", "True", "TRUE"] {
            std::env::set_var("CSM_STATUSLINE_NO_CAPTURE", v);
            assert!(capture_disabled_by_env(), "{v} should disable capture");
        }
        std::env::remove_var("CSM_STATUSLINE_NO_CAPTURE");
    }

    #[test]
    fn capture_disabled_by_env_rejects_other_values() {
        let _guard = CAPTURE_ENV_LOCK.lock().unwrap();
        for v in ["0", "false", "yes", ""] {
            std::env::set_var("CSM_STATUSLINE_NO_CAPTURE", v);
            assert!(!capture_disabled_by_env(), "{v} should not disable capture");
        }
        std::env::remove_var("CSM_STATUSLINE_NO_CAPTURE");
    }
}
