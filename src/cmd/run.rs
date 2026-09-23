//! `csm run` — the full launch pipeline: parse flags, resolve the profile dir
//! (auto-pick or explicit pin), resolve the session id (picker/resume/new),
//! build the claude CLI, and hand off to the relaunch loop.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cmd::support::{is_interactive, newuuid, profile_name_for_dir, resolve_profile_dir};
use crate::config::Config;
use crate::orca::follow::{self, EffectiveDecision, EffectiveInput, Source};
use crate::orca::slot::Slot;
use crate::{account, cli, epoch, orca, paths, picker, platform, session, sidecar, usage};

/// Budget for the launch path's one read of Orca's live selection. A timeout
/// is recorded in the negative cache, so the next launches skip the socket.
const LAUNCH_ORCA_READ_BUDGET: std::time::Duration = std::time::Duration::from_millis(1200);

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
///   2. Resolve profile dir: `--profile` pin > proactive `pick_account` with
///      stale-usage picker gate > the effective current profile. That last
///      one is [`follow::effective_current`]: the inherited
///      `CLAUDE_CONFIG_DIR` (else the default profile) outside Orca mode;
///      in Orca mode it may follow Orca's live active account, and it is
///      never the slot unless the slot is the only profile.
///   3. Resolve session id: explicit `--session-id` > `--resume` > picker >
///      auto-resume default.
///   4. Build `LaunchSpec` (session_id + profile_dir + cwd + cli) and hand off
///      to `run_relaunch_loop`.
///
/// Account picker gates:
///   `-i`/`--interactive` (manual pick) — ALWAYS open the account picker (and
///   the session picker), skipping auto-pick, as long as interactive + a
///   non-empty ProfileMap. `--profile <p>` still wins (explicit choice).
///
///   Otherwise the *stale-usage / no-data* picker opens when ALL of:
///   - interactive (isatty(0) && isatty(1))
///   - proactive pick context (not `--profile` / not `--no-pick`)
///   - `pick_account` returned `Err(FetchFailed)` (usage collection failed) OR
///     `Err(NoUsableData)` (fetch ok but no profile had scorable usage —
///     "couldn't tell" must not silently keep current).
///     NOT when hook / `--profile` / `--no-pick` / non-interactive, and NOT for
///     `AllSaturated` (real limits read → warn + keep current).
pub(crate) fn run(args: &[OsString]) -> anyhow::Result<()> {
    use crate::cli::parser::{ResumeArg, parse};
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

    // ── 1. Resolve the working directory ──────────────────────────────────────
    let cwd = std::env::current_dir().context("csm: cannot determine current directory")?;

    // ── 2. Resolve profile dir ─────────────────────────────────────────────────
    let profiles = account::ProfileMap::load().context("csm: failed to load profiles.json")?;
    // Orca mode (config + slot), loaded once. `None` = Orca mode OFF, and
    // then every step below behaves exactly as it did before Orca existed.
    let orca_mode = orca::slot::config_and_slot(&profiles);
    // A queued Orca select is applied by a detached `csm orca sync`. It is
    // spawned only AFTER this launch has resolved its current profile (which
    // reads the same pending file), so the child cannot clear the file or
    // move Orca's active account mid-resolution.
    let sync_pending = orca_mode.is_some() && orca::pending::exists();
    if orca_mode.is_none() && orca::slot::config_unreadable() {
        eprintln!(
            "csm: warning: config.json unreadable; Orca mode cannot be determined, so an Orca \
             slot profile (if any) is NOT excluded from this launch's pick. Fix the file, or \
             pass --profile."
        );
    }

    let profile_dir: PathBuf = if let Some(pin) = &flags.profile {
        // `--profile <p>` pin — explicit choice, skip all picking (wins over -i).
        if sync_pending {
            spawn_orca_sync();
        }
        let dir = resolve_profile_dir(pin, &profiles)?;
        PathBuf::from(dir)
    } else {
        // The one effective current profile every fallback below lands on
        // (never the Orca slot unless it is the only profile).
        let current = resolve_effective_current(&profiles, orca_mode.as_ref());
        if sync_pending {
            spawn_orca_sync();
        }
        if let Some(name) = &current.mirror_default
            && let Err(e) = crate::cas::write_default_profile(name, &profiles)
        {
            eprintln!("csm: warning: could not follow Orca's account in csm's default: {e}");
        }
        let ctx = PickCtx {
            current_name: &current.name,
            current_dir: PathBuf::from(&current.dir),
            prefer_current: current.prefer_current,
            slot: orca_mode.as_ref().map(|(_, s)| s),
            // Print mode under Orca (source-control AI launches, `csm -p`):
            // no picker, no network usage fetch.
            print_mode: orca_mode.is_some() && is_print_mode(&parsed.passthru),
        };
        let dir = if flags.interactive {
            // `-i`/`--interactive` — manual pick: disable *all* auto-pick / skip.
            // Always open the account picker (recommendation-ordered, never the
            // silent auto-pick), regardless of whether usage collection succeeded. Empty
            // ProfileMap (toss/first-boot) keeps current. The session picker is
            // also forced later by the same flag.
            match force_account_pick(&profiles, &ctx)? {
                Some(dir) => dir,
                None => {
                    eprintln!("csm: cancelled.");
                    return Ok(());
                }
            }
        } else if flags.no_pick {
            // `--no-pick` — keep current profile without scoring.
            ctx.current_dir.clone()
        } else {
            // Proactive pick (include_current=true — no-op switch if already best).
            // `None` = the stale-usage picker was cancelled with Escape → abort.
            match proactive_pick_profile(&profiles, &ctx, flags.pick_account)? {
                Some(dir) => dir,
                None => {
                    eprintln!("csm: cancelled.");
                    return Ok(());
                }
            }
        };
        if let Some(line) = orca_active_line(&current, &dir) {
            eprintln!("{line}");
        }
        dir
    };

    // Print every profile's dead-credential warning, right after the pick is
    // resolved and regardless of what got picked (a cancelled pick already
    // returned above — this only runs on a launch that is actually going
    // ahead). Reads the CACHED UsageData only — no network — so a dead
    // token can never slow down or fail a launch.
    print_launch_attention_warnings(&profile_dir, &profiles);

    // ── 3. Resolve session id ──────────────────────────────────────────────────
    // A picker path may yield `None` = the user pressed Escape → cancel the launch.
    // Each arm yields a `SessionResolution` that records whether the id is a
    // brand-new session (→ `--session-id`, create) or an existing one off disk
    // (→ `--resume`, continue). Passing an existing id via `--session-id` is what
    // produced the `Error: Session ID … is already in use` failure.
    let resolution: SessionResolution = if let Some(explicit_sid) = &flags.session_id {
        // `--session-id <uuid>`: the user explicitly asked to CREATE this id.
        SessionResolution::Fresh(explicit_sid.clone())
    } else if let Some(resume_arg) = &flags.resume {
        match resume_arg {
            ResumeArg::Id(raw) => {
                // Resolve alias if not UUID-shaped. Either way this is an
                // existing session the user asked to resume.
                let sid = if looks_like_uuid(raw) {
                    raw.clone()
                } else {
                    session::resolve_alias(raw).with_context(|| {
                        format!("csm: --resume alias resolution failed for {raw:?}")
                    })?
                };
                SessionResolution::Resume(sid)
            }
            ResumeArg::Picker => match resolve_session_via_picker(&cwd)? {
                Some(res) => res,
                None => {
                    eprintln!("csm: cancelled.");
                    return Ok(());
                }
            },
        }
    } else if flags.new {
        // `-n`/`--new`: explicit fresh session, no picker.
        SessionResolution::Fresh(newuuid())
    } else if flags.interactive {
        // `-i`/`--interactive`: open session picker.
        match resolve_session_via_picker(&cwd)? {
            Some(res) => res,
            None => {
                eprintln!("csm: cancelled.");
                return Ok(());
            }
        }
    } else if flags.continue_ {
        // `-c`/`--continue`: newest free session (Resume) or fresh.
        match newest_free_sid(&cwd)? {
            Some(sid) => SessionResolution::Resume(sid),
            None => SessionResolution::Fresh(newuuid()),
        }
    } else {
        // Default (no explicit flag): always open the session picker.
        match resolve_session_default(&cwd)? {
            Some(res) => res,
            None => {
                eprintln!("csm: cancelled.");
                return Ok(());
            }
        }
    };

    let session_id: String = resolution.sid().to_owned();

    // ── 4. Build the claude CLI and launch ──────────────────────────────────────
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
    let remembered = remembered_from_launch(flags, &parsed.passthru);
    if !remembered.sidecar_flags().is_empty() || remembered.passthru.is_some() {
        let _ = sidecar::merge_sidecar(&sidecar_path, &remembered);
    }

    let spec = LaunchSpec {
        session_id,
        profile_dir,
        cwd,
        cli,
    };

    // PlatformLauncher is a type alias to PosixLauncher (unix) or WindowsLauncher
    // (Windows). Construct via Default so platform-specific changes are isolated.
    let launcher = <platform::PlatformLauncher as std::default::Default>::default();
    platform::relaunch::run_relaunch_loop(&launcher, &spec)
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

/// Default session resolution (no explicit flags): ALWAYS open the session
/// picker so the choice (new / continue / pick an existing session) is never
/// made silently. The picker's `__NEW__` / `__CONTINUE__` sentinels mean a
/// zero- or one-session directory still presents a meaningful choice.
///
/// Skipping only happens where a picker *cannot* run: no usable terminal
/// (pipe / CI / hook) degrades to a fresh session inside
/// [`resolve_session_via_picker`], so non-interactive launches never block.
///
/// Returns `Ok(Some(resolution))` to launch, or `Ok(None)` when the picker was
/// cancelled (Escape / Ctrl-C).
fn resolve_session_default(cwd: &std::path::Path) -> anyhow::Result<Option<SessionResolution>> {
    resolve_session_via_picker(cwd)
}

/// Return the newest free (non-live) session id for `cwd`, or `None`.
fn newest_free_sid(cwd: &std::path::Path) -> anyhow::Result<Option<String>> {
    let rows = session::scan(cwd);
    Ok(rows
        .into_iter()
        .find(|r| !session::sid_live(&r.sid))
        .map(|r| r.sid))
}

/// Pure core of surface 4b: every stderr line the launch-time credential
/// warning prints, from a cached `UsageData` + which profile is about to
/// launch. No I/O — the real clock/cache-read live only in
/// `print_launch_attention_warnings`, so this is fully unit-testable.
///
/// Deliberately `(&UsageData, &str, DateTime<Utc>) -> Vec<String>` rather
/// than the design spec's plain `&UsageData -> Vec<String>` — `now` is
/// needed to compute a fresh relative age (never baked into the cached data;
/// see `report::Attention`'s doc), and `current_profile` is needed to decide
/// whether the extra "current profile needs login" line applies. Documented
/// deviation, consistent with `report::render_table`/`attention_lines`
/// gaining the same `now` parameter for the same reason.
fn launch_attention_lines(
    data: &usage::UsageData,
    current_profile: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut names: Vec<&String> = data.profiles.keys().collect();
    names.sort();
    for name in names {
        if let Some(attention) = &data.profiles[name].attention {
            out.extend(usage::report::attention_block_lines(name, attention, now));
        }
    }
    if let Some(attention) = data
        .profiles
        .get(current_profile)
        .and_then(|pu| pu.attention.as_ref())
        && attention.kind == usage::model::AttentionKind::NeedsLogin
    {
        out.push(format!(
                "csm: warning: current profile '{current_profile}' needs login — claude will show /login"
            ));
    }
    out
}

/// I/O shell for surface 4b: read the cache (best-effort, no network), derive
/// the launching profile's name, and print every resulting line to stderr.
/// Silently does nothing when there's no cache to read — a missing/unreadable
/// cache is not itself something a launch should warn about.
fn print_launch_attention_warnings(profile_dir: &Path, profiles: &account::ProfileMap) {
    let Some(data) = crate::cmd::usage::read_usage_cache() else {
        return;
    };
    let current = profile_name_for_dir(profile_dir, profiles);
    for line in launch_attention_lines(&data, &current, chrono::Utc::now()) {
        eprintln!("{line}");
    }
}

// ─── effective current profile (Orca-aware) ──────────────────────────────────

/// What the account-pick helpers need to know about the current profile.
struct PickCtx<'a> {
    /// The effective current profile's name.
    current_name: &'a str,
    /// Its dir: what every "keep current" fallback launches into.
    current_dir: PathBuf,
    /// Keep the current profile while it is viable (it came from Orca).
    prefer_current: bool,
    /// The Orca slot while Orca mode is ON: never a candidate or a row.
    slot: Option<&'a Slot>,
    /// Orca-mode print launch: cached usage only, never a picker.
    print_mode: bool,
}

