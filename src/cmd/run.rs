//! `csm run` — the full launch pipeline: classify the launch
//! ([`crate::launch_context`]), resolve the session id (picker/resume/new),
//! build the claude CLI, and hand off to the relaunch loop in csm's runtime
//! dir `D`.

use std::ffi::OsString;

use anyhow::Context as _;

use crate::cmd::support::newuuid;
use crate::launch_context::LaunchContext;
use crate::platform::launcher::ChildEnv;
use crate::{account, cli, launch_context, paths, picker, platform, session, sidecar, usage};

/// How a resolved session id should be handed to `claude`.
///
/// This distinction is the difference between the two claude CLI verbs:
/// - `--session-id <uuid>` *creates* a session with that id and **rejects**
///   (`Error: Session ID <uuid> is already in use`) if a session file for it
///   already exists on disk.
/// - `--resume <uuid>` *continues* an existing session.
///
/// Every resolution path knows which it produced (a brand-new UUID vs an id
/// scanned off disk), so it must carry that intent forward — otherwise the
/// launcher would `--session-id` a pre-existing id and claude would refuse to
/// start. (This was the `csm resume` "already in use" bug: resume paths picked
/// an existing id but the launcher always passed `--session-id`.)
#[derive(Debug, Clone)]
enum SessionResolution {
    /// A brand-new session id → launch with `--session-id <id>`.
    Fresh(String),
    /// An existing session id picked off disk → launch with `--resume <id>`.
    Resume(String),
}

/// Which session flag decides this launch's session, before any disk or
/// picker I/O. The order is the contract: `--session-id` > `--resume <id>`
/// > `-r` (picker) > `-n` > `-i` > `-c` > a fresh session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionChoice<'a> {
    Explicit(&'a str),
    ResumeId(&'a str),
    ResumePicker,
    New,
    Interactive,
    Continue,
    Fresh,
}

/// Pick the [`SessionChoice`] for `flags`. Pure.
fn session_choice(flags: &crate::cli::parser::Flags) -> SessionChoice<'_> {
    use crate::cli::parser::ResumeArg;
    if let Some(sid) = &flags.session_id {
        SessionChoice::Explicit(sid)
    } else if let Some(r) = &flags.resume {
        match r {
            ResumeArg::Id(raw) => SessionChoice::ResumeId(raw),
            ResumeArg::Picker => SessionChoice::ResumePicker,
        }
    } else if flags.new {
        SessionChoice::New
    } else if flags.interactive {
        SessionChoice::Interactive
    } else if flags.continue_ {
        SessionChoice::Continue
    } else {
        SessionChoice::Fresh
    }
}

impl SessionResolution {
    /// The session id string, regardless of fresh/resume.
    fn sid(&self) -> &str {
        match self {
            SessionResolution::Fresh(s) | SessionResolution::Resume(s) => s,
        }
    }
}

// ─── run ──────────────────────────────────────────────────────────────────────

