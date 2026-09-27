//! `csm accounts` — Orca's Claude accounts, from the terminal.
//!
//! `list | use <id|prefix|email> | add | import <dir>... | rm <id|prefix|email>
//! | doctor [--fix] [--offline]`. csm keeps no registry: every verb reads
//! Orca's account list (RPC when Orca runs, else the store) and changes it
//! through `orca::switch` / `orca::add`, which go over Orca's RPC when Orca
//! runs and through the offline store-write protocol when it does not.
//!
//! Output names accounts by id, email and organization only. No verb prints
//! a token, a credential or a refresh-token fingerprint's source text; the
//! doctor prints quarantine fingerprints (a truncated hash), never grants.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context as _, bail};

use crate::account::accounts::{AccountEntry, Lookup, find, label_for};
use crate::orca::AccountSource;
use crate::orca::add::{self, AccountChange, SystemClaude};
use crate::orca::context::Context;
use crate::orca::http::{OauthHttp, ProfileAnswer, SystemHttp, parse_profile};
use crate::orca::live::{ProcFacts, SystemProcs};
use crate::orca::quarantine::{self, Quarantine, Reason};
use crate::orca::readback::access_token;
use crate::orca::record::AccountRecord;
use crate::orca::runtime::{OauthIdentity, UuidMatch};
use crate::orca::stash::Stash;
use crate::orca::switch::{self, Outcome};
use crate::orca::userdata::claude_accounts_root;
use crate::orca::{OrcaView, SecretString, SnapshotOptions};

// ─── argument parsing (pure) ──────────────────────────────────────────────────

/// A parsed `csm accounts` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountsCmd {
    List,
    Use(String),
    Add,
    Import(Vec<PathBuf>),
    Rm(String),
    Doctor { fix: bool, offline: bool },
    Help,
}

/// Parse the words after `accounts`. Pure.
pub(crate) fn parse(args: &[OsString]) -> anyhow::Result<AccountsCmd> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let (verb, rest) = match words.split_first() {
        None => return Ok(AccountsCmd::List),
        Some((v, r)) => (v.as_str(), r),
    };
    let one = |what: &str| -> anyhow::Result<String> {
        match rest {
            [x] if !x.starts_with('-') => Ok(x.clone()),
            _ => bail!("csm accounts {verb}: expected exactly one {what}"),
        }
    };
    Ok(match verb {
        "list" | "ls" if rest.is_empty() => AccountsCmd::List,
        "use" => AccountsCmd::Use(one("account (id, id prefix or email)")?),
        "add" if rest.is_empty() => AccountsCmd::Add,
        "import" => {
            if rest.is_empty() || rest.iter().any(|d| d.starts_with('-')) {
                bail!("csm accounts import: expected one or more config dirs");
            }
            AccountsCmd::Import(rest.iter().map(PathBuf::from).collect())
        }
        "rm" | "remove" => AccountsCmd::Rm(one("account (id, id prefix or email)")?),
        "doctor" => {
            let mut fix = false;
            let mut offline = false;
            for f in rest {
                match f.as_str() {
                    "--fix" => fix = true,
                    "--offline" => offline = true,
                    other => bail!("csm accounts doctor: unknown flag {other:?}"),
                }
            }
            AccountsCmd::Doctor { fix, offline }
        }
        "-h" | "--help" | "help" => AccountsCmd::Help,
        other => {
            bail!("csm accounts: unknown verb {other:?} (list | use | add | import | rm | doctor)")
        }
    })
}

/// `csm accounts …`
pub(crate) fn cmd_accounts(args: &[OsString]) -> anyhow::Result<()> {
    match parse(args)? {
        AccountsCmd::List => list(),
        AccountsCmd::Use(q) => use_account(&q),
        AccountsCmd::Add => add_account(),
        AccountsCmd::Import(dirs) => import(&dirs),
        AccountsCmd::Rm(q) => remove(&q),
        AccountsCmd::Doctor { fix, offline } => doctor(fix, offline),
        AccountsCmd::Help => {
            print_help();
            Ok(())
        }
    }
}

fn print_help() {
    println!("csm accounts — Orca's Claude accounts\n");
    println!("  csm accounts [list]                  list accounts (* active, D = D's account)");
    println!("  csm accounts use <id|prefix|email>   switch the active account");
    println!("  csm accounts add                     log in a new account");
    println!("  csm accounts import <dir>...         import the login held by config dirs");
    println!("  csm accounts rm <id|prefix|email>    remove a non-active account");
    println!("  csm accounts doctor [--fix] [--offline]");
}

// ─── shared helpers ───────────────────────────────────────────────────────────

fn entries(records: &[AccountRecord]) -> Vec<AccountEntry> {
    records
        .iter()
        .filter(|a| a.is_host())
        .map(|a| AccountEntry {
            id: a.id.clone(),
            email: a.email.clone(),
            organization_name: a.organization_name.clone(),
            managed_auth_path: a.managed_auth_path.clone(),
        })
        .collect()
}

/// Resolve `query` to one host account id, or explain why not. Pure.
pub(crate) fn resolve(accounts: &[AccountEntry], query: &str) -> anyhow::Result<String> {
    match find(accounts, query) {
        Lookup::Found(e) => Ok(e.id.clone()),
        Lookup::NotFound => bail!("no Claude account matches {query:?} (see `csm accounts`)"),
        Lookup::Ambiguous(ids) => bail!(
            "{query:?} matches more than one account: {}",
            ids.join(", ")
        ),
    }
}

fn view() -> anyhow::Result<OrcaView> {
    crate::orca::snapshot(&SnapshotOptions::default()).context("csm accounts")
}

fn d_account(view: &OrcaView) -> Option<&str> {
    match view.runtime_account.as_ref()?.account.as_ref()? {
        UuidMatch::Unique(id) => Some(id.as_str()),
        _ => None,
    }
}

// ─── list ─────────────────────────────────────────────────────────────────────

/// The account table. Pure.
pub(crate) fn render_list(
    accounts: &[AccountEntry],
    active: Option<&str>,
    in_d: Option<&str>,
) -> String {
    if accounts.is_empty() {
        return "(no Claude accounts in Orca — `csm accounts add`)\n".to_owned();
    }
    let email_w = accounts
        .iter()
        .map(|a| a.email.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(1);
    let mut out = String::new();
    for a in accounts {
        let mark = if active == Some(a.id.as_str()) {
            '*'
        } else {
            ' '
        };
        let d = if in_d == Some(a.id.as_str()) {
            'D'
        } else {
            ' '
        };
        let email = a.email.as_deref().unwrap_or("-");
        let org = a.organization_name.as_deref().unwrap_or("");
        out.push_str(
            format!("{mark}{d} {email:<email_w$}  {}  {org}", a.id)
                .trim_end()
                .trim_end(),
        );
        out.push('\n');
    }
    out
}

fn list() -> anyhow::Result<()> {
    let v = view()?;
    if let Some(e) = &v.store_error {
        eprintln!("csm: warning: Orca's store: {e}");
    }
    let accts = entries(&v.accounts);
    print!(
        "{}",
        render_list(&accts, v.active_id.as_deref(), d_account(&v))
    );
    Ok(())
}

// ─── use ──────────────────────────────────────────────────────────────────────

fn route_name(r: switch::Route) -> &'static str {
    match r {
        switch::Route::Rpc => "via Orca",
        switch::Route::Offline => "offline",
        switch::Route::OfflineThenRpc => "offline, then via Orca",
        switch::Route::Noop => "no change",
    }
}

