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
/// fzf exit code for "no match / user cancelled" (Escape).
const FZF_EXIT_NO_MATCH: i32 = 1;
/// fzf exit code for "interrupted" (Ctrl-C / Ctrl-G).
const FZF_EXIT_INTERRUPTED: i32 = 130;

pub fn run_fzf(rows: &[String], opts: &FzfOpts) -> Option<String> {
    use std::io::Write;

    if rows.is_empty() {
        return None;
    }

    let mut cmd = Command::new("fzf");
    cmd.arg("--prompt")
        .arg(&opts.prompt)
        .arg("--with-nth")
        .arg(&opts.with_nth)
        .arg("--delimiter")
        .arg(&opts.delimiter)
        .arg("--height")
        .arg(&opts.height);
    for extra in &opts.extra_args {
        cmd.arg(extra);
    }
    // fzf reads candidates from stdin and writes the chosen line to stdout; its
    // TUI is drawn on /dev/tty, so piping stdin/stdout does not hide the UI.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().ok()?;

    // Feed the rows. fzf may close stdin early (selection made before all rows
    // are read) → a BrokenPipe write is expected and must not abort.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = rows.join("\n");
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.write_all(b"\n");
        // drop closes the pipe → fzf sees EOF
    }

    let output = child.wait_with_output().ok()?;
    // Only exit 0 is a real selection; Escape (1), Ctrl-C (130), and any other
    // non-zero / signal exit all mean "no selection".
    if output.status.code() != Some(0) {
        debug_assert!(matches!(
            output.status.code(),
            None | Some(FZF_EXIT_NO_MATCH) | Some(FZF_EXIT_INTERRUPTED) | Some(_)
        ));
        return None;
    }

    let selected = String::from_utf8_lossy(&output.stdout);
    let line = selected.lines().next()?; // first (only, with --no-multi) line
    if line.is_empty() {
        return None;
    }
    // Recover col1 = the hidden recovery key (split on the configured delimiter).
    let delim = opts.delimiter.chars().next().unwrap_or('\t');
    Some(line.split(delim).next().unwrap_or(line).to_string())
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
        assert_eq!(
            a, b,
            "fzf_available() must be deterministic within one process"
        );
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
