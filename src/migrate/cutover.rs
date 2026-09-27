//! The D transition (design section 3): [`cutover_gate`] and the four
//! steps of [`cutover`], [`floor_gate`] (I2's floor half) and
//! [`recheck_floor`], the session floor and `unset_floor_env`, and the
//! legacy supervisor check stage C waits on ([`supervision`]).

use std::io;
use std::path::{Path, PathBuf};

use crate::launch_context::{ConfigDirPin, OrcaMain};
use crate::orca::context::Context;
use crate::orca::http::SystemHttp;
use crate::orca::live::{DirUsers, ProcFacts, SystemProcs, dir_users, registry_users};
use crate::orca::quarantine::{Quarantine, Reason};
use crate::orca::runtime::{self, RuntimeIdentity, RuntimePaths, read_runtime_identity};
use crate::orca::stash::Stash;
use crate::orca::switch;
use crate::orca::{HostEnv, HostOs, OrcaError, OrcaView, SecretString, SnapshotOptions, fsx};

use super::adopt::{failed, read_back_one, store_less, unreadable_row};
use super::carry::*;
use super::legacy::*;
use super::state::{Cutover, MigrationState, Phase, StepStatus};
use super::{LaunchChild, Report, ReportRow};

// ─── the config-dir value (pure) ────────────────────────────────────────────────────

/// A `CLAUDE_CONFIG_DIR` value that is set at all, trimmed and without a
/// trailing separator. Pure.
pub(crate) fn set_config_dir(value: Option<&str>) -> Option<&Path> {
    let d = value.map(str::trim).filter(|d| !d.is_empty())?;
    Some(Path::new(d.trim_end_matches(['/', '\\'])))
}

// ─── legacy csm supervisors ───────────────────────────────────────────────────

/// How long after the recorded `born` a supervised claude may have
/// started: the old launcher stamped `born` right before the spawn.
pub(crate) const BORN_SLACK_SECS: i64 = 5;

/// A claude an earlier csm started and still supervises: the first pid of
/// the old state dir's `<sid>.pid` files (`(pid, born)`) that still runs
/// and started at or within [`BORN_SLACK_SECS`] after `born`, so a reused
/// pid does not match. `csm reap` reads the new state dir only and does not
/// see these. Pure over `start_time`.
pub(crate) fn legacy_supervised_child(
    pidfiles: &[(u32, i64)],
    start_time: impl Fn(u32) -> Option<u64>,
) -> Option<u32> {
    pidfiles.iter().find_map(|&(pid, born)| {
        let started = i64::try_from(start_time(pid)?).ok()?;
        (pid != 0 && started >= born - 1 && started <= born + BORN_SLACK_SECS).then_some(pid)
    })
}

/// Another csm process: one whose executable is `csm` (the `claude` alias
/// runs the same binary), other than this process and its ancestors, and,
/// with `before`, started before that time (epoch seconds). A `csm run`
/// supervisor between two relaunch hops has no claude registered
/// anywhere, and would start one into a legacy dir or `~/.claude` while
/// the migration moves them. Pure.
pub(crate) fn other_csm(
    table: &[crate::platform::proc::ProcInfo],
    this: u32,
    before: Option<u64>,
) -> Option<u32> {
    let parent = |pid: u32| table.iter().find(|p| p.pid == pid).and_then(|p| p.ppid);
    let mut mine = vec![this];
    let mut at = this;
    while let Some(pp) = parent(at) {
        if pp == 0 || mine.contains(&pp) || mine.len() > 64 {
            break;
        }
        mine.push(pp);
        at = pp;
    }
    let is_csm = |p: &crate::platform::proc::ProcInfo| {
        let stem = p
            .exe
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| crate::platform::proc_check::bare_basename(&p.name).to_owned());
        stem.eq_ignore_ascii_case("csm")
    };
    table
        .iter()
        .filter(|p| before.is_none_or(|t| p.start_time < t))
        .find(|p| !mine.contains(&p.pid) && is_csm(p))
        .map(|p| p.pid)
}

/// The `<sid>.pid` records under the old state dir.
pub(crate) fn legacy_pidfiles(home: &Path) -> Vec<(u32, i64)> {
    let Ok(rd) = std::fs::read_dir(legacy_smart_dir(home)) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pid"))
        .filter_map(|p| crate::platform::pid::read_pid_file(&p).ok().flatten())
        .collect()
}

/// Why stage C must wait for a legacy csm supervisor, if it must: a claude
/// an earlier csm started (its pidfile under the old state dir) still
/// runs, or a csm process started before this machine's migration was
/// first recorded (`seen_at`, epoch seconds) still runs. That one is the
/// old binary: between relaunch hops it has no live claude, its own
/// environment may name the floor rather than the profile it relaunches
/// into, and its next hop would land in a renamed dir. A csm started after
/// `seen_at` is the new one, which follows Orca's `D` (R1) and never
/// relaunches into a legacy dir. With no `seen_at` (a marker from before
/// it was recorded) every other csm counts. An unreadable process table
/// adds nothing here: [`dir_users`] already counts it as live.
pub(crate) fn supervision(
    home: &Path,
    procs: &dyn ProcFacts,
    seen_at: Option<i64>,
) -> Option<String> {
    if let Some(pid) = legacy_supervised_child(&legacy_pidfiles(home), |p| procs.start_time(p)) {
        return Some(format!(
            "claude pid {pid}, started by an earlier csm (~/.claude.shared/smart), still runs"
        ));
    }
    let table = procs.table()?;
    let before = seen_at.map(|t| u64::try_from(t).unwrap_or(0));
    other_csm(&table, std::process::id(), before).map(|pid| {
        format!(
            "csm pid {pid}, started before the migration, still runs; a legacy supervisor may relaunch claude into a legacy dir"
        )
    })
}

// ─── a floor set again at every login ─────────────────────────────────────────

