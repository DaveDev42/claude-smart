mod account;
mod cas;
mod cli;
mod hook;
mod paths;
mod picker;
mod platform;
mod session;
mod sidecar;
mod statusline;
mod usage;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use uuid::Uuid;

/// Generate a fresh lowercase UUID v4 for use as `--session-id`.
fn newuuid() -> String {
    Uuid::new_v4().to_string()
}

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

fn main() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();

    // argv[0]-aware dispatch: if this binary is invoked as a known alias, treat
    // it as if that subcommand was the first argument (spec §2 "Multi-call binary").
    let argv0 = args
        .first()
        .and_then(|a| {
            std::path::Path::new(a)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();

    // Determine which subcommand to dispatch and the rest of argv.
    let subcommand: &str;
    let rest: &[OsString];

    if argv0 == "csm-hook" {
        // Invoked as `csm-hook` directly (symlink / rename form) — spec §2.
        subcommand = "hook";
        rest = &args[1..];
    } else if args.len() >= 2 {
        let candidate = args[1].to_string_lossy();
        // Top-level `--version`/`-V` and `--help`/`-h` belong to csm itself, not to
        // claude. (To pass these through to claude, use `csm run -- --version`.)
        // Intercept only when they are the very first token so `csm run --help`
        // routing into cmd_run's own usage still works.
        match candidate.as_ref() {
            "--version" | "-V" => {
                println!("csm {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => {}
        }
        match candidate.as_ref() {
            "run" | "hook" | "profiles" | "usage" | "cas" | "pick-account" | "scan"
            | "current-usage" | "sidecar" | "statusline" | "completions" | "newuuid" => {
                subcommand = Box::leak(candidate.into_owned().into_boxed_str());
                rest = &args[2..];
            }
            _ => {
                // No recognized subcommand → implicit `csm run` fallthrough.
                subcommand = "run";
                rest = &args[1..];
            }
        }
    } else {
        // Bare `csm` → implicit `csm run`.
        subcommand = "run";
        rest = &args[1..];
    }

    match subcommand {
        "run" => cmd_run(rest),
        "hook" => cmd_hook(rest),
        "profiles" => cmd_profiles(rest),
        "usage" => cmd_usage(rest),
        "cas" => cmd_cas(rest),
        "pick-account" => cmd_pick_account(rest),
        "scan" => cmd_scan(rest),
        "current-usage" => cmd_current_usage(rest),
        "sidecar" => cmd_sidecar(rest),
        "statusline" => cmd_statusline(rest),
        "completions" => cmd_completions(rest),
        "newuuid" => {
            println!("{}", newuuid());
            Ok(())
        }
        other => {
            eprintln!("csm: unknown subcommand: {other}");
            eprintln!("run `csm --help` for the full surface");
            std::process::exit(1);
        }
    }
}

/// Print the top-level `csm --help` surface (noun-verb).
///
/// The reserved subcommand words are DELIBERATELY disjoint from `claude`'s
/// subcommand set (agents/auth/auto-mode/doctor/install/mcp/plugin/project/
/// setup-token/ultrareview/update). Any first token NOT listed here falls
/// through to an implicit `csm run` → forwarded verbatim to `claude`, so
/// `csm mcp …`, `csm doctor`, etc. reach claude untouched.
fn print_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!("csm {v} — claude-smart launcher\n");
    println!("USAGE");
    println!("  csm [claude-args...]                 bare = smart launch (implicit `csm run`)");
    println!(
        "  csm run [csm-flags] [-- claude...]   smart launcher (session + account + relaunch)"
    );
    println!("  csm <subcommand> ...\n");
    println!("RUN FLAGS (account + session selection)");
    println!("  --profile <name>                     launch under this profile (skip all picking)");
    println!("  -i, --interactive                    manual pick: force account + session pickers");
    println!("  --no-pick                            keep current profile, no scoring");
    println!("  -c, --continue                       resume newest free session");
    println!("  (default: always opens the session picker — new / continue / pick existing —");
    println!("   and auto-picks the best account by usage; opens the account picker when usage");
    println!(
        "   is unavailable — hub down or no scorable data — instead of silently staying put)\n"
    );
    println!("PROFILES (registry — ~/.config/claude-as/profiles.json)");
    println!("  csm profiles [list]                  list configured profiles");
    println!("  csm profiles add  <name> [<dir>]     register (dir default ~/.claude.<name>)");
    println!("  csm profiles set  <name> <dir>       register/overwrite a profile dir");
    println!("  csm profiles rm   <name>             unregister (refused if it is the default)");
    println!("  csm profiles use  <name>             set machine default + floor");
    println!("  csm profiles edit                    interactive editor (TTY)");
    println!("  csm profiles dir  [<name>]           print a profile's dir (default if omitted)\n");
    println!("USAGE METERING");
    println!("  csm usage [--json] [--no-fetch]      multi-profile usage table (offline-aware)\n");
    println!("OTHER");
    println!("  csm pick-account [<cur>] [--include-current]   scoring → winner profile");
    println!("  csm scan [<cwd>]                     session TSV for the picker");
    println!("  csm sidecar {{read|write|merge|flags}} <sid> [k=v...]");
    println!("  csm statusline                       `<profile>@<host>` for the shell prompt");
    println!("  csm completions {{zsh|bash|pwsh}}      shell completions");
    println!("  csm newuuid                          fresh lowercase UUID v4");
    println!(
        "  csm cas ...                          eval-class shim contract (machine interface)\n"
    );
    println!("Words not listed above forward to `claude` (e.g. `csm mcp`, `csm doctor`).");
    println!("To pass a csm-reserved flag to claude, use `csm run -- <args>`.");
}

// ─── run ──────────────────────────────────────────────────────────────────────

/// `csm run [csm-flags] [-- passthru...]`
///
/// Full launch path per spec §2:
///   1. Parse args via the hand-rolled `cli::parser`.
///   2. Resolve profile dir: `--profile` pin > proactive `pick_account` with
///      hub-down picker gate §4a > current `CLAUDE_CONFIG_DIR`.
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
///   Otherwise the *hub-down / no-data* picker opens when ALL of:
///   - interactive (isatty(0) && isatty(1))
///   - proactive pick context (not `--profile` / not `--no-pick`)
///   - `pick_account` returned `Err(FetchFailed)` (hub unreachable) OR
///     `Err(NoUsableData)` (fetch ok but no profile had scorable usage —
///     "couldn't tell" must not silently keep current).
///     NOT when hook / `--profile` / `--no-pick` / non-interactive, and NOT for
///     `AllSaturated` (real limits read → warn + keep current).
fn cmd_run(args: &[OsString]) -> anyhow::Result<()> {
    use cli::parser::{parse, ResumeArg};
    use platform::relaunch::LaunchSpec;
    use session::alias::looks_like_uuid;

    let parsed = parse(args);
    let flags = &parsed.flags;

    // ── 1. Resolve the working directory ──────────────────────────────────────
    let cwd = std::env::current_dir().context("csm: cannot determine current directory")?;

    // ── 2. Resolve profile dir ─────────────────────────────────────────────────
    let profiles = account::ProfileMap::load().context("csm: failed to load profiles.json")?;
    let current_profile_name = derive_current_profile_name(&profiles);

    let profile_dir: PathBuf = if let Some(pin) = &flags.profile {
        // `--profile <p>` pin — explicit choice, skip all picking (wins over -i).
        let dir = resolve_profile_dir(pin, &profiles)?;
        PathBuf::from(dir)
    } else if flags.interactive {
        // `-i`/`--interactive` — manual pick: disable *all* auto-pick / skip.
        // Always open the account picker (recommendation-ordered, never the
        // silent auto-pick), regardless of whether the hub is up. Empty
        // ProfileMap (toss/first-boot) keeps current. The session picker is
        // also forced later by the same flag.
        match force_account_pick(&profiles)? {
            Some(dir) => dir,
            None => {
                eprintln!("csm: cancelled.");
                return Ok(());
            }
        }
    } else if flags.no_pick {
        // `--no-pick` — keep current profile without scoring.
        current_profile_dir(&profiles)
    } else {
        // Proactive pick (include_current=true — no-op switch if already best).
        // `None` = the hub-down picker was cancelled with Escape → abort.
        match proactive_pick_profile(&current_profile_name, &profiles, flags.pick_account)? {
            Some(dir) => dir,
            None => {
                eprintln!("csm: cancelled.");
                return Ok(());
            }
        }
    };

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
        // Default: 0 → fresh, 1 → auto-resume, 2+ → picker.
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

    let mut cli: Vec<OsString> = Vec::new();
    match &resolution {
        SessionResolution::Fresh(sid) => {
            cli.push(OsString::from("--session-id"));
            cli.push(OsString::from(sid));
        }
        SessionResolution::Resume(sid) => {
            cli.push(OsString::from("--resume"));
            cli.push(OsString::from(sid));
        }
    }

    // Append sidecar flags (mode/effort/model from previous session).
    // Explicit passthru flags override at the claude CLI level (last-wins).
    let sc_flags = existing_sidecar.sidecar_flags();
    cli.extend_from_slice(&sc_flags);

    // Append the passthru args (user's own flags + initial prompt).
    cli.extend_from_slice(&parsed.passthru);

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
    let newest_live_label: Option<&str> = None; // TODO: derive in Phase 9

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

// ─── profile resolution helpers ───────────────────────────────────────────────

/// Resolve a profile name → absolute `CLAUDE_CONFIG_DIR` path string.
///
/// Tries the ProfileMap first; synthesises a conventional path as fallback.
fn resolve_profile_dir(profile: &str, profiles: &account::ProfileMap) -> anyhow::Result<String> {
    if let Some(dir) = profiles.get(profile) {
        return Ok(dir.to_owned());
    }
    // Fallback: synthesise conventional `~/.claude.<profile>` path.
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("csm: cannot determine HOME directory"))?;
    Ok(home
        .join(format!(".claude.{profile}"))
        .to_string_lossy()
        .into_owned())
}

/// Return the current profile dir from `$CLAUDE_CONFIG_DIR`, or the default
/// resolved from the registry (`~/.config/claude-as/{profiles.json,default}`).
fn current_profile_dir(profiles: &account::ProfileMap) -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    profiles.default_dir()
}

/// Derive the current profile name from `$CLAUDE_CONFIG_DIR` + ProfileMap.
fn derive_current_profile_name(profiles: &account::ProfileMap) -> String {
    let dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    if dir.is_empty() {
        return profiles.default_name();
    }
    // Try to reverse-lookup the dir in the profiles map.
    if let Some((name, _)) = profiles.iter().find(|(_, d)| *d == dir.as_str()) {
        return name.to_owned();
    }
    // Derive from the directory basename (e.g. `.claude.home` → `personal`).
    std::path::Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_prefix(".claude.").unwrap_or(n).to_owned())
        .unwrap_or_else(|| profiles.default_name())
}

