//! The `--eval` path: `eval_emit` performs the CAS operation side-effects and
//! prints the one line the calling shell must eval. See the `cas` module doc
//! for the overall shell-shim contract.

use std::io::Write;

use crate::account::profiles::ProfileMap;

use super::default_profile;
use super::platform;
use super::types::{Op, Shell};

// ─── eval_emit ───────────────────────────────────────────────────────────────

/// Perform the CAS operation side-effects and print the eval-able output to
/// stdout. Thin wrapper over [`eval_emit_to`] — the pure core that every
/// golden test exercises directly against an in-memory buffer.
///
/// The caller (`cmd_cas` in `main.rs`) passes the parsed `Shell` and `Op`.
/// Output contract:
///
/// ## Normal paths (export line)
/// - `Op::Switch` / `Op::Global` / `Op::Resync`: print exactly **one** line
///   (the `export`/`$env:` line) to stdout. The parent shell evals it.
///
/// ## Informational path
/// - `Op::Status` (without `--print-current`): print human-readable status
///   directly to stdout — **not** eval-able; the shim must not eval this.
/// - `Op::Status { print_current: true }`: print the export of the global
///   default so the shim can eval it to set `_claude_as_current_profile`.
///
/// ## Error paths (emit shell error snippet)
/// - `Op::Minus`: emit a shell error snippet because `_CLAUDE_AS_PREV` is a
///   non-exported zsh variable the binary cannot read.
/// - Unknown profile: emit a shell error snippet instead of returning `Err`,
///   so that `eval "$(csm cas ...)"` correctly surfaces the error to the
///   calling shell.
///
/// # Platform side-effects (Op::Global only)
/// `Op::Global` also calls `platform::launchctl_setenv` (macOS) or
/// `platform::hkcu_setenv` (Windows) so GUI and launchd / non-shell
/// processes pick up the new default immediately.
///
/// # Return value
/// Returns `Err` only for I/O errors (state-file write failure, launchctl
/// exec failure, etc.) — NOT for user-level errors like unknown profile.
/// User-level errors are surfaced via the emitted shell error snippet.
pub fn eval_emit(shell: Shell, op: &Op, profiles: &ProfileMap) -> anyhow::Result<()> {
    eval_emit_to(&mut std::io::stdout(), shell, op, profiles)
}

