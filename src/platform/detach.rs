//! Fire-and-forget child processes: spawn a helper that outlives nothing it
//! is attached to and that the caller never waits on.
//!
//! Used by `csm run` to start `csm orca sync --quiet` when a queued Orca
//! selection is waiting (see `cmd::run`). The child gets a session of its own
//! (unix `setsid`) or a detached console (Windows), all three stdio handles
//! go to the null device, and a background thread reaps it so it never
//! lingers as a zombie while the launcher supervises claude.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Spawn `program args…` detached: new session / process group, stdio null,
/// never awaited by the caller. Returns once the child has been spawned.
pub(crate) fn spawn_detached<S: AsRef<OsStr>>(program: &Path, args: &[S]) -> io::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    let mut child = cmd.spawn()?;
    // Reap in the background: the launcher only waits on its own claude
    // child, so without this the helper would sit as a zombie until csm exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and touches no memory the parent
    // shares; it is the only call made between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(io::Error::from)?;
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn spawn_detached_runs_the_child_in_its_own_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("sid");
        // The child records its process group; `setsid` also starts a new
        // group led by the child, so its pgid equals its pid.
        let script = format!(
            "ps -o pgid= -p $$ > '{m}.tmp'; echo $$ >> '{m}.tmp'; mv '{m}.tmp' '{m}'",
            m = marker.display()
        );
        spawn_detached(Path::new("/bin/sh"), &["-c", script.as_str()]).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let text = loop {
            if let Ok(t) = std::fs::read_to_string(&marker)
                && t.lines().count() >= 2
            {
                break t;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "detached child never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut lines = text.lines().map(str::trim);
        let pgid = lines.next().unwrap();
        let pid = lines.next().unwrap();
        assert_eq!(pgid, pid, "the child must lead a new session and group");
    }
}
