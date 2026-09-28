//! The macOS Keychain seam: pure builders plus ONE `run_security` shell.
//!
//! Orca and Claude Code both talk to the Keychain through the `security`
//! CLI, never Security.framework, so every item keeps `/usr/bin/security` as
//! its creating app and csm's reads through the same binary do not prompt.
//! csm uses only `/usr/bin/security`, with Orca's 3000 ms timeout.
//!
//! Names (Orca 1.4.209 keychain chunk, K:7-70):
//! - stash: service `Orca Claude Code Managed Credentials`, account = the
//!   Orca account id;
//! - runtime: scoped `Claude Code-credentials-<sha256(NFC(D)).hex[0..8]>`
//!   and unscoped `Claude Code-credentials`; account `$USER`, else
//!   `$USERNAME`, else the OS user, when it matches `^[a-zA-Z0-9._-]+$`,
//!   else `claude-code-user`. Orca writes scoped, then unscoped when the two
//!   differ.
//!
//! Write encoding (Claude Code 2.1.283, `Bi=4032`): `security -i` with the
//! stdin line `add-generic-password -U -a "<acct>" -s "<svc>" -X "<hex>"\n`
//! when that line (newline included) is at most 4032 characters, else the
//! argv form `add-generic-password -U -a <acct> -s <svc> -X <hex>`. `-X`
//! stores the decoded bytes, the same bytes Orca's `-w <json>` stores; hex
//! removes every quoting question. Every write is verified by reading the
//! item back ([`add_password`]): the exit status of `security -i` is not a
//! reliable failure signal.
//!
//! Orca's keychain chunk exports, and their ports here:
//! - `a` = l(D): the aggregate read over w(D) ([`read_runtime_aggregate`]);
//! - `o` = u(D): the scoped read over C(D), or the unscoped item for no dir
//!   ([`read_runtime_scoped`]);
//! - `l` = f(v, D): write scoped, then unscoped when different (the
//!   materialize runs it as two steps, `WriteScoped` then `WriteLegacy`,
//!   in `runtime::materialize_checked`); `c` = d(v, D): write scoped only
//!   ([`write_runtime_scoped`]);
//! - `r` = m(D): delete the scoped items of C(D) for every x() account,
//!   failing on access errors ([`delete_runtime_scoped`]);
//! - `s`/`u`/`i`: the stash read/write/delete ([`find_stash`],
//!   [`write_stash`], [`delete_stash`]);
//! - `t` = C(D): `D` plus its realpath alias ([`dir_aliases`]).
//!
//! Test guard: under `cfg(test)` [`run_security`] refuses unless a fake
//! runner is installed for the thread ([`set_fake_security`]), and refuses a
//! "fake" whose program or leading arguments name the real
//! `/usr/bin/security`. The fake is an existing interpreter plus arguments
//! (see `testsupport::FakeSecurity`): tests never create an executable.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::{HostEnv, SecretString};

/// Orca's stash service name.
pub const STASH_SERVICE: &str = "Orca Claude Code Managed Credentials";
/// Claude Code's unscoped runtime service name.
pub const RUNTIME_SERVICE: &str = "Claude Code-credentials";
/// The account name when the user name does not qualify.
pub const ACCOUNT_FALLBACK: &str = "claude-code-user";
/// Claude Code's `security -i` stdin line limit (newline included).
pub const STDIN_LINE_MAX: usize = 4032;
/// Orca's `security` timeout.
pub const TIMEOUT: Duration = Duration::from_millis(3000);
/// The only binary csm runs outside tests.
#[cfg_attr(
    all(feature = "e2e", not(test)),
    allow(dead_code, reason = "the e2e build runs the harness's fake only")
)]
pub const SECURITY_BIN: &str = "/usr/bin/security";
/// `security`'s exit status for "item not found".
pub const EXIT_NOT_FOUND: i32 = 44;

// ─── errors ───────────────────────────────────────────────────────────────────

/// A Keychain failure. Never carries a password.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeychainError {
    #[error("cannot run security: {0}")]
    Spawn(String),
    #[error("security timed out after {}ms", TIMEOUT.as_millis())]
    Timeout,
    #[error("security exited with status {code:?}: {reason}")]
    Failed { code: Option<i32>, reason: String },
    #[error("the Keychain item is empty")]
    Empty,
    #[error("the Keychain item is not UTF-8")]
    NotUtf8,
    #[error("unusable Keychain name: {0}")]
    BadName(&'static str),
    /// A test or e2e build refused to run the real `/usr/bin/security`.
    #[cfg(any(test, feature = "e2e"))]
    #[error("refused: {0}")]
    Refused(String),
}

// ─── names ────────────────────────────────────────────────────────────────────

