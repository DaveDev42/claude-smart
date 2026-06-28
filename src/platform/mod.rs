pub mod launcher;
pub mod pid;
pub mod proc_check;
pub mod relaunch;

#[cfg(unix)]
pub mod posix;

#[cfg(windows)]
pub mod windows;

// ─── Compile-time platform dispatch ──────────────────────────────────────────
//
// PlatformLauncher: the OS-specific foreground supervisor impl.
// PlatformProcCheck: the "is PID a live claude/node?" impl — `SysinfoProcCheck`
// on every platform. sysinfo gives a targeted single-process refresh + exe()
// basename on all targets, so there is no external `ps` spawn (and no separate
// POSIX vs Windows code path to keep in sync). The launcher stays OS-specific.

#[cfg(unix)]
pub use posix::PosixLauncher as PlatformLauncher;

#[cfg(windows)]
pub use windows::WindowsLauncher as PlatformLauncher;

pub use proc_check::SysinfoProcCheck as PlatformProcCheck;
