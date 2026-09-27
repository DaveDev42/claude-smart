use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;

/// PID + birth-epoch the supervisor writes into `<sid>.pid` (and `<sid>.relaunch`).
///
/// `born` is a Unix timestamp (seconds since epoch) captured immediately before
/// the child is spawned.  It acts as a nonce: the relaunch loop and the hook both
/// compare a stored `born` value against the current pidfile to confirm they are
/// looking at the same incarnation of the process and not a recycled PID.
pub struct ChildHandle {
    /// PID of the spawned child. Already written to `<sid>.pid` at spawn time,
    /// so the relaunch loop reads `born` (not `pid`) from the handle; kept for
    /// diagnostics and any future clobber-guard cross-check.
    #[allow(dead_code)]
    pub pid: u32,
    /// Read by the unix relaunch loop's born-check; the Windows launch path
    /// (`run_once`) does no relaunch, so it is unused in the Windows build.
    #[cfg_attr(windows, allow(dead_code))]
    pub born: i64,
}

/// One foreground execution of `claude`.  Impls MUST:
///   1. Inherit the caller's tty for stdin/stdout/stderr (never pipe).
///   2. Write `<sid>.pid` (`"<pid> <born>"`) **immediately after spawn, before
///      blocking in wait** — the limit-switch hook fires while claude is still
///      alive and reads this file to stamp the relaunch sentinel's `born`. If we
///      wrote it only after the child exits, the hook would find no pidfile.
///   3. Return only when the child exits.
///   4. Leave the terminal usable for the relaunch loop afterward.
///   5. Remove `<sid>.pid` is the relaunch loop's job, not the launcher's.
///
/// `env` holds child-only changes to the inherited environment: variables to
/// set (e.g. `CLAUDE_CONFIG_DIR` when the inherited value is not `D`) and
/// variables to remove (credential env that would override the managed
/// account). Impls apply them to the inherited environment rather than
/// replacing it wholesale.
///
/// `on_spawn` runs once, right after the child is spawned and its pidfile
/// written, before the launcher blocks in wait: the relaunch loop starts its
/// after-spawn work (switch recovery) there, never before exec.
pub trait Launcher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &ChildEnv,
        on_spawn: &mut dyn FnMut(),
    ) -> io::Result<(ExitStatus, ChildHandle)>;
}

/// Child-only environment changes (see [`Launcher`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    pub set: HashMap<OsString, OsString>,
    pub remove: Vec<OsString>,
}

impl ChildEnv {
    /// Apply to a `Command` that inherits the parent's environment.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        for k in &self.remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}