/// The lines naming the claude sessions in `D` that a manual switch moves
/// to `label`. Empty when none may be live. Pure.
fn live_session_lines(scan: &crate::orca::runtime::SessionScan, label: &str) -> Vec<String> {
    let mut out: Vec<String> = scan
        .live
        .iter()
        .map(|r| {
            let sid = r
                .session_id
                .as_deref()
                .map(|s| format!(", session {}", s.get(..8).unwrap_or(s)))
                .unwrap_or_default();
            format!("csm: live claude (pid {}{sid}) moves to {label}", r.pid)
        })
        .collect();
    let unsure = scan.unverifiable.len() + scan.unreadable;
    if unsure > 0 {
        out.push(format!(
            "csm: {unsure} more session record(s) in D may be live and would move to {label}"
        ));
    }
    out
}

fn use_account(query: &str) -> anyhow::Result<()> {
    let v = view()?;
    let accts = entries(&v.accounts);
    let id = resolve(&accts, query)?;
    let label = label_for(
        &id,
        accts
            .iter()
            .find(|a| a.id == id)
            .and_then(|a| a.email.as_deref()),
    );
    let procs = SystemProcs;
    let ctx = Context::current(&procs)?;
    // Like Orca's GUI, a manual switch never stops sessions: it names the
    // live ones in `D` that move with it, then proceeds (design section 4).
    if v.active_id.as_deref() != Some(id.as_str()) {
        let domain = crate::orca::runtime::this_pid_domain(ctx.os());
        if let Ok(scan) = crate::orca::runtime::scan_sessions(
            &ctx.paths.config_dir.join("sessions"),
            &domain,
            &procs,
        ) {
            for line in live_session_lines(&scan, &label) {
                eprintln!("{line}");
            }
        }
    }
    let http = SystemHttp::from_env();
    let report = ctx.with_switch_env(&procs, &http, |env| switch::switch(env, &id))?;
    match report.outcome {
        Outcome::Switched => {
            println!("csm: switched to {label} ({})", route_name(report.route));
            Ok(())
        }
        Outcome::AlreadyActive => {
            println!("csm: {label} is already active");
            Ok(())
        }
        Outcome::Uncertain(why) => bail!(
            "csm accounts use: the switch to {label} did not verify ({why}); run `csm accounts doctor`"
        ),
    }
}

// ─── add / import / rm ────────────────────────────────────────────────────────

/// How a finished add, import or rm reads, and whether it counts as done.
/// A redo Orca refused did not happen, and one it did not confirm may not
/// have: neither is reported as `verb` or exits 0. `Err` holds the line to
/// print before the non-zero exit. Pure.
fn change_outcome(verb: &str, c: &AccountChange) -> Result<String, String> {
    let who = match (&c.email, &c.id) {
        (Some(e), Some(id)) => format!("{e} ({id})"),
        (Some(e), None) => e.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => "the account".to_owned(),
    };
    let route = match c.route {
        add::Route::Rpc => "via Orca",
        add::Route::Offline => "offline",
        add::Route::OfflineThenRpc => "offline, then via Orca",
    };
    let leftover = c
        .leftover
        .as_ref()
        .map(|l| format!("; left for `csm accounts doctor`: {l}"))
        .unwrap_or_default();
    match &c.redo {
        Some(crate::orca::store::RedoOutcome::Failed(why)) => Err(format!(
            "csm: {who} was not {verb}: Orca refused it ({why}){leftover}"
        )),
        Some(crate::orca::store::RedoOutcome::Uncertain(why)) => Err(format!(
            "csm: {who} may not be {verb}: it was handed to Orca, which did not confirm it \
             ({why}); check `csm accounts doctor`{leftover}"
        )),
        _ => Ok(format!("csm: {verb} {who} ({route}){leftover}")),
    }
}

/// Print a finished change; `Err` (non-zero exit) when Orca refused or did
/// not confirm it ([`change_outcome`]).
fn report_change(cmd: &str, verb: &str, c: &AccountChange) -> anyhow::Result<()> {
    match change_outcome(verb, c) {
        Ok(line) => {
            println!("{line}");
            Ok(())
        }
        Err(line) => {
            eprintln!("{line}");
            bail!("csm accounts {cmd}: Orca did not confirm the change")
        }
    }
}

fn add_account() -> anyhow::Result<()> {
    let procs = SystemProcs;
    let ctx = Context::current(&procs)?;
    if ctx.orca_running(&procs) {
        // Orca's own flow (`orca account add --agent claude`) keeps Orca's
        // state in Orca's hands.
        let cli = find_orca_cli(&ctx, &procs)?;
        return exec_orca_add(&cli);
    }
    let cli = SystemClaude::configured().context("csm accounts add")?;
    let change = ctx.with_accounts_env(&procs, |env| add::login_add(env, &cli))?;
    report_change("add", "added", &change)
}

/// The running Orca's own CLI ([`add::orca_cli_candidates`]): its bundled
/// launcher first, then the install locations, then `PATH`.
fn find_orca_cli(ctx: &Context, procs: &dyn ProcFacts) -> anyhow::Result<PathBuf> {
    let main_exe = crate::orca::live::check(ctx.os(), &ctx.user_data.dir, procs).main_exe;
    let mut installs = crate::orca::version::install_candidates(&ctx.env, main_exe.as_deref());
    if ctx.os() == crate::orca::HostOs::Linux && !cfg!(test) {
        installs.extend(add::LINUX_INSTALL_DIRS.iter().map(PathBuf::from));
    }
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    add::orca_cli_candidates(ctx.os(), main_exe.as_deref(), &installs, &path_dirs)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "csm accounts add: Orca is running, but neither its bundled CLI nor `{}` on PATH was found; add the account in Orca's settings",
                add::orca_cli_name(ctx.os())
            )
        })
}

/// Hand the terminal to `<orca cli> account add --agent claude`.
#[cfg(all(unix, not(test)))]
fn exec_orca_add(cli: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt as _;
    let err = std::process::Command::new(cli)
        .args(add::ORCA_ADD_ARGV)
        .exec();
    Err(anyhow::Error::new(err).context(format!("csm accounts add: cannot run {}", cli.display())))
}