/// Is this a `claude -p` / `--print` launch? Pure.
fn is_print_mode(passthru: &[OsString]) -> bool {
    passthru
        .iter()
        .take_while(|a| a.as_os_str() != "--")
        .any(|a| a == "-p" || a == "--print")
}

/// The I/O shell around [`follow::effective_current`]: read the inherited
/// env dir and the default state, and (Orca mode only) the pending select
/// plus, when the decision needs it, Orca's live selection within
/// [`LAUNCH_ORCA_READ_BUDGET`].
fn resolve_effective_current(
    profiles: &account::ProfileMap,
    orca_mode: Option<&(Config, Slot)>,
) -> EffectiveDecision {
    let env_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty());
    let default_state = profiles.default_name();
    let default_dir = profiles.default_dir().to_string_lossy().into_owned();
    let slot = orca_mode.map(|(_, s)| s);
    let pending = slot.and_then(|_| orca::pending::read().ok().flatten());
    let base = EffectiveInput {
        explicit_pin: None,
        env_dir: env_dir.as_deref(),
        default_state: &default_state,
        default_dir: &default_dir,
        slot,
        registry: profiles,
        pending: pending.as_ref(),
        live: None,
        bindings: None,
        now: epoch::now_secs() as i64,
    };
    let (live, bindings) = match orca_mode {
        Some((config, slot)) if follow::needs_live(&base) => {
            match orca::user_data_dir_for(config.orca()) {
                Some(ud) => match orca::live_selection_in(&ud, LAUNCH_ORCA_READ_BUDGET) {
                    orca::OrcaState::Live(sel) => {
                        let b = orca::bind::compute(
                            &ud,
                            &sel,
                            profiles,
                            Some(slot),
                            &config.orca().bindings,
                        );
                        (Some(sel), Some(b))
                    }
                    _ => (None, None),
                },
                None => (None, None),
            }
        }
        _ => (None, None),
    };
    follow::effective_current(&EffectiveInput {
        live: live.as_ref(),
        bindings: bindings.as_ref(),
        ..base
    })
}

