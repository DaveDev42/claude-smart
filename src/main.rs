mod account;
mod cli;
mod cmd;
mod config;
mod e2e;
mod envvar;
mod epoch;
mod hook;
mod idle_compact;
mod launch_context;
mod migrate;
mod orca;
mod paths;
mod picker;
mod platform;
mod reaper;
mod screen_check;
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
    use cli::reserved::Invocation;

    e2e::guard();
    let args: Vec<OsString> = std::env::args_os().collect();

    // Hidden pty-relay leader entry point (`csm __pty-leader <slave-path> --
    // <argv...>`), intercepted before any other argv handling — csm can be
    // invoked under a `claude`-named symlink (Orca's alias), and this word
    // must never depend on that name-based dispatch below. Never reached
    // except via `RelayLauncher`'s own re-exec of `current_exe()`.
    #[cfg(unix)]
    if args.len() >= 2 && args[1] == "__pty-leader" {
        std::process::exit(platform::relay::leader::main(&args[2..]));
    }
    // Windows counterpart: the helper `ConptyLauncher` starts inside its
    // pseudoconsole to run claude there.
    #[cfg(windows)]
    if args.len() >= 2 && args[1] == platform::relay::conpty_logic::LEADER_WORD {
        std::process::exit(platform::relay::conpty::leader_main(&args[2..]));
    }

    // Top-level `--version`/`-V` and `--help`/`-h` belong to csm itself, not to
    // claude — but only under the name `csm`. `csm-hook --version` must reach
    // `cmd_hook`, and the `claude` alias forwards both to the real claude so
    // Orca's version probe sees claude's version. Intercept only as the very
    // first token so `csm run --help` still routes into run's own usage.
    if cli::reserved::invocation(&args) == Invocation::Csm && args.len() >= 2 {
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

    // argv[0]-aware dispatch (csm, csm-hook, the `claude` alias).
    // `cli::reserved::dispatch_subcommand` is the single tested source of
    // truth for this rule and the reserved word list.
    let dispatch = cli::reserved::dispatch_subcommand(&args);
    let rest: &[OsString] = &args[args.len() - dispatch.rest_len..];

    // The automatic migration's FULL and NOTE triggers (design section 1);
    // every NONE word returns before its probe, and `run`/`migrate` decide
    // for themselves.
    migrate::at_dispatch(dispatch.subcommand, rest);

    match dispatch.subcommand {
        "run" => cmd::run::run(rest),
        "claude" => cmd::claude::cmd_claude(rest),
        "hook" => cmd::hook::cmd_hook(rest),
        "accounts" => cmd::accounts::cmd_accounts(rest),
        "orca" => cmd::orca::cmd_orca(rest),
        "migrate" => cmd::migrate::cmd_migrate(rest),
        "config" => cmd::config::cmd_config(rest),
        "usage" => cmd::usage::cmd_usage(rest),
        "cas" => cmd::cas::cmd_cas(rest),
        "scan" => cmd::scan::cmd_scan(rest),
        "reap" => reaper::cmd(rest),
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
    println!("  csm run [csm-flags] [-- claude...]   smart launcher (session + relaunch)");
    println!("  csm <subcommand> ...\n");
    print_run_flags();
    println!();
    println!("ACCOUNTS (Orca's Claude accounts — csm keeps no registry of its own)");
    println!("  csm accounts [list]                  list accounts (active and D's marked)");
    println!("  csm accounts use <id|prefix|email>   switch (Orca RPC when running, else offline)");
    println!("  csm accounts add                     log in a new account");
    println!("  csm accounts import <dir>...         import the login held by config dirs");
    println!("  csm accounts rm <id|prefix|email>    remove a non-active account");
    println!(
        "  csm accounts doctor [--fix] [--offline]   check store, stashes, quarantine and D\n"
    );
    println!("ORCA");
    println!("  csm orca [status]                    Orca as csm sees it (never prints secrets)");
    println!("  csm orca setup                       create the `claude` alias for Orca\n");
    println!("MIGRATION (from the profile-based setup; also runs on its own)");
    println!("  csm migrate [--dry-run]              move this machine onto Orca's accounts now\n");
    println!("CONFIG (csm's own — ~/.config/claude-smart/config.json)");
    println!("  csm config [show]                    print the config JSON");
    println!("  csm config get launch-command        print the resolved launch command");
    println!(
        "  csm config set launch-command <cmd>...   launch <cmd> instead of `claude` (e.g. happy)"
    );
    println!("  csm config unset launch-command      revert to launching `claude`");
    println!(
        "  csm config get|set|unset min-claude-version [<v>]   the lowest claude a limit switch accepts beside an unsupervised session\n"
    );
    println!("USAGE METERING (local, per account)");
    println!(
        "  csm usage [--json] [--no-fetch] [--refresh]   multi-account usage table (offline-aware)"
    );
    println!(
        "  csm usage capture                    read statusLine stdin, merge into the store\n"
    );
    println!("OTHER");
    println!("  csm scan [<cwd>]                     session TSV for the picker");
    println!(
        "  csm reap [--dry-run] [--term] [--all|--session <sid>]   kill orphan processes left by claude"
    );
    println!("  csm sidecar {{read|write|merge|flags}} <sid> [k=v...]");
    println!("  csm statusline                       Claude Code statusLine segment");
    println!("  csm completions {{zsh|bash|pwsh}}      shell completions");
    println!("  csm newuuid                          fresh lowercase UUID v4");
    println!(
        "  csm claude <args...>                 run claude in csm's runtime dir, args verbatim\n"
    );
    println!("Words not listed above forward to `claude` (e.g. `csm mcp`, `csm doctor`).");
    println!("To pass a csm-reserved flag to claude, use `csm run -- <args>`.");
    println!("Invoked as `claude` (the `csm orca setup` alias), claude's own subcommands,");
    println!("--version and --help go to the real claude; csm's words are not dispatched.");
}

/// The `RUN FLAGS` block — shared by `csm --help` and [`print_run_help`], so
/// the two can never drift.
fn print_run_flags() {
    println!("RUN FLAGS (session selection)");
    println!("  -i, --interactive                    open the session picker");
    println!(
        "  -n, --new                            start a fresh session (shadows claude's -n/--name)"
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
    println!("  (csm stops reading flags at the first positional argument; every other claude");
    println!("   flag passes through untouched — use `csm run -- <args>` to force passthrough)");
    println!("  (-p/--print or a piped stdin runs claude verbatim. Inside Orca nothing prompts:");
    println!("   no session flag starts a fresh session. Elsewhere the session picker opens.)");
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
