//! Windows-native implementation of `Launcher`.
//!
//! This file is **only compiled on Windows** (`cfg(windows)`).  It intentionally
//! does NOT compile on macOS/Linux — the cfg guard is the contract.
//!
//! `WindowsLauncher` — console-control supervisor:
//!   - Spawns `claude.exe` sharing the console with `CREATE_NEW_PROCESS_GROUP`
//!     (`0x00000200`).  This is required so that `GenerateConsoleCtrlEvent` can
//!     target claude's group for the cooperative console-stop (§4b).
//!   - `SetConsoleCtrlHandler` in the supervisor: swallows the supervisor's own
//!     death AND actively **forwards** the interrupt to claude via
//!     `GenerateConsoleCtrlEvent(CTRL_C_EVENT | CTRL_BREAK_EVENT, child_pgid)`.
//!     Without forwarding, `CREATE_NEW_PROCESS_GROUP` would silently drop all
//!     keyboard Ctrl-C from reaching claude (Win32 reality documented in §3/§5 #1).
//!   - Polls `<sid>.stop` presence while `WaitForMultipleObjects` blocks on the
//!     child handle (~200 ms interval or `ReadDirectoryChangesW`).
//!   - On `.stop` flag: delete flag → `CTRL_BREAK_EVENT` → grace
//!     (`CLAUDE_SWITCH_GRACE_MS`, default 5 s) → `TerminateProcess` fallback.
//!   - Same clobber guard + born-match cleanup + per-launch `CLAUDE_CONFIG_DIR`
//!     env override as `PosixLauncher`.
//!
//! ⚠ BLOCKING empirical checks gate shipping this impl (§4 / §8):
//!   1. Interactive Ctrl-C forwarding: the handler must cancel claude's prompt,
//!      not kill the supervisor.
//!   2. CTRL_BREAK transcript flush: the `.jsonl` must be complete after exit.
//! **Do not ship the Windows relaunch loop until both pass against real `claude.exe`.**

#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;

use super::launcher::{ChildHandle, Launcher};

/// Windows console-control supervisor.  See module-level doc for the protocol.
pub struct WindowsLauncher;

impl Launcher for WindowsLauncher {
    fn run_foreground(
        &self,
        _cli: &[OsString],
        _env: &HashMap<OsString, OsString>,
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        unimplemented!(
            "WindowsLauncher::run_foreground: \
             CREATE_NEW_PROCESS_GROUP + SetConsoleCtrlHandler + WaitForMultipleObjects \
             (Phase 0 stub — implement LAST, after BLOCKING checks §4/§8 pass)"
        )
    }
}