/// Proactive account pick with hub-down picker fallback (spec §4a).
///
/// Returns `Ok(Some(dir))` with the resolved profile directory, or `Ok(None)`
/// when the hub-down picker was cancelled (Escape / Ctrl-C) — the caller aborts.
///
/// Pick guard (mirrors zsh `claude-smart.zsh` lines 204-209, 316-323):
/// - `pick_account(current, include_current=true)` → scoring pick.
/// - `Err(FetchFailed)` (hub down) or `Err(NoUsableData)` (fetch ok but no
///   scorable usage) + interactive → hub-down account picker (§4a).
/// - same errors + non-interactive → silent fail-safe to current.
/// - `Err(AllSaturated)` → warn + keep current (no picker; real limits read).
fn proactive_pick_profile(
    current_profile: &str,
    profiles: &account::ProfileMap,
    _force_pick: bool,
) -> anyhow::Result<Option<PathBuf>> {
    use account::scoring::ScoringError;

    let current_dir = current_profile_dir(profiles);

    // No ProfileMap (toss / first-boot) — skip all picking.
    if profiles.is_empty() {
        return Ok(Some(current_dir));
    }

    match account::pick_account(current_profile, true) {
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
        // Hub unreachable OR fetch succeeded but carried no usable usage for any
        // profile. Both mean "we could not determine the best account" — never
        // silently keep current. Open the interactive picker (interactive) or
        // fail safe to current (non-interactive), same as a hub-down miss.
        Err(ScoringError::FetchFailed(_)) | Err(ScoringError::NoUsableData) => {
            hub_down_pick(profiles, &current_dir)
        }
    }
}

/// Hub-down account picker (spec §4a Decision #1).
///
/// Interactive + fetch-miss → open the account picker with stale usage data.
/// Non-interactive → silent fail-safe to current profile.
///
/// Returns `Ok(Some(dir))` to launch under `dir`, or `Ok(None)` when the user
/// pressed Escape / Ctrl-C in the picker (cancel the launch entirely).
fn hub_down_pick(
    profiles: &account::ProfileMap,
    current_dir: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    // TTY gate: isatty(0) && isatty(1) — matches zsh `[[ -t 0 && -t 1 ]]`.
    if !is_interactive() {
        return Ok(Some(current_dir.to_path_buf()));
    }
    run_account_picker(profiles, current_dir, "hub-down picker")
}

/// Forced account picker for `-i`/`--interactive` (manual pick).
///
/// Unlike [`hub_down_pick`], this is invoked even when the hub is up and a
/// confident auto-pick exists: `-i` means "let me choose", so we skip the
/// auto-pick entirely and always present the recommendation-ordered picker
/// (Enter still takes the recommendation). The TTY gate still applies — a piped
/// `-i` has no usable terminal for the picker, so it keeps the current profile.
/// An empty ProfileMap (toss / first-boot) likewise keeps current, nothing to pick.
fn force_account_pick(profiles: &account::ProfileMap) -> anyhow::Result<Option<PathBuf>> {
    let current_dir = current_profile_dir(profiles);
    if profiles.is_empty() || !is_interactive() {
        return Ok(Some(current_dir));
    }
    run_account_picker(profiles, &current_dir, "interactive picker")
}