/// Does this LaunchAgent set `CLAUDE_CONFIG_DIR` for the login session?
/// A heuristic over text, never an execution: the plist (XML or binary;
/// both keep ASCII strings as bytes), or a script one of its absolute
/// `<string>` paths names (`read` returns at most a small file's bytes),
/// holds both `CLAUDE_CONFIG_DIR` and `setenv`. A job's own
/// `EnvironmentVariables` alone does not count: it reaches only that job.
/// Pure over `read`.
pub(crate) fn agent_sets_floor(plist: &[u8], read: &dyn Fn(&Path) -> Option<Vec<u8>>) -> bool {
    fn has(b: &[u8], needle: &str) -> bool {
        b.windows(needle.len()).any(|w| w == needle.as_bytes())
    }
    let sets = |b: &[u8]| has(b, "CLAUDE_CONFIG_DIR") && has(b, "setenv");
    if sets(plist) {
        return true;
    }
    let text = String::from_utf8_lossy(plist);
    text.split("<string>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</string>").map(|(v, _)| v.trim()))
        .filter(|v| v.starts_with('/'))
        .any(|v| read(Path::new(v)).is_some_and(|b| sets(&b)))
}

/// `~/Library/LaunchAgents/*.plist` that set `CLAUDE_CONFIG_DIR` again at
/// every login ([`agent_sets_floor`]). Unset once, such an agent puts the
/// floor back at the next login, pointing an Orca started from the Dock
/// at a profile dir retire renamed.
pub(crate) fn floor_agents(home: &Path) -> Vec<PathBuf> {
    const CAP: u64 = 256 * 1024;
    let read = |p: &Path| -> Option<Vec<u8>> {
        let m = std::fs::metadata(p).ok()?;
        (m.is_file() && m.len() <= CAP).then(|| std::fs::read(p).ok())?
    };
    let Ok(rd) = std::fs::read_dir(home.join("Library").join("LaunchAgents")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "plist"))
        .filter(|p| read(p).is_some_and(|b| agent_sets_floor(&b, &read)))
        .collect();
    out.sort();
    out
}

/// The login session's `CLAUDE_CONFIG_DIR` (the value [`unset_floor_env`]
/// clears). `None` under `cfg(test)`. The e2e build reads the file
/// `CSM_E2E_SESSION_FLOOR_FILE` names (unset, absent or blank: no floor),
/// which [`unset_floor_env`] empties, never the real session.
pub(crate) fn session_floor() -> io::Result<Option<String>> {
    if cfg!(test) {
        return Ok(None);
    }
    if crate::e2e::ENABLED {
        let Some(f) = std::env::var_os("CSM_E2E_SESSION_FLOOR_FILE") else {
            return Ok(None);
        };
        return match std::fs::read_to_string(&f) {
            Ok(s) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
    }
    session_floor_impl()
}

/// `launchctl getenv CLAUDE_CONFIG_DIR`, bounded like the unset below.
#[cfg(target_os = "macos")]
pub(crate) fn session_floor_impl() -> io::Result<Option<String>> {
    let out = launchctl(&["getenv", "CLAUDE_CONFIG_DIR"])?;
    let v = String::from_utf8_lossy(&out).trim().to_owned();
    Ok((!v.is_empty()).then_some(v))
}

#[cfg(windows)]
pub(crate) fn session_floor_impl() -> io::Result<Option<String>> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    let key: Vec<u16> = "Environment\0".encode_utf16().collect();
    let name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut len: u32 = 0;
    // SAFETY: NUL-terminated wide strings; a null buffer asks for the size.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegGetValueW failed (0x{rc:08X})"
        )));
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `len` bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegGetValueW failed (0x{rc:08X})"
        )));
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let v = String::from_utf16_lossy(&buf[..end]).trim().to_owned();
    Ok((!v.is_empty()).then_some(v))
}

/// Linux has no session-wide floor csm set (the legacy fleet exported it
/// from shell rc files, which the process-env check covers).
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn session_floor_impl() -> io::Result<Option<String>> {
    Ok(None)
}
/// Clear the machine-wide `CLAUDE_CONFIG_DIR` floor. Inert under
/// `cfg(test)`; the e2e build empties `CSM_E2E_SESSION_FLOOR_FILE` instead
/// (and touches nothing without it): the real value outlives the process
/// and belongs to the real login session.
pub(crate) fn unset_floor_env() -> io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    if crate::e2e::ENABLED {
        if let Some(f) = std::env::var_os("CSM_E2E_SESSION_FLOOR_FILE") {
            match std::fs::write(&f, b"") {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        return Ok(());
    }
    unset_floor_env_impl()
}

/// How long one `launchctl` call may take.
#[cfg(target_os = "macos")]
const LAUNCHCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `/bin/launchctl <args>` bounded by [`LAUNCHCTL_TIMEOUT`], in its own
/// process group, with no terminal: the cutover runs after a spawn, where
/// claude owns the terminal, so stdin is null and stdout and stderr are
/// piped (both are a line at most; a pipe holds far more, so waiting before
/// reading cannot deadlock). Returns stdout on success; a
/// failure carries launchctl's diagnostic, which lands in the log. The
/// value `getenv` prints is a path, not a secret.
#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("/bin/launchctl");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::platform::child::own_group(&mut cmd).spawn()?;
    let status = crate::platform::child::wait_deadline(
        &mut child,
        LAUNCHCTL_TIMEOUT,
        std::time::Duration::from_millis(20),
        true,
    )?;
    let mut out = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut out);
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let err = err.trim().to_owned();
    let verb = args.first().copied().unwrap_or_default();
    match status {
        Some(s) if s.success() => Ok(out),
        Some(s) if err.is_empty() => Err(io::Error::other(format!(
            "launchctl {verb} exited with {s}"
        ))),
        Some(s) => Err(io::Error::other(format!(
            "launchctl {verb} exited with {s}: {err}"
        ))),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("launchctl {verb} did not finish in 5 s"),
        )),
    }
}

/// `launchctl unsetenv CLAUDE_CONFIG_DIR` through the bounded [`launchctl`].
#[cfg(target_os = "macos")]
pub(crate) fn unset_floor_env_impl() -> io::Result<()> {
    launchctl(&["unsetenv", "CLAUDE_CONFIG_DIR"]).map(drop)
}