#[cfg(all(not(unix), not(test)))]
fn exec_orca_add(cli: &std::path::Path) -> anyhow::Result<()> {
    let status = std::process::Command::new(cli)
        .args(add::ORCA_ADD_ARGV)
        .status()
        .with_context(|| format!("csm accounts add: cannot run {}", cli.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
fn exec_orca_add(_: &std::path::Path) -> anyhow::Result<()> {
    bail!("cfg(test): tests never run the real orca")
}

fn import(dirs: &[PathBuf]) -> anyhow::Result<()> {
    let procs = SystemProcs;
    let ctx = Context::current(&procs)?;
    let cli = SystemClaude::configured().context("csm accounts import")?;
    let mut failed = 0usize;
    for dir in dirs {
        match ctx.with_accounts_env(&procs, |env| add::import(env, &cli, dir, None)) {
            Ok(c) => match change_outcome("imported", &c) {
                Ok(line) => println!("{line}"),
                Err(line) => {
                    failed += 1;
                    eprintln!("{line}");
                }
            },
            Err(e) => {
                failed += 1;
                eprintln!("csm accounts import: {}: {e}", dir.display());
            }
        }
    }
    if failed > 0 {
        bail!(
            "csm accounts import: {failed} of {} dirs failed",
            dirs.len()
        );
    }
    Ok(())
}

fn remove(query: &str) -> anyhow::Result<()> {
    let v = view()?;
    let accts = entries(&v.accounts);
    let id = resolve(&accts, query)?;
    if v.active_id.as_deref() == Some(id.as_str()) {
        bail!("csm accounts rm: {id} is the active account; switch to another first");
    }
    let procs = SystemProcs;
    let ctx = Context::current(&procs)?;
    let change = ctx.with_accounts_env(&procs, |env| add::remove(env, &id))?;
    report_change("rm", "removed", &change)
}

// ─── doctor ───────────────────────────────────────────────────────────────────

/// One quarantine entry, secret-free.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct QEntry {
    pub fingerprint: String,
    pub reason: String,
    pub source: String,
    pub matched: Option<String>,
    /// The grant's access-token expiry, ms since the epoch.
    pub expires_at_ms: Option<i64>,
    /// The profile endpoint's answer when the entry was filed: its HTTP
    /// status and the account uuid it named.
    pub profile_status: Option<u16>,
    pub profile_account_uuid: Option<String>,
    /// A grant the migration's settle files into its account's stash (a
    /// retired dir's, one settle's refresh or the cutover returned) that is
    /// fresher than that stash ([`crate::migrate::settle_wanted`]): settle
    /// still waits for it, which it does while Orca runs.
    pub settle_waiting: bool,
}

/// The parenthesised details of a quarantine line: reason, source, expiry,
/// profile answer, matched account and whether that account still exists.
/// Pure.
fn qentry_details(q: &QEntry, accounts: &[String]) -> String {
    let mut parts = vec![q.reason.clone(), format!("from {}", q.source)];
    if let Some(ms) = q.expires_at_ms {
        parts.push(match chrono::DateTime::from_timestamp_millis(ms) {
            Some(t) => format!("expires {}", t.format("%Y-%m-%d %H:%M UTC")),
            None => format!("expires {ms}"),
        });
    }
    match (q.profile_status, q.profile_account_uuid.as_deref()) {
        (Some(st), Some(u)) => parts.push(format!("profile {st} {u}")),
        (Some(st), None) => parts.push(format!("profile {st}")),
        (None, Some(u)) => parts.push(format!("profile {u}")),
        (None, None) => {}
    }
    if let Some(m) = q.matched.as_deref() {
        let gone = if accounts.iter().any(|a| a == m) {
            ""
        } else {
            " (account removed)"
        };
        parts.push(format!("matched {m}{gone}"));
    }
    parts.join(", ")
}

/// Everything the doctor looked at. Secret-free: grants appear only as
/// quarantine fingerprints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DoctorFacts {
    pub running: bool,
    /// The account list came from the store (Orca stopped) — the only case an
    /// orphan stash may be removed offline.
    pub from_store: bool,
    /// The profile keeps its state in SQLite (Orca 1.4.214+): a store read
    /// is Orca's export, which lags accounts added since Orca's last clean
    /// quit, so an orphan found through it is not proof.
    pub sqlite_state: bool,
    /// `switch.json` names an unfinished switch.
    pub pending_journal: bool,
    pub quarantine: Vec<QEntry>,
    /// Every account id Orca's list names (host and WSL).
    pub accounts: Vec<String>,
    /// (account id, fingerprint of its stashed grant).
    pub stash_fingerprints: Vec<(String, String)>,
    /// Stash dirs no record names.
    pub orphans: Vec<String>,
    /// Accounts whose grant the profile endpoint attributes to another uuid.
    pub profile_other: Vec<String>,
    /// Accounts whose access token the profile endpoint rejected.
    pub profile_dead: Vec<String>,
    /// Stashes that could not be read (id, reason).
    pub unreadable: Vec<(String, String)>,
    /// `D`'s account (unique match), and whether its uuid matched several.
    pub d_account: Option<String>,
    pub d_ambiguous: bool,
    pub active: Option<String>,
    /// Orca main's `D` vs csm's.
    pub dir_agrees: Option<bool>,
    /// The `claude` alias (`csm orca setup`) points at a file that is gone.
    pub alias_dangling: Option<PathBuf>,
}

/// A repair the doctor can make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fix {
    /// Recover the unfinished switch.
    Recover,
    /// Drop a quarantine entry whose grant a stash already holds.
    PurgeQuarantine { fingerprint: String, holder: String },
    /// Quarantine an orphan stash's grant, then remove the stash.
    RemoveOrphan(String),
    /// Point the dangling `claude` alias at this csm again.
    RepairAlias,
}

/// One doctor line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub text: String,
    pub fix: Option<Fix>,
}

