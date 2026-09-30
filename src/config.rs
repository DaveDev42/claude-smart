//! `csm`'s own global config — `~/.config/claude-smart/config.json`.
//!
//! This file holds csm's own runtime settings: the drop-in **launch command**
//! (run `happy`/`tp` instead of `claude`) and the **minimum claude version**
//! a limit switch requires of sessions csm does not supervise.
//!
//! Schema (JSON):
//! ```json
//! { "launchCommand": ["happy"], "minClaudeVersion": "2.1.283" }
//! ```
//! `launchCommand` is an argv token array, not a shell line: the first token is
//! the binary, any remaining tokens are prepended to the claude-style argv on
//! every spawn (e.g. `["npx", "happy"]`). Tokens are never shell-split.
//!
//! Pure core + thin I/O seam: `load_from`/`save_to` take an explicit path; `load`/`save` use the canonical
//! one. An absent file is **not** an error (returns [`Config::default`], which
//! launches the literal `claude`); a corrupt file is an `Err` at the seam, and
//! the launch path chooses leniency via `unwrap_or_default`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Global `csm` configuration. See the module docs for the on-disk schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Drop-in launch command as argv tokens. First token = binary; remaining
    /// tokens are prepended to the claude-style argv on every spawn. Empty /
    /// absent → launch the literal `claude`. e.g. `["happy"]`, `["tp"]`,
    /// `["npx", "happy"]`.
    #[serde(
        default,
        rename = "launchCommand",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub launch_command: Vec<String>,

    /// The lowest `claude --version` a limit switch accepts while a claude
    /// csm does not supervise is live in `D` (design §4 "Unsupervised
    /// sessions"). Absent → [`DEFAULT_MIN_CLAUDE_VERSION`].
    #[serde(
        default,
        rename = "minClaudeVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_claude_version: Option<String>,

    /// The idle-compact delivery mode: `"off"` | `"dry-run"` | `"on"` (design
    /// `docs/superpowers/specs/`-companion issue 36). Absent →
    /// [`DEFAULT_IDLE_COMPACT_MODE`] (`"off"`). Stored as the raw string
    /// (not the enum) so a value an older binary wrote round-trips even if
    /// this binary does not recognise it — [`Config::idle_compact_mode`] is
    /// the only place that interprets it, and falls back to `Off` rather
    /// than erroring on an unrecognised value.
    #[serde(
        default,
        rename = "idleCompact",
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_compact: Option<String>,

    /// Absorb any unknown keys written by future binary versions so a rollback
    /// to an older binary does not destroy unrecognised fields (mirrors
    /// `Sidecar.extra`).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Config {
    /// Load from the canonical path `~/.config/claude-smart/config.json`.
    /// Absent file → `Ok(Config::default())`. Parse error → `Err`.
    pub fn load() -> io::Result<Self> {
        Self::load_from(&crate::paths::config_json())
    }

    /// Load from an explicit path (testable seam).
    pub fn load_from(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config.json parse error at {}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomically serialize to the canonical path.
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&crate::paths::config_json())
    }

    /// Atomic serialize to `path` (testable seam). tmp+rename in the SAME dir
    /// (same filesystem → atomic), trailing newline.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{json}\n"))?;
        std::fs::rename(&tmp, path)
    }

    /// The configured launch command as argv tokens, or `None` when unset.
    /// Empty vec is treated as unset (→ caller uses the `claude` default).
    pub fn launch_command(&self) -> Option<Vec<OsString>> {
        if self.launch_command.is_empty() {
            None
        } else {
            Some(self.launch_command.iter().map(OsString::from).collect())
        }
    }
}

/// The default [`Config::min_claude_version`]: the first Claude Code release
/// whose strings show the refresh lock and sibling-token adoption.
pub const DEFAULT_MIN_CLAUDE_VERSION: &str = "2.1.283";

impl Config {
    /// The effective minimum claude version.
    pub fn min_claude_version(&self) -> &str {
        self.min_claude_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_MIN_CLAUDE_VERSION)
    }
}

/// The idle-compact feature's delivery mode (`csm config set idle-compact
/// off|dry-run|on`). `Off` (the default) never evaluates the trigger
/// conditions at all; `DryRun` evaluates and logs every idle period but
/// never sends `/compact` or claims the fire marker before delivery; `On`
/// delivers. See `idle_compact` for the decision function this drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleCompactMode {
    Off,
    DryRun,
    On,
}

