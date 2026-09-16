mod account;
mod cas;
mod cli;
mod cmd;
mod config;
mod envvar;
mod epoch;
mod hook;
mod paths;
mod picker;
mod platform;
mod provision;
mod reaper;
mod session;
mod sidecar;
mod statusline;
#[cfg(test)]
mod testenv;
mod usage;

// Process-wide allocator: mimalloc on every target (see Cargo.toml).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context as _;

use cmd::support::{newuuid, read_stdin_capped, CAPTURE_STDIN_CAP_BYTES};

fn main() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();

    // Top-level `--version`/`-V` and `--help`/`-h` belong to csm itself, not to
    // claude. (To pass these through to claude, use `csm run -- --version`.)
    // Intercept only when they are the very first token so `csm run --help`
    // routing into cmd_run's own usage still works, and only when this
    // invocation is not the `csm-hook` argv[0] alias — `csm-hook --version`
    // must still reach `cmd_hook`, not print csm's own version/help.
    if !cli::reserved::invoked_as_hook_alias(&args) && args.len() >= 2 {
        match args[1].to_string_lossy().as_ref() {
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
    }

    // argv[0]-aware dispatch: if this binary is invoked as a known alias, treat
    // it as if that subcommand was the first argument (multi-call binary
    // support). `cli::reserved::dispatch_subcommand` is the single tested
    // source of truth for this rule and the reserved word list.
    let (subcommand, rest_len) = cli::reserved::dispatch_subcommand(&args);
    let rest: &[OsString] = &args[args.len() - rest_len..];

    match subcommand {
        "run" => cmd::run::run(rest),
        "hook" => cmd_hook(rest),
        "profiles" => cmd::profiles::cmd_profiles(rest),
        "config" => cmd::config::cmd_config(rest),
        "usage" => cmd_usage(rest),
        "cas" => cmd::cas::cmd_cas(rest),
        "pick-account" => cmd_pick_account(rest),
        "scan" => cmd_scan(rest),
        "reap" => reaper::cmd(rest),
        "current-usage" => cmd_current_usage(rest),
        "sidecar" => cmd_sidecar(rest),
        "statusline" => statusline::run(rest),
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
/// setup-token/ultrareview/update). `config` is also disjoint — `claude config`
/// is not a recognized claude subcommand (it prints top-level help). Any first
/// token NOT listed here falls through to an implicit `csm run` → forwarded
/// verbatim to `claude`, so `csm mcp …`, `csm doctor`, etc. reach claude
/// untouched.
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
    println!("  -A, --pick-account                   force an account pick this launch (overrides --no-pick)");
    println!("  --no-pick                            keep current profile, no scoring");
    println!(
        "  -n, --new                            start a fresh session (skip the session picker)"
    );
    println!("  -c, --continue                       resume newest free session");
    println!("  -r, --resume [<id>|<alias>]          resume a session (csm also reads the id)");
    println!(
        "  --session-id <uuid>                  forwarded to claude; csm tracks it for sidecar/relaunch state"
    );
    println!("  --model <m>                          forwarded to claude; remembered across a limit-switch hop");
    println!("  --effort <e>                         forwarded to claude; remembered across a limit-switch hop");
    println!(
        "  --permission-mode <p>                forwarded to claude; remembered across a limit-switch hop"
    );
    println!("  (the six flags above are forwarded to claude AND read by csm; every other claude");
    println!("   flag passes through untouched — use `csm run -- <args>` to force passthrough)");
    println!("  (default: always opens the session picker — new / continue / pick existing —");
    println!("   and auto-picks the best account by usage; opens the account picker when no");
    println!("   usable usage data is available instead of silently staying put)\n");
    println!("PROFILES (registry — ~/.config/claude-as/profiles.json)");
    println!("  csm profiles [list]                  list configured profiles");
    println!("  csm profiles add  <name> [<dir>]     register (dir default ~/.claude.<name>)");
    println!("  csm profiles set  <name> <dir>       register/overwrite a profile dir");
    println!("  csm profiles rm   <name>             unregister (refused if it is the default)");
    println!("  csm profiles use  <name>             set machine default + floor");
    println!("  csm profiles edit                    interactive editor (TTY)");
    println!("  csm profiles dir  [<name>]           print a profile's dir (default if omitted)");
    println!(
        "  csm profiles bootstrap [<name>|--all] provision profile env (dir + shared plugins/projects)"
    );
    println!("  csm profiles doctor [--fix] [<name>|--all] diagnose/repair provisioning\n");
    println!("CONFIG (csm's own — ~/.config/claude-smart/config.json)");
    println!("  csm config [show]                    print the config JSON");
    println!("  csm config get launch-command        print the resolved launch command");
    println!(
        "  csm config set launch-command <cmd>...   launch <cmd> instead of `claude` (e.g. happy)"
    );
    println!("  csm config unset launch-command      revert to launching `claude`\n");
    println!("USAGE METERING (local, per profile)");
    println!(
        "  csm usage [--json] [--no-fetch] [--refresh] [--refresh-oauth]   multi-profile usage table (offline-aware)"
    );
    println!(
        "  csm usage capture                    read statusLine stdin, merge into the store\n"
    );
    println!("OTHER");
    println!("  csm pick-account [<cur>] [--include-current]   scoring → winner profile");
    println!("  csm scan [<cwd>]                     session TSV for the picker");
    println!(
        "  csm reap [--dry-run] [--term] [--all|--session <sid>]   kill orphan processes left by claude"
    );
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

// ─── usage ───────────────────────────────────────────────────────────────────

/// `csm usage [--json] [--no-fetch] [--refresh] [--refresh-oauth]` /
/// `csm usage capture`
///
/// Multi-profile usage table joining the registry with the local per-profile
/// usage store. Offline-aware: serves the stale positive cache with an age
/// header when local collection is unreachable (no credentials, no network).
/// `--no-fetch` reads only the cache (never touches credentials/network) for
/// fast scripted reads; `--refresh` bypasses the cache and every profile's own
/// store-record TTL, forcing a live re-probe of each profile.
///
/// `--refresh-oauth` (or `CSM_OAUTH_REFRESH=1`) is the headless-collector
/// opt-in: it permits `usage::local::refresh` to mint a new access token for
/// a profile whose own has expired while no Claude Code session is running
/// under it. It is resolved here and threaded explicitly down the fetch
/// chain, so no other entry point (statusline, picker, sidecar, hook) can
/// ever trigger a credential write.
///
/// `csm usage capture` is the statusLine-stdin capture path (see
/// [`cmd_usage_capture`]) — a distinct subverb, not a flag.
fn cmd_usage(args: &[OsString]) -> anyhow::Result<()> {
    use usage::report;

    // `csm usage capture` is checked first so the bare positional never falls
    // into the flag loop below (it takes no flags of its own).
    if args.first().map(|a| a.to_string_lossy()).as_deref() == Some("capture") {
        return cmd_usage_capture();
    }

    let mut json = false;
    let mut no_fetch = false;
    let mut refresh = false;
    let mut refresh_oauth = false;
    for a in args {
        match a.to_string_lossy().as_ref() {
            "--json" => json = true,
            "--no-fetch" => no_fetch = true,
            "--refresh" => refresh = true,
            "--refresh-oauth" => refresh_oauth = true,
            "-h" | "--help" => {
                println!("usage: csm usage [--json] [--no-fetch] [--refresh] [--refresh-oauth]");
                println!("       csm usage capture");
                println!("  --json      emit the joined registry∪local view as JSON");
                println!("  --no-fetch  read only the local cache (no live collection)");
                println!(
                    "  --refresh   bypass the cache and every profile's own TTL; re-probe live"
                );
                println!("  --refresh-oauth  for headless collectors: refresh a profile's expired");
                println!("              OAuth access token when no Claude Code session is running");
                println!(
                    "              under it (env CSM_OAUTH_REFRESH=1; not supported on macOS)"
                );
                println!(
                    "  capture     read statusLine JSON from stdin, merge into the local store"
                );
                return Ok(());
            }
            other => anyhow::bail!(
                "csm usage: unknown flag '{other}' (try --json | --no-fetch | --refresh | \
                 --refresh-oauth | capture)"
            ),
        }
    }
    // Flag OR env — resolved once, here, and passed down explicitly.
    let refresh_oauth = refresh_oauth || usage::local::refresh::opt_in_from_env();

    let profiles =
        account::ProfileMap::load().context("csm usage: failed to load profiles.json")?;
    // "Configured" now simply means the registry isn't empty — local
    // collection needs no separate opt-in env (unlike the retired remote
    // transport, which required two site-specific env vars to name it).
    let configured = !profiles.is_empty();

    // Resolve usage data + freshness. `--no-fetch` reads the cache directly;
    // `--refresh` forces fetch_with(true) (cache + per-profile TTL bypass);
    // otherwise fetch() runs the full resilience ladder (which itself prefers
    // a fresh cache before any live collection).
    // Staleness age is derived from the DATA's own per-profile `captured_at`
    // timestamps (`oldest_profile_age_secs` — the age of the least-fresh
    // served profile), never from `.usage-cache.json`'s file mtime.
    // `write_positive_cache` refreshes that mtime on every non-total-failure
    // `fetch_with` call — including a round where every profile was
    // `ServeStale`-served from a days-old store record — so the file's mtime
    // no longer reflects how old the served numbers actually are; the "⚠
    // usage data is Nm old" banner would otherwise be unreachable for exactly
    // the offline/expired-token case it exists to surface.
    let (data, stale_secs) = if !configured {
        (None, None)
    } else if no_fetch {
        let cached = read_usage_cache();
        let stale = cached
            .as_ref()
            .and_then(|d| usage::local::oldest_profile_age_secs(d, chrono::Utc::now()));
        (cached, stale)
    } else {
        let fetch_result = usage::fetch_with(refresh, refresh_oauth);
        match fetch_result {
            Ok(d) => {
                let stale = usage::local::oldest_profile_age_secs(&d, chrono::Utc::now());
                (Some(d), stale)
            }
            Err(_) => {
                // Local collection unreachable — degrade to the last-known cache, if any.
                let cached = read_usage_cache();
                let stale = cached
                    .as_ref()
                    .and_then(|d| usage::local::oldest_profile_age_secs(d, chrono::Utc::now()));
                (cached, stale)
            }
        }
    };

    let rpt = report::build_report(&profiles, data.as_ref(), configured, stale_secs);

    if json {
        println!("{}", report::render_json(&rpt)?);
    } else {
        print!("{}", report::render_table(&rpt, chrono::Utc::now()));
    }
    Ok(())
}

/// Read the positive usage cache file directly (no network, no TTL gate). Used
/// by `--no-fetch` and the offline-degrade path. Returns `None` when absent or
/// unparseable.
pub(crate) fn read_usage_cache() -> Option<usage::UsageData> {
    let raw = std::fs::read_to_string(paths::usage_cache()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `csm usage capture` — read a statusLine JSON payload from stdin and merge
/// its `rate_limits` into the active profile's local usage store record (see
/// `usage::local::record_statusline_payload`).
///
/// This is meant to run silently as a fire-and-forget tail of a
/// `statusline-command.sh`/`.ps1` (e.g. `printf '%s' "$input" | csm usage
/// capture &`), so it swallows every error — a malformed/partial payload, an
/// unresolvable profile, an unset `CLAUDE_CONFIG_DIR`, a throttled write — and
/// unconditionally prints nothing and exits 0. A statusLine command that fires
/// roughly once a second must never let a transient capture failure surface
/// as prompt noise or a non-zero exit.
fn cmd_usage_capture() -> anyhow::Result<()> {
    let raw = read_stdin_capped(CAPTURE_STDIN_CAP_BYTES);
    if let Ok(Some(capture)) = usage::local::record_statusline_payload(&raw) {
        hook::run_from_statusline(&raw, &capture);
    }
    Ok(())
}

// ─── pick-account ──────────────────────────────────────────────────────────────

/// `csm pick-account [<current>] [--include-current]`
///
/// Prints the winner profile name to stdout, or nothing on no-op.
/// Exits 1 on fetch failure.
///
/// Routes through `account::pick_account` → `scoring::pick_best_at`, which
/// scores on all three usage dimensions (session, week_all, and the
/// model-scoped weekly `week_fable`) — a profile whose `week_fable` is
/// saturated is skipped exactly like a session- or week_all-limited one.
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
                // Store as a JSON Number (the canonical Rust-binary form; the
                // legacy sidecar writer used a string, so readers tolerate both).
                patch.hop = Some(serde_json::Value::Number(serde_json::Number::from(n)));
            }
            other => anyhow::bail!("csm sidecar: unknown key {other:?}"),
        }
    }
    Ok(patch)
}