/// `csm: orca active → X`, only when `X` came from Orca (live or a pending
/// select) and is the profile actually launching. Pure.
fn orca_active_line(current: &EffectiveDecision, launched: &Path) -> Option<String> {
    let followed = matches!(current.source, Source::OrcaLive | Source::Pending);
    let same = launched
        .to_str()
        .is_some_and(|d| crate::cas::platform::dirs_equal(d, &current.dir));
    (followed && same).then(|| format!("csm: orca active → {}", current.name))
}

/// Start `csm orca sync --quiet` detached (new session, stdio null, never
/// awaited) to apply a queued Orca selection. Best-effort and silent.
fn spawn_orca_sync() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = platform::detach::spawn_detached(&exe, &["orca", "sync", "--quiet"]);
    }
}

/// Proactive account pick with stale-usage picker fallback.
///
/// See [`crate::picker::account`] for what the stale-usage picker shows.
///
/// Returns `Ok(Some(dir))` with the resolved profile directory, or `Ok(None)`
/// when the stale-usage picker was cancelled (Escape / Ctrl-C) — the caller aborts.
///
/// Pick guard (matches the legacy shell implementation's behavior):
/// - `pick_account(current, include_current=true)` → scoring pick, which
///   weighs session and week_all through `scoring::is_viable_pcts`.
///   `week_fable` (the model-scoped weekly cap) no longer factors into
///   viability at all — a current profile whose only exhausted window is
///   `week_fable` is left in place here; the Stop hook handles that case with
///   a same-account model fallback instead of a proactive account switch.
/// - `Err(FetchFailed)` (usage collection failed) or `Err(NoUsableData)` (fetch
///   ok but no scorable usage) + interactive → stale-usage account picker.
/// - same errors + non-interactive → silent fail-safe to current.
/// - `Err(AllSaturated)` → warn + keep current (no picker; real limits read).
fn proactive_pick_profile(
    profiles: &account::ProfileMap,
    ctx: &PickCtx<'_>,
    _force_pick: bool,
) -> anyhow::Result<Option<PathBuf>> {
    use account::scoring::{PickPolicy, ScoringError};

    let current_profile = ctx.current_name;
    let current_dir = ctx.current_dir.clone();

    // No ProfileMap (toss / first-boot) — skip all picking.
    if profiles.is_empty() {
        return Ok(Some(current_dir));
    }

    // Orca OFF: exactly `pick_account(current, true)`. Orca ON adds the
    // prefer-current rule for an Orca-chosen account; the account layer
    // excludes the slot from the candidates itself.
    let policy = PickPolicy {
        include_current: true,
        apply_stale_gate: true,
        prefer_current: ctx.prefer_current,
        exclude: &[],
    };
    let picked = if ctx.print_mode {
        account::pick_account_cached(current_profile, &policy)
    } else {
        account::pick_account_with(current_profile, &policy)
    };
    match picked {
        Ok(None) => {
            // Already on the best profile — keep current.
            Ok(Some(current_dir))
        }
        Ok(Some(winner)) => {
            let dir = resolve_profile_dir(&winner, profiles)
                .context("csm: proactive pick — winner profile not in map")?;
            if winner != current_profile {
                eprintln!("csm: auto-pick → {winner}");
            }
            Ok(Some(PathBuf::from(dir)))
        }
        Err(ScoringError::AllSaturated) => {
            eprintln!(
                "csm: warning: all accounts at session/week limit — keeping current profile ({current_profile})"
            );
            Ok(Some(current_dir))
        }
        // Usage collection unreachable OR fetch succeeded but carried no usable
        // usage for any profile. Both mean "we could not determine the best
        // account" — never silently keep current. Open the interactive picker
        // (interactive) or fail safe to current (non-interactive), same as a
        // stale-usage miss. A print launch never opens a picker.
        Err(ScoringError::FetchFailed(_)) | Err(ScoringError::NoUsableData) => {
            if ctx.print_mode {
                return Ok(Some(current_dir));
            }
            stale_usage_pick(profiles, ctx)
        }
    }
}