/// Shared account-picker driver for [`hub_down_pick`] and
/// [`force_account_pick`]. Builds recommendation-ordered rows (stale usage if
/// that is all we have) and maps the picker outcome:
/// - Selected → that profile's dir.
/// - Cancelled (Escape / Ctrl-C) → `None` (caller aborts the launch).
/// - Unavailable (no usable terminal / no rows) → keep current profile.
fn run_account_picker(
    profiles: &account::ProfileMap,
    current_dir: &Path,
    ctx: &str,
) -> anyhow::Result<Option<PathBuf>> {
    use picker::engine::PickerOutcome;

    let rows = build_account_rows(profiles);
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
        PickerOutcome::Unavailable => Ok(Some(current_dir.to_path_buf())),
    }
}

/// Recommendation rank for a hub-down picker row, mirroring `scoring::pick_best`.
///
/// The picker renders top-to-bottom with the cursor on the FIRST row, so
/// pressing Enter selects it. We therefore order rows so the
/// recommended profile (the one `pick_best` would auto-select when the hub is up)
/// leads, and the user can just press Enter.
///
/// Returns a sort key where SMALLER sorts first:
/// - `0` bucket = viable candidate (no error, has week_all.pct, session.pct < LIMIT,
///   week_all.pct < SATURATION). Within it, HIGHER week_all.pct ranks first
///   (negated), then SOONER reset epoch, matching `pick_best`'s tie-break.
/// - `1` bucket = everything else (saturated, session-limited, errored, or no data),
///   ordered by name for stability.
///
/// `name` is the final tie-break so ordering is deterministic.
fn account_row_rank(
    name: &str,
    data: &picker::account::StaleProfileData,
) -> (u8, i64, i64, String) {
    use account::scoring::{ABSENT_SESSION_PCT, LIMIT_PCT, SATURATION_PCT};

    let session_pct = data.session_pct.unwrap_or(ABSENT_SESSION_PCT);
    let viable = data.error.is_none()
        && data.week_all_pct.is_some()
        && session_pct < LIMIT_PCT
        && data.week_all_pct.unwrap() < SATURATION_PCT;

    if !viable {
        // Non-viable rows sink to the bottom, ordered by name.
        return (1, 0, 0, name.to_owned());
    }

    let week_pct = data.week_all_pct.unwrap();
    // Higher week_all.pct first → negate so smaller sorts first.
    // Soonest reset epoch next → i64::MAX when unknown so known beats unknown.
    let epoch = data
        .resets
        .as_deref()
        .and_then(|r| account::reset::resets_to_epoch(r).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(i64::MAX);
    (0, -week_pct, epoch, name.to_owned())
}

/// Build `AccountRow` list for the hub-down picker, ordered by recommendation so
/// the top row is what `pick_best` would auto-select (Enter selects it).
fn build_account_rows(profiles: &account::ProfileMap) -> Vec<picker::account::AccountRow> {
    use picker::account::{AccountRow, StaleProfileData};

    // Try the smart-dir cache first.
    let cache_path = paths::usage_cache();
    let (cache_mtime, cache_json) = load_stale_cache(&cache_path);

    // Hub-local fallback: when running ON the configured hub, read its own cache.
    let (cache_mtime, cache_json) = if cache_json.is_none() && is_hub_local_machine() {
        load_stale_cache(&paths::hub_local_cache())
    } else {
        (cache_mtime, cache_json)
    };

    let (cache_profiles, cache_errors) = parse_cache_sections(&cache_json);

    // Union of configured profiles + any extra profiles from cache.
    let mut all_names: Vec<String> = profiles
        .names_sorted()
        .iter()
        .map(|s| s.to_string())
        .collect();
    for name in cache_profiles.keys().chain(cache_errors.keys()) {
        if !all_names.contains(name) {
            all_names.push(name.clone());
        }
    }

    // Build (name, StaleProfileData) so we can order by recommendation before
    // rendering rows. (HashMap iteration order is non-deterministic; the rank's
    // name tie-break makes the final order stable regardless.)
    let mut entries: Vec<(String, StaleProfileData)> = all_names
        .into_iter()
        .map(|profile| {
            let data = if let Some(err) = cache_errors.get(&profile) {
                StaleProfileData {
                    session_pct: None,
                    week_all_pct: None,
                    resets: None,
                    error: Some(err.clone()),
                }
            } else if let Some(pu) = cache_profiles.get(&profile) {
                StaleProfileData {
                    session_pct: pu.session_pct,
                    week_all_pct: pu.week_all_pct,
                    resets: pu.resets.clone(),
                    error: None,
                }
            } else {
                StaleProfileData {
                    session_pct: None,
                    week_all_pct: None,
                    resets: None,
                    error: None,
                }
            };
            (profile, data)
        })
        .collect();

    // Recommended-first ordering: the top row is what pick_best would auto-select,
    // so Enter (cursor starts on row 0) selects the recommendation.
    entries.sort_by_key(|(name, data)| account_row_rank(name, data));

    // The recommended row is the FIRST entry *iff* it is a viable candidate
    // (rank bucket 0). When every profile is saturated / errored / dataless,
    // pick_best would recommend nothing, so no row gets the ★.
    let recommended_idx = entries
        .first()
        .filter(|(name, data)| account_row_rank(name, data).0 == 0)
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

/// Per-profile parsed data from the usage cache JSON.
#[derive(Default)]
struct CacheProfileEntry {
    session_pct: Option<i64>,
    week_all_pct: Option<i64>,
    resets: Option<String>,
}

/// Load a usage cache JSON file; returns `(Option<mtime_secs>, Option<Value>)`.
fn load_stale_cache(path: &std::path::Path) -> (Option<u64>, Option<serde_json::Value>) {
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let json = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    (mtime, json)
}

/// Parse `profiles` and `errors` from the usage cache JSON.
///
/// Cache shape (spec §4a):
///   `profiles[<name>].session.pct`, `.week_all.pct`, `.week_all.resets`
///   `errors[<name>]` = error string
fn parse_cache_sections(
    json: &Option<serde_json::Value>,
) -> (
    std::collections::HashMap<String, CacheProfileEntry>,
    std::collections::HashMap<String, String>,
) {
    let mut profiles: std::collections::HashMap<String, CacheProfileEntry> =
        std::collections::HashMap::new();
    let mut errors: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let v = match json {
        Some(v) => v,
        None => return (profiles, errors),
    };

    if let Some(err_map) = v.get("errors").and_then(|e| e.as_object()) {
        for (name, msg) in err_map {
            if let Some(s) = msg.as_str() {
                errors.insert(name.clone(), s.to_owned());
            }
        }
    }

    if let Some(prof_map) = v.get("profiles").and_then(|p| p.as_object()) {
        for (name, pu) in prof_map {
            let session_pct = pu
                .get("session")
                .and_then(|s| s.as_object())
                .and_then(|s| s.get("pct"))
                .and_then(|p| p.as_i64());
            let week_all_pct = pu
                .get("week_all")
                .and_then(|w| w.as_object())
                .and_then(|w| w.get("pct"))
                .and_then(|p| p.as_i64());
            let resets = pu
                .get("week_all")
                .and_then(|w| w.as_object())
                .and_then(|w| w.get("resets"))
                .and_then(|r| r.as_str())
                .map(str::to_owned);

            profiles.insert(
                name.clone(),
                CacheProfileEntry {
                    session_pct,
                    week_all_pct,
                    resets,
                },
            );
        }
    }

    (profiles, errors)
}

/// `true` when this machine **is** the configured usage hub (hub-local
/// reconciliation: read the hub's own cache directly). Driven by the same
/// `CLAUDE_HUB_HOSTNAME` env contract as `usage::transport::is_hub_local`, so
/// no machine name is compiled into the binary. Always false when unset/empty.
fn is_hub_local_machine() -> bool {
    match usage::hub_hostname() {
        Some(hub) => statusline::hostname()
            .map(|h| h.eq_ignore_ascii_case(&hub))
            .unwrap_or(false),
        None => false,
    }
}

/// `true` when both stdin and stdout are terminals — mirrors zsh `[[ -t 0 && -t 1 ]]`.
fn is_interactive() -> bool {
    #[cfg(unix)]
    {
        // Use raw fd numbers (STDIN_FILENO=0, STDOUT_FILENO=1) — `nix::unistd::isatty`
        // takes a `RawFd` (i32), not an I/O handle.
        use nix::unistd::isatty;
        let stdin_ok = isatty(0).unwrap_or(false);
        let stdout_ok = isatty(1).unwrap_or(false);
        stdin_ok && stdout_ok
    }
    #[cfg(not(unix))]
    {
        // Windows: check whether the stdio handles are console handles via
        // `GetConsoleMode`. Best-effort; fall back to env-var heuristic.
        // TODO: use windows-sys GetConsoleMode for a proper check in Phase 9.
        std::env::var("WT_SESSION").is_ok() || std::env::var("TERM").is_ok()
    }
}

// ─── hook ──────────────────────────────────────────────────────────────────────

/// `csm hook [--owner <profile_dir>]`
///
/// Parses `--owner <dir>` and calls `hook::run`.  Defaults to `$CLAUDE_CONFIG_DIR`
/// when `--owner` is absent (non-interactive / missing shim).
fn cmd_hook(args: &[OsString]) -> anyhow::Result<()> {
    let owner_dir: PathBuf = parse_owner_flag(args)
        .or_else(|| {
            std::env::var("CLAUDE_CONFIG_DIR")
                .ok()
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            // Last resort (no --owner, no $CLAUDE_CONFIG_DIR): the registry default.
            account::ProfileMap::load()
                .unwrap_or_default()
                .default_dir()
        });

    hook::run(&owner_dir)
}

/// Parse `--owner <value>` or `--owner=<value>` from an arg slice.
fn parse_owner_flag(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();
        if s == "--owner" {
            if let Some(next) = iter.next() {
                return Some(PathBuf::from(next));
            }
        } else if let Some(val) = s.strip_prefix("--owner=") {
            return Some(PathBuf::from(val));
        }
    }
    None
}

// ─── cas ───────────────────────────────────────────────────────────────────────

/// `csm cas --eval --shell {zsh|pwsh} -- <op args...>`
///
/// Parses flags and the CAS operation, loads `profiles.json`, and calls
/// `cas::eval_emit` which emits the eval-able export line (or shell error
/// snippet) to stdout.
///
/// Called from the shell shim as:
///   `eval "$(command csm cas --eval --shell zsh -- "$@")"`
///   `Invoke-Expression (csm cas --eval --shell pwsh -- @args)`
fn cmd_cas(args: &[OsString]) -> anyhow::Result<()> {
    use cas::{Op, Shell};

    let parsed = parse_cas_flags(args)?;

    // `--print-default-dir`: print the resolved default CLAUDE_CONFIG_DIR and
    // return. Used by the shell/launchd floors as the single SSOT for dir
    // derivation (no `--shell`, no eval). Takes precedence over op parsing.
    if parsed.print_default_dir {
        let profiles = account::ProfileMap::load().unwrap_or_default();
        println!("{}", profiles.default_dir().to_string_lossy());
        return Ok(());
    }

    let eval_mode = parsed.eval_mode;
    let op = parse_cas_op(&parsed.op_args)?;

    // Registry-management ops are non-eval (the shim calls `csm cas <verb>`
    // directly). They mutate profiles.json / the default state file and print
    // human output — not an eval-able line.
    if matches!(
        op,
        Op::List
            | Op::Add { .. }
            | Op::Set { .. }
            | Op::Remove { .. }
            | Op::SetDefault { .. }
            | Op::Edit
    ) {
        if eval_mode {
            anyhow::bail!("csm cas: management verbs ({op:?}) must not be wrapped in --eval");
        }
        let mut profiles =
            account::ProfileMap::load().context("csm cas: failed to load profiles.json")?;
        return cas::manage_emit(&op, &mut profiles);
    }

    let shell = if eval_mode {
        let s = parsed
            .shell
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("csm cas: --eval requires --shell <zsh|pwsh>"))?;
        Shell::parse(s).ok_or_else(|| anyhow::anyhow!("csm cas: unknown --shell value {s:?}"))?
    } else {
        Shell::Zsh // informational status path
    };

    if !eval_mode {
        // Without --eval only Op::Status is allowed among the eval-class ops.
        if !matches!(op, Op::Status { .. }) {
            anyhow::bail!("csm cas: --eval flag is required for profile switching");
        }
    }

    let profiles = account::ProfileMap::load().context("csm cas: failed to load profiles.json")?;
    cas::eval_emit(shell, &op, &profiles)
}

