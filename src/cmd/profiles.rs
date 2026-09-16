//! `csm profiles` — the human-facing registry noun, and the `bootstrap`/
//! `doctor` provisioning verbs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cmd::cas::parse_cas_op;
use crate::cmd::support::{current_profile_dir, derive_current_profile_name, resolve_profile_dir};
use crate::{account, cas, provision};

/// `csm profiles <verb> ...` — the human-facing registry noun.
///
/// A thin noun-verb front over the SAME `cas::Op` handlers the `cas` management
/// verbs use (no duplicate registry logic). Verbs:
///   list | add <name> [<dir>] | set <name> <dir> | rm|remove <name>
///   use <name> | edit | dir [<name>]
///
/// Bare `csm profiles` ≡ `csm profiles list`. `dir` is a profiles-only
/// convenience (prints a profile's config dir; default profile when omitted).
pub(crate) fn cmd_profiles(args: &[OsString]) -> anyhow::Result<()> {
    use cas::Op;

    let verb = args.first().map(|a| a.to_string_lossy().into_owned());
    let rest: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // `dir` is profiles-only (not a cas Op): print a profile's resolved dir.
    if verb.as_deref() == Some("dir") {
        let profiles =
            account::ProfileMap::load().context("csm profiles: failed to load profiles.json")?;
        let dir = match rest.first() {
            Some(name) => profiles.get(name).map(str::to_owned).ok_or_else(|| {
                anyhow::anyhow!(
                    "csm profiles dir: unknown profile '{name}' — configured: {}",
                    profiles.names_sorted().join(", ")
                )
            })?,
            None => profiles.default_dir().to_string_lossy().into_owned(),
        };
        println!("{dir}");
        return Ok(());
    }

    // `bootstrap` / `doctor` are profiles-only provisioning verbs (not cas Ops).
    // They make a profile satisfy the invariants csm depends on (dir + plugins →
    // shared SSOT), so csm can stand up / repair its own environment.
    if verb.as_deref() == Some("bootstrap") {
        return cmd_profiles_bootstrap(&rest);
    }
    if verb.as_deref() == Some("doctor") {
        return cmd_profiles_doctor(&rest);
    }

    // Map the verb to a cas::Op. Bare/`list` → List; everything else reuses the
    // exact same parser the `cas` verbs use (so behavior cannot diverge).
    let op = match verb.as_deref() {
        None | Some("list") => Op::List,
        Some("edit") => Op::Edit,
        Some(v @ ("add" | "set" | "remove" | "rm" | "use")) => {
            // Rebuild the op-args vec in the shape parse_cas_op expects.
            let mut op_args = Vec::with_capacity(1 + rest.len());
            op_args.push(v.to_owned());
            op_args.extend(rest.iter().cloned());
            parse_cas_op(&op_args)?
        }
        Some(other) => {
            anyhow::bail!(
                "csm profiles: unknown verb '{other}' \
                 (expected list|add|set|rm|use|edit|dir|bootstrap|doctor)"
            );
        }
    };

    let mut profiles =
        account::ProfileMap::load().context("csm profiles: failed to load profiles.json")?;
    cas::manage_emit(&op, &mut profiles)
}

