//! `csm statusline` — print `<account>@<host>` for Claude Code's statusLine
//! (or a shell prompt).
//!
//! ## Output contract
//!
//! One line: the host field, `<label>@<host>` when csm knows which Orca
//! account the session runs on, else `<host>`. The label is the local part of
//! the account's email (design §9), or the first 8 chars of its id when Orca
//! has no email for it.
//!
//! ## Piggyback statusLine capture (local usage collection)
//!
//! Claude Code's own `statusLine` command runs with the full statusLine JSON
//! (which carries `rate_limits` for the live account, refreshed roughly once a
//! second) on stdin; a shell prompt or manual invocation inherits a TTY stdin
//! with nothing on it. `run()` tells the two apart with
//! `stdin().is_terminal()`: when stdin is NOT a terminal it reads stdin
//! (capped, see [`CAPTURE_STDIN_CAP_BYTES`]) and feeds it to
//! [`usage::local::record_statusline_payload_in`] best-effort. The reading is
//! attributed to an account by the identity-change rule in
//! `usage::local` (design §4 "Attribution"). `CSM_STATUSLINE_NO_CAPTURE=1`
//! (or `true`) disables the read entirely.
//!
//! After the segment is printed, the same payload:
//!
//! 1. is forwarded to Orca's `/statusline/claude` receiver when the session
//!    runs in an Orca pane (design §8, [`crate::orca::forward`]): the gate and
//!    the 15 s throttle run here, the POST runs in a detached
//!    `csm statusline --orca-forward` child;
//! 2. drives the limit-switch trigger ([`crate::hook::run_from_statusline`]),
//!    since Claude Code fires no hook for a subscription cap. Disabling the
//!    capture disables both.
//!
//! ### Host display
//!
//! The short hostname (first DNS label) is read at runtime and optionally
//! rewritten by the `CSM_HOST_REPLACE` rule (see [`apply_host_replace`]); the
//! binary hardcodes no naming convention. With `CSM_HOST_REPLACE=Acme-/` a host
//! `Acme-Laptop` renders as `Laptop`; with no rule it renders verbatim.
//!
//! ## Segment format
//!
//! | Condition | Output |
//! |-----------|--------|
//! | account known, email `alice@example.com` | `alice@Laptop` |
//! | account known, no email | `<first 8 chars of id>@Laptop` |
//! | no account known | `Laptop` |

use std::ffi::OsString;
use std::io::IsTerminal;

use anyhow::Result;

use crate::account::accounts::AccountSet;
use crate::orca::forward;
use crate::usage;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Hard cap on how much of a piggybacked statusLine stdin payload `run()` will
/// ever read — mirrors `main::CAPTURE_STDIN_CAP_BYTES`; kept as a separate
/// constant since `csm usage capture` and `csm statusline` are different
/// entry points that happen to share the same defensive ceiling.
const CAPTURE_STDIN_CAP_BYTES: u64 = 256 * 1024;

/// Subcommand handler: print `<label>@<host>` (or just `<host>`) to stdout.
///
/// Also piggybacks the statusLine-stdin usage capture, the Orca forward and
/// the limit trigger (see the module doc). None of them affects the printed
/// segment or the exit code.
///
/// `csm statusline --orca-forward` is the forwarding child
/// ([`forward::run_child`]); it prints nothing.
pub fn run(args: &[OsString]) -> Result<()> {
    if args.first().is_some_and(|a| a == forward::FORWARD_ARG) {
        forward::run_child();
        return Ok(());
    }
    let raw = should_capture_stdin().then(|| read_stdin_capped(CAPTURE_STDIN_CAP_BYTES));
    let accounts = AccountSet::load();
    let capture = raw.as_deref().and_then(|r| {
        usage::local::record_statusline_payload_in(r, &accounts)
            .ok()
            .flatten()
    });
    let label = segment_label(&accounts, capture.as_ref().map(|c| c.account_id.as_str()));
    run_with_capture(raw, capture, label)
}

