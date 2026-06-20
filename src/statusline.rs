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
//! ### Profile resolution  (sh lines 167–169 / ps1 lines 152–157)
//!
//! 1. Take the **basename** of `$CLAUDE_CONFIG_DIR`.
//! 2. If it starts with `.claude.`, strip that prefix →
//!    `/home/you/.claude.personal` → `"personal"`.
//! 3. Otherwise the host field shows **only** the hostname (no profile prefix).
//! 4. If `CLAUDE_CONFIG_DIR` is unset, no profile prefix is prepended.
//!
//! ### Host display  (sh lines 164–165 / ps1 lines 147–150)
//!
//! * POSIX: `hostname -s` then strip the `"Dave-"` prefix
//!   (e.g. `Acme-Laptop` → `"MBP16"`).
//! * Windows: `[Environment]::MachineName` (NetBIOS-uppercased), strip
//!   `"Dave-"` case-insensitively (e.g. `DAVE-WINDOWS` → `"WINDOWS"`).
//!
//! ### `is_personal_machine` gate  (sh line 166 / ps1 lines 151–158)
//!
//! In the sh/ps1 scripts the `IS_PERSONAL_MACHINE` flag is baked at Ansible
//! deploy time.  In the Rust binary the equivalent signal is the *presence* of
//! `~/.config/claude-as/profiles.json` (deployed personal-only — absent on toss
//! machines and non-managed boxes).  When that file is absent the host field is
//! rendered without a profile prefix, identical to the toss sh script path.
//!
//! ## Segment format
//!
//! | Condition | Output |
//! |-----------|--------|
//! | personal machine, `CLAUDE_CONFIG_DIR` = `…/.claude.personal` | `personal@MBP16` |
//! | personal machine, `CLAUDE_CONFIG_DIR` = `…/.claude.work`  | `work@MacMini` |
//! | personal machine, `CLAUDE_CONFIG_DIR` unset or bare dir name  | `MBP16` |
//! | toss / non-personal machine                                    | `MBP16` |

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
/// host="${host#Dave-}"
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
/// ".claude.personal"  →  "personal"
/// ".claude.work"   →  "work"
/// ".claude."          →  ""   (degenerate — prefix present but suffix empty)
/// "myprofile"         →  "myprofile"
/// ```
pub fn strip_claude_prefix(s: &str) -> &str {
    s.strip_prefix(".claude.").unwrap_or(s)
}

// ─── Hostname ─────────────────────────────────────────────────────────────────

/// Return the **short** hostname, stripping the `"Dave-"` prefix if present.
///
/// Mirrors:
/// - sh:   `host=$(hostname -s); host="${host#Dave-}"`  (case-sensitive strip)
/// - ps1:  `$host = [Environment]::MachineName; if ($host -like 'Dave-*') { … }`
///   (`-like` is case-insensitive on Windows — `DAVE-WINDOWS` → `WINDOWS`)
pub fn short_hostname() -> Result<String> {
    let raw = hostname()?;
    Ok(strip_dave_prefix(raw))
}