/// Pure core of [`eval_emit`]: same contract, but the eval-able / status
/// output goes to `w` instead of directly to stdout. Side effects (state-file
/// write, platform setenv, provisioning) and the stderr informational lines
/// are unchanged — only the machine-interface bytes are redirected, which is
/// what the golden tests below assert byte-for-byte.
pub fn eval_emit_to(
    w: &mut impl Write,
    shell: Shell,
    op: &Op,
    profiles: &ProfileMap,
) -> anyhow::Result<()> {
    match op {
        Op::Switch { profile } => {
            match resolve_profile(profile, profiles) {
                Ok(dir) => {
                    writeln!(w, "{}", shell.export_line(&dir))?;
                }
                Err(e) => {
                    // Emit an error snippet so eval surfaces the error even
                    // though it discards the binary's exit code.
                    writeln!(w, "{}", shell.error_snippet(&e.to_string()))?;
                }
            }
        }

        Op::Minus => {
            // _CLAUDE_AS_PREV is a non-exported zsh variable — the binary
            // cannot read it. Emit a shell error snippet with the same
            // `claude-as: no previous profile to toggle to` message the
            // legacy shell implementation used:
            //   if [[ -z "${_CLAUDE_AS_PREV:-}" ]]; then
            //     print -u2 "claude-as: no previous profile to toggle to"
            //     return 1
            //   fi
            writeln!(
                w,
                "{}",
                shell.error_snippet("claude-as: no previous profile to toggle to")
            )?;
        }

        Op::Global { profile } => {
            match resolve_profile(profile, profiles) {
                Ok(dir) => {
                    // 1. Write the state file.
                    super::write_default_profile(profile, profiles)?;
                    // 2. Platform-specific side-effect (launchctl / HKCU).
                    //    Soft failure: launchctl error does not abort the export.
                    if let Err(e) = platform::apply_global(profile, &dir) {
                        eprintln!("cas: platform setenv warning: {e}");
                    }
                    // 2b. Provision the target (dir + plugins → shared SSOT) so a
                    //     `cas <profile>` shell switch lands on a consistent dir.
                    crate::provision::ensure_provisioned_soft(std::path::Path::new(&dir));
                    // 3. Emit the per-shell export line.
                    writeln!(w, "{}", shell.export_line(&dir))?;
                    // 4. Print the informational message to stderr (matches
                    //    zsh `print "global default → $profile ($dir)"` which
                    //    goes to stdout in the original but is printed before
                    //    eval — here we print to stderr so it doesn't confuse
                    //    the eval).
                    eprintln!("global default → {profile} ({dir})");
                    eprintln!(
                        "(new shells follow this via ~/.zshenv guard; running claude sessions keep their captured paths)"
                    );
                }
                Err(e) => {
                    writeln!(w, "{}", shell.error_snippet(&e.to_string()))?;
                }
            }
        }

        Op::Resync => {
            // Re-read the state file and emit the export for its current value.
            // Matches the legacy shell implementation:
            //   def=$(_claude_as_default_profile)
            //   def_dir="${CLAUDE_PROFILES[$def]}"
            //   _CLAUDE_AS_PREV=$(_claude_as_current_profile)
            //   export CLAUDE_CONFIG_DIR="$def_dir"
            //   print "shell → $def ($def_dir)"
            let profile = default_profile(profiles);
            match resolve_profile(&profile, profiles) {
                Ok(dir) => {
                    writeln!(w, "{}", shell.export_line(&dir))?;
                    eprintln!("shell → {profile} ({dir})");
                }
                Err(e) => {
                    writeln!(w, "{}", shell.error_snippet(&e.to_string()))?;
                }
            }
        }

        Op::Status { print_current } => {
            if *print_current {
                // 1-line form for `_claude_as_current_profile` shim helper.
                // The shim uses:
                //   _claude_as_current_profile() {
                //     command csm cas --eval --shell zsh -- status --print-current
                //   }
                // and evals the result to get the current profile name (not the
                // full export). The shell shim calls this directly (not via
                // eval) to read the current profile name:
                //   _claude_as_current_profile() { command csm cas --eval --shell
                //     zsh -- status --print-current; }
                // So we just print the profile name (the one word the shell
                // captures via command substitution).
                let current_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
                let profile_name = if current_dir.is_empty() {
                    "unknown".to_owned()
                } else {
                    profiles
                        .iter()
                        .find(|(_, dir)| *dir == current_dir.as_str())
                        .map(|(name, _)| name.to_owned())
                        .unwrap_or_else(|| "unknown".to_owned())
                };
                writeln!(w, "{profile_name}")?;
            } else {
                // Full status display — NOT eval-able. Matches the legacy
                // shell implementation's no-args branch.
                print_status(w, shell, profiles)?;
            }
        }

        // Registry-management ops are handled by `manage_emit`, not the eval
        // path. `cmd_cas` routes them away from here; this arm only guards the
        // type system (and surfaces a clear error if routing ever regresses).
        Op::List
        | Op::Add { .. }
        | Op::Set { .. }
        | Op::Remove { .. }
        | Op::SetDefault { .. }
        | Op::Edit => {
            anyhow::bail!("internal: {op:?} is a management op — route to manage_emit");
        }
    }
    Ok(())
}

/// Resolve a profile name to its `CLAUDE_CONFIG_DIR` path.
///
/// Falls back to constructing `~/.claude.<profile>` when `profiles` is empty
/// (toss machines / first-boot). An unknown profile in a populated map is an
/// error.
pub(super) fn resolve_profile(profile: &str, profiles: &ProfileMap) -> anyhow::Result<String> {
    if profiles.is_empty() {
        // Toss machine or pre-ansible boot: synthesize the conventional path.
        return Ok(crate::paths::synthesize_profile_dir(profile)
            .to_string_lossy()
            .into_owned());
    }
    profiles.get(profile).map(str::to_owned).ok_or_else(|| {
        let available: Vec<&str> = profiles.names_sorted();
        anyhow::anyhow!(
            "cas: unknown profile '{}' — available: {}",
            profile,
            available.join(", ")
        )
    })
}

