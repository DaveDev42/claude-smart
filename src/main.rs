mod account;
mod cas;
mod cli;
mod cmd;
mod config;
mod envvar;
mod epoch;
mod homeguard;
mod hook;
mod orca;
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

use cmd::support::newuuid;

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
    // source of truth for this rule, the reserved word list, and the one
    // csm-global flag that may precede the subcommand word (`--profile`).
    let dispatch = cli::reserved::dispatch_subcommand(&args);
    let rest: &[OsString] = &args[args.len() - dispatch.rest_len..];

    // `csm --profile <name> <subcommand>`: pin CLAUDE_CONFIG_DIR so everything
    // below — statusline, usage, hook, `profiles dir`, sidecar, the `claude`
    // passthrough — reads that profile. `run` is the exception: it gets the
    // flag re-injected instead, so `cli::parser`'s `--profile` stays the one
    // place a launch resolves its pin.
    if let Some(name) = dispatch.profile.as_deref()
        && dispatch.subcommand != "run"
    {
        pin_global_profile(name)?;
    }

    match dispatch.subcommand {
        "run" => cmd::run::run(&cli::reserved::run_args_with_profile(
            dispatch.profile.as_deref(),
            rest,
        )),
        "claude" => cmd::claude::cmd_claude(rest),
        "hook" => cmd::hook::cmd_hook(rest),
        "profiles" => cmd::profiles::cmd_profiles(rest),
        "config" => cmd::config::cmd_config(rest),
        "orca" => cmd::orca::cmd_orca(rest),
        "usage" => cmd::usage::cmd_usage(rest),
        "cas" => cmd::cas::cmd_cas(rest),
        "pick-account" => cmd::pick_account::cmd_pick_account(rest),
        "scan" => cmd::scan::cmd_scan(rest),
        "reap" => reaper::cmd(rest),
        "current-usage" => cmd::pick_account::cmd_current_usage(rest),
        "sidecar" => cmd::sidecar::cmd_sidecar(rest),
        "statusline" => statusline::run(rest),
        "completions" => cmd::completions::cmd_completions(rest),
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

/// Pin `CLAUDE_CONFIG_DIR` for a csm-global `--profile <name>` that preceded a
/// reserved subcommand word (`csm --profile work statusline`).
///
/// The name resolves through `ProfileMap` via the same
/// `cmd::support::resolve_profile_dir` that `csm run --profile` uses — registry
/// hit first, conventional `~/.claude.<name>` synthesis as the fallback — so
/// both spellings land on the same directory, and the profile is provisioned
/// the same best-effort way a launch provisions it.
fn pin_global_profile(name: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let profiles = account::ProfileMap::load().context("csm: failed to load profiles.json")?;
    let dir = cmd::support::resolve_profile_dir(name, &profiles)?;
    provision::ensure_provisioned_soft(std::path::Path::new(&dir));

    // SAFETY: this is `main()` before any subcommand handler runs and before
    // anything in csm spawns a thread, so the process is single-threaded and
    // no other thread can be reading the environment concurrently. (The only
    // other `set_var` call sites in the crate are the test-only ones in
    // `testenv`.)
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };
    Ok(())
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
    println!("  csm [--profile <name>] <subcommand> ...\n");
    print_run_flags();
    println!();
    println!("PROFILES (registry — ~/.config/claude-as/profiles.json)");
    println!("  csm profiles [list]                  list configured profiles");
    println!("  csm profiles add  <name> [<dir>]     register (dir default ~/.claude.<name>)");
    println!("  csm profiles set  <name> <dir>       register/overwrite a profile dir");
    println!("  csm profiles rm   <name>             unregister (refused if it is the default)");
    println!("  csm profiles use  <name>             set machine default + floor");
    println!("  csm profiles edit                    interactive editor (TTY)");
    println!("  csm profiles dir  [<name>]           print a profile's dir (default if omitted)");
    println!(
        "  csm profiles bootstrap [<name>|--all] provision profile env (dir + shared plugins/projects/sessions)"
    );
    println!(
        "  csm profiles doctor [--fix] [--fix-home] [<name>|--all]   check profile dirs / shared links; --fix repairs profiles, --fix-home repairs the ~/.claude shim\n"
    );
    println!("CONFIG (csm's own — ~/.config/claude-smart/config.json)");
    println!("  csm config [show]                    print the config JSON");
    println!("  csm config get launch-command        print the resolved launch command");
    println!(
        "  csm config set launch-command <cmd>...   launch <cmd> instead of `claude` (e.g. happy)"
    );
    println!("  csm config unset launch-command      revert to launching `claude`");
    println!(
        "  csm config get|set|unset orca.follow-switch|orca.user-data-dir   Orca interop settings\n"
    );
    println!("ORCA (desktop-app account interop)");
    println!(
        "  csm orca init [--slot <p>] [--dir <d>] [--no-floor] [--force]   register the slot, floor → slot"
    );
    println!("  csm orca disable                     Orca mode off; floor back to the default");
    println!("  csm orca status [--json] [--strict]  diagnose the Orca setup");
    println!("  csm orca accounts [--json]           Orca accounts + bound profiles");
    println!(
        "  csm orca use <profile|email|id> [--queue]   select an account in Orca (queue if not running)"
    );
    println!(
        "  csm orca sync [--quiet]              apply a queued select; mirror Orca's active account\n"
    );
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
        "  csm claude <args...>                 run claude under csm's profile, args forwarded verbatim"
    );
    println!(
        "  csm cas ...                          eval-class shim contract (machine interface)\n"
    );
    println!("Words not listed above forward to `claude` (e.g. `csm mcp`, `csm doctor`).");
    println!("To pass a csm-reserved flag to claude, use `csm run -- <args>`.");
}

