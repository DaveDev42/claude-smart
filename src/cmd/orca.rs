//! `csm orca` — Orca as csm sees it, and the one-time pane setup.
//!
//! `status` prints what csm resolved: whether Orca runs, its userData, csm's
//! `D` and Orca's, the active account, whether RPC answered, the Orca version
//! against csm's tested range, and the store's schemaVersion. It reads only;
//! it never prints a token or Orca's RPC auth token.
//!
//! `setup` creates `<state>/bin/claude` (`claude.exe` on Windows), the alias
//! Orca's `agentCmdOverrides.claude` should name so every Orca pane runs csm.
//! A symlink on POSIX; on Windows, where a symlink needs a privilege, a hard
//! link or else a copy. csm never writes Orca's settings: `setup` prints the
//! value to set.
//!
//! The symlink names a path that survives an upgrade: the `csm` on `PATH`
//! that resolves to this binary (a package manager's `bin/csm` link), else
//! the path csm was started by, never the versioned file behind a
//! Homebrew-style link, which the next upgrade removes. `status` and
//! `accounts doctor` report an alias whose target is gone.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

use crate::orca::AccountSource;
use crate::orca::live::SystemProcs;
use crate::orca::runtime::UuidMatch;
use crate::orca::{HostEnv, HostOs, OrcaView, SnapshotOptions, fsx, version};

/// `csm orca …`
pub(crate) fn cmd_orca(args: &[OsString]) -> anyhow::Result<()> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["status"] => status(),
        ["setup"] => setup(),
        ["-h" | "--help" | "help"] => {
            println!("csm orca status   what csm sees of Orca");
            println!("csm orca setup    create the `claude` alias for agentCmdOverrides.claude");
            Ok(())
        }
        _ => bail!("csm orca: expected `status` or `setup`"),
    }
}

// ─── status ───────────────────────────────────────────────────────────────────

/// The status report. Pure over the snapshot.
pub(crate) fn render_status(v: &OrcaView) -> String {
    let mut out = String::new();
    let mut line = |k: &str, val: String| out.push_str(&format!("{k:<13} {val}\n"));
    line(
        "orca",
        if v.running { "running" } else { "stopped" }.to_owned(),
    );
    line("userData", v.user_data.dir.display().to_string());
    line(
        "D",
        format!(
            "{}{}",
            v.runtime.config_dir.display(),
            if v.runtime.env_set {
                " (CLAUDE_CONFIG_DIR)"
            } else {
                ""
            }
        ),
    );
    line(
        "orca D",
        match (&v.orca_runtime_dir, v.runtime_dir_agrees) {
            (Some(d), agrees) => {
                let mut out = d.display().to_string();
                if let Some(n) = &v.orca_dir_note {
                    out.push_str(&format!(" ({n})"));
                }
                if agrees == Some(false) {
                    out.push_str(" (differs from csm's)");
                }
                out
            }
            (None, _) if v.running => "unknown".to_owned(),
            (None, _) => "-".to_owned(),
        },
    );
    let email_of = |id: &str| {
        v.accounts
            .iter()
            .find(|a| a.id == id)
            .and_then(|a| a.email.clone())
    };
    let named = |id: &str| match email_of(id) {
        Some(e) => format!("{e} ({id})"),
        None => id.to_owned(),
    };
    line(
        "active",
        v.active_id.as_deref().map(named).unwrap_or("-".to_owned()),
    );
    line(
        "D account",
        match v.runtime_account.as_ref().map(|r| r.account.as_ref()) {
            None | Some(None) => "none (D holds no login)".to_owned(),
            Some(Some(UuidMatch::Unique(id))) => named(id),
            Some(Some(UuidMatch::None)) => "not an Orca account".to_owned(),
            Some(Some(UuidMatch::Ambiguous(ids))) => format!("ambiguous: {}", ids.join(", ")),
        },
    );
    line(
        "accounts",
        format!(
            "{} ({})",
            v.host_accounts().count(),
            match v.source {
                AccountSource::Rpc => "RPC",
                AccountSource::Store => "store",
                AccountSource::None => "none",
            }
        ),
    );
    line(
        "rpc",
        match (&v.source, &v.rpc_error) {
            (AccountSource::Rpc, _) => "reachable".to_owned(),
            (_, Some(e)) => format!("unreachable: {e}"),
            _ if v.running => "unreachable".to_owned(),
            _ => "- (Orca stopped)".to_owned(),
        },
    );
    line(
        "version",
        match &v.version.version {
            Some(ver) if v.version_ok => format!("{ver} (tested)"),
            Some(ver) => format!(
                "{ver} (untested; csm was tested with {}; offline writes refused)",
                version::TESTED.join(", ")
            ),
            None => "unknown (offline writes refused)".to_owned(),
        },
    );
    line("schema", v.schema_version.clone().unwrap_or("-".to_owned()));
    if let Some(e) = &v.store_error {
        line("store", format!("unreadable: {e}"));
    }
    if v.sqlite_state {
        line(
            "store",
            "SQLite (profile-state.db, read-only); orca-data.json is Orca's export and may be stale"
                .to_owned(),
        );
    }
    line(
        "offline",
        if v.offline_write_allowed() {
            "writes allowed"
        } else {
            "writes refused"
        }
        .to_owned(),
    );
    out
}