/// Stale-usage account picker. See [`crate::picker::account`].
///
/// Interactive + fetch-miss → open the account picker with stale usage data.
/// Non-interactive → silent fail-safe to current profile.
///
/// Returns `Ok(Some(dir))` to launch under `dir`, or `Ok(None)` when the user
/// pressed Escape / Ctrl-C in the picker (cancel the launch entirely).
fn stale_usage_pick(
    profiles: &account::ProfileMap,
    ctx: &PickCtx<'_>,
) -> anyhow::Result<Option<PathBuf>> {
    // TTY gate: isatty(0) && isatty(1) — matches zsh `[[ -t 0 && -t 1 ]]`.
    if !is_interactive() {
        return Ok(Some(ctx.current_dir.clone()));
    }
    run_account_picker(profiles, ctx, "stale-usage picker")
}

/// Forced account picker for `-i`/`--interactive` (manual pick).
///
/// Unlike [`stale_usage_pick`], this is invoked even when usage collection
/// succeeded and a
/// confident auto-pick exists: `-i` means "let me choose", so we skip the
/// auto-pick entirely and always present the recommendation-ordered picker
/// (Enter still takes the recommendation). The TTY gate still applies — a piped
/// `-i` has no usable terminal for the picker, so it keeps the current profile.
/// An empty ProfileMap (toss / first-boot) likewise keeps current, nothing to pick.
fn force_account_pick(
    profiles: &account::ProfileMap,
    ctx: &PickCtx<'_>,
) -> anyhow::Result<Option<PathBuf>> {
    if profiles.is_empty() || !is_interactive() || ctx.print_mode {
        return Ok(Some(ctx.current_dir.clone()));
    }
    run_account_picker(profiles, ctx, "manual account picker")
}

/// Shared account-picker driver for [`stale_usage_pick`] and
/// [`force_account_pick`]. Builds recommendation-ordered rows (stale usage if
/// that is all we have) and maps the picker outcome:
/// - Selected → that profile's dir.
/// - Cancelled (Escape / Ctrl-C) → `None` (caller aborts the launch).
/// - Unavailable (no usable terminal / no rows) → keep current profile.
fn run_account_picker(
    profiles: &account::ProfileMap,
    pick: &PickCtx<'_>,
    ctx: &str,
) -> anyhow::Result<Option<PathBuf>> {
    use picker::engine::PickerOutcome;

    let current_dir = &pick.current_dir;
    let rows = build_account_rows(profiles, pick.slot);
    let ap = picker::AccountPicker::new(rows);

    match ap.pick() {
        PickerOutcome::Selected(winner) => {
            let dir = resolve_profile_dir(&winner, profiles)
                .with_context(|| format!("csm: {ctx} — selected profile not in map"))?;
            Ok(Some(PathBuf::from(dir)))
        }
        // Escape / Ctrl-C → cancel the launch.
        PickerOutcome::Cancelled => Ok(None),
        // No usable terminal / empty → keep current profile (graceful degrade).
        // `SelectedMulti` is unreachable here (the account picker is single-select
        // `run_picker`); fold it into the same graceful degrade to stay exhaustive.
        PickerOutcome::Unavailable | PickerOutcome::SelectedMulti(_) => {
            Ok(Some(current_dir.to_path_buf()))
        }
    }
}

/// Recommendation rank for a stale-usage picker row, mirroring `scoring::pick_best`.
///
/// The picker renders top-to-bottom with the cursor on the FIRST row, so
/// pressing Enter selects it. We therefore order rows so the
/// recommended profile (the one `pick_best` would auto-select when usage
/// collection succeeds)
/// leads, and the user can just press Enter.
///
/// Viability is delegated to `scoring::is_viable_pcts` — the SINGLE viability
/// authority also used by `pick_best_at` — rather than a second hand-rolled
/// check. `week_fable_pct` no longer sinks a row on its own (a
/// model-scoped-only cap still leaves the row usable on another model); only
/// `session_pct`/`week_all_pct` do.
///
/// Returns a sort key where SMALLER sorts first:
/// - `0` bucket = viable candidate (no error, has week_all.pct, and
///   `is_viable_pcts(session_pct, week_all_pct, week_fable_pct)` is `true` —
///   i.e. neither session nor week_all is at or over its threshold;
///   `week_fable_pct` is passed through but no longer read by the predicate).
///   Within it, SOONER effective weekly reset epoch ranks first (`i64::MAX`
///   when unknown, so a known reset beats an unknown one), then HIGHER
///   week_all.pct (negated), matching `pick_best`'s ranking. The "effective"
///   epoch is the LATER of `week_all`'s and `week_fable`'s reset (when both
///   are known) — mirrors `pick_best_at`'s identical rule (see its doc): a
///   viable row is under neither cap, but only fully fresh once BOTH weekly
///   windows have rolled over.
/// - `1` bucket = everything else (saturated on any of the three dimensions,
///   session-limited, errored, or no data), ordered by name for stability.
///
/// `name` is the final tie-break so ordering is deterministic. `now` is the
/// reference instant for reset-string parsing — callers pass `Utc::now()`
/// once per picker build; tests inject a fixed instant for determinism.
fn account_row_rank(
    name: &str,
    data: &picker::account::StaleProfileData,
    now: chrono::DateTime<chrono::Utc>,
) -> (u8, i64, i64, String) {
    use account::scoring::{ABSENT_SESSION_PCT, effective_reset_epoch, is_viable_pcts};

    let session_pct = data.session_pct.unwrap_or(ABSENT_SESSION_PCT);
    let viable = data.error.is_none()
        && data.week_all_pct.is_some()
        && is_viable_pcts(session_pct, data.week_all_pct, data.week_fable_pct);

    if !viable {
        // Non-viable rows sink to the bottom, ordered by name.
        return (1, 0, 0, name.to_owned());
    }

    let week_pct = data.week_all_pct.unwrap();
    // Soonest EFFECTIVE weekly reset epoch first → i64::MAX when unknown so
    // known beats unknown. Higher week_all.pct next → negate so smaller sorts
    // first. Each dimension prefers its machine-native `resets_at` epoch
    // (carried straight through from the local collector) over re-parsing the
    // `resets` display string — same precedence as
    // `UsageSection::reset_instant` / `scoring::pick_best_at`. `week_fable`
    // carries no display-string fallback here (only `resets_at`) — a minor,
    // deliberate asymmetry: the stale-usage picker's cache read never needed a
    // fable resets STRING before this field existed, and the epoch is what
    // ranking actually consumes.
    let week_all_epoch = data.resets_at.or_else(|| {
        data.resets
            .as_deref()
            .and_then(|r| account::reset::resets_to_epoch_at(r, now).ok())
            .map(|dt| dt.timestamp())
    });
    let week_fable_epoch = data.week_fable_resets_at;
    let epoch = effective_reset_epoch(week_all_epoch, week_fable_epoch);
    (0, epoch, -week_pct, name.to_owned())
}