#[cfg(windows)]
pub(crate) fn unset_floor_env_impl() -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    let key: Vec<u16> = "Environment\0".encode_utf16().collect();
    let name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated wide strings and an out-pointer.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegOpenKeyExW failed (0x{rc:08X})"
        )));
    }
    // SAFETY: hkey was opened above.
    let rc = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
    // SAFETY: hkey was opened above.
    unsafe { RegCloseKey(hkey) };
    if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
        return Err(io::Error::other(format!(
            "RegDeleteValueW failed (0x{rc:08X})"
        )));
    }
    let mut result: usize = 0;
    // SAFETY: a broadcast with a static wide string and a timeout.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST as HWND,
            WM_SETTINGCHANGE,
            0,
            key.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn unset_floor_env_impl() -> io::Result<()> {
    Ok(())
}

/// The command that clears the floor by hand, for the failure line.
#[cfg(windows)]
pub(crate) const FLOOR_UNSET_HINT: &str = "reg delete HKCU\\Environment /v CLAUDE_CONFIG_DIR /f";
#[cfg(not(windows))]
pub(crate) const FLOOR_UNSET_HINT: &str = "launchctl unsetenv CLAUDE_CONFIG_DIR";

// ─── the cutover gate (pure) ──────────────────────────────────────────────────

/// Orca as the cutover sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OrcaAt {
    Stopped,
    /// Orca runs, but its environment (so its `D`) cannot be read.
    Unreadable,
    /// Orca runs in this `D`.
    Dir(PathBuf),
}

impl OrcaAt {
    /// From what [`crate::launch_context::orca_main_dir`] read and whether
    /// the liveness check sees Orca: a running Orca whose runtime file does
    /// not name it counts as unreadable (fail closed).
    pub(crate) fn of(main: &OrcaMain, running: bool) -> OrcaAt {
        match main {
            OrcaMain::Dir(o) => OrcaAt::Dir(o.dir.clone()),
            OrcaMain::Unreadable => OrcaAt::Unreadable,
            OrcaMain::Stopped if running => OrcaAt::Unreadable,
            OrcaMain::Stopped => OrcaAt::Stopped,
        }
    }
}

/// What Orca's store lets step 2 do with Orca stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreKind {
    /// A JSON store: the offline switch's a=i repair. `Err`: why csm may
    /// not write `D` offline here.
    Json(Result<(), String>),
    /// A SQLite export (section 4): its active account is materialized, the
    /// store is never written. `Err` as for [`StoreKind::Json`].
    Sqlite(Result<(), String>),
    /// macOS with no store yet: Orca's first start creates one.
    Missing,
    /// No Orca store of this host's own ([`store_less`]): the files stages
    /// only.
    StoreLess,
}

/// Whether csm may write `D` with Orca stopped: never on Windows (Orca
/// detection there is inferred, so the fleet contract keeps A1 and A2 on
/// RPC), only for a tested Orca version and a userData csm may write.
/// Pure.
pub(crate) fn offline_d_gate(
    os: HostOs,
    version_ok: bool,
    store_access_allowed: bool,
) -> Result<(), String> {
    if os == HostOs::Windows {
        Err("on Windows csm changes Orca's accounts only through a running Orca".into())
    } else if !version_ok {
        Err("the installed Orca version is not one csm was tested with".into())
    } else if !store_access_allowed {
        Err("this Orca userData is not csm's to write".into())
    } else {
        Ok(())
    }
}

/// Everything [`cutover_gate`] decides on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CutoverFacts<'a> {
    /// Stage B settled once (I1: B completes before any floor setter goes).
    pub carried: bool,
    /// The login session's `CLAUDE_CONFIG_DIR`; `Err`: it cannot be read.
    pub floor: Result<Option<&'a str>, &'a str>,
    /// The recorded legacy dirs.
    pub legacy_dirs: &'a [PathBuf],
    pub home: &'a Path,
    pub orca: &'a OrcaAt,
    /// Orca runs and listed its accounts over RPC.
    pub listed: bool,
    /// Orca's active host account, only when it names a managed record.
    pub active: Option<&'a str>,
    /// Read only with Orca stopped.
    pub store: &'a StoreKind,
    /// `~/.claude` holds what step 1 removes: a grant that is not the
    /// active stash's, or an `oauthAccount` in `~/.claude.json`.
    pub neutralise_needed: bool,
    /// A claude is live in `~/.claude` without `CLAUDE_CONFIG_DIR` (unknown
    /// counts as live).
    pub live_implicit: bool,
}

/// Step 2: what goes into `~/.claude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Materialise {
    /// Orca runs: its next start materializes the active account itself.
    Nothing,
    /// The offline switch to the active account (the a=i repair).
    Switch(String),
    /// Section 4: the SQLite export's active account, no store write.
    Export(String),
    /// A store-less host: the floor profile's login, only when `~/.claude`
    /// has none.
    StoreLess,
}

/// The cutover's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CutoverPlan {
    Go {
        neutralise: bool,
        materialise: Materialise,
        clear_floor: bool,
    },
    Wait(String),
}

/// Does `value` name one of `dirs` (lexically, trailing separators
/// ignored)? Pure.
pub(crate) fn names_one_of(value: &Path, dirs: &[PathBuf]) -> bool {
    let v = lexical(value);
    dirs.iter().any(|d| lexical(d) == v)
}

