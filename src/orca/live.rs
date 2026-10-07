//! Is Orca running? Fail closed.
//!
//! csm may write Orca's store, stashes or runtime dir only while Orca is
//! stopped, so this check errs toward "running" (design section 2). Orca
//! counts as running when ANY of these holds:
//! - (a) POSIX: `<userData>/SingletonLock`, the symlink Electron's single
//!   instance lock leaves (target `<hostname>-<pid>`), names this host and a
//!   live pid;
//! - (b) the `orca-runtime.json` pid is alive and is Orca's main executable;
//! - (c) the process table shows Orca's main executable;
//! - (d) any probe is unreadable (a lock that is not a symlink, a runtime
//!   file that does not parse, a process table that cannot be read, a
//!   process named like Orca whose executable cannot be read).
//!
//! Orca's main executable: macOS `<bundle>.app/Contents/MacOS/Orca`, Linux
//! `orca-ide` (electron-builder's `linux.executableName`, chosen so the
//! package does not claim GNOME Orca's `/usr/bin/orca`; `orca`/`Orca` still
//! match, which only errs toward "running"), Windows `Orca.exe`; in
//! every case without a `--type=` argument (renderer, GPU, utility and
//! crashpad helpers carry one) and without a script argument (the PTY
//! daemon runs `daemon-entry.js` under `ELECTRON_RUN_AS_NODE`, and Orca's CLI
//! launcher runs `-e <script>` the same way). Helpers never count; Claude
//! panes the daemon keeps alive after Orca quits are live sessions in `D`
//! instead ([`super::runtime`]).
//!
//! The write protocol's L0/L1/L2 checks go through [`Liveness`], which
//! returns a [`LiveMark`]: the verdict plus the instance fingerprint
//! (`SingletonLock` target, runtime-file `runtimeId`/`pid`/`startedAt`), so
//! an Orca that started and quit between two checks still shows as a
//! change.
//!
//! Every process query goes through [`ProcFacts`], so tests inject a fake
//! process table. Under `cfg(test)` the real [`SystemProcs`] reports an
//! empty (readable) table, so no test can see an Orca running on the
//! developer's machine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::rpc::{RuntimeMetadata, read_runtime_metadata};
use super::userdata::HostOs;
use crate::platform::proc::ProcInfo;

/// Electron's lock file in userData (POSIX).
pub const SINGLETON_LOCK: &str = "SingletonLock";

// ─── process facts seam ───────────────────────────────────────────────────────

/// The process queries every liveness decision uses.
pub trait ProcFacts {
    /// Is `pid` running?
    fn alive(&self, pid: u32) -> bool;
    /// One process's identity, `None` when it is gone.
    fn probe(&self, pid: u32) -> Option<ProcInfo>;
    /// The whole process table, `None` when it cannot be read.
    fn table(&self) -> Option<Vec<ProcInfo>>;
    /// A process's start time in epoch seconds.
    fn start_time(&self, pid: u32) -> Option<u64> {
        self.probe(pid).map(|p| p.start_time)
    }
    /// A process's start time in clock ticks since boot (`/proc/<pid>/stat`
    /// field 22), the unit Claude Code writes to a Linux session record's
    /// `procStart`. `None` off Linux or when it cannot be read.
    fn start_ticks(&self, pid: u32) -> Option<u64> {
        let _ = pid;
        None
    }
    /// A process's environment block, `None` when it cannot be read (the
    /// default: a source that cannot read environments reads none).
    fn environ(&self, pid: u32) -> Option<Vec<OsString>> {
        let _ = pid;
        None
    }
}

/// The real machine, through `sysinfo`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcs;

impl ProcFacts for SystemProcs {
    fn alive(&self, pid: u32) -> bool {
        crate::platform::proc::is_running(pid)
    }

    fn probe(&self, pid: u32) -> Option<ProcInfo> {
        if pid == 0 {
            return None;
        }
        crate::platform::proc::probe(pid)
    }

    #[cfg(target_os = "linux")]
    fn start_ticks(&self, pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat_start_ticks(&stat)
    }

    #[cfg(not(test))]
    fn table(&self) -> Option<Vec<ProcInfo>> {
        let t = crate::platform::proc::snapshot();
        // An empty table means sysinfo could not read it: csm itself runs.
        if t.is_empty() {
            return None;
        }
        // The e2e build sees only the sandbox's processes, never the
        // machine's real Orca.
        if crate::e2e::ENABLED {
            return Some(
                t.into_iter()
                    .filter(|p| crate::e2e::in_sandbox(p.exe.as_deref()))
                    .collect(),
            );
        }
        Some(t)
    }

    /// Test build: an empty, readable table. A test never sees the real
    /// machine's Orca.
    #[cfg(test)]
    fn table(&self) -> Option<Vec<ProcInfo>> {
        crate::usage::reach::note("proc-sweep");
        Some(Vec::new())
    }

    #[cfg(not(test))]
    fn environ(&self, pid: u32) -> Option<Vec<OsString>> {
        if pid == 0 {
            return None;
        }
        crate::platform::proc::environ(pid)
    }

    /// Test build: never the real machine's environments.
    #[cfg(test)]
    fn environ(&self, _pid: u32) -> Option<Vec<OsString>> {
        None
    }
}

/// Field 22 (`starttime`, clock ticks since boot) of a `/proc/<pid>/stat`
/// line. `comm` (field 2) is parenthesised and may hold spaces and
/// parentheses, so the fields are counted after the LAST `)`. Pure.
#[cfg(any(target_os = "linux", test))]
pub fn stat_start_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the `)`: field 3 (state) is the first token, so field 22 is the
    // 20th.
    rest.split_whitespace().nth(19)?.parse().ok()
}

// ─── main-executable match ────────────────────────────────────────────────────

/// Is a process Orca's main process?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMatch {
    Yes,
    No,
    /// Named like Orca, but its executable path is unreadable.
    Unknown,
}

fn is_helper_argv(cmd: &[std::ffi::OsString]) -> bool {
    cmd.iter().skip(1).any(|a| {
        let a = a.to_string_lossy();
        a.starts_with("--type=") || a == "-e" || a.ends_with(".js")
    })
}

