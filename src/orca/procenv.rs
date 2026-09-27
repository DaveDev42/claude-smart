//! Orca main's runtime dir, read from its process environment.
//!
//! csm switches accounts only when its own `D` equals the `D` the running
//! Orca materializes into; otherwise an Orca-side switch would land in a dir
//! csm's sessions never read. Orca's `D` follows getRuntimePaths over Orca's
//! OWN environment: `CLAUDE_CONFIG_DIR` trimmed, else `$HOME/.claude`
//! (`os.homedir()` reads `HOME` on POSIX).
//!
//! The environment block comes from `sysinfo` (macOS `KERN_PROCARGS2` for a
//! same-user, non-platform process, Linux `/proc/<pid>/environ`, Windows the PEB). An
//! unreadable block is `None`, which callers read as "refuse the switch".
//!
//! The block may hold secrets (Orca exports hook tokens to its panes and may
//! carry API keys). Only `CLAUDE_CONFIG_DIR` and `HOME` are looked at; the
//! block is dropped right after and never printed. Salvaged from the
//! reverted commit de91859.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::runtime::RuntimePaths;

/// Orca main's runtime dir and whether its own environment named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaDir {
    /// `D`: `CLAUDE_CONFIG_DIR` trimmed, else `$HOME/.claude`.
    pub dir: PathBuf,
    /// `CLAUDE_CONFIG_DIR` was set and non-blank. Orca exports the variable
    /// to its claude panes only then (runtime-paths.ts: `envPatch:
    /// inheritedConfigDir ? { CLAUDE_CONFIG_DIR: configDir } : {}`).
    pub explicit: bool,
}

/// Orca's `D` rule over one environment block. `fallback_home` stands in
/// for a block without `HOME`. Pure.
#[cfg(test)]
fn config_dir_from_environ<S: AsRef<std::ffi::OsStr>>(
    env: &[S],
    fallback_home: Option<&Path>,
) -> Option<PathBuf> {
    orca_dir_from_environ(env, fallback_home).map(|o| o.dir)
}

/// [`config_dir_from_environ`] plus whether the variable was set. Pure.
pub fn orca_dir_from_environ<S: AsRef<std::ffi::OsStr>>(
    env: &[S],
    fallback_home: Option<&Path>,
) -> Option<OrcaDir> {
    let lookup = |key: &str| {
        env.iter().find_map(|kv| {
            let kv = kv.as_ref().to_str()?;
            let (k, v) = kv.split_once('=')?;
            (k == key).then(|| v.to_owned())
        })
    };
    let home = lookup("HOME")
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| fallback_home.map(Path::to_path_buf));
    let ccd = lookup("CLAUDE_CONFIG_DIR");
    let set = ccd.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match (set, home) {
        (Some(d), _) => Some(OrcaDir {
            dir: PathBuf::from(d),
            explicit: true,
        }),
        (None, Some(h)) => Some(OrcaDir {
            dir: h.join(".claude"),
            explicit: false,
        }),
        (None, None) => None,
    }
}

/// The environment block of `pid`. A seam so tests read a child they
/// spawned; nothing here prints it.
fn environ_of(pid: u32) -> Option<Vec<OsString>> {
    crate::platform::proc::environ(pid)
}

/// The `D` the running Orca main process `pid` materializes into. `None`
/// when its environment cannot be read.
pub fn orca_runtime_dir(pid: u32, fallback_home: Option<&Path>) -> Option<PathBuf> {
    orca_dir(pid, fallback_home).map(|o| o.dir)
}

/// [`orca_runtime_dir`] plus whether Orca main's environment set
/// `CLAUDE_CONFIG_DIR`.
pub fn orca_dir(pid: u32, fallback_home: Option<&Path>) -> Option<OrcaDir> {
    if pid == 0 {
        return None;
    }
    let env = environ_of(pid)?;
    orca_dir_from_environ(&env, fallback_home)
}