/// Parsed `csm cas` flags.
#[derive(Debug, Default, PartialEq, Eq)]
struct CasFlags {
    eval_mode: bool,
    shell: Option<String>,
    op_args: Vec<String>,
    /// `--print-default-dir`: print the resolved default dir and exit (floor SSOT).
    print_default_dir: bool,
}

/// Parse `--eval`, `--shell`, `--print-default-dir`, and `--` sections from
/// `csm cas` arguments.
fn parse_cas_flags(args: &[OsString]) -> anyhow::Result<CasFlags> {
    let mut f = CasFlags::default();
    let mut past_double_dash = false;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if past_double_dash {
            f.op_args.push(arg.to_string_lossy().into_owned());
            continue;
        }
        let s = arg.to_string_lossy();
        if s == "--" {
            past_double_dash = true;
        } else if s == "--eval" {
            f.eval_mode = true;
        } else if s == "--print-default-dir" {
            f.print_default_dir = true;
        } else if s == "--shell" {
            if let Some(next) = iter.next() {
                f.shell = Some(next.to_string_lossy().into_owned());
            }
        } else if let Some(val) = s.strip_prefix("--shell=") {
            f.shell = Some(val.to_owned());
        } else {
            // Positional arg before `--`: treat as start of op args.
            f.op_args.push(s.into_owned());
            for remaining in iter.by_ref() {
                f.op_args.push(remaining.to_string_lossy().into_owned());
            }
            break;
        }
    }

    Ok(f)
}