fn name_is(os: HostOs, name: &str) -> bool {
    match os {
        HostOs::MacOs => name == "Orca",
        HostOs::Linux => name == "orca-ide" || name == "orca" || name == "Orca",
        HostOs::Windows => name.eq_ignore_ascii_case("Orca.exe"),
    }
}

/// The last path component, split on both `/` and `\\` so the pure matcher
/// gives the same answer for a Windows path on any host.
fn file_name(p: &Path) -> Option<&str> {
    let s = p.to_str()?;
    s.rsplit(['/', '\\']).next().filter(|n| !n.is_empty())
}

/// The macOS bundle layout `<X>.app/Contents/MacOS/Orca`.
fn is_macos_main_path(exe: &Path) -> bool {
    let mut up = exe.ancestors().skip(1);
    let (Some(macos), Some(contents), Some(bundle)) = (up.next(), up.next(), up.next()) else {
        return false;
    };
    file_name(exe) == Some("Orca")
        && file_name(macos) == Some("MacOS")
        && file_name(contents) == Some("Contents")
        && bundle.extension().is_some_and(|e| e == "app")
}

/// Is `p` Orca's main process on `os`? Pure.
pub fn orca_main_match(os: HostOs, p: &ProcInfo) -> MainMatch {
    if is_helper_argv(&p.cmd) {
        return MainMatch::No;
    }
    match &p.exe {
        Some(exe) => {
            let ok = match os {
                HostOs::MacOs => is_macos_main_path(exe),
                HostOs::Linux | HostOs::Windows => file_name(exe).is_some_and(|n| name_is(os, n)),
            };
            if ok { MainMatch::Yes } else { MainMatch::No }
        }
        None if name_is(os, &p.name) => MainMatch::Unknown,
        None => MainMatch::No,
    }
}

/// The `.app` bundle of a macOS main executable path. Pure.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    is_macos_main_path(exe).then(|| exe.ancestors().nth(3).map(Path::to_path_buf))?
}

// ─── probes ───────────────────────────────────────────────────────────────────

/// What `SingletonLock` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingletonProbe {
    Absent,
    Target { host: String, pid: u32 },
    Unreadable(String),
}

/// `<hostname>-<pid>`; the host may contain dashes. Pure.
pub fn parse_singleton_target(s: &str) -> Option<(String, u32)> {
    let (host, pid) = s.rsplit_once('-')?;
    let pid: u32 = pid.parse().ok()?;
    (!host.is_empty() && pid > 0).then(|| (host.to_owned(), pid))
}

/// Read `<userData>/SingletonLock`. Windows has no such file.
pub fn read_singleton(user_data: &Path) -> SingletonProbe {
    if cfg!(windows) {
        return SingletonProbe::Absent;
    }
    let path = user_data.join(SINGLETON_LOCK);
    match std::fs::read_link(&path) {
        Ok(t) => match t.to_str().and_then(parse_singleton_target) {
            Some((host, pid)) => SingletonProbe::Target { host, pid },
            None => SingletonProbe::Unreadable("SingletonLock target does not parse".into()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SingletonProbe::Absent,
        Err(e) => SingletonProbe::Unreadable(format!("SingletonLock: {}", e.kind())),
    }
}

/// What `orca-runtime.json` says.
#[derive(Debug, Clone)]
pub enum RuntimeProbe {
    Absent,
    Present(RuntimeMetadata),
    Unreadable(String),
}

pub fn read_runtime(user_data: &Path) -> RuntimeProbe {
    match read_runtime_metadata(user_data) {
        Ok(None) => RuntimeProbe::Absent,
        Ok(Some(m)) => RuntimeProbe::Present(m),
        Err(e) => RuntimeProbe::Unreadable(e.to_string()),
    }
}

/// This machine's host name: as Chromium's lock writes it (POSIX), and as
/// Node's `os.hostname()` returns it, which Claude Code lowercases into a
/// Windows session record's pidDomain (`win32:<host>`). On Windows that is
/// libuv's `GetHostNameW`, the DNS host name, read here through
/// `GetComputerNameExW(ComputerNameDnsHostname)` (the NetBIOS name would be
/// upper-cased and cut at 15 characters). `None` when it cannot be read.
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
    }
    #[cfg(windows)]
    {
        windows_dns_hostname()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(windows)]
fn windows_dns_hostname() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };
    let mut size: u32 = 0;
    // SAFETY: a size query with a null buffer is the documented pattern; the
    // call only writes `size`.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut size) };
    if size == 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0u16; size as usize];
    // SAFETY: `buf` holds `size` u16s, the length the call was told.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut size) };
    if ok == 0 {
        return None;
    }
    buf.truncate(size as usize);
    let name = String::from_utf16(&buf).ok()?;
    (!name.is_empty()).then_some(name)
}

/// Every input of the running check.
#[derive(Debug, Clone)]
pub struct LiveInputs {
    pub os: HostOs,
    pub hostname: Option<String>,
    pub singleton: SingletonProbe,
    pub runtime: RuntimeProbe,
}

impl LiveInputs {
    /// Gather the file probes for `user_data`.
    pub fn gather(os: HostOs, user_data: &Path) -> LiveInputs {
        LiveInputs {
            os,
            hostname: hostname(),
            singleton: read_singleton(user_data),
            runtime: read_runtime(user_data),
        }
    }
}

// ─── decision ─────────────────────────────────────────────────────────────────

/// Why Orca counts as running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningReason {
    /// (a) `SingletonLock` names this host and a live pid.
    SingletonLock { pid: u32 },
    /// (b) the runtime file's pid is Orca's live main process.
    RuntimePid { pid: u32 },
    /// (c) the process table shows Orca's main process.
    ProcessTable { pid: u32 },
    /// (d) a probe could not be read.
    Unreadable(String),
}

impl std::fmt::Display for RunningReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunningReason::SingletonLock { pid } => write!(f, "SingletonLock names live pid {pid}"),
            RunningReason::RuntimePid { pid } => write!(f, "orca-runtime.json pid {pid} is Orca"),
            RunningReason::ProcessTable { pid } => write!(f, "Orca main process {pid}"),
            RunningReason::Unreadable(why) => write!(f, "cannot tell ({why})"),
        }
    }
}

