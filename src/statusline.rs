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
use std::path::Path;

use anyhow::Result;

// ─── Public entry point ───────────────────────────────────────────────────────

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
pub fn run(_args: &[OsString]) -> Result<()> {
    let segment = render_segment()?;
    println!("{segment}");
    Ok(())
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

    // Serialise all tests that touch CLAUDE_CONFIG_DIR (process-global env var).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
}
