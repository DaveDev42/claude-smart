//! `csm statusline` — print `<profile>@<host>` for shell prompt integration.
//!
//! Profile is derived from `$CLAUDE_CONFIG_DIR` using the same logic as the
//! existing `statusline-command.sh.j2`:
//!
//! 1. Take the **basename** (last path component) of `CLAUDE_CONFIG_DIR`.
//! 2. If it starts with `.claude.`, strip that prefix → e.g.
//!    `/home/you/.claude.personal` → `"personal"`.
//! 3. Otherwise use the raw basename as-is.
//! 4. If `CLAUDE_CONFIG_DIR` is unset, return `"unknown"`.
//!
//! Host is the system short hostname:
//! - POSIX: `gethostname(2)` via `nix::unistd::gethostname`, first label only
//!   (strips FQDN suffix).
//! - Windows: `GetComputerNameExW(ComputerNameNetBIOS, …)` via `windows-sys`.
//!
//! Ships DORMANT (N5): `csm statusline` is built but the settings.json
//! `statuslineCommand` entry still points at `statusline-command.sh` until the
//! explicit cutover described in §7 of the design spec.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{bail, Result};

// ─── Public entry point ──────────────────────────────────────────────────────

/// Subcommand handler: print `<profile>@<host>` to stdout.
pub fn run(_args: &[OsString]) -> Result<()> {
    let profile = current_profile()?;
    let host = hostname()?;
    println!("{profile}@{host}");
    Ok(())
}

// ─── Profile resolution ──────────────────────────────────────────────────────

/// Derive the profile label from `$CLAUDE_CONFIG_DIR`.
///
/// Rules (mirrors `statusline-command.sh.j2` lines 167–169):
/// - Absent env var → `"unknown"`.
/// - Take `basename($CLAUDE_CONFIG_DIR)`.
/// - If the basename starts with `.claude.`, strip that prefix.
/// - Otherwise return the raw basename.
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
                    // Path ends in a root or is somehow empty.
                    bail!(
                        "CLAUDE_CONFIG_DIR={:?} has no file-name component",
                        path
                    )
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
/// "myprofile"         →  "myprofile"
/// ".claude."          →  ""   (degenerate — prefix present but suffix empty)
/// ```
pub fn strip_claude_prefix(s: &str) -> &str {
    s.strip_prefix(".claude.").unwrap_or(s)
}

// ─── Hostname ────────────────────────────────────────────────────────────────

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
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameNetBIOS, GetComputerNameExW,
    };

    // First call: pass null + &mut size to obtain required buffer length.
    let mut size: u32 = 0;
    // SAFETY: querying buffer size with null pointer — documented Windows API pattern.
    unsafe { GetComputerNameExW(ComputerNameNetBIOS, std::ptr::null_mut(), &mut size) };

    let mut buf: Vec<u16> = vec![0u16; size as usize];
    let ok = unsafe {
        GetComputerNameExW(ComputerNameNetBIOS, buf.as_mut_ptr(), &mut size)
    };
    if ok == 0 {
        bail!("GetComputerNameExW failed");
    }
    buf.truncate(size as usize);
    Ok(OsString::from_wide(&buf).to_string_lossy().into_owned())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

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

    // ── strip_claude_prefix ──────────────────────────────────────────────────

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

    // ── current_profile — env var extraction ────────────────────────────────

    #[test]
    fn leaf_personal_path() {
        // /home/you/.claude.personal  →  basename ".claude.personal"  →  "personal"
        assert_eq!(
            profile_with_dir("/home/you/.claude.personal"),
            "personal"
        );
    }

    #[test]
    fn leaf_work_path() {
        // /home/you/.claude.work  →  basename ".claude.work"  →  "work"
        assert_eq!(
            profile_with_dir("/home/you/.claude.work"),
            "work"
        );
    }

    #[test]
    fn leaf_bare_profile_no_dot_prefix() {
        // A dir with no .claude. prefix is returned as-is.
        assert_eq!(profile_with_dir("myprofile"), "myprofile");
    }

    #[test]
    fn leaf_trailing_slash_stripped() {
        // std::path::Path normalises trailing slashes; basename is still correct.
        assert_eq!(
            profile_with_dir("/home/you/.claude.personal/"),
            "personal"
        );
    }

    #[test]
    fn absent_env_var_returns_unknown() {
        assert_eq!(profile_with_no_var(), "unknown");
    }

    #[test]
    fn leaf_macos_style_absolute_path() {
        // /Users/example/.claude.personal  →  "personal"
        assert_eq!(
            profile_with_dir("/Users/example/.claude.personal"),
            "personal"
        );
    }

    #[test]
    fn leaf_windows_style_path() {
        // On POSIX, Path::file_name on a Windows-style path treats the whole
        // string as the basename (no separator recognised).
        // The result is implementation-defined but must not panic.
        // We just verify it returns *something* non-empty.
        let result = profile_with_dir(r"C:\Users\example\.claude.personal");
        assert!(!result.is_empty());
    }

    // ── hostname ─────────────────────────────────────────────────────────────

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

    // ── run() smoke ──────────────────────────────────────────────────────────

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
}
