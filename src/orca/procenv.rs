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
//! A headless Orca (`orca-ide --serve`, Electron) retitles its process, which
//! leaves `/proc/<pid>/environ` readable but blank. On Linux that case reads
//! the nearest same-uid ancestor's environment (the `serve` launcher, the
//! `xvfb-run` shell), then the owner's home. macOS and Windows keep the
//! single-block read: their APIs do not tell a blank block from a denied one.
//!
//! The block may hold secrets (Orca exports hook tokens to its panes and may
//! carry API keys). Only `CLAUDE_CONFIG_DIR` and `HOME` are looked at; the
//! block is dropped right after and never printed. Salvaged from the
//! reverted commit de91859.

use std::path::{Path, PathBuf};

use super::runtime::RuntimePaths;
use crate::platform::proc::{EnvRead, environ_read};

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

/// Where Orca's `D` was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirSource {
    /// Orca main's own environment.
    Own,
    /// Orca main's environment is blank (retitled process); this ancestor's
    /// environment named it.
    Ancestor(u32),
    /// Blank and no ancestor named it: the main process owner's home.
    OwnerHome,
}

impl DirSource {
    /// Short note for `csm orca status`; `None` for the ordinary case.
    pub fn note(self) -> Option<String> {
        match self {
            DirSource::Own => None,
            DirSource::Ancestor(pid) => Some(format!("from parent pid {pid}'s environment")),
            DirSource::OwnerHome => Some("from the Orca owner's home".to_owned()),
        }
    }
}

/// The decision over what was read: Orca main's block, its same-uid
/// ancestors nearest first, and the owner's home. Pure.
///
/// An unreadable main block stays `None` (fail closed). A blank one is a
/// retitled Electron process: the nearest ancestor whose environment names
/// `D` (via `CLAUDE_CONFIG_DIR` or `HOME`) answers, else the owner's home.
pub(crate) fn resolve_environ(
    main: &EnvRead,
    ancestors: &[(u32, EnvRead)],
    owner_home: Option<&Path>,
    fallback_home: Option<&Path>,
) -> Option<(OrcaDir, DirSource)> {
    match main {
        EnvRead::Unreadable => None,
        EnvRead::Entries(env) => {
            orca_dir_from_environ(env, fallback_home).map(|d| (d, DirSource::Own))
        }
        EnvRead::Empty => ancestors
            .iter()
            .find_map(|(pid, read)| match read {
                EnvRead::Entries(env) => {
                    orca_dir_from_environ(env, None).map(|d| (d, DirSource::Ancestor(*pid)))
                }
                _ => None,
            })
            .or_else(|| {
                owner_home.map(|h| {
                    (
                        OrcaDir {
                            dir: h.join(".claude"),
                            explicit: false,
                        },
                        DirSource::OwnerHome,
                    )
                })
            }),
    }
}

/// Same-uid ancestors of `pid`, nearest first, at most [`MAX_ANCESTORS`],
/// each with its environment. Linux only; empty elsewhere.
#[cfg(target_os = "linux")]
fn ancestry(pid: u32) -> (Vec<(u32, EnvRead)>, Option<PathBuf>) {
    use crate::platform::proc;
    /// How far up the parent chain a blank-environ Orca is followed.
    const MAX_ANCESTORS: usize = 4;
    let Some((mut next, uid)) = proc::parent_and_uid(pid) else {
        return (Vec::new(), None);
    };
    let mut out = Vec::new();
    while out.len() < MAX_ANCESTORS && next > 1 {
        let Some((ppid, puid)) = proc::parent_and_uid(next) else {
            break;
        };
        if puid != uid {
            break;
        }
        out.push((next, proc::environ_read(next)));
        next = ppid;
    }
    (out, proc::home_of_uid(uid))
}

#[cfg(not(target_os = "linux"))]
fn ancestry(_pid: u32) -> (Vec<(u32, EnvRead)>, Option<PathBuf>) {
    (Vec::new(), None)
}

