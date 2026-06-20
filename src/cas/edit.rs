//! `csm profiles edit` — interactive registry editor.
//!
//! A menu loop over [`ProfileMap`] using `std::io::stdin().read_line` (NO fzf
//! dependency, so it behaves identically on Windows-native where fzf may be
//! absent). The logic is split for testability:
//!
//! - **pure core** — [`apply_edit_action`]: takes `&mut ProfileMap` + an
//!   [`Action`] and returns an [`Outcome`]. No I/O, no clock, no stdin. Every
//!   branch (add / dup-reject / invalid-name / edit-dir / rename / delete /
//!   set-default) is unit-tested with scripted `Action` sequences.
//! - **I/O shell** — [`run_interactive`]: TTY gate, render, read an [`Action`]
//!   from stdin, apply, persist immediately. The only untested part (kept thin).
//!
//! Persistence: every mutating action calls [`ProfileMap::save`] right away so a
//! mid-session Ctrl-C never corrupts the registry. The default-NAME state file
//! is written via the same `write_default_profile` + platform-floor path that
//! `csm profiles use` uses, so the interactive `set-default` is identical to the
//! scriptable one.
//!
//! # Spec reference
//! `docs/2026-06-19-csm-usage-and-interactive-cas-edit.md` §2.

use std::io::{self, Write};

use crate::account::profiles::ProfileMap;

// ─── action / outcome (pure-core vocabulary) ───────────────────────────────────

/// A single editor action, parsed from a menu choice + its prompted arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Register a new profile. `dir = None` → synthesize `~/.claude.<name>`.
    Add { name: String, dir: Option<String> },
    /// Change an existing profile's dir.
    EditDir { name: String, dir: String },
    /// Rename a profile (its dir is preserved; default follows if it was default).
    Rename { from: String, to: String },
    /// Unregister a profile (dir retained on disk; refused if it is the default).
    Delete { name: String },
    /// Set the global default to `name` (state file + platform floor).
    SetDefault { name: String },
    /// Leave the loop.
    Quit,
}

/// What [`apply_edit_action`] decided. The I/O shell turns this into a message
/// + persistence; the unit tests assert on it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The registry was mutated and should be persisted. `msg` is user-facing.
    /// `set_default` carries a profile name when the default-NAME state file
    /// must also be (re)written to it (rename-follows-default / set-default).
    Changed {
        msg: String,
        set_default: Option<String>,
    },
    /// Nothing changed; `msg` explains why (no-op or a recoverable user error).
    NoChange { msg: String },
    /// The user asked to quit.
    Quit,
}

// ─── pure core ─────────────────────────────────────────────────────────────────