/// The running verdict.
#[derive(Debug, Clone)]
pub struct LiveReport {
    pub running: bool,
    pub reasons: Vec<RunningReason>,
    /// Orca's main pid when one was identified.
    pub main_pid: Option<u32>,
    /// Its executable path, when read.
    pub main_exe: Option<PathBuf>,
    /// The runtime file (RPC metadata), when present.
    pub runtime: Option<RuntimeMetadata>,
}

/// Decide whether Orca runs. Pure over `inputs` and `facts`.
pub fn classify(inputs: &LiveInputs, facts: &dyn ProcFacts) -> LiveReport {
    let mut reasons = Vec::new();
    let mut main: Option<ProcInfo> = None;

    // (a)
    match &inputs.singleton {
        SingletonProbe::Absent => {}
        SingletonProbe::Unreadable(why) => reasons.push(RunningReason::Unreadable(why.clone())),
        SingletonProbe::Target { host, pid } => match &inputs.hostname {
            None => reasons.push(RunningReason::Unreadable("host name unknown".into())),
            Some(h) if h == host => {
                if facts.alive(*pid) {
                    reasons.push(RunningReason::SingletonLock { pid: *pid });
                }
            }
            // Another host's lock (a shared home): not this machine's Orca.
            Some(_) => {}
        },
    }

    // (b)
    let runtime = match &inputs.runtime {
        RuntimeProbe::Absent => None,
        RuntimeProbe::Unreadable(why) => {
            reasons.push(RunningReason::Unreadable(why.clone()));
            None
        }
        RuntimeProbe::Present(meta) => {
            if meta.pid != 0 && facts.alive(meta.pid) {
                match facts.probe(meta.pid) {
                    // Gone between the two queries.
                    None => {}
                    Some(p) => match orca_main_match(inputs.os, &p) {
                        MainMatch::Yes => {
                            reasons.push(RunningReason::RuntimePid { pid: p.pid });
                            main = Some(p);
                        }
                        MainMatch::Unknown => reasons.push(RunningReason::Unreadable(format!(
                            "executable of runtime pid {}",
                            meta.pid
                        ))),
                        // A reused pid.
                        MainMatch::No => {}
                    },
                }
            }
            Some(meta.clone())
        }
    };

    // (c)
    match facts.table() {
        None => reasons.push(RunningReason::Unreadable("process table".into())),
        Some(table) => {
            for p in table {
                match orca_main_match(inputs.os, &p) {
                    MainMatch::Yes => {
                        reasons.push(RunningReason::ProcessTable { pid: p.pid });
                        if main.is_none() {
                            main = Some(p);
                        }
                    }
                    MainMatch::Unknown => reasons.push(RunningReason::Unreadable(format!(
                        "executable of process {}",
                        p.pid
                    ))),
                    MainMatch::No => {}
                }
            }
        }
    }

    LiveReport {
        running: !reasons.is_empty(),
        reasons,
        main_pid: main.as_ref().map(|p| p.pid),
        main_exe: main.and_then(|p| p.exe),
        runtime,
    }
}

/// The running check against the real machine for `user_data`.
pub fn check(os: HostOs, user_data: &Path, facts: &dyn ProcFacts) -> LiveReport {
    classify(&LiveInputs::gather(os, user_data), facts)
}

// ─── liveness marks (L0 / L1 / L2) ────────────────────────────────────────────

/// One liveness check: the verdict and the instance fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveMark {
    pub running: bool,
    /// Why it counts as running (display text, no secrets).
    pub reasons: Vec<String>,
    /// `SingletonLock` as read: `host-pid`, or the unreadable reason.
    pub singleton: Option<String>,
    /// The runtime file's `(runtimeId, pid, startedAt)`.
    pub runtime: Option<(String, u32, Option<i64>)>,
}

impl LiveMark {
    /// A stopped Orca with no lock and no runtime file.
    #[cfg(test)]
    pub fn stopped() -> LiveMark {
        LiveMark::default()
    }

    /// Same Orca instance state as `earlier`: the lock and the runtime file
    /// read the same.
    pub fn same_instance_as(&self, earlier: &LiveMark) -> bool {
        self.singleton == earlier.singleton && self.runtime == earlier.runtime
    }

    /// Does this later check allow a write that `l0` allowed? Orca stopped
    /// and nothing about its instance changed.
    pub fn still_clear_of(&self, l0: &LiveMark) -> bool {
        !self.running && self.same_instance_as(l0)
    }

    /// The mark of one gathered probe set. Pure.
    pub fn of(inputs: &LiveInputs, report: &LiveReport) -> LiveMark {
        let singleton = match &inputs.singleton {
            SingletonProbe::Absent => None,
            SingletonProbe::Target { host, pid } => Some(format!("{host}-{pid}")),
            SingletonProbe::Unreadable(why) => Some(format!("unreadable: {why}")),
        };
        let runtime = match &inputs.runtime {
            RuntimeProbe::Absent => None,
            RuntimeProbe::Present(m) => Some((m.runtime_id.clone(), m.pid, m.started_at)),
            RuntimeProbe::Unreadable(why) => Some((format!("unreadable: {why}"), 0, None)),
        };
        LiveMark {
            running: report.running,
            reasons: report.reasons.iter().map(ToString::to_string).collect(),
            singleton,
            runtime,
        }
    }
}

/// The source of liveness marks (the real machine, or a script in tests).
pub trait Liveness {
    fn mark(&self) -> LiveMark;
}

/// The real check for one userData.
pub struct SystemLiveness<'a> {
    pub os: HostOs,
    pub user_data: PathBuf,
    pub facts: &'a dyn ProcFacts,
}

impl Liveness for SystemLiveness<'_> {
    fn mark(&self) -> LiveMark {
        let inputs = LiveInputs::gather(self.os, &self.user_data);
        let report = classify(&inputs, self.facts);
        LiveMark::of(&inputs, &report)
    }
}

// ─── who uses a config dir ────────────────────────────────────────────────────

