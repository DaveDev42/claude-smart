//! Publish-gate guard: fail the build if any private identifier leaks into
//! **non-test** source. This is the load-bearing check that keeps the public
//! crate free of Dave's tailnet suffix, hub hostname, and private Anthropic
//! account names — all of which must come from runtime config/env, never from
//! compiled-in defaults or allowlists.
//!
//! Test fixtures (`#[cfg(test)]` modules) legitimately use these strings as
//! example data, so we scan only lines outside test modules and outside the
//! `tests/` dir itself. The heuristic: skip a file's lines once we enter a
//! `#[cfg(test)]` / `mod tests` region (everything after it in that file is
//! test code in this crate's layout — test modules are always last).

use std::fs;
use std::path::{Path, PathBuf};

/// Substrings that must never appear in shipped (non-test) source.
const FORBIDDEN: &[&str] = &[
    "example-tnet",      // tailnet suffix
    "workstation",   // hub hostname (lowercase)
    "Workstation",   // hub hostname (display)
    "user", // personal email local-part
];

/// Profile-name allowlist leak: these must not appear as string literals in
/// non-test production logic (they are valid only as test fixtures + the
/// dave-environment SSOT, never compiled into this binary).
const FORBIDDEN_PROFILE_LITERALS: &[&str] = &["\"personal\"", "\"work\""];

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

/// Return the production-code lines of a file: everything up to the first
/// `#[cfg(test)]` (test modules are always last in this crate's files).
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        lines.push((i + 1, line));
    }
    lines
}

#[test]
fn no_private_identifiers_in_shipped_source() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        src_dir.display()
    );

    let mut violations: Vec<String> = Vec::new();

    for file in &files {
        let src = fs::read_to_string(file).expect("read source");
        for (lineno, line) in production_lines(&src) {
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: forbidden identifier {:?}",
                        file.display(),
                        lineno,
                        needle
                    ));
                }
            }
            // Profile-name literals are forbidden in production logic. Doc
            // comments (`///`, `//!`, `//`) may mention them illustratively.
            let code = line.trim_start();
            if !code.starts_with("//") {
                for needle in FORBIDDEN_PROFILE_LITERALS {
                    if line.contains(needle) {
                        violations.push(format!(
                            "{}:{}: profile-name literal {:?} in production code \
                             (use ProfileMap, not a hardcoded name)",
                            file.display(),
                            lineno,
                            needle
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "private identifiers leaked into shipped source:\n{}",
        violations.join("\n")
    );
}