/// Orca's S(): the runtime service for config dir `dir`, unscoped for none.
/// Pure.
pub fn runtime_service(dir: Option<&str>) -> String {
    match dir.filter(|d| !d.is_empty()) {
        None => RUNTIME_SERVICE.to_owned(),
        Some(d) => {
            let nfc: String = d.nfc().collect();
            let digest = Sha256::digest(nfc.as_bytes());
            let hex = hex_lower(&digest);
            format!("{RUNTIME_SERVICE}-{}", &hex[..8])
        }
    }
}

fn account_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// JS `a || b`: the first non-empty value.
fn first_set<'a>(vals: &[Option<&'a str>]) -> Option<&'a str> {
    vals.iter().flatten().copied().find(|s| !s.is_empty())
}

/// Orca's b(): the runtime item's account name. Pure.
pub fn account_name(user: Option<&str>, username: Option<&str>, os_user: Option<&str>) -> String {
    match first_set(&[user, username, os_user]) {
        Some(n) if account_name_ok(n) => n.to_owned(),
        _ => ACCOUNT_FALLBACK.to_owned(),
    }
}

/// Orca's x(): every account name a runtime delete covers (the sanitized
/// name, plus the raw env name when it differs).
pub fn account_names_for_delete(
    user: Option<&str>,
    username: Option<&str>,
    os_user: Option<&str>,
) -> Vec<String> {
    let e = account_name(user, username, os_user);
    match first_set(&[user, username]) {
        Some(raw) if raw != e => vec![e, raw.to_owned()],
        _ => vec![e],
    }
}

// ─── pure builders ────────────────────────────────────────────────────────────

/// Lowercase hex, as `Buffer.toString("hex")` writes it.
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Names go inside double quotes on the `-i` line; refuse anything that
/// could break out of them or end the line.
fn check_name(s: &str, what: &'static str) -> Result<(), KeychainError> {
    if s.is_empty() || s.contains(['"', '\\', '\n', '\r', '\0']) {
        return Err(KeychainError::BadName(what));
    }
    Ok(())
}

/// How one add is sent to `security`.
#[derive(Clone, PartialEq, Eq)]
pub enum AddInvocation {
    /// `security -i` with this stdin line (newline included).
    Stdin(String),
    /// `security <argv…>`.
    Argv(Vec<String>),
}

impl std::fmt::Debug for AddInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddInvocation::Stdin(l) => write!(f, "Stdin(<redacted {} chars>)", l.len()),
            AddInvocation::Argv(a) => write!(f, "Argv(<redacted {} args>)", a.len()),
        }
    }
}

/// Claude Code's add encoding for `secret` under (`acct`, `svc`). Pure.
pub fn add_invocation(
    acct: &str,
    svc: &str,
    secret: &[u8],
) -> Result<AddInvocation, KeychainError> {
    check_name(acct, "account")?;
    check_name(svc, "service")?;
    let hex = hex_lower(secret);
    let line = format!("add-generic-password -U -a \"{acct}\" -s \"{svc}\" -X \"{hex}\"\n");
    if line.len() <= STDIN_LINE_MAX {
        return Ok(AddInvocation::Stdin(line));
    }
    Ok(AddInvocation::Argv(vec![
        "add-generic-password".into(),
        "-U".into(),
        "-a".into(),
        acct.into(),
        "-s".into(),
        svc.into(),
        "-X".into(),
        hex,
    ]))
}