/// Build `AccountRow` list for the stale-usage picker, ordered by recommendation so
/// the top row is what `pick_best` would auto-select (Enter selects it).
fn build_account_rows(
    profiles: &account::ProfileMap,
    slot: Option<&Slot>,
) -> Vec<picker::account::AccountRow> {
    use picker::account::{AccountRow, StaleProfileData};

    // Read the smart-dir cache (the positive TTL cache `usage::fetch` writes
    // from local collection).
    let cache_path = paths::usage_cache();
    let cache_mtime = cache_mtime(&cache_path);
    let cache_data = crate::cmd::usage::read_usage_cache();

    let all_names = account_row_names(profiles, cache_data.as_ref(), slot);

    // Build (name, StaleProfileData) so we can order by recommendation before
    // rendering rows. (HashMap iteration order is non-deterministic; the rank's
    // name tie-break makes the final order stable regardless.)
    let mut entries: Vec<(String, StaleProfileData)> = all_names
        .into_iter()
        .map(|profile| {
            let error = cache_data
                .as_ref()
                .and_then(|d| d.errors.as_ref())
                .and_then(|e| e.get(&profile))
                .cloned();
            let pu = cache_data.as_ref().and_then(|d| d.profiles.get(&profile));
            let data = if let Some(err) = error {
                StaleProfileData {
                    session_pct: None,
                    week_all_pct: None,
                    resets: None,
                    resets_at: None,
                    week_fable_pct: None,
                    week_fable_resets_at: None,
                    error: Some(err),
                    attention: None,
                }
            } else if let Some(pu) = pu {
                StaleProfileData {
                    session_pct: pu.session.as_ref().map(|s| s.pct),
                    week_all_pct: pu.week_all.as_ref().map(|s| s.pct),
                    resets: pu.week_all.as_ref().and_then(|s| s.resets.clone()),
                    resets_at: pu.week_all.as_ref().and_then(|s| s.resets_at),
                    week_fable_pct: pu.week_fable.as_ref().map(|s| s.pct),
                    week_fable_resets_at: pu.week_fable.as_ref().and_then(|s| s.resets_at),
                    error: None,
                    attention: pu.attention.clone(),
                }
            } else {
                StaleProfileData {
                    session_pct: None,
                    week_all_pct: None,
                    resets: None,
                    resets_at: None,
                    week_fable_pct: None,
                    week_fable_resets_at: None,
                    error: None,
                    attention: None,
                }
            };
            (profile, data)
        })
        .collect();

    // Recommended-first ordering: the top row is what pick_best would auto-select,
    // so Enter (cursor starts on row 0) selects the recommendation. One shared
    // `now` so every row's reset epoch is parsed against the same instant.
    let now = chrono::Utc::now();
    entries.sort_by_key(|(name, data)| account_row_rank(name, data, now));

    // The recommended row is the FIRST entry *iff* it is a viable candidate
    // (rank bucket 0). When every profile is saturated / errored / dataless,
    // pick_best would recommend nothing, so no row gets the ★.
    let recommended_idx = entries
        .first()
        .filter(|(name, data)| account_row_rank(name, data, now).0 == 0)
        .map(|_| 0usize);

    entries
        .iter()
        .enumerate()
        .map(|(idx, (profile, data))| {
            let recommended = Some(idx) == recommended_idx;
            AccountRow::build(profile, data, cache_mtime, recommended)
        })
        .collect()
}

/// The account picker's row names: every configured profile plus any extra
/// profile the usage cache knows, minus the Orca slot (not an account of its
/// own). Registry names first in sorted order, then cache-only names. Pure.
fn account_row_names(
    profiles: &account::ProfileMap,
    cache_data: Option<&usage::UsageData>,
    slot: Option<&Slot>,
) -> Vec<String> {
    let mut all_names: Vec<String> = profiles
        .names_sorted()
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(data) = cache_data {
        for name in data
            .profiles
            .keys()
            .chain(data.errors.as_ref().map(|e| e.keys()).into_iter().flatten())
        {
            if !all_names.contains(name) {
                all_names.push(name.clone());
            }
        }
    }
    all_names.retain(|n| slot.is_none_or(|s| !s.is_profile(n)));
    all_names
}

