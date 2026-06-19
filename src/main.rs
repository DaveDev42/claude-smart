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
                println!("csm {} — claude-smart launcher\n", env!("CARGO_PKG_VERSION"));
                println!("usage: csm <run|hook|cas|pick-account|scan|current-usage|sidecar|statusline|completions|newuuid>");
                println!("       csm [claude-flags...]        (bare = implicit `csm run`)");
                println!("       csm run [csm-flags] [-- claude-passthru...]");
                return Ok(());
            }
            _ => {}
        }
        match candidate.as_ref() {
            "run"
            | "hook"
            | "cas"
            | "pick-account"
            | "scan"
            | "current-usage"
            | "sidecar"
            | "statusline"
            | "completions"
            | "newuuid" => {
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
            eprintln!("usage: csm <run|hook|cas|pick-account|scan|current-usage|sidecar|statusline|completions|newuuid>");
            std::process::exit(1);
        }
    }
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
/// Hub-down picker gate (spec §4a):
///   Open the interactive account picker ONLY when ALL of:
///   - interactive (isatty(0) && isatty(1))
///   - proactive pick context (not `--profile` / not `--no-pick`)
///   - `pick_account` returned `Err(FetchFailed)`
///     NOT when hook / `--profile` / `--no-pick` / non-interactive.
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
        // `--profile <p>` pin — skip all picking.
        let dir = resolve_profile_dir(pin, &profiles)?;
        PathBuf::from(dir)
    } else if flags.no_pick {
        // `--no-pick` — keep current profile without scoring.
        current_profile_dir()
    } else {
        // Proactive pick (include_current=true — no-op switch if already best).
        proactive_pick_profile(&current_profile_name, &profiles, flags.pick_account)?
    };

    // ── 3. Resolve session id ──────────────────────────────────────────────────
    let session_id: String = if let Some(explicit_sid) = &flags.session_id {
        explicit_sid.clone()
    } else if let Some(resume_arg) = &flags.resume {
        match resume_arg {
            ResumeArg::Id(raw) => {
                // Resolve alias if not UUID-shaped.
                if looks_like_uuid(raw) {
                    raw.clone()
                } else {
                    session::resolve_alias(raw)
                        .with_context(|| format!("csm: --resume alias resolution failed for {raw:?}"))?
                }
            }
            ResumeArg::Picker => resolve_session_via_picker(&cwd)?,
        }
    } else if flags.new {
        // `-n`/`--new`: explicit fresh session.
        newuuid()
    } else if flags.interactive {
        // `-i`/`--interactive`: open session picker.
        resolve_session_via_picker(&cwd)?
    } else if flags.continue_ {
        // `-c`/`--continue`: newest free session or fresh.
        newest_free_sid(&cwd)?.unwrap_or_else(newuuid)
    } else {
        // Default: 0 → fresh, 1 → auto-resume, 2+ → picker.
        resolve_session_default(&cwd)?
    };

    // ── 4. Build the claude CLI and launch ──────────────────────────────────────
    // Always pass `--session-id <sid>`.
    // Restore previous mode/effort/model via sidecar flags (perfect-continue).
    let sidecar_path = paths::sidecar(&session_id);
    let existing_sidecar = sidecar::read_sidecar(&sidecar_path).unwrap_or_default();

    let mut cli: Vec<OsString> = Vec::new();
    cli.push(OsString::from("--session-id"));
    cli.push(OsString::from(&session_id));

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

/// Open the interactive fzf session picker. Falls back to fresh UUID when
/// fzf is unavailable, no rows exist, or the user escapes.
fn resolve_session_via_picker(cwd: &std::path::Path) -> anyhow::Result<String> {
    use picker::session::SessionRow as PickerRow;
    use picker::session::{PickedSession, SessionPicker};

    let rows = session::scan(cwd);

    // Convert `session::SessionRow` → `picker::session::SessionRow` (picker
    // wants an `is_live` field; `session` module doesn't carry that).
    let picker_rows: Vec<PickerRow> = rows
        .iter()
        .map(|r| PickerRow {
            sid:      r.sid.clone(),
            mtime:    r.mtime as u64,
            human_ts: r.human_ts.clone(),
            mode:     r.mode.clone(),
            label:    r.label.clone(),
            is_live:  session::sid_live(&r.sid),
        })
        .collect();

    let sp = SessionPicker::new(picker_rows);
    let newest_live_label: Option<&str> = None; // TODO: derive in Phase 9

    match sp.pick(newest_live_label) {
        None | Some(PickedSession::Fresh) => Ok(newuuid()),
        Some(PickedSession::Continue) => {
            Ok(newest_free_sid(cwd)?.unwrap_or_else(newuuid))
        }
        Some(PickedSession::Resume(sid)) => Ok(sid),
    }
}