/// Apply `action` to `profiles` in memory and report the [`Outcome`].
///
/// Pure: mutates only the passed map, performs no I/O (the caller persists on
/// `Changed`). Mirrors the validation rules of the scriptable `cas` verbs so the
/// interactive and non-interactive paths cannot diverge:
/// - add: valid name, reject duplicate.
/// - edit-dir: must exist.
/// - rename: valid new name, source exists, target free; if source was the
///   default, the new name is returned in `set_default`.
/// - delete: must exist; refuse if it is the current default.
/// - set-default: must exist (populated map); returned in `set_default`.
///
/// `mkdir` for add/edit-dir is the caller's job (an I/O side-effect); the core
/// only records the dir in the map.
pub fn apply_edit_action(profiles: &mut ProfileMap, action: Action) -> Outcome {
    match action {
        Action::Quit => Outcome::Quit,

        Action::Add { name, dir } => {
            if !ProfileMap::is_valid_name(&name) {
                return Outcome::NoChange {
                    msg: format!("invalid name '{name}' (allowed: letters, digits, . _ -)"),
                };
            }
            if profiles.contains(&name) {
                return Outcome::NoChange {
                    msg: format!("profile '{name}' already exists — use edit-dir to change it"),
                };
            }
            let resolved = synth_dir(&name, dir.as_deref());
            profiles.insert(name.clone(), resolved.clone());
            Outcome::Changed {
                msg: format!("added '{name}' → {resolved}"),
                set_default: None,
            }
        }

        Action::EditDir { name, dir } => {
            if !profiles.contains(&name) {
                return Outcome::NoChange {
                    msg: format!("no such profile '{name}'"),
                };
            }
            if dir.trim().is_empty() {
                return Outcome::NoChange {
                    msg: "dir cannot be empty".to_owned(),
                };
            }
            profiles.insert(name.clone(), dir.clone());
            Outcome::Changed {
                msg: format!("'{name}' → {dir}"),
                set_default: None,
            }
        }

        Action::Rename { from, to } => {
            if !profiles.contains(&from) {
                return Outcome::NoChange {
                    msg: format!("no such profile '{from}'"),
                };
            }
            if !ProfileMap::is_valid_name(&to) {
                return Outcome::NoChange {
                    msg: format!("invalid name '{to}' (allowed: letters, digits, . _ -)"),
                };
            }
            if from == to {
                return Outcome::NoChange {
                    msg: "name unchanged".to_owned(),
                };
            }
            if profiles.contains(&to) {
                return Outcome::NoChange {
                    msg: format!("target name '{to}' already exists"),
                };
            }
            let was_default = profiles.default_name() == from;
            let dir = profiles.remove(&from).unwrap_or_default();
            profiles.insert(to.clone(), dir);
            Outcome::Changed {
                msg: format!("renamed '{from}' → '{to}'"),
                // If the renamed profile was the global default, repoint it so
                // the default state file never dangles at a removed name.
                set_default: was_default.then(|| to.clone()),
            }
        }

        Action::Delete { name } => {
            if !profiles.contains(&name) {
                return Outcome::NoChange {
                    msg: format!("no such profile '{name}'"),
                };
            }
            if profiles.default_name() == name {
                return Outcome::NoChange {
                    msg: format!("'{name}' is the global default — set-default elsewhere first"),
                };
            }
            let dir = profiles.remove(&name).unwrap_or_default();
            Outcome::Changed {
                msg: format!("removed '{name}' (dir retained on disk: {dir})"),
                set_default: None,
            }
        }

        Action::SetDefault { name } => {
            // Populated map requires membership (empty/synth never reaches the
            // interactive editor — the TTY menu lists only existing profiles).
            if !profiles.contains(&name) {
                return Outcome::NoChange {
                    msg: format!("no such profile '{name}'"),
                };
            }
            Outcome::Changed {
                msg: format!("global default → {name}"),
                set_default: Some(name),
            }
        }
    }
}

/// Resolve the dir for an `Add`: explicit when non-empty, else `~/.claude.<name>`.
fn synth_dir(name: &str, dir: Option<&str>) -> String {
    match dir {
        Some(d) if !d.trim().is_empty() => d.trim().to_owned(),
        _ => dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(format!(".claude.{name}"))
            .to_string_lossy()
            .into_owned(),
    }
}

// ─── I/O shell ─────────────────────────────────────────────────────────────────

/// Run the interactive editor loop. Requires a TTY on both stdin and stdout.
///
/// `profiles` is loaded fresh and mutable by the caller (`cmd_profiles`), so
/// writes persist. Returns `Ok(())` after the user quits (or EOF). Each mutating
/// action persists immediately via [`ProfileMap::save`]; `set-default` also writes
/// the default-NAME state file and applies the platform floor.
pub fn run_interactive(profiles: &mut ProfileMap) -> anyhow::Result<()> {
    if !is_interactive() {
        anyhow::bail!(
            "csm profiles edit: requires an interactive terminal \
             (use `csm profiles add|set|rm|use` for scripting)"
        );
    }

    let stdin = io::stdin();
    loop {
        render_menu(profiles);
        let names = profiles.names_sorted();
        let names: Vec<String> = names.into_iter().map(str::to_owned).collect();

        print!("> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            // EOF (Ctrl-D) → quit cleanly.
            println!();
            break;
        }
        let choice = line.trim().to_ascii_lowercase();

        let action = match parse_choice(&choice, &names, &stdin) {
            Ok(Some(a)) => a,
            Ok(None) => continue, // unrecognized / blank → redraw
            Err(e) => {
                eprintln!("  {e}");
                continue;
            }
        };

        match apply_edit_action(profiles, action) {
            Outcome::Quit => break,
            Outcome::NoChange { msg } => println!("  {msg}"),
            Outcome::Changed { msg, set_default } => {
                profiles.save()?;
                if let Some(def) = set_default {
                    apply_default(&def, profiles)?;
                }
                // For add/edit-dir, create the dir on disk (best-effort).
                ensure_dirs(profiles);
                println!("  \u{2713} {msg}");
            }
        }
    }
    println!("done.");
    Ok(())
}

