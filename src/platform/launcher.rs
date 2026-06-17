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
    pub pid: u32,
    pub born: i64,
}

/// One foreground execution of `claude`.  Impls MUST:
///   1. Inherit the caller's tty for stdin/stdout/stderr (never pipe).
///   2. Return only when the child exits.
///   3. Leave the terminal usable for the relaunch loop afterward.
///   4. Write `<sid>.pid` before returning to the relaunch loop.
///
/// The `env` map contains child-only environment overrides (e.g.
/// `CLAUDE_CONFIG_DIR`).  Impls merge these into the inherited environment
/// rather than replacing it wholesale.
pub trait Launcher {
    fn run_foreground(
        &self,
        cli: &[OsString],
        env: &HashMap<OsString, OsString>, // child-only overrides
    ) -> io::Result<(ExitStatus, ChildHandle)>;
}
