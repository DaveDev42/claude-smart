//! fzf shell-out machinery shared by the session and account pickers.
//!
//! Spec §2 Picker / §3 Architecture "fzf shell-out picker":
//!
//! Both pickers use tab-delimited rows piped to fzf stdin, a hidden col1 recovery
//! key, and `--with-nth` to display the rest. The selected row's col1 is recovered
//! via `cut -f1` equivalent (field split in Rust, no subprocess).
//!
//! The outcome distinguishes three cases that callers handle differently:
//! - `Selected` — the user chose a row.
//! - `Cancelled` — the user pressed Escape / Ctrl-C (fzf exit 1 / 130). Callers
//!   treat this as "abort the whole operation", NOT as a fall-through default.
//! - `Unavailable` — fzf is missing or produced no usable output. Callers degrade
//!   gracefully (session: newest-free-sid/fresh; account: current profile).

use std::process::{Command, Stdio};

/// The result of an fzf invocation.
///
/// Separating `Cancelled` from `Unavailable` is what lets a caller honor Escape
/// as "cancel the command" instead of silently proceeding with a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// The user selected a row; the payload is the recovered col1 key.
    Selected(String),
    /// The user pressed Escape or Ctrl-C (fzf exit 1 / 130) — abort.
    Cancelled,
    /// fzf was unavailable (not on PATH, spawn failed) or returned no usable
    /// output — the caller should degrade to its default path.
    Unavailable,
}

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

/// Pipe `rows` to fzf and recover the **first tab-delimited field** (the hidden
/// recovery key, col1) of the selected row, as a [`PickerOutcome`].
///
/// `rows` — each element is one fzf input line. May contain `\t`; the first
/// field is the hidden recovery key. Pass `--with-nth=2..` (or similar) in
/// `opts` to hide col1 from the display.
///
/// Returns a [`PickerOutcome`]:
/// - `Selected(col1)` — user selected a row; col1 is split out on the delimiter.
/// - `Cancelled` — fzf exited 1 (Escape / no match) or 130 (Ctrl-C / Ctrl-G).
/// - `Unavailable` — spawn failed, empty input, or exit 0 with empty output.
///
/// fzf exit code for "no match / user cancelled" (Escape).
const FZF_EXIT_NO_MATCH: i32 = 1;
/// fzf exit code for "interrupted" (Ctrl-C / Ctrl-G).
const FZF_EXIT_INTERRUPTED: i32 = 130;

pub fn run_fzf(rows: &[String], opts: &FzfOpts) -> PickerOutcome {
    use std::io::Write;

    if rows.is_empty() {
        return PickerOutcome::Unavailable;
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

    // A spawn failure means fzf is unavailable — degrade, don't treat as cancel.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return PickerOutcome::Unavailable,
    };

    // Feed the rows. fzf may close stdin early (selection made before all rows
    // are read) → a BrokenPipe write is expected and must not abort.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = rows.join("\n");
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.write_all(b"\n");
        // drop closes the pipe → fzf sees EOF
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return PickerOutcome::Unavailable,
    };

    match output.status.code() {
        Some(0) => {}
        // Escape (1) and Ctrl-C (130) are explicit user cancellation.
        Some(FZF_EXIT_NO_MATCH) | Some(FZF_EXIT_INTERRUPTED) => return PickerOutcome::Cancelled,
        // Any other non-zero / signal exit is not a usable selection; degrade
        // rather than cancel, so a weird fzf error doesn't kill the launch.
        _ => return PickerOutcome::Unavailable,
    }

    let selected = String::from_utf8_lossy(&output.stdout);
    let line = match selected.lines().next() {
        Some(l) if !l.is_empty() => l,
        _ => return PickerOutcome::Unavailable,
    };
    // Recover col1 = the hidden recovery key (split on the configured delimiter).
    let delim = opts.delimiter.chars().next().unwrap_or('\t');
    PickerOutcome::Selected(line.split(delim).next().unwrap_or(line).to_string())
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

    /// Empty input rows never spawn fzf and map to `Unavailable` (a degrade),
    /// NOT `Cancelled` — there was nothing for the user to cancel. The caller
    /// must fall back, not abort.
    #[test]
    fn run_fzf_empty_rows_is_unavailable_not_cancelled() {
        let out = run_fzf(&[], &FzfOpts::default());
        assert_eq!(out, PickerOutcome::Unavailable);
    }

    /// `PickerOutcome` keeps `Cancelled` (Escape/Ctrl-C) distinct from
    /// `Unavailable` (degrade) — the whole point of the type. A caller that
    /// collapsed them would reintroduce the "Escape silently proceeds" bug.
    #[test]
    fn picker_outcome_cancelled_differs_from_unavailable() {
        assert_ne!(PickerOutcome::Cancelled, PickerOutcome::Unavailable);
        assert_ne!(
            PickerOutcome::Selected("x".into()),
            PickerOutcome::Cancelled
        );
    }
}