/// The findings for `f`. Pure.
pub(crate) fn findings(f: &DoctorFacts) -> Vec<Finding> {
    let mut out = Vec::new();
    let push =
        |out: &mut Vec<Finding>, text: String, fix: Option<Fix>| out.push(Finding { text, fix });
    if f.pending_journal {
        push(
            &mut out,
            "an unfinished switch is pending (switch.json)".into(),
            Some(Fix::Recover),
        );
    }
    let stash_fps: BTreeMap<&str, &str> = f
        .stash_fingerprints
        .iter()
        .map(|(id, fp)| (fp.as_str(), id.as_str()))
        .collect();
    for q in &f.quarantine {
        let held = stash_fps.get(q.fingerprint.as_str());
        // A pre-login copy that stayed: an `accounts add` login stopped
        // before its cleanup, so the unscoped item may still hold the new
        // login's grant beside D's identity, which Orca's read-back files
        // under the active account.
        let interrupted = if held.is_none() && q.reason == "pre-login" {
            format!(
                ": an `accounts add` login was interrupted; if the {} Keychain item now holds \
                 another grant, put this one back there before Orca starts",
                crate::orca::keychain::RUNTIME_SERVICE
            )
        } else {
            String::new()
        };
        let text = format!(
            "quarantined grant {} ({}){}{interrupted}",
            q.fingerprint,
            qentry_details(q, &f.accounts),
            held.map(|id| if q.reason == "extra-logins" {
                format!(
                    ": stash {id} holds its Claude grant but not the MCP logins beside it; \
                     --fix keeps it until the stash holds those too"
                )
            } else {
                format!(": stash {id} already holds it")
            })
            .unwrap_or_default()
        );
        // Settle files it only with Orca stopped: while Orca never quits
        // the entry just sits here, so say what finishes it.
        let settle = match (&q.matched, held.is_none() && q.settle_waiting) {
            (Some(id), true) => format!(
                ": it is fresher than stash {id}, and the migration files it there only with \
                 Orca stopped{}",
                if f.running {
                    "; quit Orca and run `csm migrate` to settle it"
                } else {
                    "; run `csm migrate` to settle it"
                }
            ),
            _ => String::new(),
        };
        let text = format!("{text}{settle}");
        let fix = held.map(|id| Fix::PurgeQuarantine {
            fingerprint: q.fingerprint.clone(),
            holder: (*id).to_owned(),
        });
        push(&mut out, text, fix);
    }
    for id in &f.orphans {
        let stale = f.from_store && f.sqlite_state;
        let fix = (!f.running && f.from_store && !stale).then(|| Fix::RemoveOrphan(id.clone()));
        let why = if fix.is_some() {
            ""
        } else if stale {
            " (read from Orca's SQLite export, which may lag; start Orca to check)"
        } else {
            " (repair only with Orca stopped)"
        };
        push(
            &mut out,
            format!("orphan stash {id}: no Orca account names it{why}"),
            fix,
        );
    }
    let mut by_fp: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, fp) in &f.stash_fingerprints {
        by_fp.entry(fp.as_str()).or_default().push(id.as_str());
    }
    for ids in by_fp.values().filter(|v| v.len() > 1) {
        push(
            &mut out,
            format!("accounts {} hold the same refresh token", ids.join(", ")),
            None,
        );
    }
    for id in &f.profile_other {
        push(
            &mut out,
            format!("stash {id} holds a grant for another account (profile check)"),
            None,
        );
    }
    for id in &f.profile_dead {
        push(
            &mut out,
            format!("stash {id}: access token rejected (401); Orca or claude refreshes it on use"),
            None,
        );
    }
    for (id, why) in &f.unreadable {
        push(&mut out, format!("stash {id} unreadable: {why}"), None);
    }
    if f.d_ambiguous {
        push(
            &mut out,
            "D's account matches more than one stash".into(),
            None,
        );
    }
    match (&f.d_account, &f.active) {
        (Some(d), Some(a)) if d != a => push(
            &mut out,
            format!("D holds {d} but Orca's active account is {a} (`csm accounts use {a}`)"),
            None,
        ),
        (None, Some(a)) if !f.d_ambiguous => push(
            &mut out,
            format!("D's account is unknown; Orca's active account is {a}"),
            None,
        ),
        _ => {}
    }
    if f.dir_agrees == Some(false) {
        push(
            &mut out,
            "Orca's D differs from csm's (CLAUDE_CONFIG_DIR)".into(),
            None,
        );
    }
    if let Some(t) = &f.alias_dangling {
        push(
            &mut out,
            format!(
                "the claude alias points at {}, which is gone (an upgrade removed it?); Orca's claude panes cannot start",
                t.display()
            ),
            Some(Fix::RepairAlias),
        );
    }
    out
}

/// One secret-free line for a recovery.
fn recovery_line(r: &switch::Recovery) -> String {
    match r {
        switch::Recovery::Nothing => "no unfinished switch".into(),
        switch::Recovery::ClearedForOrca => {
            "Orca runs; its own sync owns D, the intent was cleared".into()
        }
        switch::Recovery::Neutralized => "Orca names no active account; D was made neutral".into(),
        switch::Recovery::Untouched => {
            "the unfinished switch never touched D; nothing to repair".into()
        }
        switch::Recovery::KeptSystemDefault => {
            "Orca names no active account and D holds the system default; D was left alone".into()
        }
        switch::Recovery::RestoredSystemDefault(r) => {
            let mut line = if r.had_snapshot {
                "D was put back to the system default login".to_owned()
            } else {
                "no system-default snapshot; D was made neutral".to_owned()
            };
            if !r.quarantined.is_empty() {
                line.push_str(&format!(
                    "; displaced grants quarantined: {}",
                    r.quarantined.join(", ")
                ));
            }
            line
        }
        switch::Recovery::Repaired(rep) => format!("D now holds {}", rep.to),
        switch::Recovery::Uncertain(why) => format!("the repair did not verify: {why}"),
        switch::Recovery::Failed(why) => format!("repair failed, D was made neutral: {why}"),
        switch::Recovery::Busy => {
            "another csm holds switch.lock; nothing was repaired, try again later".into()
        }
        switch::Recovery::Deferred(why) => format!("nothing was repaired: {why}"),
    }
}

/// How `doctor --fix` counts a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Fixed,
    /// Nothing was attempted (a busy lock, or a repair that has to wait
    /// for Orca to stop): not fixed, and not a failure of the repair.
    Skipped,
    /// The repair ran and failed.
    Failed,
}

/// The verdict on one recovery. Pure.
fn recovery_verdict(r: &switch::Recovery) -> Verdict {
    match r {
        switch::Recovery::Failed(_) | switch::Recovery::Uncertain(_) => Verdict::Failed,
        switch::Recovery::Busy | switch::Recovery::Deferred(_) => Verdict::Skipped,
        switch::Recovery::Nothing
        | switch::Recovery::ClearedForOrca
        | switch::Recovery::Neutralized
        | switch::Recovery::Untouched
        | switch::Recovery::KeptSystemDefault
        | switch::Recovery::RestoredSystemDefault(_)
        | switch::Recovery::Repaired(_) => Verdict::Fixed,
    }
}

fn reason_name(r: Reason) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{r:?}"))
}