/// After the cutover, an Orca whose live `D` is still a recorded legacy
/// dir keeps its panes and sessions on the old account. Returns that dir
/// and the dir to adopt (`~/.claude`). It looks at Orca's `D` only, never at
/// the caller's shell pin. Pure over `stale` (the recorded legacy dirs plus
/// `~/.claude`, [`crate::migrate::stale_dirs`]).
pub(crate) fn old_orca_d(
    orca_d: Option<&Path>,
    stale: &[PathBuf],
    home: &Path,
    cutover_done: bool,
) -> Option<(PathBuf, PathBuf)> {
    let trim = |p: &Path| PathBuf::from(p.to_string_lossy().trim_end_matches(['/', '\\']));
    let target = home.join(".claude");
    let d = trim(orca_d?);
    (cutover_done && d != trim(&target) && stale.iter().any(|s| trim(s) == d))
        .then_some((d, target))
}

/// The restart warning for [`old_orca_d`]. Pure.
pub(crate) fn restart_note(old: &Path, target: &Path) -> String {
    format!(
        "restart Orca to adopt {}: it still runs the old D {}, so its panes and csm disagree on the account",
        target.display(),
        old.display()
    )
}

/// The note for an inherited `CLAUDE_CONFIG_DIR` that status and doctor
/// ignored, as a launch would. Pure.
pub(crate) fn pin_note(pin: &str) -> String {
    format!(
        "CLAUDE_CONFIG_DIR={} in this shell is a retired profile dir and is ignored; open a new shell",
        pin.trim()
    )
}

/// [`old_orca_d`] over the migration marker. Reads files.
pub(crate) fn old_orca_d_now(env: &HostEnv, orca_d: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    let (stale, cutover_done, _) = crate::migrate::stale_dirs(env);
    old_orca_d(orca_d, &stale, &env.home, cutover_done)
}

/// A live claude on a recorded legacy dir blocks the migration's retire step
/// and keeps reporting that dir's account. Pure; `users` is
/// [`crate::migrate::legacy_dirs_in_use`].
pub(crate) fn legacy_use_notes(users: &[(PathBuf, String)]) -> Vec<String> {
    users
        .iter()
        .map(|(dir, who)| {
            format!(
                "a claude still runs on the legacy dir {} ({who}); it blocks the migration's retire step, close it",
                dir.display()
            )
        })
        .collect()
}

/// The lines about live users of legacy dirs. Reads the marker and the
/// process table.
pub(crate) fn legacy_use_notes_now(env: &HostEnv) -> Vec<String> {
    let users = crate::migrate::legacy_dirs_in_use(env, &crate::orca::live::SystemProcs);
    legacy_use_notes(&users)
}

fn status() -> anyhow::Result<()> {
    let (env, ignored) = crate::launch_context::status_env().context("csm orca status")?;
    let v = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &SystemProcs);
    print!("{}", render_status(&v));
    if let Some(pin) = &ignored {
        println!("{:<13} {}", "note", pin_note(pin));
    }
    let alias = alias_path(&fsx::state_dir(&env), env.os);
    println!("{:<13} {}", "alias", alias_line(&alias_state(&alias)));
    println!("{:<13} {}", "migration", crate::migrate::status_line());
    if let Some(w) = override_warning_now(&v) {
        println!("{:<13} {w}", "warning");
    }
    let old = old_orca_d_now(&env, v.orca_runtime_dir.as_deref());
    let restart = old.map(|(o, t)| restart_note(&o, &t));
    for n in restart.into_iter().chain(legacy_use_notes_now(&env)) {
        println!("{:<13} {n}", "WARNING");
    }
    Ok(())
}