/// The account label for the segment: the capture's account, else `D`'s
/// current account, else none.
fn segment_label(accounts: &AccountSet, captured: Option<&str>) -> Option<String> {
    captured
        .filter(|id| !id.is_empty())
        .or(accounts.current.as_deref())
        .map(|id| accounts.label(id))
}

/// The body of [`run`] after the piggybacked stdin capture has been resolved.
/// Split out so tests can drive it with `None` instead of exercising
/// `should_capture_stdin`/`read_stdin_capped`, which spawn a real
/// stdin-reading thread that outlives the test when the test process's stdin
/// is not at EOF (an interactive terminal, or a pipe that never closes) — the
/// thread holds `Stdin`'s internal lock for the rest of its blocking read, so
/// any later test that touches stdin then hangs waiting on that lock.
pub(crate) fn run_with_capture(
    raw: Option<String>,
    capture: Option<usage::local::StatuslineCapture>,
    label: Option<String>,
) -> Result<()> {
    let segment = format_segment(short_hostname()?, label.as_deref());
    println!("{segment}");
    let Some(raw) = raw else {
        return Ok(());
    };
    // Orca's feed first: the limit trigger below may end this session.
    let payload = forward::normalize_payload(&raw);
    if forward::gate_and_stamp(payload) {
        forward::spawn_forward(payload);
    }
    if let Some(capture) = capture {
        crate::hook::run_from_statusline(&raw, &capture);
    }
    // idle_compact reads the payload's own fields directly, independent of
    // the capture/attribution above. Writes no stdout of its own — the
    // segment above is already printed and this must never corrupt it.
    crate::idle_compact::run_from_raw(&raw);
    Ok(())
}

/// `true` when `run()` should read stdin for the piggyback capture: stdin is
/// not a terminal (a real TTY-backed shell-prompt invocation never has a
/// payload to read) AND `CSM_STATUSLINE_NO_CAPTURE` is not `1`/`true`.
///
/// Split from `run()` so the env-gate logic is testable without needing to
/// control the process's actual stdin.
fn should_capture_stdin() -> bool {
    if std::io::stdin().is_terminal() {
        return false;
    }
    !capture_disabled_by_env()
}

/// `true` iff `CSM_STATUSLINE_NO_CAPTURE` is set to `1` or `true` (any case).
fn capture_disabled_by_env() -> bool {
    match std::env::var("CSM_STATUSLINE_NO_CAPTURE") {
        Ok(v) => matches!(v.trim(), "1" | "true" | "True" | "TRUE"),
        Err(_) => false,
    }
}

/// Hard deadline on the piggybacked stdin read (see [`read_stdin_capped`]).
/// The statusLine case (this command's actual reason to read stdin at all)
/// writes its payload and closes stdin essentially immediately; this is
/// generous for that case while still bounding the failure mode below.
const CAPTURE_STDIN_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

/// Read stdin up to `max_bytes`, lossily decoding as UTF-8, bounded by
/// [`CAPTURE_STDIN_DEADLINE`] as well as `max_bytes`.
///
/// The byte cap alone does not bound *time*: `should_capture_stdin`'s gate is
/// "stdin is not a terminal", which is broader than "invoked as a statusLine
/// command" — a caller that runs `csm statusline` with an inherited pipe it
/// does not itself own (e.g. `producer | while read -r line; do prompt="$(csm
/// statusline)"; done`) hands this a stdin that may deliver under `max_bytes`
/// and never close, and a plain `read_to_end` would then block forever,
/// hanging the caller's loop. The read runs on a background thread instead;
/// this function waits only up to the deadline and returns whatever arrived
/// (empty string on timeout — `record_statusline_payload` treats that as a
/// no-op, same as any other unusable payload). The spawned thread is not
/// joined on timeout — nothing here can force a blocking `Read` to return
/// early — but it is harmless to leak until the process exits shortly after
/// this call returns (`run()` prints the segment next and returns).
///
/// Mirrors `cmd::support::read_stdin_capped`'s byte-cap contract; duplicated
/// rather than shared because that twin has no need for the deadline thread
/// here — `csm usage capture` reads from a pipe Claude Code always closes
/// promptly, while this command's stdin gate (`should_capture_stdin`) is
/// broader ("not a terminal") and can see a caller-owned pipe that never
/// closes, so only this copy needs the timeout to avoid hanging on it.
fn read_stdin_capped(max_bytes: u64) -> String {
    use std::io::Read;
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::stdin().take(max_bytes).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(CAPTURE_STDIN_DEADLINE) {
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => String::new(),
    }
}

