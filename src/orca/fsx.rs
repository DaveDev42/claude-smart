//! Guarded file primitives for every write csm makes into Orca's userData,
//! the runtime dir `D` and csm's own state dir.
//!
//! Temp names follow the writer csm stands in for:
//! - the store (Orca's DSe/OSe, M:13675, M:15021): `<file>.<pid>.<ms>.<hex>.tmp`,
//!   written, fsynced, renamed, then the dir fsynced ([`TmpStyle::Store`]);
//! - everything else Orca writes through its fs-utils `a()` (stash children,
//!   `D/.credentials.json`, `.claude.json`, the system-default snapshot):
//!   `<path>.<pid>.<uuid>.tmp`, written with the mode, renamed, no fsync
//!   ([`TmpStyle::Uuid`]).
//!
//! A tmp file is created exclusively (never follows a planted file) and
//! removed on every error path. Rename on Windows retries on the transient
//! sharing errors the way Orca's `x()` does (6 tries, 50 ms steps).
//!
//! `switch.lock` ([`SwitchLock`]) is an OS advisory lock (flock on unix,
//! LockFileEx on Windows, through `std::fs::File::lock`) on
//! `<state>/switch.lock`. It serializes csm processes only; Orca never takes
//! it.
//!
//! csm's state dir ([`state_dir`]): `$XDG_STATE_HOME/csm` (an absolute
//! value only, as the XDG spec says), else `~/.local/state/csm`, on every
//! unix including macOS; `%LOCALAPPDATA%\csm` on Windows.
//!
//! Test guard: under `cfg(test)` every write, remove, mkdir and lock here
//! refuses a path outside the temp dir ([`guard`]), so no test can reach the
//! real userData, `~/.claude*` or the real state dir even through a bug in a
//! resolver.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::HostEnv;
use super::userdata::HostOs;

// ─── test guard ───────────────────────────────────────────────────────────────

/// Refuse a path outside the temp dir (test builds only).
#[cfg(test)]
pub fn guard(path: &Path) -> io::Result<()> {
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cfg(test): refusing {}: {why}", path.display()),
        ))
    };
    if !path.is_absolute() {
        return refuse("relative path");
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return refuse("'..' component");
    }
    let tmp = std::env::temp_dir();
    let tmp_real = std::fs::canonicalize(&tmp).ok();
    let under = path.starts_with(&tmp) || tmp_real.as_ref().is_some_and(|t| path.starts_with(t));
    if !under {
        return refuse("outside the temp dir");
    }
    if let Some(real_home) = dirs::home_dir() {
        // A temp dir inside the real home must still never reach its dotfiles.
        let tmp_in_home = tmp.starts_with(&real_home);
        if !tmp_in_home && path.starts_with(&real_home) {
            return refuse("inside the real home");
        }
    }
    Ok(())
}

/// Production build: no guard.
#[cfg(not(test))]
#[inline]
pub fn guard(_path: &Path) -> io::Result<()> {
    Ok(())
}

// ─── state dir ────────────────────────────────────────────────────────────────

/// csm's state dir for `env`. Pure.
pub fn state_dir(env: &HostEnv) -> PathBuf {
    let set = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && Path::new(s).is_absolute())
            .map(PathBuf::from)
    };
    match env.os {
        HostOs::Windows => set(&env.localappdata)
            .unwrap_or_else(|| env.home.join("AppData").join("Local"))
            .join("csm"),
        HostOs::MacOs | HostOs::Linux => set(&env.xdg_state_home)
            .unwrap_or_else(|| env.home.join(".local").join("state"))
            .join("csm"),
    }
}

// ─── modes and dirs ───────────────────────────────────────────────────────────

/// `mkdir -p` with `mode` on every dir it creates (Node's
/// `mkdirSync(recursive, mode)`). Existing dirs keep their mode.
pub fn create_dir_all(path: &Path, mode: u32) -> io::Result<()> {
    guard(path)?;
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    b.create(path)
}

/// `chmod` (no-op off unix).
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    guard(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        Ok(())
    }
}

/// The permission bits of `path` (unix), `None` elsewhere or when absent.
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn open_new(path: &Path, mode: u32) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(mode);
        o.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    o.open(path)
}

/// Create `path` exclusively (Node's `flag: "wx"`) with `bytes` and `mode`.
pub fn write_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    guard(path)?;
    let mut f = open_new(path, mode)?;
    if let Err(e) = f.write_all(bytes) {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

// ─── atomic writes ────────────────────────────────────────────────────────────

/// Which writer's temp name to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmpStyle {
    /// `<file>.<pid>.<ms>.<hex>.tmp` (Orca's store save).
    Store,
    /// `<path>.<pid>.<uuid>.tmp` (Orca's fs-utils writer).
    Uuid,
}