/// Who uses a config dir: the migration's retire gate asks before it
/// renames a legacy dir. Anything short of a certain [`DirUsers::Free`]
/// counts as live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirUsers {
    /// No live claude registered in the dir and no claude or csm process
    /// whose environment names it.
    Free,
    /// A live user, and which.
    Live(String),
    /// Could not tell (an unreadable table or environment): counts as live.
    Unknown(String),
}

/// A process whose environment may pin a config dir for a claude: claude
/// itself (the native build, or node/bun running Claude Code's script) or
/// a csm (a `csm run` supervisor between two hops, the `claude` alias).
/// Pure.
pub fn claude_like(p: &ProcInfo) -> bool {
    let stem = p
        .exe
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .map(|s| crate::platform::proc_check::bare_basename(s).to_owned())
        .unwrap_or_else(|| crate::platform::proc_check::bare_basename(&p.name).to_owned());
    let name = crate::platform::proc_check::bare_basename(&p.name);
    let is = |w: &str| stem.eq_ignore_ascii_case(w) || name.eq_ignore_ascii_case(w);
    if is("claude") || is("csm") {
        return true;
    }
    (is("node") || is("bun"))
        && p.cmd
            .iter()
            .skip(1)
            .any(|a| a.to_string_lossy().contains("claude"))
}

/// This process and its ancestors in `table`. Pure.
fn self_and_ancestors(table: &[ProcInfo], this: u32) -> Vec<u32> {
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
    mine
}

/// The same dir: equal once trimmed of blanks and trailing separators, or
/// once both resolve.
fn same_path(a: &Path, b: &Path) -> bool {
    let trim = |p: &Path| {
        PathBuf::from(
            p.to_string_lossy()
                .trim()
                .trim_end_matches(['/', '\\'])
                .to_owned(),
        )
    };
    trim(a) == trim(b)
        || std::fs::canonicalize(a)
            .ok()
            .is_some_and(|x| std::fs::canonicalize(b).ok() == Some(x))
}

/// The non-blank `CLAUDE_CONFIG_DIR` an environment block sets.
fn environ_config_dir(env: &[OsString]) -> Option<PathBuf> {
    env.iter().find_map(|kv| {
        let kv = kv.to_string_lossy();
        let (k, v) = kv.split_once('=')?;
        (k.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR") && !v.trim().is_empty())
            .then(|| PathBuf::from(v.trim()))
    })
}

/// Does a claude with this environment run in `dir`: its non-blank
/// `CLAUDE_CONFIG_DIR` names `dir`, or it sets none and `dir` is the
/// implicit `~/.claude`?
fn environ_uses(env: &[OsString], dir: &Path, home: &Path) -> bool {
    match environ_config_dir(env) {
        Some(_) => environ_names(env, dir),
        None => same_path(&home.join(".claude"), dir),
    }
}

/// [`registry_users`]'s core. `scan` is `dir/sessions` scanned (`None`:
/// it could not be listed), `environ` one process's environment, `alive`
/// whether a pid still runs. Pure over its facts.
///
/// In the legacy layout every profile dir's `sessions` links to one
/// machine-wide registry (and after B1 `~/.claude/sessions` is that
/// registry), so a live record there says only that some claude runs
/// somewhere. Each live record's pid is therefore attributed by its
/// environment. A record that cannot be attributed (an unreadable
/// environment of a pid still running, another pid domain, a file that
/// does not parse, a registry that cannot be listed) counts as the dir's:
/// fail closed.
pub fn registry_users_in(
    dir: &Path,
    home: &Path,
    scan: Option<&super::runtime::SessionScan>,
    environ: &dyn Fn(u32) -> Option<Vec<OsString>>,
    alive: &dyn Fn(u32) -> bool,
) -> Option<String> {
    let reg = dir.join("sessions");
    let Some(scan) = scan else {
        return Some(format!("{} cannot be listed", reg.display()));
    };
    if scan.unreadable > 0 {
        return Some(format!(
            "a session record in {} cannot be read",
            reg.display()
        ));
    }
    if let Some(r) = scan.unverifiable.first() {
        return Some(format!(
            "session pid {} in {} cannot be verified",
            r.pid,
            reg.display()
        ));
    }
    for r in &scan.live {
        match environ(r.pid) {
            Some(env) if environ_uses(&env, dir, home) => {
                return Some(format!(
                    "a claude session (pid {}) runs in {}",
                    r.pid,
                    dir.display()
                ));
            }
            Some(_) => {}
            None if !alive(r.pid) => {}
            None => {
                return Some(format!(
                    "the environment of session pid {} cannot be read, so it may run in {}",
                    r.pid,
                    dir.display()
                ));
            }
        }
    }
    None
}

/// Is a claude registered in `dir/sessions` running in `dir` (see
/// [`registry_users_in`])? `Some(why)` when it is, or may be.
pub fn registry_users(
    os: HostOs,
    dir: &Path,
    home: &Path,
    procs: &dyn ProcFacts,
) -> Option<String> {
    let domain = super::runtime::this_pid_domain(os);
    let scan = super::runtime::scan_sessions(&dir.join("sessions"), &domain, procs).ok();
    registry_users_in(dir, home, scan.as_ref(), &|p| procs.environ(p), &|p| {
        procs.alive(p)
    })
}

/// Does an environment block set `CLAUDE_CONFIG_DIR` to `dir` (trimmed,
/// without a trailing separator, or the same dir once both resolve)?
fn environ_names(env: &[OsString], dir: &Path) -> bool {
    let trim = |s: &str| PathBuf::from(s.trim().trim_end_matches(['/', '\\']));
    let want = trim(&dir.to_string_lossy());
    let real = std::fs::canonicalize(dir).ok();
    env.iter().any(|kv| {
        let kv = kv.to_string_lossy();
        let Some((k, v)) = kv.split_once('=') else {
            return false;
        };
        if !k.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR") || v.trim().is_empty() {
            return false;
        }
        let v = trim(v);
        v == want || (real.is_some() && std::fs::canonicalize(&v).ok() == real)
    })
}