/// `find-generic-password -s <svc> -a <acct> -w` (Orca's T()).
pub fn find_argv(svc: &str, acct: &str) -> Vec<String> {
    ["find-generic-password", "-s", svc, "-a", acct, "-w"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// `find-generic-password -s <svc> -a <acct>` without `-w`: the item's
/// attributes only, never its secret. A presence probe.
pub fn presence_argv(svc: &str, acct: &str) -> Vec<String> {
    ["find-generic-password", "-s", svc, "-a", acct]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// `delete-generic-password -s <svc> -a <acct>` (Orca's D()).
pub fn delete_argv(svc: &str, acct: &str) -> Vec<String> {
    ["delete-generic-password", "-s", svc, "-a", acct]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

// ─── the one shell ────────────────────────────────────────────────────────────

/// What `security` returned. `stdout` may be a password: `Debug` redacts.
pub struct SecurityOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl std::fmt::Debug for SecurityOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityOutput")
            .field("code", &self.code)
            .field(
                "stdout",
                &format_args!("<redacted {} bytes>", self.stdout.len()),
            )
            .field("stderr", &self.stderr)
            .finish()
    }
}

impl Drop for SecurityOutput {
    fn drop(&mut self) {
        super::zero(&mut self.stdout);
    }
}

impl SecurityOutput {
    /// Orca's k(): exit 44 or a "could not be found" message.
    pub fn is_not_found(&self) -> bool {
        let s = self.stderr.to_lowercase();
        self.code == Some(EXIT_NOT_FOUND)
            || s.contains("could not be found")
            || s.contains("not be found")
    }

    fn reason(&self) -> String {
        let first = self.stderr.lines().next().unwrap_or("").trim();
        first.chars().take(200).collect()
    }
}

/// A test's stand-in for `security`: a program plus the arguments that go
/// before the `security` arguments (e.g. `/usr/bin/perl -e <script> --
/// <root>`). Tests never create an executable of their own: the program is
/// an existing interpreter and per-test state travels in `lead`.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FakeRunner {
    pub program: PathBuf,
    pub lead: Vec<std::ffi::OsString>,
}

#[cfg(test)]
thread_local! {
    static FAKE_SECURITY: std::cell::RefCell<Option<FakeRunner>> =
        const { std::cell::RefCell::new(None) };
}

/// Install (or clear) this thread's fake `security` runner for tests.
#[cfg(test)]
pub(crate) fn set_fake_security(runner: Option<FakeRunner>) {
    FAKE_SECURITY.with(|f| *f.borrow_mut() = runner);
}

// Test-only override of [`TIMEOUT`] for this thread (a slow CI host);
// production always uses Orca's 3000 ms.
#[cfg(test)]
thread_local! {
    static TEST_TIMEOUT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_timeout(t: Option<Duration>) {
    TEST_TIMEOUT.with(|c| c.set(t));
}

#[cfg(not(test))]
fn timeout() -> Duration {
    // The e2e build's fake runs under perl on a possibly loaded host.
    if crate::e2e::ENABLED {
        return Duration::from_secs(20);
    }
    TIMEOUT
}

#[cfg(test)]
fn timeout() -> Duration {
    TEST_TIMEOUT.with(|c| c.get()).unwrap_or(TIMEOUT)
}

/// The program [`run_security`] runs and the arguments it puts first.
#[cfg(all(not(test), not(feature = "e2e")))]
fn security_binary() -> Result<(PathBuf, Vec<std::ffi::OsString>), KeychainError> {
    Ok((PathBuf::from(SECURITY_BIN), Vec::new()))
}

/// e2e build: the harness's fake only, never the real binary.
#[cfg(all(not(test), feature = "e2e"))]
fn security_binary() -> Result<(PathBuf, Vec<std::ffi::OsString>), KeychainError> {
    crate::e2e::security_program().map_err(|why| KeychainError::Refused(format!("e2e: {why}")))
}

/// Test build: only an installed fake, never the real binary.
#[cfg(test)]
fn security_binary() -> Result<(PathBuf, Vec<std::ffi::OsString>), KeychainError> {
    let fake = FAKE_SECURITY
        .with(|f| f.borrow().clone())
        .ok_or_else(|| KeychainError::Refused("cfg(test): no fake security installed".into()))?;
    let real = Path::new(SECURITY_BIN);
    let names_real = |p: &Path| {
        p == real
            || matches!(
                (std::fs::canonicalize(p), std::fs::canonicalize(real)),
                (Ok(a), Ok(b)) if a == b
            )
    };
    // Neither the program nor any leading argument (an interpreter's script
    // or `exec` target) may be the real binary.
    if names_real(&fake.program) || fake.lead.iter().any(|a| names_real(Path::new(a))) {
        return Err(KeychainError::Refused(
            "cfg(test): the fake security is the real /usr/bin/security".into(),
        ));
    }
    Ok((fake.program, fake.lead))
}

/// Run `security <args>` with optional `stdin`, bounded by [`TIMEOUT`].
/// On timeout the child's process group is killed and reaped within
/// [`crate::platform::child::REAP_LIMIT`].
pub fn run_security(
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<SecurityOutput, KeychainError> {
    crate::usage::reach::note("keychain");
    let (bin, lead) = security_binary()?;
    run_with(&bin, &lead, args, stdin, timeout())
}

fn run_with(
    bin: &Path,
    lead: &[std::ffi::OsString],
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<SecurityOutput, KeychainError> {
    use crate::platform::child;

    let mut cmd = Command::new(bin);
    cmd.args(lead)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child::own_group(&mut cmd)
        .spawn()
        .map_err(|e| KeychainError::Spawn(e.kind().to_string()))?;

    let writer = match (stdin, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => {
            let mut data = data.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
                super::zero(&mut data);
            }))
        }
        _ => None,
    };
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());

    let started = Instant::now();
    let status = match child::wait_deadline(&mut child, timeout, Duration::from_millis(5), true) {
        Ok(Some(s)) => s,
        // The pipe threads are abandoned, not joined: a grandchild may
        // still hold a pipe open. They end when it closes.
        Ok(None) => return Err(KeychainError::Timeout),
        Err(e) => return Err(KeychainError::Spawn(e.kind().to_string())),
    };
    // The child exited; its pipes close unless something it started still
    // holds them. Bound the collection by what is left of the deadline (at
    // least the reap limit) so that case cannot hang either.
    let left = timeout
        .saturating_sub(started.elapsed())
        .max(child::REAP_LIMIT);
    let (Ok(stdout), Ok(stderr)) = (out_rx.recv_timeout(left), err_rx.recv_timeout(left)) else {
        return Err(KeychainError::Timeout);
    };
    if let Some(w) = writer
        && w.is_finished()
    {
        let _ = w.join();
    }
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    Ok(SecurityOutput {
        code: status.code(),
        stdout,
        stderr,
    })
}

