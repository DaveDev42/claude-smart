//! `csm profiles` — the human-facing registry noun, and the `bootstrap`/
//! `doctor` provisioning verbs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cmd::cas::parse_cas_op;
use crate::cmd::support::{current_profile_dir, derive_current_profile_name, resolve_profile_dir};
use crate::{account, cas, provision};
#[cfg(unix)]
use crate::{homeguard, paths};

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
/// registered profile with `--all`): the dir exists and `plugins`, `projects`
/// and `sessions` are symlinks to their shared SSOTs under
/// `~/.claude.shared/`. Idempotent — safe to re-run. With no args, bootstraps
/// the current/default profile.
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
                    "bootstrap [{name}] {} → plugins: {}; projects: {}; sessions: {}",
                    dir.display(),
                    describe_link(&report.plugins),
                    describe_link(&report.projects),
                    describe_link(&report.sessions)
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

/// `csm profiles doctor [--fix] [--fix-home] [<name> | --all]`
///
/// Diagnose the provisioning invariants and report what is broken. `--fix`
/// repairs anything unhealthy (the same code path as `bootstrap`) and drains
/// any leftover sessions staging dir, which is not a profile's health and so
/// would otherwise go unrepaired when every profile is already linked. Without
/// `--fix` it is read-only (a dry run). Defaults to every registered profile.
///
/// The `~/.claude` compatibility shim is a separate, machine-wide axis reported
/// on the first line, before any profile and before the empty-registry exit.
/// `--fix` never touches it and `--fix-home` never touches a profile: the two
/// flags are independent, so repairing one is never a side effect of the other.
fn cmd_profiles_doctor(rest: &[String]) -> anyhow::Result<()> {
    let profiles =
        account::ProfileMap::load().context("csm profiles doctor: failed to load profiles.json")?;

    let fix = rest.iter().any(|a| a == "--fix");
    let fix_home = rest.iter().any(|a| a == "--fix-home");
    let named: Option<&String> = rest.iter().find(|a| !a.starts_with('-'));

    report_home_shim(&profiles, fix_home);

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
                        "  → fixed: plugins {}; projects {}; sessions {}",
                        describe_link(&report.plugins),
                        describe_link(&report.projects),
                        describe_link(&report.sessions)
                    );
                }
                Err(e) => {
                    eprintln!("  → fix FAILED: {e}");
                }
            }
        }
    }

    // Not a profile's health: every link is already right. But a staged entry
    // must never sit there silently. Most staging dirs are transient — csm
    // drains them on the next launch — so only `--fix`, which has just drained
    // them, calls what is left unmergeable.
    if fix {
        for stage in provision::drain_leftover_sessions_staging() {
            println!(
                "! sessions staging dir not emptied: {} (what is left is what the \
                 collision policy would not move; review it by hand)",
                stage.display()
            );
        }
    } else {
        for stage in provision::leftover_sessions_staging() {
            println!(
                "! sessions staging dir still present: {} (csm drains it on the next \
                 launch, or now with `csm profiles doctor --fix`)",
                stage.display()
            );
        }
    }

    if unhealthy == 0 {
        println!("all profiles healthy.");
    } else if !fix {
        println!("\n{unhealthy} profile(s) need provisioning — run `csm profiles doctor --fix`.");
    }
    Ok(())
}

/// Print the machine-wide `~/.claude` shim line, the optional identity-file
/// advisory, and — under `--fix-home` — what the repair did.
#[cfg(unix)]
fn report_home_shim(profiles: &account::ProfileMap, fix_home: bool) {
    let finding = homeguard::inspect(profiles);
    println!("home ~/.claude: {}", describe_home_shim(&finding));
    let registered = matches!(
        finding.verdict,
        homeguard::HomeShimVerdict::Registered { .. }
    );
    if !finding.identity_files.is_empty() && !registered {
        println!(
            "  holds {}: a login outside the registry that csm does not meter; \
             register it (csm profiles add <name> ~/.claude) or remove them",
            finding.identity_files.join(", ")
        );
    }
    if !fix_home {
        return;
    }
    match homeguard::ensure_home_shim(profiles, homeguard::ShimMode::Repair) {
        Ok(homeguard::ShimOutcome::Skipped(verdict)) => {
            println!("  → skipped: {}", describe_shim_skip(&verdict));
        }
        Ok(homeguard::ShimOutcome::PartiallyMerged(report)) => {
            println!(
                "  → skipped: {} name{} already exist{} in the shared transcript dir; \
                 nothing moved, ~/.claude/projects left as found",
                report.left_behind.len(),
                if report.left_behind.len() == 1 {
                    ""
                } else {
                    "s"
                },
                if report.left_behind.len() == 1 {
                    "s"
                } else {
                    ""
                },
            );
            for left in &report.left_behind {
                println!("    conflict: {}", left.display());
            }
        }
        Ok(homeguard::ShimOutcome::Merged(report)) => {
            println!(
                "  → fixed: home projects {} and linked",
                describe_merge(&report)
            );
        }
        // The link was already right and only its target was missing, so
        // `describe_link` would report "already linked" and say nothing about
        // what the repair actually did.
        Ok(homeguard::ShimOutcome::Linked(_))
            if finding.verdict == homeguard::HomeShimVerdict::SharedMissing =>
        {
            println!("  → fixed: shared transcript dir recreated; the link resolves again");
        }
        Ok(homeguard::ShimOutcome::Linked(outcome)) => {
            println!("  → fixed: home projects {}", describe_link(&outcome));
        }
        Err(e) => eprintln!("  → fix-home FAILED: {e}"),
    }
}