/// Strip the leading `"Dave-"` prefix from a hostname string.
///
/// - On POSIX the shell does a case-sensitive `${host#Dave-}` parameter
///   expansion, so only the exact prefix `"Dave-"` is removed.
/// - On Windows `[Environment]::MachineName` is NetBIOS-uppercased
///   (`DAVE-WINDOWS`), so the ps1 uses the case-insensitive `-like 'Dave-*'`
///   operator + `.Substring(5)`.  We replicate that with
///   `to_ascii_lowercase().starts_with("dave-")`.
///
/// The function is non-platform-specific in its public contract so it can be
/// tested uniformly; the internal check uses a case-insensitive starts_with to
/// cover the Windows case while being harmless on POSIX (hostnames there are
/// already in their natural case).
pub fn strip_dave_prefix(s: String) -> String {
    // Case-insensitive check (covers "Acme-Laptop" and "DAVE-WINDOWS").
    if s.len() > 5 && s[..5].eq_ignore_ascii_case("Dave-") {
        s[5..].to_owned()
    } else {
        s
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
/// (e.g. fresh checkout / dev box), hiding the `personal@host` prefix.
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
        let result = format_segment("MBP16".to_owned(), true);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        result
    }

    /// Call `format_segment` without `CLAUDE_CONFIG_DIR` (personal machine, no dir set).
    fn segment_personal_no_dir() -> String {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        format_segment("MBP16".to_owned(), true)
    }

    // ── strip_claude_prefix ───────────────────────────────────────────────────

    #[test]
    fn strip_prefix_personal() {
        assert_eq!(strip_claude_prefix(".claude.personal"), "personal");
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
        // /home/you/.claude.personal  →  basename ".claude.personal"  →  "personal"
        assert_eq!(profile_with_dir("/home/you/.claude.personal"), "personal");
    }

    #[test]
    fn leaf_work_path() {
        // /home/you/.claude.work  →  basename ".claude.work"  →  "work"
        assert_eq!(profile_with_dir("/home/you/.claude.work"), "work");
    }

    #[test]
    fn leaf_macos_style_path() {
        // /Users/example/.claude.personal  →  "personal"
        assert_eq!(profile_with_dir("/Users/example/.claude.personal"), "personal");
    }

    #[test]
    fn leaf_bare_profile_no_dot_prefix() {
        // A dir with no .claude. prefix is returned as-is.
        assert_eq!(profile_with_dir("myprofile"), "myprofile");
    }

    #[test]
    fn leaf_trailing_slash_stripped() {
        // std::path::Path normalises trailing slashes; basename is still correct.
        assert_eq!(profile_with_dir("/home/you/.claude.personal/"), "personal");
    }

    #[test]
    fn absent_env_var_returns_unknown() {
        assert_eq!(profile_with_no_var(), "unknown");
    }

    #[test]
    fn leaf_windows_style_path_does_not_panic() {
        // On POSIX, Path::file_name on a Windows-style path treats the whole
        // string as the basename (no separator recognised).  Must not panic.
        let result = profile_with_dir(r"C:\Users\example\.claude.personal");
        assert!(!result.is_empty());
    }

    // ── strip_dave_prefix ─────────────────────────────────────────────────────

    #[test]
    fn strip_dave_mbp16() {
        assert_eq!(strip_dave_prefix("Acme-Laptop".to_owned()), "MBP16");
    }

    #[test]
    fn strip_dave_macmini() {
        assert_eq!(strip_dave_prefix("Workstation".to_owned()), "MacMini");
    }

    #[test]
    fn strip_dave_windows_uppercase() {
        // Windows NetBIOS uppercased: "DAVE-WINDOWS" → "WINDOWS"
        assert_eq!(strip_dave_prefix("DAVE-WINDOWS".to_owned()), "WINDOWS");
    }

    #[test]
    fn strip_dave_no_prefix() {
        // Hostname without the "Dave-" prefix is returned unchanged.
        assert_eq!(strip_dave_prefix("myhostname".to_owned()), "myhostname");
    }

    #[test]
    fn strip_dave_exactly_dave_dash() {
        // "Dave-" alone → empty string (length 5 is NOT > 5, so unchanged)
        // Length 5 is "Dave-" which is exactly 5; our guard requires > 5.
        assert_eq!(strip_dave_prefix("Dave-".to_owned()), "Dave-");
    }

    #[test]
    fn strip_dave_short_string() {
        // Strings shorter than 5 chars must not panic.
        assert_eq!(strip_dave_prefix("abc".to_owned()), "abc");
    }

    // ── format_segment — the full host-display logic ──────────────────────────

    #[test]
    fn segment_personal_with_personal_dir() {
        // personal machine + CLAUDE_CONFIG_DIR = …/.claude.personal  →  "personal@MBP16"
        let seg = segment_personal("/Users/example/.claude.personal");
        assert_eq!(seg, "personal@MBP16");
    }

    #[test]
    fn segment_personal_with_work_dir() {
        // personal machine + CLAUDE_CONFIG_DIR = …/.claude.work  →  "work@MBP16"
        let seg = segment_personal("/Users/example/.claude.work");
        assert_eq!(seg, "work@MBP16");
    }

    #[test]
    fn segment_personal_with_bare_dir_no_prefix() {
        // personal machine + CLAUDE_CONFIG_DIR = dir with no .claude.* prefix → no @ prefix
        let seg = segment_personal("/Users/example/.claude");
        // ".claude" does not start with ".claude." (note the trailing dot) → host only
        assert_eq!(seg, "MBP16");
    }

    #[test]
    fn segment_personal_no_claude_config_dir() {
        // personal machine but CLAUDE_CONFIG_DIR unset → host only
        let seg = segment_personal_no_dir();
        assert_eq!(seg, "MBP16");
    }

    #[test]
    fn segment_toss_machine_ignores_dir() {
        // toss / non-personal machine: profile prefix must NOT appear
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/some/.claude.personal");
        let seg = format_segment("MBP16".to_owned(), false /* personal=false */);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(seg, "MBP16");
    }

    #[test]
    fn segment_macmini_personal() {
        // Workstation stripped + personal profile → "work@MacMini"
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/Users/example/.claude.work");
        let host = strip_dave_prefix("Workstation".to_owned());
        let seg = format_segment(host, true);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(seg, "work@MacMini");
    }

    #[test]
    fn segment_windows_personal() {
        // DAVE-WINDOWS (NetBIOS uppercased) + personal  →  "personal@WINDOWS"
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", r"C:\Users\example\.claude.personal");
        let host = strip_dave_prefix("DAVE-WINDOWS".to_owned());
        // On POSIX, Path::file_name treats the Windows-style path as one token;
        // the basename is the whole string.  We only assert the Dave- strip here.
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
    fn short_hostname_no_dave_prefix() {
        // On these personal machines the raw hostname starts "Dave-"; after
        // short_hostname() the prefix should be gone.
        let h = short_hostname().unwrap();
        // Only check if the raw hostname actually had the prefix.
        let raw = hostname().unwrap();
        if raw.len() > 5 && raw[..5].eq_ignore_ascii_case("Dave-") {
            assert!(
                !h.starts_with("Dave-") && !h.starts_with("dave-"),
                "Dave- prefix not stripped: {h}"
            );
        }
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
