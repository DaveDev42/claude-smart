//! Platform-specific side-effects for `csm cas -g` (global profile switch).
//!
//! After writing `~/.config/claude-as/default` the binary must also propagate
//! the new `CLAUDE_CONFIG_DIR` to long-running processes that were not spawned
//! from a shell that sources the zshenv/pwsh floor:
//!
//! - **macOS** (`cfg(target_os = "macos")`): `launchctl setenv CLAUDE_CONFIG_DIR <dir>`
//!   updates the `gui/<uid>` launchd domain so GUI apps (Dock, Spotlight, non-login
//!   shells) pick up the new value immediately without a re-login.
//!   Mirrors the zsh `cas -g` path:
//!   `[[ "$OSTYPE" == darwin* ]] && /bin/launchctl setenv CLAUDE_CONFIG_DIR "$dir"`
//!   (`shared/zsh/claude-as.zsh` line 135).
//!
//! - **Windows** (`cfg(windows)`): writes `CLAUDE_CONFIG_DIR` to
//!   `HKCU\Environment` via `RegSetValueExW` and broadcasts a
//!   `WM_SETTINGCHANGE "Environment"` message so Explorer and new console
//!   windows inherit the updated value. This is the Windows parity for the
//!   macOS `launchctl setenv` call.
//!
//! - **Other POSIX** (Linux / WSL): no persistent non-shell environment
//!   propagation mechanism exists; the shell-only guard (`~/.zshenv`) is
//!   sufficient. `apply_global` is a no-op.
//!
//! # Soft-failure contract
//!
//! Both `launchctl_setenv` and `hkcu_setenv` treat failure as *soft*: a missing
//! binary, a permission error, or a locked registry key logs a warning to stderr
//! but does **not** prevent the live-shell export from succeeding. This mirrors
//! the zsh `… 2>/dev/null` suppression on line 135 of `claude-as.zsh`.
//!
//! # Spec reference
//! `docs/superpowers/specs/2026-06-17-csm-rust-port-design.md` §2 "cas -g",
//! §3 "`cas --eval` shell-export shim", §5 #3 "cas live-shell env export"
//! `docs/superpowers/specs/2026-06-18-csm-rust-crate-scaffold.md` §3 `cas/platform.rs`

/// Apply the platform-specific global setenv side-effect for `cas -g`.
///
/// Called **after** the state file has been written. Failures are soft
/// (logged to stderr, not returned as errors) so a missing launchctl or a
/// locked registry key does not prevent the shell export from succeeding.
///
/// Arguments:
/// - `profile` — the canonical profile name (for error messages / logging)
/// - `dir`     — the resolved `CLAUDE_CONFIG_DIR` path to broadcast
pub fn apply_global(profile: &str, dir: &str) -> std::io::Result<()> {
    apply_global_impl(profile, dir)
}

// ─── macOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn apply_global_impl(profile: &str, dir: &str) -> std::io::Result<()> {
    launchctl_setenv(profile, dir)
}

/// Invoke `/bin/launchctl setenv CLAUDE_CONFIG_DIR <dir>` to propagate the
/// new config dir to the launchd `gui/<uid>` domain.
///
/// This updates the `gui/<uid>` launchd domain so GUI apps (Dock, Spotlight,
/// non-login shells spawned by launchd) inherit the new `CLAUDE_CONFIG_DIR`
/// immediately — without requiring a re-login.
///
/// Mirrors the zsh `cas -g` path (claude-as.zsh line 135):
/// ```zsh
/// [[ "$OSTYPE" == darwin* ]] && /bin/launchctl setenv CLAUDE_CONFIG_DIR "$dir" 2>/dev/null
/// ```
///
/// Failure is **soft** — we print a warning to stderr but do NOT return an
/// error, matching the `2>/dev/null` suppression in the zsh source. The
/// live-shell export still succeeds even if launchctl is unavailable (e.g. CI
/// / a container / a sandboxed test environment).
#[cfg(target_os = "macos")]
pub fn launchctl_setenv(_profile: &str, dir: &str) -> std::io::Result<()> {
    use std::process::Command;

    // `/bin/launchctl setenv CLAUDE_CONFIG_DIR <dir>`
    // Matches the zsh line exactly: the env var name is hardcoded, the value
    // is the resolved config-dir path.
    let status = Command::new("/bin/launchctl")
        .args(["setenv", "CLAUDE_CONFIG_DIR", dir])
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            // Non-zero exit from launchctl: warn but don't abort (mirrors 2>/dev/null).
            eprintln!(
                "cas: launchctl setenv exited with {s} — GUI apps may not see the new profile until re-login"
            );
        }
        Err(e) => {
            // launchctl not found or not executable: warn but don't abort.
            eprintln!("cas: launchctl setenv failed: {e} — GUI apps may not see the new profile");
        }
    }

    Ok(())
}