/// Parse the CAS operation from the op-args slice.
fn parse_cas_op(op_args: &[String]) -> anyhow::Result<cas::Op> {
    use cas::Op;
    match op_args.first().map(String::as_str) {
        None | Some("status") => {
            let print_current = op_args
                .get(1)
                .map(|s| s == "--print-current")
                .unwrap_or(false);
            Ok(Op::Status { print_current })
        }
        Some("-") => Ok(Op::Minus),
        Some("resync") => Ok(Op::Resync),
        Some("-g") | Some("--global") => {
            let profile = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("csm cas: -g/--global requires a profile argument")
            })?;
            Ok(Op::Global { profile })
        }
        // ── registry management verbs (reserved words; routed to manage_emit) ──
        Some("list") => Ok(Op::List),
        Some("add") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("add: requires a profile name (`csm profiles add <name> [<dir>]`)")
            })?;
            Ok(Op::Add {
                name,
                dir: op_args.get(2).cloned(),
            })
        }
        Some("set") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("set: requires <name> <dir> (`csm profiles set <name> <dir>`)")
            })?;
            let dir = op_args.get(2).cloned().ok_or_else(|| {
                anyhow::anyhow!("set: requires <name> <dir> (`csm profiles set <name> <dir>`)")
            })?;
            Ok(Op::Set { name, dir })
        }
        Some("remove") | Some("rm") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("remove: requires a profile name (`csm profiles rm <name>`)")
            })?;
            Ok(Op::Remove { name })
        }
        Some("use") => {
            let name = op_args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("use: requires a profile name (`csm profiles use <name>`)")
            })?;
            Ok(Op::SetDefault { name })
        }
        Some("edit") => Ok(Op::Edit),
        Some(profile) => Ok(Op::Switch {
            profile: profile.to_owned(),
        }),
    }
}

// ─── profiles ────────────────────────────────────────────────────────────────

/// `csm profiles <verb> ...` — the human-facing registry noun.
///
/// A thin noun-verb front over the SAME `cas::Op` handlers the `cas` management
/// verbs use (no duplicate registry logic). Verbs:
///   list | add <name> [<dir>] | set <name> <dir> | rm|remove <name>
///   use <name> | edit | dir [<name>]
///
/// Bare `csm profiles` ≡ `csm profiles list`. `dir` is a profiles-only
/// convenience (prints a profile's config dir; default profile when omitted).
fn cmd_profiles(args: &[OsString]) -> anyhow::Result<()> {
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
                 (expected list|add|set|rm|use|edit|dir)"
            );
        }
    };

    let mut profiles =
        account::ProfileMap::load().context("csm profiles: failed to load profiles.json")?;
    cas::manage_emit(&op, &mut profiles)
}

// ─── usage ───────────────────────────────────────────────────────────────────

/// `csm usage [--json] [--no-fetch]`
///
/// Multi-profile usage table joining the registry with the hub's usage blob.
/// Offline-aware: serves the stale positive cache with an age header when the
/// hub is unreachable; prints a "disabled" message (registry still shown) when
/// metering env is unset. `--no-fetch` reads only the cache (never touches the
/// network) for fast scripted reads.
fn cmd_usage(args: &[OsString]) -> anyhow::Result<()> {
    use usage::report;

    let mut json = false;
    let mut no_fetch = false;
    for a in args {
        match a.to_string_lossy().as_ref() {
            "--json" => json = true,
            "--no-fetch" => no_fetch = true,
            "-h" | "--help" => {
                println!("usage: csm usage [--json] [--no-fetch]");
                println!("  --json      emit the joined registry∪hub view as JSON");
                println!("  --no-fetch  read only the local cache (no network)");
                return Ok(());
            }
            other => anyhow::bail!("csm usage: unknown flag '{other}' (try --json | --no-fetch)"),
        }
    }

    let profiles =
        account::ProfileMap::load().context("csm usage: failed to load profiles.json")?;
    let configured = usage::is_configured();

    // Resolve usage data + freshness. `--no-fetch` reads the cache directly;
    // otherwise fetch() runs the full resilience ladder (which itself prefers
    // a fresh cache before any network).
    let (data, stale_secs) = if !configured {
        (None, None)
    } else if no_fetch {
        let age = usage::cache_age_secs();
        (read_usage_cache(), age)
    } else {
        match usage::fetch() {
            Ok(d) => {
                // fetch() may have served a cached blob; surface its age so an
                // offline serve is labeled stale. Hub-local serves are fresh
                // (age ~0), so the stale header self-suppresses below 60s.
                (Some(d), usage::cache_age_secs())
            }
            Err(_) => {
                // Hub unreachable — degrade to the last-known cache, if any.
                (read_usage_cache(), usage::cache_age_secs())
            }
        }
    };

    let rpt = report::build_report(&profiles, data.as_ref(), configured, stale_secs);

    if json {
        println!("{}", report::render_json(&rpt)?);
    } else {
        print!("{}", report::render_table(&rpt));
    }
    Ok(())
}

/// Read the positive usage cache file directly (no network, no TTL gate). Used
/// by `--no-fetch` and the offline-degrade path. Returns `None` when absent or
/// unparseable.
fn read_usage_cache() -> Option<usage::UsageData> {
    let raw = std::fs::read_to_string(paths::usage_cache()).ok()?;
    serde_json::from_str(&raw).ok()
}

// ─── pick-account ──────────────────────────────────────────────────────────────

/// `csm pick-account [<current>] [--include-current]`
///
/// Prints the winner profile name to stdout, or nothing on no-op.
/// Exits 1 on fetch failure.
fn cmd_pick_account(args: &[OsString]) -> anyhow::Result<()> {
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
    // (empty stdout, a hint on stderr, rc 0) instead of attempting a hub fetch
    // that fails with a raw "hub returned empty payload". Mirrors the
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

// ─── scan ──────────────────────────────────────────────────────────────────────

/// `csm scan <cwd>`
///
/// Print TSV rows (newest-first) to stdout.
fn cmd_scan(args: &[OsString]) -> anyhow::Result<()> {
    let cwd = match args.first() {
        Some(a) => PathBuf::from(a),
        None => std::env::current_dir().context("csm scan: cannot determine cwd")?,
    };
    for row in session::scan(&cwd) {
        println!("{}", row.to_tsv());
    }
    Ok(())
}

// ─── current-usage ─────────────────────────────────────────────────────────────

/// `csm current-usage <profile>`
///
/// Print `<session_pct> <week_all_pct>` or nothing (errored/absent).
fn cmd_current_usage(args: &[OsString]) -> anyhow::Result<()> {
    let profile = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("csm current-usage: profile argument required"))?;
    if let Some((s, w)) = account::current_usage(&profile) {
        println!("{s} {w}");
    }
    Ok(())
}