/// Default session resolution (no explicit flags):
///   0 free sessions → fresh UUID
///   1 free session  → auto-resume that session (0-based free-session sort)
///   2+ free sessions → open picker
fn resolve_session_default(cwd: &std::path::Path) -> anyhow::Result<String> {
    let rows = session::scan(cwd);
    let free: Vec<&session::SessionRow> = rows
        .iter()
        .filter(|r| !session::sid_live(&r.sid))
        .collect();

    match free.len() {
        0 => Ok(newuuid()),
        1 => Ok(free[0].sid.clone()),
        _ => resolve_session_via_picker(cwd),
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

// ─── profile resolution helpers ───────────────────────────────────────────────

/// Resolve a profile name → absolute `CLAUDE_CONFIG_DIR` path string.
///
/// Tries the ProfileMap first; synthesises a conventional path as fallback.
fn resolve_profile_dir(profile: &str, profiles: &account::ProfileMap) -> anyhow::Result<String> {
    if let Some(dir) = profiles.get(profile) {
        return Ok(dir.to_owned());
    }
    // Fallback: synthesise conventional `~/.claude.<profile>` path.
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("csm: cannot determine HOME directory"))?;
    Ok(home
        .join(format!(".claude.{profile}"))
        .to_string_lossy()
        .into_owned())
}

/// Return the current profile dir from `$CLAUDE_CONFIG_DIR`, or the default
/// derived from `~/.config/claude-as/default`.
fn current_profile_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let default = cas::default_profile();
    dirs::home_dir()
        .map(|h| h.join(format!(".claude.{default}")))
        .unwrap_or_else(|| PathBuf::from(format!(".claude.{default}")))
}

/// Derive the current profile name from `$CLAUDE_CONFIG_DIR` + ProfileMap.
fn derive_current_profile_name(profiles: &account::ProfileMap) -> String {
    let dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    if dir.is_empty() {
        return cas::default_profile();
    }
    // Try to reverse-lookup the dir in the profiles map.
    if let Some((name, _)) = profiles.iter().find(|(_, d)| *d == dir.as_str()) {
        return name.to_owned();
    }
    // Derive from the directory basename (e.g. `.claude.personal` → `personal`).
    std::path::Path::new(&dir)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_prefix(".claude.").unwrap_or(n).to_owned())
        .unwrap_or_else(cas::default_profile)
}

/// Proactive account pick with hub-down picker fallback (spec §4a).
///
/// Returns the resolved profile directory.
///
/// Pick guard (mirrors zsh `claude-smart.zsh` lines 204-209, 316-323):
/// - `pick_account(current, include_current=true)` → scoring pick.
/// - `Err(FetchFailed)` + interactive → hub-down fzf account picker (§4a).
/// - `Err(FetchFailed)` + non-interactive → silent fail-safe to current.
/// - `Err(AllSaturated)` → warn + keep current (no picker; spec §4a).
fn proactive_pick_profile(
    current_profile: &str,
    profiles: &account::ProfileMap,
    _force_pick: bool,
) -> anyhow::Result<PathBuf> {
    use account::scoring::ScoringError;

    let current_dir = current_profile_dir();

    // No ProfileMap (toss / first-boot) — skip all picking.
    if profiles.is_empty() {
        return Ok(current_dir);
    }

    match account::pick_account(current_profile, true) {
        Ok(None) => {
            // Already on the best profile — keep current.
            Ok(current_dir)
        }
        Ok(Some(winner)) => {
            let dir = resolve_profile_dir(&winner, profiles)
                .context("csm: proactive pick — winner profile not in map")?;
            if winner != current_profile {
                eprintln!("csm: auto-pick → {winner}");
            }
            Ok(PathBuf::from(dir))
        }
        Err(ScoringError::AllSaturated) => {
            eprintln!(
                "csm: warning: all accounts at session/week limit — keeping current profile ({current_profile})"
            );
            Ok(current_dir)
        }
        Err(ScoringError::FetchFailed(_)) => hub_down_pick(profiles, &current_dir),
    }
}

/// Hub-down account picker (spec §4a Decision #1).
///
/// Interactive + fetch-miss → open fzf account picker with stale usage data.
/// Non-interactive → silent fail-safe to current profile.
fn hub_down_pick(
    profiles: &account::ProfileMap,
    current_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // TTY gate: isatty(0) && isatty(1) — matches zsh `[[ -t 0 && -t 1 ]]`.
    if !is_interactive() {
        return Ok(current_dir.to_path_buf());
    }

    let rows = build_account_rows(profiles);
    let ap = picker::AccountPicker::new(rows);

    match ap.pick() {
        Some(winner) => {
            let dir = resolve_profile_dir(&winner, profiles)
                .context("csm: hub-down picker — selected profile not in map")?;
            Ok(PathBuf::from(dir))
        }
        None => Ok(current_dir.to_path_buf()),
    }
}