/// Read a pipe to its end on a thread; the bytes arrive on the channel.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut b);
        }
        let _ = tx.send(b);
    });
    rx
}

/// Read a generic password the way Orca's T() does: `Ok(None)` when absent,
/// the trimmed text otherwise. An item that is empty after trimming is an
/// error (Orca throws, and its callers read that as "no credentials").
pub fn find_password(svc: &str, acct: &str) -> Result<Option<SecretString>, KeychainError> {
    let out = run_security(&find_argv(svc, acct), None)?;
    if out.code != Some(0) {
        if out.is_not_found() {
            return Ok(None);
        }
        return Err(KeychainError::Failed {
            code: out.code,
            reason: out.reason(),
        });
    }
    let text = std::str::from_utf8(&out.stdout).map_err(|_| KeychainError::NotUtf8)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(KeychainError::Empty);
    }
    Ok(Some(SecretString::new(trimmed.to_owned())))
}

/// Does the item exist? Runs [`presence_argv`], which never reads the
/// secret (so it cannot raise an access prompt either); stdout, the item's
/// attributes, is dropped unread.
pub fn has_password(svc: &str, acct: &str) -> Result<bool, KeychainError> {
    let out = run_security(&presence_argv(svc, acct), None)?;
    match out.code {
        Some(0) => Ok(true),
        _ if out.is_not_found() => Ok(false),
        code => Err(KeychainError::Failed {
            code,
            reason: out.reason(),
        }),
    }
}

/// The stash credential of account `id` (macOS).
pub fn find_stash(id: &str) -> Result<Option<SecretString>, KeychainError> {
    find_password(STASH_SERVICE, id)
}

// ─── C() / w() ────────────────────────────────────────────────────────────────

/// Orca's C(): `dir` as given, plus its realpath when that differs. For a
/// path that does not exist yet, the realpath of the nearest existing
/// ancestor joined with the rest (stopping at a dangling link, a non-ENOENT
/// error, or a `..` component).
pub fn dir_aliases(dir: &str) -> Vec<String> {
    let mut out = vec![dir.to_owned()];
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut a = PathBuf::from(dir);
    loop {
        match std::fs::canonicalize(&a) {
            Ok(real) => {
                let mut joined = real;
                for r in &rest {
                    joined.push(r);
                }
                let s = joined.to_string_lossy().into_owned();
                if s != dir {
                    out.push(s);
                }
                break;
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound
                    || dir.split(['/', '\\']).any(|p| p == "..")
                {
                    break;
                }
                match std::fs::symlink_metadata(&a) {
                    Ok(_) => break,
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => break,
                    Err(_) => {}
                }
                let parent = match a.parent() {
                    Some(p) if p.as_os_str().is_empty() => PathBuf::from("."),
                    Some(p) => p.to_path_buf(),
                    None => break,
                };
                if parent == a {
                    break;
                }
                if let Some(name) = a.file_name() {
                    rest.insert(0, name.to_owned());
                }
                a = parent;
            }
        }
    }
    out
}

