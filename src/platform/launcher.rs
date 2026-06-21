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
/// The `env` map contains child-only environment overrides (e.g.
/// `CLAUDE_CONFIG_DIR`).  Impls merge these into the inherited environment
/// rather than replacing it wholesale.
pub trait Launcher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &HashMap<OsString, OsString>, // child-only overrides
    ) -> io::Result<(ExitStatus, ChildHandle)>;
}