/// Map a menu key + interactive prompts into an [`Action`].
///
/// Returns `Ok(None)` for an unrecognized/blank choice (the loop redraws),
/// `Err` for a recoverable input error (the loop reports and redraws).
fn parse_choice(
    choice: &str,
    names: &[String],
    stdin: &io::Stdin,
) -> anyhow::Result<Option<Action>> {
    match choice {
        "q" | "quit" => Ok(Some(Action::Quit)),
        "a" | "add" => {
            let name = prompt(stdin, "  new profile name: ")?;
            if name.is_empty() {
                return Ok(None);
            }
            let dir = prompt(stdin, "  config dir (blank = ~/.claude.<name>): ")?;
            Ok(Some(Action::Add {
                name,
                dir: if dir.is_empty() { None } else { Some(dir) },
            }))
        }
        "e" | "edit" | "edit-dir" => {
            let name = pick(stdin, names, "edit-dir")?;
            match name {
                Some(name) => {
                    let dir = prompt(stdin, "  new config dir: ")?;
                    Ok(Some(Action::EditDir { name, dir }))
                }
                None => Ok(None),
            }
        }
        "r" | "rename" => {
            let from = pick(stdin, names, "rename")?;
            match from {
                Some(from) => {
                    let to = prompt(stdin, "  new name: ")?;
                    if to.is_empty() {
                        return Ok(None);
                    }
                    Ok(Some(Action::Rename { from, to }))
                }
                None => Ok(None),
            }
        }
        "d" | "delete" | "rm" => {
            let name = pick(stdin, names, "delete")?;
            Ok(name.map(|name| Action::Delete { name }))
        }
        "*" | "default" | "set-default" => {
            let name = pick(stdin, names, "set-default")?;
            Ok(name.map(|name| Action::SetDefault { name }))
        }
        _ => Ok(None),
    }
}

/// Prompt for a line of input, returning the trimmed string.
fn prompt(stdin: &io::Stdin, label: &str) -> anyhow::Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let mut s = String::new();
    stdin.read_line(&mut s)?;
    Ok(s.trim().to_owned())
}

/// Prompt the user to pick a profile by number (1-based) for `verb`.
/// Returns `Ok(None)` on a blank/invalid choice.
fn pick(stdin: &io::Stdin, names: &[String], verb: &str) -> anyhow::Result<Option<String>> {
    if names.is_empty() {
        println!("  (no profiles to {verb})");
        return Ok(None);
    }
    let raw = prompt(stdin, &format!("  {verb} which # (blank to cancel)? "))?;
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<usize>() {
        Ok(n) if n >= 1 && n <= names.len() => Ok(Some(names[n - 1].clone())),
        _ => {
            println!("  invalid selection '{raw}'");
            Ok(None)
        }
    }
}

/// Render the profile list + action menu.
fn render_menu(profiles: &ProfileMap) {
    let default = profiles.default_name();
    let names = profiles.names_sorted();
    println!();
    println!(
        "csm profiles edit — {} profile(s), default: {}",
        names.len(),
        if default.is_empty() {
            "(none)"
        } else {
            &default
        }
    );
    let current = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    for (i, name) in names.iter().enumerate() {
        let dir = profiles.get(name).unwrap_or("");
        let mut tags = String::new();
        if *name == default {
            tags.push_str(" [default]");
        }
        if !current.is_empty() && dir == current {
            tags.push_str(" [current shell]");
        }
        println!("  {}) {:<14} {}{}", i + 1, name, dir, tags);
    }
    if names.is_empty() {
        println!("  (none yet)");
    }
    println!("Actions: [a]dd  [e]dit-dir  [r]ename  [d]elete  [*]set-default  [q]uit");
}