/// The `D` the running Orca main process `pid` materializes into. `None`
/// when its environment cannot be read.
pub fn orca_runtime_dir(pid: u32, fallback_home: Option<&Path>) -> Option<PathBuf> {
    orca_dir(pid, fallback_home).map(|o| o.dir)
}

/// [`orca_runtime_dir`] plus whether Orca main's environment set
/// `CLAUDE_CONFIG_DIR`.
pub fn orca_dir(pid: u32, fallback_home: Option<&Path>) -> Option<OrcaDir> {
    orca_dir_sourced(pid, fallback_home).map(|(d, _)| d)
}

/// [`orca_dir`] plus where the answer came from.
pub fn orca_dir_sourced(pid: u32, fallback_home: Option<&Path>) -> Option<(OrcaDir, DirSource)> {
    if pid == 0 {
        return None;
    }
    let main = environ_read(pid);
    let (ancestors, owner_home) = if main == EnvRead::Empty {
        ancestry(pid)
    } else {
        (Vec::new(), None)
    };
    resolve_environ(&main, &ancestors, owner_home.as_deref(), fallback_home)
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
    use std::ffi::OsString;

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

    fn ent(v: &[&str]) -> EnvRead {
        EnvRead::Entries(v.iter().map(OsString::from).collect())
    }

    #[test]
    fn blank_environ_takes_the_nearest_ancestor_with_home() {
        let anc = [
            (20, EnvRead::Empty),
            (10, ent(&["PATH=/bin", "HOME=/home/example"])),
            (5, ent(&["HOME=/elsewhere"])),
        ];
        let (d, src) =
            resolve_environ(&EnvRead::Empty, &anc, Some(Path::new("/home/owner")), None).unwrap();
        assert_eq!(d.dir, PathBuf::from("/home/example/.claude"));
        assert!(!d.explicit);
        assert_eq!(src, DirSource::Ancestor(10));
    }

    #[test]
    fn blank_environ_takes_an_ancestors_claude_config_dir() {
        let anc = [(
            10,
            ent(&["HOME=/home/example", "CLAUDE_CONFIG_DIR=/home/example/.cc"]),
        )];
        let (d, src) = resolve_environ(&EnvRead::Empty, &anc, None, None).unwrap();
        assert_eq!(d.dir, PathBuf::from("/home/example/.cc"));
        assert!(d.explicit);
        assert_eq!(src, DirSource::Ancestor(10));
    }

    #[test]
    fn blank_environ_without_a_useful_ancestor_uses_the_owner_home() {
        let anc = [(10, EnvRead::Unreadable), (9, ent(&["PATH=/bin"]))];
        let (d, src) =
            resolve_environ(&EnvRead::Empty, &anc, Some(Path::new("/home/owner")), None).unwrap();
        assert_eq!(d.dir, PathBuf::from("/home/owner/.claude"));
        assert_eq!(src, DirSource::OwnerHome);
        assert_eq!(resolve_environ(&EnvRead::Empty, &anc, None, None), None);
    }

    #[test]
    fn unreadable_environ_never_falls_back() {
        let anc = [(10, ent(&["HOME=/home/example"]))];
        assert_eq!(
            resolve_environ(
                &EnvRead::Unreadable,
                &anc,
                Some(Path::new("/home/owner")),
                Some(Path::new("/h"))
            ),
            None
        );
    }

    #[test]
    fn readable_environ_ignores_ancestors() {
        let anc = [(10, ent(&["HOME=/home/other"]))];
        let own = ent(&["HOME=/home/example"]);
        let (d, src) = resolve_environ(&own, &anc, Some(Path::new("/home/owner")), None).unwrap();
        assert_eq!(d.dir, PathBuf::from("/home/example/.claude"));
        assert_eq!(src, DirSource::Own);
    }

    #[test]
    fn source_note_only_for_the_fallbacks() {
        assert_eq!(DirSource::Own.note(), None);
        assert!(DirSource::Ancestor(7).note().unwrap().contains("pid 7"));
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
