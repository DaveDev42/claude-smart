//! Single seam onto the `sysinfo` crate, shared by the two call sites that
//! used to talk to `sysinfo` independently: the targeted single-pid identity
//! probe (`proc_check`'s hot-path kill-gate check) and the full-table sweep
//! (the reaper's candidate scan). Both need the same handful of process
//! fields, so both read them through [`ProcInfo`] instead of each owning its
//! own `sysinfo` call.

use std::ffi::OsString;
use std::path::PathBuf;

/// One process's identity + lineage, as read from `sysinfo`.
///
/// Field selection serves both consumers: [`probe`] reads `name`/`exe`/`cmd`
/// (first token only) for the identity check; [`snapshot`] reads
/// `exe`/`ppid`/`start_time`/`cmd` (the whole vec, joined for display) for the
/// reaper's candidate table.
#[derive(Debug, Clone)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cmd: Vec<OsString>,
    pub start_time: u64,
}

/// Targeted single-process refresh: `System::new()` starts empty and
/// `refresh_processes_specifics` loads exactly this one pid, so the process
/// table is never swept. Never widen this past a targeted refresh — a full
/// sweep stalls the hot Stop path on a busy Windows box.
///
/// `ProcessRefreshKind::nothing()` leaves exe and cmd unset (the name is
/// always filled), so both are requested explicitly. Adding a field to this
/// refresh kind is forbidden: it is the hot-path cost this fn protects.
/// Returns `None` when `pid` is not running.
pub(crate) fn probe(pid: u32) -> Option<ProcInfo> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let sys_pid = Pid::from_u32(pid);
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet);
    if sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[sys_pid]), true, kind) == 0 {
        return None;
    }
    let proc_ = sys.process(sys_pid)?;
    Some(ProcInfo {
        pid,
        ppid: proc_.parent().map(|p| p.as_u32()),
        name: proc_.name().to_string_lossy().into_owned(),
        exe: proc_.exe().map(|p| p.to_path_buf()),
        cmd: proc_.cmd().to_vec(),
        start_time: proc_.start_time(),
    })
}

/// Full process-table sweep: `System::new_all()`. Off the hot path — never
/// called from the latency-sensitive Stop path [`probe`] guards.
pub(crate) fn snapshot() -> Vec<ProcInfo> {
    use sysinfo::System;

    let sys = System::new_all();
    sys.processes()
        .iter()
        .map(|(pid, proc_)| ProcInfo {
            pid: pid.as_u32(),
            ppid: proc_.parent().map(|p| p.as_u32()),
            name: proc_.name().to_string_lossy().into_owned(),
            exe: proc_.exe().map(|p| p.to_path_buf()),
            cmd: proc_.cmd().to_vec(),
            start_time: proc_.start_time(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{probe, snapshot};

    /// Spawn a child, then walk it through both constructors and assert the
    /// exe basename normalises identically — i.e. the one `ProcInfo.exe`
    /// field is enough for both consumers' own normalisation.
    #[cfg(unix)]
    #[test]
    fn probe_and_snapshot_agree_on_exe_base() {
        use crate::platform::proc_check::{bare_basename, wait_until_live_claude_or_node};

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
        let mut child = std::process::Command::new(&link).arg("30").spawn().unwrap();
        let pid = child.id();
        let live = wait_until_live_claude_or_node(pid, std::time::Duration::from_secs(5));

        let probed = probe(pid);
        let snapshotted = snapshot().into_iter().find(|p| p.pid == pid);

        let _ = child.kill();
        let _ = child.wait();

        assert!(live, "the spawned child must be recognized as live first");
        let base_of = |info: &super::ProcInfo| {
            info.exe
                .as_deref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|n| bare_basename(n).to_ascii_lowercase())
        };
        assert_eq!(
            probed.as_ref().and_then(base_of),
            snapshotted.as_ref().and_then(base_of),
            "probe and snapshot must normalise the same pid's exe to the same basename"
        );
    }

    /// `ProcInfo.cmd` serves both derivations without a second field: the
    /// probe path needs the first token (argv[0]), the reaper path needs a
    /// lossy join of the whole vec for display.
    #[test]
    fn proc_info_cmd_serves_both_consumers() {
        let cmd: Vec<std::ffi::OsString> = vec!["claude".into(), "--flag".into(), "value".into()];

        let argv0 = cmd.first().and_then(|s| s.to_str());
        assert_eq!(argv0, Some("claude"));

        let joined = cmd
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "claude --flag value");
    }
}