/// Non-unix: the `projects` entry would be an OS-side junction, so there is no
/// shim for csm to diagnose or repair here.
#[cfg(not(unix))]
fn report_home_shim(_profiles: &account::ProfileMap, _fix_home: bool) {
    println!("home ~/.claude: ok (dir linking handled OS-side on this platform)");
}

/// One-line description of a [`homeguard::HomeShimFinding`].
#[cfg(unix)]
fn describe_home_shim(f: &homeguard::HomeShimFinding) -> String {
    use homeguard::HomeShimVerdict as V;
    let shared_dir = paths::session_base_dir();
    let shared = shared_dir.display();
    // A registered `~/.claude` is provisioned like any profile, and profile
    // provisioning renames a real `projects` dir to `projects.bak` once the
    // shared store exists. Say so: a `.bak` takes that history out of reach of a
    // plain `claude --resume`, which is the one thing this shim exists to keep.
    let backup_on_fix =
        matches!(f.projects, Some(provision::LinkState::RealDir)) && shared_dir.exists();
    match &f.verdict {
        V::Ok => format!("shim ok (projects → {shared})"),
        V::SharedMissing => format!(
            "projects → {shared}, which is gone: the link dangles and \
             reading ~/.claude/projects fails; --fix-home recreates it"
        ),
        V::Absent => "absent: tools that hardcode ~/.claude/projects see no sessions; \
             --fix-home creates the shim"
            .to_owned(),
        V::ProjectsMissing => format!("no projects entry; --fix-home links it to {shared}"),
        V::ProjectsRealDir => format!(
            "projects is a real dir (transcripts outside the shared dir); \
             --fix-home merges it into {shared} and links"
        ),
        V::ProjectsWrongLink(t) => format!(
            "projects symlinked to {} instead of {shared}; --fix-home repoints",
            t.display()
        ),
        V::ProjectsNotADir => "projects is a file; --fix-home backs it up and links".to_owned(),
        V::LinkedElsewhere(t) => format!("is a symlink → {} (left alone)", t.display()),
        V::Registered { name } if backup_on_fix => format!(
            "registered as profile {name}; its projects is a real dir, so \
             `csm profiles doctor --fix` backs it up as projects.bak instead of \
             merging it into {shared} — move those transcripts across by hand first"
        ),
        V::Registered { name } => {
            format!("registered as profile {name} (provisioned like any profile)")
        }
        V::Unsupported => "is not a directory (left alone)".to_owned(),
    }
}

/// Why `--fix-home` left the shim as it found it.
#[cfg(unix)]
fn describe_shim_skip(verdict: &homeguard::HomeShimVerdict) -> String {
    use homeguard::HomeShimVerdict as V;
    match verdict {
        V::Ok => "already linked".to_owned(),
        V::LinkedElsewhere(t) => format!("~/.claude is a symlink → {}", t.display()),
        V::Registered { name } => {
            format!("~/.claude is registered profile {name} — profile provisioning owns it")
        }
        V::Unsupported => "~/.claude is not a directory".to_owned(),
        other => format!("{other:?}"),
    }
}

/// What a [`homeguard::MergeReport`] moved, in words.
#[cfg(unix)]
fn describe_merge(report: &homeguard::MergeReport) -> String {
    format!(
        "merged {} dir(s) and {} file(s) into the shared transcript dir",
        report.moved_dirs, report.moved_files
    )
}

/// One-line description of a [`provision::LinkOutcome`] for command output.
fn describe_link(outcome: &provision::LinkOutcome) -> String {
    use provision::LinkOutcome::*;
    match outcome {
        AlreadyLinked => "already linked to shared SSOT".to_owned(),
        Created => "linked to shared SSOT".to_owned(),
        SeededShared => "seeded shared SSOT and linked".to_owned(),
        BackedUp(p) => format!("backed up to {} and linked", p.display()),
        Merged { moved, leftover } => {
            let mut line = format!(
                "moved {moved} entr{} into the shared SSOT and linked",
                if *moved == 1 { "y" } else { "ies" }
            );
            if !leftover.is_empty() {
                line.push_str(&format!(
                    "; left in staging for review: {}",
                    leftover
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            line
        }
        Skipped => "skipped (handled OS-side)".to_owned(),
    }
}

/// One-line description of a single unhealthy [`provision::LinkState`] axis, or
/// `None` when that axis is already healthy. `what` names the subdir
/// (`"plugins"`/`"projects"`/`"sessions"`) and `real_dir_note` is the axis-specific
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
        Dangling => Some(format!(
            "{what} links to a shared dir that no longer exists (link dangles)"
        )),
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
        describe_link_state(
            "sessions",
            "cross-session messaging cannot see other profiles",
            &diag.sessions,
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