impl IdleCompactMode {
    /// Parse the on-disk/CLI string form. `None` for anything else — the
    /// caller decides whether that means "reject the input" (`csm config
    /// set`) or "fall back to the default" (`Config::idle_compact_mode`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "off" => Some(Self::Off),
            "dry-run" => Some(Self::DryRun),
            "on" => Some(Self::On),
            _ => None,
        }
    }

    /// The canonical string form — round-trips through [`Self::parse`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dry-run",
            Self::On => "on",
        }
    }
}

/// [`Config::idle_compact`]'s default when unset.
pub const DEFAULT_IDLE_COMPACT_MODE: &str = "off";

impl Config {
    /// The effective idle-compact mode: the configured string, parsed —
    /// unset, blank, or an unrecognised value all fall back to
    /// [`DEFAULT_IDLE_COMPACT_MODE`] rather than erroring, matching
    /// [`Config::min_claude_version`]'s leniency.
    pub fn idle_compact_mode(&self) -> IdleCompactMode {
        let raw = self
            .idle_compact
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_IDLE_COMPACT_MODE);
        IdleCompactMode::parse(raw).unwrap_or(IdleCompactMode::Off)
    }
}

/// Parse a dotted numeric version (`2.1.283`, a leading `v` allowed, any
/// `-pre`/`+build` suffix ignored). Pure.
pub fn parse_version(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+', ' ']).next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

/// The first version-looking token of `claude --version` output
/// (`2.1.283 (Claude Code)`). Pure.
pub fn version_from_output(out: &str) -> Option<String> {
    out.split_whitespace()
        .find(|t| parse_version(t).is_some_and(|p| p.len() >= 2))
        .map(|t| t.trim_start_matches(['v', 'V']).to_owned())
}

/// `Some(true)` when `v >= floor`, `None` when either does not parse. Pure.
pub fn version_at_least(v: &str, floor: &str) -> Option<bool> {
    let (a, b) = (parse_version(v)?, parse_version(floor)?);
    let n = a.len().max(b.len());
    let at = |x: &[u64], i: usize| x.get(i).copied().unwrap_or(0);
    for i in 0..n {
        if at(&a, i) != at(&b, i) {
            return Some(at(&a, i) > at(&b, i));
        }
    }
    Some(true)
}

/// Resolve the launch command argv for a spawn, reading the env + config file.
///
/// Precedence (highest → lowest):
///   1. `CLAUDE_SMART_CLAUDE_BIN` env (a single binary; tests / floors).
///   2. `config.json` `launchCommand` tokens (the drop-in alternative).
///   3. default `["claude"]`.
///
/// Always returns at least one token: `out[0]` is the binary for
/// `Command::new`, and `out[1..]` are argv tokens to PREPEND to the
/// claude-style `cli` args. A config parse error degrades to the default
/// (`unwrap_or_default`) rather than aborting the launch.
///
/// **Recursion guard.** csm may itself be installed as `claude` (the alias
/// `csm orca setup` creates). When the binary `out[0]` names resolves to csm,
/// that candidate is skipped and the next one on `PATH` with the same name is
/// used instead ([`skip_self`]). When every candidate is csm, the tokens come
/// back unchanged; [`launch_command_for_spawn`] refuses that case.
pub fn resolve_launch_command() -> Vec<OsString> {
    let mut tokens = configured_launch_command();
    if let SelfCheck::Replaced(p) = self_check(&tokens[0]) {
        tokens[0] = p.into_os_string();
    }
    tokens
}

/// The configured launch command with no `PATH` lookup (name matching, where
/// only the basename matters).
pub fn configured_launch_command() -> Vec<OsString> {
    resolve_launch_command_with(
        std::env::var_os("CLAUDE_SMART_CLAUDE_BIN"),
        &Config::load().unwrap_or_default(),
    )
}

/// [`resolve_launch_command`] for a spawn: refuses when every candidate is
/// csm itself, which would exec csm in a loop.
pub fn launch_command_for_spawn() -> io::Result<Vec<OsString>> {
    let tokens = configured_launch_command();
    match self_check(&tokens[0]) {
        SelfCheck::NotSelf => Ok(tokens),
        SelfCheck::Replaced(p) => {
            let mut out = tokens;
            out[0] = p.into_os_string();
            Ok(out)
        }
        SelfCheck::OnlySelf => Err(io::Error::other(format!(
            "every {:?} on PATH is csm itself; install claude or set `csm config set launch-command`",
            tokens[0].to_string_lossy()
        ))),
    }
}

/// The real `claude` binary, never csm: the bare name, or the next `PATH`
/// entry when the first one is csm. For the `claude --version` floor probe,
/// which asks claude itself, not a configured drop-in launcher. (csm's
/// `claude auth` calls use the launch command: `orca::add::SystemClaude::configured`.)
pub fn real_claude_program() -> io::Result<PathBuf> {
    let name = std::ffi::OsStr::new("claude");
    match self_check(name) {
        SelfCheck::NotSelf => Ok(PathBuf::from(name)),
        SelfCheck::Replaced(p) => Ok(p),
        SelfCheck::OnlySelf => Err(io::Error::other(
            "every `claude` on PATH is csm itself; install Claude Code",
        )),
    }
}

/// Pure precedence resolver (no env/file I/O) — the testable seam behind
/// [`resolve_launch_command`]. See it for the precedence contract.
pub fn resolve_launch_command_with(env_bin: Option<OsString>, cfg: &Config) -> Vec<OsString> {
    if let Some(bin) = env_bin {
        return vec![bin];
    }
    cfg.launch_command()
        .unwrap_or_else(|| vec![OsString::from("claude")])
}

// ─── recursion guard ──────────────────────────────────────────────────────────

/// What [`skip_self`] found for a launch binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfCheck {
    /// The binary is not csm (or cannot be found): use the token as given.
    NotSelf,
    /// The first match was csm; this later `PATH` entry is not.
    Replaced(PathBuf),
    /// Every match is csm.
    OnlySelf,
}

