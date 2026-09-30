//! `csm config` — csm's own global config (`~/.config/claude-smart/
//! config.json`).

use std::ffi::OsString;

use anyhow::Context as _;

use crate::config;

/// `csm config <verb> ...`
///
/// Verbs (noun-verb, mirroring `cmd_profiles`):
///   show                         print the config JSON (bare `csm config` ≡ show)
///   get   launch-command         print the resolved launch command tokens
///   set   launch-command <c>...  launch `<c> …` instead of `claude` (drop-in)
///   unset launch-command         clear the override (revert to `claude`)
///   get   min-claude-version     print the effective floor
///   set   min-claude-version <v> the lowest claude version a limit switch
///                                accepts while an unsupervised claude is live
///   unset min-claude-version     back to the default (2.1.283)
///   get   idle-compact           print the effective mode (off/dry-run/on)
///   set   idle-compact <mode>    off|dry-run|on — see `idle_compact`
///   unset idle-compact           back to the default (off)
pub(crate) fn cmd_config(args: &[OsString]) -> anyhow::Result<()> {
    let verb = args.first().map(|a| a.to_string_lossy().into_owned());
    let key = args.get(1).map(|a| a.to_string_lossy().into_owned());
    // Remaining tokens (after `<verb> <key>`) are the launch-command argv.
    let rest: Vec<String> = args
        .iter()
        .skip(2)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    /// Reject any key but the three exposed ones.
    fn require_key(key: Option<&str>, verb: &str) -> anyhow::Result<()> {
        match key {
            Some("launch-command" | "min-claude-version" | "idle-compact") => Ok(()),
            Some(other) => anyhow::bail!(
                "csm config {verb}: unknown key '{other}' (expected launch-command, min-claude-version or idle-compact)"
            ),
            None => anyhow::bail!(
                "csm config {verb}: missing key (expected launch-command, min-claude-version or idle-compact)"
            ),
        }
    }
    let floor = key.as_deref() == Some("min-claude-version");
    let idle_compact = key.as_deref() == Some("idle-compact");

    match verb.as_deref() {
        None | Some("show") => {
            let cfg = config::Config::load().context("csm config: failed to load config.json")?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        Some("get") if floor => {
            let cfg = config::Config::load().unwrap_or_default();
            println!("{}", cfg.min_claude_version());
        }
        Some("set") if floor => {
            let Some(v) = rest.first() else {
                anyhow::bail!("csm config set min-claude-version: missing version (e.g. 2.1.283)");
            };
            if config::parse_version(v).is_none() {
                anyhow::bail!("csm config set min-claude-version: '{v}' is not a version");
            }
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.min_claude_version = Some(v.clone());
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("unset") if floor => {
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.min_claude_version = None;
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("get") if idle_compact => {
            let cfg = config::Config::load().unwrap_or_default();
            println!("{}", cfg.idle_compact_mode().as_str());
        }
        Some("set") if idle_compact => {
            let Some(v) = rest.first() else {
                anyhow::bail!("csm config set idle-compact: missing mode (off|dry-run|on)");
            };
            if config::IdleCompactMode::parse(v).is_none() {
                anyhow::bail!(
                    "csm config set idle-compact: '{v}' is not a mode (expected off, dry-run or on)"
                );
            }
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.idle_compact = Some(v.clone());
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("unset") if idle_compact => {
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.idle_compact = None;
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("get") => {
            require_key(key.as_deref(), "get")?;
            // Print the EFFECTIVE launch command (env > config > "claude"),
            // space-joined, so users see what `csm run` will actually spawn.
            let tokens = config::resolve_launch_command();
            let joined: Vec<String> = tokens
                .iter()
                .map(|t| t.to_string_lossy().into_owned())
                .collect();
            println!("{}", joined.join(" "));
        }
        Some("set") => {
            require_key(key.as_deref(), "set")?;
            if rest.is_empty() {
                anyhow::bail!(
                    "csm config set launch-command: missing command \
                     (e.g. `csm config set launch-command happy`)"
                );
            }
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.launch_command = rest;
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("unset") => {
            require_key(key.as_deref(), "unset")?;
            let mut cfg = config::Config::load().unwrap_or_default();
            cfg.launch_command.clear();
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some(other) => {
            anyhow::bail!("csm config: unknown verb '{other}' (expected show|get|set|unset)");
        }
    }
    Ok(())
}
