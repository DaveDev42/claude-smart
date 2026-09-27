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

    /// [`WriteOpts::PRIVATE`], fsynced (the file, then its dir): for a
    /// file whose loss after a power cut loses a credential or a pending
    /// repair (a quarantined grant whose source is removed right after, the
    /// switch journal).
    pub const PRIVATE_DURABLE: WriteOpts = WriteOpts {
        durable: true,
        ..WriteOpts::PRIVATE
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

// ─── Claude Code's config lock ────────────────────────────────────────────────

/// When a held `<config>.lock` counts as abandoned. Claude Code takes its
/// config lock through proper-lockfile with the library's default
/// `stale: 1e4` (CC 2.1.283 `Mo`: `Ti(e, {lockfilePath: `${e}.lock`, …})`)
/// and refreshes the dir's mtime every `stale / 2`; the 2000 ms in
/// proper-lockfile is only the floor it clamps `stale` to. Taking over a
/// lock younger than this could break a live Claude Code's write.
pub const CLAUDE_CONFIG_LOCK_STALE: Duration = Duration::from_secs(10);

/// Claude Code's own lock on a config file (`~/.claude.json`): the dir
/// `<config>.lock`, created with `mkdir` and removed on release, the
/// proper-lockfile protocol. A lock whose mtime is older than
/// [`CLAUDE_CONFIG_LOCK_STALE`] is removed and taken, as proper-lockfile
/// does. csm holds it for one read-merge-write, far below the stale time,
/// so it never refreshes the mtime on a timer; [`ClaudeConfigLock::touch`]
/// refreshes it once before the write.
#[derive(Debug)]
pub struct ClaudeConfigLock {
    dir: PathBuf,
}

impl ClaudeConfigLock {
    /// `<config>.lock`. Pure.
    pub fn lock_path(config: &Path) -> PathBuf {
        let mut s = config.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// Take the lock on `config`, waiting up to `wait`. A held lock at the
    /// deadline is an `io::ErrorKind::WouldBlock` error.
    pub fn acquire(config: &Path, wait: Duration) -> io::Result<ClaudeConfigLock> {
        let dir = Self::lock_path(config);
        guard(&dir)?;
        let deadline = Instant::now() + wait;
        loop {
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(ClaudeConfigLock { dir }),
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                Err(_) => {}
            }
            let stale = match std::fs::symlink_metadata(&dir).and_then(|m| m.modified()) {
                // Released between the mkdir and the stat: try again.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
                Ok(t) => t.elapsed().is_ok_and(|age| age > CLAUDE_CONFIG_LOCK_STALE),
            };
            if stale {
                match std::fs::remove_dir(&dir) {
                    Ok(()) => continue,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} is held (a Claude Code is saving it)", dir.display()),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Set the lock's mtime to now (best effort), so a waiter never sees it
    /// as stale while csm still writes.
    pub fn touch(&self) {
        let now = std::time::SystemTime::now();
        let mut o = OpenOptions::new();
        o.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS (a dir handle).
            o.access_mode(0x0100).custom_flags(0x0200_0000);
        }
        if let Ok(f) = o.open(&self.dir) {
            let _ = f.set_modified(now);
        }
    }
}

impl Drop for ClaudeConfigLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Claude Code's lock on `config` taken where every Claude Code that saves
/// that file takes it. Claude Code locks the config path as it builds it
/// (`<CLAUDE_CONFIG_DIR or home>/.claude.json` plus `.lock`, never
/// resolved), so for a linked `~/.claude.json` the lock is at the link's
/// name; a Claude Code whose config path names the link's target locks at
/// the target's. Both are taken, the link's first.
#[derive(Debug)]
pub struct ClaudeConfigLocks(Vec<ClaudeConfigLock>);

impl ClaudeConfigLocks {
    /// The config path, and the resolved one when `config` itself is a
    /// link (a path that only resolves through a linked parent dir names
    /// the same lock dir, which is not taken twice). Pure apart from the
    /// resolving.
    pub fn lock_targets(config: &Path) -> Vec<PathBuf> {
        let mut out = vec![config.to_path_buf()];
        let resolved_self = config
            .parent()
            .and_then(|d| std::fs::canonicalize(d).ok())
            .zip(config.file_name())
            .map(|(d, n)| d.join(n));
        if let (Ok(real), Some(this)) = (std::fs::canonicalize(config), resolved_self)
            && real != this
        {
            out.push(real);
        }
        out
    }

    /// Take every lock of [`Self::lock_targets`], all within `wait`.
    pub fn acquire(config: &Path, wait: Duration) -> io::Result<ClaudeConfigLocks> {
        let deadline = Instant::now() + wait;
        let mut held = Vec::new();
        for t in Self::lock_targets(config) {
            let left = deadline.saturating_duration_since(Instant::now());
            held.push(ClaudeConfigLock::acquire(&t, left)?);
        }
        Ok(ClaudeConfigLocks(held))
    }

    /// [`ClaudeConfigLock::touch`] on each.
    pub fn touch(&self) {
        for l in &self.0 {
            l.touch();
        }
    }
}

// ─── links ────────────────────────────────────────────────────────────────────

/// Put a compat link at `link` naming `target`, the way the legacy layout's
/// links were made: a symlink on unix; on Windows a junction for a dir and
/// a hard link for a file (neither needs the symlink privilege). An
/// existing `link` is an `AlreadyExists` error.
pub fn link_compat(link: &Path, target: &Path, is_dir: bool) -> io::Result<()> {
    guard(link)?;
    #[cfg(unix)]
    {
        let _ = is_dir;
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if is_dir {
            junction(link, target)
        } else {
            std::fs::hard_link(target, link)
        }
    }
}

/// `mklink /J <link> <target>` through the system `cmd.exe`, bounded.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    if std::fs::symlink_metadata(link).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", link.display()),
        ));
    }
    let cmd = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("cmd.exe"))
        .unwrap_or_else(|| PathBuf::from("cmd.exe"));
    let mut child = std::process::Command::new(cmd)
        .raw_arg(format!(
            "/D /C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let status = crate::platform::child::wait_deadline(
        &mut child,
        Duration::from_secs(10),
        Duration::from_millis(20),
        false,
    )?;
    let made = std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink());
    match status {
        Some(s) if s.success() && made => Ok(()),
        _ if std::fs::symlink_metadata(link).is_ok() && !made => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", link.display()),
        )),
        _ => Err(io::Error::other(format!(
            "mklink /J {} did not make a junction",
            link.display()
        ))),
    }
}