// ─── sidecar ───────────────────────────────────────────────────────────────────

/// `csm sidecar {read|write|merge|flags} <sid> [key=value...]`
fn cmd_sidecar(args: &[OsString]) -> anyhow::Result<()> {
    use sidecar::{merge_sidecar, read_sidecar, write_sidecar};

    let op = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| {
            anyhow::anyhow!("csm sidecar: operation required (read|write|merge|flags)")
        })?;
    let sid = args
        .get(1)
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("csm sidecar: session id required"))?;
    let path = paths::sidecar(&sid);

    match op.as_str() {
        "read" => {
            let s = read_sidecar(&path)?;
            println!("{}", serde_json::to_string(&s)?);
        }
        "write" => {
            let patch = parse_sidecar_kv_args(&args[2..])?;
            write_sidecar(&path, &patch)?;
        }
        "merge" => {
            let patch = parse_sidecar_kv_args(&args[2..])?;
            merge_sidecar(&path, &patch)?;
        }
        "flags" => {
            let s = read_sidecar(&path)?;
            let flags = s.sidecar_flags();
            // Print each flag pair on its own line for shell consumption.
            let mut i = 0;
            while i < flags.len() {
                if i + 1 < flags.len() {
                    println!(
                        "{} {}",
                        flags[i].to_string_lossy(),
                        flags[i + 1].to_string_lossy()
                    );
                    i += 2;
                } else {
                    println!("{}", flags[i].to_string_lossy());
                    i += 1;
                }
            }
        }
        other => {
            anyhow::bail!("csm sidecar: unknown operation {other:?} — use read|write|merge|flags")
        }
    }
    Ok(())
}

/// Parse `key=value` args into a `Sidecar` patch for `write` / `merge`.
///
/// Recognised keys: `session_id`, `permission_mode`, `effort`, `model`,
/// `cwd`, `profile`, `hop`.
fn parse_sidecar_kv_args(args: &[OsString]) -> anyhow::Result<sidecar::Sidecar> {
    let mut patch = sidecar::Sidecar::default();
    for arg in args {
        let s = arg.to_string_lossy();
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("csm sidecar: expected key=value, got {s:?}"))?;
        match key {
            "session_id" | "sessionId" => patch.session_id = Some(value.to_owned()),
            "permission_mode" | "permissionMode" => patch.permission_mode = Some(value.to_owned()),
            "effort" => patch.effort = Some(value.to_owned()),
            "model" => patch.model = Some(value.to_owned()),
            "cwd" => patch.cwd = Some(value.to_owned()),
            "profile" => patch.profile = Some(value.to_owned()),
            "hop" => {
                let n: i64 = value.parse().with_context(|| {
                    format!("csm sidecar: hop must be an integer, got {value:?}")
                })?;
                // Store as a JSON Number (the canonical Rust-binary form; §6 compat).
                patch.hop = Some(serde_json::Value::Number(serde_json::Number::from(n)));
            }
            other => anyhow::bail!("csm sidecar: unknown key {other:?}"),
        }
    }
    Ok(patch)
}

// ─── statusline ────────────────────────────────────────────────────────────────

fn cmd_statusline(args: &[OsString]) -> anyhow::Result<()> {
    statusline::run(args)
}

// ─── completions ───────────────────────────────────────────────────────────────

/// `csm completions {zsh|bash|pwsh}`
fn cmd_completions(args: &[OsString]) -> anyhow::Result<()> {
    use clap_complete::Shell;

    let shell_str = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Accept `pwsh` as an alias for clap's `powershell` token — both csm's
    // `--help` and the shim contract speak of `pwsh`, so the completions verb
    // must too. (`clap_complete::Shell::from_str` only knows `powershell`.)
    let normalized = if shell_str.eq_ignore_ascii_case("pwsh") {
        "powershell".to_owned()
    } else {
        shell_str.clone()
    };

    let shell: Shell = normalized.parse().map_err(|_| {
        anyhow::anyhow!(
            "csm completions: unknown shell {shell_str:?} — use zsh, bash, pwsh, or powershell"
        )
    })?;

    cli::completions::generate(shell, &mut std::io::stdout());
    Ok(())
}