/// Modification time of a usage cache file, as a unix epoch, or `None` when
/// the file is absent/unreadable.
fn cache_mtime(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(epoch::from_systemtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_mtime_absent_file_is_none() {
        assert_eq!(
            cache_mtime(std::path::Path::new("/nonexistent/for/csm")),
            None
        );
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

    // ── account_row_rank (stale-usage picker: recommended profile leads) ─────────
    // The picker starts the cursor on row 0, so the top row is what Enter
    // selects. account_row_rank must order rows the same way pick_best chooses,
    // so the recommendation leads and a bare Enter picks it.

    use picker::account::StaleProfileData;

    fn data(session: Option<i64>, week: Option<i64>, resets: Option<&str>) -> StaleProfileData {
        StaleProfileData {
            session_pct: session,
            week_all_pct: week,
            resets: resets.map(|s| s.to_owned()),
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        }
    }

    /// Like [`data`] but also carrying a `week_fable_pct` reading (`None` =
    /// no model-scoped weekly cap for this profile).
    fn data_with_fable(
        session: Option<i64>,
        week: Option<i64>,
        resets: Option<&str>,
        fable_pct: Option<i64>,
    ) -> StaleProfileData {
        StaleProfileData {
            week_fable_pct: fable_pct,
            ..data(session, week, resets)
        }
    }

    /// Like [`data`] but with an explicit `resets_at` epoch, for tests that
    /// pin the epoch-preferred ranking.
    fn data_with_epoch(
        session: Option<i64>,
        week: Option<i64>,
        resets: Option<&str>,
        resets_at: Option<i64>,
    ) -> StaleProfileData {
        StaleProfileData {
            session_pct: session,
            week_all_pct: week,
            resets: resets.map(|s| s.to_owned()),
            resets_at,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: None,
            attention: None,
        }
    }

    /// Fixed reference instant for reset parsing (noon UTC Jun 17 2026 — the
    /// `reset.rs` test convention), so date-string ordering never depends on
    /// the wall clock at test time.
    fn rank_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap()
    }

    /// Sort names by rank and return them in display order (row 0 first).
    fn ranked_order(mut rows: Vec<(&str, StaleProfileData)>) -> Vec<String> {
        rows.sort_by_key(|(name, data)| account_row_rank(name, data, rank_now()));
        rows.into_iter().map(|(n, _)| n.to_owned()).collect()
    }

    #[test]
    fn viable_sooner_reset_leads() {
        // Both viable; the SOONER weekly reset is the recommendation (pick_best
        // drains the account whose budget refills first), even against a much
        // lower week_all.pct. It must be row 0.
        let order = ranked_order(vec![
            (
                "later",
                data(Some(2), Some(70), Some("Jun 20 at 9pm (Asia/Seoul)")),
            ),
            (
                "sooner",
                data(Some(5), Some(10), Some("Jun 18 at 9pm (Asia/Seoul)")),
            ),
        ]);
        assert_eq!(order, vec!["sooner", "later"]);
    }

    #[test]
    fn resets_at_epoch_preferred_over_resets_string() {
        // "sooner" carries a `resets_at` epoch well before "later"'s parsed
        // reset date, but a `resets` STRING that would fail to parse at all —
        // `resets_at` must still win, mirroring
        // `UsageSection::reset_instant`'s own precedence.
        let sooner_epoch = rank_now().timestamp() + 1_000;
        let order = ranked_order(vec![
            (
                "later",
                data(Some(2), Some(70), Some("Jun 20 at 9pm (Asia/Seoul)")),
            ),
            (
                "sooner",
                data_with_epoch(
                    Some(5),
                    Some(10),
                    Some("not a valid reset string"),
                    Some(sooner_epoch),
                ),
            ),
        ]);
        assert_eq!(order, vec!["sooner", "later"]);
    }

    #[test]
    fn viable_no_resets_higher_week_pct_leads() {
        // Both viable with unknown resets → falls back to the higher
        // week_all.pct (pick_best's secondary key). It must be row 0.
        let order = ranked_order(vec![
            ("low", data(Some(2), Some(10), None)),
            ("high", data(Some(5), Some(40), None)),
        ]);
        assert_eq!(order, vec!["high", "low"]);
    }

    #[test]
    fn saturated_and_errored_sink_below_viable() {
        let errored = StaleProfileData {
            session_pct: None,
            week_all_pct: None,
            resets: None,
            resets_at: None,
            week_fable_pct: None,
            week_fable_resets_at: None,
            error: Some("no credentials".to_owned()),
            attention: None,
        };
        let order = ranked_order(vec![
            ("saturated", data(Some(5), Some(96), None)), // week >= 95 → not viable
            ("errored", errored),
            ("viable", data(Some(5), Some(50), None)),
            ("nodata", data(None, None, None)),
        ]);
        // The one viable profile must lead; the rest sink (name-ordered).
        assert_eq!(order[0], "viable");
        assert!(order[1..].contains(&"saturated".to_owned()));
        assert!(order[1..].contains(&"errored".to_owned()));
        assert!(order[1..].contains(&"nodata".to_owned()));
    }

    #[test]
    fn session_limited_is_not_viable() {
        // session.pct >= 99 → excluded from viable even if week is low.
        let order = ranked_order(vec![
            ("limited", data(Some(99), Some(5), None)),
            ("ok", data(Some(10), Some(20), None)),
        ]);
        assert_eq!(order[0], "ok");
    }

    #[test]
    fn known_reset_beats_unknown() {
        // A known reset epoch beats an unknown (None) one regardless of pct,
        // mirroring pick_best's primary key (unknown parses to i64::MAX).
        let order = ranked_order(vec![
            ("noreset", data(Some(3), Some(80), None)),
            (
                "hasreset",
                data(Some(3), Some(30), Some("Jun 18 at 9pm (Asia/Seoul)")),
            ),
        ]);
        assert_eq!(order, vec!["hasreset", "noreset"]);
    }

    // ── model-scoped weekly (week_fable) gate ──────────────────────────────
    // account_row_rank no longer sinks a fable-saturated row: it
    // routes through the same `scoring::is_viable_pcts` authority
    // `pick_best_at` uses, and that predicate dropped the week_fable branch
    // (a model-scoped-only cap is handled by the Stop hook's same-account
    // model fallback instead of exclusion — see `src/hook/detect.rs`).

    #[test]
    fn fable_saturated_row_no_longer_sinks_below_viable() {
        // The bucket assertion is the proof: a fable-saturated row must land
        // in the viable bucket (0), not sink to bucket 1. This is checked
        // directly on the rank tuple, not inferred from sort order, so
        // nothing about naming or a second row can make it pass for the
        // wrong reason.
        let (bucket, ..) = account_row_rank(
            "fable_capped",
            &data_with_fable(Some(5), Some(10), None, Some(100)),
            rank_now(),
        );
        assert_eq!(
            bucket, 0,
            "a fable-saturated row must land in the viable bucket (0), not sink to bucket 1"
        );

        // And with a second, uncapped row present: the capped row's name
        // sorts BEFORE the uncapped one, so if a regression reintroduced
        // sinking (bucket 1 vs bucket 0), the capped row would visibly move
        // to the end. Seeing it stay first is real proof both rows tied on
        // rank, not name-order luck landing on the row under test.
        let order = ranked_order(vec![
            (
                "aaa_fable_capped",
                data_with_fable(Some(5), Some(10), None, Some(100)),
            ),
            ("zzz_avail", data_with_fable(Some(5), Some(10), None, None)),
        ]);
        assert_eq!(
            order,
            vec!["aaa_fable_capped", "zzz_avail"],
            "both rows are viable now; identical rank key ties to name order — a \
             sinking regression would move the capped row to the end instead"
        );
    }

    #[test]
    fn fable_none_row_stays_viable() {
        // week_fable_pct: None (no model-scoped cap for this profile) must not
        // sink the row — it stays in the viable (bucket 0) group.
        let order = ranked_order(vec![(
            "only",
            data_with_fable(Some(5), Some(10), None, None),
        )]);
        assert_eq!(order, vec!["only"]);
    }

    #[test]
    fn fable_just_under_saturation_stays_viable() {
        use account::scoring::SATURATION_PCT;
        let order = ranked_order(vec![(
            "almost",
            data_with_fable(Some(5), Some(10), None, Some(SATURATION_PCT - 1)),
        )]);
        assert_eq!(order, vec!["almost"]);
    }

    #[test]
    fn only_fable_difference_no_longer_affects_row_rank() {
        // Two rows identical except for fable saturation. When a
        // model-scoped-only cap used to exclude a profile outright, the
        // uncapped one always led; now both are viable and tie on rank (same
        // reset, same week_pct), so name order breaks the tie. This is a
        // consequence of dropping the viability branch, not a ranking change.
        //
        // Names are picked so the alphabetically-first one ("avail") carries
        // the WORSE (higher) fable pct: if rank tracked fable pct instead of
        // name — the regression this test exists to catch — "fable_ok" (the
        // lower pct) would lead instead, not silently agree.
        let resets = Some("Jun 20 at 9pm (Asia/Seoul)");
        let order = ranked_order(vec![
            (
                "avail",
                data_with_fable(Some(5), Some(30), resets, Some(99)),
            ),
            (
                "fable_ok",
                data_with_fable(Some(5), Some(30), resets, Some(20)),
            ),
        ]);
        assert_eq!(order, vec!["avail", "fable_ok"]);
    }

    #[test]
    fn effective_epoch_uses_later_of_week_all_and_fable_reset() {
        // Mirrors `scoring::ranking_uses_later_of_week_all_and_fable_reset`:
        // the LATER of the two known reset epochs is the binding constraint.
        let sooner_fable_epoch = rank_now().timestamp() + 1_000; // well before Jun 20
        let later_all_epoch = rank_now().timestamp() + 10_000; // still before Jun 20

        // "flat": both dimensions resolve to the SAME (later) epoch as
        // "mixed"'s effective (max) epoch, but via week_all directly, so this
        // pins the max() computation rather than tying on a single field.
        let mixed = StaleProfileData {
            week_fable_resets_at: Some(sooner_fable_epoch),
            ..data_with_epoch(Some(5), Some(10), None, Some(later_all_epoch))
        };
        let sooner_flat = StaleProfileData {
            week_fable_resets_at: Some(sooner_fable_epoch),
            ..data_with_epoch(Some(5), Some(30), None, Some(sooner_fable_epoch))
        };
        let order = ranked_order(vec![("mixed", mixed), ("sooner_flat", sooner_flat)]);
        assert_eq!(
            order,
            vec!["sooner_flat", "mixed"],
            "the row whose LATER (binding) dimension resets sooner must lead"
        );
    }

    #[test]
    fn needs_refresh_attention_does_not_change_rank_bucket() {
        // A `NeedsRefresh` profile still has usable percentages, so it must
        // stay in the viable bucket (0) exactly like a plain healthy row —
        // `attention` only adds display information, it is not a viability
        // input (see `account_row_rank`'s doc).
        let plain = data(Some(5), Some(10), None);
        let with_attention = StaleProfileData {
            attention: Some(usage::model::Attention {
                kind: usage::model::AttentionKind::NeedsRefresh,
                message: "credentials expired".to_string(),
                action: "csm --profile home".to_string(),
                since_epoch: Some(1_000),
            }),
            ..data(Some(5), Some(10), None)
        };
        assert_eq!(
            account_row_rank("home", &plain, rank_now()).0,
            0,
            "plain viable row must be bucket 0"
        );
        assert_eq!(
            account_row_rank("home", &with_attention, rank_now()).0,
            0,
            "a NeedsRefresh row with usable percentages must stay bucket 0"
        );
        assert_eq!(
            account_row_rank("home", &plain, rank_now()),
            account_row_rank("home", &with_attention, rank_now()),
            "attention must not change the rank tuple at all"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Launch-time credential warnings — `launch_attention_lines`/
    // `profile_name_for_dir`.
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
                    usage::model::AttentionKind::NeedsRefresh => "csm --profile home".to_string(),
                },
                since_epoch: Some(attention_now().timestamp() - 3600),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn launch_attention_lines_empty_when_nothing_needs_attention() {
        let data = usage::UsageData::default();
        assert!(launch_attention_lines(&data, "home", attention_now()).is_empty());
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
        let lines = launch_attention_lines(&data, "other", attention_now());
        assert_eq!(
            lines.len(),
            4,
            "two 2-line blocks, sorted by name: {lines:#?}"
        );
        assert!(lines[0].starts_with("\u{26a0} home:"), "{lines:#?}");
        assert!(lines[2].starts_with("\u{26a0} work:"), "{lines:#?}");
        assert!(
            !lines.iter().any(|l| l.contains("current profile")),
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
        let lines = launch_attention_lines(&data, "work", attention_now());
        assert_eq!(
            lines.last().unwrap(),
            "csm: warning: current profile 'work' needs login — claude will show /login"
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
        let lines = launch_attention_lines(&data, "home", attention_now());
        assert!(
            !lines.iter().any(|l| l.contains("current profile")),
            "{lines:#?}"
        );
    }

    // ── effective current profile (Orca-aware launch path) ─────────────────

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn print_mode_detects_short_and_long_flag_before_the_separator() {
        assert!(is_print_mode(&os(&["-p", "hello"])));
        assert!(is_print_mode(&os(&["--model", "m", "--print"])));
        assert!(!is_print_mode(&os(&["hello"])));
        assert!(!is_print_mode(&os(&["--", "-p"])));
        assert!(!is_print_mode(&os(&["--permission-mode", "plan"])));
    }

    fn decision(source: Source, dir: &str) -> EffectiveDecision {
        EffectiveDecision {
            name: "work".to_owned(),
            dir: dir.to_owned(),
            source,
            prefer_current: true,
            mirror_default: None,
        }
    }

    #[test]
    fn orca_active_line_only_when_the_followed_profile_launches() {
        let d = decision(Source::OrcaLive, "/Users/example/.claude.work");
        assert_eq!(
            orca_active_line(&d, Path::new("/Users/example/.claude.work/")).as_deref(),
            Some("csm: orca active → work")
        );
        assert_eq!(
            orca_active_line(&d, Path::new("/Users/example/.claude.home")),
            None,
            "an auto-pick away from the followed profile prints nothing"
        );
        let p = decision(Source::Pending, "/Users/example/.claude.work");
        assert!(orca_active_line(&p, Path::new("/Users/example/.claude.work")).is_some());
        let legacy = decision(Source::Legacy, "/Users/example/.claude.work");
        assert_eq!(
            orca_active_line(&legacy, Path::new("/Users/example/.claude.work")),
            None
        );
    }

    #[test]
    fn account_row_names_exclude_the_slot() {
        let mut pm = account::ProfileMap::default();
        pm.insert("work".into(), "/Users/example/.claude.work".into());
        pm.insert("orca".into(), "/Users/example/.claude.orca".into());
        let mut data = usage::UsageData::default();
        data.profiles.insert("orca".into(), Default::default());
        data.profiles.insert("extra".into(), Default::default());
        let slot = Slot {
            name: "orca".into(),
            dir: "/Users/example/.claude.orca".into(),
        };
        assert_eq!(
            account_row_names(&pm, Some(&data), Some(&slot)),
            vec!["work".to_string(), "extra".to_string()]
        );
        // Orca OFF: unchanged union.
        let mut off = account_row_names(&pm, Some(&data), None);
        off.sort();
        assert_eq!(off, vec!["extra", "orca", "work"]);
    }

    /// Write `profiles.json` + the default state under the test HOME.
    fn registry_with_default(home: &Path, names: &[&str], default: &str) -> account::ProfileMap {
        let mut pm = account::ProfileMap::default();
        for n in names {
            pm.insert(
                (*n).to_owned(),
                home.join(format!(".claude.{n}"))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        pm.save().unwrap();
        crate::cas::write_default_profile(default, &pm).unwrap();
        account::ProfileMap::load().unwrap()
    }

    /// Orca mode OFF: the decision is exactly the old
    /// `derive_current_profile_name` / `current_profile_dir` pair, for every
    /// shape of inherited `CLAUDE_CONFIG_DIR`.
    #[test]
    fn effective_current_without_orca_equals_the_legacy_derivation() {
        use crate::cmd::support::{current_profile_dir, derive_current_profile_name};

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let work_dir = home.join(".claude.work").to_string_lossy().into_owned();
        let stray = home.join(".claude.stray").to_string_lossy().into_owned();
        crate::testenv::with_test_home(home, || {
            let pm = registry_with_default(home, &["home", "work"], "home");
            for env in [
                None,
                Some(""),
                Some(work_dir.as_str()),
                Some(stray.as_str()),
                Some("/"),
            ] {
                crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", env, || {
                    let d = resolve_effective_current(&pm, None);
                    assert_eq!(d.name, derive_current_profile_name(&pm), "env {env:?}");
                    assert_eq!(
                        PathBuf::from(&d.dir),
                        current_profile_dir(&pm),
                        "env {env:?}"
                    );
                    assert_eq!(d.source, Source::Legacy);
                    assert!(!d.prefer_current);
                    assert_eq!(d.mirror_default, None);
                });
            }
        });
    }

    /// Orca mode ON, Orca not running: an env dir of unset / the slot / the
    /// default dir follows csm's default state, never the slot.
    #[test]
    fn effective_current_in_orca_mode_never_lands_in_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        crate::testenv::with_test_home(home, || {
            let pm = orca::integrate::test_support::orca_home(home, &["home", "work"]);
            crate::cas::write_default_profile("home", &pm).unwrap();
            let pm = account::ProfileMap::load().unwrap();
            let orca_mode = orca::slot::config_and_slot(&pm);
            assert!(orca_mode.is_some());
            let slot_dir = home.join(".claude.orca").to_string_lossy().into_owned();
            let home_dir = home.join(".claude.home").to_string_lossy().into_owned();
            let work_dir = home.join(".claude.work").to_string_lossy().into_owned();
            for env in [None, Some(slot_dir.as_str()), Some(home_dir.as_str())] {
                crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", env, || {
                    let d = resolve_effective_current(&pm, orca_mode.as_ref());
                    assert_eq!(d.name, "home", "env {env:?}");
                    assert_eq!(d.source, Source::DefaultState);
                    assert!(!d.prefer_current);
                });
            }
            // A deliberate per-shell pin is kept.
            crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", Some(&work_dir), || {
                let d = resolve_effective_current(&pm, orca_mode.as_ref());
                assert_eq!((d.name.as_str(), d.source), ("work", Source::EnvPin));
            });
        });
    }
}