/// Pure core of the recursion guard.
///
/// `token` is the launch binary. A token with a directory part is checked as
/// written and, when it is csm, looked up again by file name on `path_dirs`.
/// A bare name is looked up on `path_dirs` in order, with `exe_suffix`
/// appended when the name has no extension (`.exe` on Windows, the way
/// `Command` resolves it). Candidates that are not files are skipped.
pub fn skip_self(
    token: &std::ffi::OsStr,
    path_dirs: &[PathBuf],
    exe_suffix: &str,
    is_file: &dyn Fn(&Path) -> bool,
    is_self: &dyn Fn(&Path) -> bool,
) -> SelfCheck {
    let as_path = Path::new(token);
    let mut saw_self = false;
    if as_path.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        if !is_self(as_path) {
            return SelfCheck::NotSelf;
        }
        saw_self = true;
    }
    let Some(file_name) = as_path.file_name() else {
        return SelfCheck::NotSelf;
    };
    let mut name = file_name.to_os_string();
    if !exe_suffix.is_empty() && Path::new(&name).extension().is_none() {
        name.push(exe_suffix);
    }
    for dir in path_dirs {
        let candidate = dir.join(&name);
        if !is_file(&candidate) {
            continue;
        }
        if is_self(&candidate) {
            saw_self = true;
            continue;
        }
        return if saw_self {
            SelfCheck::Replaced(candidate)
        } else {
            SelfCheck::NotSelf
        };
    }
    if saw_self {
        SelfCheck::OnlySelf
    } else {
        SelfCheck::NotSelf
    }
}

/// I/O shell: run [`skip_self`] against the real `PATH` and this executable.
fn self_check(token: &std::ffi::OsStr) -> SelfCheck {
    let Ok(me) = std::env::current_exe() else {
        return SelfCheck::NotSelf;
    };
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    skip_self(token, &dirs, suffix, &|p| p.is_file(), &|p| {
        same_file(p, &me)
    })
}

