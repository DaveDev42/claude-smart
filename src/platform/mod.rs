pub mod launcher;
pub mod proc_check;
pub mod relaunch;
pub mod pid;

#[cfg(unix)]
pub mod posix;

#[cfg(windows)]
pub mod windows;

// ─── Compile-time platform dispatch ──────────────────────────────────────────
//
// PlatformLauncher: the OS-specific foreground supervisor impl.
// PlatformProcCheck: the OS-specific "is PID a live claude/node?" impl.
//
// Linux (WSL): PosixLauncher + PosixProcCheck.  Both fit because "claude" and
// "node" are well under the 15-char comm truncation limit, and ps(1) is always
// present.  sysinfo would also work on Linux; ps is simpler.

#[cfg(unix)]
pub use posix::PosixLauncher as PlatformLauncher;

#[cfg(windows)]
pub use windows::WindowsLauncher as PlatformLauncher;

#[cfg(unix)]
pub use posix::PosixProcCheck as PlatformProcCheck;

// Windows-native: use SysinfoProcCheck (exe() path instead of ps comm).
#[cfg(all(not(unix), windows))]
pub use proc_check::SysinfoProcCheck as PlatformProcCheck;
