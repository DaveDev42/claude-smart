//! Platform-specific side-effects for `csm cas -g` (global profile switch).
//!
//! After writing `~/.config/claude-as/default` the binary must also propagate
//! the new `CLAUDE_CONFIG_DIR` to long-running processes that were not spawned
//! from a shell that sources the zshenv/pwsh floor:
//!
//! - **macOS** (`cfg(target_os = "macos")`): `launchctl setenv CLAUDE_CONFIG_DIR <dir>`
//!   updates the `gui/<uid>` launchd domain so GUI apps (Dock, Spotlight, non-login
//!   shells) pick up the new value immediately without a re-login.
//!   Matches the legacy shell implementation's `cas -g` path:
//!   `[[ "$OSTYPE" == darwin* ]] && /bin/launchctl setenv CLAUDE_CONFIG_DIR "$dir"`.
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
//! but does **not** prevent the live-shell export from succeeding. This matches
//! the legacy shell implementation's `… 2>/dev/null` suppression.
//!
//! # Inert under `cfg(test)`
//!
//! Both setters return before touching launchd / HKCU when the crate is
//! compiled for its own unit tests. The side effect is machine-wide and
//! outlives the test process: a real `launchctl setenv` from a test run left
//! every GUI-launched `claude` on the developer's machine (Orca included)
//! pointing at a deleted test tempdir until re-login, and a native Windows
//! test run wrote a POSIX test literal into `HKCU\Environment`. Every test
//! that reaches `apply_global` — the `cas -g` / `profiles use` paths, the
//! editor — therefore exercises the state file and the emitted shell snippet
//! only.

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
/// Matches the legacy shell implementation's `cas -g` path:
/// ```zsh
/// [[ "$OSTYPE" == darwin* ]] && /bin/launchctl setenv CLAUDE_CONFIG_DIR "$dir" 2>/dev/null
/// ```
///
/// Failure is **soft** — we print a warning to stderr but do NOT return an
/// error, matching the `2>/dev/null` suppression in the shell source. The
/// live-shell export still succeeds even if launchctl is unavailable (e.g. CI
/// / a container / a sandboxed test environment).
#[cfg(target_os = "macos")]
pub fn launchctl_setenv(_profile: &str, dir: &str) -> std::io::Result<()> {
    use std::process::Command;

    // Unit tests must never reach launchd — see the module doc ("Inert under
    // cfg(test)"): the floor is machine-wide and survives the test process.
    if cfg!(test) {
        return Ok(());
    }

    // `/bin/launchctl setenv CLAUDE_CONFIG_DIR <dir>`
    // Matches the legacy shell implementation exactly: the env var name is
    // hardcoded, the value is the resolved config-dir path.
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
/// On POSIX this code is not compiled.
#[cfg(windows)]
pub fn hkcu_setenv(_profile: &str, dir: &str) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::HWND,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW,
            RegSetValueExW,
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    };

    // Unit tests must never write HKCU\Environment — see the module doc
    // ("Inert under cfg(test)"): the value is user-wide and survives the test
    // process.
    if cfg!(test) {
        return Ok(());
    }

    // Convert the dir string to a null-terminated UTF-16 for Win32 APIs.
    let dir_wide: Vec<u16> = dir.encode_utf16().chain(std::iter::once(0)).collect();

    // Convert the registry key path to wide.
    let key_path: Vec<u16> = "Environment\0".encode_utf16().collect();
    let value_name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let env_str: Vec<u16> = "Environment\0".encode_utf16().collect();

    // 1. Open HKCU\Environment with KEY_SET_VALUE.
    let mut hkey: HKEY = std::ptr::null_mut();
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
        eprintln!(
            "cas: RegSetValueExW failed (0x{set_result:08X}) — HKCU\\Environment\\CLAUDE_CONFIG_DIR not updated"
        );
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

    /// `apply_global` must not panic or return a hard error on any platform.
    /// Under `cfg(test)` the macOS / Windows setters are inert (module doc),
    /// so this never touches launchd or HKCU; the path is deliberately one
    /// that must never become a real floor.
    #[test]
    fn apply_global_is_ok_and_inert_under_test() {
        let result = apply_global("home", "/nonexistent/.claude.home");
        assert!(
            result.is_ok(),
            "apply_global must not return a hard error: {result:?}"
        );
    }
}