/// One live process that uses a config dir, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirUser {
    pub pid: u32,
    pub name: String,
    /// Start time, already formatted for display (empty when unknown).
    pub started: String,
    /// The parent's process name when the table has it (a launcher such as
    /// a browser or an Orca helper).
    pub parent: Option<String>,
}

/// Most users a report lists before it says "and N more".
pub const MAX_LISTED_USERS: usize = 10;

/// What names the users of `dir`: one process reads "pid 4 (claude, ...)
/// runs with CLAUDE_CONFIG_DIR=dir", several read "N processes run with
/// CLAUDE_CONFIG_DIR=dir: ...", listing at most [`MAX_LISTED_USERS`]. Pure.
pub fn format_dir_users(dir: &Path, users: &[DirUser]) -> String {
    let one = |u: &DirUser| {
        let mut bits = vec![u.name.clone()];
        if !u.started.is_empty() {
            bits.push(format!("started {}", u.started));
        }
        if let Some(p) = &u.parent {
            bits.push(format!("parent {p}"));
        }
        format!("pid {} ({})", u.pid, bits.join(", "))
    };
    if let [u] = users {
        return format!("{} runs with CLAUDE_CONFIG_DIR={}", one(u), dir.display());
    }
    let mut list: Vec<String> = users.iter().take(MAX_LISTED_USERS).map(one).collect();
    if users.len() > MAX_LISTED_USERS {
        list.push(format!("and {} more", users.len() - MAX_LISTED_USERS));
    }
    format!(
        "{} processes run with CLAUDE_CONFIG_DIR={}: {}",
        users.len(),
        dir.display(),
        list.join("; ")
    )
}

/// [`dir_users`]'s core over its facts: `registered` is
/// [`super::context::live_claude_in`]'s answer (a scan error already counts
/// as live), `table` the process table, `this` csm's own pid (it and its
/// ancestors never count), `environ` one process's environment. Every live
/// user is listed, not the first one found.
pub fn dir_users_in(
    dir: &Path,
    registered: bool,
    table: Option<&[ProcInfo]>,
    this: u32,
    environ: &dyn Fn(u32) -> Option<Vec<OsString>>,
) -> DirUsers {
    let reg = format!(
        "a claude session is registered in {}",
        dir.join("sessions").display()
    );
    let Some(table) = table else {
        return if registered {
            DirUsers::Live(reg)
        } else {
            DirUsers::Unknown("the process table cannot be read".into())
        };
    };
    let mine = self_and_ancestors(table, this);
    let mut unknown = None;
    let mut users = Vec::new();
    for p in table
        .iter()
        .filter(|p| !mine.contains(&p.pid) && claude_like(p))
    {
        match environ(p.pid) {
            Some(env) if environ_names(&env, dir) => users.push(DirUser {
                pid: p.pid,
                name: p.name.clone(),
                started: started_text(p.start_time),
                parent: p
                    .ppid
                    .and_then(|pp| table.iter().find(|q| q.pid == pp))
                    .map(|q| q.name.clone()),
            }),
            Some(_) => {}
            None => {
                // Gone since the sweep, or unreadable: only a process still
                // there counts.
                unknown.get_or_insert(p.pid);
            }
        }
    }
    if !users.is_empty() {
        let list = format_dir_users(dir, &users);
        return DirUsers::Live(if registered {
            format!("{reg}; {list}")
        } else {
            list
        });
    }
    if registered {
        return DirUsers::Live(reg);
    }
    match unknown {
        Some(pid) => DirUsers::Unknown(format!(
            "the environment of pid {pid} cannot be read, so it may use {}",
            dir.display()
        )),
        None => DirUsers::Free,
    }
}