/// The cutover's preconditions (design section 3), in order: B done, the
/// floor readable and naming a recorded legacy dir (or `~/.claude`, which
/// I3 wants cleared too) when set, Orca's environment readable when it
/// runs, Orca's active host account managed and confirmed (RPC, the JSON
/// store, or the SQLite export of section 4; never null, never the system
/// default), `D` writable offline when Orca is stopped, and no live
/// implicit claude when step 1 edits `~/.claude`. Pure.
pub(crate) fn cutover_gate(f: &CutoverFacts<'_>) -> CutoverPlan {
    let wait = |s: &str| CutoverPlan::Wait(s.to_owned());
    if !f.carried {
        return wait("the shared dirs and ~/.claude.json are not carried into ~/.claude yet");
    }
    let floor = match f.floor {
        Err(e) => {
            return CutoverPlan::Wait(format!(
                "cannot read the login session's CLAUDE_CONFIG_DIR ({e})"
            ));
        }
        Ok(v) => set_config_dir(v),
    };
    let home_d = f.home.join(".claude");
    if let Some(v) = floor {
        let mut known: Vec<PathBuf> = f.legacy_dirs.to_vec();
        known.push(home_d.clone());
        if !names_one_of(v, &known) {
            return CutoverPlan::Wait(format!(
                "the login session's CLAUDE_CONFIG_DIR is {}, which is no recorded profile dir; \
                 csm leaves it alone, clear it by hand (`{FLOOR_UNSET_HINT}`)",
                v.display()
            ));
        }
    }
    let clear_floor = floor.is_some();
    let orca_in_home = matches!(f.orca, OrcaAt::Dir(d) if lexical(d) == lexical(&home_d));
    let materialise = match f.orca {
        OrcaAt::Unreadable => {
            return wait("Orca runs, but its environment cannot be read, so its D is unknown");
        }
        OrcaAt::Dir(_) if !f.listed => {
            return wait("Orca runs but did not list its accounts");
        }
        OrcaAt::Dir(_) => match f.active {
            None => {
                return wait(
                    "Orca has no active account yet; csm selects the floor profile's first",
                );
            }
            Some(_) => Materialise::Nothing,
        },
        OrcaAt::Stopped => match (f.store, f.active) {
            (StoreKind::StoreLess, _) => Materialise::StoreLess,
            (StoreKind::Missing, _) => {
                return wait("Orca has no store yet; the cutover waits for Orca's first start");
            }
            (StoreKind::Sqlite(_), None) => {
                return wait(
                    "Orca keeps its state in SQLite and its export names no active account; \
                     the cutover waits for Orca to run",
                );
            }
            (StoreKind::Json(_), None) => {
                return wait(
                    "Orca's store names no active account; the cutover waits for Orca to run",
                );
            }
            (StoreKind::Json(Err(why)) | StoreKind::Sqlite(Err(why)), Some(_)) => {
                return CutoverPlan::Wait(format!(
                    "Orca is stopped and {why}; the cutover waits for Orca to run"
                ));
            }
            (StoreKind::Json(Ok(())), Some(id)) => Materialise::Switch(id.to_owned()),
            (StoreKind::Sqlite(Ok(())), Some(id)) => Materialise::Export(id.to_owned()),
        },
    };
    // Step 1 only where step 2 writes nothing: the offline switch and the
    // export read `~/.claude` back first, and that read-back attributes a
    // grant by `~/.claude.json`'s identity, so stripping it first would
    // file a rotated copy of the active account's grant as no one's. They
    // rewrite the identity and the grant themselves.
    let neutralise = f.neutralise_needed && !orca_in_home && materialise == Materialise::Nothing;
    if neutralise && f.live_implicit {
        return wait(
            "a claude runs in ~/.claude without CLAUDE_CONFIG_DIR; the cutover edits ~/.claude \
             once it ends",
        );
    }
    CutoverPlan::Go {
        neutralise,
        materialise,
        clear_floor,
    }
}

/// What one legacy dir needs before step 2 writes the active account into
/// `~/.claude` with Orca stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HolderGate {
    /// The dir does not hold the active account's login, or holds the
    /// stash's copy and no claude uses it.
    Clear,
    /// It holds a fresher grant of the active account and nothing uses it:
    /// read it back into the stash first.
    ReadBack,
    /// Not now, and why.
    Wait(String),
}

/// [`HolderGate`] for a legacy dir that is `status` against Orca's store,
/// with `live` naming a claude that uses it (the launch's own child
/// included). A dir holding the active account's login keeps a refresh
/// token step 2 would copy into `~/.claude`: while a claude runs there the
/// two copies fork at the next refresh, and a fresher grant there means the
/// stash's is stale (maybe rotated dead). `unreadable`: not every grant of
/// the dir could be read. Pure.
pub(crate) fn holder_gate(
    name: &str,
    status: &Status,
    active: &str,
    live: Option<&str>,
    unreadable: bool,
) -> HolderGate {
    let Status::InOrca { id, fresher } = status else {
        return HolderGate::Clear;
    };
    if id != active {
        return HolderGate::Clear;
    }
    if let Some(who) = live {
        return HolderGate::Wait(format!(
            "{name} holds the active account's login and a claude uses it ({who}); the cutover \
             copies that login into ~/.claude once it ends"
        ));
    }
    match fresher {
        _ if unreadable => HolderGate::Wait(format!(
            "{name} holds the active account's login, but some of its grants could not be read"
        )),
        Some(false) => HolderGate::Clear,
        Some(true) => HolderGate::ReadBack,
        None => HolderGate::Wait(format!(
            "{name} holds the active account's login, and it could not be compared with the stash"
        )),
    }
}

// ─── the floor after the cutover (pure) ───────────────────────────────────────

/// I2's floor half for the floor profile's dir: may it be renamed?
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FloorGate {
    /// The floor is absent in a boot later than the cutover's.
    Pass,
    /// No cutover recorded yet.
    NoCutover,
    /// Still the cutover's boot: apps started before it may carry the old
    /// value.
    SameBoot,
    /// A boot id (the recorded one or this boot's) cannot be read.
    BootUnknown,
    /// The floor is set (again): the value.
    FloorSet(String),
    /// The floor cannot be read.
    FloorUnreadable,
}