// ─── Windows ─────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn apply_global_impl(profile: &str, dir: &str) -> std::io::Result<()> {
    hkcu_setenv(profile, dir)
}

/// Write `CLAUDE_CONFIG_DIR = <dir>` to `HKCU\Environment` and broadcast
/// a `WM_SETTINGCHANGE "Environment"` message so Explorer and new console
/// windows inherit the new value.
///
/// Uses `windows-sys 0.52` raw API:
/// - `RegOpenKeyExW` / `RegSetValueExW` for the registry write
/// - `SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, "Environment", …)`
///   for the broadcast
///
/// Failure is soft — we print a warning to stderr but do NOT return an error.
///
/// # Status (Phase 4 — Windows supervisor)
/// The registry write and `SendMessageTimeout` broadcast are implemented here
/// as stubs that log a warning. The full Win32 implementation will be added
/// during the Windows supervisor phase (Phase 13 per §6 of the scaffold spec).
/// On POSIX this code is not compiled.
#[cfg(windows)]
pub fn hkcu_setenv(_profile: &str, dir: &str) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::HWND,
        System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
            REG_SZ,
        },
        UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        },
    };

    // Convert the dir string to a null-terminated UTF-16 for Win32 APIs.
    let dir_wide: Vec<u16> = dir.encode_utf16().chain(std::iter::once(0)).collect();

    // Convert the registry key path to wide.
    let key_path: Vec<u16> = "Environment\0".encode_utf16().collect();
    let value_name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let env_str: Vec<u16> = "Environment\0".encode_utf16().collect();

    // 1. Open HKCU\Environment with KEY_SET_VALUE.
    // HKEY in windows-sys 0.52 is an isize handle (not a raw pointer).
    let mut hkey: HKEY = 0;
    let open_result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };

    if open_result != 0 {
        eprintln!(
            "cas: RegOpenKeyExW failed (0x{open_result:08X}) — HKCU\\Environment not updated"
        );
        return Ok(()); // soft failure
    }

    // 2. Write CLAUDE_CONFIG_DIR as REG_SZ.
    let set_result = unsafe {
        RegSetValueExW(
            hkey,
            value_name.as_ptr(),
            0,
            REG_SZ,
            dir_wide.as_ptr() as *const u8,
            (dir_wide.len() * 2) as u32,
        )
    };

    unsafe { RegCloseKey(hkey) };

    if set_result != 0 {
        eprintln!("cas: RegSetValueExW failed (0x{set_result:08X}) — HKCU\\Environment\\CLAUDE_CONFIG_DIR not updated");
        return Ok(()); // soft failure
    }

    // 3. Broadcast WM_SETTINGCHANGE "Environment" so Explorer and new
    //    console windows inherit the change (matching the pwsh pattern used
    //    by HKCU env writes elsewhere in windows/powershell-profile/).
    let mut result: usize = 0;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST as HWND,
            WM_SETTINGCHANGE,
            0,
            env_str.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }

    Ok(())
}

// ─── Other POSIX (Linux / WSL) ────────────────────────────────────────────────

#[cfg(all(unix, not(target_os = "macos")))]
fn apply_global_impl(_profile: &str, _dir: &str) -> std::io::Result<()> {
    // Linux / WSL: no persistent non-shell env propagation mechanism.
    // The ~/.zshenv guard is the sole floor; no additional side-effect needed.
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `apply_global` must not panic or return hard errors on the current
    /// platform. On macOS CI, launchctl may fail (sandbox) — that's OK.
    /// On Linux/WSL, it is a no-op.
    #[test]
    fn apply_global_does_not_panic() {
        // Soft failure only — never panics or returns hard error on POSIX.
        let result = apply_global("home", "/tmp/.claude.home");
        assert!(
            result.is_ok(),
            "apply_global must not return hard error: {:?}",
            result
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchctl_setenv_does_not_panic_with_valid_args() {
        // In a sandboxed CI environment launchctl may fail — that's the soft
        // failure we're testing: it should log a warning, not panic or return Err.
        let result = launchctl_setenv("home", "/tmp/.claude.home");
        assert!(
            result.is_ok(),
            "launchctl_setenv should soft-fail, not hard-fail: {:?}",
            result
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_apply_global_is_noop() {
        // On Linux/WSL the function must succeed (no-op).
        let result = apply_global("home", "/tmp/.claude.home");
        assert!(result.is_ok());
    }
}