/// Orca's w(): the scoped services over C(dir), then the unscoped one,
/// unique, in order.
pub fn runtime_read_services(dir: Option<&str>) -> Vec<String> {
    let Some(dir) = dir.filter(|d| !d.is_empty()) else {
        return vec![RUNTIME_SERVICE.to_owned()];
    };
    let mut out: Vec<String> = Vec::new();
    for s in dir_aliases(dir)
        .iter()
        .map(|d| runtime_service(Some(d)))
        .chain(std::iter::once(RUNTIME_SERVICE.to_owned()))
    {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

// ─── account names ────────────────────────────────────────────────────────────

/// The runtime item's account names for this user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainUser {
    /// b(): the account every read and write uses.
    pub acct: String,
    /// x(): the accounts a runtime delete covers.
    pub delete_accts: Vec<String>,
}

impl KeychainUser {
    pub fn from_env(env: &HostEnv) -> KeychainUser {
        let (u, n, o) = (
            env.user.as_deref(),
            env.username.as_deref(),
            env.os_user.as_deref(),
        );
        KeychainUser {
            acct: account_name(u, n, o),
            delete_accts: account_names_for_delete(u, n, o),
        }
    }
}

// ─── reads over the runtime items ─────────────────────────────────────────────

/// Orca's u() (`De.o`): the first scoped item over C(dir); the unscoped item
/// when `dir` is `None`.
pub fn read_runtime_scoped(
    dir: Option<&str>,
    user: &KeychainUser,
) -> Result<Option<SecretString>, KeychainError> {
    let Some(dir) = dir.filter(|d| !d.is_empty()) else {
        return find_password(RUNTIME_SERVICE, &user.acct);
    };
    for alias in dir_aliases(dir) {
        if let Some(v) = find_password(&runtime_service(Some(&alias)), &user.acct)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Every spelling of `dir` a claude process may have hashed into its
/// Keychain service name: as given, with and without one trailing
/// separator, and each form's [`dir_aliases`] walk. Deduped, in order.
pub fn dir_spellings(dir: &str) -> Vec<String> {
    let trimmed = dir.trim_end_matches(['/', '\\']);
    let base = if trimmed.is_empty() { dir } else { trimmed };
    let sep = if base.contains('\\') && !base.contains('/') {
        '\\'
    } else {
        '/'
    };
    let forms = [dir.to_owned(), base.to_owned(), format!("{base}{sep}")];
    let mut out: Vec<String> = Vec::new();
    for f in forms {
        for a in dir_aliases(&f) {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

/// Orca's l() (`De.a`): the first item over w(dir).
pub fn read_runtime_aggregate(
    dir: Option<&str>,
    user: &KeychainUser,
) -> Result<Option<SecretString>, KeychainError> {
    for svc in runtime_read_services(dir) {
        if let Some(v) = find_password(&svc, &user.acct)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

// ─── writes ───────────────────────────────────────────────────────────────────

/// Add or update (`-U`) the item, then read it back and compare. The value
/// must be non-blank (Orca's reads treat blank as an error) and is compared
/// the way Orca reads it (trimmed).
pub fn add_password(svc: &str, acct: &str, secret: &str) -> Result<(), KeychainError> {
    if secret.trim().is_empty() {
        return Err(KeychainError::Empty);
    }
    let out = match add_invocation(acct, svc, secret.as_bytes())? {
        AddInvocation::Stdin(mut line) => {
            let r = run_security(&["-i".to_owned()], Some(line.as_bytes()));
            super::zero(super::unsafe_bytes_of(&mut line));
            r?
        }
        AddInvocation::Argv(mut argv) => {
            let r = run_security(&argv, None);
            for a in argv.iter_mut() {
                super::zero(super::unsafe_bytes_of(a));
            }
            r?
        }
    };
    if out.code != Some(0) {
        return Err(KeychainError::Failed {
            code: out.code,
            reason: out.reason(),
        });
    }
    match find_password(svc, acct)? {
        Some(got) if got.expose() == secret.trim() => Ok(()),
        _ => Err(KeychainError::Failed {
            code: out.code,
            reason: "the item did not read back as written".into(),
        }),
    }
}

/// Delete the item. `Ok(false)` when it did not exist.
pub fn delete_password(svc: &str, acct: &str) -> Result<bool, KeychainError> {
    let out = run_security(&delete_argv(svc, acct), None)?;
    if out.code == Some(0) {
        return Ok(true);
    }
    if out.is_not_found() {
        return Ok(false);
    }
    Err(KeychainError::Failed {
        code: out.code,
        reason: out.reason(),
    })
}

/// Orca's g(): write account `id`'s stash item.
pub fn write_stash(id: &str, secret: &str) -> Result<(), KeychainError> {
    add_password(STASH_SERVICE, id, secret)
}

/// Orca's _(): delete account `id`'s stash item; absent is fine.
pub fn delete_stash(id: &str) -> Result<bool, KeychainError> {
    delete_password(STASH_SERVICE, id)
}

/// Orca's d() (`De.c`): write the scoped item for `dir` only.
pub fn write_runtime_scoped(
    secret: &str,
    dir: Option<&str>,
    user: &KeychainUser,
) -> Result<(), KeychainError> {
    add_password(&runtime_service(dir), &user.acct, secret)
}

/// Orca's m() (`De.r`): delete the scoped items of C(dir) (the unscoped item
/// for `None`) under every x() account; not-found is fine, other failures
/// are errors.
pub fn delete_runtime_scoped(dir: Option<&str>, user: &KeychainUser) -> Result<(), KeychainError> {
    let services: Vec<String> = match dir.filter(|d| !d.is_empty()) {
        None => vec![RUNTIME_SERVICE.to_owned()],
        Some(d) => dir_aliases(d)
            .iter()
            .map(|a| runtime_service(Some(a)))
            .collect(),
    };
    for svc in services {
        for acct in &user.delete_accts {
            delete_password(&svc, acct)?;
        }
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_service_matches_the_fixed_vector_and_nfc() {
        // sha256("/Users/example/.claude.work") starts a343d19d (the same
        // vector usage::local::creds pins).
        assert_eq!(
            runtime_service(Some("/Users/example/.claude.work")),
            "Claude Code-credentials-a343d19d"
        );
        assert_eq!(
            runtime_service(Some("/Users/example/caf\u{00e9}")),
            runtime_service(Some("/Users/example/cafe\u{0301}"))
        );
        assert_eq!(runtime_service(None), RUNTIME_SERVICE);
        assert_eq!(runtime_service(Some("")), RUNTIME_SERVICE);
    }

    #[test]
    fn account_name_follows_k7_to_k9() {
        assert_eq!(
            account_name(Some("example"), Some("other"), None),
            "example"
        );
        assert_eq!(account_name(Some(""), Some("other"), None), "other");
        assert_eq!(account_name(None, None, Some("os.user")), "os.user");
        assert_eq!(
            account_name(Some("has space"), None, None),
            ACCOUNT_FALLBACK
        );
        assert_eq!(account_name(None, None, None), ACCOUNT_FALLBACK);
        assert_eq!(
            account_names_for_delete(Some("has space"), None, None),
            vec![ACCOUNT_FALLBACK.to_owned(), "has space".to_owned()]
        );
        assert_eq!(
            account_names_for_delete(Some("example"), None, None),
            vec!["example"]
        );
    }

    #[test]
    fn add_uses_the_stdin_line_up_to_4032_chars_then_argv() {
        let fixed = "add-generic-password -U -a \"example\" -s \"svc\" -X \"\"\n".len();
        let fits = (STDIN_LINE_MAX - fixed) / 2;
        let AddInvocation::Stdin(line) =
            add_invocation("example", "svc", &vec![b'a'; fits]).unwrap()
        else {
            panic!("expected the -i form");
        };
        assert_eq!(line.len(), STDIN_LINE_MAX);
        assert!(line.starts_with("add-generic-password -U -a \"example\" -s \"svc\" -X \"6161"));
        assert!(line.ends_with("\"\n"));

        let AddInvocation::Argv(argv) =
            add_invocation("example", "svc", &vec![b'a'; fits + 1]).unwrap()
        else {
            panic!("expected the argv form");
        };
        assert_eq!(
            &argv[..7],
            [
                "add-generic-password",
                "-U",
                "-a",
                "example",
                "-s",
                "svc",
                "-X"
            ]
        );
        assert_eq!(argv[7].len(), (fits + 1) * 2);
        let dbg = format!("{:?}", AddInvocation::Argv(argv));
        assert!(!dbg.contains("6161"), "{dbg}");
    }

    #[test]
    fn hex_is_lowercase_and_exact() {
        assert_eq!(hex_lower(b"{\"a\":1}\xff"), "7b2261223a317dff");
    }

    #[test]
    fn names_that_break_quoting_are_refused() {
        for bad in ["", "a\"b", "a\\b", "a\nb"] {
            assert!(add_invocation(bad, "svc", b"x").is_err());
            assert!(add_invocation("acct", bad, b"x").is_err());
        }
    }

    #[test]
    fn guard_refuses_without_a_fake() {
        set_fake_security(None);
        let e = run_security(&find_argv("svc", "acct"), None).unwrap_err();
        assert!(matches!(e, KeychainError::Refused(_)), "{e:?}");
    }

    #[test]
    fn guard_refuses_the_real_binary_as_a_fake() {
        set_fake_security(Some(FakeRunner {
            program: PathBuf::from(SECURITY_BIN),
            lead: Vec::new(),
        }));
        let e = run_security(&find_argv("svc", "acct"), None).unwrap_err();
        set_fake_security(None);
        assert!(matches!(e, KeychainError::Refused(_)), "{e:?}");
    }

    #[test]
    fn guard_refuses_the_real_binary_behind_an_interpreter() {
        for lead in [
            vec![SECURITY_BIN.into()],
            vec![
                "-c".into(),
                "exec \"$0\" \"$@\"".into(),
                SECURITY_BIN.into(),
            ],
        ] {
            set_fake_security(Some(FakeRunner {
                program: PathBuf::from("/bin/sh"),
                lead,
            }));
            let e = run_security(&find_argv("svc", "acct"), None).unwrap_err();
            set_fake_security(None);
            assert!(matches!(e, KeychainError::Refused(_)), "{e:?}");
        }
    }

    #[cfg(unix)]
    /// The presence probe never passes `-w`, so it never reads a secret.
    #[test]
    fn has_password_probes_without_reading_the_secret() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        fake.put(
            "svc-a",
            "alice",
            br#"{"claudeAiOauth":{"accessToken":"at-a"}}"#,
        );
        assert!(has_password("svc-a", "alice").unwrap());
        assert!(!has_password("svc-b", "alice").unwrap());
        let argv = fake.argv();
        assert_eq!(argv.len(), 2, "{argv:?}");
        assert!(
            argv.iter().all(|a| !a.split(' ').any(|t| t == "-w")),
            "{argv:?}"
        );
        fake.fail_find("svc-a", true);
        assert!(has_password("svc-a", "alice").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn find_reads_trimmed_bytes_and_absent_items() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        fake.put(STASH_SERVICE, "id-1", b"{\"claudeAiOauth\":{}}");
        let got = find_stash("id-1").unwrap().unwrap();
        assert_eq!(got.expose(), "{\"claudeAiOauth\":{}}");
        assert!(find_stash("id-2").unwrap().is_none());
        fake.put(STASH_SERVICE, "blank", b"  ");
        assert_eq!(find_stash("blank").unwrap_err(), KeychainError::Empty);
    }

    #[cfg(unix)]
    #[test]
    fn fake_security_round_trips_both_add_forms() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let small = br#"{"claudeAiOauth":{"accessToken":"tok"}}"#.to_vec();
        let big = vec![b'z'; 5000];
        for (acct, bytes) in [("small", &small), ("big", &big)] {
            match add_invocation(acct, STASH_SERVICE, bytes).unwrap() {
                AddInvocation::Stdin(line) => {
                    let out = run_security(&["-i".to_owned()], Some(line.as_bytes())).unwrap();
                    assert_eq!(out.code, Some(0), "{out:?}");
                }
                AddInvocation::Argv(argv) => {
                    let out = run_security(&argv, None).unwrap();
                    assert_eq!(out.code, Some(0), "{out:?}");
                }
            }
            assert_eq!(
                fake.get(STASH_SERVICE, acct).as_deref(),
                Some(bytes.as_slice())
            );
        }
        assert_eq!(fake.calls(), vec!["-i", "add-generic-password"]);
        // -U updates in place.
        let AddInvocation::Stdin(line) = add_invocation("small", STASH_SERVICE, b"v2").unwrap()
        else {
            panic!()
        };
        run_security(&["-i".to_owned()], Some(line.as_bytes())).unwrap();
        assert_eq!(
            fake.get(STASH_SERVICE, "small").as_deref(),
            Some(&b"v2"[..])
        );
        let out = run_security(&delete_argv(STASH_SERVICE, "small"), None).unwrap();
        assert_eq!(out.code, Some(0));
        assert!(fake.get(STASH_SERVICE, "small").is_none());
        let out = run_security(&delete_argv(STASH_SERVICE, "small"), None).unwrap();
        assert!(out.is_not_found());
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_binary_times_out() {
        // `/bin/sleep` directly: no generated script, no grandchild.
        let start = Instant::now();
        let e = run_with(
            Path::new("/bin/sleep"),
            &["30".into()],
            &[],
            None,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(e, KeychainError::Timeout);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// A grandchild holding the pipes open cannot stretch the call past the
    /// deadline: the whole group is killed.
    #[cfg(unix)]
    #[test]
    fn a_hung_grandchild_is_killed_with_its_group() {
        let start = Instant::now();
        let e = run_with(
            Path::new("/bin/sh"),
            &["-c".into(), "/bin/sleep 30 | /bin/cat".into()],
            &[],
            None,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(e, KeychainError::Timeout);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dir_aliases_follow_c() {
        assert_eq!(
            dir_aliases("/nonexistent-root-xyz/a/b"),
            vec!["/nonexistent-root-xyz/a/b".to_owned()]
                .into_iter()
                .chain(
                    std::fs::canonicalize("/")
                        .ok()
                        .map(|r| r.join("nonexistent-root-xyz/a/b"))
                        .map(|p| p.to_string_lossy().into_owned())
                        .filter(|p| p != "/nonexistent-root-xyz/a/b")
                )
                .collect::<Vec<_>>()
        );
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap();
        let given = tmp.path().join("missing").join("deeper");
        let given_s = given.to_str().unwrap();
        let aliases = dir_aliases(given_s);
        assert_eq!(aliases[0], given_s);
        let want = real.join("missing").join("deeper");
        if want != given {
            assert_eq!(
                aliases,
                vec![given_s.to_owned(), want.to_string_lossy().into_owned()]
            );
        } else {
            assert_eq!(aliases.len(), 1);
        }
        // A ".." anywhere stops the walk on a missing path.
        let dotted = format!("{}/x/../missing", tmp.path().display());
        assert_eq!(dir_aliases(&dotted), vec![dotted.clone()]);
    }

    #[cfg(unix)]
    #[test]
    fn dir_aliases_resolve_a_symlinked_dir_and_stop_at_a_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();
        let link = real.join("link");
        assert_eq!(
            dir_aliases(link.to_str().unwrap()),
            vec![
                link.to_string_lossy().into_owned(),
                real.join("target").to_string_lossy().into_owned()
            ]
        );
        std::os::unix::fs::symlink(real.join("gone"), real.join("dangling")).unwrap();
        let d = real.join("dangling").join("sub");
        assert_eq!(dir_aliases(d.to_str().unwrap()).len(), 1);
    }

    #[test]
    fn read_services_are_scoped_aliases_then_unscoped() {
        assert_eq!(runtime_read_services(None), vec![RUNTIME_SERVICE]);
        let s = runtime_read_services(Some("/nonexistent-root-xyz/.claude"));
        assert_eq!(s.last().map(String::as_str), Some(RUNTIME_SERVICE));
        assert_eq!(s[0], runtime_service(Some("/nonexistent-root-xyz/.claude")));
    }

    #[cfg(unix)]
    fn user() -> KeychainUser {
        KeychainUser {
            acct: "example".into(),
            delete_accts: vec!["example".into()],
        }
    }

    #[cfg(unix)]
    #[test]
    fn writes_are_verified_by_reading_back() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        write_stash("id-1", "{\"claudeAiOauth\":{\"accessToken\":\"t\"}}").unwrap();
        assert!(fake.get(STASH_SERVICE, "id-1").is_some());
        fake.fail_add(STASH_SERVICE, true);
        let e = write_stash("id-2", "{}").unwrap_err();
        assert!(matches!(e, KeychainError::Failed { .. }), "{e:?}");
        fake.fail_add(STASH_SERVICE, false);
        fake.drop_add(STASH_SERVICE, true);
        let e = write_stash("id-3", "{}").unwrap_err();
        assert!(matches!(e, KeychainError::Failed { .. }), "{e:?}");
        assert!(!format!("{e}").contains("{}"));
        fake.drop_add(STASH_SERVICE, false);
        assert_eq!(write_stash("id-4", "  "), Err(KeychainError::Empty));
        assert!(delete_stash("id-1").unwrap());
        assert!(!delete_stash("id-1").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_writes_and_reads_follow_orca() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let d = "/nonexistent-root-xyz/.claude";
        let u = user();
        // Orca's De.l: the scoped item, then the unscoped one.
        write_runtime_scoped("grant-a", Some(d), &u).unwrap();
        write_runtime_scoped("grant-a", None, &u).unwrap();
        assert_eq!(
            fake.get(&runtime_service(Some(d)), "example").as_deref(),
            Some(&b"grant-a"[..])
        );
        assert_eq!(
            fake.get(RUNTIME_SERVICE, "example").as_deref(),
            Some(&b"grant-a"[..])
        );
        write_runtime_scoped("grant-b", Some(d), &u).unwrap();
        assert_eq!(
            read_runtime_scoped(Some(d), &u).unwrap().unwrap().expose(),
            "grant-b"
        );
        assert_eq!(
            read_runtime_scoped(None, &u).unwrap().unwrap().expose(),
            "grant-a"
        );
        assert_eq!(
            read_runtime_aggregate(Some(d), &u)
                .unwrap()
                .unwrap()
                .expose(),
            "grant-b"
        );
        delete_runtime_scoped(Some(d), &u).unwrap();
        assert_eq!(
            read_runtime_aggregate(Some(d), &u)
                .unwrap()
                .unwrap()
                .expose(),
            "grant-a"
        );
        delete_runtime_scoped(None, &u).unwrap();
        assert!(read_runtime_aggregate(Some(d), &u).unwrap().is_none());
        fake.fail_find(RUNTIME_SERVICE, true);
        assert!(read_runtime_aggregate(Some(d), &u).is_err());
    }

    #[test]
    fn keychain_user_follows_b_and_x() {
        let mut env = HostEnv::for_test(Path::new("/Users/example"), super::super::HostOs::MacOs);
        env.user = Some("has space".into());
        let u = KeychainUser::from_env(&env);
        assert_eq!(u.acct, ACCOUNT_FALLBACK);
        assert_eq!(u.delete_accts, vec![ACCOUNT_FALLBACK, "has space"]);
    }
    #[cfg(unix)]
    #[test]
    fn dir_spellings_cover_the_trailing_slash_and_the_realpath() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let d = real.to_string_lossy().into_owned();
        let got = dir_spellings(&d);
        assert_eq!(got[0], d);
        assert!(got.contains(&format!("{d}/")));
        let canon = std::fs::canonicalize(&real)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(got.contains(&canon));
        let slashed = dir_spellings(&format!("{d}/"));
        assert!(slashed.contains(&d));
        let mut dedup = got.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), got.len());
    }
}