/// Write the default-NAME state file + apply the platform floor (mirrors
/// `csm profiles use` / `cas use`).
fn apply_default(name: &str, profiles: &ProfileMap) -> anyhow::Result<()> {
    crate::cas::write_default_profile(name, profiles)?;
    if let Some(dir) = profiles.get(name) {
        if let Err(e) = crate::cas::platform::apply_global(name, dir) {
            eprintln!("  (platform floor warning: {e})");
        }
    }
    Ok(())
}

/// Best-effort: create each profile's config dir if missing.
fn ensure_dirs(profiles: &ProfileMap) {
    for (_, dir) in profiles.iter() {
        let _ = std::fs::create_dir_all(dir);
    }
}

/// True when both stdin and stdout are TTYs.
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn registry(pairs: &[(&str, &str)]) -> ProfileMap {
        let mut m = HashMap::new();
        for (n, d) in pairs {
            m.insert((*n).to_owned(), (*d).to_owned());
        }
        ProfileMap(m)
    }

    #[test]
    fn add_valid_inserts() {
        let mut p = registry(&[]);
        let out = apply_edit_action(
            &mut p,
            Action::Add {
                name: "work".into(),
                dir: Some("/tmp/.claude.work".into()),
            },
        );
        assert!(matches!(
            out,
            Outcome::Changed {
                set_default: None,
                ..
            }
        ));
        assert_eq!(p.get("work"), Some("/tmp/.claude.work"));
    }

    #[test]
    fn add_blank_dir_synthesizes() {
        let mut p = registry(&[]);
        let out = apply_edit_action(
            &mut p,
            Action::Add {
                name: "work".into(),
                dir: None,
            },
        );
        assert!(matches!(out, Outcome::Changed { .. }));
        assert!(p.get("work").unwrap().ends_with(".claude.work"));
    }

    #[test]
    fn add_duplicate_rejected() {
        let mut p = registry(&[("work", "/tmp/work")]);
        let out = apply_edit_action(
            &mut p,
            Action::Add {
                name: "work".into(),
                dir: None,
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
        // unchanged
        assert_eq!(p.get("work"), Some("/tmp/work"));
    }

    #[test]
    fn add_invalid_name_rejected() {
        let mut p = registry(&[]);
        let out = apply_edit_action(
            &mut p,
            Action::Add {
                name: "has space".into(),
                dir: None,
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
        assert!(p.is_empty());
    }

    #[test]
    fn edit_dir_changes_existing() {
        let mut p = registry(&[("work", "/old")]);
        let out = apply_edit_action(
            &mut p,
            Action::EditDir {
                name: "work".into(),
                dir: "/new".into(),
            },
        );
        assert!(matches!(out, Outcome::Changed { .. }));
        assert_eq!(p.get("work"), Some("/new"));
    }

    #[test]
    fn edit_dir_missing_profile_no_change() {
        let mut p = registry(&[]);
        let out = apply_edit_action(
            &mut p,
            Action::EditDir {
                name: "x".into(),
                dir: "/d".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
    }

    #[test]
    fn edit_dir_empty_rejected() {
        let mut p = registry(&[("work", "/old")]);
        let out = apply_edit_action(
            &mut p,
            Action::EditDir {
                name: "work".into(),
                dir: "  ".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
        assert_eq!(p.get("work"), Some("/old"));
    }

    #[test]
    fn rename_moves_dir() {
        // Two profiles so the ambient `default` state file (read by
        // default_name()) does not implicitly make "work" the preferred default
        // — keeps this test about dir-movement, not default-following (covered
        // separately in rename_default_follows).
        let mut p = registry(&[("work", "/w"), ("keep", "/k")]);
        let out = apply_edit_action(
            &mut p,
            Action::Rename {
                from: "work".into(),
                to: "job".into(),
            },
        );
        // The dir moved and the source name is gone, regardless of whether the
        // global default happened to point at "work" in this environment.
        assert!(matches!(out, Outcome::Changed { .. }));
        assert_eq!(p.get("job"), Some("/w"));
        assert!(!p.contains("work"));
        assert!(p.contains("keep"));
    }

    #[test]
    fn rename_target_exists_rejected() {
        let mut p = registry(&[("work", "/w"), ("job", "/j")]);
        let out = apply_edit_action(
            &mut p,
            Action::Rename {
                from: "work".into(),
                to: "job".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
        // both intact
        assert_eq!(p.get("work"), Some("/w"));
        assert_eq!(p.get("job"), Some("/j"));
    }

    #[test]
    fn rename_invalid_target_rejected() {
        let mut p = registry(&[("work", "/w")]);
        let out = apply_edit_action(
            &mut p,
            Action::Rename {
                from: "work".into(),
                to: "a/b".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
        assert!(p.contains("work"));
    }

    #[test]
    fn rename_default_follows() {
        // A single-profile map: default_name() resolves to "work" (preferred).
        let mut p = registry(&[("work", "/w")]);
        assert_eq!(p.default_name(), "work");
        let out = apply_edit_action(
            &mut p,
            Action::Rename {
                from: "work".into(),
                to: "job".into(),
            },
        );
        match out {
            Outcome::Changed { set_default, .. } => {
                assert_eq!(
                    set_default.as_deref(),
                    Some("job"),
                    "default must follow rename"
                );
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn delete_non_default_ok() {
        // Two profiles; default resolves to alphabetical-first "a". Delete "b".
        let mut p = registry(&[("a", "/a"), ("b", "/b")]);
        assert_eq!(p.default_name(), "a");
        let out = apply_edit_action(&mut p, Action::Delete { name: "b".into() });
        assert!(matches!(out, Outcome::Changed { .. }));
        assert!(!p.contains("b"));
    }

    #[test]
    fn delete_default_refused() {
        let mut p = registry(&[("a", "/a"), ("b", "/b")]);
        assert_eq!(p.default_name(), "a");
        let out = apply_edit_action(&mut p, Action::Delete { name: "a".into() });
        assert!(matches!(out, Outcome::NoChange { .. }));
        assert!(p.contains("a"), "default must not be deleted");
    }

    #[test]
    fn delete_missing_no_change() {
        let mut p = registry(&[("a", "/a")]);
        let out = apply_edit_action(
            &mut p,
            Action::Delete {
                name: "nope".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
    }

    #[test]
    fn set_default_existing_returns_name() {
        let mut p = registry(&[("a", "/a"), ("b", "/b")]);
        let out = apply_edit_action(&mut p, Action::SetDefault { name: "b".into() });
        match out {
            Outcome::Changed { set_default, .. } => assert_eq!(set_default.as_deref(), Some("b")),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn set_default_missing_no_change() {
        let mut p = registry(&[("a", "/a")]);
        let out = apply_edit_action(
            &mut p,
            Action::SetDefault {
                name: "ghost".into(),
            },
        );
        assert!(matches!(out, Outcome::NoChange { .. }));
    }

    #[test]
    fn quit_is_quit() {
        let mut p = registry(&[]);
        assert_eq!(apply_edit_action(&mut p, Action::Quit), Outcome::Quit);
    }

    /// A scripted sequence: add two, set-default, rename the default, delete the
    /// other — exercising the pure core end to end without any I/O.
    #[test]
    fn scripted_lifecycle() {
        let mut p = registry(&[]);
        assert!(matches!(
            apply_edit_action(
                &mut p,
                Action::Add {
                    name: "alpha".into(),
                    dir: Some("/a".into())
                }
            ),
            Outcome::Changed { .. }
        ));
        assert!(matches!(
            apply_edit_action(
                &mut p,
                Action::Add {
                    name: "beta".into(),
                    dir: Some("/b".into())
                }
            ),
            Outcome::Changed { .. }
        ));
        // set-default beta
        match apply_edit_action(
            &mut p,
            Action::SetDefault {
                name: "beta".into(),
            },
        ) {
            Outcome::Changed { set_default, .. } => {
                assert_eq!(set_default.as_deref(), Some("beta"))
            }
            o => panic!("{o:?}"),
        }
        // rename alpha (non-default) → gamma; default does NOT follow.
        match apply_edit_action(
            &mut p,
            Action::Rename {
                from: "alpha".into(),
                to: "gamma".into(),
            },
        ) {
            // NOTE: default_name() here reads the real state file, which in a
            // test env is unlikely to be "alpha"; assert structurally instead.
            Outcome::Changed { .. } => {}
            o => panic!("{o:?}"),
        }
        assert!(p.contains("gamma"));
        assert!(p.contains("beta"));
        assert!(!p.contains("alpha"));
    }
}
