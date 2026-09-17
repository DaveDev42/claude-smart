//! Publish-gate guard: fail the build if any private identifier leaks into the
//! shipped crate — **including test fixtures**. The public crate must not carry
//! the operator's tailnet suffix, host names, account-profile names, real
//! home paths, personal email, or the operator's private companion-repo name
//! anywhere; all such values come from runtime config/env, and examples must
//! use neutral placeholders (`work`, `home`, `/Users/example`, `Acme-…`).
//!
//! Every `.rs` file under `src/` is scanned in full (production and `#[cfg(test)]`
//! alike). This file — the guard's own forbidden list — is the only thing that
//! names the identifiers, and it lives under `tests/`, which is not scanned.
//!
//! Two scopes:
//! - `src/**/*.rs`: all three rules — `forbidden()`, the `Dave-` host-prefix
//!   rule, and the quoted-profile-literal rule.
//! - `README.md`, `CLAUDE.md`, `Cargo.toml`, `examples/*.sh`: the `forbidden()`
//!   substring scan only (the other two rules are `src/`-specific: host-prefix
//!   literals and quoted profile literals are meaningful only in code).
//!
//! `tests/` itself stays excluded from every scope.

use std::fs;
use std::path::{Path, PathBuf};

/// Substrings that must never appear anywhere in shipped `src/` (including test
/// fixtures). Written as split / assembled fragments so the full literals do
/// not sit searchable in this public file.
fn forbidden() -> Vec<String> {
    vec![
        format!("{}{}", "tail", "91e9e"),        // tailnet suffix
        format!("{}{}", "dave-", "macmini"),     // operator hostname (lowercase)
        format!("{}{}", "Dave-", "MacMini"),     // operator hostname (display)
        format!("{}{}", "helloworld", "4625"),   // personal email local-part
        format!("{}{}", "ely", "vian"),          // private account-profile name
        format!("/Users/{}", "dave"),            // real home path (account owner)
        format!("/home/{}", "dave"),             // real home path (Linux/WSL)
        format!(r"C:\Users\{}", "dave"),         // real home path (Windows)
        format!("{}-{}", "dave", "environment"), // operator's private companion repo
    ]
}

/// Host-naming-convention leak: the operator's machines are prefixed `Dave-`.
/// That prefix must not be compiled into the binary or its fixtures — host
/// rewriting is injected at runtime via `CSM_HOST_REPLACE`. Assembled so the
/// literal isn't searchable here.
fn forbidden_host_prefix() -> String {
    format!("{}-", "Dave")
}

/// Profile-name literals forbidden as quoted string literals anywhere in `src/`.
/// They are valid only in the operator's private deployment repo, never
/// compiled in. Built from fragments + the quote chars so the full literal
/// isn't searchable here.
fn forbidden_profile_literals() -> Vec<String> {
    let q = '"';
    vec![
        format!("{q}{}{q}", "personal"),
        format!("{q}{}{}{q}", "ely", "vian"),
    ]
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// Scan one file for `forbidden()` substrings only (the widened, non-`src/`
/// scope). Missing files are skipped rather than failing the test — the
/// caller passes only paths it expects to exist, but a partial checkout
/// (e.g. no `examples/`) should not turn into a spurious failure here.
fn scan_forbidden_only(file: &Path, forbidden: &[String], violations: &mut Vec<String>) {
    let Ok(src) = fs::read_to_string(file) else {
        return;
    };
    for (i, line) in src.lines().enumerate() {
        let lineno = i + 1;
        for needle in forbidden {
            if line.contains(needle) {
                violations.push(format!(
                    "{}:{}: forbidden identifier {:?}",
                    file.display(),
                    lineno,
                    needle
                ));
            }
        }
    }
}

#[test]
fn no_private_identifiers_in_shipped_source() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = root.join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        src_dir.display()
    );

    let mut violations: Vec<String> = Vec::new();
    let forbidden = forbidden();
    let host_prefix = forbidden_host_prefix();
    let profile_literals = forbidden_profile_literals();

    for file in &files {
        let src = fs::read_to_string(file).expect("read source");
        // Scan EVERY line — production and `#[cfg(test)]` fixtures alike. The
        // public crate must carry no private identifier even in example data.
        for (i, line) in src.lines().enumerate() {
            let lineno = i + 1;
            for needle in &forbidden {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: forbidden identifier {:?}",
                        file.display(),
                        lineno,
                        needle
                    ));
                }
            }
            // The operator's `Dave-` host prefix must not be compiled in —
            // host rewriting is injected via CSM_HOST_REPLACE at runtime.
            if line.contains(&host_prefix) {
                violations.push(format!(
                    "{}:{}: hardcoded host prefix {:?} (use CSM_HOST_REPLACE at runtime)",
                    file.display(),
                    lineno,
                    host_prefix
                ));
            }
            // Profile-name literals are forbidden as string literals. Doc
            // comments (`///`, `//!`, `//`) may mention them illustratively.
            let code = line.trim_start();
            if !code.starts_with("//") {
                for needle in &profile_literals {
                    if line.contains(needle) {
                        violations.push(format!(
                            "{}:{}: profile-name literal {:?} (use ProfileMap / a neutral example)",
                            file.display(),
                            lineno,
                            needle
                        ));
                    }
                }
            }
        }
    }

    // Widened scope: the substring-only scan over shipped docs/build files
    // that sit outside `src/` but still land in the published tarball or the
    // repo checkout. `tests/` itself stays excluded from every scope.
    let mut doc_files: Vec<PathBuf> = vec![
        root.join("README.md"),
        root.join("CLAUDE.md"),
        root.join("Cargo.toml"),
    ];
    if let Ok(entries) = fs::read_dir(root.join("examples")) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "sh").unwrap_or(false) {
                doc_files.push(p);
            }
        }
    }
    for file in &doc_files {
        scan_forbidden_only(file, &forbidden, &mut violations);
    }

    assert!(
        violations.is_empty(),
        "private identifiers leaked into shipped source:\n{}",
        violations.join("\n")
    );
}
