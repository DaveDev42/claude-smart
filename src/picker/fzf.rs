//! fzf shell-out machinery shared by the session and account pickers.
//!
//! Spec §2 Picker / §3 Architecture "fzf shell-out picker":
//!
//! Both pickers use tab-delimited rows piped to fzf stdin, a hidden col1 recovery
//! key, and `--with-nth` to display the rest. The selected row's col1 is recovered
//! via `cut -f1` equivalent (field split in Rust, no subprocess).
//!
//! Empty selection (user pressed Escape) or fzf exit code 130 (Ctrl-C) → `None`.
//! The caller degrades gracefully (session: newest-free-sid/fresh; account: current
//! profile + stderr warning).

use std::process::{Command, Stdio};

/// Options passed to a single fzf invocation.
///
/// These map directly to fzf CLI flags. Additional flags can be appended via
/// `extra_args`.
#[derive(Debug, Clone)]
pub struct FzfOpts {
    /// `--prompt` label shown in the fzf header bar.
    pub prompt: String,
    /// `--with-nth` specifier — which tab-delimited fields to display (e.g. `"3.."`)
    pub with_nth: String,
    /// `--delimiter` (default `\t`).
    pub delimiter: String,
    /// `--height` (e.g. `"40%"`).
    pub height: String,
    /// Additional raw flags forwarded verbatim to fzf.
    pub extra_args: Vec<String>,
}

impl Default for FzfOpts {
    fn default() -> Self {
        Self {
            prompt: "select > ".to_string(),
            with_nth: "2..".to_string(),
            delimiter: "\t".to_string(),
            height: "40%".to_string(),
            extra_args: vec!["--reverse".to_string(), "--no-multi".to_string()],
        }
    }
}

/// Return `true` if `fzf` is available in `$PATH`.
///
/// Implementation: attempt `Command::new("fzf").arg("--version")`, check success.
/// This is a REAL implementation (spec says "REAL via which" — we probe the binary
/// directly rather than shelling to `which`/`type` so the check is
/// cross-platform and avoids a shell round-trip).
///
/// Result is not cached; callers that call this in a hot loop should cache it
/// themselves.
pub fn fzf_available() -> bool {
    Command::new("fzf")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pipe `rows` to fzf and return the **first tab-delimited field** (the hidden
/// recovery key, col1) of the selected row, or `None` on empty/cancelled
/// selection.
///
/// `rows` — each element is one fzf input line. May contain `\t`; the first
/// field is the hidden recovery key. Pass `--with-nth=2..` (or similar) in
/// `opts` to hide col1 from the display.
///
/// Returns:
/// - `Some(col1)` — user selected a row; col1 is extracted by splitting on `\t`.
/// - `None` — fzf was not selected (exit 130 = Ctrl-C/Escape, or empty output,
///   or fzf exited with any non-zero code that does not represent a valid
///   selection).
///
/// **Phase 0 stub** — body is `unimplemented!()`.
pub fn run_fzf(rows: &[String], opts: &FzfOpts) -> Option<String> {
    let _ = (rows, opts); // suppress unused-variable warnings while unimplemented
    unimplemented!("run_fzf: pipe rows to fzf, recover col1 from selected line")
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `fzf_available()` must return a `bool` without panicking.
    ///
    /// In CI fzf may or may not be present; the test just verifies the function
    /// is callable and returns a stable result (idempotent across two calls).
    #[test]
    fn fzf_available_is_stable() {
        let a = fzf_available();
        let b = fzf_available();
        assert_eq!(a, b, "fzf_available() must be deterministic within one process");
    }

    /// When fzf is not available, `fzf_available()` returns `false` (not panic).
    ///
    /// We can't easily inject a "no fzf" environment, so just assert the return
    /// type compiles and the function doesn't panic.
    #[test]
    fn fzf_available_returns_bool() {
        let result: bool = fzf_available();
        // The value is environment-dependent; we only assert it compiles + runs.
        let _ = result;
    }

    /// `FzfOpts::default()` produces sane values.
    #[test]
    fn fzf_opts_default_values() {
        let opts = FzfOpts::default();
        assert_eq!(opts.delimiter, "\t");
        assert!(!opts.prompt.is_empty());
        assert!(opts.extra_args.contains(&"--reverse".to_string()));
        assert!(opts.extra_args.contains(&"--no-multi".to_string()));
    }
}
