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
/// Failure is soft — we print a warning to stderr but do **not** return an
/// error, mirroring the zsh `… 2>/dev/null` suppression. The live-shell
/// export still succeeds even if launchctl is unavailable (e.g. CI / a
/// container / a sandboxed test environment).
///
/// # Status (Phase 0)
/// Body is `unimplemented!()` — the signature, linkage, and soft-error
/// contract are final; the exec call will be filled in during Phase 11
/// (implementation order §6).
#[cfg(target_os = "macos")]
pub fn launchctl_setenv(profile: &str, dir: &str) -> std::io::Result<()> {
    // Phase 0: signature is locked; body deferred.
    // Final implementation will be approximately:
    //
    //   std::process::Command::new("/bin/launchctl")
    //       .args(["setenv", "CLAUDE_CONFIG_DIR", dir])
    //       .status()
    //       .ok(); // soft — ignore failure
    //   Ok(())
    //
    let _ = (profile, dir); // suppress unused warnings in Phase 0
    unimplemented!(
        "cas: launchctl_setenv not yet implemented (Phase 11); \
         will exec /bin/launchctl setenv CLAUDE_CONFIG_DIR '{dir}'"
    )
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
/// # Status (Phase 0)
/// Body is `unimplemented!()` — the signature, imports, and raw-API strategy
/// are final; the Win32 calls will be filled in during Phase 13 (Windows
/// supervisor, the last phase per §6).
#[cfg(windows)]
pub fn hkcu_setenv(profile: &str, dir: &str) -> std::io::Result<()> {
    // Phase 0: signature is locked; body deferred.
    //
    // Final implementation sketch (windows-sys 0.52):
    //
    //   use windows_sys::Win32::{
    //       System::Registry::{RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE},
    //       UI::WindowsAndMessaging::{
    //           SendMessageTimeoutW, HWND_BROADCAST, WM_SETTINGCHANGE,
    //           SMTO_ABORTIFHUNG,
    //       },
    //       Foundation::HWND,
    //   };
    //   // 1. Open HKCU\Environment with KEY_SET_VALUE.
    //   // 2. RegSetValueExW → REG_EXPAND_SZ or REG_SZ.
    //   // 3. SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0,
    //   //        "Environment" as wide ptr, SMTO_ABORTIFHUNG, 5000, ptr::null_mut()).
    //   Ok(())
    //
    let _ = (profile, dir); // suppress unused warnings in Phase 0
    unimplemented!(
        "cas: hkcu_setenv not yet implemented (Phase 13); \
         will write HKCU\\Environment\\CLAUDE_CONFIG_DIR and broadcast WM_SETTINGCHANGE"
    )
}

// ─── Other POSIX (Linux / WSL) ────────────────────────────────────────────────

#[cfg(all(unix, not(target_os = "macos")))]
fn apply_global_impl(_profile: &str, _dir: &str) -> std::io::Result<()> {
    // Linux / WSL: no persistent non-shell env propagation mechanism.
    // The ~/.zshenv guard is the sole floor; no additional side-effect needed.
    Ok(())
}
