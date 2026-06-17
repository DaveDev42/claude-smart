//! POSIX (macOS + Linux/WSL) implementations of `Launcher` and `ProcCheck`.
//!
//! `PosixLauncher` — foreground job-control supervisor:
//!   - Opens `/dev/tty` to get a handle for `tcsetpgrp`.
//!   - `pre_exec`: `setsid()` (or `setpgid(0,0)` fallback) so the child becomes
//!     its own process-group leader.
//!   - `SIG_IGN` SIGTTOU around the `tcsetpgrp` write (otherwise the supervisor
//!     stops itself if it is not already the foreground process group).
//!   - `tcsetpgrp(tty_fd, child_pgid)` hands the terminal to claude; the kernel
//!     then delivers Ctrl-C / Ctrl-Z / SIGWINCH *directly* to claude's foreground
//!     pgrp — the supervisor installs NO SIGINT/SIGTERM handler and just blocks
//!     in `wait()`.
//!   - On exit: `tcsetpgrp(tty_fd, parent_pgid)` reclaims the tty and SIGTTOU is
//!     restored.
//!
//! `PosixProcCheck` — `ps -o comm= -p <pid>` (NOT `-o args=` — that would leak
//! `CLAUDE_CONFIG_DIR` into log-visible call traces).

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;

use super::launcher::{ChildHandle, Launcher};
use super::proc_check::{ProcCheck, is_claude_or_node_name};

// ─── PosixLauncher ────────────────────────────────────────────────────────────

/// POSIX foreground supervisor.  See module-level doc for the full protocol.
pub struct PosixLauncher;

impl Launcher for PosixLauncher {
    fn run_foreground(
        &self,
        _cli: &[OsString],
        _env: &HashMap<OsString, OsString>,
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        unimplemented!(
            "PosixLauncher::run_foreground: \
             setsid + tcsetpgrp foreground supervisor (Phase 0 stub — implement in Phase 9)"
        )
    }
}

// ─── PosixProcCheck ───────────────────────────────────────────────────────────

/// POSIX `ProcCheck` implementation via `ps -o comm= -p <pid>`.
///
/// Uses `comm` (not `args`) so that `CLAUDE_CONFIG_DIR` and other env vars are
/// never visible in the command that this process itself runs (avoids leaking
/// secrets into audit logs or `ps` output visible to other users).
///
/// Linux `comm` truncates at 15 chars, but "claude" (6) and "node" (4) both fit.
pub struct PosixProcCheck;

impl ProcCheck for PosixProcCheck {
    fn is_live_claude_or_node(pid: u32) -> bool {
        use std::process::Command;

        let out = Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output();

        match out {
            Ok(o) if o.status.success() => {
                let comm = String::from_utf8_lossy(&o.stdout);
                let basename = comm.trim();
                // `comm` may include a full path on some systems; take the last
                // component before the name check.
                let name = std::path::Path::new(basename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(basename);
                is_claude_or_node_name(name)
            }
            // Process absent or ps failed → not live.
            _ => false,
        }
    }
}