/// Gather the doctor's facts. Reads files and stashes (the Keychain on
/// macOS); calls the profile endpoint unless `offline`.
pub(crate) fn gather(
    ctx: &Context,
    view: &OrcaView,
    http: &dyn OauthHttp,
    offline: bool,
) -> DoctorFacts {
    let os = ctx.os();
    let ud = &ctx.user_data.dir;
    let mut f = DoctorFacts {
        running: view.running,
        from_store: view.source == AccountSource::Store,
        sqlite_state: view.sqlite_state,
        pending_journal: switch::read_journal(&ctx.state).is_some_and(|j| j.pending()),
        active: view.active_id.clone(),
        dir_agrees: view.runtime_dir_agrees,
        ..Default::default()
    };
    let alias = crate::cmd::orca::alias_path(&ctx.state, os);
    if let crate::cmd::orca::AliasState::Dangling(t) = crate::cmd::orca::alias_state(&alias) {
        f.alias_dangling = Some(t);
    }
    let quarantine = Quarantine::new(os, &ctx.state);
    let qmetas = quarantine.list();
    // Entries settle would file, by account: their grants are compared
    // with that account's stash below.
    let settle_ids: Vec<Option<String>> = qmetas
        .iter()
        .map(|m| {
            m.matched_account
                .clone()
                .filter(|_| crate::migrate::settle_reason(m.reason))
        })
        .collect();
    f.quarantine = qmetas
        .into_iter()
        .map(|m| QEntry {
            fingerprint: m.fingerprint,
            reason: reason_name(m.reason),
            source: m.source,
            matched: m.matched_account,
            expires_at_ms: m.expires_at.filter(|v| v.is_finite()).map(|v| v as i64),
            profile_status: m.profile_status,
            profile_account_uuid: m.profile_account_uuid,
            settle_waiting: false,
        })
        .collect();
    f.accounts = view.accounts.iter().map(|a| a.id.clone()).collect();
    match view
        .runtime_account
        .as_ref()
        .and_then(|r| r.account.as_ref())
    {
        Some(UuidMatch::Unique(id)) => f.d_account = Some(id.clone()),
        Some(UuidMatch::Ambiguous(_)) => f.d_ambiguous = true,
        _ => {}
    }

    // Store records carry the stash paths; RPC records do not.
    let path_of = |id: &str| {
        view.store
            .as_ref()
            .and_then(|s| s.account(id))
            .and_then(|r| r.managed_auth_path.clone())
    };
    // (account id, its stash's grant) for the settle comparison; `None`: the
    // stash holds no grant. An unreadable stash is left out.
    let mut stash_grants: Vec<(String, Option<SecretString>)> = Vec::new();
    for rec in view.accounts.iter().filter(|a| a.is_host()) {
        let stash = match Stash::open(ud, &rec.id, path_of(&rec.id).as_deref()) {
            Ok(s) => s,
            Err(e) => {
                f.unreadable.push((rec.id.clone(), e.to_string()));
                continue;
            }
        };
        let creds = match stash.credentials(os) {
            Ok(Some(c)) => c,
            Ok(None) => {
                stash_grants.push((rec.id.clone(), None));
                continue;
            }
            Err(e) => {
                f.unreadable.push((rec.id.clone(), e.to_string()));
                continue;
            }
        };
        f.stash_fingerprints
            .push((rec.id.clone(), quarantine::fingerprint(creds.expose())));
        stash_grants.push((rec.id.clone(), Some(creds.clone())));
        if offline {
            continue;
        }
        let stash_uuid = stash
            .oauth_account()
            .ok()
            .flatten()
            .and_then(|v| OauthIdentity::from_value(&v).account_uuid);
        let Some(token) = access_token(creds.expose()) else {
            continue;
        };
        match http.get_profile(&token).map(|r| parse_profile(&r)) {
            Ok(ProfileAnswer::Ok {
                account_uuid: Some(u),
                ..
            }) if stash_uuid.as_deref().is_some_and(|s| s != u) => {
                f.profile_other.push(rec.id.clone())
            }
            Ok(ProfileAnswer::Unauthorized) => f.profile_dead.push(rec.id.clone()),
            _ => {}
        }
    }

    // A quarantined grant settle would file and that is fresher than its
    // account's stash: settle still waits for it.
    for (q, id) in f.quarantine.iter_mut().zip(&settle_ids) {
        let Some(id) = id else { continue };
        let Some((_, stash)) = stash_grants.iter().find(|(s, _)| s == id) else {
            continue;
        };
        if let Ok(Some(entry)) = quarantine.get(&q.fingerprint) {
            q.settle_waiting =
                crate::migrate::settle_wanted(entry.expose(), stash.as_ref().map(|s| s.expose()));
        }
    }

    // Orphans: stash dirs under claude-accounts/ that no record names.
    let known: Vec<&str> = view.accounts.iter().map(|a| a.id.as_str()).collect();
    if let Ok(rd) = std::fs::read_dir(claude_accounts_root(ud)) {
        let mut orphans: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| !known.contains(&n.as_str()))
            .collect();
        orphans.sort();
        f.orphans = orphans;
    }
    f
}

/// Apply one fix. The quarantine and orphan fixes re-check their facts
/// under `switch.lock` (see [`add::remove_orphan`], [`add::purge_quarantine`]).
/// `Err` is a failed repair; `Ok` carries the verdict and its line.
fn apply(ctx: &Context, facts: &dyn ProcFacts, fix: &Fix) -> anyhow::Result<(Verdict, String)> {
    match fix {
        Fix::Recover => {
            let http = SystemHttp::from_env();
            let r = ctx.with_switch_env(facts, &http, switch::recover)?;
            let line = recovery_line(&r);
            match recovery_verdict(&r) {
                Verdict::Failed => Err(anyhow::anyhow!(line)),
                v => Ok((v, line)),
            }
        }
        Fix::PurgeQuarantine {
            fingerprint,
            holder,
        } => {
            ctx.with_accounts_env(facts, |env| add::purge_quarantine(env, fingerprint, holder))?;
            Ok((
                Verdict::Fixed,
                format!("dropped quarantine entry {fingerprint}"),
            ))
        }
        Fix::RepairAlias => crate::cmd::orca::repair_alias(&ctx.env).map(|l| (Verdict::Fixed, l)),
        Fix::RemoveOrphan(id) => {
            // Re-checked under switch.lock against a fresh read of the store.
            let line = match ctx.with_accounts_env(facts, |env| add::remove_orphan(env, id))? {
                Some(k) => format!("removed orphan stash {id} (its Keychain item remains: {k})"),
                None => format!("removed orphan stash {id} (grant kept in quarantine)"),
            };
            Ok((Verdict::Fixed, line))
        }
    }
}