/// Is `a` the same executable as `b`? Canonical paths, then (unix) the same
/// inode, then identical bytes — a Windows hardlink or copy named
/// `claude.exe` canonicalizes to its own path.
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) else {
        return false;
    };
    if ca == cb {
        return true;
    }
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(&ca), std::fs::metadata(&cb)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
            return true;
        }
    }
    if ma.len() != mb.len() {
        return false;
    }
    match (std::fs::read(&ca), std::fs::read(&cb)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parse(json: &str) -> Config {
        serde_json::from_str(json).expect("valid json")
    }

    // ── idle_compact mode ─────────────────────────────────────────────────────

    #[test]
    fn idle_compact_mode_defaults_to_off() {
        assert_eq!(Config::default().idle_compact_mode(), IdleCompactMode::Off);
    }

    #[test]
    fn idle_compact_mode_parses_every_valid_value() {
        for (raw, want) in [
            ("off", IdleCompactMode::Off),
            ("dry-run", IdleCompactMode::DryRun),
            ("on", IdleCompactMode::On),
        ] {
            let cfg = Config {
                idle_compact: Some(raw.to_owned()),
                ..Default::default()
            };
            assert_eq!(cfg.idle_compact_mode(), want, "raw={raw}");
            assert_eq!(want.as_str(), raw);
        }
    }

    #[test]
    fn idle_compact_mode_unrecognised_value_falls_back_to_off() {
        let cfg = Config {
            idle_compact: Some("banana".to_owned()),
            ..Default::default()
        };
        assert_eq!(cfg.idle_compact_mode(), IdleCompactMode::Off);
    }

    #[test]
    fn idle_compact_mode_blank_value_falls_back_to_off() {
        let cfg = Config {
            idle_compact: Some("   ".to_owned()),
            ..Default::default()
        };
        assert_eq!(cfg.idle_compact_mode(), IdleCompactMode::Off);
    }

    #[test]
    fn idle_compact_mode_serializes_only_when_set() {
        assert_eq!(serde_json::to_string(&Config::default()).unwrap(), "{}");
        let cfg = Config {
            idle_compact: Some("dry-run".to_owned()),
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains(r#""idleCompact":"dry-run""#), "got {s}");
    }

    #[test]
    fn idle_compact_json_key_is_camel_case() {
        let cfg = parse(r#"{"idleCompact": "on"}"#);
        assert_eq!(cfg.idle_compact_mode(), IdleCompactMode::On);
    }

    #[test]
    fn min_claude_version_defaults_and_overrides() {
        assert_eq!(Config::default().min_claude_version(), "2.1.283");
        let cfg = parse(r#"{"minClaudeVersion":"2.2.0"}"#);
        assert_eq!(cfg.min_claude_version(), "2.2.0");
        let blank = parse(r#"{"minClaudeVersion":"  "}"#);
        assert_eq!(blank.min_claude_version(), "2.1.283");
    }

    #[test]
    fn version_compare() {
        assert_eq!(version_at_least("2.1.283", "2.1.283"), Some(true));
        assert_eq!(version_at_least("2.1.284", "2.1.283"), Some(true));
        assert_eq!(version_at_least("2.2", "2.1.283"), Some(true));
        assert_eq!(version_at_least("2.1.99", "2.1.283"), Some(false));
        assert_eq!(version_at_least("v3.0.0-beta", "2.1.283"), Some(true));
        assert_eq!(version_at_least("garbage", "2.1.283"), None);
    }

    #[test]
    fn version_from_claude_output() {
        assert_eq!(
            version_from_output("2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(version_from_output("Claude Code"), None);
    }

    #[test]
    fn absent_file_is_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.launch_command.is_empty());
        assert_eq!(cfg.launch_command(), None);
    }

    #[test]
    fn empty_object_is_default() {
        let cfg = parse("{}");
        assert!(cfg.launch_command.is_empty());
        assert_eq!(cfg.launch_command(), None);
    }

    #[test]
    fn single_token_roundtrip() {
        let cfg = parse(r#"{"launchCommand": ["happy"]}"#);
        assert_eq!(cfg.launch_command(), Some(vec![OsString::from("happy")]));
    }

    #[test]
    fn multi_token_roundtrip() {
        let cfg = parse(r#"{"launchCommand": ["npx", "happy"]}"#);
        assert_eq!(
            cfg.launch_command(),
            Some(vec![OsString::from("npx"), OsString::from("happy")])
        );
    }

    #[test]
    fn save_to_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = Config {
            launch_command: vec!["tp".to_owned()],
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'), "config.json must end with newline");
        // No leftover tmp sibling (atomic rename).
        assert!(!path.with_extension("json.tmp").exists());

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.launch_command(), Some(vec![OsString::from("tp")]));
    }

    #[test]
    fn default_config_serializes_as_empty_object() {
        // skip_serializing_if keeps a default write clean (good Ansible diff).
        let s = serde_json::to_string(&Config::default()).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn unknown_future_keys_preserved() {
        let cfg = parse(r#"{"launchCommand": ["happy"], "futureKey": 42}"#);
        assert_eq!(cfg.extra.get("futureKey"), Some(&serde_json::json!(42)));
        // Reserialize still carries the unknown key (rollback safety).
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains("futureKey"), "unknown key dropped: {s}");
    }

    #[test]
    fn invalid_json_is_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(Config::load_from(&path).is_err());
    }

    // ─── resolver precedence (pure seam, no env/file I/O) ───────────────────────

    #[test]
    fn env_override_wins_over_config() {
        let cfg = parse(r#"{"launchCommand": ["happy"]}"#);
        let out = resolve_launch_command_with(Some(OsString::from("xtest")), &cfg);
        assert_eq!(out, vec![OsString::from("xtest")]);
    }

    #[test]
    fn config_used_when_no_env() {
        let cfg = parse(r#"{"launchCommand": ["happy"]}"#);
        let out = resolve_launch_command_with(None, &cfg);
        assert_eq!(out, vec![OsString::from("happy")]);
    }

    #[test]
    fn multi_token_config_passthrough() {
        let cfg = parse(r#"{"launchCommand": ["npx", "happy"]}"#);
        let out = resolve_launch_command_with(None, &cfg);
        assert_eq!(out, vec![OsString::from("npx"), OsString::from("happy")]);
    }

    #[test]
    fn default_claude_when_nothing_set() {
        let out = resolve_launch_command_with(None, &Config::default());
        assert_eq!(out, vec![OsString::from("claude")]);
    }

    // ── recursion guard ───────────────────────────────────────────────────────

    fn dirs(ds: &[&str]) -> Vec<PathBuf> {
        ds.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn skip_self_leaves_a_real_claude_alone() {
        let d = dirs(&["/a", "/b"]);
        let r = skip_self(
            std::ffi::OsStr::new("claude"),
            &d,
            "",
            &|p| p == Path::new("/a/claude"),
            &|_| false,
        );
        assert_eq!(r, SelfCheck::NotSelf);
    }

    #[test]
    fn skip_self_moves_past_csm_to_the_next_path_entry() {
        let d = dirs(&["/alias", "/missing", "/real"]);
        let r = skip_self(
            std::ffi::OsStr::new("claude"),
            &d,
            "",
            &|p| p == Path::new("/alias/claude") || p == Path::new("/real/claude"),
            &|p| p == Path::new("/alias/claude"),
        );
        assert_eq!(r, SelfCheck::Replaced(PathBuf::from("/real/claude")));
    }

    #[test]
    fn skip_self_reports_only_self() {
        let d = dirs(&["/alias"]);
        let r = skip_self(std::ffi::OsStr::new("claude"), &d, "", &|_| true, &|_| true);
        assert_eq!(r, SelfCheck::OnlySelf);
    }

    #[test]
    fn skip_self_checks_an_explicit_path_then_searches_by_name() {
        let d = dirs(&["/real"]);
        let r = skip_self(
            std::ffi::OsStr::new("/Users/example/.local/state/csm/bin/claude"),
            &d,
            "",
            &|p| p == Path::new("/real/claude"),
            &|p| p.starts_with("/Users/example"),
        );
        assert_eq!(r, SelfCheck::Replaced(PathBuf::from("/real/claude")));
        // An explicit path that is not csm is used as written.
        let r = skip_self(
            std::ffi::OsStr::new("/opt/claude/bin/claude"),
            &d,
            "",
            &|_| true,
            &|_| false,
        );
        assert_eq!(r, SelfCheck::NotSelf);
    }

    #[test]
    fn skip_self_appends_the_exe_suffix_to_a_bare_name() {
        let d = dirs(&["/alias", "/real"]);
        let r = skip_self(
            std::ffi::OsStr::new("claude"),
            &d,
            ".exe",
            &|p| p.extension().is_some_and(|e| e == "exe"),
            &|p| p.starts_with("/alias"),
        );
        assert_eq!(r, SelfCheck::Replaced(PathBuf::from("/real/claude.exe")));
    }

    #[test]
    fn same_file_detects_a_copy_and_rejects_a_different_file() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        std::fs::write(&a, b"binary").unwrap();
        std::fs::write(&b, b"binary").unwrap();
        std::fs::write(&c, b"other!").unwrap();
        assert!(same_file(&a, &a));
        assert!(same_file(&a, &b));
        assert!(!same_file(&a, &c));
        assert!(!same_file(&a, &dir.path().join("missing")));
    }
}