/// Does csm's `D` agree with Orca's? Compares canonical paths when both
/// exist, else the paths as written. Pure over its inputs apart from
/// canonicalization.
pub fn same_runtime_dir(csm: &RuntimePaths, orca_dir: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    csm.config_dir == orca_dir || canon(&csm.config_dir) == canon(orca_dir)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_dir_wins_trimmed() {
        let env = [
            "HOME=/Users/example",
            "CLAUDE_CONFIG_DIR=  /Users/example/.claude.work ",
        ];
        assert_eq!(
            config_dir_from_environ(&env, None),
            Some(PathBuf::from("/Users/example/.claude.work"))
        );
    }

    #[test]
    fn blank_or_missing_falls_back_to_home() {
        for env in [
            vec!["HOME=/Users/example", "CLAUDE_CONFIG_DIR=   "],
            vec!["HOME=/Users/example"],
        ] {
            assert_eq!(
                config_dir_from_environ(&env, None),
                Some(PathBuf::from("/Users/example/.claude"))
            );
        }
        let env = ["PATH=/usr/bin"];
        assert_eq!(
            config_dir_from_environ(&env, Some(Path::new("/Users/example"))),
            Some(PathBuf::from("/Users/example/.claude"))
        );
        assert_eq!(config_dir_from_environ(&env, None), None);
        // A value containing '=' is kept whole.
        let env = ["CLAUDE_CONFIG_DIR=/Users/example/a=b"];
        assert_eq!(
            config_dir_from_environ(&env, None),
            Some(PathBuf::from("/Users/example/a=b"))
        );
    }

    #[test]
    fn explicit_only_when_the_variable_is_set() {
        let set = [
            "HOME=/Users/example",
            "CLAUDE_CONFIG_DIR=/Users/example/.claude",
        ];
        assert_eq!(
            orca_dir_from_environ(&set, None),
            Some(OrcaDir {
                dir: PathBuf::from("/Users/example/.claude"),
                explicit: true
            })
        );
        for env in [
            vec!["HOME=/Users/example"],
            vec!["HOME=/Users/example", "CLAUDE_CONFIG_DIR= "],
        ] {
            assert_eq!(
                orca_dir_from_environ(&env, None),
                Some(OrcaDir {
                    dir: PathBuf::from("/Users/example/.claude"),
                    explicit: false
                })
            );
        }
    }

    #[test]
    fn same_runtime_dir_compares_canonically() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("claude");
        std::fs::create_dir_all(&d).unwrap();
        let paths =
            crate::orca::runtime::runtime_paths(Some(d.to_str().unwrap()), dir.path(), |_| false);
        assert!(same_runtime_dir(&paths, &d));
        std::fs::create_dir_all(dir.path().join("x")).unwrap();
        assert!(same_runtime_dir(
            &paths,
            &dir.path().join("x").join("..").join("claude")
        ));
        assert!(!same_runtime_dir(&paths, &dir.path().join("other")));
    }

    #[test]
    fn pid_zero_is_unreadable() {
        assert_eq!(orca_runtime_dir(0, None), None);
    }

    /// Not a check on its own: [`reads_a_childs_claude_config_dir`] re-runs
    /// this test binary with `CSM_TEST_ENV_SLEEPER` set so the child sleeps.
    /// The child must be a non-platform binary: macOS hides the environment
    /// of Apple platform binaries such as `/bin/sleep` from `KERN_PROCARGS2`
    /// (Orca, an Electron app, is not one).
    #[test]
    fn env_sleeper_child() {
        if std::env::var_os("CSM_TEST_ENV_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    /// Read a child's environment the way the Orca check does.
    #[cfg(unix)]
    #[test]
    fn reads_a_childs_claude_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let want = dir.path().join("runtime-d");
        let exe = std::env::current_exe().unwrap();
        let mut child = crate::platform::child::ChildGuard::spawn(
            std::process::Command::new(exe)
                .args([
                    "--exact",
                    "orca::procenv::tests::env_sleeper_child",
                    "--test-threads=1",
                    "-q",
                ])
                .env_clear()
                .env("CSM_TEST_ENV_SLEEPER", "1")
                .env("HOME", dir.path())
                .env("CLAUDE_CONFIG_DIR", &want)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = None;
        while std::time::Instant::now() < deadline {
            got = orca_runtime_dir(child.id(), None);
            if got.as_deref() == Some(want.as_path()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.stop();
        assert_eq!(got.as_deref(), Some(want.as_path()));
    }
}