/// A process start time (epoch seconds) as local `MM-DD HH:MM`; empty for 0.
fn started_text(epoch: u64) -> String {
    i64::try_from(epoch)
        .ok()
        .filter(|e| *e > 0)
        .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Who uses `dir` right now (design section 2, C): a live claude
/// registered in `dir/sessions` that runs in `dir` ([`registry_users`]: the
/// legacy registry is shared, so a record elsewhere's claude does not
/// count), or a claude or csm process whose environment sets
/// `CLAUDE_CONFIG_DIR` to it. Unreadable counts as live.
pub fn dir_users(os: HostOs, dir: &Path, home: &Path, procs: &dyn ProcFacts) -> DirUsers {
    let registered = registry_users(os, dir, home, procs).is_some();
    let table = procs.table();
    dir_users_in(
        dir,
        registered,
        table.as_deref(),
        std::process::id(),
        &|pid| match procs.environ(pid) {
            Some(env) => Some(env),
            // Exited since the sweep: it uses nothing.
            None if !procs.alive(pid) => Some(Vec::new()),
            None => None,
        },
    )
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{FakeProcs, proc_info};

    const MAC_MAIN: &str = "/Applications/Orca.app/Contents/MacOS/Orca";

    #[test]
    fn stat_start_ticks_counts_fields_after_the_last_paren() {
        let line = "463492 (claude) S 1 463492 463492 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 11 0 1945239 1000 200 18446744073709551615 0 0";
        assert_eq!(stat_start_ticks(line), Some(1_945_239));
        // `comm` with spaces and parentheses.
        let odd = "42 (a) b (c)) R 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 777 1 1 0";
        assert_eq!(stat_start_ticks(odd), Some(777));
        assert_eq!(stat_start_ticks("42 (short) S 1 2"), None);
        assert_eq!(stat_start_ticks("no parens"), None);
    }

    fn inputs(singleton: SingletonProbe, runtime: RuntimeProbe) -> LiveInputs {
        LiveInputs {
            os: HostOs::MacOs,
            hostname: Some("host-a.local".into()),
            singleton,
            runtime,
        }
    }

    fn meta(pid: u32) -> RuntimeMetadata {
        crate::orca::rpc::parse_runtime_metadata(&format!(
            r#"{{"runtimeId":"rt","pid":{pid},"transports":[],"authToken":"t","startedAt":1}}"#
        ))
        .unwrap()
    }

    #[test]
    fn main_executable_match_per_os() {
        let mac = |exe: &str, args: &[&str]| {
            orca_main_match(HostOs::MacOs, &proc_info(1, "Orca", Some(exe), args))
        };
        assert_eq!(mac(MAC_MAIN, &[]), MainMatch::Yes);
        assert_eq!(
            mac(
                "/Users/example/Applications/Orca.app/Contents/MacOS/Orca",
                &[]
            ),
            MainMatch::Yes
        );
        // Helpers and node-mode children never count.
        assert_eq!(
            mac(
                "/Applications/Orca.app/Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper",
                &[]
            ),
            MainMatch::No
        );
        assert_eq!(mac(MAC_MAIN, &["--type=renderer"]), MainMatch::No);
        assert_eq!(
            mac(MAC_MAIN, &["/x/out/main/daemon-entry.js"]),
            MainMatch::No
        );
        assert_eq!(mac(MAC_MAIN, &["-e", "require('x')"]), MainMatch::No);
        assert_eq!(mac("/usr/local/bin/Orca", &[]), MainMatch::No);
        // Named like Orca, executable unreadable: unknown (fails closed).
        let p = proc_info(1, "Orca", None, &[]);
        assert_eq!(orca_main_match(HostOs::MacOs, &p), MainMatch::Unknown);
        let p = proc_info(1, "zsh", None, &[]);
        assert_eq!(orca_main_match(HostOs::MacOs, &p), MainMatch::No);

        // Orca ships its Linux main as `orca-ide` (electron-builder
        // `linux.executableName`), deb/rpm under /opt/Orca and AppImage
        // under a /tmp/.mount_* dir.
        let lin = proc_info(1, "orca-ide", Some("/opt/Orca/orca-ide"), &[]);
        assert_eq!(orca_main_match(HostOs::Linux, &lin), MainMatch::Yes);
        let lin = proc_info(1, "orca-ide", Some("/tmp/.mount_OrcaAb/orca-ide"), &[]);
        assert_eq!(orca_main_match(HostOs::Linux, &lin), MainMatch::Yes);
        let lin = proc_info(1, "orca-ide", None, &[]);
        assert_eq!(orca_main_match(HostOs::Linux, &lin), MainMatch::Unknown);
        let lin = proc_info(
            1,
            "orca-ide",
            Some("/opt/Orca/orca-ide"),
            &["--type=gpu-process"],
        );
        assert_eq!(orca_main_match(HostOs::Linux, &lin), MainMatch::No);
        let win = proc_info(1, "Orca.exe", Some("C:\\Programs\\Orca\\Orca.exe"), &[]);
        assert_eq!(orca_main_match(HostOs::Windows, &win), MainMatch::Yes);
        let win = proc_info(
            1,
            "Orca.exe",
            Some("C:\\Programs\\Orca\\Orca.exe"),
            &["--type=crashpad-handler"],
        );
        assert_eq!(orca_main_match(HostOs::Windows, &win), MainMatch::No);

        assert_eq!(
            bundle_of(Path::new(MAC_MAIN)),
            Some(PathBuf::from("/Applications/Orca.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/bin/orca")), None);
    }

    #[test]
    fn singleton_target_parsing() {
        assert_eq!(
            parse_singleton_target("host-a.local-4242"),
            Some(("host-a.local".into(), 4242))
        );
        assert_eq!(parse_singleton_target("nohyphen"), None);
        assert_eq!(parse_singleton_target("-12"), None);
        assert_eq!(parse_singleton_target("h-0"), None);
        assert_eq!(parse_singleton_target("h-x"), None);
    }

    #[test]
    fn nothing_found_is_not_running() {
        let facts = FakeProcs::default();
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Absent),
            &facts,
        );
        assert!(!r.running, "{:?}", r.reasons);
    }

    #[test]
    fn singleton_lock_counts_only_for_this_host_and_a_live_pid() {
        let facts = FakeProcs::default().alive(4242);
        let lock = |host: &str, pid| SingletonProbe::Target {
            host: host.into(),
            pid,
        };
        let r = classify(
            &inputs(lock("host-a.local", 4242), RuntimeProbe::Absent),
            &facts,
        );
        assert_eq!(r.reasons, vec![RunningReason::SingletonLock { pid: 4242 }]);
        let r = classify(
            &inputs(lock("host-a.local", 7), RuntimeProbe::Absent),
            &facts,
        );
        assert!(!r.running, "stale lock");
        let r = classify(
            &inputs(lock("host-b.local", 4242), RuntimeProbe::Absent),
            &facts,
        );
        assert!(!r.running, "another host's lock");
        let mut i = inputs(lock("host-a.local", 4242), RuntimeProbe::Absent);
        i.hostname = None;
        assert!(
            classify(&i, &facts).running,
            "unknown host name fails closed"
        );
        let r = classify(
            &inputs(SingletonProbe::Unreadable("x".into()), RuntimeProbe::Absent),
            &facts,
        );
        assert!(r.running);
    }

    #[test]
    fn runtime_pid_counts_only_when_it_is_orca_main() {
        let orca = proc_info(900, "Orca", Some(MAC_MAIN), &[]);
        let facts = FakeProcs::default().with(orca.clone());
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Present(meta(900))),
            &facts,
        );
        assert!(r.running);
        assert_eq!(r.reasons[0], RunningReason::RuntimePid { pid: 900 });
        assert_eq!(r.main_pid, Some(900));
        assert_eq!(r.main_exe.as_deref(), Some(Path::new(MAC_MAIN)));

        // A reused pid (some other program) and a dead pid do not count.
        let other = proc_info(900, "zsh", Some("/bin/zsh"), &[]);
        let facts = FakeProcs::default().with_hidden(other);
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Present(meta(900))),
            &facts,
        );
        assert!(!r.running);
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Present(meta(901))),
            &FakeProcs::default(),
        );
        assert!(!r.running);
        assert!(r.runtime.is_some(), "the metadata is still reported");

        // Linux: the runtime pid's executable is `.../orca-ide`.
        let lin = proc_info(900, "orca-ide", Some("/opt/Orca/orca-ide"), &[]);
        let facts = FakeProcs::default().with_hidden(lin);
        let mut i = inputs(SingletonProbe::Absent, RuntimeProbe::Present(meta(900)));
        i.os = HostOs::Linux;
        let r = classify(&i, &facts);
        assert!(r.running, "{:?}", r.reasons);
        assert_eq!(r.reasons[0], RunningReason::RuntimePid { pid: 900 });
        assert_eq!(r.main_exe.as_deref(), Some(Path::new("/opt/Orca/orca-ide")));

        // An unreadable runtime file fails closed.
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Unreadable("x".into())),
            &FakeProcs::default(),
        );
        assert!(r.running);
    }

    #[test]
    fn process_table_finds_main_and_ignores_helpers() {
        let facts = FakeProcs::default()
            .with(proc_info(
                10,
                "Orca Helper",
                Some("/Applications/Orca.app/Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper"),
                &["/Applications/Orca.app/Contents/Resources/app.asar/out/main/daemon-entry.js"],
            ))
            .with(proc_info(11, "Orca", Some(MAC_MAIN), &["--type=gpu-process"]));
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Absent),
            &facts,
        );
        assert!(!r.running, "{:?}", r.reasons);

        let facts = facts.with(proc_info(12, "Orca", Some(MAC_MAIN), &[]));
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Absent),
            &facts,
        );
        assert_eq!(r.reasons, vec![RunningReason::ProcessTable { pid: 12 }]);
        assert_eq!(r.main_pid, Some(12));

        let unreadable = FakeProcs::default().unreadable_table();
        let r = classify(
            &inputs(SingletonProbe::Absent, RuntimeProbe::Absent),
            &unreadable,
        );
        assert!(r.running);
        let unknown = FakeProcs::default().with(proc_info(13, "Orca", None, &[]));
        assert!(
            classify(
                &inputs(SingletonProbe::Absent, RuntimeProbe::Absent),
                &unknown
            )
            .running
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_probes_read_the_lock_and_runtime_file() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        assert_eq!(read_singleton(ud), SingletonProbe::Absent);
        assert!(matches!(read_runtime(ud), RuntimeProbe::Absent));
        std::os::unix::fs::symlink("host-a.local-77", ud.join(SINGLETON_LOCK)).unwrap();
        assert_eq!(
            read_singleton(ud),
            SingletonProbe::Target {
                host: "host-a.local".into(),
                pid: 77
            }
        );
        std::fs::remove_file(ud.join(SINGLETON_LOCK)).unwrap();
        std::fs::write(ud.join(SINGLETON_LOCK), "not a link").unwrap();
        assert!(matches!(read_singleton(ud), SingletonProbe::Unreadable(_)));
        std::fs::write(ud.join("orca-runtime.json"), "{bad").unwrap();
        assert!(matches!(read_runtime(ud), RuntimeProbe::Unreadable(_)));
    }

    #[test]
    fn system_procs_table_is_empty_under_test() {
        assert_eq!(SystemProcs.table().map(|t| t.len()), Some(0));
    }

    #[test]
    fn marks_fingerprint_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let facts = FakeProcs::default();
        let live = SystemLiveness {
            os: HostOs::MacOs,
            user_data: dir.path().to_path_buf(),
            facts: &facts,
        };
        let l0 = live.mark();
        assert!(!l0.running && l0 == LiveMark::stopped());
        // A runtime file for a dead pid: not running, but a different
        // instance state than L0.
        std::fs::write(
            dir.path().join("orca-runtime.json"),
            r#"{"runtimeId":"rt-1","pid":999999,"transports":[],"authToken":"tok","startedAt":5}"#,
        )
        .unwrap();
        let l1 = live.mark();
        assert!(!l1.running, "{l1:?}");
        assert!(!l1.still_clear_of(&l0));
        assert_eq!(l1.runtime, Some(("rt-1".into(), 999999, Some(5))));
        assert!(!format!("{l1:?}").contains("tok"));
        assert!(live.mark().still_clear_of(&l1));
    }

    #[test]
    fn dir_users_scans_claude_environments() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude.work");
        std::fs::create_dir(&dir).unwrap();
        let other = tmp.path().join(".claude.home");
        let set = format!("CLAUDE_CONFIG_DIR={}/", dir.display());
        let set_other = format!("CLAUDE_CONFIG_DIR={}", other.display());
        let claude = proc_info(40, "claude", Some("/opt/bin/claude"), &[]);
        let node = proc_info(
            41,
            "node",
            Some("/usr/bin/node"),
            &["/lib/claude-code/cli.js"],
        );
        let shell = proc_info(42, "zsh", Some("/bin/zsh"), &[]);
        let users = |f: &FakeProcs| {
            let t = f.table();
            dir_users_in(&dir, false, t.as_deref(), 7, &|p| f.environ(p))
        };
        // A claude pinned to the dir (trailing separator and all) is live.
        let f = FakeProcs::default()
            .with(claude.clone())
            .with_env(40, &["PATH=/bin", &set]);
        assert!(matches!(users(&f), DirUsers::Live(w) if w.contains("pid 40")));
        // Node running Claude Code counts; a shell never does.
        let f = FakeProcs::default()
            .with(node.clone())
            .with_env(41, &[&set])
            .with(shell.clone())
            .with_env(42, &[&set]);
        assert!(matches!(users(&f), DirUsers::Live(w) if w.contains("pid 41")));
        let f = FakeProcs::default().with(shell).with_env(42, &[&set]);
        assert_eq!(users(&f), DirUsers::Free);
        // Another dir, or no variable at all: free.
        let f = FakeProcs::default()
            .with(claude.clone())
            .with_env(40, &[&set_other])
            .with(node)
            .with_env(41, &["PATH=/bin"]);
        assert_eq!(users(&f), DirUsers::Free);
        // An unreadable environment or table counts as live.
        let f = FakeProcs::default().with(claude.clone());
        assert!(matches!(users(&f), DirUsers::Unknown(_)));
        assert!(!matches!(users(&f), DirUsers::Free));
        let f = FakeProcs::default().unreadable_table();
        assert!(matches!(users(&f), DirUsers::Unknown(_)));
        // csm itself and its ancestors never count.
        let me = ProcInfo {
            ppid: Some(40),
            ..proc_info(7, "csm", Some("/opt/bin/csm"), &[])
        };
        let f = FakeProcs::default()
            .with(me)
            .with(claude)
            .with_env(40, &[&set]);
        assert_eq!(users(&f), DirUsers::Free);
        // A registered session wins whatever the table says.
        let t: Vec<ProcInfo> = Vec::new();
        assert!(matches!(
            dir_users_in(&dir, true, Some(&t), 7, &|_| None),
            DirUsers::Live(_)
        ));
    }

    #[test]
    fn every_live_user_is_listed_with_its_parent_and_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude.work");
        std::fs::create_dir(&dir).unwrap();
        let set = format!("CLAUDE_CONFIG_DIR={}", dir.display());
        let mut f = FakeProcs::default().with(ProcInfo {
            pid: 5,
            ppid: Some(1),
            ..proc_info(5, "Google Chrome", Some("/Applications/Chrome"), &[])
        });
        for pid in 100..112u32 {
            let p = ProcInfo {
                ppid: Some(5),
                start_time: 1_790_000_000,
                ..proc_info(pid, "claude", Some("/opt/bin/claude"), &[])
            };
            f = f.with(p).with_env(pid, &[&set]);
        }
        let t = f.table();
        let DirUsers::Live(w) = dir_users_in(&dir, false, t.as_deref(), 7, &|p| f.environ(p))
        else {
            panic!("expected live");
        };
        assert!(
            w.starts_with("12 processes run with CLAUDE_CONFIG_DIR="),
            "{w}"
        );
        assert!(w.contains("pid 100 (claude, started "), "{w}");
        assert!(w.contains("parent Google Chrome"), "{w}");
        assert!(w.contains("pid 109 ") && !w.contains("pid 110 "), "{w}");
        assert!(w.ends_with("and 2 more"), "{w}");
    }

    #[test]
    fn format_dir_users_single_and_unknown_parent() {
        let d = Path::new("/example/.claude.old");
        let u = |pid, parent: Option<&str>| DirUser {
            pid,
            name: "claude".into(),
            started: String::new(),
            parent: parent.map(str::to_owned),
        };
        assert_eq!(
            format_dir_users(d, &[u(9, None)]),
            "pid 9 (claude) runs with CLAUDE_CONFIG_DIR=/example/.claude.old"
        );
        let w = format_dir_users(d, &[u(9, Some("Orca Helper")), u(10, None)]);
        assert_eq!(
            w,
            "2 processes run with CLAUDE_CONFIG_DIR=/example/.claude.old: \
             pid 9 (claude, parent Orca Helper); pid 10 (claude)"
        );
    }

    /// The legacy registry is one machine-wide dir every profile's
    /// `sessions` links to: a live record there counts for a dir only when
    /// its pid runs in that dir (its `CLAUDE_CONFIG_DIR`, or the implicit
    /// `~/.claude` without one). What cannot be attributed counts as live.
    #[test]
    fn a_shared_registry_counts_a_record_only_for_its_own_dir() {
        use crate::orca::runtime::{SessionRecord, SessionScan};
        let home = Path::new("/Users/example");
        let work = home.join(".claude.work");
        let other = home.join(".claude.home");
        let implicit = home.join(".claude");
        let rec = |pid| SessionRecord {
            pid,
            session_id: None,
            proc_start: None,
            proc_start_ft: None,
            pid_domain: None,
            kind: None,
            status: None,
        };
        let scan = SessionScan {
            live: vec![rec(40), rec(41)],
            ..SessionScan::default()
        };
        let set_work = format!("CLAUDE_CONFIG_DIR={}", work.display());
        let env = |pid: u32| -> Option<Vec<OsString>> {
            match pid {
                40 => Some(vec![OsString::from(&set_work)]),
                41 => Some(vec![OsString::from("PATH=/bin")]),
                _ => None,
            }
        };
        let alive = |_: u32| true;
        let users = |d: &Path| registry_users_in(d, home, Some(&scan), &env, &alive);
        assert!(users(&work).is_some_and(|w| w.contains("pid 40")));
        assert!(users(&implicit).is_some_and(|w| w.contains("pid 41")));
        assert_eq!(users(&other), None);
        // An unreadable environment: live while the pid runs, nothing once
        // it exited.
        let one = SessionScan {
            live: vec![rec(42)],
            ..SessionScan::default()
        };
        assert!(registry_users_in(&other, home, Some(&one), &env, &|_| true).is_some());
        assert_eq!(
            registry_users_in(&other, home, Some(&one), &env, &|_| false),
            None
        );
        // Another pid domain, an unreadable record, an unlisted registry.
        let unv = SessionScan {
            unverifiable: vec![rec(43)],
            ..SessionScan::default()
        };
        assert!(registry_users_in(&other, home, Some(&unv), &env, &alive).is_some());
        let bad = SessionScan {
            unreadable: 1,
            ..SessionScan::default()
        };
        assert!(registry_users_in(&other, home, Some(&bad), &env, &alive).is_some());
        assert!(registry_users_in(&other, home, None, &env, &alive).is_some());
    }

    /// Two profile dirs linked to one registry: a claude running in one of
    /// them keeps only that one in use.
    #[cfg(unix)]
    #[test]
    fn dir_users_over_a_shared_registry_attributes_each_record() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let shared = home.join(".claude.shared").join("sessions");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("40.json"), r#"{"sessionId":"s-40"}"#).unwrap();
        let work = home.join(".claude.work");
        let other = home.join(".claude.home");
        for d in [&work, &other] {
            std::fs::create_dir_all(d).unwrap();
            std::os::unix::fs::symlink(&shared, d.join("sessions")).unwrap();
        }
        let set = format!("CLAUDE_CONFIG_DIR={}", work.display());
        let f = FakeProcs::default()
            .with(proc_info(40, "claude", Some("/opt/bin/claude"), &[]))
            .with_env(40, &[&set]);
        assert!(matches!(
            dir_users(HostOs::Linux, &work, home, &f),
            DirUsers::Live(_)
        ));
        assert_eq!(dir_users(HostOs::Linux, &other, home, &f), DirUsers::Free);
    }
}
