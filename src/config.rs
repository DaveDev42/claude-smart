//! `csm`'s own global config — `~/.config/claude-smart/config.json`.
//!
//! Distinct from `~/.config/claude-as/` (the profile-switch contract shared with
//! the `cas` shell shims): this file holds csm's own runtime settings, currently
//! just the drop-in **launch command** (run `happy`/`tp` instead of `claude`).
//!
//! Schema (JSON, like `profiles.json`):
//! ```json
//! { "launchCommand": ["happy"] }
//! ```
//! `launchCommand` is an argv token array, not a shell line: the first token is
//! the binary, any remaining tokens are prepended to the claude-style argv on
//! every spawn (e.g. `["npx", "happy"]`). Tokens are never shell-split.
//!
//! Pure core + thin I/O seam, mirroring [`crate::account::profiles::ProfileMap`]:
//! `load_from`/`save_to` take an explicit path; `load`/`save` use the canonical
//! one. An absent file is **not** an error (returns [`Config::default`], which
//! launches the literal `claude`); a corrupt file is an `Err` at the seam, and
//! the launch path chooses leniency via `unwrap_or_default`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;

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
    /// (same filesystem → atomic), trailing newline — same contract as
    /// `ProfileMap::save_to`.
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
pub fn resolve_launch_command() -> Vec<OsString> {
    resolve_launch_command_with(
        std::env::var_os("CLAUDE_SMART_CLAUDE_BIN"),
        &Config::load().unwrap_or_default(),
    )
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

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parse(json: &str) -> Config {
        serde_json::from_str(json).expect("valid json")
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
}