/// `csm run [csm-flags] [-- passthru...]`
///
/// Full launch path:
///   1. Parse args via the hand-rolled `cli::parser`.
///   2. Classify the launch ([`launch_context`]). `Print` execs claude
///      verbatim (the `csm claude` passthrough) and stops here.
///   3. Resolve `D` and the child env ([`launch_context::launch_dir`], the
///      credential-env strip for a managed account). Interactive only:
///      repair an unfinished switch, then the pre-launch switch rule
///      ([`prelaunch_decision`]).
///   4. Resolve session id: explicit `--session-id` > `--resume` > `-n` >
///      `-i` picker > `-c` > a fresh session. The session picker opens only
///      for `-i` or a bare `-r`/`--resume`.
///   5. Record the account and launch time in the sidecar, build `LaunchSpec`
///      and hand off to `run_relaunch_loop`.
///
/// Inside Orca (a pane or a structured session) nothing prompts and nothing
/// touches the Keychain, the network or the switch journal before claude
/// starts; csm's own lines go to its log except fatal errors and the one
/// relaunch line.
pub(crate) fn run(args: &[OsString]) -> anyhow::Result<()> {
    use crate::cli::parser::parse;
    use platform::relaunch::LaunchSpec;
    use session::alias::looks_like_uuid;

    let parsed = parse(args);
    let flags = &parsed.flags;

    // ── 0. `csm run --help` — run's own usage, never a launch ──────────────────
    // The parser only sets this for an `-h`/`--help` that arrived before any
    // passthru token and before `--`, so `csm run -- --help` still reaches
    // claude (see `cli::parser::Flags::help`).
    if flags.help {
        crate::print_run_help();
        return Ok(());
    }

    // ── 1. Classify the launch ────────────────────────────────────────────────
    let launch = launch_context::current(args);
    if launch.context == LaunchContext::Print {
        // `-p` or a piped stdin: claude runs verbatim, no sidecar, no usage
        // fetch, no switch, no relaunch.
        return crate::cmd::claude::cmd_claude(args);
    }
    let quiet = launch.context.is_orca();

    // ── 2. Resolve the working directory, D and the child env ─────────────────
    let cwd = std::env::current_dir().context("csm: cannot determine current directory")?;
    let dir = launch_context::launch_dir(quiet);
    if !quiet {
        // Interactive: an unfinished switch is repaired before the account
        // decision; inside Orca it waits until after the spawn (relaunch loop).
        prelaunch_recovery();
    }
    // Reads files only (store, stashes' oauth-account.json, D's .claude.json).
    let mut accounts = launch_accounts(dir.as_ref());
    if launch.context == LaunchContext::Interactive
        && let Some(line) = prelaunch_switch(&accounts)
    {
        eprintln!("{line}");
        accounts = launch_accounts(dir.as_ref());
    }
    let env = child_env(
        dir.as_ref()
            .map_or(launch_context::ConfigDirPin::Leave, |d| d.pin.clone()),
        accounts.active.is_some(),
        std::env::vars_os(),
    );

    // Every account's dead-credential warning, from the CACHED UsageData
    // only (no network). Inside Orca the lines go to csm's log.
    for line in launch_attention_warnings(&accounts) {
        if quiet {
            let _ = crate::hook::notify::append_log("launch", &line);
        } else {
            eprintln!("{line}");
        }
    }

    // ── 4. Resolve session id ──────────────────────────────────────────────────
    // A picker path may yield `None` = the user pressed Escape → cancel the launch.
    // Each arm yields a `SessionResolution` that records whether the id is a
    // brand-new session (→ `--session-id`, create) or an existing one off disk
    // (→ `--resume`, continue). Passing an existing id via `--session-id` is what
    // produced the `Error: Session ID … is already in use` failure.
    let resolution: SessionResolution = match session_choice(flags) {
        // `--session-id <uuid>`: the user explicitly asked to CREATE this id.
        SessionChoice::Explicit(sid) => SessionResolution::Fresh(sid.to_owned()),
        SessionChoice::ResumeId(raw) => {
            // Resolve alias if not UUID-shaped. Either way this is an
            // existing session the user asked to resume.
            let sid = if looks_like_uuid(raw) {
                raw.to_owned()
            } else {
                session::resolve_alias(raw)
                    .with_context(|| format!("csm: --resume alias resolution failed for {raw:?}"))?
            };
            SessionResolution::Resume(sid)
        }
        // `-r` without an id, and `-i`/`--interactive`: the session picker.
        SessionChoice::ResumePicker | SessionChoice::Interactive => {
            match resolve_session_via_picker(&cwd)? {
                Some(res) => res,
                None => {
                    eprintln!("csm: cancelled.");
                    return Ok(());
                }
            }
        }
        // `-n`/`--new`: explicit fresh session, no picker.
        SessionChoice::New => SessionResolution::Fresh(newuuid()),
        // `-c`/`--continue`: newest free session (Resume) or fresh.
        SessionChoice::Continue => match newest_free_sid(&cwd)? {
            Some(sid) => SessionResolution::Resume(sid),
            None => SessionResolution::Fresh(newuuid()),
        },
        // Default (no session flag): a fresh session, in every context.
        SessionChoice::Fresh => SessionResolution::Fresh(newuuid()),
    };

    let session_id: String = resolution.sid().to_owned();

    // ── 5. Build the claude CLI and launch ──────────────────────────────────────
    // Choose the verb by intent: `--session-id` creates a new session, `--resume`
    // continues an existing one. Using `--session-id` for an existing id is what
    // claude rejects with "Session ID … is already in use".
    // Restore previous mode/effort/model via sidecar flags (perfect-continue).
    let sidecar_path = paths::sidecar(&session_id);
    let existing_sidecar = sidecar::read_sidecar(&sidecar_path).unwrap_or_default();

    let cli = launch_cli(&resolution, &existing_sidecar, flags, &parsed.passthru);

    // Remember what this invocation asked for so a later `csm -r <sid>` restores
    // the mode/effort/model (perfect-continue) and a limit-switch hop can replay
    // the session-shaping claude flags. Best-effort: a launch must never fail
    // because the sidecar could not be written.
    // The account and launch time key usage captures and the follow check.
    let mut remembered = remembered_from_launch(flags, &parsed.passthru);
    let (account_id, born) = launch_stamp(dir.as_ref());
    remembered.account_id = account_id;
    remembered.born = Some(born);
    let _ = sidecar::merge_sidecar(&sidecar_path, &remembered);

    let spec = LaunchSpec {
        session_id,
        pin: dir
            .as_ref()
            .map_or(launch_context::ConfigDirPin::Leave, |d| d.pin.clone()),
        env,
        quiet,
        cwd,
        cli,
    };

    // PlatformLauncher is a type alias to PosixLauncher (unix) or WindowsLauncher
    // (Windows). Construct via Default so platform-specific changes are isolated.
    let launcher = <platform::PlatformLauncher as std::default::Default>::default();
    platform::relaunch::run_relaunch_loop(&launcher, &spec)
}

/// The accounts as the child will see them. Only a pinned launch reads `D`
/// as an explicit `CLAUDE_CONFIG_DIR` (whose identity lives in
/// `D/.claude.json`); an unpinned one inherits this process's own
/// environment, where an unset variable puts the identity in
/// `~/.claude.json`. Forcing `D` for an unpinned launch made the default
/// layout look logged out, so the sidecar lost its `account_id`. A launch
/// that removes the variable reads the way the child will: unset.
fn launch_accounts(dir: Option<&launch_context::LaunchDir>) -> account::AccountSet {
    match dir {
        Some(d) => account::AccountSet::load_pinned(&d.pin),
        None => account::AccountSet::load(),
    }
}

