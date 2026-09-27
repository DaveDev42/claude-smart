//! How csm was launched, decided once before any account or session work.
//!
//! [`launch_context`] is pure over the environment, argv and whether stdin is
//! a terminal. The checks run in this order:
//!
//! 1. [`LaunchContext::Print`]: `-p`/`--print` before `--`, or stdin is not a
//!    terminal. csm execs claude verbatim with the inherited environment. This
//!    covers Orca's source-control AI and model discovery, both `-p` launches
//!    with the prompt on stdin.
//! 2. [`LaunchContext::OrcaPane`]: `ORCA_PANE_KEY` is non-empty. Orca exports
//!    it (with `ORCA_TERMINAL_HANDLE` and `ORCA_AGENT_LAUNCH_TOKEN`) into
//!    every pane shell.
//! 3. [`LaunchContext::OrcaStructured`]: `ORCA_AGENT_SESSION_SPAWN_TOKEN` is
//!    non-empty (Orca's structured agent sessions).
//! 4. [`LaunchContext::Interactive`]: everything else.
//!
//! `CSM_ORCA=0|1` forces the Orca detection of steps 2-3 off or on (on with no
//! Orca key present means `OrcaPane`). `CSM_EMBEDDED` is the older spelling
//! of the same override and is honoured when `CSM_ORCA` is unset. Print mode
//! is never overridden: a `-p` launch or a piped stdin has no one to prompt.
//!
//! `ORCA_USER_DATA_PATH` and `ORCA_APP_VERSION` are deliberately NOT signals.
//! Orca sets both on its own process environment, so every descendant
//! inherits them: a tmux server or an editor started in a pane, and SSH-relay
//! children that strip only the four pane keys. Treating those as Orca
//! launches would drop the limit switch and forward `-n` to claude.
//!
//! The module also holds the two pure environment rules every launch path
//! shares: when to pin `CLAUDE_CONFIG_DIR` ([`config_dir_pin`], and inside
//! Orca [`orca_config_dir_pin`], which follows Orca's own rule and may remove
//! the variable) and which auth variables to strip for a managed account
//! ([`auth_env_to_strip`]).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

// ─── classification ───────────────────────────────────────────────────────────

/// Where a `csm run` launch came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchContext {
    /// `-p`/`--print`, or stdin is not a terminal: exec claude verbatim.
    Print,
    /// An Orca terminal pane.
    OrcaPane,
    /// An Orca structured agent session.
    OrcaStructured,
    /// A terminal outside Orca.
    Interactive,
}

impl LaunchContext {
    /// Orca launched this process (a pane or a structured session): no
    /// account decision, no picker or prompt of any kind.
    pub fn is_orca(self) -> bool {
        matches!(
            self,
            LaunchContext::OrcaPane | LaunchContext::OrcaStructured
        )
    }
}

/// The classification plus how csm was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Launch {
    pub context: LaunchContext,
    /// argv[0] is the `claude` alias (`csm orca setup`), not `csm`.
    pub via_alias: bool,
}

/// A non-empty (after trim) environment value.
fn set(get: &dyn Fn(&str) -> Option<String>, key: &str) -> bool {
    get(key).is_some_and(|v| !v.trim().is_empty())
}

/// `CSM_ORCA` (or its older alias `CSM_EMBEDDED`): `Some(true)` forces Orca
/// detection on, `Some(false)` off, `None` leaves it to the environment. Any
/// value other than `0`/`1` counts as unset.
fn orca_override(get: &dyn Fn(&str) -> Option<String>) -> Option<bool> {
    for key in ["CSM_ORCA", "CSM_EMBEDDED"] {
        match get(key).as_deref().map(str::trim) {
            Some("1") => return Some(true),
            Some("0") => return Some(false),
            _ => {}
        }
    }
    None
}

/// `-p`/`--print` appears before the first `--`.
pub fn has_print_flag(argv: &[OsString]) -> bool {
    argv.iter()
        .map(|a| a.as_os_str())
        .take_while(|a| *a != OsStr::new("--"))
        .any(|a| a == OsStr::new("-p") || a == OsStr::new("--print"))
}

/// Is `argv0` the `claude` alias? Compares the lowercased file stem, so
/// `claude.exe` counts on Windows.
pub fn is_claude_alias(argv0: &OsStr) -> bool {
    Path::new(argv0)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("claude"))
}