// ─── PlatformLauncher Default impl ────────────────────────────────────────────

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parser;

    // ══════════════════════════════════════════════════════════════════════════
    // Dispatch routing — verify that the argument dispatcher picks the right
    // subcommand word, covering the full table in main().
    // Pure-logic tests: no subprocess / real I/O / network calls.
    // ══════════════════════════════════════════════════════════════════════════

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
    }

    /// Mirror the dispatch logic in `main()`: given a full argv (including argv[0]),
    /// return `(subcommand, rest_len)` without executing the handler.
    fn dispatch_subcommand(args: &[OsString]) -> (&'static str, usize) {
        let argv0 = args
            .first()
            .and_then(|a| {
                std::path::Path::new(a)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_ascii_lowercase)
            })
            .unwrap_or_default();

        if argv0 == "csm-hook" {
            return ("hook", args.len() - 1);
        }
        if args.len() >= 2 {
            let candidate = args[1].to_string_lossy();
            match candidate.as_ref() {
                "run" | "hook" | "profiles" | "usage" | "cas" | "pick-account" | "scan"
                | "current-usage" | "sidecar" | "statusline" | "completions" | "newuuid" => {
                    return (
                        Box::leak(candidate.into_owned().into_boxed_str()),
                        args.len() - 2,
                    );
                }
                _ => {}
            }
        }
        ("run", args.len() - 1)
    }

    // ── explicit subcommands ──────────────────────────────────────────────────

    #[test]
    fn dispatch_explicit_hook() {
        let a = argv(&["csm", "hook", "--owner", "/tmp/.claude.home"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "hook");
        assert_eq!(rest_len, 2);
    }

    #[test]
    fn dispatch_explicit_cas() {
        let a = argv(&["csm", "cas", "--eval", "--shell", "zsh", "--", "home"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "cas");
        assert_eq!(rest_len, 5);
    }

    #[test]
    fn dispatch_explicit_pick_account() {
        let a = argv(&["csm", "pick-account", "home", "--include-current"]);
        let (cmd, _) = dispatch_subcommand(&a);
        assert_eq!(cmd, "pick-account");
    }

    #[test]
    fn dispatch_explicit_profiles() {
        let a = argv(&["csm", "profiles", "list"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "profiles");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_usage() {
        let a = argv(&["csm", "usage", "--json"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "usage");
        assert_eq!(rest_len, 1);
    }

    /// A word that is NOT a reserved csm subcommand falls through to `run`
    /// (→ forwarded to claude). This is the collision-avoidance contract: any
    /// claude subcommand (mcp/doctor/update/…) is forwarded, never hijacked.
    #[test]
    fn dispatch_claude_subcommands_fall_through_to_run() {
        for w in [
            "mcp", "doctor", "update", "agents", "auth", "plugin", "project",
        ] {
            let a = argv(&["csm", w, "--some-flag"]);
            let (cmd, _) = dispatch_subcommand(&a);
            assert_eq!(
                cmd, "run",
                "`csm {w}` must fall through to run (forward to claude)"
            );
        }
    }

    #[test]
    fn dispatch_explicit_scan() {
        let a = argv(&["csm", "scan", "/tmp/project"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "scan");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_current_usage() {
        let a = argv(&["csm", "current-usage", "home"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "current-usage");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_sidecar() {
        let a = argv(&["csm", "sidecar", "read", "abc-sid"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "sidecar");
        assert_eq!(rest_len, 2);
    }

    #[test]
    fn dispatch_explicit_statusline() {
        let a = argv(&["csm", "statusline"]);
        let (cmd, _) = dispatch_subcommand(&a);
        assert_eq!(cmd, "statusline");
    }

    #[test]
    fn dispatch_explicit_completions() {
        let a = argv(&["csm", "completions", "zsh"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "completions");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_newuuid() {
        let a = argv(&["csm", "newuuid"]);
        let (cmd, _) = dispatch_subcommand(&a);
        assert_eq!(cmd, "newuuid");
    }

    // ── implicit `run` fallthrough ────────────────────────────────────────────

    #[test]
    fn dispatch_bare_csm_is_run() {
        let a = argv(&["csm"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "run");
        assert_eq!(rest_len, 0);
    }

    #[test]
    fn dispatch_csm_flag_only_is_run() {
        let a = argv(&["csm", "-c"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "run");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_unknown_subcommand_falls_through_to_run() {
        let a = argv(&["csm", "unknowncmd"]);
        let (cmd, _) = dispatch_subcommand(&a);
        assert_eq!(cmd, "run");
    }

    #[test]
    fn dispatch_explicit_run_subcommand() {
        let a = argv(&["csm", "run", "-c", "--profile=personal"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "run");
        assert_eq!(rest_len, 2);
    }

    // ── argv[0]-aware hook dispatch ───────────────────────────────────────────

    #[test]
    fn dispatch_argv0_csm_hook_routes_to_hook() {
        let a = argv(&["csm-hook", "--owner", "/tmp/dir"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "hook");
        assert_eq!(rest_len, 2);
    }

    #[test]
    fn dispatch_argv0_csm_hook_no_args() {
        let a = argv(&["csm-hook"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "hook");
        assert_eq!(rest_len, 0);
    }

    // ── parser integration: parser output feeds dispatch correctly ────────────

    #[test]
    fn parser_run_flags() {
        let a = argv(&["csm", "run", "-c", "--profile=work"]);
        let rest = &a[2..];
        let parsed = parser::parse(rest);
        assert!(parsed.flags.continue_);
        assert_eq!(parsed.flags.profile.as_deref(), Some("work"));
        assert!(parsed.passthru.is_empty());
    }

    #[test]
    fn parser_run_passthru() {
        let a = argv(&["csm", "run", "--", "--dangerously-skip-permissions"]);
        let rest = &a[2..];
        let parsed = parser::parse(rest);
        assert!(!parsed.flags.continue_);
        assert_eq!(
            parsed.passthru,
            vec![OsString::from("--dangerously-skip-permissions")]
        );
    }

    // ── parse_owner_flag ──────────────────────────────────────────────────────

    #[test]
    fn parse_owner_flag_space_form() {
        let args = argv(&["--owner", "/Users/example/.claude.home"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.home")));
    }

    #[test]
    fn parse_owner_flag_equals_form() {
        let args = argv(&["--owner=/Users/example/.claude.home"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.home")));
    }

    #[test]
    fn parse_owner_flag_absent_returns_none() {
        let args = argv(&["--other", "value"]);
        assert!(parse_owner_flag(&args).is_none());
    }

    #[test]
    fn parse_owner_flag_empty_slice() {
        assert!(parse_owner_flag(&[]).is_none());
    }

    // ── parse_cas_flags ───────────────────────────────────────────────────────

    #[test]
    fn parse_cas_flags_eval_shell_double_dash() {
        let args = argv(&["--eval", "--shell", "zsh", "--", "home"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.eval_mode);
        assert_eq!(f.shell.as_deref(), Some("zsh"));
        assert_eq!(f.op_args, vec!["home"]);
        assert!(!f.print_default_dir);
    }

    #[test]
    fn parse_cas_flags_equals_form_shell() {
        let args = argv(&["--eval", "--shell=pwsh", "--", "work"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.eval_mode);
        assert_eq!(f.shell.as_deref(), Some("pwsh"));
        assert_eq!(f.op_args, vec!["work"]);
    }

    #[test]
    fn parse_cas_flags_no_eval_mode() {
        let args = argv(&["status"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(!f.eval_mode);
        assert_eq!(f.op_args, vec!["status"]);
    }

    #[test]
    fn parse_cas_flags_global_op() {
        let args = argv(&["--eval", "--shell", "zsh", "--", "-g", "home"]);
        let f = parse_cas_flags(&args).unwrap();
        assert_eq!(f.op_args, vec!["-g", "home"]);
    }

    #[test]
    fn parse_cas_flags_print_default_dir() {
        let args = argv(&["--print-default-dir"]);
        let f = parse_cas_flags(&args).unwrap();
        assert!(f.print_default_dir);
        assert!(!f.eval_mode);
        assert!(f.op_args.is_empty());
    }

    // ── parse_cas_op ──────────────────────────────────────────────────────────

    #[test]
    fn parse_cas_op_switch() {
        let op = parse_cas_op(&["home".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Switch { profile } if profile == "home"));
    }

    #[test]
    fn parse_cas_op_minus() {
        let op = parse_cas_op(&["-".to_owned()]).unwrap();
        assert_eq!(op, cas::Op::Minus);
    }

    #[test]
    fn parse_cas_op_global() {
        let op = parse_cas_op(&["-g".to_owned(), "work".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Global { profile } if profile == "work"));
    }

    #[test]
    fn parse_cas_op_resync() {
        let op = parse_cas_op(&["resync".to_owned()]).unwrap();
        assert_eq!(op, cas::Op::Resync);
    }

    #[test]
    fn parse_cas_op_status_no_args() {
        let op = parse_cas_op(&[]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: false
            }
        ));
    }

    #[test]
    fn parse_cas_op_status_explicit() {
        let op = parse_cas_op(&["status".to_owned()]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: false
            }
        ));
    }

    #[test]
    fn parse_cas_op_status_print_current() {
        let op = parse_cas_op(&["status".to_owned(), "--print-current".to_owned()]).unwrap();
        assert!(matches!(
            op,
            cas::Op::Status {
                print_current: true
            }
        ));
    }

    #[test]
    fn parse_cas_op_global_long_form() {
        let op = parse_cas_op(&["--global".to_owned(), "home".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Global { profile } if profile == "home"));
    }

    // ── parse_cas_op: registry management verbs ───────────────────────────────

    #[test]
    fn parse_cas_op_list() {
        assert!(matches!(
            parse_cas_op(&["list".to_owned()]).unwrap(),
            cas::Op::List
        ));
    }

    #[test]
    fn parse_cas_op_add_with_and_without_dir() {
        let op = parse_cas_op(&["add".to_owned(), "work".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Add { ref name, dir: None } if name == "work"));
        let op = parse_cas_op(&["add".to_owned(), "work".to_owned(), "/d".to_owned()]).unwrap();
        assert!(
            matches!(op, cas::Op::Add { ref name, dir: Some(ref d) } if name == "work" && d == "/d")
        );
        // missing name → err
        assert!(parse_cas_op(&["add".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_set_requires_name_and_dir() {
        let op = parse_cas_op(&["set".to_owned(), "w".to_owned(), "/d".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Set { ref name, ref dir } if name == "w" && dir == "/d"));
        assert!(parse_cas_op(&["set".to_owned(), "w".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_remove_and_rm_alias() {
        let op = parse_cas_op(&["remove".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Remove { ref name } if name == "w"));
        let op = parse_cas_op(&["rm".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Remove { ref name } if name == "w"));
        assert!(parse_cas_op(&["remove".to_owned()]).is_err());
    }

    #[test]
    fn parse_cas_op_use_sets_default() {
        let op = parse_cas_op(&["use".to_owned(), "w".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::SetDefault { ref name } if name == "w"));
        assert!(parse_cas_op(&["use".to_owned()]).is_err());
    }

    // ── parse_sidecar_kv_args ─────────────────────────────────────────────────

    #[test]
    fn parse_sidecar_kv_permission_mode() {
        let args = argv(&["permission_mode=bypassPermissions"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.permission_mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn parse_sidecar_kv_effort() {
        let args = argv(&["effort=max"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.effort.as_deref(), Some("max"));
    }

    #[test]
    fn parse_sidecar_kv_hop() {
        let args = argv(&["hop=1"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.hop_int(), 1);
    }

    #[test]
    fn parse_sidecar_kv_hop_invalid() {
        let args = argv(&["hop=notanumber"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }

    #[test]
    fn parse_sidecar_kv_unknown_key_errors() {
        let args = argv(&["unknownkey=value"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }

    #[test]
    fn parse_sidecar_kv_no_equals_errors() {
        let args = argv(&["permission_mode"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }

    // ── parse_cache_sections ──────────────────────────────────────────────────

    #[test]
    fn parse_cache_sections_full_payload() {
        let json: serde_json::Value = serde_json::json!({
            "profiles": {
                "home": {
                    "session": { "pct": 3 },
                    "week_all": { "pct": 32, "resets": "Jun 18 at 9pm (Asia/Seoul)" }
                },
                "work": {
                    "session": null,
                    "week_all": { "pct": 80, "resets": null }
                }
            },
            "errors": {
                "broken": "no credentials"
            }
        });
        let (profiles, errors) = parse_cache_sections(&Some(json));
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles["home"].session_pct, Some(3));
        assert_eq!(profiles["home"].week_all_pct, Some(32));
        assert_eq!(
            profiles["home"].resets.as_deref(),
            Some("Jun 18 at 9pm (Asia/Seoul)")
        );
        assert!(profiles["work"].session_pct.is_none());
        assert_eq!(profiles["work"].week_all_pct, Some(80));
        assert_eq!(errors["broken"], "no credentials");
    }

    #[test]
    fn parse_cache_sections_absent_errors_key() {
        let json: serde_json::Value = serde_json::json!({
            "profiles": {
                "home": {
                    "week_all": { "pct": 50 }
                }
            }
        });
        let (profiles, errors) = parse_cache_sections(&Some(json));
        assert_eq!(profiles.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_cache_sections_none_input() {
        let (profiles, errors) = parse_cache_sections(&None);
        assert!(profiles.is_empty());
        assert!(errors.is_empty());
    }

    // ── newuuid ───────────────────────────────────────────────────────────────

    #[test]
    fn newuuid_produces_lowercase_uuid() {
        let id = newuuid();
        assert_eq!(id.len(), 36, "UUID must be 36 chars");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        assert_eq!(id, id.to_lowercase(), "UUID must be lowercase");
    }

    #[test]
    fn newuuid_unique_each_call() {
        let a = newuuid();
        let b = newuuid();
        assert_ne!(a, b, "consecutive UUIDs must differ");
    }

    // ── is_interactive (smoke test — cannot assert value in non-tty env) ───────

    #[test]
    fn is_interactive_does_not_panic() {
        let _ = is_interactive();
    }

    // ── session-verb selection (regression: "Session ID … is already in use") ──
    // Mirror the cold-launch verb choice in main(): Fresh → `--session-id`,
    // Resume → `--resume`. The bug was that an existing (resumed) session id was
    // launched with `--session-id`, which claude rejects as "already in use".

    /// The leading two CLI tokens (verb + id) main() builds for a resolution.
    fn launch_verb_and_id(res: &SessionResolution) -> (OsString, OsString) {
        let mut cli: Vec<OsString> = Vec::new();
        match res {
            SessionResolution::Fresh(sid) => {
                cli.push(OsString::from("--session-id"));
                cli.push(OsString::from(sid));
            }
            SessionResolution::Resume(sid) => {
                cli.push(OsString::from("--resume"));
                cli.push(OsString::from(sid));
            }
        }
        (cli[0].clone(), cli[1].clone())
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

    // ── account_row_rank (hub-down picker: recommended profile leads) ─────────
    // The picker starts the cursor on row 0, so the top row is what Enter
    // selects. account_row_rank must order rows the same way pick_best chooses,
    // so the recommendation leads and a bare Enter picks it.

    use picker::account::StaleProfileData;

    fn data(session: Option<i64>, week: Option<i64>, resets: Option<&str>) -> StaleProfileData {
        StaleProfileData {
            session_pct: session,
            week_all_pct: week,
            resets: resets.map(|s| s.to_owned()),
            error: None,
        }
    }

    /// Sort names by rank and return them in display order (row 0 first).
    fn ranked_order(mut rows: Vec<(&str, StaleProfileData)>) -> Vec<String> {
        rows.sort_by_key(|(name, data)| account_row_rank(name, data));
        rows.into_iter().map(|(n, _)| n.to_owned()).collect()
    }

    #[test]
    fn viable_higher_week_pct_leads() {
        // Both viable; the higher week_all.pct is the recommendation (pick_best
        // picks the most-used non-saturated account). It must be row 0.
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
            error: Some("no credentials".to_owned()),
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
    fn equal_week_pct_known_reset_beats_unknown() {
        // Same week_all.pct → a known reset epoch beats an unknown (None) one,
        // mirroring pick_best's "known beats unknown" tie-break. This is
        // date-independent (no reliance on what "today" is, unlike comparing two
        // bare month/day strings whose inferred year flips around today).
        let order = ranked_order(vec![
            ("noreset", data(Some(3), Some(30), None)),
            (
                "hasreset",
                data(Some(3), Some(30), Some("Jun 18 at 9pm (Asia/Seoul)")),
            ),
        ]);
        assert_eq!(order, vec!["hasreset", "noreset"]);
    }
}