fn doctor(fix: bool, offline: bool) -> anyhow::Result<()> {
    let procs = SystemProcs;
    let ctx = Context::current(&procs)?;
    let v = view()?;
    let http = SystemHttp::from_env();
    let facts = gather(&ctx, &v, &http, offline);
    let found = findings(&facts);
    println!(
        "Orca: {}; accounts: {} ({})",
        if v.running { "running" } else { "stopped" },
        v.accounts.iter().filter(|a| a.is_host()).count(),
        match v.source {
            AccountSource::Rpc => "RPC",
            AccountSource::Store => "store",
            AccountSource::None => "none",
        }
    );
    if found.is_empty() {
        println!("no problems found");
        return Ok(());
    }
    for f in &found {
        let tag = if f.fix.is_some() { "fixable" } else { "note" };
        println!("  [{tag}] {}", f.text);
    }
    if !fix {
        if found.iter().any(|f| f.fix.is_some()) {
            println!("run `csm accounts doctor --fix` to repair the fixable items");
        }
        return Ok(());
    }
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for fx in found.iter().filter_map(|f| f.fix.as_ref()) {
        match apply(&ctx, &procs, fx) {
            Ok((Verdict::Fixed, line)) => println!("  fixed: {line}"),
            Ok((_, line)) => {
                skipped += 1;
                eprintln!("  skipped: {line}");
            }
            Err(e) => {
                failed += 1;
                eprintln!("  not fixed: {e}");
            }
        }
    }
    if let Some(why) = fix_summary(failed, skipped) {
        bail!("csm accounts doctor: {why}");
    }
    Ok(())
}

