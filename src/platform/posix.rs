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
use std::os::unix::process::CommandExt; // pre_exec
use std::process::{Command, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use super::launcher::{ChildHandle, Launcher};
use super::proc_check::{is_claude_or_node_name, ProcCheck};

/// The binary `csm run` launches. Overridable via `CLAUDE_SMART_CLAUDE_BIN`
/// for tests / non-standard installs (matches the shell's `$CLAUDE_BIN`).
fn claude_bin() -> OsString {
    std::env::var_os("CLAUDE_SMART_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"))
}

// ─── PosixLauncher ────────────────────────────────────────────────────────────

/// POSIX foreground supervisor.  See module-level doc for the full protocol.
#[derive(Default)]
pub struct PosixLauncher;

impl Launcher for PosixLauncher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &HashMap<OsString, OsString>,
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        use std::os::unix::io::AsFd;

        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
        use nix::unistd::{setpgid, tcgetpgrp, tcsetpgrp, Pid};

        let born = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Open the controlling tty for the tcsetpgrp handoff. If there is no tty
        // (piped/headless), skip job control and just inherit fds — the child
        // still runs in the foreground of whatever it inherited.
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok();

        let mut cmd = Command::new(claude_bin());
        cmd.args(cli);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // stdin/stdout/stderr inherited by default (never piped) — the child must
        // own the real tty for a usable interactive session.

        // pre_exec runs in the child between fork and exec: make the child its own
        // process-group leader so the kernel delivers Ctrl-C/Ctrl-Z to its pgrp
        // (not the supervisor). setpgid(0,0) — NOT setsid (which would drop the
        // controlling tty we need for the tcsetpgrp grant-back).
        unsafe {
            cmd.pre_exec(|| {
                let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        let pid = child.id();
        let child_pgid = Pid::from_raw(pid as i32);

        // Write the pidfile NOW, while claude is alive — the limit-switch hook
        // fires mid-session and reads `<sid>.pid` to stamp the sentinel's born.
        let _ = crate::platform::pid::write_pid_file(&crate::paths::pid_file(sid), pid, born);

        // Parent also sets the child's pgid (race-free: whoever wins, the child
        // lands in its own group). Idempotent.
        let _ = setpgid(child_pgid, child_pgid);

        // Hand the terminal to the child's pgrp. tcsetpgrp from a background pgrp
        // raises SIGTTOU (would stop the supervisor) — ignore it across the
        // handoff, then restore.
        let saved_ttou = if tty.is_some() {
            unsafe {
                let ign = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
                sigaction(Signal::SIGTTOU, &ign).ok()
            }
        } else {
            None
        };

        let parent_pgid = tty.as_ref().and_then(|t| {
            let fd = t.as_fd();
            let prev = tcgetpgrp(fd).ok();
            let _ = tcsetpgrp(fd, child_pgid);
            prev
        });

        // The supervisor installs NO SIGINT/SIGTERM handler: with the child in the
        // foreground pgrp, the kernel routes keyboard signals straight to claude.
        // We just block in wait().
        let status = wait_for(child)?;

        // Reclaim the terminal for the relaunch loop, then restore SIGTTOU.
        if let (Some(t), Some(prev)) = (tty.as_ref(), parent_pgid) {
            let _ = tcsetpgrp(t.as_fd(), prev);
        }
        if let Some(prev) = saved_ttou {
            unsafe {
                let _ = sigaction(Signal::SIGTTOU, &prev);
            }
        }

        Ok((status, ChildHandle { pid, born }))
    }
}

/// Wait for `child`, retrying on EINTR-interrupted waits.
fn wait_for(mut child: std::process::Child) -> io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
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
