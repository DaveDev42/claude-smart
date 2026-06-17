//! Windows-native implementation of `Launcher` + `ProcCheck` helpers.
//!
//! This file is **only compiled on Windows** (`cfg(windows)`).  It intentionally
//! does NOT compile on macOS/Linux — the cfg guard is the contract.
//!
//! `WindowsLauncher` — console-control supervisor (spec §4b):
//!   - Spawns `claude.exe` sharing the console with `CREATE_NEW_PROCESS_GROUP`
//!     (`0x00000200`).  This puts claude in its own process group so that
//!     `GenerateConsoleCtrlEvent` can target it for the cooperative console-stop.
//!   - **BLOCKER B1 (spec §3/§5 #1):** `CREATE_NEW_PROCESS_GROUP` causes the OS to
//!     stop delivering keyboard Ctrl-C to the new group.  The supervisor therefore
//!     installs a `SetConsoleCtrlHandler` that, on `CTRL_C_EVENT`, **forwards** the
//!     interrupt to claude's group via `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)`
//!     and returns TRUE (handled — do NOT let the default handler kill the
//!     supervisor).  Without forwarding, interactive Ctrl-C would be silently
//!     swallowed and never reach claude.
//!   - Blocks on the child via `WaitForSingleObject`, polling `<sid>.stop` on a
//!     timeout so the limit-switch hook's cooperative stop is picked up.
//!   - On `.stop` flag: delete flag → `CTRL_BREAK_EVENT` to claude's group →
//!     grace (`CLAUDE_SWITCH_GRACE_MS`, default 5 s) → `TerminateProcess` fallback.
//!   - Writes `<sid>.pid` immediately after spawn (same born-timing contract as
//!     `PosixLauncher` — the hook reads it mid-session).
//!
//! ⚠ Two BLOCKING empirical checks gate shipping the Windows relaunch loop
//!   (§4 / §8), verified on Acme-Win against real `claude.exe`:
//!     1. Interactive Ctrl-C forwarding cancels claude's prompt (not the supervisor).
//!     2. CTRL_BREAK transcript flush: the `.jsonl` is complete after a switch.

#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};

use super::launcher::{ChildHandle, Launcher};

/// `CREATE_NEW_PROCESS_GROUP` — claude becomes its own console process group so
/// `GenerateConsoleCtrlEvent` can target it without hitting the whole console.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// The child's process-group id, shared with the console-control handler.  Set to
/// the child pid immediately after spawn; 0 means "no child / don't forward".
static CHILD_PGID: AtomicU32 = AtomicU32::new(0);

/// The binary `csm run` launches. Overridable via `CLAUDE_SMART_CLAUDE_BIN`.
fn claude_bin() -> OsString {
    std::env::var_os("CLAUDE_SMART_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"))
}

/// Console control handler. Runs on a dedicated OS thread when the user presses
/// Ctrl-C / Ctrl-Break or the console signals close.
///
/// On `CTRL_C_EVENT` we FORWARD to claude's group and return TRUE so the default
/// handler (which would terminate the supervisor) does not run. This is the B1
/// fix — without it, `CREATE_NEW_PROCESS_GROUP` swallows keyboard Ctrl-C.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    let pgid = CHILD_PGID.load(Ordering::SeqCst);
    match ctrl_type {
        CTRL_C_EVENT => {
            if pgid != 0 {
                // Forward the interrupt to claude's process group.
                let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid);
            }
            TRUE // handled — do not let the default handler kill us
        }
        // Ctrl-Break: also forward (claude treats it like an interrupt). We do not
        // use the supervisor's own break path for keyboard breaks.
        CTRL_BREAK_EVENT => {
            if pgid != 0 {
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pgid);
            }
            TRUE
        }
        // Close/logoff/shutdown: let the default handler proceed (return FALSE) so
        // the OS tears the console down normally.
        _ => FALSE,
    }
}

/// Windows console-control supervisor.  See module-level doc for the protocol.
#[derive(Default)]
pub struct WindowsLauncher;

impl Launcher for WindowsLauncher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &HashMap<OsString, OsString>,
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        let born = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut cmd = Command::new(claude_bin());
        cmd.args(cli);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Own process group so GenerateConsoleCtrlEvent can target claude alone.
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        // stdio inherited by default (shares the console) — never piped.

        let child = cmd.spawn()?;
        let pid = child.id();

        // Publish the child group id, THEN install the forwarding handler. With
        // CREATE_NEW_PROCESS_GROUP the OS will otherwise drop keyboard Ctrl-C.
        CHILD_PGID.store(pid, Ordering::SeqCst);
        // SAFETY: registering a process-wide console control handler.
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), TRUE);
        }

        // Write the pidfile NOW (born-timing): the hook reads it mid-session.
        let _ = crate::platform::pid::write_pid_file(&crate::paths::pid_file(sid), pid, born);

        let stop_flag = crate::paths::stop_flag(sid);
        let grace = Duration::from_millis(
            std::env::var("CLAUDE_SWITCH_GRACE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5_000),
        );

        let status = supervise(child, &stop_flag, pid, grace)?;

        // Tear down the handler + clear the shared pgid so a stray late Ctrl-C
        // after exit is a no-op.
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), FALSE);
        }
        CHILD_PGID.store(0, Ordering::SeqCst);

        Ok((status, ChildHandle { pid, born }))
    }
}

/// Block on `child`, polling `stop_flag`. On stop: CTRL_BREAK → grace →
/// TerminateProcess. Returns when the child has exited.
fn supervise(
    mut child: std::process::Child,
    stop_flag: &std::path::Path,
    pgid: u32,
    grace: Duration,
) -> io::Result<ExitStatus> {
    const POLL: Duration = Duration::from_millis(200);
    loop {
        // Has the child exited on its own?
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        // Cooperative stop requested by the hook?
        if stop_flag.exists() {
            // Consume the flag so we don't re-trigger.
            let _ = std::fs::remove_file(stop_flag);
            // Ask claude to wind down gracefully (flush its transcript .jsonl).
            // SAFETY: documented console-control API.
            unsafe {
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pgid);
            }
            // Grace period for the graceful exit + transcript flush.
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                std::thread::sleep(POLL);
            }
            // Still alive after grace → hard kill (last resort).
            let _ = child.kill();
            return child.wait();
        }

        std::thread::sleep(POLL);
    }
}
