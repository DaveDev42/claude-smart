pub mod child;
pub mod launcher;
pub mod pid;
pub(crate) mod proc;
pub mod proc_check;
pub mod relaunch;

#[cfg(unix)]
pub mod posix;

#[cfg(unix)]
pub mod relay;

#[cfg(windows)]
pub mod windows;

// ─── Compile-time platform dispatch ──────────────────────────────────────────
//
// PlatformProcCheck: the "is PID a live claude/node?" impl — `SysinfoProcCheck`
// on every platform. sysinfo gives a targeted single-process refresh + the
// name/exe/argv[0] on all targets, so there is no external `ps` spawn (and no
// separate POSIX vs Windows code path to keep in sync). The launcher (which IS
// OS-specific, and on unix picked between direct/relay) is chosen through
// `pick_launcher` below rather than a bare type alias.

pub use proc_check::SysinfoProcCheck as PlatformProcCheck;

/// Pick the launcher for this `csm` process: the pty relay if idle-compact's
/// activation predicate says so (see `relay::should_activate`), the direct
/// (today's-behaviour) launcher otherwise. Chosen once per process start —
/// a later, per-hop *setup* failure inside the relay falls back to the
/// direct launcher for that hop only, without needing to re-run this pick.
#[cfg(unix)]
pub fn pick_launcher(mode: crate::config::IdleCompactMode) -> Box<dyn launcher::Launcher> {
    if relay::platform_should_activate(mode) {
        Box::new(relay::RelayLauncher::with_observer(std::sync::Arc::new(
            crate::idle_compact::supervisor::Supervisor::new(),
        )))
    } else {
        Box::new(posix::PosixLauncher)
    }
}

#[cfg(windows)]
pub fn pick_launcher(_mode: crate::config::IdleCompactMode) -> Box<dyn launcher::Launcher> {
    Box::new(windows::WindowsLauncher)
}
