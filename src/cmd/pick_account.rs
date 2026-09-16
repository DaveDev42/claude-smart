//! `csm pick-account` and `csm current-usage`.

use std::ffi::OsString;

use crate::account;

/// `csm pick-account [<current>] [--include-current]`
///
/// Prints the winner profile name to stdout, or nothing on no-op.
/// Exits 1 on fetch failure.
///
/// Routes through `account::pick_account` → `scoring::pick_best_at`, which
/// scores on all three usage dimensions (session, week_all, and the
/// model-scoped weekly `week_fable`) — a profile whose `week_fable` is
/// saturated is skipped exactly like a session- or week_all-limited one.
pub(crate) fn cmd_pick_account(args: &[OsString]) -> anyhow::Result<()> {
    let mut current = String::new();
    let mut include_current = false;
    for arg in args {
        let s = arg.to_string_lossy();
        if s == "--include-current" {
            include_current = true;
        } else if !s.starts_with('-') {
            current = s.into_owned();
        }
    }

    // Degraded mode: no registry → no accounts to pick between. Bail gracefully
    // (empty stdout, a hint on stderr, rc 0) instead of attempting a remote
    // fetch that fails with a raw "empty payload" error. Mirrors the
    // `profiles.is_empty()` guard in `proactive_pick_profile`.
    if account::ProfileMap::load().unwrap_or_default().is_empty() {
        eprintln!("csm pick-account: no profiles configured — `csm profiles add <name>`");
        return Ok(());
    }

    match account::pick_account(&current, include_current) {
        Ok(Some(winner)) => println!("{winner}"),
        Ok(None) => {}
        Err(account::scoring::ScoringError::AllSaturated) => {
            eprintln!("csm pick-account: all accounts saturated");
        }
        Err(account::scoring::ScoringError::NoUsableData) => {
            eprintln!("csm pick-account: no usable usage data for any profile");
            std::process::exit(1);
        }
        Err(account::scoring::ScoringError::FetchFailed(e)) => {
            eprintln!("csm pick-account: usage fetch failed: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `csm current-usage <profile>`
///
/// Print `<session_pct> <week_all_pct>` or nothing (errored/absent).
pub(crate) fn cmd_current_usage(args: &[OsString]) -> anyhow::Result<()> {
    let profile = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("csm current-usage: profile argument required"))?;
    if let Some((s, w)) = account::current_usage(&profile) {
        println!("{s} {w}");
    }
    Ok(())
}
