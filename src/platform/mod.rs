pub mod child;
pub mod launcher;
pub mod pid;
pub(crate) mod proc;
pub mod proc_check;
pub mod relaunch;

#[cfg(unix)]
pub mod posix;

#[cfg(windows)]
pub mod windows;

// ─── Compile-time platform dispatch ──────────────────────────────────────────
//
// PlatformProcCheck: the "is PID a live claude/node?" impl — `SysinfoProcCheck`
// on every platform. sysinfo gives a targeted single-process refresh + the
// name/exe/argv[0] on all targets, so there is no external `ps` spawn (and no
// separate POSIX vs Windows code path to keep in sync). The launcher (which IS
// OS-specific) is chosen through
// `pick_launcher` below rather than a bare type alias.

pub use proc_check::SysinfoProcCheck as PlatformProcCheck;

/// The launcher for this `csm` process: csm hands the real terminal (or
/// console) straight to claude.
#[cfg(unix)]
pub fn pick_launcher() -> Box<dyn launcher::Launcher> {
    Box::new(posix::PosixLauncher)
}

/// Windows: the direct launcher, sharing the console with claude.
#[cfg(windows)]
pub fn pick_launcher() -> Box<dyn launcher::Launcher> {
    Box::new(windows::WindowsLauncher)
}