/// Print informational status (no eval output). Matches the legacy shell
/// implementation's `cas` with no args.
pub(super) fn print_status(
    w: &mut impl Write,
    _shell: Shell,
    profiles: &ProfileMap,
) -> anyhow::Result<()> {
    // The live shell's CLAUDE_CONFIG_DIR is read from the environment.
    // The binary does not have a "previous profile" concept (that lives in the
    // shell's `_CLAUDE_AS_PREV` var). We render what we can.
    let current_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    let default = default_profile(profiles);
    let default_dir = profiles.default_dir().to_string_lossy().into_owned();

    // Resolve current profile name from CLAUDE_CONFIG_DIR.
    // Reproduces the legacy shell implementation's `_claude_as_current_profile`.
    let current_name = if current_dir.is_empty() {
        "unset".to_owned()
    } else {
        profiles
            .iter()
            .find(|(_, dir)| *dir == current_dir.as_str())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    };

    // Matches the legacy shell implementation:
    //   print "current shell:  $current_profile ($shell_state)"
    //   print "global default: $default_profile (~/.config/claude-as/default)"
    //   print "available:"
    //   for k in ${(ko)CLAUDE_PROFILES}; do
    //     mark=" "; [[ "$k" == "$current_profile" ]] && mark="*"
    //     [[ "$k" == "$default_profile" ]] && mark="${mark}d" || mark="${mark} "
    //     printf '  %s %-12s %s\n' "$mark" "$k" "$dir"
    //   done
    //   print "(legend: * = current shell, d = global default)"
    let shell_state = if current_dir.is_empty() {
        "unset (no zshenv guard? new shells won't have a profile)".to_owned()
    } else {
        current_dir.clone()
    };

    writeln!(w, "current shell:  {current_name} ({shell_state})")?;
    // Show the RESOLVED config dir of the default profile (not a hardcoded path),
    // so the user can see where the default actually points — falls back to the
    // pointer-file location when the dir can't be resolved.
    if default.is_empty() {
        // Degraded / no registry: there is no default profile name, so don't print
        // an empty name with a bogus `.claude.`-suffixed dir built from it.
        writeln!(w, "global default: (none — no default profile set)")?;
    } else if default_dir.is_empty() {
        writeln!(w, "global default: {default} (~/.config/claude-as/default)")?;
    } else {
        writeln!(w, "global default: {default} ({default_dir})")?;
    }
    writeln!(w, "available:")?;

    if profiles.is_empty() {
        writeln!(w, "  (profiles.json absent — CAS/pick features disabled)")?;
    } else {
        for name in profiles.names_sorted() {
            let dir = profiles.get(name).unwrap_or("");
            let is_current = dir == current_dir.as_str();
            let is_default = name == default.as_str();
            let mark = match (is_current, is_default) {
                (true, true) => "*d",
                (true, false) => "* ",
                (false, true) => " d",
                (false, false) => "  ",
            };
            writeln!(w, "  {mark} {name:<12} {dir}")?;
        }
        writeln!(w, "(legend: * = current shell, d = global default)")?;
    }

    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a two-profile `ProfileMap` pointing at paths under `/tmp` so tests
    /// do not depend on the real home directory.
    fn test_profiles() -> ProfileMap {
        let mut m = HashMap::new();
        m.insert("home".to_owned(), "/tmp/.claude.home".to_owned());
        m.insert("work".to_owned(), "/tmp/.claude.work".to_owned());
        ProfileMap(m)
    }

    fn empty_profiles() -> ProfileMap {
        ProfileMap::default()
    }

    // ── resolve_profile tests ─────────────────────────────────────────────────

    #[test]
    fn resolve_profile_personal_in_map() {
        let profiles = test_profiles();
        let result = resolve_profile("home", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.home");
    }

    #[test]
    fn resolve_profile_work_in_map() {
        let profiles = test_profiles();
        let result = resolve_profile("work", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.work");
    }

    #[test]
    fn resolve_profile_unknown_in_populated_map_errors() {
        let profiles = test_profiles();
        let result = resolve_profile("hacker", &profiles);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown profile"),
            "expected 'unknown profile' in: {msg}"
        );
        assert!(
            msg.contains("home"),
            "expected available profiles in: {msg}"
        );
    }

    #[test]
    fn resolve_profile_empty_map_synthesizes_path() {
        let profiles = empty_profiles();
        let result = resolve_profile("home", &profiles);
        assert!(result.is_ok());
        let dir = result.unwrap();
        // Should end with .claude.home
        assert!(dir.ends_with(".claude.home"), "synthesized dir: {dir}");
    }

    // ── eval_emit: Op::Minus emits error snippet ──────────────────────────────

    /// Capture what eval_emit prints to stdout by redirecting via a temp pipe.
    /// Since we can't redirect stdout in unit tests portably, we test the
    /// error snippet emission logic by calling `Shell::error_snippet` directly
    /// (which `eval_emit` delegates to for `Op::Minus`).
    #[test]
    fn op_minus_produces_error_snippet_for_zsh() {
        // Verify the snippet that will be emitted is well-formed zsh.
        let snippet = Shell::Zsh.error_snippet("claude-as: no previous profile to toggle to");
        assert!(snippet.contains("no previous profile"), "got: {snippet}");
        assert!(snippet.ends_with("; false"), "got: {snippet}");
    }

    #[test]
    fn op_minus_produces_error_snippet_for_pwsh() {
        let snippet = Shell::Pwsh.error_snippet("claude-as: no previous profile to toggle to");
        assert!(snippet.contains("no previous profile"), "got: {snippet}");
        assert!(snippet.ends_with("exit 1"), "got: {snippet}");
    }

    // ── eval_emit: allowlist rejection via error snippet ─────────────────────

    /// When an unknown profile is passed, eval_emit must NOT return Err (which
    /// would let the shim silently swallow the error) but must emit an error
    /// snippet so `eval` propagates the failure.
    ///
    /// We test by calling `resolve_profile` directly — the exact same logic
    /// eval_emit uses — to verify it returns Err, then verify
    /// `Shell::error_snippet` would wrap it into a valid snippet.
    #[test]
    fn unknown_profile_produces_error_snippet_not_panic() {
        let profiles = test_profiles();
        let err = resolve_profile("badprofile", &profiles).unwrap_err();
        let snippet = Shell::Zsh.error_snippet(&err.to_string());
        assert!(snippet.contains("unknown profile"), "got: {snippet}");
        assert!(snippet.ends_with("; false"), "got: {snippet}");
    }

    #[test]
    fn unknown_profile_error_snippet_for_pwsh() {
        let profiles = test_profiles();
        let err = resolve_profile("badprofile", &profiles).unwrap_err();
        let snippet = Shell::Pwsh.error_snippet(&err.to_string());
        assert!(snippet.contains("unknown profile"), "got: {snippet}");
        assert!(snippet.ends_with("exit 1"), "got: {snippet}");
    }

    // ── toggle logic tests ────────────────────────────────────────────────────

    /// The zsh toggle: if `_CLAUDE_AS_PREV` == "work" and user runs `cas -`,
    /// the shim resolves it to `cas work`. Here we verify that
    /// `resolve_profile("work", ...)` succeeds — the toggle logic itself
    /// is on the shim side, but the binary must handle the resolved profile name.
    #[test]
    fn toggle_resolves_previous_profile() {
        let profiles = test_profiles();
        // Simulate: user was on "home", ran `cas work` (which sets
        // _CLAUDE_AS_PREV="home"), then runs `cas -`.
        // The shim resolves _CLAUDE_AS_PREV to "home" and calls csm with "home".
        let result = resolve_profile("home", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.home");
    }

    /// Toggle to "work" (the other direction).
    #[test]
    fn toggle_resolves_other_profile() {
        let profiles = test_profiles();
        // Simulate: user was on "work", _CLAUDE_AS_PREV="work", `cas -`.
        // Actually: if current is personal and prev was work, toggle → work.
        let result = resolve_profile("work", &profiles);
        assert_eq!(result.unwrap(), "/tmp/.claude.work");
    }

    // ── resync logic ──────────────────────────────────────────────────────────

    /// Resync reads `default_profile()` and resolves it. Verify the logic
    /// produces the correct export line for a known profile (using the inner
    /// functions).
    #[test]
    fn resync_resolves_via_default_profile_logic() {
        let profiles = test_profiles();
        // Simulate default_profile() returning "home":
        let profile = "home";
        let dir = resolve_profile(profile, &profiles).unwrap();
        let line = Shell::Zsh.export_line(&dir);
        assert_eq!(line, "export CLAUDE_CONFIG_DIR='/tmp/.claude.home'");
    }

    #[test]
    fn resync_pwsh_form() {
        let profiles = test_profiles();
        let dir = resolve_profile("work", &profiles).unwrap();
        let line = Shell::Pwsh.export_line(&dir);
        assert_eq!(line, "$env:CLAUDE_CONFIG_DIR = '/tmp/.claude.work'");
    }

    // ── golden tests: eval_emit_to exact stdout bytes (test-05) ──────────────
    //
    // These pin the exact bytes `eval_emit_to` writes for every reachable
    // (Op, Shell) combination — the invariant-5 tripwire for the shell-shim
    // machine interface (`csm cas --eval` stdout). `Op::Global`, `Op::Resync`,
    // and the full `Op::Status` render also read/write process-global state
    // (a home-rooted state file, `CLAUDE_CONFIG_DIR`), so those cases run
    // under an isolated home dir and/or the crate's shared `CLAUDE_CONFIG_DIR`
    // lock — never the developer's real `~/.config/claude-as/default`.

    /// Run `f` with the resolved home dir pointed at a fresh, empty temp dir
    /// (dropped at the end), restoring the previous override afterward.
    /// Overrides `crate::paths::home_dir()`'s thread-local test hook rather
    /// than the `HOME` env var — on Windows `dirs::home_dir()` ignores `HOME`
    /// entirely, so an env-var fixture would give no isolation there. No lock
    /// needed: the override is thread-local, and each test runs on its own
    /// thread (see `crate::testenv`).
    fn with_isolated_home<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || f(tmp.path()))
    }

    /// Run `eval_emit_to` against an in-memory buffer and return its decoded
    /// stdout bytes alongside the `Result`.
    fn emit(shell: Shell, op: &Op, profiles: &ProfileMap) -> (String, anyhow::Result<()>) {
        let mut buf = Vec::new();
        let r = eval_emit_to(&mut buf, shell, op, profiles);
        (String::from_utf8(buf).unwrap(), r)
    }

    #[test]
    fn golden_switch_zsh() {
        let profiles = test_profiles();
        let (out, r) = emit(
            Shell::Zsh,
            &Op::Switch {
                profile: "home".to_owned(),
            },
            &profiles,
        );
        r.unwrap();
        assert_eq!(out, "export CLAUDE_CONFIG_DIR='/tmp/.claude.home'\n");
    }

    #[test]
    fn golden_switch_pwsh() {
        let profiles = test_profiles();
        let (out, r) = emit(
            Shell::Pwsh,
            &Op::Switch {
                profile: "work".to_owned(),
            },
            &profiles,
        );
        r.unwrap();
        assert_eq!(out, "$env:CLAUDE_CONFIG_DIR = '/tmp/.claude.work'\n");
    }

    #[test]
    fn golden_switch_unknown_profile_zsh() {
        let profiles = test_profiles();
        let expected = Shell::Zsh.error_snippet(
            &resolve_profile("hacker", &profiles)
                .unwrap_err()
                .to_string(),
        );
        let (out, r) = emit(
            Shell::Zsh,
            &Op::Switch {
                profile: "hacker".to_owned(),
            },
            &profiles,
        );
        r.unwrap();
        assert_eq!(out, format!("{expected}\n"));
    }

    #[test]
    fn golden_switch_unknown_profile_pwsh() {
        let profiles = test_profiles();
        let expected = Shell::Pwsh.error_snippet(
            &resolve_profile("hacker", &profiles)
                .unwrap_err()
                .to_string(),
        );
        let (out, r) = emit(
            Shell::Pwsh,
            &Op::Switch {
                profile: "hacker".to_owned(),
            },
            &profiles,
        );
        r.unwrap();
        assert_eq!(out, format!("{expected}\n"));
    }

    #[test]
    fn golden_minus_zsh() {
        let profiles = test_profiles();
        let (out, r) = emit(Shell::Zsh, &Op::Minus, &profiles);
        r.unwrap();
        assert_eq!(
            out,
            ">&2 printf '%s\\n' 'claude-as: no previous profile to toggle to'; false\n"
        );
    }

    #[test]
    fn golden_minus_pwsh() {
        let profiles = test_profiles();
        let (out, r) = emit(Shell::Pwsh, &Op::Minus, &profiles);
        r.unwrap();
        assert_eq!(
            out,
            "Write-Error 'claude-as: no previous profile to toggle to'; exit 1\n"
        );
    }

    #[test]
    fn golden_global_zsh() {
        with_isolated_home(|home| {
            let dir = home.join(".claude.home").to_string_lossy().into_owned();
            let mut m = HashMap::new();
            m.insert("home".to_owned(), dir.clone());
            m.insert(
                "work".to_owned(),
                home.join(".claude.work").to_string_lossy().into_owned(),
            );
            let profiles = ProfileMap(m);
            let (out, r) = emit(
                Shell::Zsh,
                &Op::Global {
                    profile: "home".to_owned(),
                },
                &profiles,
            );
            r.unwrap();
            assert_eq!(out, format!("export CLAUDE_CONFIG_DIR='{dir}'\n"));
        });
    }

    #[test]
    fn golden_global_pwsh() {
        with_isolated_home(|home| {
            let dir = home.join(".claude.work").to_string_lossy().into_owned();
            let mut m = HashMap::new();
            m.insert(
                "home".to_owned(),
                home.join(".claude.home").to_string_lossy().into_owned(),
            );
            m.insert("work".to_owned(), dir.clone());
            let profiles = ProfileMap(m);
            let (out, r) = emit(
                Shell::Pwsh,
                &Op::Global {
                    profile: "work".to_owned(),
                },
                &profiles,
            );
            r.unwrap();
            assert_eq!(out, format!("$env:CLAUDE_CONFIG_DIR = '{dir}'\n"));
        });
    }

    #[test]
    fn golden_global_unknown_profile_zsh() {
        // No filesystem side effects on the error path (resolve_profile fails
        // before the state-file write), so no HOME isolation is needed.
        let profiles = test_profiles();
        let expected = Shell::Zsh.error_snippet(
            &resolve_profile("hacker", &profiles)
                .unwrap_err()
                .to_string(),
        );
        let (out, r) = emit(
            Shell::Zsh,
            &Op::Global {
                profile: "hacker".to_owned(),
            },
            &profiles,
        );
        r.unwrap();
        assert_eq!(out, format!("{expected}\n"));
    }

    #[test]
    fn golden_resync_zsh() {
        with_isolated_home(|_home| {
            // Isolated HOME has no state file, so default_profile() falls
            // back to the alphabetical-first profile — "home".
            let profiles = test_profiles();
            let (out, r) = emit(Shell::Zsh, &Op::Resync, &profiles);
            r.unwrap();
            assert_eq!(out, "export CLAUDE_CONFIG_DIR='/tmp/.claude.home'\n");
        });
    }

    #[test]
    fn golden_resync_pwsh() {
        with_isolated_home(|_home| {
            let profiles = test_profiles();
            let (out, r) = emit(Shell::Pwsh, &Op::Resync, &profiles);
            r.unwrap();
            assert_eq!(out, "$env:CLAUDE_CONFIG_DIR = '/tmp/.claude.home'\n");
        });
    }

    #[test]
    fn golden_status_print_current_known() {
        crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", Some("/tmp/.claude.work"), || {
            let profiles = test_profiles();
            let (out, r) = emit(
                Shell::Zsh,
                &Op::Status {
                    print_current: true,
                },
                &profiles,
            );
            r.unwrap();
            assert_eq!(out, "work\n");
        });
    }

    #[test]
    fn golden_status_print_current_unset() {
        crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", None, || {
            let profiles = test_profiles();
            let (out, r) = emit(
                Shell::Pwsh,
                &Op::Status {
                    print_current: true,
                },
                &profiles,
            );
            r.unwrap();
            assert_eq!(out, "unknown\n");
        });
    }

    #[test]
    fn golden_status_full_display() {
        crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", Some("/tmp/.claude.work"), || {
            with_isolated_home(|_home| {
                let profiles = test_profiles();
                let (out, r) = emit(
                    Shell::Zsh,
                    &Op::Status {
                        print_current: false,
                    },
                    &profiles,
                );
                r.unwrap();
                // Isolated HOME → no state file → default is "home" (alphabetical-first).
                assert_eq!(
                    out,
                    "current shell:  work (/tmp/.claude.work)\n\
                     global default: home (/tmp/.claude.home)\n\
                     available:\n\
                     \x20  d home         /tmp/.claude.home\n\
                     \x20 *  work         /tmp/.claude.work\n\
                     (legend: * = current shell, d = global default)\n"
                );
            });
        });
    }
}