/// Build `AccountRow` list for the hub-down picker.
fn build_account_rows(
    profiles: &account::ProfileMap,
) -> Vec<picker::account::AccountRow> {
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
    all_names.sort_unstable();

    all_names
        .iter()
        .map(|profile| {
            let data = if let Some(err) = cache_errors.get(profile) {
                StaleProfileData {
                    session_pct: None,
                    week_all_pct: None,
                    resets: None,
                    error: Some(err.clone()),
                }
            } else if let Some(pu) = cache_profiles.get(profile) {
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
            AccountRow::build(profile, &data, cache_mtime)
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
fn load_stale_cache(
    path: &std::path::Path,
) -> (Option<u64>, Option<serde_json::Value>) {
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
    let mut errors: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

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
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            let default = cas::default_profile();
            dirs::home_dir()
                .map(|h| h.join(format!(".claude.{default}")))
                .unwrap_or_else(|| PathBuf::from(format!(".claude.{default}")))
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

    let (eval_mode, shell_opt, op_args) = parse_cas_flags(args)?;

    let shell = if eval_mode {
        let s = shell_opt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("csm cas: --eval requires --shell <zsh|pwsh>"))?;
        Shell::parse(s)
            .ok_or_else(|| anyhow::anyhow!("csm cas: unknown --shell value {s:?}"))?
    } else {
        Shell::Zsh // informational status path
    };

    let op = parse_cas_op(&op_args)?;

    if !eval_mode {
        // Without --eval only Op::Status is allowed (informational).
        if !matches!(op, Op::Status { .. }) {
            anyhow::bail!("csm cas: --eval flag is required for profile switching");
        }
    }

    let profiles = account::ProfileMap::load().context("csm cas: failed to load profiles.json")?;
    cas::eval_emit(shell, &op, &profiles)
}

/// Parse `--eval`, `--shell`, and `--` sections from `csm cas` arguments.
///
/// Returns `(eval_mode, Option<shell_str>, op_args_after_double_dash)`.
fn parse_cas_flags(args: &[OsString]) -> anyhow::Result<(bool, Option<String>, Vec<String>)> {
    let mut eval_mode = false;
    let mut shell: Option<String> = None;
    let mut op_args: Vec<String> = Vec::new();
    let mut past_double_dash = false;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if past_double_dash {
            op_args.push(arg.to_string_lossy().into_owned());
            continue;
        }
        let s = arg.to_string_lossy();
        if s == "--" {
            past_double_dash = true;
        } else if s == "--eval" {
            eval_mode = true;
        } else if s == "--shell" {
            if let Some(next) = iter.next() {
                shell = Some(next.to_string_lossy().into_owned());
            }
        } else if let Some(val) = s.strip_prefix("--shell=") {
            shell = Some(val.to_owned());
        } else {
            // Positional arg before `--`: treat as start of op args.
            op_args.push(s.into_owned());
            for remaining in iter.by_ref() {
                op_args.push(remaining.to_string_lossy().into_owned());
            }
            break;
        }
    }

    Ok((eval_mode, shell, op_args))
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
            let profile = op_args
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("csm cas: -g/--global requires a profile argument"))?;
            Ok(Op::Global { profile })
        }
        Some(profile) => Ok(Op::Switch { profile: profile.to_owned() }),
    }
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

    match account::pick_account(&current, include_current) {
        Ok(Some(winner)) => println!("{winner}"),
        Ok(None) => {}
        Err(account::scoring::ScoringError::AllSaturated) => {
            eprintln!("csm pick-account: all accounts saturated");
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
        .ok_or_else(|| anyhow::anyhow!("csm sidecar: operation required (read|write|merge|flags)"))?;
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
        other => anyhow::bail!(
            "csm sidecar: unknown operation {other:?} — use read|write|merge|flags"
        ),
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
            "permission_mode" | "permissionMode" => {
                patch.permission_mode = Some(value.to_owned())
            }
            "effort" => patch.effort = Some(value.to_owned()),
            "model" => patch.model = Some(value.to_owned()),
            "cwd" => patch.cwd = Some(value.to_owned()),
            "profile" => patch.profile = Some(value.to_owned()),
            "hop" => {
                let n: i64 = value
                    .parse()
                    .with_context(|| format!("csm sidecar: hop must be an integer, got {value:?}"))?;
                // Store as a JSON Number (the canonical Rust-binary form; §6 compat).
                patch.hop = Some(serde_json::Value::Number(
                    serde_json::Number::from(n),
                ));
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

    let shell: Shell = shell_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "csm completions: unknown shell {shell_str:?} — use zsh, bash, or powershell"
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
                "run" | "hook" | "cas" | "pick-account" | "scan" | "current-usage"
                | "sidecar" | "statusline" | "completions" | "newuuid" => {
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
        let a = argv(&["csm", "hook", "--owner", "/tmp/.claude.personal"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "hook");
        assert_eq!(rest_len, 2);
    }

    #[test]
    fn dispatch_explicit_cas() {
        let a = argv(&["csm", "cas", "--eval", "--shell", "zsh", "--", "personal"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "cas");
        assert_eq!(rest_len, 5);
    }

    #[test]
    fn dispatch_explicit_pick_account() {
        let a = argv(&["csm", "pick-account", "personal", "--include-current"]);
        let (cmd, _) = dispatch_subcommand(&a);
        assert_eq!(cmd, "pick-account");
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
        let a = argv(&["csm", "current-usage", "personal"]);
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
        let a = argv(&["csm", "-n"]);
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
        let a = argv(&["csm", "run", "-n", "--profile=personal"]);
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
        let a = argv(&["csm", "run", "-n", "--profile=work"]);
        let rest = &a[2..];
        let parsed = parser::parse(rest);
        assert!(parsed.flags.new);
        assert_eq!(parsed.flags.profile.as_deref(), Some("work"));
        assert!(parsed.passthru.is_empty());
    }

    #[test]
    fn parser_run_passthru() {
        let a = argv(&["csm", "run", "--", "--dangerously-skip-permissions"]);
        let rest = &a[2..];
        let parsed = parser::parse(rest);
        assert!(!parsed.flags.new);
        assert_eq!(
            parsed.passthru,
            vec![OsString::from("--dangerously-skip-permissions")]
        );
    }

    // ── parse_owner_flag ──────────────────────────────────────────────────────

    #[test]
    fn parse_owner_flag_space_form() {
        let args = argv(&["--owner", "/Users/example/.claude.personal"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.personal")));
    }

    #[test]
    fn parse_owner_flag_equals_form() {
        let args = argv(&["--owner=/Users/example/.claude.personal"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.personal")));
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
        let args = argv(&["--eval", "--shell", "zsh", "--", "personal"]);
        let (eval, shell, op_args) = parse_cas_flags(&args).unwrap();
        assert!(eval);
        assert_eq!(shell.as_deref(), Some("zsh"));
        assert_eq!(op_args, vec!["personal"]);
    }

    #[test]
    fn parse_cas_flags_equals_form_shell() {
        let args = argv(&["--eval", "--shell=pwsh", "--", "work"]);
        let (eval, shell, op_args) = parse_cas_flags(&args).unwrap();
        assert!(eval);
        assert_eq!(shell.as_deref(), Some("pwsh"));
        assert_eq!(op_args, vec!["work"]);
    }

    #[test]
    fn parse_cas_flags_no_eval_mode() {
        let args = argv(&["status"]);
        let (eval, _shell, op_args) = parse_cas_flags(&args).unwrap();
        assert!(!eval);
        assert_eq!(op_args, vec!["status"]);
    }

    #[test]
    fn parse_cas_flags_global_op() {
        let args = argv(&["--eval", "--shell", "zsh", "--", "-g", "personal"]);
        let (_eval, _shell, op_args) = parse_cas_flags(&args).unwrap();
        assert_eq!(op_args, vec!["-g", "personal"]);
    }

    // ── parse_cas_op ──────────────────────────────────────────────────────────

    #[test]
    fn parse_cas_op_switch() {
        let op = parse_cas_op(&["personal".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Switch { profile } if profile == "personal"));
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
        assert!(matches!(op, cas::Op::Status { print_current: false }));
    }

    #[test]
    fn parse_cas_op_status_explicit() {
        let op = parse_cas_op(&["status".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Status { print_current: false }));
    }

    #[test]
    fn parse_cas_op_status_print_current() {
        let op = parse_cas_op(&["status".to_owned(), "--print-current".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Status { print_current: true }));
    }

    #[test]
    fn parse_cas_op_global_long_form() {
        let op =
            parse_cas_op(&["--global".to_owned(), "personal".to_owned()]).unwrap();
        assert!(matches!(op, cas::Op::Global { profile } if profile == "personal"));
    }

    // ── parse_sidecar_kv_args ─────────────────────────────────────────────────

    #[test]
    fn parse_sidecar_kv_permission_mode() {
        let args = argv(&["permission_mode=bypassPermissions"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(
            patch.permission_mode.as_deref(),
            Some("bypassPermissions")
        );
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
                "personal": {
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
        assert_eq!(profiles["personal"].session_pct, Some(3));
        assert_eq!(profiles["personal"].week_all_pct, Some(32));
        assert_eq!(
            profiles["personal"].resets.as_deref(),
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
                "personal": {
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
}