/// The floor gate. `floor_now`: `Err` when the floor cannot be read. Pure.
pub(crate) fn floor_gate(
    cutover: Option<&Cutover>,
    boot_now: Option<&str>,
    floor_now: Result<Option<&str>, ()>,
) -> FloorGate {
    let Some(c) = cutover else {
        return FloorGate::NoCutover;
    };
    match floor_now.map(set_config_dir) {
        Err(()) => return FloorGate::FloorUnreadable,
        Ok(Some(v)) => return FloorGate::FloorSet(v.display().to_string()),
        Ok(None) => {}
    }
    match (c.boot_id.as_deref(), boot_now) {
        (Some(then), Some(now)) if fsx::same_boot(then, now) => FloorGate::SameBoot,
        (Some(_), Some(_)) => FloorGate::Pass,
        _ => FloorGate::BootUnknown,
    }
}

/// The pending line for a floor gate that holds the floor dir. Pure.
pub(crate) fn floor_gate_line(g: &FloorGate) -> Option<String> {
    match g {
        FloorGate::Pass => None,
        FloorGate::NoCutover => Some("the cutover has not run yet".into()),
        FloorGate::SameBoot => Some(
            "waits for a reboot after the cutover (apps started before it may still use the dir)"
                .into(),
        ),
        FloorGate::BootUnknown => Some(
            "this machine's boot id cannot be read, so csm cannot tell a reboot happened; the dir \
             stays"
                .into(),
        ),
        FloorGate::FloorSet(v) => Some(format!(
            "the login session's CLAUDE_CONFIG_DIR is set again (to {v})"
        )),
        FloorGate::FloorUnreadable => {
            Some("the login session's CLAUDE_CONFIG_DIR cannot be read".into())
        }
    }
}

/// What set the floor again, for the note. Pure over `agents`.
pub(crate) fn floor_writer(os: HostOs, agents: &[PathBuf]) -> String {
    if !agents.is_empty() {
        let names: Vec<String> = agents.iter().map(|p| p.display().to_string()).collect();
        return format!(
            "set at login by {}; unload and delete it (`launchctl bootout gui/$(id -u) <plist>`)",
            names.join(", ")
        );
    }
    match os {
        HostOs::Windows => {
            "a login script or scheduled task writes HKCU\\Environment; remove it".into()
        }
        _ => "something sets it at login (a LaunchAgent or login script); remove it".into(),
    }
}

// ─── the cutover (I/O shell) ──────────────────────────────────────────────────

fn row(report: &mut Report, home: &Path, line: impl Into<String>) {
    report.rows.push(ReportRow {
        name: "cutover".to_owned(),
        dir: home.join(".claude"),
        stage: "cutover",
        line: line.into(),
    });
}

/// `~/.claude`'s state for step 1: an orphan grant (a `.credentials.json`
/// that is not the active stash's, byte for byte) and whether
/// `~/.claude.json` names an `oauthAccount`.
struct HomeState {
    orphan: Option<SecretString>,
    identity: bool,
}

fn home_state(paths: &RuntimePaths, active: Option<&SecretString>) -> io::Result<HomeState> {
    let file = crate::orca::read_capped(&paths.credentials_path, 1024 * 1024)?;
    let orphan = file
        .filter(|f| active.is_none_or(|a| a.expose() != f.as_str()))
        .map(SecretString::new);
    let identity = matches!(read_runtime_identity(paths), RuntimeIdentity::Present(_));
    Ok(HomeState { orphan, identity })
}

/// The active account's stashed grant, when there is one.
fn active_stash_creds(ctx: &Context, view: &OrcaView, id: Option<&str>) -> Option<SecretString> {
    let id = id?;
    Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref())
        .ok()?
        .credentials(ctx.os())
        .ok()
        .flatten()
}

/// Does step 1 have anything to do: `~/.claude` holds a grant that is not
/// the active stash's, or `~/.claude.json` names an account. Never while
/// Orca runs in `~/.claude` or on a store-less host. A dry run reads no
/// secret (decision 6): the stash is not read, so any grant file counts as
/// one step 1 may quarantine. Unreadable counts as needed.
pub(crate) fn neutralise_needed(
    ctx: &Context,
    view: &OrcaView,
    active: Option<&str>,
    orca_in_home: bool,
    store: &StoreKind,
    dry_run: bool,
) -> bool {
    if orca_in_home || *store == StoreKind::StoreLess {
        return false;
    }
    let stash = if dry_run {
        None
    } else {
        active_stash_creds(ctx, view, active)
    };
    home_state(&ctx.paths, stash.as_ref())
        .map(|h| h.orphan.is_some() || h.identity)
        .unwrap_or(true)
}

/// Step 1: quarantine, then remove, an orphan `~/.claude/.credentials.json`
/// and strip `oauthAccount` from `~/.claude.json`, under `switch.lock` and
/// Claude Code's config lock. The unscoped Keychain item stays: Orca
/// mirrors its active account there on every materialize.
fn neutralise(
    ctx: &Context,
    view: &OrcaView,
    active: Option<&str>,
    now_ms: i64,
) -> anyhow::Result<Vec<String>> {
    let _switch = fsx::SwitchLock::acquire(&ctx.state, B2_WAIT)?;
    let stash = active_stash_creds(ctx, view, active);
    let st = home_state(&ctx.paths, stash.as_ref())?;
    let mut lines = Vec::new();
    // The account `~/.claude.json` names, read before the strip below: the
    // grants filed here keep it, so settle can still store a fresher one
    // (after its profile check) instead of leaving it no one's.
    let records: Vec<crate::orca::record::AccountRecord> = view.host_accounts().cloned().collect();
    let owner = identity_owner(&ctx.paths, &records);
    let q = Quarantine::new(ctx.os(), &ctx.state);
    // The unscoped Keychain item stays (Orca mirrors its active account
    // there on every materialize), but a copy that is not the stash's may
    // be the only live one of a grant a plain claude refreshed.
    if ctx.os() == HostOs::MacOs {
        let item = crate::orca::keychain::read_runtime_scoped(None, &ctx.keychain_user)
            .map_err(|e| anyhow::anyhow!("cannot read the unscoped Keychain item ({e})"))?;
        if let Some(item) = item
            && stash.as_ref().is_none_or(|s| s.expose() != item.expose())
        {
            let filed = q.file(
                item.expose(),
                Reason::Cutover,
                "keychain",
                owner.as_deref(),
                None,
                now_ms,
            )?;
            lines.push(format!(
                "the unscoped Keychain item was not the active stash's grant: quarantined a copy \
                 as {} (the item stays)",
                filed.fingerprint()
            ));
        }
    }
    if let Some(grant) = &st.orphan {
        let filed = q.file(
            grant.expose(),
            Reason::Cutover,
            "file",
            owner.as_deref(),
            None,
            now_ms,
        )?;
        let p = &ctx.paths.credentials_path;
        fsx::guard(p)?;
        fsx::remove_file(p)?;
        lines.push(format!(
            "~/.claude/.credentials.json was not the active account's grant: quarantined as {} \
             and removed",
            filed.fingerprint()
        ));
    }
    if st.identity {
        let held = super::hold_config_lock(&ctx.paths.config_path, B2_WAIT)?;
        held.locks.touch();
        runtime::clear_identity(&ctx.paths)?;
        lines.push(format!(
            "removed oauthAccount from {}, so Orca's first start there matches grants by refresh \
             token only",
            ctx.paths.config_path.display()
        ));
    }
    Ok(lines)
}

