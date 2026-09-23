//! `csm orca …` — the Orca desktop-app interop verbs.
//!
//! - `init [--slot <p>] [--dir <d>] [--no-floor] [--force]` — register the
//!   slot profile, point the machine-wide floor at it (§ migration order:
//!   refuse while Orca runs, print the re-login steps, then register).
//! - `disable` — Orca mode off; the floor returns to the default profile.
//! - `status [--json] [--strict]` — the [`crate::orca::slot::Diagnosis`].
//! - `accounts [--json]` — Orca's accounts (live, else saved) + bindings.
//! - `use <profile|email|account-id> [--queue]` — select in Orca (10 s).
//! - `sync [--quiet]` — apply a queued select; mirror Orca's active account
//!   into csm's default state. Always exits 0.
//!
//! The decisions live in pure helpers (flag parsing, target resolution,
//! migration planning, the accounts view) that are unit-tested; the verbs
//! themselves are thin I/O shells. Nothing here writes Orca's store; the
//! only write to Orca is `select_account` (see [`crate::orca`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::Serialize;

use crate::account::ProfileMap;
use crate::cas;
use crate::config::Config;
use crate::orca::bind::{self, Bindings};
use crate::orca::pending::{self, PendingSelect, Verdict};
use crate::orca::slot::{self, Slot};
use crate::orca::{self as orca_mod, Identity, OrcaState, SelectError, SelectOutcome, Selection};

/// Budget for a read-only `accounts.list` from an explicit command.
const READ_BUDGET: Duration = Duration::from_secs(3);
/// Budget for `csm orca use` / a pending select applied by `sync`.
const SELECT_BUDGET: Duration = Duration::from_secs(10);
/// Default slot profile name.
const DEFAULT_SLOT: &str = "orca";

/// `csm orca <verb> …`
pub(crate) fn cmd_orca(args: &[OsString]) -> anyhow::Result<()> {
    let verb = args.first().map(|a| a.to_string_lossy().into_owned());
    let rest: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match verb.as_deref() {
        None | Some("status") => cmd_status(&rest),
        Some("init") => cmd_init(&rest),
        Some("disable") => cmd_disable(&rest),
        Some("accounts") => cmd_accounts(&rest),
        Some("use") => cmd_use(&rest),
        Some("sync") => cmd_sync(&rest),
        Some("-h" | "--help" | "help") => {
            print_usage();
            Ok(())
        }
        Some(other) => anyhow::bail!(
            "csm orca: unknown verb '{other}' (expected init|disable|status|accounts|use|sync)"
        ),
    }
}

fn print_usage() {
    println!("csm orca — Orca desktop-app account interop\n");
    println!("  csm orca init [--slot <p>] [--dir <d>] [--no-floor] [--force]");
    println!("  csm orca disable");
    println!("  csm orca status [--json] [--strict]");
    println!("  csm orca accounts [--json]");
    println!("  csm orca use <profile|email|account-id> [--queue]");
    println!("  csm orca sync [--quiet]");
}

// ─── flag parsing (pure) ──────────────────────────────────────────────────────

/// Parsed verb arguments: boolean flags seen, `--key value` options, positionals.
type ParsedArgs = (Vec<String>, Vec<(String, String)>, Vec<String>);

/// Split `args` into recognised boolean flags, `--key value` options and
/// positionals. Unknown `--flags` are an error. Pure.
fn parse_flags(
    verb: &str,
    args: &[String],
    bools: &[&str],
    valued: &[&str],
) -> anyhow::Result<ParsedArgs> {
    let mut on = Vec::new();
    let mut opts = Vec::new();
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some((k, v)) = a.split_once('=')
            && valued.contains(&k)
        {
            opts.push((k.to_owned(), v.to_owned()));
        } else if bools.contains(&a.as_str()) {
            on.push(a.clone());
        } else if valued.contains(&a.as_str()) {
            let v = it
                .next()
                .with_context(|| format!("csm orca {verb}: {a} needs a value"))?;
            opts.push((a.clone(), v.clone()));
        } else if a.starts_with('-') {
            anyhow::bail!("csm orca {verb}: unknown flag '{a}'");
        } else {
            pos.push(a.clone());
        }
    }
    Ok((on, opts, pos))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct InitFlags {
    slot: Option<String>,
    dir: Option<String>,
    no_floor: bool,
    force: bool,
}