/// The account and `born` stamp this launch records, from a read of `D`
/// taken right before the spawn. The `accounts` read at the top of `run` is
/// older than the session picker, which waits on the user for as long as
/// they like; a switch that lands meanwhile (a peer's limit hop, Orca's GUI)
/// makes it stale. Handing that stale identity to `launch_born` would log a
/// switch back to it, then the next statusLine tick a switch forward again,
/// and every session born before that second event would lose its captures
/// (and with them the statusLine limit trigger).
fn launch_stamp(dir: Option<&launch_context::LaunchDir>) -> (Option<String>, i64) {
    let now = launch_accounts(dir);
    let born = crate::usage::local::launch_born(now.current_uuid.as_deref());
    (now.current, born)
}

/// What this launch hands the sidecar to remember: the mode/effort/model the
/// user named explicitly, and the arguments `csm run` forwarded to claude
/// untouched.
///
/// The passthru is stored as launched, non-empty only — the filtering of which
/// of those flags a relaunch may replay belongs to the hop that replays them
/// (`cli::carry::carry_passthru`), not to the launch that records them, so a
/// later change to that allow-list applies to sessions launched today. A
/// non-UTF-8 argument is recorded lossily: the sidecar is JSON, and a flag that
/// survives a switch with a mangled byte is a better outcome than none of them
/// surviving because one argument was not text.
fn remembered_from_launch(flags: &cli::parser::Flags, passthru: &[OsString]) -> sidecar::Sidecar {
    let launched: Vec<String> = passthru
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    sidecar::Sidecar {
        permission_mode: flags.permission_mode.clone(),
        effort: flags.effort.clone(),
        model: flags.model.clone(),
        passthru: (!launched.is_empty()).then_some(launched),
        ..Default::default()
    }
}

/// The claude CLI for a cold launch: the session verb, then mode/effort/model,
/// then the user's passthrough args and initial prompt.
///
/// The flags this invocation was given win over the values the sidecar
/// remembers from an earlier launch of the same session; a sidecar value is
/// used only when the matching flag is absent. (Until this was factored out,
/// `csm run` parsed `--permission-mode`/`--effort`/`--model` and then dropped
/// them: only the sidecar's values ever reached claude.)
fn launch_cli(
    resolution: &SessionResolution,
    remembered: &sidecar::Sidecar,
    flags: &cli::parser::Flags,
    passthru: &[OsString],
) -> Vec<OsString> {
    let mut cli: Vec<OsString> = Vec::new();
    match resolution {
        SessionResolution::Fresh(sid) => {
            cli.push(OsString::from("--session-id"));
            cli.push(OsString::from(sid));
        }
        SessionResolution::Resume(sid) => {
            cli.push(OsString::from("--resume"));
            cli.push(OsString::from(sid));
        }
    }
    let effective = sidecar::Sidecar {
        permission_mode: flags
            .permission_mode
            .clone()
            .or_else(|| remembered.permission_mode.clone()),
        effort: flags.effort.clone().or_else(|| remembered.effort.clone()),
        model: flags.model.clone().or_else(|| remembered.model.clone()),
        ..Default::default()
    };
    cli.extend(effective.sidecar_flags());
    cli.extend_from_slice(passthru);
    cli
}

// ─── session id resolution helpers ───────────────────────────────────────────

/// Open the interactive session picker.
///
/// Returns:
/// - `Ok(Some(resolution))` — a session to launch (selected → Resume, continued
///   → Resume, or fresh → Fresh, including the graceful degrade to Fresh when
///   there is no usable terminal / no rows).
/// - `Ok(None)` — the user pressed Escape / Ctrl-C: cancel the launch entirely.
fn resolve_session_via_picker(cwd: &std::path::Path) -> anyhow::Result<Option<SessionResolution>> {
    use picker::session::SessionRow as PickerRow;
    use picker::session::{PickedSession, SessionPicker};

    let rows = session::scan(cwd);

    // Convert `session::SessionRow` → `picker::session::SessionRow` (picker
    // wants an `is_live` field; `session` module doesn't carry that).
    let picker_rows: Vec<PickerRow> = rows
        .iter()
        .map(|r| PickerRow {
            sid: r.sid.clone(),
            mtime: r.mtime as u64,
            human_ts: r.human_ts.clone(),
            mode: r.mode.clone(),
            label: r.label.clone(),
            is_live: session::sid_live(&r.sid),
        })
        .collect();

    let sp = SessionPicker::new(picker_rows);
    // Always `None` today: a deliberate simplification, not yet derived.
    let newest_live_label: Option<&str> = None;

    match sp.pick(newest_live_label) {
        // `None` (no usable terminal) and `Fresh` both mean "start new" — degrade.
        None | Some(PickedSession::Fresh) => Ok(Some(SessionResolution::Fresh(newuuid()))),
        // Continue → newest free session (Resume) — but if there is none, the
        // unwrap_or falls back to a brand-new id, which must launch as Fresh.
        Some(PickedSession::Continue) => Ok(Some(match newest_free_sid(cwd)? {
            Some(sid) => SessionResolution::Resume(sid),
            None => SessionResolution::Fresh(newuuid()),
        })),
        Some(PickedSession::Resume(sid)) => Ok(Some(SessionResolution::Resume(sid))),
        // Escape / Ctrl-C → cancel the whole launch.
        Some(PickedSession::Cancel) => Ok(None),
    }
}