/// The host account `~/.claude.json`'s `oauthAccount` names, by Orca's
/// identity triple.
fn identity_owner(
    paths: &RuntimePaths,
    records: &[crate::orca::record::AccountRecord],
) -> Option<String> {
    let RuntimeIdentity::Present(oi) = read_runtime_identity(paths) else {
        return None;
    };
    let key = crate::orca::record::IdentityKey::new(
        oi.email.as_deref(),
        oi.organization_uuid.as_deref(),
        crate::orca::record::AuthRuntime::Host,
        None,
    )?;
    crate::orca::record::find_by_identity(records, &key).map(|r| r.id.clone())
}

/// Before step 2 with Orca stopped: every legacy dir holding the active
/// account's login is idle and no fresher than the stash, reading a
/// fresher one back first. `Err`: the cutover waits, and why. `dry_run`
/// reads no secret and reports what it would do in `would`.
#[allow(
    clippy::too_many_arguments,
    reason = "the cutover's facts, passed through"
)]
fn holders_settled(
    ctx: &Context,
    procs: &dyn ProcFacts,
    view: &OrcaView,
    legacy: &Legacy,
    active: &str,
    child: &LaunchChild,
    dry_run: bool,
    would: &mut Vec<String>,
    report: &mut Report,
) -> Result<(), String> {
    let host: Vec<crate::orca::record::AccountRecord> = view.host_accounts().cloned().collect();
    let http = SystemHttp::from_env();
    for p in legacy.profiles.iter().filter(|p| p.dir.is_dir()) {
        let name = p.dir.display().to_string();
        let live = if child.uses(&p.dir) {
            Some("this launch's claude".to_owned())
        } else {
            match dir_users(ctx.os(), &p.dir, &ctx.env.home, procs) {
                DirUsers::Free => None,
                DirUsers::Live(who) => Some(who),
                DirUsers::Unknown(why) => Some(format!("unknown: {why}")),
            }
        };
        let probe = if dry_run {
            Probe::Presence
        } else {
            Probe::Read
        };
        let facts = profile_facts(ctx, p, probe);
        let status = classify(&facts, &host, &|id| {
            if dry_run {
                None
            } else {
                stash_grant(ctx, view, id)
            }
        });
        if dry_run {
            // Presence only: a login of the active account is compared once
            // the cutover runs for real.
            if let Status::InOrca { id, .. } = &status
                && id == active
            {
                if let Some(who) = live {
                    return Err(format!(
                        "{name} holds the active account's login and a claude uses it ({who})"
                    ));
                }
                would.push(format!(
                    "read {name}'s login back into stash {active} if it is fresher"
                ));
            }
            continue;
        }
        let unreadable = unreadable_row(&facts).is_some();
        match holder_gate(&name, &status, active, live.as_deref(), unreadable) {
            HolderGate::Clear => {}
            HolderGate::Wait(why) => return Err(why),
            HolderGate::ReadBack => {
                match read_back_one(ctx, procs, view, &http, &p.dir, active) {
                    Ok(line) => {
                        row(report, &ctx.env.home, format!("{name}: {line}"));
                        report.changed.push(format!("{name}: {line}"));
                    }
                    Err(e) => return Err(format!("{name}'s fresher login: {e}")),
                }
                let facts = profile_facts(ctx, p, Probe::Read);
                let again = classify(&facts, &host, &|id| stash_grant(ctx, view, id));
                if holder_gate(
                    &name,
                    &again,
                    active,
                    None,
                    unreadable_row(&facts).is_some(),
                ) != HolderGate::Clear
                {
                    return Err(format!(
                        "{name}'s login of the active account is still fresher than the stash \
                         after the read-back (its access token may be refused until a claude \
                         there refreshes it)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Step 2 on a store-less host: the floor profile's login goes into
/// `~/.claude` only when `~/.claude` holds none; its `oauthAccount` joins
/// `~/.claude.json` only when that file names none.
fn store_less_login(ctx: &Context, floor_dir: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let Some(floor_dir) = floor_dir.filter(|d| d.is_dir()) else {
        return Ok(Vec::new());
    };
    let _switch = fsx::SwitchLock::acquire(&ctx.state, B2_WAIT)?;
    let paths = &ctx.paths;
    let mut lines = Vec::new();
    let has_login = paths.credentials_path.exists();
    let from = floor_dir.join(".credentials.json");
    if has_login {
        return Ok(lines);
    }
    let Some(grant) = crate::orca::read_capped(&from, 1024 * 1024)? else {
        return Ok(lines);
    };
    let grant = SecretString::new(grant);
    fsx::create_dir_all(&paths.config_dir, 0o700)?;
    fsx::write_atomic(
        &paths.credentials_path,
        grant.expose().as_bytes(),
        fsx::WriteOpts::PRIVATE,
    )?;
    lines.push(format!(
        "~/.claude had no login: copied {}'s into ~/.claude/.credentials.json",
        floor_dir.display()
    ));
    let ident = crate::orca::runtime::read_json_object(&floor_dir.join(".claude.json"))
        .and_then(|m| m.get("oauthAccount").cloned())
        .filter(|v| !v.is_null());
    if let Some(v) = ident
        && !matches!(read_runtime_identity(paths), RuntimeIdentity::Present(_))
    {
        let held = super::hold_config_lock(&paths.config_path, B2_WAIT)?;
        held.locks.touch();
        runtime::restore_identity(paths, Some(&v))?;
        lines.push(format!(
            "set {}'s oauthAccount from the floor profile's",
            paths.config_path.display()
        ));
    }
    Ok(lines)
}

/// The store's shape for the gate, from a snapshot.
pub(crate) fn store_kind(ctx: &Context, view: &OrcaView) -> StoreKind {
    let access = ctx.user_data.store_access_allowed();
    if store_less(ctx.os(), view.running, view.store.is_some(), access) {
        StoreKind::StoreLess
    } else if view.store.is_none() {
        StoreKind::Missing
    } else {
        let gate = offline_d_gate(ctx.os(), view.version_ok, access);
        if view.sqlite_state {
            StoreKind::Sqlite(gate)
        } else {
            StoreKind::Json(gate)
        }
    }
}

/// The active host account Orca confirms: a running Orca's list (only when
/// RPC answered), else the store's (or export's) active id; either way only
/// when it names a host record.
pub(crate) fn confirmed_active(view: &OrcaView) -> Option<&str> {
    if view.running && view.rpc_error.is_some() {
        return None;
    }
    view.active_id
        .as_deref()
        .filter(|id| view.host_accounts().any(|a| a.id == *id))
}

/// Run the cutover once its gate passes: the four steps of design section
/// 3, each safe to stop after. Returns whether the cutover is recorded.
pub(crate) fn cutover(
    env: &HostEnv,
    legacy: &Legacy,
    dry_run: bool,
    now: i64,
    child: &LaunchChild,
    st: &mut MigrationState,
    report: &mut Report,
) -> bool {
    crate::usage::reach::note("migrate-cutover");
    let home = &env.home;
    let procs = SystemProcs;
    // Step 2 writes the implicit ~/.claude whatever this shell exports.
    let mut ienv = env.clone();
    ConfigDirPin::Unset.apply_to(&mut ienv);
    let ctx = Context::from_env(ienv, &procs);
    let view = match crate::orca::snapshot(&SnapshotOptions::default()) {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(format!("cutover: {e}"));
            return false;
        }
    };
    let orca = OrcaAt::of(&crate::launch_context::orca_main_dir(env), view.running);
    let floor = session_floor();
    let floor_err = floor.as_ref().err().map(|e| e.to_string());
    let floor_val = floor.as_ref().ok().cloned().flatten();
    let active = confirmed_active(&view).map(str::to_owned);
    let store = store_kind(&ctx, &view);
    let orca_in_home =
        matches!(&orca, OrcaAt::Dir(d) if lexical(d) == lexical(&home.join(".claude")));
    let neutralise_needed = neutralise_needed(
        &ctx,
        &view,
        active.as_deref(),
        orca_in_home,
        &store,
        dry_run,
    );
    // The launch's own claude counts before it registers: the post-spawn
    // run starts right after the spawn.
    let live_implicit = child.uses(&home.join(".claude"))
        || registry_users(ctx.os(), &home.join(".claude"), home, &procs).is_some();
    let dirs: Vec<PathBuf> = legacy.profiles.iter().map(|p| p.dir.clone()).collect();
    let plan = cutover_gate(&CutoverFacts {
        carried: st.phase >= Phase::Cutover,
        floor: match &floor_err {
            Some(e) => Err(e.as_str()),
            None => Ok(floor_val.as_deref()),
        },
        legacy_dirs: &dirs,
        home,
        orca: &orca,
        listed: view.running && view.rpc_error.is_none(),
        active: active.as_deref(),
        store: &store,
        neutralise_needed,
        live_implicit,
    });
    let (neutralise_step, materialise, clear_floor) = match plan {
        CutoverPlan::Wait(why) => {
            row(report, home, format!("waits: {why}"));
            report.pending.push(format!("cutover: {why}"));
            st.set_step("cutover", StepStatus::Pending, Some("deferred"));
            return false;
        }
        CutoverPlan::Go {
            neutralise,
            materialise,
            clear_floor,
        } => (neutralise, materialise, clear_floor),
    };
    let mut what = Vec::new();
    // With Orca stopped, step 2 copies the active account's stash into
    // `~/.claude`: every legacy dir still holding that login must be idle
    // and no fresher than the stash first.
    if let Materialise::Switch(id) | Materialise::Export(id) = &materialise
        && let Err(why) = holders_settled(
            &ctx, &procs, &view, legacy, id, child, dry_run, &mut what, report,
        )
    {
        row(report, home, format!("waits: {why}"));
        report.pending.push(format!("cutover: {why}"));
        st.set_step("cutover", StepStatus::Pending, Some("deferred"));
        return false;
    }
    if dry_run {
        if neutralise_step {
            what.push(
                "neutralise ~/.claude (quarantine ~/.claude/.credentials.json if it is not the \
                 active stash's grant, drop oauthAccount)"
                    .to_owned(),
            );
        }
        match &materialise {
            Materialise::Nothing => {}
            Materialise::Switch(id) | Materialise::Export(id) => {
                what.push(format!("put Orca's active account {id} into ~/.claude"))
            }
            Materialise::StoreLess => {
                what.push("copy the floor profile's login into ~/.claude if it has none".into())
            }
        }
        if clear_floor {
            what.push("clear the login session's CLAUDE_CONFIG_DIR".into());
        }
        what.push("record the cutover".into());
        row(report, home, format!("would {}", what.join(", then ")));
        return false;
    }
    let now_ms = now.saturating_mul(1000);
    // Step 1.
    if neutralise_step {
        match neutralise(&ctx, &view, active.as_deref(), now_ms) {
            Ok(lines) => {
                for l in lines {
                    row(report, home, l.clone());
                    report.changed.push(l);
                }
            }
            Err(e) => {
                failed(report, st, "cutover", "cutover: ~/.claude".into(), &e, now);
                return false;
            }
        }
    }
    // Step 2. Never with a refresh: until stage C retires them, legacy dirs
    // may hold the same refresh token, and a refresh here would rotate it
    // away from them. The switch env counts a claude as live, which uses
    // Orca's order and skips the refresh; the stash is copied as it is.
    let http = SystemHttp::from_env();
    let step2: anyhow::Result<Option<String>> = match &materialise {
        Materialise::Nothing => Ok(None),
        Materialise::Switch(id) => ctx
            .with_switch_env_child(&procs, &http, true, |e| switch::switch(e, id))
            .map_err(anyhow::Error::from)
            .and_then(|r| step2_line(id, &r.outcome)),
        Materialise::Export(id) => ctx
            .with_switch_env_child(&procs, &http, true, |e| {
                switch::materialize_export_active(e, id)
            })
            .map_err(anyhow::Error::from)
            .and_then(|r| step2_line(id, &r.outcome)),
        Materialise::StoreLess => {
            let floor_dir = legacy
                .floor
                .as_deref()
                .and_then(|n| legacy.profiles.iter().find(|p| p.name == n))
                .map(|p| p.dir.as_path());
            store_less_login(&ctx, floor_dir).map(|l| (!l.is_empty()).then(|| l.join("; ")))
        }
    };
    match step2 {
        Ok(Some(l)) => {
            row(report, home, l.clone());
            report.changed.push(l);
        }
        Ok(None) => {}
        Err(e) => {
            failed(report, st, "cutover", "cutover: ~/.claude".into(), &e, now);
            return false;
        }
    }
    // Step 3.
    if clear_floor {
        if let Err(e) = unset_floor_env() {
            report.errors.push(format!(
                "cutover: cannot clear the login session's CLAUDE_CONFIG_DIR ({e}); run \
                 `{FLOOR_UNSET_HINT}`"
            ));
            st.set_step("cutover", StepStatus::Error, Some("io"));
            return false;
        }
        let l = format!(
            "cleared the login session's CLAUDE_CONFIG_DIR (was {})",
            floor_val.as_deref().unwrap_or("").trim()
        );
        row(report, home, l.clone());
        report.changed.push(l);
        crate::e2e::point("migrate-cutover-cleared");
    }
    // Step 4.
    st.cutover = Some(Cutover {
        at: now,
        boot_id: fsx::boot_id(),
    });
    st.set_step("cutover", StepStatus::Done, None);
    let l = "recorded the cutover: claude runs in ~/.claude from now on".to_owned();
    row(report, home, l.clone());
    report.changed.push(l);
    if let OrcaAt::Dir(d) = &orca
        && !orca_in_home
    {
        report.pending.push(format!(
            "restart Orca to finish: it still runs in {} and moves to ~/.claude at its next start \
             (csm never restarts it)",
            d.display()
        ));
    }
    true
}

/// Step 2's changed line, or its failure. Pure.
fn step2_line(id: &str, outcome: &switch::Outcome) -> anyhow::Result<Option<String>> {
    match outcome {
        switch::Outcome::Switched => Ok(Some(format!(
            "~/.claude now holds Orca's active account ({id})"
        ))),
        switch::Outcome::AlreadyActive => Ok(None),
        switch::Outcome::Uncertain(why) => Err(anyhow::Error::from(OrcaError::Refused(format!(
            "Orca came up while ~/.claude was written ({why}); `csm accounts doctor` reconciles"
        )))),
    }
}

/// After the cutover: a floor set again (a LaunchAgent or login script not
/// yet removed) is cleared again, and the record moves to this boot, since
/// what started in this boot may carry the value; the note names the
/// writer. A floor naming something else is left alone and reported.
pub(crate) fn recheck_floor(
    env: &HostEnv,
    legacy: &Legacy,
    dry_run: bool,
    st: &mut MigrationState,
    report: &mut Report,
) {
    let home = &env.home;
    let v = match session_floor() {
        Ok(None) => return,
        Ok(Some(v)) => v,
        Err(e) => {
            report.pending.push(format!(
                "cannot read the login session's CLAUDE_CONFIG_DIR ({e})"
            ));
            return;
        }
    };
    let Some(p) = set_config_dir(Some(&v)) else {
        return;
    };
    let mut known: Vec<PathBuf> = legacy.profiles.iter().map(|p| p.dir.clone()).collect();
    known.push(home.join(".claude"));
    if !names_one_of(p, &known) {
        report.pending.push(format!(
            "the login session's CLAUDE_CONFIG_DIR is {} again, which is no recorded profile \
             dir; clear it by hand (`{FLOOR_UNSET_HINT}`)",
            p.display()
        ));
        return;
    }
    let agents = if env.os == HostOs::MacOs {
        floor_agents(home)
    } else {
        Vec::new()
    };
    let writer = floor_writer(env.os, &agents);
    if dry_run {
        row(
            report,
            home,
            format!("would clear the login session's CLAUDE_CONFIG_DIR again ({writer})"),
        );
        return;
    }
    if let Err(e) = unset_floor_env() {
        report.errors.push(format!(
            "the login session's CLAUDE_CONFIG_DIR was set again (to {}) and cannot be cleared \
             ({e}); run `{FLOOR_UNSET_HINT}`",
            p.display()
        ));
        return;
    }
    if let Some(c) = st.cutover.as_mut() {
        c.boot_id = fsx::boot_id();
    }
    let l = format!(
        "the login session's CLAUDE_CONFIG_DIR was set again (to {}) and was cleared again",
        p.display()
    );
    row(report, home, l.clone());
    report.changed.push(l);
    report
        .pending
        .push(format!("the floor keeps coming back: {writer}"));
}