// ─── statusline ────────────────────────────────────────────────────────────────

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
    use crate::cli::reserved::dispatch_subcommand;

    // ══════════════════════════════════════════════════════════════════════════
    // Dispatch routing — verify that the argument dispatcher picks the right
    // subcommand word, covering the full table in main(). Exercises
    // `cli::reserved::dispatch_subcommand`, the single tested source of truth
    // for the reserved word list (`cli::reserved` module).
    // Pure-logic tests: no subprocess / real I/O / network calls.
    // ══════════════════════════════════════════════════════════════════════════

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
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

    #[test]
    fn dispatch_explicit_config() {
        let a = argv(&["csm", "config", "show"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "config");
        assert_eq!(rest_len, 1);
    }

    #[test]
    fn dispatch_explicit_reap() {
        let a = argv(&["csm", "reap", "--dry-run"]);
        let (cmd, rest_len) = dispatch_subcommand(&a);
        assert_eq!(cmd, "reap");
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

    // ── UsageData decode (cache_mtime / read_usage_cache field mapping) ───────

    #[test]
    fn usage_data_full_payload_maps_fields() {
        let json = serde_json::json!({
            "profiles": {
                "home": {
                    "session": { "pct": 3 },
                    "week_all": { "pct": 32, "resets": "Jun 18 at 9pm (Asia/Seoul)", "resets_at": 1_781_000_000_i64 }
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
        let data: usage::UsageData = serde_json::from_value(json).unwrap();
        assert_eq!(data.profiles.len(), 2);
        let home = &data.profiles["home"];
        assert_eq!(home.session.as_ref().map(|s| s.pct), Some(3));
        let home_week_all = home.week_all.as_ref().unwrap();
        assert_eq!(home_week_all.pct, 32);
        assert_eq!(
            home_week_all.resets.as_deref(),
            Some("Jun 18 at 9pm (Asia/Seoul)")
        );
        assert_eq!(home_week_all.resets_at, Some(1_781_000_000));

        let work = &data.profiles["work"];
        assert!(work.session.is_none());
        let work_week_all = work.week_all.as_ref().unwrap();
        assert_eq!(work_week_all.pct, 80);
        assert_eq!(
            work_week_all.resets_at, None,
            "resets_at absent in cache JSON must parse as None"
        );

        assert_eq!(data.errors.unwrap()["broken"], "no credentials");
    }

    #[test]
    fn usage_data_absent_errors_key_parses_none() {
        let json = serde_json::json!({
            "profiles": {
                "home": {
                    "week_all": { "pct": 50 }
                }
            }
        });
        let data: usage::UsageData = serde_json::from_value(json).unwrap();
        assert_eq!(data.profiles.len(), 1);
        assert!(data.errors.is_none());
    }

    #[test]
    fn usage_data_empty_object_parses_to_defaults() {
        let data: usage::UsageData = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(data.profiles.is_empty());
        assert!(data.errors.is_none());
    }
}