/// Pure core. `get` reads one environment variable; `argv0` is the program
/// name; `argv` holds the arguments after the dispatch word (what `csm run`
/// would parse); `stdin_tty` says whether stdin is a terminal.
pub fn launch_context(
    get: &dyn Fn(&str) -> Option<String>,
    argv0: &OsStr,
    argv: &[OsString],
    stdin_tty: bool,
) -> Launch {
    let context = if has_print_flag(argv) || !stdin_tty {
        LaunchContext::Print
    } else {
        match orca_override(get) {
            Some(false) => LaunchContext::Interactive,
            forced => {
                if set(get, "ORCA_PANE_KEY") {
                    LaunchContext::OrcaPane
                } else if set(get, "ORCA_AGENT_SESSION_SPAWN_TOKEN") {
                    LaunchContext::OrcaStructured
                } else if forced == Some(true) {
                    LaunchContext::OrcaPane
                } else {
                    LaunchContext::Interactive
                }
            }
        }
    };
    Launch {
        context,
        via_alias: is_claude_alias(argv0),
    }
}

/// I/O shell over the real process.
pub fn current(argv: &[OsString]) -> Launch {
    use std::io::IsTerminal as _;
    let argv0 = std::env::args_os().next().unwrap_or_default();
    launch_context(
        &|k| std::env::var(k).ok(),
        &argv0,
        argv,
        std::io::stdin().is_terminal(),
    )
}

// ─── CLAUDE_CONFIG_DIR pin ────────────────────────────────────────────────────

/// The `CLAUDE_CONFIG_DIR` a launch must export so claude runs in `d`, or
/// `None` to leave the inherited environment alone.
///
/// claude reads `CLAUDE_CONFIG_DIR` when set and non-blank, else `~/.claude`.
/// csm pins `d` only when that effective dir differs from it (a login
/// dotfile overrode Orca's value, say). It never sets the variable when the
/// default already lands in `d`: an explicit `CLAUDE_CONFIG_DIR` changes the
/// Keychain item claude uses, so setting it "just in case" would move the
/// credentials out from under Orca.
pub fn config_dir_pin(inherited: Option<&str>, home: &Path, d: &Path) -> Option<PathBuf> {
    let effective = match inherited.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home.join(".claude"),
    };
    (effective != d).then(|| d.to_path_buf())
}

/// I/O shell for the `csm claude` passthrough and print mode: the pin for
/// the `D` this process's own environment names (always `None` unless the
/// inherited value is blank or unreadable in a way `runtime_paths` corrects).
pub fn runtime_dir_pin() -> Option<PathBuf> {
    let env = crate::orca::HostEnv::current().ok()?;
    let paths =
        crate::orca::runtime::runtime_paths(env.claude_config_dir.as_deref(), &env.home, |p| {
            p.exists()
        });
    config_dir_pin(
        env.claude_config_dir.as_deref(),
        &env.home,
        &paths.config_dir,
    )
}

/// What a launch does to the child's `CLAUDE_CONFIG_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDirPin {
    /// Leave the inherited value (or its absence) alone.
    Leave,
    /// Export this dir.
    Set(PathBuf),
    /// Remove the variable. Orca main runs without it, so Orca's own claude
    /// panes run without it too: claude then reads `~/.claude.json` (unless
    /// `~/.claude/.claude.json` exists) and the unscoped Keychain item,
    /// which is what Orca materializes. An explicit `~/.claude` would move
    /// claude to `~/.claude/.claude.json` and the scoped item instead.
    Unset,
}

impl ConfigDirPin {
    /// Give `env` the `CLAUDE_CONFIG_DIR` the child runs with: `Set(p)`
    /// exports `p`, `Unset` removes it, `Leave` keeps the inherited value.
    /// The supervisor's own process environment keeps the inherited value
    /// (only the child's is edited), so every supervisor-side read of `D`
    /// (the limit switch, the recovery, the sidecar's account) goes through
    /// this to act on the `D` its child runs in.
    pub fn apply_to(&self, env: &mut crate::orca::HostEnv) {
        match self {
            ConfigDirPin::Leave => {}
            ConfigDirPin::Set(p) => env.claude_config_dir = Some(p.to_string_lossy().into_owned()),
            ConfigDirPin::Unset => env.claude_config_dir = None,
        }
    }
}

impl From<Option<PathBuf>> for ConfigDirPin {
    fn from(p: Option<PathBuf>) -> ConfigDirPin {
        p.map_or(ConfigDirPin::Leave, ConfigDirPin::Set)
    }
}