/// The error `doctor --fix` exits with, when anything was not fixed. Pure.
fn fix_summary(failed: usize, skipped: usize) -> Option<String> {
    match (failed, skipped) {
        (0, 0) => None,
        (f, 0) => Some(format!("{f} repair(s) failed")),
        (0, s) => Some(format!("{s} repair(s) skipped; run it again later")),
        (f, s) => Some(format!("{f} repair(s) failed, {s} skipped")),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{FakeProcs, creds_json, make_stash, oauth_json, record_json};
    use crate::orca::{HostEnv, HostOs};

    /// An add, import or rm Orca refused or did not confirm is not
    /// reported as done and exits non-zero.
    #[test]
    fn change_outcome_fails_a_refused_or_unconfirmed_redo() {
        use crate::orca::store::RedoOutcome;
        let change = |redo, leftover: Option<&str>| AccountChange {
            route: add::Route::OfflineThenRpc,
            id: None,
            email: Some("carol@example.com".into()),
            redo,
            leftover: leftover.map(str::to_owned),
        };
        let ok = change_outcome("added", &change(None, None)).unwrap();
        assert_eq!(ok, "csm: added carol@example.com (offline, then via Orca)");
        let e = change_outcome(
            "added",
            &change(
                Some(RedoOutcome::Failed("duplicate".into())),
                Some("stash x"),
            ),
        )
        .unwrap_err();
        assert!(
            e.contains("was not added") && e.contains("duplicate"),
            "{e}"
        );
        assert!(e.contains("stash x"), "{e}");
        let e = change_outcome(
            "imported",
            &change(Some(RedoOutcome::Uncertain("timeout".into())), None),
        )
        .unwrap_err();
        assert!(
            e.contains("may not be imported") && e.contains("doctor"),
            "{e}"
        );
        assert!(report_change("add", "added", &change(None, None)).is_ok());
        assert!(
            report_change(
                "add",
                "added",
                &change(Some(RedoOutcome::Failed("no".into())), None)
            )
            .is_err()
        );
    }

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    fn entry(id: &str, email: &str) -> AccountEntry {
        AccountEntry {
            id: id.into(),
            email: Some(email.into()),
            organization_name: Some("Acme".into()),
            managed_auth_path: None,
        }
    }

    #[test]
    fn a_failed_or_skipped_recovery_is_never_reported_as_fixed() {
        use switch::Recovery;
        assert_eq!(
            recovery_verdict(&Recovery::Failed("x".into())),
            Verdict::Failed
        );
        assert_eq!(recovery_verdict(&Recovery::Busy), Verdict::Skipped);
        assert_eq!(
            recovery_verdict(&Recovery::Deferred("Orca runs".into())),
            Verdict::Skipped
        );
        // A hand-over to an Orca that came up mid-repair and did not
        // verify is not a fix: doctor exits non-zero.
        assert_eq!(
            recovery_verdict(&Recovery::Uncertain("no answer".into())),
            Verdict::Failed
        );
        assert!(recovery_line(&Recovery::Uncertain("no answer".into())).contains("did not verify"));
        assert_eq!(recovery_verdict(&Recovery::Nothing), Verdict::Fixed);
        assert_eq!(recovery_verdict(&Recovery::ClearedForOrca), Verdict::Fixed);
        assert_eq!(fix_summary(0, 0), None);
        assert!(fix_summary(1, 0).unwrap().contains("1 repair(s) failed"));
        assert!(fix_summary(0, 2).unwrap().contains("2 repair(s) skipped"));
        assert!(fix_summary(1, 1).is_some());
    }

    #[test]
    fn parse_every_verb() {
        assert_eq!(parse(&[]).unwrap(), AccountsCmd::List);
        assert_eq!(parse(&os(&["list"])).unwrap(), AccountsCmd::List);
        assert_eq!(
            parse(&os(&["use", "alice@example.com"])).unwrap(),
            AccountsCmd::Use("alice@example.com".into())
        );
        assert_eq!(parse(&os(&["add"])).unwrap(), AccountsCmd::Add);
        assert_eq!(
            parse(&os(&["import", "/Users/example/.claude.work", "/x"])).unwrap(),
            AccountsCmd::Import(vec![
                PathBuf::from("/Users/example/.claude.work"),
                PathBuf::from("/x")
            ])
        );
        assert_eq!(
            parse(&os(&["rm", "abc"])).unwrap(),
            AccountsCmd::Rm("abc".into())
        );
        assert_eq!(
            parse(&os(&["doctor", "--fix", "--offline"])).unwrap(),
            AccountsCmd::Doctor {
                fix: true,
                offline: true
            }
        );
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        assert!(parse(&os(&["use"])).is_err());
        assert!(parse(&os(&["use", "a", "b"])).is_err());
        assert!(parse(&os(&["import"])).is_err());
        assert!(parse(&os(&["doctor", "--force"])).is_err());
        assert!(parse(&os(&["frobnicate"])).is_err());
        assert!(parse(&os(&["add", "extra"])).is_err());
    }

    #[test]
    fn resolve_by_id_prefix_and_email() {
        let accts = vec![
            entry("aaaa1111", "alice@example.com"),
            entry("bbbb2222", "bob@example.com"),
        ];
        assert_eq!(resolve(&accts, "aaaa1111").unwrap(), "aaaa1111");
        assert_eq!(resolve(&accts, "bbbb").unwrap(), "bbbb2222");
        assert_eq!(resolve(&accts, "Alice@Example.com").unwrap(), "aaaa1111");
        assert!(resolve(&accts, "zzz").is_err());
    }

    #[test]
    fn render_list_marks_active_and_d() {
        let accts = vec![
            entry("aaaa1111", "alice@example.com"),
            entry("bbbb2222", "bob@example.com"),
        ];
        let out = render_list(&accts, Some("bbbb2222"), Some("aaaa1111"));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with(" D alice@example.com"), "{out}");
        assert!(lines[1].starts_with("*  bob@example.com"), "{out}");
        assert!(lines[1].contains("bbbb2222") && lines[1].ends_with("Acme"));
        assert!(render_list(&[], None, None).contains("no Claude accounts"));
    }

    /// Round 8: a pre-login copy left in the quarantine is named as an
    /// interrupted login, with what to check before Orca starts.
    #[test]
    fn a_leftover_pre_login_copy_is_named_as_an_interrupted_login() {
        assert_eq!(reason_name(Reason::PreLogin), "pre-login");
        let f = DoctorFacts {
            quarantine: vec![QEntry {
                fingerprint: "fp-pre".into(),
                reason: reason_name(Reason::PreLogin),
                source: "legacy-keychain".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = findings(&f);
        let line = &out
            .iter()
            .find(|x| x.text.contains("fp-pre"))
            .expect("listed")
            .text;
        assert!(
            line.contains("interrupted") && line.contains("before Orca starts"),
            "{line}"
        );
    }

    /// Round 8: a retired copy that also holds MCP logins is not reported
    /// as one the stash already holds.
    #[test]
    fn an_extra_logins_copy_is_not_reported_as_held() {
        assert_eq!(reason_name(Reason::ExtraLogins), "extra-logins");
        let f = DoctorFacts {
            quarantine: vec![QEntry {
                fingerprint: "fp-x".into(),
                reason: reason_name(Reason::ExtraLogins),
                source: "file".into(),
                ..Default::default()
            }],
            stash_fingerprints: vec![("acct-a".into(), "fp-x".into())],
            ..Default::default()
        };
        let out = findings(&f);
        let line = &out
            .iter()
            .find(|x| x.text.contains("fp-x"))
            .expect("listed")
            .text;
        assert!(!line.contains("already holds it"), "{line}");
        assert!(line.contains("MCP logins"), "{line}");
    }

    #[test]
    fn findings_cover_each_fact() {
        let f = DoctorFacts {
            pending_journal: true,
            quarantine: vec![
                QEntry {
                    fingerprint: "fp-held".into(),
                    reason: "no-match".into(),
                    source: "file".into(),
                    ..Default::default()
                },
                QEntry {
                    fingerprint: "fp-loose".into(),
                    reason: "ambiguous".into(),
                    source: "scoped-keychain".into(),
                    matched: Some("a".into()),
                    ..Default::default()
                },
                QEntry {
                    fingerprint: "fp-gone".into(),
                    reason: "profile-mismatch".into(),
                    source: "file".into(),
                    matched: Some("gone".into()),
                    expires_at_ms: Some(1_767_225_600_000),
                    profile_status: Some(200),
                    profile_account_uuid: Some("u-x".into()),
                    settle_waiting: false,
                },
            ],
            accounts: vec!["a".into(), "b".into(), "c".into()],
            stash_fingerprints: vec![
                ("a".into(), "fp-held".into()),
                ("b".into(), "fp-dup".into()),
                ("c".into(), "fp-dup".into()),
            ],
            orphans: vec!["zz".into()],
            from_store: true,
            profile_other: vec!["b".into()],
            d_account: Some("a".into()),
            active: Some("b".into()),
            dir_agrees: Some(false),
            ..Default::default()
        };
        let got = findings(&f);
        let fixes: Vec<_> = got.iter().filter_map(|x| x.fix.clone()).collect();
        assert_eq!(
            fixes,
            vec![
                Fix::Recover,
                Fix::PurgeQuarantine {
                    fingerprint: "fp-held".into(),
                    holder: "a".into()
                },
                Fix::RemoveOrphan("zz".into())
            ]
        );
        let text: Vec<&str> = got.iter().map(|x| x.text.as_str()).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("b, c hold the same refresh token"))
        );
        assert!(
            text.iter()
                .any(|t| t.contains("stash b holds a grant for another"))
        );
        assert!(
            text.iter()
                .any(|t| t.contains("D holds a but Orca's active account is b"))
        );
        assert!(text.iter().any(|t| t.contains("Orca's D differs")));
        // Quarantine lines: expiry, profile answer, and a vanished account.
        let loose = text.iter().find(|t| t.contains("fp-loose")).unwrap();
        assert!(
            loose.contains("matched a") && !loose.contains("removed"),
            "{loose}"
        );
        let gone = text.iter().find(|t| t.contains("fp-gone")).unwrap();
        assert!(gone.contains("expires 2026-01-01 00:00 UTC"), "{gone}");
        assert!(gone.contains("profile 200 u-x"), "{gone}");
        assert!(gone.contains("matched gone (account removed)"), "{gone}");
        // A removed account's entry is listed, never purged (Q4).
        assert!(!fixes.iter().any(|x| matches!(
            x,
            Fix::PurgeQuarantine { fingerprint, .. } if fingerprint == "fp-gone"
        )));
    }

    #[test]
    fn use_names_the_live_sessions_that_move() {
        use crate::orca::runtime::{SessionRecord, SessionScan};
        let rec = |pid: u32, sid: Option<&str>| SessionRecord {
            pid,
            session_id: sid.map(str::to_owned),
            proc_start: None,
            proc_start_ft: None,
            pid_domain: None,
            kind: None,
            status: None,
        };
        assert!(live_session_lines(&SessionScan::default(), "bob").is_empty());
        let scan = SessionScan {
            live: vec![rec(4242, Some("0123456789abcdef-rest")), rec(4343, None)],
            unverifiable: vec![rec(99, None)],
            dead: 3,
            unreadable: 1,
        };
        assert_eq!(
            live_session_lines(&scan, "bob"),
            vec![
                "csm: live claude (pid 4242, session 01234567) moves to bob",
                "csm: live claude (pid 4343) moves to bob",
                "csm: 2 more session record(s) in D may be live and would move to bob",
            ]
        );
    }

    #[test]
    fn a_dangling_alias_is_fixable() {
        let f = DoctorFacts {
            alias_dangling: Some(PathBuf::from(
                "/opt/homebrew/Cellar/claude-smart/0.3.7/bin/csm",
            )),
            ..Default::default()
        };
        let got = findings(&f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fix, Some(Fix::RepairAlias));
        assert!(got[0].text.contains("is gone"), "{}", got[0].text);
    }

    #[test]
    fn orphans_are_not_fixable_while_orca_runs() {
        let f = DoctorFacts {
            orphans: vec!["zz".into()],
            running: true,
            from_store: false,
            ..Default::default()
        };
        let got = findings(&f);
        assert_eq!(got.len(), 1);
        assert!(got[0].fix.is_none());
        assert!(got[0].text.contains("Orca stopped"));
    }

    #[test]
    fn orphans_read_from_a_sqlite_export_are_listed_but_not_fixable() {
        let f = DoctorFacts {
            orphans: vec!["zz".into()],
            running: false,
            from_store: true,
            sqlite_state: true,
            ..Default::default()
        };
        let got = findings(&f);
        assert_eq!(got.len(), 1);
        assert!(got[0].fix.is_none());
        assert!(got[0].text.contains("may lag"), "{}", got[0].text);
    }

    #[test]
    fn healthy_facts_have_no_findings() {
        let f = DoctorFacts {
            d_account: Some("a".into()),
            active: Some("a".into()),
            dir_agrees: Some(true),
            stash_fingerprints: vec![("a".into(), "fp1".into())],
            ..Default::default()
        };
        assert!(findings(&f).is_empty());
    }

    /// `gather` over a temp Linux userData: file stashes, one orphan, one
    /// duplicate grant, D pointing at the non-active account. Offline, so
    /// no HTTP call is made; no Keychain on Linux.
    #[test]
    fn gather_reads_a_linux_store_offline() {
        use crate::orca::http::FakeHttp;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::Linux);
        let ctx = Context::from_env(env.clone(), &FakeProcs::default());
        let ud = ctx.user_data.dir.clone();
        let shared = creds_json("at-x", "rt-shared", 1);
        let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
        let bob = serde_json::to_vec(&oauth_json("u-b", "bob@example.com", None)).unwrap();
        make_stash(&ud, "acct-a", Some(&alice), Some(shared.as_bytes()));
        make_stash(&ud, "acct-b", Some(&bob), Some(shared.as_bytes()));
        make_stash(&ud, "orphan-1", None, None);
        crate::orca::testsupport::write_store(
            &ud,
            &[
                record_json(&ud, "acct-a", "alice@example.com", None),
                record_json(&ud, "acct-b", "bob@example.com", None),
            ],
            Some("acct-b"),
        );
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            serde_json::json!({"oauthAccount": {"accountUuid": "u-a"}}).to_string(),
        )
        .unwrap();

        let v =
            crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &FakeProcs::default());
        let http = FakeHttp::default();
        let f = gather(&ctx, &v, &http, true);
        assert!(!f.running && f.from_store);
        assert_eq!(f.orphans, vec!["orphan-1".to_string()]);
        assert_eq!(f.stash_fingerprints.len(), 2);
        assert_eq!(f.stash_fingerprints[0].1, f.stash_fingerprints[1].1);
        assert_eq!(f.d_account.as_deref(), Some("acct-a"));
        assert_eq!(f.active.as_deref(), Some("acct-b"));
        let text = findings(&f)
            .into_iter()
            .map(|x| x.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("rt-shared"), "no secret in output: {text}");
        assert!(!text.contains("at-x"), "no secret in output: {text}");
    }

    /// Settle files a retired dir's fresher grant only with Orca stopped.
    /// While Orca never quits, the doctor says so and how to finish it.
    #[test]
    fn a_grant_settle_waits_for_says_how_to_finish_it() {
        let entry = |running| DoctorFacts {
            running,
            quarantine: vec![QEntry {
                fingerprint: "fp-r".into(),
                reason: "retired".into(),
                source: "/Users/example/.claude.work".into(),
                matched: Some("a".into()),
                settle_waiting: true,
                ..Default::default()
            }],
            accounts: vec!["a".into()],
            stash_fingerprints: vec![("a".into(), "fp-old".into())],
            d_account: Some("a".into()),
            active: Some("a".into()),
            dir_agrees: Some(true),
            ..Default::default()
        };
        let got = findings(&entry(true));
        assert_eq!(got.len(), 1);
        assert!(got[0].fix.is_none(), "settle is not the doctor's to run");
        assert!(
            got[0].text.contains("fresher than stash a")
                && got[0].text.contains("quit Orca and run `csm migrate`"),
            "{}",
            got[0].text
        );
        let got = findings(&entry(false));
        assert!(
            got[0].text.contains("run `csm migrate` to settle it")
                && !got[0].text.contains("quit Orca"),
            "{}",
            got[0].text
        );
        let mut f = entry(true);
        f.quarantine[0].settle_waiting = false;
        assert!(!findings(&f)[0].text.contains("settle"));
    }

    /// `gather` marks a retired grant fresher than its account's stash as
    /// waiting for settle, and a staler one, or one filed for another
    /// reason, as not.
    #[test]
    fn gather_marks_the_grants_settle_waits_for() {
        use crate::orca::http::FakeHttp;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::Linux);
        let ctx = Context::from_env(env.clone(), &FakeProcs::default());
        let ud = ctx.user_data.dir.clone();
        let stash = creds_json("at-s", "rt-s", 2_000_000_000_000);
        let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
        make_stash(&ud, "acct-a", Some(&alice), Some(stash.as_bytes()));
        crate::orca::testsupport::write_store(
            &ud,
            &[record_json(&ud, "acct-a", "alice@example.com", None)],
            Some("acct-a"),
        );
        let q = Quarantine::new(HostOs::Linux, &ctx.state);
        let fresh = creds_json("at-f", "rt-f", 2_100_000_000_000);
        let stale = creds_json("at-o", "rt-o", 1_000_000_000_000);
        let other = creds_json("at-n", "rt-n", 2_200_000_000_000);
        let fp = |c: &str| quarantine::fingerprint(c);
        q.file(&fresh, Reason::Retired, "dir", Some("acct-a"), None, 1)
            .unwrap();
        q.file(&stale, Reason::Retired, "dir", Some("acct-a"), None, 1)
            .unwrap();
        q.file(&other, Reason::NoMatch, "file", Some("acct-a"), None, 1)
            .unwrap();

        let v =
            crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &FakeProcs::default());
        let f = gather(&ctx, &v, &FakeHttp::default(), true);
        let waiting = |c: &str| {
            f.quarantine
                .iter()
                .find(|q| q.fingerprint == fp(c))
                .map(|q| q.settle_waiting)
        };
        assert_eq!(waiting(&fresh), Some(true));
        assert_eq!(waiting(&stale), Some(false));
        assert_eq!(waiting(&other), Some(false));
        let text = findings(&f)
            .into_iter()
            .map(|x| x.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("fresher than stash acct-a"), "{text}");
        assert!(
            !text.contains("rt-f") && !text.contains("at-f"),
            "no secret: {text}"
        );
    }
}