/// Build the host segment. Pure.
pub fn format_segment(host: String, label: Option<&str>) -> String {
    match label.map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => format!("{l}@{host}"),
        None => host,
    }
}

// ─── Hostname ─────────────────────────────────────────────────────────────────

/// Environment variable carrying an optional hostname-rewrite rule, so the
/// binary never hardcodes any site-specific naming convention. Format is a
/// single `find/replace` pair (literal, case-insensitive find, first match):
/// e.g. `Acme-/` turns `Acme-Laptop` into `Laptop`. Unset/empty → no rewrite.
pub const HOST_REPLACE_ENV: &str = "CSM_HOST_REPLACE";

/// Return the **short** hostname, applying the optional `CSM_HOST_REPLACE`
/// rewrite if one is configured. With no rule set, the raw short hostname is
/// returned unchanged — the binary carries no built-in naming convention.
pub fn short_hostname() -> Result<String> {
    let raw = hostname()?;
    Ok(apply_host_replace(
        raw,
        std::env::var(HOST_REPLACE_ENV).ok().as_deref(),
    ))
}

/// Apply a `find/replace` rewrite rule to a hostname.
///
/// `rule` is `Some("find/replace")` (e.g. `"Acme-/"` to drop an `Acme-`
/// prefix); the find is matched case-insensitively at the first occurrence and
/// replaced literally. `None`, an empty rule, or a rule without a `/` separator
/// leaves the hostname untouched. This keeps every site-specific convention out
/// of the binary — the rule is injected from the environment
/// (a deployment can set CSM_HOST_REPLACE=Acme-/ to drop a site prefix).
pub fn apply_host_replace(s: String, rule: Option<&str>) -> String {
    let Some(rule) = rule.filter(|r| !r.is_empty()) else {
        return s;
    };
    let Some((find, replace)) = rule.split_once('/') else {
        return s;
    };
    if find.is_empty() {
        return s;
    }
    // Case-insensitive search for the first occurrence of `find`.
    let lower_s = s.to_ascii_lowercase();
    let lower_find = find.to_ascii_lowercase();
    match lower_s.find(&lower_find) {
        Some(idx) => format!("{}{}{}", &s[..idx], replace, &s[idx + find.len()..]),
        None => s,
    }
}

/// Return the system short hostname (first DNS label; no domain suffix).
pub fn hostname() -> Result<String> {
    hostname_impl()
}

#[cfg(unix)]
fn hostname_impl() -> Result<String> {
    use nix::unistd::gethostname;
    let name = gethostname()?;
    let raw = name.to_string_lossy();
    // Strip FQDN suffix (keep first label only), matching `hostname -s`.
    let short = raw.split('.').next().unwrap_or(&raw);
    Ok(short.to_owned())
}