/// `csm profiles bootstrap [<name> | --all]`
///
/// Stand up / repair the provisioning invariants for one profile (or every
/// registered profile with `--all`): the dir exists and `plugins`/`projects`
/// are symlinks to their shared SSOTs (`~/.claude.shared/{plugins,projects}`).
/// Idempotent — safe to re-run. With no args, bootstraps the current/default
/// profile.
fn cmd_profiles_bootstrap(rest: &[String]) -> anyhow::Result<()> {
    let profiles = account::ProfileMap::load()
        .context("csm profiles bootstrap: failed to load profiles.json")?;

    let all = rest.iter().any(|a| a == "--all");
    let named: Option<&String> = rest.iter().find(|a| !a.starts_with('-'));

    // Resolve the (name, dir) targets to provision.
    let targets: Vec<(String, PathBuf)> = if all {
        if profiles.is_empty() {
            eprintln!("csm profiles bootstrap: no profiles configured — `csm profiles add <name>`");
            return Ok(());
        }
        profiles
            .names_sorted()
            .into_iter()
            .map(|n| (n.to_owned(), PathBuf::from(profiles.get(n).unwrap_or(""))))
            .collect()
    } else if let Some(name) = named {
        let dir = resolve_profile_dir(name, &profiles)?;
        vec![(name.clone(), PathBuf::from(dir))]
    } else {
        // Default to the current/default profile.
        let name = derive_current_profile_name(&profiles);
        let dir = current_profile_dir(&profiles);
        vec![(name, dir)]
    };

    let mut failures = 0;
    for (name, dir) in &targets {
        match provision::ensure_profile_provisioned(name, dir) {
            Ok(report) => {
                println!(
                    "bootstrap [{name}] {} → plugins: {}; projects: {}",
                    dir.display(),
                    describe_link(&report.plugins),
                    describe_link(&report.projects)
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("bootstrap [{name}] FAILED: {e}");
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("csm profiles bootstrap: {failures} profile(s) failed");
    }
    Ok(())
}

/// `csm profiles doctor [--fix] [<name> | --all]`
///
/// Diagnose the provisioning invariants and report what is broken. `--fix`
/// repairs anything unhealthy (the same code path as `bootstrap`). Without
/// `--fix` it is read-only (a dry run). Defaults to every registered profile.
fn cmd_profiles_doctor(rest: &[String]) -> anyhow::Result<()> {
    let profiles =
        account::ProfileMap::load().context("csm profiles doctor: failed to load profiles.json")?;

    let fix = rest.iter().any(|a| a == "--fix");
    let named: Option<&String> = rest.iter().find(|a| !a.starts_with('-'));

    let targets: Vec<(String, PathBuf)> = if let Some(name) = named {
        let dir = resolve_profile_dir(name, &profiles)?;
        vec![(name.clone(), PathBuf::from(dir))]
    } else {
        if profiles.is_empty() {
            eprintln!("csm profiles doctor: no profiles configured — `csm profiles add <name>`");
            return Ok(());
        }
        profiles
            .names_sorted()
            .into_iter()
            .map(|n| (n.to_owned(), PathBuf::from(profiles.get(n).unwrap_or(""))))
            .collect()
    };

    let mut unhealthy = 0;
    for (name, dir) in &targets {
        let diag = provision::diagnose_profile(dir);
        if diag.is_healthy() {
            println!("✓ [{name}] healthy ({})", dir.display());
            continue;
        }
        unhealthy += 1;
        println!("✗ [{name}] {}", describe_diagnosis(&diag, dir));
        if fix {
            match provision::ensure_profile_provisioned(name, dir) {
                Ok(report) => {
                    println!(
                        "  → fixed: plugins {}; projects {}",
                        describe_link(&report.plugins),
                        describe_link(&report.projects)
                    );
                }
                Err(e) => {
                    eprintln!("  → fix FAILED: {e}");
                }
            }
        }
    }

    if unhealthy == 0 {
        println!("all profiles healthy.");
    } else if !fix {
        println!("\n{unhealthy} profile(s) need provisioning — run `csm profiles doctor --fix`.");
    }
    Ok(())
}

/// One-line description of a [`provision::LinkOutcome`] for command output.
fn describe_link(outcome: &provision::LinkOutcome) -> String {
    use provision::LinkOutcome::*;
    match outcome {
        AlreadyLinked => "already linked to shared SSOT".to_owned(),
        Created => "linked to shared SSOT".to_owned(),
        SeededShared => "seeded shared SSOT and linked".to_owned(),
        BackedUp(p) => format!("backed up to {} and linked", p.display()),
        Skipped => "skipped (handled OS-side)".to_owned(),
    }
}

/// One-line description of a single unhealthy [`provision::LinkState`] axis, or
/// `None` when that axis is already healthy. `what` names the subdir
/// (`"plugins"`/`"projects"`) and `real_dir_note` is the axis-specific
/// consequence of it being a diverged per-profile dir.
#[cfg(unix)]
fn describe_link_state(
    what: &str,
    real_dir_note: &str,
    state: &provision::LinkState,
) -> Option<String> {
    use provision::LinkState::*;
    match state {
        Ok => None,
        Missing => Some(format!("{what} not linked (no entry)")),
        RealDir => Some(format!("{what} is a per-profile dir ({real_dir_note})")),
        WrongLink(t) => Some(format!(
            "{what} symlinked to wrong target ({})",
            t.display()
        )),
        NotADir => Some(format!("{what} is a file, not a dir/symlink")),
    }
}

/// One-line description of an unhealthy [`provision::ProfileDiagnosis`].
#[cfg(unix)]
fn describe_diagnosis(diag: &provision::ProfileDiagnosis, dir: &Path) -> String {
    if !diag.dir_exists {
        return format!("profile dir missing ({})", dir.display());
    }
    let parts: Vec<String> = [
        describe_link_state(
            "plugins",
            "causes marketplace cache-miss on switch",
            &diag.plugins,
        ),
        describe_link_state(
            "projects",
            "transcripts invisible to other profiles",
            &diag.projects,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "healthy".to_owned()
    } else {
        parts.join("; ")
    }
}

/// Non-unix: nothing beyond directory existence is checked on this platform —
/// the symlink invariant is delegated to OS-native tooling, so the diagnosis
/// carries no link detail to describe.
#[cfg(not(unix))]
fn describe_diagnosis(diag: &provision::ProfileDiagnosis, dir: &Path) -> String {
    if !diag.dir_exists {
        format!("profile dir missing ({})", dir.display())
    } else {
        "ok (dir linking handled OS-side on this platform)".to_owned()
    }
}