/// Return the newest free (non-live) session id for `cwd`, or `None`.
fn newest_free_sid(cwd: &std::path::Path) -> anyhow::Result<Option<String>> {
    let rows = session::scan(cwd);
    Ok(rows
        .into_iter()
        .find(|r| !session::sid_live(&r.sid))
        .map(|r| r.sid))
}

/// Pure core of the launch-time credential warning: every stderr line, from
/// a cached `UsageData` (keyed by account id), the labels to print and the
/// account `D` holds. No I/O — `now` is passed in for the relative age.
fn launch_attention_lines(
    data: &usage::UsageData,
    current: &str,
    label: &dyn Fn(&str) -> String,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut ids: Vec<&String> = data.profiles.keys().collect();
    ids.sort();
    for id in ids {
        if let Some(attention) = &data.profiles[id].attention {
            out.extend(usage::report::attention_block_lines(
                &label(id),
                attention,
                now,
            ));
        }
    }
    if let Some(attention) = data
        .profiles
        .get(current)
        .and_then(|pu| pu.attention.as_ref())
        && attention.kind == usage::model::AttentionKind::NeedsLogin
    {
        out.push(format!(
            "csm: warning: current account '{}' needs login — claude will show /login",
            label(current)
        ));
    }
    out
}

/// I/O shell: read the cache (best-effort, no network). Nothing when there
/// is no cache to read.
fn launch_attention_warnings(accounts: &account::AccountSet) -> Vec<String> {
    let Some(data) = crate::cmd::usage::read_usage_cache() else {
        return Vec::new();
    };
    launch_attention_lines(
        &data,
        accounts.current.as_deref().unwrap_or(""),
        &|id| accounts.label(id),
        chrono::Utc::now(),
    )
}

// ─── child env ────────────────────────────────────────────────────────────────

/// Pure: the child's env changes. `pin` is the `CLAUDE_CONFIG_DIR` to set
/// (only when the inherited value differs from `D`); `managed` says Orca's
/// active account is one of its managed accounts, in which case the explicit
/// auth overrides are stripped exactly as Orca strips them
/// ([`launch_context::auth_env_to_strip`]). Values are read only to classify
/// `ANTHROPIC_CUSTOM_HEADERS`; none is kept.
fn child_env(
    pin: launch_context::ConfigDirPin,
    managed: bool,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> ChildEnv {
    use launch_context::ConfigDirPin;
    let mut env = ChildEnv::default();
    match pin {
        ConfigDirPin::Set(dir) => {
            env.set
                .insert(OsString::from("CLAUDE_CONFIG_DIR"), dir.into_os_string());
        }
        ConfigDirPin::Unset => env.remove.push(OsString::from("CLAUDE_CONFIG_DIR")),
        ConfigDirPin::Leave => {}
    }
    if managed {
        let vars: Vec<(String, String)> = vars
            .into_iter()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.to_string_lossy().into_owned())))
            .collect();
        let strip = launch_context::auth_env_to_strip(
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            cfg!(windows),
        );
        env.remove.extend(strip.into_iter().map(OsString::from));
    }
    env
}

// ─── interactive pre-launch ───────────────────────────────────────────────────

/// What an Interactive launch does about a capped account.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreLaunch {
    /// Nothing to do: the account is viable, or its usage is unknown.
    Launch,
    /// Switch to this account before launching.
    Switch(String),
    /// Launch anyway, with this warning line.
    Warn(String),
}

/// Pure: design §5 "Interactive". Switch before launch only when the active
/// account is known to be capped, a viable candidate exists, and no other
/// claude is live in `D`; otherwise launch, with one warning when capped.
fn prelaunch_decision(
    current_label: &str,
    active_viable: Option<bool>,
    candidate: Option<(String, String)>,
    other_claude_live: bool,
) -> PreLaunch {
    if active_viable != Some(false) {
        return PreLaunch::Launch;
    }
    match candidate {
        None => PreLaunch::Warn(format!(
            "csm: warning: account {current_label} is capped and no other account has headroom"
        )),
        Some((_, label)) if other_claude_live => PreLaunch::Warn(format!(
            "csm: warning: account {current_label} is capped; another claude is running in this \
             config dir, so csm did not switch (run `csm accounts use {label}` to switch)"
        )),
        Some((id, _)) => PreLaunch::Switch(id),
    }
}