/// Orca's pane rule (runtime-paths.ts `envPatch`): Orca exports
/// `CLAUDE_CONFIG_DIR` only when its own environment set it. `orca_explicit`
/// is Orca main's trimmed value, `None` when unset. A pane that inherited a
/// different value (a login dotfile) gets Orca's value back; with Orca
/// unset, any inherited value is removed. Pure.
pub fn orca_config_dir_pin(inherited: Option<&str>, orca_explicit: Option<&Path>) -> ConfigDirPin {
    match orca_explicit {
        Some(x) => match inherited.map(str::trim).filter(|s| !s.is_empty()) {
            Some(i) if Path::new(i) == x => ConfigDirPin::Leave,
            _ => ConfigDirPin::Set(x.to_path_buf()),
        },
        None if inherited.is_some() => ConfigDirPin::Unset,
        None => ConfigDirPin::Leave,
    }
}

/// `D` for a `csm run` launch and what to do with the child's
/// `CLAUDE_CONFIG_DIR` so it runs there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchDir {
    pub d: PathBuf,
    pub pin: ConfigDirPin,
}

/// I/O shell. Inside Orca (`orca == true`), `D` is Orca main's own
/// `CLAUDE_CONFIG_DIR` read from its process environment when readable, so
/// a login dotfile that overrode the pane's value cannot move claude out of
/// Orca's dir, and the pin follows Orca's rule ([`orca_config_dir_pin`]);
/// otherwise, and outside Orca, it is this process's own `D`
/// ([`config_dir_pin`]). Reads files and the process table only: no
/// Keychain, no network.
pub fn launch_dir(orca: bool) -> Option<LaunchDir> {
    let env = crate::orca::HostEnv::current().ok()?;
    let inherited = env.claude_config_dir.as_deref();
    if orca && let Some(o) = orca_main_dir(&env) {
        let pin = orca_config_dir_pin(inherited, o.explicit.then_some(o.dir.as_path()));
        return Some(LaunchDir { d: o.dir, pin });
    }
    let d = crate::orca::runtime::runtime_paths(inherited, &env.home, |p| p.exists()).config_dir;
    let pin = config_dir_pin(inherited, &env.home, &d).into();
    Some(LaunchDir { d, pin })
}

/// Orca main's `CLAUDE_CONFIG_DIR` (else its `~/.claude`), from its
/// runtime metadata's pid and that process's environment.
fn orca_main_dir(env: &crate::orca::HostEnv) -> Option<crate::orca::procenv::OrcaDir> {
    use crate::orca::{procenv, rpc, userdata};
    let alive = |pid: u32| pid != 0 && crate::platform::proc::is_running(pid);
    let ud = userdata::resolve(env, |dir| {
        rpc::read_runtime_metadata(dir)
            .ok()
            .flatten()
            .is_some_and(|m| alive(m.pid))
    });
    let meta = rpc::read_runtime_metadata(&ud.dir).ok().flatten()?;
    if !alive(meta.pid) {
        return None;
    }
    procenv::orca_dir(meta.pid, Some(&env.home))
}

// ─── managed-account auth env ─────────────────────────────────────────────────

/// Explicit auth overrides Orca strips from a managed-account launch.
pub const MANAGED_AUTH_ENV: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

/// Does an `ANTHROPIC_CUSTOM_HEADERS` value carry a credential? Orca's rule:
/// any mention of `authorization`, `x-api-key`, `api-key` or `bearer`.
fn headers_carry_credential(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    ["authorization", "x-api-key", "api-key", "bearer"]
        .iter()
        .any(|k| v.contains(k))
}