// ─── the pane override ────────────────────────────────────────────────────────

/// `agentCmdOverrides.claude` out of Orca's settings, and nothing else: the
/// same object holds `agentDefaultEnv`, which may carry secrets. Takes the
/// `settings.get` answer (`{settings: {…}}`) or the store's document (whose
/// top-level `settings` holds the same key). Pure.
pub(crate) fn claude_override(v: &serde_json::Value) -> Option<String> {
    v.get("settings")?
        .get("agentCmdOverrides")?
        .get("claude")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The program an override command line runs: its first token, with the
/// quotes Orca's pane launcher strips. Pure.
pub(crate) fn override_program(cmd: &str) -> Option<String> {
    let cmd = cmd.trim_start();
    let prog = match cmd.chars().next()? {
        q @ ('"' | '\'') => cmd[1..].split(q).next().unwrap_or(""),
        _ => cmd.split_whitespace().next().unwrap_or(""),
    };
    (!prog.is_empty()).then(|| prog.to_owned())
}

/// The warning for an override whose executable is missing. `found` says
/// whether a program (a path, or a bare name looked up on `PATH`) exists.
/// No override: no warning (Orca then runs plain `claude`). Pure over
/// `found`.
pub(crate) fn override_warning(cmd: Option<&str>, found: impl Fn(&str) -> bool) -> Option<String> {
    let prog = override_program(cmd?)?;
    (!found(&prog)).then(|| {
        format!(
            "Orca's agentCmdOverrides.claude runs {prog}, which does not exist: Orca's claude \
             panes fail to start. Point it at an existing csm (`csm orca setup` prints the value)"
        )
    })
}

/// Does `prog` exist: a path as given (`~/` under the home dir), or a bare
/// name on `PATH` (with the Windows executable suffixes)?
fn program_found(prog: &str, home: Option<&Path>) -> bool {
    let p = match (prog.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(prog),
    };
    if prog.contains(['/', '\\']) {
        return p.is_file();
    }
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|d| exts.iter().any(|e| d.join(format!("{prog}{e}")).is_file()))
    })
}

/// The override warning for this machine: `settings.get` while Orca runs,
/// the store's `settings` otherwise. Reads only the one key.
fn override_warning_now(v: &OrcaView) -> Option<String> {
    let value = if v.running {
        crate::orca::rpc::call(
            &v.user_data.dir,
            &crate::orca::rpc::Method::SettingsGet,
            crate::orca::rpc::LIST_TIMEOUT,
        )
        .ok()?
    } else {
        let choice = crate::orca::userdata::data_file(&v.user_data.dir);
        let db = choice.state_db_files().into_iter().next()?;
        match crate::orca::store::load_state_db_settings(&db) {
            Ok(Some(settings)) => serde_json::json!({ "settings": settings }),
            _ => {
                let f = crate::orca::store::load_choice(&choice).ok()??;
                serde_json::from_slice(&f.bytes).ok()?
            }
        }
    };
    let cmd = claude_override(&value);
    let home = HostEnv::current().ok().map(|e| e.home);
    override_warning(cmd.as_deref(), |p| program_found(p, home.as_deref()))
}

// ─── setup ────────────────────────────────────────────────────────────────────

/// `<state>/bin/claude[.exe]`. Pure.
pub(crate) fn alias_path(state: &Path, os: HostOs) -> PathBuf {
    let name = if os == HostOs::Windows {
        "claude.exe"
    } else {
        "claude"
    };
    state.join("bin").join(name)
}

/// What [`install_alias`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AliasOutcome {
    Created,
    AlreadyCurrent,
    Replaced,
}

/// The value to put in `agentCmdOverrides.claude`: the alias path, quoted
/// when it holds whitespace. Pure.
pub(crate) fn override_value(alias: &Path) -> String {
    let s = alias.display().to_string();
    if s.chars().any(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s
    }
}

/// Make `alias` run `exe`. Idempotent.
#[cfg(unix)]
pub(crate) fn install_alias(exe: &Path, alias: &Path) -> io::Result<AliasOutcome> {
    if let Some(dir) = alias.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let existed = match std::fs::read_link(alias) {
        Ok(target) if target == exe => return Ok(AliasOutcome::AlreadyCurrent),
        Ok(_) => true,
        Err(_) => std::fs::symlink_metadata(alias).is_ok(),
    };
    // Build the link beside the alias, then rename over it: the alias is
    // never missing while a pane may start.
    let tmp = alias.with_file_name(format!(".claude.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(exe, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, alias) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(if existed {
        AliasOutcome::Replaced
    } else {
        AliasOutcome::Created
    })
}