/// I/O shell for [`prelaunch_decision`]. Returns the one line to print.
fn prelaunch_switch(accounts: &account::AccountSet) -> Option<String> {
    use crate::orca::context::Context;
    use crate::orca::http::SystemHttp;
    use crate::orca::live::SystemProcs;
    use crate::orca::switch;

    let current = accounts.current.as_deref()?;
    let data = usage::fetch().ok()?;
    let active_viable = account::limit_switch::viable_in(&data, current);
    if active_viable != Some(false) {
        return None;
    }
    let candidate = account::scoring::pick_best_gated(&data, current, false, true)
        .ok()
        .flatten()
        .filter(|id| accounts.contains(id))
        .map(|id| {
            let label = accounts.label(&id);
            (id, label)
        });
    let procs = SystemProcs;
    let ctx = Context::current(&procs).ok()?;
    let other_live = candidate.is_some() && ctx.live_claude(&procs);
    let current_label = accounts.label(current);
    match prelaunch_decision(&current_label, active_viable, candidate, other_live) {
        PreLaunch::Launch => None,
        PreLaunch::Warn(line) => Some(line),
        PreLaunch::Switch(id) => {
            let http = SystemHttp::from_env();
            let to = accounts.label(&id);
            match ctx.with_switch_env(&procs, &http, |env| switch::switch(env, &id)) {
                Ok(r) if !matches!(r.outcome, switch::Outcome::Uncertain(_)) => Some(format!(
                    "csm: account {current_label} is capped; switched to {to}"
                )),
                Ok(_) => Some(format!(
                    "csm: warning: account {current_label} is capped; the switch to {to} ended \
                     uncertain (run `csm accounts doctor --fix`)"
                )),
                Err(e) => Some(format!(
                    "csm: warning: account {current_label} is capped; switching to {to} failed: {e}"
                )),
            }
        }
    }
}