/// The variable names to remove from a managed-account launch (Orca's `xh`
/// with `stripAuthEnv`). Windows compares names case-insensitively. Values
/// are read only to classify `ANTHROPIC_CUSTOM_HEADERS` and never returned.
pub fn auth_env_to_strip<'a>(
    vars: impl IntoIterator<Item = (&'a str, &'a str)>,
    windows: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, value) in vars {
        let key = if windows {
            name.to_ascii_uppercase()
        } else {
            name.to_owned()
        };
        let auth = MANAGED_AUTH_ENV.contains(&key.as_str());
        let headers = key == "ANTHROPIC_CUSTOM_HEADERS" && headers_carry_credential(value);
        if auth || headers {
            out.push(name.to_owned());
        }
    }
    out
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    fn ctx(
        pairs: &'static [(&'static str, &'static str)],
        argv: &[&str],
        tty: bool,
    ) -> LaunchContext {
        launch_context(&env(pairs), OsStr::new("csm"), &os(argv), tty).context
    }

    /// An Orca pane shell (M:42088 pane key, M:144222 handle and launch token).
    const PANE: &[(&str, &str)] = &[
        ("ORCA_PANE_KEY", "tab-1:leaf-2"),
        ("ORCA_TERMINAL_HANDLE", "term-3"),
        ("ORCA_AGENT_LAUNCH_TOKEN", "launch-token"),
        (
            "ORCA_USER_DATA_PATH",
            "/Users/example/Library/Application Support/orca",
        ),
        ("ORCA_APP_VERSION", "1.4.209"),
    ];

    #[test]
    fn orca_pane_key_is_a_pane() {
        assert_eq!(
            ctx(PANE, &["--resume", "abc"], true),
            LaunchContext::OrcaPane
        );
        assert!(LaunchContext::OrcaPane.is_orca());
    }

    #[test]
    fn blank_pane_key_is_not_a_pane() {
        assert_eq!(
            ctx(&[("ORCA_PANE_KEY", "  ")], &[], true),
            LaunchContext::Interactive
        );
    }

    #[test]
    fn structured_session_token() {
        assert_eq!(
            ctx(&[("ORCA_AGENT_SESSION_SPAWN_TOKEN", "t")], &[], true),
            LaunchContext::OrcaStructured
        );
        assert!(LaunchContext::OrcaStructured.is_orca());
    }

    /// M:39892: a tmux server or SSH-relay child inherits only Orca's
    /// app-wide vars. It must stay Interactive so the limit switch works.
    #[test]
    fn inherited_app_vars_alone_are_interactive() {
        let relay: &[(&str, &str)] = &[
            (
                "ORCA_USER_DATA_PATH",
                "/Users/example/Library/Application Support/orca",
            ),
            ("ORCA_APP_VERSION", "1.4.209"),
        ];
        assert_eq!(ctx(relay, &[], true), LaunchContext::Interactive);
        assert!(!LaunchContext::Interactive.is_orca());
    }

    #[test]
    fn print_flag_before_double_dash_wins_over_a_pane() {
        assert_eq!(ctx(PANE, &["-p", "hi"], true), LaunchContext::Print);
        assert_eq!(
            ctx(&[], &["--model", "x", "--print"], true),
            LaunchContext::Print
        );
    }

    #[test]
    fn print_flag_after_double_dash_is_claude_payload() {
        assert_eq!(ctx(&[], &["--", "-p"], true), LaunchContext::Interactive);
    }

    #[test]
    fn non_tty_stdin_is_print() {
        assert_eq!(ctx(PANE, &[], false), LaunchContext::Print);
        assert_eq!(ctx(&[], &[], false), LaunchContext::Print);
    }

    #[test]
    fn csm_orca_zero_turns_detection_off() {
        let pairs: &[(&str, &str)] = &[("CSM_ORCA", "0"), ("ORCA_PANE_KEY", "tab:pane")];
        assert_eq!(ctx(pairs, &[], true), LaunchContext::Interactive);
    }

    #[test]
    fn csm_orca_one_forces_a_pane() {
        assert_eq!(
            ctx(&[("CSM_ORCA", "1")], &[], true),
            LaunchContext::OrcaPane
        );
        // Forced on never overrides Print.
        assert_eq!(
            ctx(&[("CSM_ORCA", "1")], &["-p"], true),
            LaunchContext::Print
        );
        // A real structured token keeps its own class under the force.
        let pairs: &[(&str, &str)] = &[("CSM_ORCA", "1"), ("ORCA_AGENT_SESSION_SPAWN_TOKEN", "t")];
        assert_eq!(ctx(pairs, &[], true), LaunchContext::OrcaStructured);
    }

    #[test]
    fn csm_embedded_is_an_alias_and_csm_orca_wins() {
        assert_eq!(
            ctx(&[("CSM_EMBEDDED", "1")], &[], true),
            LaunchContext::OrcaPane
        );
        let off: &[(&str, &str)] = &[("CSM_EMBEDDED", "0"), ("ORCA_PANE_KEY", "tab:pane")];
        assert_eq!(ctx(off, &[], true), LaunchContext::Interactive);
        let both: &[(&str, &str)] = &[("CSM_ORCA", "1"), ("CSM_EMBEDDED", "0")];
        assert_eq!(ctx(both, &[], true), LaunchContext::OrcaPane);
    }

    #[test]
    fn other_override_values_fall_through() {
        let pairs: &[(&str, &str)] = &[("CSM_ORCA", "yes"), ("ORCA_PANE_KEY", "tab:pane")];
        assert_eq!(ctx(pairs, &[], true), LaunchContext::OrcaPane);
        assert_eq!(
            ctx(&[("CSM_ORCA", "yes")], &[], true),
            LaunchContext::Interactive
        );
    }

    #[test]
    fn alias_detection_by_stem() {
        for name in [
            "claude",
            "/Users/example/.local/state/csm/bin/claude",
            "claude.exe",
            "CLAUDE.EXE",
        ] {
            assert!(is_claude_alias(OsStr::new(name)), "{name}");
        }
        for name in ["csm", "csm.exe", "claude-code", "/usr/bin/csm"] {
            assert!(!is_claude_alias(OsStr::new(name)), "{name}");
        }
        let l = launch_context(&env(&[]), OsStr::new("claude"), &[], true);
        assert!(l.via_alias);
    }

    // ── config_dir_pin ────────────────────────────────────────────────────────

    #[test]
    fn pin_is_none_when_the_default_already_lands_in_d() {
        let home = Path::new("/Users/example");
        assert_eq!(config_dir_pin(None, home, &home.join(".claude")), None);
        assert_eq!(config_dir_pin(Some(" "), home, &home.join(".claude")), None);
    }

    #[test]
    fn pin_is_none_when_the_inherited_dir_is_d() {
        let home = Path::new("/Users/example");
        let d = Path::new("/Users/example/.claude.orca");
        assert_eq!(
            config_dir_pin(Some("/Users/example/.claude.orca"), home, d),
            None
        );
    }

    #[test]
    fn pin_sets_d_when_the_inherited_dir_differs() {
        let home = Path::new("/Users/example");
        let d = Path::new("/Users/example/.claude");
        assert_eq!(
            config_dir_pin(Some("/Users/example/.claude.work"), home, d),
            Some(d.to_path_buf())
        );
        let d2 = Path::new("/Users/example/.claude.orca");
        assert_eq!(config_dir_pin(None, home, d2), Some(d2.to_path_buf()));
    }

    // ── orca_config_dir_pin ───────────────────────────────────────────────────

    /// Orca main runs without `CLAUDE_CONFIG_DIR`: a pane that inherited one
    /// (a login dotfile) must drop it, never set `~/.claude` explicitly.
    #[test]
    fn orca_unset_removes_an_inherited_dir() {
        assert_eq!(
            orca_config_dir_pin(Some("/Users/example/.claude.work"), None),
            ConfigDirPin::Unset
        );
        assert_eq!(
            orca_config_dir_pin(Some("/Users/example/.claude"), None),
            ConfigDirPin::Unset
        );
        assert_eq!(orca_config_dir_pin(Some(" "), None), ConfigDirPin::Unset);
        assert_eq!(orca_config_dir_pin(None, None), ConfigDirPin::Leave);
    }

    #[test]
    fn orca_set_pins_its_own_value() {
        let x = Path::new("/Users/example/.claude.orca");
        assert_eq!(
            orca_config_dir_pin(Some("/Users/example/.claude.orca"), Some(x)),
            ConfigDirPin::Leave
        );
        assert_eq!(
            orca_config_dir_pin(Some("/Users/example/.claude.work"), Some(x)),
            ConfigDirPin::Set(x.to_path_buf())
        );
        assert_eq!(
            orca_config_dir_pin(None, Some(x)),
            ConfigDirPin::Set(x.to_path_buf())
        );
    }

    #[test]
    fn pin_conversion() {
        let d = PathBuf::from("/Users/example/.claude");
        assert_eq!(ConfigDirPin::from(None), ConfigDirPin::Leave);
        assert_eq!(ConfigDirPin::from(Some(d.clone())), ConfigDirPin::Set(d));
    }

    // ── auth_env_to_strip ─────────────────────────────────────────────────────

    #[test]
    fn strips_the_four_auth_vars_and_credential_headers_only() {
        let vars = [
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_AUTH_TOKEN", "t"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "o"),
            ("AWS_BEARER_TOKEN_BEDROCK", "b"),
            ("ANTHROPIC_CUSTOM_HEADERS", "Authorization: Bearer x"),
            ("PATH", "/usr/bin"),
            ("anthropic_api_key", "k"),
        ];
        let out = auth_env_to_strip(vars, false);
        assert_eq!(out.len(), 5, "{out:?}");
        assert!(!out.contains(&"anthropic_api_key".to_string()));
    }

    #[test]
    fn benign_custom_headers_stay() {
        let out = auth_env_to_strip([("ANTHROPIC_CUSTOM_HEADERS", "X-Trace: 1")], false);
        assert!(out.is_empty());
    }

    #[test]
    fn windows_matches_names_case_insensitively() {
        let out = auth_env_to_strip(
            [
                ("anthropic_api_key", "k"),
                ("Anthropic_Custom_Headers", "x-api-key: 1"),
            ],
            true,
        );
        assert_eq!(out, vec!["anthropic_api_key", "Anthropic_Custom_Headers"]);
    }
}