/// Remove the link at `path` (never what it names): `remove_file`, or on
/// Windows `remove_dir` for a dir symlink or junction.
pub fn remove_link(path: &Path) -> io::Result<()> {
    guard(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if std::fs::symlink_metadata(path)?
            .file_type()
            .is_symlink_dir()
        {
            return std::fs::remove_dir(path);
        }
    }
    std::fs::remove_file(path)
}

/// Are `a` and `b` one file (a hard link pair)? `false` when either
/// cannot be read.
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        match (file_id(a), file_id(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }
}

/// (volume serial, file index) of `p`.
#[cfg(windows)]
fn file_id(p: &Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let f = File::open(p).ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the call and `info` is a valid out
    // pointer of the right type.
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    (ok != 0).then_some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// Are `a` and `b` on one filesystem (a rename between them cannot fail
/// with `EXDEV`)? Unix compares `st_dev`; elsewhere `true`, and the
/// rename's `CrossesDevices` error is the check. Unreadable: `false`.
pub fn same_fs(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

// ─── boot id ──────────────────────────────────────────────────────────────────

/// The prefix of a Windows boot id: Windows has no boot uuid, so the boot
/// time (epoch seconds) stands in for one.
pub const BOOT_TIME_PREFIX: &str = "boot-time:";

/// How far apart two readings of the Windows boot time may lie and still
/// name one boot: the value is derived from the uptime counter and the
/// wall clock, so a clock adjustment moves it.
pub const BOOT_TIME_SLACK_SECS: i64 = 120;

/// This boot's identity, for the migration's floor gate (design I2): macOS
/// `kern.bootsessionuuid`, Linux `/proc/sys/kernel/random/boot_id`, Windows
/// the boot time. `None` when it cannot be read, and always under
/// `cfg(test)`; the e2e build reads `CSM_E2E_BOOT_ID` instead.
pub fn boot_id() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    if crate::e2e::ENABLED {
        return std::env::var("CSM_E2E_BOOT_ID")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
    }
    boot_id_impl()
}

#[cfg(target_os = "macos")]
fn boot_id_impl() -> Option<String> {
    use nix::libc;
    let name = c"kern.bootsessionuuid";
    let mut buf = [0u8; 128];
    let mut len: libc::size_t = buf.len();
    // SAFETY: a NUL-terminated name, a buffer of `len` bytes and its length.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let raw = &buf[..len.min(buf.len())];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = String::from_utf8_lossy(&raw[..end]).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn boot_id_impl() -> Option<String> {
    let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let s = s.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(windows)]
fn boot_id_impl() -> Option<String> {
    let t = sysinfo::System::boot_time();
    (t > 0).then(|| format!("{BOOT_TIME_PREFIX}{t}"))
}

/// Do two boot ids name the same boot? Equal strings do; two Windows boot
/// times within [`BOOT_TIME_SLACK_SECS`] of each other do too. Pure.
pub fn same_boot(a: &str, b: &str) -> bool {
    let time = |s: &str| s.strip_prefix(BOOT_TIME_PREFIX)?.parse::<i64>().ok();
    match (time(a), time(b)) {
        (Some(x), Some(y)) => (x - y).abs() <= BOOT_TIME_SLACK_SECS,
        _ => a == b,
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

    /// Quarantined grants and the switch journal are fsynced: their source
    /// is overwritten or removed right after they are written.
    #[test]
    fn the_durable_private_write_is_private_and_fsynced() {
        let d = WriteOpts::PRIVATE_DURABLE;
        assert!(d.durable && !WriteOpts::PRIVATE.durable);
        assert_eq!(d.mode, 0o600);
        assert!(matches!(d.tmp, TmpStyle::Uuid));
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("q.json");
        write_atomic(&p, b"x", d).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        #[cfg(unix)]
        assert_eq!(mode_of(&p), Some(0o600));
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

    /// The config lock of a linked config is taken at the link's own name
    /// (as Claude Code takes it) and at the target's; a config reached
    /// only through a linked parent dir has one lock dir, taken once.
    #[test]
    #[cfg(unix)]
    fn config_locks_cover_a_linked_config() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles").join("claude.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"{}").unwrap();
        let link = tmp.path().join(".claude.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(ClaudeConfigLocks::lock_targets(&link).len(), 2);
        assert_eq!(ClaudeConfigLocks::lock_targets(&real), vec![real.clone()]);
        let dir_link = tmp.path().join("linked-dir");
        std::os::unix::fs::symlink(real.parent().unwrap(), &dir_link).unwrap();
        assert_eq!(
            ClaudeConfigLocks::lock_targets(&dir_link.join("claude.json")).len(),
            1
        );
        for held_at in [&link, &real] {
            let held = ClaudeConfigLock::acquire(held_at, Duration::ZERO).unwrap();
            let e = ClaudeConfigLocks::acquire(&link, Duration::from_millis(40)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
            drop(held);
        }
        let both = ClaudeConfigLocks::acquire(&link, Duration::ZERO).unwrap();
        assert!(ClaudeConfigLock::lock_path(&link).is_dir());
        assert!(ClaudeConfigLock::lock_path(&real).is_dir());
        drop(both);
        assert!(!ClaudeConfigLock::lock_path(&link).exists());
        assert!(!ClaudeConfigLock::lock_path(&real).exists());
        // Through a linked parent dir: one lock dir, not a wait on itself.
        let one = ClaudeConfigLocks::acquire(&dir_link.join("claude.json"), Duration::ZERO);
        assert!(one.is_ok());
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
    fn claude_config_lock_is_claude_codes_mkdir_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join(".claude.json");
        let lock = ClaudeConfigLock::lock_path(&config);
        assert_eq!(lock, tmp.path().join(".claude.json.lock"));
        let held = ClaudeConfigLock::acquire(&config, Duration::ZERO).unwrap();
        assert!(lock.is_dir(), "proper-lockfile's lock is a dir");
        held.touch();
        // A live holder: a second taker waits, then gives up.
        let err = ClaudeConfigLock::acquire(&config, Duration::from_millis(60)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        assert!(!lock.exists(), "released by removing the dir");
        // An abandoned lock (older than the stale time) is taken over; a
        // young one is not.
        std::fs::create_dir(&lock).unwrap();
        let young = std::time::SystemTime::now() - Duration::from_secs(5);
        filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(young)).unwrap();
        assert!(ClaudeConfigLock::acquire(&config, Duration::ZERO).is_err());
        let old = std::time::SystemTime::now() - CLAUDE_CONFIG_LOCK_STALE - Duration::from_secs(1);
        filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(old)).unwrap();
        let taken = ClaudeConfigLock::acquire(&config, Duration::ZERO).unwrap();
        drop(taken);
        assert!(!lock.exists());
        // The guard holds for the lock too.
        if let Some(home) = dirs::home_dir()
            && !std::env::temp_dir().starts_with(&home)
        {
            assert!(ClaudeConfigLock::acquire(&home.join(".claude.json"), Duration::ZERO).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn compat_links_resolve_to_their_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("t"), b"x").unwrap();
        let link = tmp.path().join("link");
        link_compat(&link, &real, true).unwrap();
        assert_eq!(std::fs::read(link.join("t")).unwrap(), b"x");
        assert_eq!(
            link_compat(&link, &real, true).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        remove_link(&link).unwrap();
        assert!(real.join("t").exists(), "only the link goes");
        let f = tmp.path().join("f");
        std::fs::write(&f, b"1").unwrap();
        let hard = tmp.path().join("h");
        std::fs::hard_link(&f, &hard).unwrap();
        assert!(same_file(&f, &hard));
        assert!(!same_file(&f, &real.join("t")));
        assert!(!same_file(&f, &tmp.path().join("nope")));
        assert!(same_fs(&f, &real));
    }

    #[test]
    fn remove_is_tolerant_of_absence() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!remove_file(&tmp.path().join("nope")).unwrap());
        remove_dir_all(&tmp.path().join("nope")).unwrap();
    }

    #[test]
    fn boot_ids_compare_by_value_and_windows_times_with_slack() {
        assert!(same_boot("0b5c-uuid", "0b5c-uuid"));
        assert!(!same_boot("0b5c-uuid", "77aa-uuid"));
        assert!(same_boot("boot-time:1000", "boot-time:1090"));
        assert!(!same_boot("boot-time:1000", "boot-time:5000"));
        assert!(!same_boot("boot-time:1000", "0b5c-uuid"));
        // Never the real machine's boot under test.
        assert_eq!(boot_id(), None);
    }
}
