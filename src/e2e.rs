//! Seams for the end-to-end harness (`e2e/run.sh`), compiled only with the
//! `e2e` cargo feature and never into a release build or a unit test.
//!
//! A binary built with `--features e2e` is a sandbox binary:
//!
//! - it refuses to run unless `CSM_E2E_SANDBOX` names an absolute dir that
//!   `HOME` (and every set `XDG_*_HOME`) lies under ([`guard`]);
//! - the macOS Keychain goes to the harness's fake `security` script, run
//!   through `/usr/bin/perl`, and to nothing else ([`security_program`]);
//! - the process-table sweep sees only processes whose executable lies under
//!   the sandbox, so the machine's real Orca never counts as running
//!   ([`in_sandbox`]);
//! - the Orca version can be injected (`CSM_E2E_ORCA_VERSION`), since Linux
//!   has no version source and offline writes need a tested one
//!   ([`orca_version`]);
//! - named points in the store-write protocol run a harness script
//!   (`CSM_E2E_POINT_HOOK`), which is how a scenario starts the fake Orca
//!   exactly at L1 or L2 ([`point`]);
//! - `csm migrate retire` never runs `launchctl`.
//!
//! Without the feature (or under `cfg(test)`) every function here is an
//! inert stub the optimizer removes.

// Stubs keep one call shape for both builds; some go unused off unix.
#![cfg_attr(not(all(feature = "e2e", not(test))), allow(dead_code))]

use std::ffi::OsString;
use std::path::PathBuf;

/// Is this a sandbox binary?
pub const ENABLED: bool = cfg!(all(feature = "e2e", not(test)));

// ─── guard ────────────────────────────────────────────────────────────────────

/// Exit unless the environment is the harness sandbox. Called first thing
/// in `main`.
#[cfg(all(feature = "e2e", not(test)))]
pub fn guard() {
    if let Err(why) = check_sandbox(
        std::env::var_os("CSM_E2E_SANDBOX"),
        std::env::var_os("HOME"),
        &[
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("XDG_DATA_HOME"),
        ],
    ) {
        eprintln!("csm (e2e build): refusing to run: {why}");
        std::process::exit(97);
    }
}

#[cfg(not(all(feature = "e2e", not(test))))]
pub fn guard() {}

/// Pure: is `home` (and every set XDG dir) under `sandbox`?
pub fn check_sandbox(
    sandbox: Option<OsString>,
    home: Option<OsString>,
    xdg: &[Option<OsString>],
) -> Result<PathBuf, String> {
    let sandbox = sandbox
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or("CSM_E2E_SANDBOX is not set")?;
    if !sandbox.is_absolute() {
        return Err("CSM_E2E_SANDBOX is not absolute".into());
    }
    let under = |p: &std::path::Path| p.is_absolute() && p.starts_with(&sandbox);
    let home = home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    if !under(&home) {
        return Err("HOME is not inside CSM_E2E_SANDBOX".into());
    }
    for d in xdg.iter().flatten().filter(|d| !d.is_empty()) {
        if !under(std::path::Path::new(d)) {
            return Err("an XDG_*_HOME is set outside CSM_E2E_SANDBOX".into());
        }
    }
    Ok(sandbox)
}

fn sandbox() -> Option<PathBuf> {
    std::env::var_os("CSM_E2E_SANDBOX")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

// ─── Keychain ─────────────────────────────────────────────────────────────────

/// The fake `security`: `(/usr/bin/perl, [<script>, <root>])` from
/// `CSM_E2E_SECURITY` (the script) and `CSM_E2E_SECURITY_ROOT` (its item
/// store). `Err` when either is missing or either names the real binary; a
/// sandbox binary never falls back to `/usr/bin/security`.
pub fn security_program() -> Result<(PathBuf, Vec<OsString>), String> {
    let script = std::env::var_os("CSM_E2E_SECURITY")
        .filter(|s| !s.is_empty())
        .ok_or("CSM_E2E_SECURITY is not set")?;
    let root = std::env::var_os("CSM_E2E_SECURITY_ROOT")
        .filter(|s| !s.is_empty())
        .ok_or("CSM_E2E_SECURITY_ROOT is not set")?;
    let real = std::path::Path::new("/usr/bin/security");
    let names_real = |p: &std::path::Path| {
        p == real
            || matches!(
                (std::fs::canonicalize(p), std::fs::canonicalize(real)),
                (Ok(a), Ok(b)) if a == b
            )
    };
    if names_real(std::path::Path::new(&script)) || names_real(std::path::Path::new(&root)) {
        return Err("the fake security names the real /usr/bin/security".into());
    }
    Ok((PathBuf::from("/usr/bin/perl"), vec![script, root]))
}

// ─── process table ────────────────────────────────────────────────────────────

/// Does a process with executable `exe` count in the sandbox's process
/// table? Only executables under `CSM_E2E_SANDBOX` do.
pub fn in_sandbox(exe: Option<&std::path::Path>) -> bool {
    match (sandbox(), exe) {
        (Some(s), Some(e)) => e.starts_with(&s),
        _ => false,
    }
}

// ─── Orca version ─────────────────────────────────────────────────────────────

/// The injected Orca version (`CSM_E2E_ORCA_VERSION`).
#[cfg(all(feature = "e2e", not(test)))]
pub fn orca_version() -> Option<String> {
    std::env::var("CSM_E2E_ORCA_VERSION")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(not(all(feature = "e2e", not(test))))]
pub fn orca_version() -> Option<String> {
    None
}

// ─── named points ─────────────────────────────────────────────────────────────

/// Run `/bin/sh $CSM_E2E_POINT_HOOK <name>` and wait for it (60 s cap).
/// The script decides whether the point matters to the running scenario.
#[cfg(all(feature = "e2e", not(test), unix))]
pub fn point(name: &str) {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    let Some(script) = std::env::var_os("CSM_E2E_POINT_HOOK").filter(|s| !s.is_empty()) else {
        return;
    };
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(script)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let Ok(mut child) = crate::platform::child::own_group(&mut cmd).spawn() else {
        return;
    };
    let _ = crate::platform::child::wait_deadline(
        &mut child,
        Duration::from_secs(60),
        Duration::from_millis(20),
        true,
    );
}

#[cfg(not(all(feature = "e2e", not(test), unix)))]
pub fn point(_name: &str) {}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn the_guard_wants_home_inside_the_sandbox() {
        assert!(check_sandbox(os("/tmp/sb"), os("/tmp/sb/home"), &[]).is_ok());
        assert!(check_sandbox(None, os("/tmp/sb/home"), &[]).is_err());
        assert!(check_sandbox(os("rel"), os("rel/home"), &[]).is_err());
        assert!(check_sandbox(os("/tmp/sb"), os("/Users/example"), &[]).is_err());
        assert!(check_sandbox(os("/tmp/sb"), None, &[]).is_err());
        // A sibling that shares the prefix text is not inside.
        assert!(check_sandbox(os("/tmp/sb"), os("/tmp/sb2/home"), &[]).is_err());
        assert!(
            check_sandbox(
                os("/tmp/sb"),
                os("/tmp/sb/home"),
                &[os("/Users/example/.local/state")]
            )
            .is_err()
        );
        assert!(
            check_sandbox(
                os("/tmp/sb"),
                os("/tmp/sb/home"),
                &[os("/tmp/sb/state"), None]
            )
            .is_ok()
        );
    }

    #[test]
    fn stubs_are_inert_in_unit_tests() {
        const { assert!(!ENABLED) };
        assert_eq!(orca_version(), None);
        guard();
        point("store-L1");
    }
}