/// Make `alias` run `exe`: a hard link, else a copy. Idempotent.
#[cfg(not(unix))]
pub(crate) fn install_alias(exe: &Path, alias: &Path) -> io::Result<AliasOutcome> {
    if let Some(dir) = alias.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let existed = alias.exists();
    if existed && same_bytes(exe, alias)? {
        return Ok(AliasOutcome::AlreadyCurrent);
    }
    let tmp = alias.with_file_name(format!(".claude.{}.tmp.exe", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(exe, &tmp).is_err() {
        std::fs::copy(exe, &tmp)?;
    }
    if existed {
        // A running alias cannot be replaced on Windows; say so plainly.
        if let Err(e) = std::fs::remove_file(alias) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "cannot replace {} (in use by a pane?): {e}",
                    alias.display()
                ),
            ));
        }
    }
    std::fs::rename(&tmp, alias)?;
    Ok(if existed {
        AliasOutcome::Replaced
    } else {
        AliasOutcome::Created
    })
}

#[cfg(not(unix))]
fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// The path the alias should name. `invoked` is `current_exe()` as the OS
/// reports it, `canonical` its resolved form, `on_path` each `csm` on `PATH`
/// with its resolved form. The first `PATH` entry that resolves to this very
/// binary wins (Homebrew's `bin/csm` link, which `brew upgrade` repoints);
/// else the absolute path csm was started by; else the resolved path. Pure.
pub(crate) fn link_target(
    invoked: &Path,
    canonical: &Path,
    on_path: &[(PathBuf, PathBuf)],
) -> PathBuf {
    if let Some((p, _)) = on_path
        .iter()
        .find(|(p, c)| p.is_absolute() && c == canonical)
    {
        return p.clone();
    }
    if invoked.is_absolute() {
        return invoked.to_path_buf();
    }
    canonical.to_path_buf()
}

/// Every `csm` on `PATH` with its resolved path.
fn csm_on_path() -> Vec<(PathBuf, PathBuf)> {
    let name = if cfg!(windows) { "csm.exe" } else { "csm" };
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .filter(|p| p.is_file())
        .filter_map(|p| std::fs::canonicalize(&p).ok().map(|c| (p, c)))
        .collect()
}

/// This binary's stable path for the alias ([`link_target`]).
fn own_link_target() -> anyhow::Result<PathBuf> {
    let invoked = std::env::current_exe().context("cannot find csm's own path")?;
    let canonical = std::fs::canonicalize(&invoked).unwrap_or_else(|_| invoked.clone());
    Ok(link_target(&invoked, &canonical, &csm_on_path()))
}

/// What `<state>/bin/claude` is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AliasState {
    Absent,
    /// It runs a binary; POSIX: the link's target.
    Present(PathBuf),
    /// A symlink whose target is gone (an upgrade removed it).
    Dangling(PathBuf),
}

/// Inspect the alias. Reads metadata only.
pub(crate) fn alias_state(alias: &Path) -> AliasState {
    match std::fs::read_link(alias) {
        Ok(target) => {
            let full = if target.is_absolute() {
                target.clone()
            } else {
                alias
                    .parent()
                    .map(|p| p.join(&target))
                    .unwrap_or(target.clone())
            };
            if full.exists() {
                AliasState::Present(target)
            } else {
                AliasState::Dangling(target)
            }
        }
        Err(_) if alias.exists() => AliasState::Present(alias.to_path_buf()),
        Err(_) => AliasState::Absent,
    }
}

/// One line for [`AliasState`]. Pure.
pub(crate) fn alias_line(s: &AliasState) -> String {
    match s {
        AliasState::Absent => "- (`csm orca setup` creates it)".to_owned(),
        AliasState::Present(t) => t.display().to_string(),
        AliasState::Dangling(t) => format!(
            "broken: {} is gone; run `csm orca setup` again",
            t.display()
        ),
    }
}

/// Point the alias at this binary again. `accounts doctor --fix` uses it
/// for a dangling alias.
pub(crate) fn repair_alias(env: &HostEnv) -> anyhow::Result<String> {
    let alias = alias_path(&fsx::state_dir(env), env.os);
    let exe = own_link_target()?;
    install_alias(&exe, &alias).with_context(|| format!("cannot update {}", alias.display()))?;
    Ok(format!("{} now runs {}", alias.display(), exe.display()))
}

