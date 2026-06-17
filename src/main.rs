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
use uuid::Uuid;

/// Print a fresh lowercase UUID v4. Used as --session-id on a cold launch.
fn newuuid() -> String {
    Uuid::new_v4().to_string()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();

    // argv[0]-aware dispatch: if this binary is invoked as a known alias, treat
    // it as if that subcommand was the first argument.
    let argv0 = args
        .first()
        .and_then(|a| {
            std::path::Path::new(a)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();

    // Determine which subcommand to dispatch.
    // For the single-binary (no split csm-hook) form, `csm hook` is a subcommand.
    let subcommand: &str;
    let rest: &[OsString];

    if argv0 == "csm-hook" {
        // Invoked as csm-hook directly (symlink / rename form)
        subcommand = "hook";
        rest = &args[1..];
    } else if args.len() >= 2 {
        let candidate = args[1].to_string_lossy();
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
                // No recognized subcommand → implicit `csm run`
                subcommand = "run";
                rest = &args[1..];
            }
        }
    } else {
        // Bare `csm` → implicit `csm run`
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

// ─── subcommand stubs ─────────────────────────────────────────────────────────
// Each arm will grow into a real implementation. Stubs are marked unimplemented!
// so the compiler enforces they are wired before reaching callers, but the binary
// compiles and dispatches correctly today.

fn cmd_run(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm run: foreground launcher + relaunch loop (see launcher/, session/, picker/, account/)")
}

fn cmd_hook(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm hook: limit-switch hook (stdin JSON → detect → stop → relaunch sentinel) (see hook/)")
}

fn cmd_cas(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm cas: --eval emit + state-file write + launchctl/HKCU side-effects (see cas/)")
}

fn cmd_pick_account(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm pick-account: account scorer (see account/)")
}

fn cmd_scan(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm scan: session scan TSV (see session/)")
}

fn cmd_current_usage(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm current-usage: usage transport (see usage/)")
}

fn cmd_sidecar(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm sidecar: read/write/merge/flags (see sidecar/)")
}

fn cmd_statusline(args: &[OsString]) -> anyhow::Result<()> {
    statusline::run(args)
}

fn cmd_completions(_args: &[OsString]) -> anyhow::Result<()> {
    unimplemented!("csm completions: clap shell completions (see cli/completions.rs)")
}
