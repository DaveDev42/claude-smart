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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{FakeProcs, proc_info};

    const MAC_MAIN: &str = "/Applications/Orca.app/Contents/MacOS/Orca";

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
}