/// Interactive only: repair an unfinished switch before the account
/// decision. A failure prints one line and the launch goes on.
fn prelaunch_recovery() {
    use crate::orca::context::Context;
    use crate::orca::http::SystemHttp;
    use crate::orca::live::SystemProcs;
    use crate::orca::switch::{self, Recovery};

    let state = paths::smart_dir_no_create();
    if !switch::read_journal(&state).is_some_and(|j| j.pending()) {
        return;
    }
    let procs = SystemProcs;
    let Ok(ctx) = Context::current(&procs) else {
        return;
    };
    let http = SystemHttp::from_env();
    let why = match ctx.with_switch_env(&procs, &http, switch::recover) {
        Ok(Recovery::Failed(why)) => why,
        // Another csm holds switch.lock (a switch or a login in progress):
        // nothing was attempted, so there is no failure to report. The
        // supervisor tries again once the child runs.
        Ok(Recovery::Busy) => return,
        // Orca runs and the repair has to wait for it to stop: nothing was
        // written, and the reason is worth one line.
        // Orca started during the repair (the hand-over did not verify):
        // Orca owns D now, and the line says why.
        Ok(Recovery::Deferred(why) | Recovery::Uncertain(why)) => {
            eprintln!("csm: an unfinished account switch was not repaired: {why}");
            return;
        }
        Err(e) => e.to_string(),
        Ok(_) => return,
    };
    eprintln!(
        "csm: an unfinished account switch could not be repaired ({why}); run `csm accounts doctor --fix`"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A switch that landed while the session picker was open: the
    /// sidecar's account and `born` come from `D` as it is at the spawn,
    /// and the launch logs no switch back to the account the pre-picker
    /// read saw.
    #[test]
    fn the_launch_stamp_reads_d_after_the_picker() {
        use crate::orca::testsupport::{make_stash, record_json, write_store};
        use crate::orca::userdata::{HostOs, default_user_data};
        let home = tempfile::tempdir().unwrap();
        let ud = default_user_data(HostOs::current(), home.path(), None, None);
        write_store(
            &ud,
            &[
                record_json(&ud, "id-a", "alice@example.com", None),
                record_json(&ud, "id-b", "bob@example.com", None),
            ],
            Some("id-a"),
        );
        make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        make_stash(&ud, "id-b", Some(br#"{"accountUuid":"u-b"}"#), None);
        let d = home.path().join("claude-d");
        std::fs::create_dir_all(&d).unwrap();
        let dir = launch_context::LaunchDir {
            d: d.clone(),
            pin: launch_context::ConfigDirPin::Set(d.clone()),
        };
        crate::testenv::with_test_home(home.path(), || {
            std::fs::create_dir_all(crate::paths::smart_dir_no_create()).unwrap();
            let set_d = |uuid: &str| {
                std::fs::write(
                    d.join(".claude.json"),
                    format!(r#"{{"oauthAccount":{{"accountUuid":"{uuid}"}}}}"#),
                )
                .unwrap();
            };
            // Before the picker: D on a, and a statusLine tick saw it.
            set_d("u-a");
            let before = launch_accounts(Some(&dir));
            assert_eq!(before.current.as_deref(), Some("id-a"));
            crate::usage::local::note_identity(Some("u-a"), 100);
            // While the picker is open, a peer's hop moves D to b and notes it.
            set_d("u-b");
            crate::usage::local::note_identity(Some("u-b"), 200);
            // The user picks a session; the stamp reads D now.
            let (account, born) = launch_stamp(Some(&dir));
            assert_eq!(account.as_deref(), Some("id-b"));
            assert!(born >= 200);
            assert_eq!(
                std::fs::read_to_string(crate::paths::last_identity()).unwrap(),
                "u-b",
                "no switch back to the pre-picker identity"
            );
            assert_eq!(
                std::fs::read_to_string(crate::paths::last_identity_switch())
                    .unwrap()
                    .trim(),
                "200",
                "the hop's switch event stays the last one"
            );
        });
    }

    fn choice_of(args: &[&str]) -> String {
        let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
        let parsed = crate::cli::parser::parse(&argv);
        format!("{:?}", session_choice(&parsed.flags))
    }

    /// The session-flag precedence: `--session-id` > `--resume <id>` > `-r`
    /// (picker) > `-n` > `-i` > `-c` > fresh, pair by pair, whatever the
    /// order on the command line.
    #[test]
    fn session_choice_precedence() {
        let sid = "11111111-2222-4333-8444-555555555555";
        let rid = "66666666-7777-4888-9999-aaaaaaaaaaaa";
        assert_eq!(choice_of(&[]), "Fresh");
        assert_eq!(choice_of(&["-c"]), "Continue");
        assert_eq!(choice_of(&["-i"]), "Interactive");
        assert_eq!(choice_of(&["-n"]), "New");
        assert_eq!(choice_of(&["-r"]), "ResumePicker");
        let resume = format!("ResumeId({rid:?})");
        let explicit = format!("Explicit({sid:?})");
        // An explicit `--resume <id>` wins over `-n`, in either order.
        assert_eq!(choice_of(&["-n", "--resume", rid]), resume);
        assert_eq!(choice_of(&["--resume", rid, "-n"]), resume);
        // `--session-id` wins over `--resume`.
        assert_eq!(choice_of(&["--resume", rid, "--session-id", sid]), explicit);
        assert_eq!(choice_of(&["--session-id", sid, "-r", rid]), explicit);
        // `-n` wins over `-i` and `-c`; `-i` wins over `-c`.
        assert_eq!(choice_of(&["-c", "-n"]), "New");
        assert_eq!(choice_of(&["-n", "-i"]), "New");
        assert_eq!(choice_of(&["-c", "-i"]), "Interactive");
        // A bare `-r` (picker) still wins over `-n`.
        assert_eq!(choice_of(&["-n", "-r"]), "ResumePicker");
    }

    // ── child_env ─────────────────────────────────────────────────────────────

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn child_env_pins_only_when_asked() {
        use launch_context::ConfigDirPin;
        let env = child_env(
            ConfigDirPin::Leave,
            false,
            vars(&[("ANTHROPIC_API_KEY", "x")]),
        );
        assert_eq!(env, ChildEnv::default());
        let env = child_env(
            ConfigDirPin::Set(std::path::PathBuf::from("/Users/example/.claude")),
            false,
            Vec::new(),
        );
        assert_eq!(
            env.set.get(&OsString::from("CLAUDE_CONFIG_DIR")),
            Some(&OsString::from("/Users/example/.claude"))
        );
        assert!(env.remove.is_empty());
    }

    /// Orca main without `CLAUDE_CONFIG_DIR`: the child loses the inherited
    /// value instead of getting an explicit `~/.claude`, and the auth strip
    /// still applies.
    #[test]
    fn child_env_removes_the_dir_when_orca_runs_without_it() {
        use launch_context::ConfigDirPin;
        let env = child_env(
            ConfigDirPin::Unset,
            true,
            vars(&[("ANTHROPIC_API_KEY", "x")]),
        );
        assert!(env.set.is_empty());
        assert!(env.remove.contains(&OsString::from("CLAUDE_CONFIG_DIR")));
        assert!(env.remove.contains(&OsString::from("ANTHROPIC_API_KEY")));
    }

    #[test]
    fn child_env_strips_auth_for_a_managed_account() {
        let env = child_env(
            launch_context::ConfigDirPin::Leave,
            true,
            vars(&[
                ("ANTHROPIC_API_KEY", "x"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "y"),
                ("ANTHROPIC_CUSTOM_HEADERS", "Authorization: Bearer z"),
                ("PATH", "/usr/bin"),
            ]),
        );
        let mut removed: Vec<String> = env
            .remove
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        removed.sort();
        assert_eq!(
            removed,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_CUSTOM_HEADERS",
                "CLAUDE_CODE_OAUTH_TOKEN"
            ]
        );
        // No value is ever kept.
        assert!(format!("{env:?}").find("Bearer").is_none());
    }

    // ── prelaunch_decision ────────────────────────────────────────────────────

    #[test]
    fn prelaunch_launches_when_viable_or_unknown() {
        let cand = Some(("id-b".to_owned(), "bob".to_owned()));
        assert_eq!(
            prelaunch_decision("alice", Some(true), cand.clone(), false),
            PreLaunch::Launch
        );
        assert_eq!(
            prelaunch_decision("alice", None, cand, false),
            PreLaunch::Launch
        );
    }

    #[test]
    fn prelaunch_switches_only_with_a_candidate_and_no_other_claude() {
        let cand = Some(("id-b".to_owned(), "bob".to_owned()));
        assert_eq!(
            prelaunch_decision("alice", Some(false), cand.clone(), false),
            PreLaunch::Switch("id-b".into())
        );
        match prelaunch_decision("alice", Some(false), cand, true) {
            PreLaunch::Warn(l) => assert!(l.contains("csm accounts use bob"), "{l}"),
            other => panic!("{other:?}"),
        }
        match prelaunch_decision("alice", Some(false), None, false) {
            PreLaunch::Warn(l) => assert!(l.contains("alice"), "{l}"),
            other => panic!("{other:?}"),
        }
    }

    // ── session-verb selection (regression: "Session ID … is already in use") ──
    // Mirror the cold-launch verb choice in main(): Fresh → `--session-id`,
    // Resume → `--resume`. The bug was that an existing (resumed) session id was
    // launched with `--session-id`, which claude rejects as "already in use".

    /// The leading two CLI tokens (verb + id) main() builds for a resolution.
    fn launch_verb_and_id(res: &SessionResolution) -> (OsString, OsString) {
        let cli = launch_cli(res, &Default::default(), &parse_flags(&[]), &[]);
        (cli[0].clone(), cli[1].clone())
    }

    /// Run the real `csm run` parser over `args` and return its flags.
    fn parse_flags(args: &[&str]) -> cli::parser::Flags {
        let os: Vec<OsString> = args.iter().map(OsString::from).collect();
        cli::parser::parse(&os).flags
    }

    fn strs(cli: &[OsString]) -> Vec<String> {
        cli.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    // ── launch_cli: explicit --permission-mode/--effort/--model reach claude ──
    // Regression: `csm --model X` used to launch claude without `--model` at
    // all, because the parsed flag was never put back on the CLI.

    #[test]
    fn launch_cli_forwards_explicit_flags_before_passthru() {
        let res = SessionResolution::Fresh("11111111-2222-3333-4444-555555555555".to_owned());
        let flags = parse_flags(&[
            "--permission-mode",
            "plan",
            "--effort",
            "high",
            "--model",
            "claude-x-1",
        ]);
        let passthru = [OsString::from("--settings"), OsString::from("s.json")];
        let cli = launch_cli(&res, &Default::default(), &flags, &passthru);
        assert_eq!(
            strs(&cli),
            [
                "--session-id",
                "11111111-2222-3333-4444-555555555555",
                "--permission-mode",
                "plan",
                "--effort",
                "high",
                "--model",
                "claude-x-1",
                "--settings",
                "s.json",
            ]
        );
    }

    #[test]
    fn launch_cli_explicit_flag_wins_over_sidecar() {
        let res = SessionResolution::Resume("aabd04a6-7a93-4f38-88cb-ff942f94d013".to_owned());
        let remembered = sidecar::Sidecar {
            model: Some("remembered-model".to_owned()),
            effort: Some("low".to_owned()),
            ..Default::default()
        };
        let flags = parse_flags(&["--model", "explicit-model"]);
        let cli = strs(&launch_cli(&res, &remembered, &flags, &[]));
        // The remembered effort still applies; the remembered model does not.
        assert_eq!(
            cli,
            [
                "--resume",
                "aabd04a6-7a93-4f38-88cb-ff942f94d013",
                "--effort",
                "low",
                "--model",
                "explicit-model",
            ]
        );
        assert!(!cli.iter().any(|t| t == "remembered-model"));
    }

    #[test]
    fn launch_cli_uses_sidecar_when_no_flag_given() {
        let res = SessionResolution::Resume("aabd04a6-7a93-4f38-88cb-ff942f94d013".to_owned());
        let remembered = sidecar::Sidecar {
            permission_mode: Some("acceptEdits".to_owned()),
            ..Default::default()
        };
        let cli = strs(&launch_cli(&res, &remembered, &parse_flags(&[]), &[]));
        assert_eq!(
            cli,
            [
                "--resume",
                "aabd04a6-7a93-4f38-88cb-ff942f94d013",
                "--permission-mode",
                "acceptEdits",
            ]
        );
    }

    #[test]
    fn launch_cli_without_flags_or_sidecar_is_verb_and_passthru_only() {
        let res = SessionResolution::Fresh("11111111-2222-3333-4444-555555555555".to_owned());
        let passthru = [OsString::from("hello")];
        let cli = strs(&launch_cli(
            &res,
            &Default::default(),
            &parse_flags(&[]),
            &passthru,
        ));
        assert_eq!(
            cli,
            [
                "--session-id",
                "11111111-2222-3333-4444-555555555555",
                "hello"
            ]
        );
    }

    // ── remembered_from_launch: what the sidecar keeps from this launch ───────

    #[test]
    fn remembered_from_launch_keeps_flags_and_passthru() {
        let flags = parse_flags(&["--model", "claude-x-1", "--effort", "high"]);
        let passthru = [
            OsString::from("--dangerously-skip-permissions"),
            OsString::from("--add-dir"),
            OsString::from("/Users/example/a"),
        ];
        let remembered = remembered_from_launch(&flags, &passthru);
        assert_eq!(remembered.model.as_deref(), Some("claude-x-1"));
        assert_eq!(remembered.effort.as_deref(), Some("high"));
        assert_eq!(
            remembered.passthru,
            Some(vec![
                "--dangerously-skip-permissions".to_owned(),
                "--add-dir".to_owned(),
                "/Users/example/a".to_owned(),
            ]),
            "the passthru is remembered as launched, in order"
        );
    }

    #[test]
    fn remembered_from_launch_records_passthru_without_any_csm_flag() {
        // The case the write gate used to miss: no --model/--effort/
        // --permission-mode, so sidecar_flags() is empty, yet the launch still
        // has a shape a switch must restore.
        let passthru = [OsString::from("--dangerously-skip-permissions")];
        let remembered = remembered_from_launch(&parse_flags(&[]), &passthru);
        assert!(remembered.sidecar_flags().is_empty());
        assert_eq!(
            remembered.passthru,
            Some(vec!["--dangerously-skip-permissions".to_owned()])
        );
    }

    #[test]
    fn remembered_from_launch_of_a_bare_launch_is_empty() {
        // Nothing to remember → nothing written, so an existing sidecar's
        // passthru is not clobbered with an empty list by a later bare resume.
        let remembered = remembered_from_launch(&parse_flags(&[]), &[]);
        assert!(remembered.sidecar_flags().is_empty());
        assert!(remembered.passthru.is_none());
    }

    #[test]
    fn fresh_resolution_launches_with_session_id() {
        let res = SessionResolution::Fresh("11111111-2222-3333-4444-555555555555".to_owned());
        let (verb, id) = launch_verb_and_id(&res);
        assert_eq!(verb, OsString::from("--session-id"));
        assert_eq!(id, OsString::from("11111111-2222-3333-4444-555555555555"));
        assert_eq!(res.sid(), "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn resume_resolution_launches_with_resume_not_session_id() {
        // The exact failure mode: an existing id must NOT be passed via
        // --session-id (claude → "Session ID … is already in use").
        let existing = "aabd04a6-7a93-4f38-88cb-ff942f94d013".to_owned();
        let res = SessionResolution::Resume(existing.clone());
        let (verb, id) = launch_verb_and_id(&res);
        assert_eq!(
            verb,
            OsString::from("--resume"),
            "resumed sessions must use --resume, never --session-id"
        );
        assert_ne!(
            verb,
            OsString::from("--session-id"),
            "the 'already in use' bug: --session-id on an existing id"
        );
        assert_eq!(id, OsString::from(&existing));
        assert_eq!(res.sid(), existing);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Launch-time credential warnings — `launch_attention_lines`/
    // the current account id.
    // ══════════════════════════════════════════════════════════════════════════

    fn attention_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    fn profile_with_attention(kind: usage::model::AttentionKind) -> usage::model::ProfileUsage {
        usage::model::ProfileUsage {
            attention: Some(usage::model::Attention {
                kind,
                message: match kind {
                    usage::model::AttentionKind::NeedsLogin => "credentials expired".to_string(),
                    usage::model::AttentionKind::NeedsRefresh => "access token expired".to_string(),
                },
                action: match kind {
                    usage::model::AttentionKind::NeedsLogin => {
                        "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"
                            .to_string()
                    }
                    usage::model::AttentionKind::NeedsRefresh => {
                        "csm accounts use home".to_string()
                    }
                },
                since_epoch: Some(attention_now().timestamp() - 3600),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn launch_attention_lines_empty_when_nothing_needs_attention() {
        let data = usage::UsageData::default();
        assert!(
            launch_attention_lines(&data, "home", &|id| id.to_owned(), attention_now()).is_empty()
        );
    }

    #[test]
    fn launch_attention_lines_includes_every_attentive_profile_sorted() {
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "work".to_string(),
            profile_with_attention(usage::model::AttentionKind::NeedsLogin),
        );
        profiles.insert(
            "home".to_string(),
            profile_with_attention(usage::model::AttentionKind::NeedsRefresh),
        );
        let data = usage::UsageData {
            profiles,
            ..Default::default()
        };
        // Neither "other" nor a healthy profile is the current one, so no
        // extra "current profile needs login" line.
        let lines = launch_attention_lines(&data, "other", &|id| id.to_owned(), attention_now());
        assert_eq!(
            lines.len(),
            4,
            "two 2-line blocks, sorted by name: {lines:#?}"
        );
        assert!(lines[0].starts_with("\u{26a0} home:"), "{lines:#?}");
        assert!(lines[2].starts_with("\u{26a0} work:"), "{lines:#?}");
        assert!(
            !lines.iter().any(|l| l.contains("current account")),
            "current profile isn't in the map at all: {lines:#?}"
        );
    }

    #[test]
    fn launch_attention_lines_adds_extra_line_when_current_profile_needs_login() {
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "work".to_string(),
            profile_with_attention(usage::model::AttentionKind::NeedsLogin),
        );
        let data = usage::UsageData {
            profiles,
            ..Default::default()
        };
        let lines = launch_attention_lines(&data, "work", &|id| id.to_owned(), attention_now());
        assert_eq!(
            lines.last().unwrap(),
            "csm: warning: current account 'work' needs login — claude will show /login"
        );
    }

    #[test]
    fn launch_attention_lines_no_extra_line_when_current_profile_only_needs_refresh() {
        // NeedsRefresh is not a login-blocking state — launching under it IS
        // the fix — so it must never get the "/login" extra line.
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "home".to_string(),
            profile_with_attention(usage::model::AttentionKind::NeedsRefresh),
        );
        let data = usage::UsageData {
            profiles,
            ..Default::default()
        };
        let lines = launch_attention_lines(&data, "home", &|id| id.to_owned(), attention_now());
        assert!(
            !lines.iter().any(|l| l.contains("current account")),
            "{lines:#?}"
        );
    }
}
