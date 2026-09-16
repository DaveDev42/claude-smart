//! `csm config` — csm's own global config (`~/.config/claude-smart/
//! config.json`), distinct from the `~/.config/claude-as/` profile contract.

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
///
/// Only the `launch-command` key is exposed today; the grammar leaves room for
/// future keys without changing the verb shape.
pub(crate) fn cmd_config(args: &[OsString]) -> anyhow::Result<()> {
    let verb = args.first().map(|a| a.to_string_lossy().into_owned());
    let key = args.get(1).map(|a| a.to_string_lossy().into_owned());
    // Remaining tokens (after `<verb> <key>`) are the launch-command argv.
    let rest: Vec<String> = args
        .iter()
        .skip(2)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    /// Reject any key other than the single one we expose today.
    fn require_launch_command(key: Option<&str>, verb: &str) -> anyhow::Result<()> {
        match key {
            Some("launch-command") => Ok(()),
            Some(other) => {
                anyhow::bail!("csm config {verb}: unknown key '{other}' (expected launch-command)")
            }
            None => anyhow::bail!("csm config {verb}: missing key (expected launch-command)"),
        }
    }

    match verb.as_deref() {
        None | Some("show") => {
            let cfg = config::Config::load().context("csm config: failed to load config.json")?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        Some("get") => {
            require_launch_command(key.as_deref(), "get")?;
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
            require_launch_command(key.as_deref(), "set")?;
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
            require_launch_command(key.as_deref(), "unset")?;
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