fn parse_init_flags(args: &[String]) -> anyhow::Result<InitFlags> {
    let (on, opts, pos) = parse_flags(
        "init",
        args,
        &["--no-floor", "--force"],
        &["--slot", "--dir"],
    )?;
    if let Some(p) = pos.first() {
        anyhow::bail!("csm orca init: unexpected argument '{p}'");
    }
    let opt = |k: &str| {
        opts.iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    Ok(InitFlags {
        slot: opt("--slot"),
        dir: opt("--dir"),
        no_floor: on.iter().any(|f| f == "--no-floor"),
        force: on.iter().any(|f| f == "--force"),
    })
}

/// `(boolean flags seen, positionals)` for the verbs without valued options.
fn parse_simple(
    verb: &str,
    args: &[String],
    bools: &[&str],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let (on, _, pos) = parse_flags(verb, args, bools, &[])?;
    Ok((on, pos))
}

// ─── shared loading ───────────────────────────────────────────────────────────

/// Registry + config, refusing an unreadable config (every verb here either
/// writes it or decides Orca-mode behaviour from it).
fn load_state(verb: &str) -> anyhow::Result<(ProfileMap, Config)> {
    let profiles = ProfileMap::load()
        .with_context(|| format!("csm orca {verb}: failed to load profiles.json"))?;
    let config = Config::load().with_context(|| {
        format!("csm orca {verb}: config.json is unreadable; fix or remove it first")
    })?;
    Ok((profiles, config))
}

fn require_slot(verb: &str, config: &Config, profiles: &ProfileMap) -> anyhow::Result<Slot> {
    slot::warn_if_unregistered(config, profiles);
    slot::active_slot(config, profiles).with_context(|| {
        format!("csm orca {verb}: Orca mode is off (set it up with `csm orca init`)")
    })
}

fn user_data(verb: &str, config: &Config) -> anyhow::Result<PathBuf> {
    orca_mod::user_data_dir_for(config.orca()).with_context(|| {
        format!(
            "csm orca {verb}: Orca's userData dir could not be resolved \
             (set ORCA_USER_DATA_PATH or `csm config set orca.user-data-dir <dir>`)"
        )
    })
}

fn is_running(user_data: &Path) -> Option<u32> {
    match orca_mod::runtime_metadata_in(user_data) {
        Ok(Some(m)) if m.is_alive() => Some(m.pid),
        _ => None,
    }
}

// ─── init ─────────────────────────────────────────────────────────────────────

/// Where Orca materialized accounts before the slot existed, in the §1.7
/// order: Orca's own process env, the launchd floor, the floor file, else
/// `~/.claude`. Pure.
fn former_runtime_dir(
    process_env: Option<String>,
    launchd: Option<String>,
    floor_file: Option<String>,
    home_claude: String,
) -> (String, &'static str) {
    if let Some(d) = process_env {
        return (d, "Orca's process environment");
    }
    if let Some(d) = launchd {
        return (d, "the launchd floor");
    }
    if let Some(d) = floor_file {
        return (d, "~/.config/claude-as/floor-dir");
    }
    (home_claude, "the ~/.claude default")
}

/// Registered non-slot profiles affected by the migration: those whose dir IS
/// the former runtime dir, plus those logged into the same identity as it
/// (Orca's read-back rotated their shared grant). Sorted. Pure.
fn affected_profiles(
    former_dir: &str,
    former_identity: Option<&Identity>,
    profiles: &[(String, String, Option<Identity>)],
    slot_name: &str,
) -> Vec<String> {
    let mut out: Vec<String> = profiles
        .iter()
        .filter(|(n, _, _)| n != slot_name)
        .filter(|(_, dir, id)| {
            cas::platform::dirs_equal(dir, former_dir)
                || matches!((former_identity, id), (Some(f), Some(p)) if bind::match_kind(f, p).is_some())
        })
        .map(|(n, _, _)| n.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Why the slot cannot be registered as `(name, dir)`, if it cannot. Pure.
/// `slot_identity` is the identity currently logged into `dir`;
/// `already_slot` is true when config already names `name` as the slot (a
/// re-run — Orca has materialized accounts into it, so an identity is
/// expected).
fn slot_registration_refusal(
    name: &str,
    dir: &str,
    profiles: &ProfileMap,
    slot_identity: Option<&Identity>,
    already_slot: bool,
) -> Option<String> {
    if !ProfileMap::is_valid_name(name) {
        return Some(format!(
            "invalid slot profile name '{name}' (allowed: letters, digits, . _ -)"
        ));
    }
    if let Some(existing) = profiles.get(name)
        && !cas::platform::dirs_equal(existing, dir)
    {
        return Some(format!(
            "profile '{name}' is already registered at {existing}; pass `--dir {existing}` \
             or choose another `--slot`"
        ));
    }
    for (other, odir) in profiles.iter() {
        if other != name && cas::platform::dirs_equal(odir, dir) {
            return Some(format!(
                "{dir} is already profile '{other}''s dir; the slot needs a dir of its own"
            ));
        }
    }
    if !already_slot && let Some(id) = slot_identity {
        return Some(format!(
            "{dir} is logged in as {}; the slot must be a profile with no account of its own \
             (choose another `--slot`/`--dir`)",
            id.email.as_deref().unwrap_or("an account")
        ));
    }
    None
}

fn cmd_init(args: &[String]) -> anyhow::Result<()> {
    let flags = parse_init_flags(args)?;
    let (mut profiles, mut config) = load_state("init")?;
    let name = flags
        .slot
        .clone()
        .unwrap_or_else(|| DEFAULT_SLOT.to_owned());
    let dir = match (&flags.dir, profiles.get(&name)) {
        (Some(d), _) => d.trim().to_owned(),
        (None, Some(d)) => d.to_owned(),
        (None, None) => crate::paths::synthesize_profile_dir(&name)
            .to_string_lossy()
            .into_owned(),
    };
    if !Path::new(&dir).is_absolute() {
        anyhow::bail!("csm orca init: --dir must be absolute (got {dir})");
    }

    // 1. Orca must be quit (its quit-time sync pulls the current grant into
    //    its stash first).
    let ud = orca_mod::user_data_dir_for(config.orca());
    let running_pid = ud.as_deref().and_then(is_running);
    if let Some(pid) = running_pid
        && !flags.force
    {
        anyhow::bail!(
            "csm orca init: Orca is running (pid {pid}). Quit Orca first so its quit-time sync \
             stores the current login, then re-run (or pass --force)."
        );
    }

    // 2. The former runtime dir and the profiles it affects.
    let home_claude = crate::paths::home_claude_dir()
        .to_string_lossy()
        .into_owned();
    let process_env = running_pid
        .and_then(orca_mod::runtime_config_dir)
        .map(|p| p.to_string_lossy().into_owned());
    let floor_file = std::fs::read_to_string(crate::paths::floor_dir_file())
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let (former, former_src) = former_runtime_dir(
        process_env,
        cas::platform::launchctl_getenv_config_dir(),
        floor_file,
        home_claude,
    );
    let former_identity = orca_mod::identity::read_profile_identity(Path::new(&former))
        .ok()
        .flatten();
    let with_ids: Vec<(String, String, Option<Identity>)> = profiles
        .names_sorted()
        .into_iter()
        .map(|n| {
            let d = profiles.get(n).unwrap_or_default().to_owned();
            let id = orca_mod::identity::read_profile_identity(Path::new(&d))
                .ok()
                .flatten();
            (n.to_owned(), d, id)
        })
        .collect();
    // A re-run after a completed migration finds the slot itself here; there
    // is nothing left to migrate then.
    let migrated = cas::platform::dirs_equal(&former, &dir);
    let affected = if migrated {
        Vec::new()
    } else {
        affected_profiles(&former, former_identity.as_ref(), &with_ids, &name)
    };

    // 4 (checked before anything is printed as done). Slot registration.
    let already_slot = config.orca().slot_profile.as_deref() == Some(name.as_str());
    let slot_identity = orca_mod::identity::read_profile_identity(Path::new(&dir))
        .ok()
        .flatten();
    if let Some(msg) =
        slot_registration_refusal(&name, &dir, &profiles, slot_identity.as_ref(), already_slot)
    {
        anyhow::bail!("csm orca init: {msg}");
    }

    // 3. Operator steps.
    println!("Orca's former runtime dir: {former} (from {former_src})");
    if migrated {
        println!("Orca already materializes into the slot; nothing to migrate.");
    } else if affected.is_empty() {
        println!("No registered profile shares it; nothing to re-login.");
    } else {
        println!("Profiles sharing it (Orca's token read-back touched their login):");
        for p in &affected {
            println!("  - {p}");
        }
        println!("Before starting Orca again:");
        println!("  1. stop every claude process running in those profiles' dirs;");
        for p in &affected {
            println!("  2. csm --profile {p} claude auth login");
        }
        println!(
            "     (never a bare `claude auth login` — it lands wherever the shell points, and \
             Orca would read it back)"
        );
        println!("  3. only then start Orca.");
    }

    // 4. Register + provision the slot, write the config.
    std::fs::create_dir_all(&dir).with_context(|| format!("csm orca init: cannot create {dir}"))?;
    if profiles.get(&name).is_none() {
        profiles.insert(name.clone(), dir.clone());
        profiles
            .save()
            .context("csm orca init: failed to write profiles.json")?;
    }
    if let Err(e) = crate::provision::ensure_profile_provisioned(&name, Path::new(&dir)) {
        eprintln!("csm orca init: warning: provisioning the slot failed: {e}");
    }
    config.orca.slot_profile = Some(name.clone());
    config
        .save()
        .context("csm orca init: failed to write config.json")?;
    println!("Orca slot: profile '{name}' → {dir}");

    // Floor → slot (apply_global applies `cas::floor_dir`, the slot, and
    // writes floor-dir).
    if flags.no_floor {
        println!("--no-floor: the machine-wide floor was left where it is.");
    } else {
        if let Err(e) = cas::platform::apply_global(&name, &dir) {
            eprintln!("csm orca init: warning: setting the floor failed: {e}");
        }
        report_floor(&dir);
    }

    // 5. settings.json + companion steps.
    if Path::new(&dir).join("settings.json").is_file() {
        println!("{dir}/settings.json: present.");
    } else {
        println!(
            "{dir}/settings.json: missing — template it like your other profiles (hooks, \
             statusline); csm does not create it."
        );
    }
    print_companion_steps(&dir);
    Ok(())
}

/// After a floor write: confirm the launchd value (macOS) or explain the
/// manual step (Linux / Windows).
fn report_floor(slot_dir: &str) {
    match orca_mod::HostOs::current() {
        orca_mod::HostOs::MacOs => match cas::platform::launchctl_getenv_config_dir() {
            Some(v) if cas::platform::dirs_equal(&v, slot_dir) => {
                println!("launchd CLAUDE_CONFIG_DIR = {v} (the slot).");
            }
            other => eprintln!(
                "csm orca: warning: launchd CLAUDE_CONFIG_DIR is {} — expected the slot {slot_dir}",
                other.as_deref().unwrap_or("unset")
            ),
        },
        orca_mod::HostOs::Linux => println!(
            "Linux has no machine-wide floor: start Orca with CLAUDE_CONFIG_DIR={slot_dir}."
        ),
        orca_mod::HostOs::Windows => println!(
            "Windows: the HKCU floor is the slot; account selection sync is not supported here."
        ),
    }
}

fn print_companion_steps(slot_dir: &str) {
    let csm = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "csm".to_owned());
    println!("\nOperator-side steps (csm does not do these):");
    println!(
        "  1. Login-time floor writer: call csm by absolute path (`{csm} cas --print-floor-dir`) \
         and fall back to ~/.config/claude-as/floor-dir before any hardcoded default."
    );
    println!(
        "  2. Start Orca from that same login agent after the floor is set; remove any separate \
         Orca launch-at-login item (the two race)."
    );
    println!(
        "  3. Shell startup keeps CLAUDE_CONFIG_DIR=$(csm cas --print-default-dir); a raw-file \
         fallback may read floor-dir only for the floor, never for the shell dir."
    );
    println!(
        "  4. Template {slot_dir}/settings.json like the other profiles and add the slot to your \
         profiles template."
    );
    println!("  5. Re-login the affected profiles once, with Orca quit (listed above).");
}

// ─── disable ──────────────────────────────────────────────────────────────────

fn cmd_disable(args: &[String]) -> anyhow::Result<()> {
    let (_, pos) = parse_simple("disable", args, &[])?;
    if let Some(p) = pos.first() {
        anyhow::bail!("csm orca disable: unexpected argument '{p}'");
    }
    let (profiles, mut config) = load_state("disable")?;
    let Some(name) = config.orca.slot_profile.take() else {
        println!("Orca mode is already off.");
        return Ok(());
    };
    config
        .save()
        .context("csm orca disable: failed to write config.json")?;
    // A queued select (and the RPC negative cache) belongs to the Orca mode
    // just turned off; a later re-`init` must not revive it.
    let _ = pending::clear();
    orca_mod::rpc::clear_negative_cache();
    // Orca mode is off now, so apply_global publishes the default dir as-is
    // (and records it in floor-dir).
    let default = profiles.default_name();
    let dir = profiles.default_dir().to_string_lossy().into_owned();
    if let Err(e) = cas::platform::apply_global(&default, &dir) {
        eprintln!("csm orca disable: warning: restoring the floor failed: {e}");
    }
    println!("Orca mode off. Floor → default profile '{default}' ({dir}).");
    println!("The profile '{name}' stays registered (remove it with `csm profiles rm {name}`).");
    println!(
        "WARNING: the floor now points at the per-account profile '{default}'. An Orca \
         (re)started from this floor adopts {dir} as its runtime dir: it reads that profile's \
         login into its own account list and writes its active account's credentials over it."
    );
    println!(
        "  Do not restart Orca from this floor. Quit it and keep it quit while Orca mode is off, \
         or re-run `csm orca init` to give it back a dedicated slot dir."
    );
    Ok(())
}

// ─── status ───────────────────────────────────────────────────────────────────

fn cmd_status(args: &[String]) -> anyhow::Result<()> {
    let (on, pos) = parse_simple("status", args, &["--json", "--strict"])?;
    if let Some(p) = pos.first() {
        anyhow::bail!("csm orca status: unexpected argument '{p}'");
    }
    let json = on.iter().any(|f| f == "--json");
    let strict = on.iter().any(|f| f == "--strict");
    let profiles = ProfileMap::load().context("csm orca status: failed to load profiles.json")?;
    let config = Config::load().map_err(|e| e.to_string());
    let input = slot::gather(
        &profiles,
        config.as_ref().map_err(Clone::clone),
        READ_BUDGET,
    );
    let d = slot::diagnose(&input);
    if json {
        println!("{}", serde_json::to_string_pretty(&d)?);
    } else {
        print!("{}", d.render());
    }
    if strict && d.has_error() {
        std::process::exit(1);
    }
    Ok(())
}

// ─── accounts ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct AccountRow {
    id: String,
    email: String,
    runtime: String,
    #[serde(rename = "organizationName")]
    organization_name: Option<String>,
    active: bool,
    profile: Option<String>,
    #[serde(rename = "bindingKind")]
    binding_kind: Option<bind::MatchKind>,
    candidates: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AccountsView {
    /// `live` or `offline`.
    source: &'static str,
    note: Option<String>,
    #[serde(rename = "activeAccountId")]
    active_account_id: Option<String>,
    accounts: Vec<AccountRow>,
    ties: Vec<bind::Tie>,
}

/// Join Orca's accounts with their bindings. Pure.
fn accounts_view(
    sel: &Selection,
    source: &'static str,
    note: Option<String>,
    bindings: &Bindings,
) -> AccountsView {
    let active = sel.effective_active_id();
    AccountsView {
        source,
        note,
        active_account_id: active.map(str::to_owned),
        accounts: sel
            .accounts
            .iter()
            .map(|a| {
                let b = bindings.binding(&a.id);
                AccountRow {
                    id: a.id.clone(),
                    email: a.email.clone(),
                    runtime: a.runtime.clone(),
                    organization_name: a.organization_name.clone(),
                    active: active == Some(a.id.as_str()),
                    profile: b.map(|b| b.profile.clone()),
                    binding_kind: b.map(|b| b.kind),
                    candidates: b.map(|b| b.candidates.clone()).unwrap_or_default(),
                }
            })
            .collect(),
        ties: bindings.ties.clone(),
    }
}

fn render_accounts(v: &AccountsView) -> String {
    let mut out = format!("Orca accounts ({})\n", v.source);
    if let Some(n) = &v.note {
        out.push_str(&format!("  note: {n}\n"));
    }
    if v.accounts.is_empty() {
        out.push_str("  (none)\n");
    }
    for a in &v.accounts {
        let mark = if a.active { "*" } else { " " };
        let bound = match (&a.profile, a.binding_kind) {
            (Some(p), Some(k)) if k.is_weak() => format!("→ {p} (weak: {k:?})"),
            (Some(p), _) => format!("→ {p}"),
            (None, _) if a.runtime != "host" => format!("({} account, not bound)", a.runtime),
            (None, _) => "→ (no profile)".to_owned(),
        };
        out.push_str(&format!("{mark} {:<32} {:<38} {bound}\n", a.email, a.id));
    }
    for t in &v.ties {
        out.push_str(&format!(
            "  tie: {} matches {} — using {}\n",
            t.account_id,
            t.profiles.join(", "),
            t.chosen
        ));
    }
    out
}

fn cmd_accounts(args: &[String]) -> anyhow::Result<()> {
    let (on, pos) = parse_simple("accounts", args, &["--json"])?;
    if let Some(p) = pos.first() {
        anyhow::bail!("csm orca accounts: unexpected argument '{p}'");
    }
    let json = on.iter().any(|f| f == "--json");
    let (profiles, config) = load_state("accounts")?;
    let ud = user_data("accounts", &config)?;
    let slot = slot::active_slot(&config, &profiles);
    let (sel, source, note) = match orca_mod::selection_in(&ud, READ_BUDGET) {
        OrcaState::Live(s) => (s, "live", None),
        OrcaState::Offline(s) => (
            s,
            "offline",
            Some("Orca is not running; saved state".to_owned()),
        ),
        OrcaState::Absent => {
            anyhow::bail!("csm orca accounts: no Orca data under {}", ud.display())
        }
        OrcaState::Unknown(reason) => match orca_mod::offline_selection_in(&ud) {
            Ok(Some(s)) => (
                s,
                "offline",
                Some(format!("live state unavailable ({reason}); saved state")),
            ),
            _ => anyhow::bail!("csm orca accounts: Orca's state could not be read: {reason}"),
        },
    };
    let bindings = bind::compute(&ud, &sel, &profiles, slot.as_ref(), &config.orca().bindings);
    let view = accounts_view(&sel, source, note, &bindings);
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print!("{}", render_accounts(&view));
    }
    Ok(())
}

// ─── use ──────────────────────────────────────────────────────────────────────

/// A resolved `csm orca use` target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    account_id: String,
    profile: Option<String>,
}