fn setup() -> anyhow::Result<()> {
    let env = HostEnv::current().context("csm orca setup")?;
    let alias = alias_path(&fsx::state_dir(&env), env.os);
    let exe = own_link_target().context("csm orca setup")?;
    let outcome = install_alias(&exe, &alias)
        .with_context(|| format!("csm orca setup: cannot create {}", alias.display()))?;
    let verb = match outcome {
        AliasOutcome::Created => "created",
        AliasOutcome::AlreadyCurrent => "already current:",
        AliasOutcome::Replaced => "updated",
    };
    println!("csm: {verb} {}", alias.display());
    println!();
    println!("In Orca's settings, set the Claude agent command override to:");
    println!("  {}", override_value(&alias));
    println!("(agentCmdOverrides.claude; csm does not edit Orca's settings)");
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{FakeProcs, record_json, write_store};

    #[test]
    fn alias_is_named_for_the_os() {
        let s = Path::new("/Users/example/.local/state/csm");
        assert_eq!(alias_path(s, HostOs::MacOs), s.join("bin").join("claude"));
        assert_eq!(
            alias_path(s, HostOs::Windows),
            s.join("bin").join("claude.exe")
        );
    }

    #[test]
    fn override_value_quotes_whitespace() {
        assert_eq!(
            override_value(Path::new("/Users/example/.local/state/csm/bin/claude")),
            "/Users/example/.local/state/csm/bin/claude"
        );
        assert_eq!(
            override_value(Path::new(r"C:\Users\A B\AppData\Local\csm\bin\claude.exe")),
            r#""C:\Users\A B\AppData\Local\csm\bin\claude.exe""#
        );
    }

    #[test]
    fn install_alias_is_idempotent_and_replaces_a_stale_one() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("csm-real");
        std::fs::write(&exe, b"binary-v1").unwrap();
        let alias = alias_path(&tmp.path().join("state"), HostOs::Linux);
        assert_eq!(install_alias(&exe, &alias).unwrap(), AliasOutcome::Created);
        assert_eq!(
            install_alias(&exe, &alias).unwrap(),
            AliasOutcome::AlreadyCurrent
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"binary-v1");
        let exe2 = tmp.path().join("csm-new");
        std::fs::write(&exe2, b"binary-v2").unwrap();
        assert_eq!(
            install_alias(&exe2, &alias).unwrap(),
            AliasOutcome::Replaced
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"binary-v2");
        // No temp link left behind.
        let left: Vec<_> = std::fs::read_dir(alias.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(left.len(), 1);
    }

    /// Homebrew: `bin/csm` links into a versioned keg. The alias names the
    /// `PATH` link, which survives `brew upgrade`, never the keg path.
    #[test]
    fn link_target_prefers_the_path_link_over_a_versioned_keg() {
        use crate::testenv::abs;
        let keg = &abs("/opt/homebrew/Cellar/claude-smart/0.3.7/bin/csm");
        let link = abs("/opt/homebrew/bin/csm");
        let other = (
            abs("/Users/example/.cargo/bin/csm"),
            abs("/Users/example/.cargo/bin/csm"),
        );
        let on_path = vec![other.clone(), (link.clone(), keg.clone())];
        // Linux reports the resolved path; macOS the invoked one.
        assert_eq!(link_target(keg, keg, &on_path), link);
        assert_eq!(link_target(&link, keg, &on_path), link);
        // Not on PATH: the absolute invoked path, unresolved.
        assert_eq!(link_target(&link, keg, std::slice::from_ref(&other)), link);
        // Relative invocation and nothing on PATH: the resolved path.
        assert_eq!(
            link_target(Path::new("target/debug/csm"), keg, &[]),
            keg.clone()
        );
        // A relative PATH entry never wins.
        let rel = vec![(PathBuf::from("bin/csm"), keg.clone())];
        assert_eq!(link_target(&link, keg, &rel), link);
    }

    #[cfg(unix)]
    #[test]
    fn alias_state_sees_a_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("keg").join("csm");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"binary").unwrap();
        let alias = alias_path(&tmp.path().join("state"), HostOs::Linux);
        assert_eq!(alias_state(&alias), AliasState::Absent);
        install_alias(&exe, &alias).unwrap();
        assert_eq!(alias_state(&alias), AliasState::Present(exe.clone()));
        // The upgrade's cleanup removes the old keg.
        std::fs::remove_dir_all(exe.parent().unwrap()).unwrap();
        let st = alias_state(&alias);
        assert_eq!(st, AliasState::Dangling(exe.clone()));
        assert!(alias_line(&st).contains("csm orca setup"));
    }

    #[test]
    fn restart_only_when_orcas_own_d_is_a_recorded_legacy_dir() {
        let home = Path::new("/example");
        let stale = vec![
            PathBuf::from("/example/.claude.old"),
            PathBuf::from("/example/.claude"),
        ];
        let old = Path::new("/example/.claude.old");
        let (o, t) = old_orca_d(Some(old), &stale, home, true).unwrap();
        assert_eq!(
            (o.as_path(), t.as_path()),
            (old, Path::new("/example/.claude"))
        );
        let n = restart_note(&o, &t);
        assert!(
            n.contains(&format!("restart Orca to adopt {}:", t.display())),
            "{n}"
        );
        assert!(n.contains(&o.display().to_string()), "{n}");
        // Orca already on the target, on an unrecorded dir, unreadable, or
        // before the cutover: nothing to restart.
        let none = |d: Option<&str>, cut| old_orca_d(d.map(Path::new), &stale, home, cut);
        assert_eq!(none(Some("/example/.claude"), true), None);
        assert_eq!(none(Some("/example/.claude/"), true), None);
        assert_eq!(none(Some("/elsewhere/claude"), true), None);
        assert_eq!(none(None, true), None);
        assert_eq!(none(Some("/example/.claude.old"), false), None);
    }

    #[test]
    fn an_ignored_pin_is_named_as_a_note() {
        let n = pin_note("/example/.claude.old");
        assert!(n.contains("CLAUDE_CONFIG_DIR=/example/.claude.old"), "{n}");
        assert!(n.contains("is ignored; open a new shell"), "{n}");
    }

    #[test]
    fn legacy_use_names_the_dir_and_the_process() {
        assert!(legacy_use_notes(&[]).is_empty());
        let n = legacy_use_notes(&[(
            PathBuf::from("/example/.claude.old"),
            "pid 42 (claude) runs with CLAUDE_CONFIG_DIR=/example/.claude.old".into(),
        )]);
        assert_eq!(n.len(), 1);
        assert!(n[0].contains("/example/.claude.old"), "{}", n[0]);
        assert!(n[0].contains("pid 42"), "{}", n[0]);
        assert!(
            n[0].contains("blocks the migration's retire step"),
            "{}",
            n[0]
        );
    }

    #[test]
    fn status_renders_a_stopped_store_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::for_test(tmp.path(), HostOs::Linux);
        let ud = crate::orca::userdata::resolve(&env, |_| false).dir;
        write_store(
            &ud,
            &[record_json(&ud, "acct-a", "alice@example.com", None)],
            Some("acct-a"),
        );
        let v =
            crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &FakeProcs::default());
        let out = render_status(&v);
        assert!(out.contains("stopped"), "{out}");
        assert!(out.contains("alice@example.com (acct-a)"), "{out}");
        assert!(out.contains("(store)"), "{out}");
        assert!(out.contains("- (Orca stopped)"), "{out}");
        assert!(out.contains(&tmp.path().join(".claude").display().to_string()));
    }

    #[test]
    fn override_warning_reads_only_the_claude_key() {
        let answer = serde_json::json!({
            "settings": {
                "agentCmdOverrides": { "claude": "/opt/csm/bin/claude --flag", "codex": "x" },
                "agentDefaultEnv": { "SECRET_TOKEN": "sk-do-not-print" }
            }
        });
        assert_eq!(
            claude_override(&answer).as_deref(),
            Some("/opt/csm/bin/claude --flag")
        );
        assert_eq!(claude_override(&serde_json::json!({"settings": {}})), None);
        assert_eq!(claude_override(&serde_json::json!({})), None);
        assert_eq!(
            override_program("\"/Users/example/My Tools/claude\" --x").as_deref(),
            Some("/Users/example/My Tools/claude")
        );
        assert_eq!(override_program("csm run").as_deref(), Some("csm"));
        assert_eq!(override_program("   "), None);
        // Missing: one warning that names the program and nothing else.
        let w = override_warning(claude_override(&answer).as_deref(), |_| false).unwrap();
        assert!(w.contains("/opt/csm/bin/claude"), "{w}");
        assert!(
            !w.contains("sk-do-not-print") && !w.contains("--flag"),
            "{w}"
        );
        assert_eq!(
            override_warning(claude_override(&answer).as_deref(), |_| true),
            None
        );
        assert_eq!(override_warning(None, |_| false), None);
    }
}
