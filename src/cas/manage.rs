//! Registry-management verbs (`list`/`add`/`set`/`remove`/`use`/`edit`) — the
//! non-eval half of `cas`. See the `cas` module doc for the overall contract.

use crate::account::profiles::ProfileMap;

use super::edit;
use super::eval::{print_status, resolve_profile};
use super::platform;
use super::types::{Op, Shell};
use super::write_default_profile;

/// Perform a registry-management op (`list`/`add`/`set`/`remove`/`use`) and
/// print human-readable output. These ops are **non-eval** — the shim calls
/// `csm cas <verb>` directly (no `--eval`), so we print status text, not an
/// export line. The caller (`cmd_cas`) routes here when `eval_mode == false`
/// and the op is not `Status`.
///
/// `profiles` is loaded fresh (and mutably) by the caller so writes persist.
pub fn manage_emit(op: &Op, profiles: &mut ProfileMap) -> anyhow::Result<()> {
    match op {
        Op::List => {
            print_status(&mut std::io::stdout(), Shell::Zsh, profiles)?;
        }

        Op::Add { name, dir } => {
            if !ProfileMap::is_valid_name(name) {
                anyhow::bail!(
                    "add: invalid profile name '{name}' (allowed: letters, digits, . _ -)"
                );
            }
            if profiles.contains(name) {
                anyhow::bail!(
                    "add: profile '{name}' already exists ({}). Use `csm profiles set {name} <dir>` to change its dir.",
                    profiles.get(name).unwrap_or("")
                );
            }
            let dir = resolve_new_dir(name, dir.as_deref());
            std::fs::create_dir_all(&dir)
                .map_err(|e| anyhow::anyhow!("add: cannot create dir '{dir}': {e}"))?;
            // Provision the new profile (dir + plugins → shared SSOT) so it is
            // consistent the moment it is registered, not only on first launch.
            crate::provision::ensure_provisioned_soft(std::path::Path::new(&dir));
            profiles.insert(name.clone(), dir.clone());
            profiles.save()?;
            eprintln!("added profile '{name}' → {dir}");
        }

        Op::Set { name, dir } => {
            if !ProfileMap::is_valid_name(name) {
                anyhow::bail!(
                    "set: invalid profile name '{name}' (allowed: letters, digits, . _ -)"
                );
            }
            if dir.is_empty() {
                anyhow::bail!("set: <dir> is required");
            }
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("set: cannot create dir '{dir}': {e}"))?;
            crate::provision::ensure_provisioned_soft(std::path::Path::new(dir));
            let prev = profiles.insert(name.clone(), dir.clone());
            profiles.save()?;
            match prev {
                Some(old) if old != *dir => eprintln!("set profile '{name}' → {dir} (was {old})"),
                _ => eprintln!("set profile '{name}' → {dir}"),
            }
        }

        Op::Remove { name } => {
            if !profiles.contains(name) {
                anyhow::bail!(
                    "remove: no such profile '{name}' — configured: {}",
                    profiles.names_sorted().join(", ")
                );
            }
            // Refuse to orphan the global default (the dir on disk is retained).
            if profiles.default_name() == *name {
                anyhow::bail!(
                    "remove: '{name}' is the global default — set the default elsewhere first \
                     (`csm profiles use <other>`)"
                );
            }
            let dir = profiles.remove(name).unwrap_or_default();
            profiles.save()?;
            eprintln!("removed profile '{name}' (dir retained on disk: {dir})");
        }

        Op::SetDefault { name } => {
            // Validate against the live registry, write the state file, and set
            // the platform floor (launchctl/HKCU) so new shells + GUI/launchd
            // pick it up. Unlike `-g` this emits NO per-shell export line.
            write_default_profile(name, profiles)?;
            let dir = resolve_profile(name, profiles)?;
            if let Err(e) = platform::apply_global(name, &dir) {
                eprintln!("cas: platform setenv warning: {e}");
            }
            crate::provision::ensure_provisioned_soft(std::path::Path::new(&dir));
            eprintln!("global default → {name} ({dir})");
            eprintln!(
                "(new shells + GUI/launchd follow this; your current shell keeps its profile until you run `cas {name}` or open a new shell)"
            );
        }

        Op::Edit => {
            // Interactive editor — loads/saves the registry through the same
            // `profiles` map (the caller passes it mutable so writes persist).
            edit::run_interactive(profiles)?;
        }

        // Switch/Global/Resync/Minus/Status are handled by `eval_emit`; routing
        // in `cmd_cas` guarantees they never reach here.
        Op::Switch { .. } | Op::Global { .. } | Op::Resync | Op::Minus | Op::Status { .. } => {
            anyhow::bail!("internal: {op:?} is not a management op");
        }
    }
    Ok(())
}

/// Resolve the dir for `cas add`: explicit `dir` if given, else the
/// conventional `~/.claude.<name>`.
fn resolve_new_dir(name: &str, dir: Option<&str>) -> String {
    match dir {
        Some(d) if !d.is_empty() => d.to_owned(),
        _ => crate::paths::synthesize_profile_dir(name)
            .to_string_lossy()
            .into_owned(),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── registry management: resolve_new_dir ──────────────────────────────────

    #[test]
    fn resolve_new_dir_explicit_wins() {
        assert_eq!(
            resolve_new_dir("work", Some("/custom/work")),
            "/custom/work"
        );
    }

    #[test]
    fn resolve_new_dir_synthesizes_conventional() {
        // Empty / None dir → ~/.claude.<name>.
        let got = resolve_new_dir("work", None);
        assert!(got.ends_with(".claude.work"), "got: {got}");
        let got = resolve_new_dir("work", Some(""));
        assert!(got.ends_with(".claude.work"), "got: {got}");
    }
}