/// Resolve a profile name, account id, or email to an Orca account. Pure.
fn resolve_target(
    target: &str,
    sel: &Selection,
    bindings: &Bindings,
    profiles: &ProfileMap,
    slot: &Slot,
) -> Result<Target, String> {
    if slot.is_profile(target) {
        return Err(format!(
            "'{target}' is the Orca slot; name a bound profile, an email, or an account id"
        ));
    }
    if profiles.contains(target) {
        return match bindings.account_for_profile(target) {
            Some(id) => Ok(Target {
                account_id: id.to_owned(),
                profile: Some(target.to_owned()),
            }),
            None => Err(format!(
                "profile '{target}' has no Orca account (no identity match; see `csm orca accounts`)"
            )),
        };
    }
    if let Some(a) = sel.account(target) {
        return Ok(Target {
            account_id: a.id.clone(),
            profile: bindings.profile_for(&a.id).map(str::to_owned),
        });
    }
    if target.contains('@') {
        let hits: Vec<&orca_mod::Account> = sel
            .accounts
            .iter()
            .filter(|a| a.is_host() && a.email.eq_ignore_ascii_case(target))
            .collect();
        return match hits.as_slice() {
            [a] => Ok(Target {
                account_id: a.id.clone(),
                profile: bindings.profile_for(&a.id).map(str::to_owned),
            }),
            [] => Err(format!("no Orca host account has the email {target}")),
            many => Err(format!(
                "{target} matches several Orca accounts ({}); use the account id",
                many.iter()
                    .map(|a| a.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    }
    Err(format!(
        "'{target}' is not a registered profile, an Orca account id, or an email"
    ))
}

fn cmd_use(args: &[String]) -> anyhow::Result<()> {
    let (on, pos) = parse_simple("use", args, &["--queue"])?;
    let queue = on.iter().any(|f| f == "--queue");
    let [target] = pos.as_slice() else {
        anyhow::bail!("csm orca use: expected one <profile|email|account-id>");
    };
    let (profiles, config) = load_state("use")?;
    let slot = require_slot("use", &config, &profiles)?;
    let ud = user_data("use", &config)?;
    let running = is_running(&ud).is_some();

    let live = if running {
        match orca_mod::live_selection_fresh_in(&ud, READ_BUDGET) {
            OrcaState::Live(s) => Some(s),
            _ => None,
        }
    } else {
        None
    };
    let offline = orca_mod::data_file::read_in(&ud).ok().flatten();
    let sel = live
        .clone()
        .or_else(|| offline.as_ref().map(|d| d.selection.clone()))
        .context("csm orca use: Orca's accounts could not be read (live or saved)")?;
    let bindings = bind::compute(&ud, &sel, &profiles, Some(&slot), &config.orca().bindings);
    let t = resolve_target(target, &sel, &bindings, &profiles, &slot)
        .map_err(|m| anyhow::anyhow!("csm orca use: {m}"))?;

    if !running {
        if !queue {
            eprintln!(
                "csm orca use: Orca is not running; start it and retry, or pass --queue to apply \
                 this at the next `csm orca sync`"
            );
            std::process::exit(1);
        }
        if !cfg!(unix) {
            // A queued select could never apply here (no select transport).
            anyhow::bail!(
                "csm orca use --queue: Orca account selection is not supported on this platform"
            );
        }
        let Some(profile) = t.profile.clone() else {
            anyhow::bail!(
                "csm orca use --queue: account {} has no bound profile to queue",
                t.account_id
            );
        };
        let prior = offline
            .as_ref()
            .and_then(|d| d.selection.effective_active_id())
            .map(str::to_owned);
        let now = crate::epoch::now_secs() as i64;
        pending::write(&PendingSelect::new(
            &t.account_id,
            &profile,
            now,
            prior.as_deref(),
        ))
        .context("csm orca use: failed to write the pending select")?;
        cas::write_default_profile(&profile, &profiles)?;
        println!(
            "Queued: Orca will switch to {} at the next `csm orca sync`; csm default → {profile}.",
            t.account_id
        );
        return Ok(());
    }

    match orca_mod::select_account(&t.account_id, &slot.dir, SELECT_BUDGET) {
        Ok(SelectOutcome::Selected) => {
            let _ = pending::clear();
            if let Some(p) = &t.profile {
                cas::write_default_profile(p, &profiles)?;
            }
            let email = sel
                .account(&t.account_id)
                .map(|a| a.email.clone())
                .unwrap_or_default();
            match &t.profile {
                Some(p) => println!(
                    "Orca active → {email} ({}); csm default → {p}.",
                    t.account_id
                ),
                None => println!(
                    "Orca active → {email} ({}); no bound profile, csm default unchanged.",
                    t.account_id
                ),
            }
            Ok(())
        }
        Ok(SelectOutcome::Unknown) => {
            eprintln!(
                "csm orca use: Orca did not confirm the switch within {}s; it may still apply \
                 (check with `csm orca status`). csm's default was not changed.",
                SELECT_BUDGET.as_secs()
            );
            std::process::exit(1);
        }
        Err(e @ SelectError::RuntimeDirMismatch { .. }) => {
            anyhow::bail!("csm orca use: {e} (see `csm orca status`)")
        }
        Err(e) => anyhow::bail!("csm orca use: {e}"),
    }
}

// ─── sync ─────────────────────────────────────────────────────────────────────

/// What `sync` should do with a pending select. Pure.
#[derive(Debug, Clone, PartialEq)]
enum PendingAction {
    /// Nothing queued.
    None,
    /// Select it in Orca.
    Apply(PendingSelect),
    /// Delete it; `note` explains why (`None` = silently, already applied).
    Drop(Option<String>),
    /// Leave it for a later sync.
    Keep,
}

fn pending_action(
    p: Option<Result<PendingSelect, String>>,
    live_effective: Option<Option<&str>>,
    now: i64,
) -> PendingAction {
    let p = match p {
        None => return PendingAction::None,
        Some(Err(e)) => return PendingAction::Drop(Some(format!("unreadable ({e})"))),
        Some(Ok(p)) => p,
    };
    match live_effective {
        // Orca not live: only age can be judged.
        None => match pending::verdict(&p, p.expected_prior_active_id.as_deref(), now) {
            Verdict::Expired => PendingAction::Drop(Some("older than 24h".to_owned())),
            _ => PendingAction::Keep,
        },
        Some(active) => match pending::verdict(&p, active, now) {
            Verdict::Valid => PendingAction::Apply(p),
            Verdict::AlreadyApplied => PendingAction::Drop(None),
            Verdict::Expired => PendingAction::Drop(Some("older than 24h".to_owned())),
            Verdict::Superseded => PendingAction::Drop(Some(
                "Orca's active account changed since it was queued".to_owned(),
            )),
        },
    }
}

fn cmd_sync(args: &[String]) -> anyhow::Result<()> {
    let quiet = args.iter().any(|a| a == "--quiet");
    let say = |m: String| {
        if !quiet {
            println!("{m}");
        }
    };
    let warn = |m: String| {
        if !quiet {
            eprintln!("csm orca sync: {m}");
        }
    };
    // Always exit 0: this runs detached from `csm run`.
    let (profiles, config) = match load_state("sync") {
        Ok(s) => s,
        Err(e) => {
            warn(format!("{e:#}"));
            return Ok(());
        }
    };
    let Some(slot) = slot::active_slot(&config, &profiles) else {
        say("Orca mode is off.".to_owned());
        return Ok(());
    };
    let Some(ud) = orca_mod::user_data_dir_for(config.orca()) else {
        warn("Orca's userData dir could not be resolved".to_owned());
        return Ok(());
    };
    let now = crate::epoch::now_secs() as i64;
    let state = orca_mod::live_selection_fresh_in(&ud, READ_BUDGET);
    let live = match &state {
        OrcaState::Live(s) => Some(s.clone()),
        _ => None,
    };
    let pend = match pending::read() {
        Ok(p) => p.map(Ok),
        Err(e) => Some(Err(e.to_string())),
    };
    let mut active: Option<String> = live
        .as_ref()
        .and_then(Selection::effective_active_id)
        .map(str::to_owned);
    // `true` while a queued select survives this sync unapplied: `csm run`
    // follows that queued profile, so mirroring Orca's (old) active account
    // into the default here would fight it on every launch.
    let mut pending_still_queued = false;
    match pending_action(pend, live.as_ref().map(|s| s.effective_active_id()), now) {
        PendingAction::None => {}
        PendingAction::Keep => pending_still_queued = true,
        PendingAction::Drop(note) => {
            let _ = pending::clear();
            if let Some(n) = note {
                warn(format!("dropped the queued Orca selection: {n}"));
            }
        }
        PendingAction::Apply(p) => {
            match orca_mod::select_account(&p.account_id, &slot.dir, SELECT_BUDGET) {
                Ok(SelectOutcome::Selected) => {
                    let _ = pending::clear();
                    active = Some(p.account_id.clone());
                    say(format!(
                        "Applied the queued Orca selection ({}).",
                        p.account_id
                    ));
                }
                Ok(SelectOutcome::Unknown) => {
                    pending_still_queued = true;
                    warn("Orca has not confirmed the queued selection yet; will retry".to_owned());
                }
                Err(SelectError::Rejected(m)) => {
                    let _ = pending::clear();
                    warn(format!("Orca rejected the queued selection ({m}); dropped"));
                }
                Err(e) => {
                    pending_still_queued = true;
                    warn(format!("queued selection not applied: {e}"));
                }
            }
        }
    }

    let Some(sel) = live else {
        match state {
            OrcaState::Unknown(r) => warn(format!("Orca's state could not be read: {r}")),
            _ => say("Orca is not running.".to_owned()),
        }
        return Ok(());
    };
    let Some(active) = active else {
        say("Orca active: System default (no managed account).".to_owned());
        return Ok(());
    };
    let bindings = bind::compute(&ud, &sel, &profiles, Some(&slot), &config.orca().bindings);
    let default = profiles.default_name();
    let email = sel
        .account(&active)
        .map(|a| a.email.clone())
        .unwrap_or_default();
    let bound = bindings.profile_for_active(&active, Some(&default));
    match plan_mirror(bound, &default, &slot, pending_still_queued) {
        MirrorPlan::Write(p) => match cas::write_default_profile(p, &profiles) {
            Ok(()) => say(format!("Orca active: {email} → {p} (csm default updated)")),
            Err(e) => warn(format!("could not update csm's default: {e}")),
        },
        MirrorPlan::Leave(p) => say(format!("Orca active: {email} → {p}")),
        MirrorPlan::HeldByPending(p) => say(format!(
            "Orca active: {email} → {p} (csm default left as-is: a queued selection is pending)"
        )),
        MirrorPlan::Unbound => say(format!(
            "Orca active: {email} ({active}) → no bound profile"
        )),
    }
    Ok(())
}

/// What `sync` does with csm's default after reading Orca's active account.
#[derive(Debug, PartialEq, Eq)]
enum MirrorPlan<'a> {
    /// Write this profile as csm's default.
    Write(&'a str),
    /// Already the default, or the slot: report only.
    Leave(&'a str),
    /// A queued select is still pending: `csm run` follows it, so the default
    /// is not moved back to Orca's pre-queue account.
    HeldByPending(&'a str),
    /// Orca's active account has no bound profile.
    Unbound,
}

/// Decide the default-mirror step of `sync`. Pure.
fn plan_mirror<'a>(
    bound: Option<&'a str>,
    default: &str,
    slot: &Slot,
    pending_still_queued: bool,
) -> MirrorPlan<'a> {
    match bound {
        None => MirrorPlan::Unbound,
        Some(p) if p == default || slot.is_profile(p) => MirrorPlan::Leave(p),
        Some(p) if pending_still_queued => MirrorPlan::HeldByPending(p),
        Some(p) => MirrorPlan::Write(p),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::Account;
    use crate::orca::bind::Binding;
    use std::collections::HashMap;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn sync_mirror_is_held_while_a_queued_select_is_pending() {
        let slot = Slot {
            name: "orca".into(),
            dir: "/Users/example/.claude.orca".into(),
        };
        // Pending kept → no mirror to Orca's old active account.
        assert_eq!(
            plan_mirror(Some("home"), "work", &slot, true),
            MirrorPlan::HeldByPending("home")
        );
        assert_eq!(
            plan_mirror(Some("home"), "work", &slot, false),
            MirrorPlan::Write("home")
        );
        assert_eq!(
            plan_mirror(Some("work"), "work", &slot, false),
            MirrorPlan::Leave("work")
        );
        assert_eq!(
            plan_mirror(Some("orca"), "work", &slot, false),
            MirrorPlan::Leave("orca")
        );
        assert_eq!(plan_mirror(None, "work", &slot, true), MirrorPlan::Unbound);
    }

    fn reg(pairs: &[(&str, &str)]) -> ProfileMap {
        ProfileMap(
            pairs
                .iter()
                .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
                .collect::<HashMap<_, _>>(),
        )
    }

    fn email_id(e: &str) -> Identity {
        Identity {
            email: Some(e.into()),
            ..Default::default()
        }
    }

    #[test]
    fn init_flags() {
        let f =
            parse_init_flags(&s(&["--slot", "o2", "--dir=/Users/example/.o2", "--force"])).unwrap();
        assert_eq!(f.slot.as_deref(), Some("o2"));
        assert_eq!(f.dir.as_deref(), Some("/Users/example/.o2"));
        assert!(f.force && !f.no_floor);
        assert!(parse_init_flags(&s(&["--bogus"])).is_err());
        assert!(parse_init_flags(&s(&["--slot"])).is_err());
        assert!(parse_init_flags(&s(&["extra"])).is_err());
        assert_eq!(parse_init_flags(&[]).unwrap(), InitFlags::default());
    }

    #[test]
    fn former_runtime_dir_order() {
        let h = "/Users/example/.claude".to_owned();
        let d = |p: Option<&str>, l: Option<&str>, f: Option<&str>| {
            former_runtime_dir(
                p.map(Into::into),
                l.map(Into::into),
                f.map(Into::into),
                h.clone(),
            )
            .0
        };
        assert_eq!(d(Some("/p"), Some("/l"), Some("/f")), "/p");
        assert_eq!(d(None, Some("/l"), Some("/f")), "/l");
        assert_eq!(d(None, None, Some("/f")), "/f");
        assert_eq!(d(None, None, None), h);
    }

    #[test]
    fn affected_profiles_by_dir_and_identity() {
        let profiles = vec![
            (
                "work".to_owned(),
                "/Users/example/.claude.work".to_owned(),
                Some(email_id("alice@example.com")),
            ),
            (
                "home".to_owned(),
                "/Users/example/.claude.home".to_owned(),
                Some(email_id("bob@example.com")),
            ),
            (
                "alias".to_owned(),
                "/Users/example/.claude.alias".to_owned(),
                Some(email_id("ALICE@example.com")),
            ),
            (
                "orca".to_owned(),
                "/Users/example/.claude.work".to_owned(),
                None,
            ),
        ];
        let got = affected_profiles(
            "/Users/example/.claude.work/",
            Some(&email_id("alice@example.com")),
            &profiles,
            "orca",
        );
        assert_eq!(got, vec!["alias", "work"]);
        assert!(affected_profiles("/Users/example/.claude", None, &profiles, "orca").is_empty());
    }

    #[test]
    fn slot_registration_refusals() {
        let p = reg(&[
            ("work", "/Users/example/.claude.work"),
            ("orca", "/Users/example/.claude.orca"),
        ]);
        let ok =
            |n, d, id: Option<&Identity>, already| slot_registration_refusal(n, d, &p, id, already);
        assert_eq!(
            ok("orca", "/Users/example/.claude.orca", None, false),
            None,
            "re-register same dir"
        );
        assert!(
            ok("orca", "/Users/example/.claude.o2", None, false).is_some(),
            "re-point"
        );
        assert!(
            ok("o2", "/Users/example/.claude.work", None, false).is_some(),
            "shared dir"
        );
        assert!(ok("bad name", "/Users/example/.x", None, false).is_some());
        let id = email_id("alice@example.com");
        assert!(
            ok("o2", "/Users/example/.claude.o2", Some(&id), false).is_some(),
            "logged in"
        );
        assert_eq!(
            ok("orca", "/Users/example/.claude.orca", Some(&id), true),
            None,
            "re-run: Orca materialized an identity into the slot"
        );
    }

    fn sel() -> Selection {
        let acct = |id: &str, email: &str, rt: &str| Account {
            id: id.into(),
            email: email.into(),
            organization_uuid: None,
            organization_name: None,
            runtime: rt.into(),
        };
        Selection {
            accounts: vec![
                acct("acct-1", "alice@example.com", "host"),
                acct("acct-2", "bob@example.com", "host"),
                acct("acct-3", "bob@example.com", "host"),
                acct("acct-w", "carol@example.com", "wsl"),
            ],
            active_id: Some("acct-1".into()),
            host_active_id: None,
            rate_limits: None,
        }
    }

    fn bindings() -> Bindings {
        let mut b = Bindings::default();
        b.by_account.insert(
            "acct-1".into(),
            Binding {
                profile: "work".into(),
                kind: bind::MatchKind::Uuid,
                via_override: false,
                candidates: vec!["work".into()],
            },
        );
        b
    }

    #[test]
    fn target_resolution() {
        let p = reg(&[
            ("work", "/Users/example/.claude.work"),
            ("home", "/Users/example/.claude.home"),
            ("orca", "/Users/example/.claude.orca"),
        ]);
        let slot = Slot {
            name: "orca".into(),
            dir: "/Users/example/.claude.orca".into(),
        };
        let (sel, b) = (sel(), bindings());
        let r = |t| resolve_target(t, &sel, &b, &p, &slot);
        assert_eq!(
            r("work"),
            Ok(Target {
                account_id: "acct-1".into(),
                profile: Some("work".into())
            })
        );
        assert!(r("home").is_err(), "unbound profile");
        assert!(r("orca").is_err(), "the slot is never a target");
        assert_eq!(r("acct-2").unwrap().profile, None);
        assert_eq!(r("ALICE@example.com").unwrap().account_id, "acct-1");
        assert!(r("bob@example.com").unwrap_err().contains("several"));
        assert!(
            r("carol@example.com").is_err(),
            "wsl accounts are not host accounts"
        );
        assert!(r("nobody").is_err());
    }

    #[test]
    fn accounts_view_marks_active_and_bindings() {
        let v = accounts_view(&sel(), "live", None, &bindings());
        assert_eq!(v.active_account_id.as_deref(), Some("acct-1"));
        assert!(v.accounts[0].active);
        assert_eq!(v.accounts[0].profile.as_deref(), Some("work"));
        assert_eq!(v.accounts[1].profile, None);
        let text = render_accounts(&v);
        assert!(text.contains("* alice@example.com"), "{text}");
        assert!(text.contains("(wsl account, not bound)"), "{text}");
    }

    #[test]
    fn pending_actions() {
        let p = PendingSelect::new("acct-2", "home", 1_000, Some("acct-1"));
        assert_eq!(
            pending_action(None, Some(Some("acct-1")), 1_010),
            PendingAction::None
        );
        assert_eq!(
            pending_action(Some(Ok(p.clone())), Some(Some("acct-1")), 1_010),
            PendingAction::Apply(p.clone())
        );
        assert_eq!(
            pending_action(Some(Ok(p.clone())), Some(Some("acct-2")), 1_010),
            PendingAction::Drop(None)
        );
        assert!(matches!(
            pending_action(Some(Ok(p.clone())), Some(Some("acct-9")), 1_010),
            PendingAction::Drop(Some(_))
        ));
        assert_eq!(
            pending_action(Some(Ok(p.clone())), None, 1_010),
            PendingAction::Keep
        );
        assert!(matches!(
            pending_action(Some(Ok(p)), None, 1_000 + pending::MAX_AGE_SECS),
            PendingAction::Drop(Some(_))
        ));
        assert!(matches!(
            pending_action(Some(Err("bad".into())), None, 0),
            PendingAction::Drop(Some(_))
        ));
    }

    /// End-to-end `init` → `disable` in an isolated home (no Orca, no real
    /// launchd: the setters are inert under cfg(test)).
    #[test]
    fn init_then_disable_in_an_isolated_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        // Pin Orca's userData inside the temp home on every OS (the Linux /
        // Windows defaults read XDG_CONFIG_HOME / APPDATA, not the test home).
        let ud = home.join("orca-user-data").to_string_lossy().into_owned();
        crate::testenv::with_env_var("ORCA_USER_DATA_PATH", Some(&ud), || {
            crate::testenv::with_test_home(&home, || {
                let work = home.join(".claude.work").to_string_lossy().into_owned();
                let mut p = reg(&[("work", &work)]);
                p.save().unwrap();
                cas::write_default_profile("work", &p).unwrap();

                cmd_init(&[]).unwrap();
                p = ProfileMap::load().unwrap();
                let slot_dir = home.join(".claude.orca").to_string_lossy().into_owned();
                assert_eq!(p.get("orca"), Some(slot_dir.as_str()));
                let cfg = Config::load().unwrap();
                assert_eq!(cfg.orca().slot_profile.as_deref(), Some("orca"));
                assert_eq!(
                    cas::floor_dir(&p, &cfg),
                    PathBuf::from(&slot_dir),
                    "floor → slot"
                );
                assert_eq!(
                    std::fs::read_to_string(crate::paths::floor_dir_file()).unwrap(),
                    format!("{slot_dir}\n")
                );
                assert_eq!(p.default_name(), "work", "the shell default is untouched");

                // Idempotent.
                cmd_init(&[]).unwrap();

                cmd_disable(&[]).unwrap();
                let cfg = Config::load().unwrap();
                assert_eq!(cfg.orca().slot_profile, None);
                assert!(
                    ProfileMap::load().unwrap().contains("orca"),
                    "slot stays registered"
                );
                assert_eq!(
                    std::fs::read_to_string(crate::paths::floor_dir_file()).unwrap(),
                    format!("{work}\n")
                );
            })
        });
    }
}