#[cfg(windows)]
fn hostname_impl() -> Result<String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::{ComputerNameNetBIOS, GetComputerNameExW};

    // First call: pass null + &mut size to obtain required buffer length.
    let mut size: u32 = 0;
    // SAFETY: querying buffer size with null pointer — documented Windows API pattern.
    unsafe { GetComputerNameExW(ComputerNameNetBIOS, std::ptr::null_mut(), &mut size) };

    let mut buf: Vec<u16> = vec![0u16; size as usize];
    let ok = unsafe { GetComputerNameExW(ComputerNameNetBIOS, buf.as_mut_ptr(), &mut size) };
    if ok == 0 {
        anyhow::bail!("GetComputerNameExW failed");
    }
    buf.truncate(size as usize);
    Ok(OsString::from_wide(&buf).to_string_lossy().into_owned())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var mutations below all go through `crate::testenv`'s per-name
    // locks — shared across module boundaries with `usage::local`'s own
    // CLAUDE_CONFIG_DIR-mutating tests, since a module-local lock cannot
    // protect against a different module's test interleaving on the same
    // process-global variable. See `crate::testenv` for why.

    // ── apply_host_replace (CSM_HOST_REPLACE rewrite rule) ────────────────────

    const PREFIX_RULE: Option<&str> = Some("Acme-/");

    #[test]
    fn replace_prefix_basic() {
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), PREFIX_RULE),
            "Laptop"
        );
    }

    #[test]
    fn replace_prefix_workstation() {
        assert_eq!(
            apply_host_replace("Acme-Workstation".to_owned(), PREFIX_RULE),
            "Workstation"
        );
    }

    #[test]
    fn replace_prefix_case_insensitive() {
        // Windows NetBIOS uppercased: "ACME-WINDOWS" still matches "Acme-/".
        assert_eq!(
            apply_host_replace("ACME-WINDOWS".to_owned(), PREFIX_RULE),
            "WINDOWS"
        );
    }

    #[test]
    fn replace_no_match_unchanged() {
        // Hostname without the configured prefix is returned unchanged.
        assert_eq!(
            apply_host_replace("myhostname".to_owned(), PREFIX_RULE),
            "myhostname"
        );
    }

    #[test]
    fn replace_no_rule_unchanged() {
        // With no rule (None / empty) the hostname is never rewritten.
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), None),
            "Acme-Laptop"
        );
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), Some("")),
            "Acme-Laptop"
        );
    }

    #[test]
    fn replace_short_string_no_panic() {
        // Strings shorter than the find pattern must not panic.
        assert_eq!(apply_host_replace("abc".to_owned(), PREFIX_RULE), "abc");
    }

    #[test]
    fn replace_malformed_rule_unchanged() {
        // A rule without a '/' separator is ignored.
        assert_eq!(
            apply_host_replace("Acme-Laptop".to_owned(), Some("noseparator")),
            "Acme-Laptop"
        );
    }

    // ── format_segment / segment_label ────────────────────────────────────────

    #[test]
    fn segment_with_label() {
        assert_eq!(
            format_segment("Laptop".to_owned(), Some("alice")),
            "alice@Laptop"
        );
    }

    #[test]
    fn segment_without_label_is_host() {
        assert_eq!(format_segment("Laptop".to_owned(), None), "Laptop");
        assert_eq!(format_segment("Laptop".to_owned(), Some("  ")), "Laptop");
    }

    #[test]
    fn segment_after_host_replace() {
        let host = apply_host_replace("Acme-Workstation".to_owned(), Some("Acme-/"));
        assert_eq!(format_segment(host, Some("bob")), "bob@Workstation");
    }

    fn entry(id: &str, email: Option<&str>) -> crate::account::accounts::AccountEntry {
        crate::account::accounts::AccountEntry {
            id: id.to_owned(),
            email: email.map(str::to_owned),
            organization_name: None,
            managed_auth_path: None,
        }
    }

    #[test]
    fn label_prefers_the_capture_then_current() {
        let set = AccountSet {
            accounts: vec![
                entry("a1", Some("alice@example.com")),
                entry("b2", Some("bob@example.com")),
            ],
            current: Some("a1".to_owned()),
            ..AccountSet::default()
        };
        assert_eq!(segment_label(&set, Some("b2")).as_deref(), Some("bob"));
        assert_eq!(segment_label(&set, None).as_deref(), Some("alice"));
        assert_eq!(segment_label(&set, Some("")).as_deref(), Some("alice"));
        let empty = AccountSet::default();
        assert_eq!(segment_label(&empty, None), None);
    }

    #[test]
    fn label_of_an_account_without_email_is_the_id_prefix() {
        let set = AccountSet {
            accounts: vec![entry("0123456789abcdef", None)],
            ..AccountSet::default()
        };
        assert_eq!(
            segment_label(&set, Some("0123456789abcdef")).as_deref(),
            Some("01234567")
        );
    }

    // ── short_hostname ────────────────────────────────────────────────────────

    #[test]
    fn short_hostname_returns_nonempty() {
        let h = short_hostname().expect("short_hostname() must not error");
        assert!(!h.is_empty());
    }

    #[test]
    fn short_hostname_no_domain_suffix() {
        let h = short_hostname().unwrap();
        assert!(
            !h.contains('.') || h.starts_with('.'),
            "short hostname should have no interior FQDN dot: {h}"
        );
    }

    #[test]
    fn short_hostname_applies_env_rule() {
        // short_hostname() honors CSM_HOST_REPLACE: with a rule matching the
        // raw hostname's first label, that prefix is rewritten away. The binary
        // hardcodes no convention — the rule comes entirely from the env.
        let raw = hostname().unwrap();
        // Build a rule that strips the raw hostname's leading char as a prefix,
        // so the assertion holds on any host without depending on a real name.
        if let Some(first) = raw.chars().next() {
            crate::testenv::with_env_var(HOST_REPLACE_ENV, Some(&format!("{first}/")), || {
                let h = short_hostname().unwrap();
                // The first occurrence of `first` is removed → result is
                // shorter or equal, and never starts with that exact char
                // at index 0 unless it repeated. Just assert the rewrite
                // ran (length strictly decreased).
                assert!(
                    h.len() < raw.len(),
                    "rule should have removed one char: {raw} → {h}"
                );
            });
        }
    }

    #[test]
    fn short_hostname_no_rule_is_raw() {
        // With no CSM_HOST_REPLACE set, short_hostname() == raw short hostname.
        crate::testenv::with_env_var(HOST_REPLACE_ENV, None, || {
            let h = short_hostname().unwrap();
            let raw = hostname().unwrap();
            assert_eq!(h, raw, "no rule → hostname unchanged");
        });
    }

    // ── hostname (raw) ────────────────────────────────────────────────────────

    #[test]
    fn hostname_returns_nonempty_string() {
        let h = hostname().expect("hostname() must not error");
        assert!(!h.is_empty(), "hostname must be non-empty");
        assert!(
            !h.contains('\n'),
            "hostname must not contain a newline, got {h:?}"
        );
    }

    #[test]
    fn hostname_no_domain_suffix() {
        let h = hostname().unwrap();
        assert!(!h.starts_with('.'), "hostname must not start with a dot");
        assert!(!h.ends_with('.'), "hostname must not end with a dot");
        if let Some(first) = h.split('.').next() {
            assert!(!first.is_empty(), "first hostname label must be non-empty");
        }
    }

    // ── run() smoke ───────────────────────────────────────────────────────────

    #[test]
    fn run_does_not_panic_or_error() {
        // `run` itself may spawn the stdin-reading thread inside
        // `read_stdin_capped`, which blocks past the test if the test
        // process's stdin never reaches EOF — so this drives
        // `run_with_capture` directly with no capture, exactly what
        // `should_capture_stdin` returning false (or a timed-out read)
        // produces.
        let result = run_with_capture(None, None, Some("alice".to_owned()));
        assert!(
            result.is_ok(),
            "run_with_capture returned Err: {:?}",
            result.err()
        );
    }

    #[test]
    fn capture_disabled_by_env_unset_is_false() {
        crate::testenv::with_env_var("CSM_STATUSLINE_NO_CAPTURE", None, || {
            assert!(!capture_disabled_by_env());
        });
    }

    #[test]
    fn capture_disabled_by_env_recognizes_1_and_true() {
        for v in ["1", "true", "True", "TRUE"] {
            crate::testenv::with_env_var("CSM_STATUSLINE_NO_CAPTURE", Some(v), || {
                assert!(capture_disabled_by_env(), "{v} should disable capture");
            });
        }
    }

    #[test]
    fn capture_disabled_by_env_rejects_other_values() {
        for v in ["0", "false", "yes", ""] {
            crate::testenv::with_env_var("CSM_STATUSLINE_NO_CAPTURE", Some(v), || {
                assert!(!capture_disabled_by_env(), "{v} should not disable capture");
            });
        }
    }
}
