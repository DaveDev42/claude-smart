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
    // source of truth for this rule and the reserved word list.
    let (subcommand, rest_len) = cli::reserved::dispatch_subcommand(&args);
    let rest: &[OsString] = &args[args.len() - rest_len..];

    match subcommand {
        "run" => cmd::run::run(rest),
        "hook" => cmd::hook::cmd_hook(rest),
        "profiles" => cmd::profiles::cmd_profiles(rest),
        "config" => cmd::config::cmd_config(rest),
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