/// The temp path for `path`. Pure apart from the clock and randomness.
pub fn tmp_path(path: &Path, style: TmpStyle) -> PathBuf {
    let pid = std::process::id();
    let suffix = match style {
        TmpStyle::Store => {
            // Math.random().toString(16).slice(2): 13 hex digits or so.
            let r = uuid::Uuid::new_v4().simple().to_string();
            format!(".{pid}.{}.{}.tmp", super::now_ms(), &r[..13])
        }
        TmpStyle::Uuid => format!(".{pid}.{}.tmp", uuid::Uuid::new_v4().hyphenated()),
    };
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// How to write one file.
#[derive(Debug, Clone, Copy)]
pub struct WriteOpts {
    /// Mode of the new file (unix).
    pub mode: u32,
    pub tmp: TmpStyle,
    /// fsync the tmp before the rename and the dir after it.
    pub durable: bool,
}

impl WriteOpts {
    /// Orca's fs-utils writer with mode 0600.
    pub const PRIVATE: WriteOpts = WriteOpts {
        mode: 0o600,
        tmp: TmpStyle::Uuid,
        durable: false,
    };
}

/// A written, not yet renamed temp file. Dropping it removes the file.
#[derive(Debug)]
pub struct PendingTmp {
    tmp: PathBuf,
    target: PathBuf,
    durable: bool,
    done: bool,
}

impl PendingTmp {
    #[cfg(test)]
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// Rename over the target (and fsync the dir when durable).
    pub fn commit(mut self) -> io::Result<()> {
        guard(&self.target)?;
        rename_retry(&self.tmp, &self.target)?;
        self.done = true;
        if self.durable {
            sync_dir(&self.target);
        }
        Ok(())
    }

    /// Remove the temp file.
    pub fn discard(mut self) {
        let _ = std::fs::remove_file(&self.tmp);
        self.done = true;
    }
}

impl Drop for PendingTmp {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Write `bytes` to a fresh temp beside `path` (step one of an atomic
/// write). The caller commits or discards it.
pub fn write_tmp(path: &Path, bytes: &[u8], opts: WriteOpts) -> io::Result<PendingTmp> {
    guard(path)?;
    let tmp = tmp_path(path, opts.tmp);
    guard(&tmp)?;
    let mut f = open_new(&tmp, opts.mode)?;
    let pending = PendingTmp {
        tmp,
        target: path.to_path_buf(),
        durable: opts.durable,
        done: false,
    };
    f.write_all(bytes)?;
    // The create mode is masked by the umask; set it exactly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(opts.mode))?;
    }
    if opts.durable {
        f.sync_all()?;
    }
    Ok(pending)
}

/// tmp + rename in one step.
pub fn write_atomic(path: &Path, bytes: &[u8], opts: WriteOpts) -> io::Result<()> {
    write_tmp(path, bytes, opts)?.commit()
}

fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    let tries = if cfg!(windows) { 6 } else { 1 };
    let mut n = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e)
                if n < tries
                    && matches!(
                        e.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
                    ) =>
            {
                std::thread::sleep(Duration::from_millis(50 * n));
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// fsync a file's parent dir (best effort; unix only).
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

// ─── removal ──────────────────────────────────────────────────────────────────

/// Remove a file; `Ok(false)` when it was absent.
pub fn remove_file(path: &Path) -> io::Result<bool> {
    guard(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `rm -rf` (Node's `rmSync(recursive, force)`); absent is fine.
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    guard(path)?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ─── switch.lock ──────────────────────────────────────────────────────────────

/// The lock file name in the state dir.
pub const SWITCH_LOCK: &str = "switch.lock";

/// A held `switch.lock`. Released on drop.
#[derive(Debug)]
pub struct SwitchLock {
    file: File,
}

impl SwitchLock {
    /// Take `<state>/switch.lock`, waiting up to `wait`.
    pub fn acquire(state: &Path, wait: Duration) -> io::Result<SwitchLock> {
        create_dir_all(state, 0o700)?;
        let path = state.join(SWITCH_LOCK);
        guard(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(SwitchLock { file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "another csm holds switch.lock",
                    ));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
    }
}

impl Drop for SwitchLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_refuses_paths_outside_the_temp_dir() {
        assert!(guard(Path::new("/etc/passwd")).is_err());
        assert!(guard(Path::new("relative/file")).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(guard(&tmp.path().join("a")).is_ok());
        assert!(guard(&tmp.path().join("..").join("x")).is_err());
        if let Some(home) = dirs::home_dir()
            && !std::env::temp_dir().starts_with(&home)
        {
            for p in [
                home.join(".claude").join(".credentials.json"),
                home.join(".claude.json"),
                home.join(".local")
                    .join("state")
                    .join("csm")
                    .join("switch.lock"),
                home.join("Library")
                    .join("Application Support")
                    .join("orca")
                    .join("orca-data.json"),
            ] {
                assert!(write_atomic(&p, b"x", WriteOpts::PRIVATE).is_err());
                assert!(remove_file(&p).is_err());
                assert!(create_dir_all(&p, 0o700).is_err());
            }
            assert!(SwitchLock::acquire(&home.join(".local/state/csm"), Duration::ZERO).is_err());
        }
    }

    #[test]
    fn state_dir_follows_xdg_and_localappdata() {
        let home = Path::new("/Users/example");
        let mut env = HostEnv::for_test(home, HostOs::MacOs);
        assert_eq!(state_dir(&env), home.join(".local/state/csm"));
        env.xdg_state_home = Some("/var/state".into());
        assert_eq!(state_dir(&env), Path::new("/var/state/csm"));
        env.xdg_state_home = Some("relative".into());
        assert_eq!(state_dir(&env), home.join(".local/state/csm"));
        let mut w = HostEnv::for_test(home, HostOs::Windows);
        assert_eq!(
            state_dir(&w),
            home.join("AppData").join("Local").join("csm")
        );
        w.localappdata = Some(if cfg!(windows) {
            r"C:\Users\example\AppData\Local".into()
        } else {
            "/c/Users/example/AppData/Local".into()
        });
        assert!(state_dir(&w).ends_with("csm"));
        assert!(state_dir(&w).starts_with(w.localappdata.as_deref().unwrap()));
    }

    #[test]
    fn state_dir_under_test_resolves_inside_the_test_home() {
        let tmp = tempfile::tempdir().unwrap();
        let env = crate::testenv::with_test_home(tmp.path(), HostEnv::current).unwrap();
        assert!(state_dir(&env).starts_with(tmp.path()));
        crate::testenv::set_test_home(None);
        assert!(HostEnv::current().is_err());
    }

    #[test]
    fn tmp_names_follow_orca() {
        let p = Path::new("/x/orca-data.json");
        let s = tmp_path(p, TmpStyle::Store);
        let name = s.file_name().unwrap().to_str().unwrap().to_owned();
        let parts: Vec<&str> = name.split('.').collect();
        // orca-data . json . pid . ms . hex . tmp
        assert_eq!(parts.len(), 6, "{name}");
        assert_eq!(parts[2], std::process::id().to_string());
        assert!(parts[3].parse::<i64>().is_ok());
        assert!(parts[4].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parts[5], "tmp");
        let u = tmp_path(Path::new("/x/.credentials.json"), TmpStyle::Uuid);
        let name = u.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(&format!(".credentials.json.{}.", std::process::id())));
        assert!(name.ends_with(".tmp"));
        assert_eq!(name.matches('-').count(), 4, "a hyphenated uuid");
    }

    #[test]
    fn atomic_write_sets_the_mode_and_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.json");
        write_atomic(&p, b"one", WriteOpts::PRIVATE).unwrap();
        write_atomic(
            &p,
            b"two",
            WriteOpts {
                mode: 0o640,
                tmp: TmpStyle::Store,
                durable: true,
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        #[cfg(unix)]
        assert_eq!(mode_of(&p), Some(0o640));
        let names: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn a_discarded_or_dropped_tmp_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        let t = write_tmp(&p, b"x", WriteOpts::PRIVATE).unwrap();
        assert!(t.tmp().exists());
        let path = t.tmp().to_path_buf();
        t.discard();
        assert!(!path.exists());
        let t = write_tmp(&p, b"x", WriteOpts::PRIVATE).unwrap();
        let path = t.tmp().to_path_buf();
        drop(t);
        assert!(!path.exists() && !p.exists());
    }

    #[test]
    fn write_new_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("marker");
        write_new(&p, b"id\n", 0o600).unwrap();
        assert!(write_new(&p, b"other\n", 0o600).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"id\n");
        #[cfg(unix)]
        assert_eq!(mode_of(&p), Some(0o600));
    }

    #[test]
    #[cfg(unix)]
    fn mkdir_applies_the_mode_to_created_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b");
        create_dir_all(&deep, 0o700).unwrap();
        assert_eq!(mode_of(&deep), Some(0o700));
        assert_eq!(mode_of(&tmp.path().join("a")), Some(0o700));
        create_dir_all(&deep, 0o755).unwrap();
        assert_eq!(mode_of(&deep), Some(0o700));
    }

    #[test]
    fn switch_lock_excludes_a_second_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let held = SwitchLock::acquire(&state, Duration::ZERO).unwrap();
        let state2 = state.clone();
        let other =
            std::thread::spawn(move || SwitchLock::acquire(&state2, Duration::from_millis(50)));
        let err = other.join().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        SwitchLock::acquire(&state, Duration::ZERO).unwrap();
    }

    #[test]
    fn remove_is_tolerant_of_absence() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!remove_file(&tmp.path().join("nope")).unwrap());
        remove_dir_all(&tmp.path().join("nope")).unwrap();
    }
}