/// The `RUN FLAGS` block — shared by `csm --help` and [`print_run_help`], so
/// the two can never drift.
fn print_run_flags() {
    println!("RUN FLAGS (account + session selection)");
    println!("  --profile <name>                     launch under this profile (skip all picking)");
    println!("  -i, --interactive                    manual pick: force account + session pickers");
    println!(
        "  -A, --pick-account                   force an account pick this launch (overrides --no-pick)"
    );
    println!("  --no-pick                            keep current profile, no scoring");
    println!(
        "  -n, --new                            start a fresh session (skip the session picker)"
    );
    println!("  -c, --continue                       resume newest free session");
    println!("  -r, --resume [<id>|<alias>]          resume a session (csm also reads the id)");
    println!(
        "  --session-id <uuid>                  forwarded to claude; csm tracks it for sidecar/relaunch state"
    );
    println!(
        "  --model <m>                          forwarded to claude; remembered across a limit-switch hop"
    );
    println!(
        "  --effort <e>                         forwarded to claude; remembered across a limit-switch hop"
    );
    println!(
        "  --permission-mode <p>                forwarded to claude; remembered across a limit-switch hop"
    );
    println!("  (the six flags above are forwarded to claude AND read by csm; every other claude");
    println!("   flag passes through untouched — use `csm run -- <args>` to force passthrough)");
    println!("  (default: always opens the session picker — new / continue / pick existing —");
    println!("   and auto-picks the best account by usage; opens the account picker when no");
    println!("   usable usage data is available instead of silently staying put)");
}

/// `csm run --help` — run's own usage, printed instead of being forwarded to
/// claude. Called from `cmd::run::run` when the parser saw `-h`/`--help`
/// before any passthru token; `csm run -- --help` still reaches claude.
pub(crate) fn print_run_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!("csm {v} — `csm run`, the smart launcher\n");
    println!("USAGE");
    println!("  csm run [csm-flags] [-- claude-args...]");
    println!("  csm [claude-args...]                 bare = implicit `csm run`\n");
    print_run_flags();
    println!();
    println!("Everything after `--` goes to claude verbatim, including flags csm reads");
    println!("itself: `csm run -- --help` prints claude's help, not this one.");
}
