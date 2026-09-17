//! In-process fuzzy picker shared by the session and account pickers.
//!
//! Replaces the former `fzf` shell-out. The matcher is `nucleo-matcher` (the
//! scoring core of helix's nucleo); the TUI is drawn with `crossterm`.
//!
//! Contract (matches the former `run_fzf`):
//! - Each row is delimiter-separated; **field 1 is a hidden recovery key** (col1).
//! - `display_from` (was `--with-nth=N..`) selects which fields are shown AND
//!   fuzzy-matched; col1 stays hidden.
//! - Single select, best match on top (was `--no-multi --reverse`).
//! - Returns a [`PickerOutcome`]: `Selected(col1)`, `Cancelled` (Escape/Ctrl-C),
//!   or `Unavailable` (empty rows / no usable terminal / terminal I/O error).
//!   Keeping `Cancelled` distinct from `Unavailable` is what lets a caller honor
//!   Escape as "abort the launch" instead of silently proceeding with a default.
//!
//! crossterm reads keys from the controlling terminal even when this process's
//! stdin/stdout are piped (its `tty_fd()` opens `/dev/tty`, or `CONIN$` on
//! Windows), so the picker works like fzf with redirected stdio. We still refuse
//! to draw when stdin/stdout are not terminals (`terminal_available()`) so a
//! non-interactive context degrades (`Unavailable`) instead of blocking on a
//! hidden TUI.
//!
//! Pure core (`project_display` / `recover_col1` / `rank`) is unit-tested; the
//! crossterm event loop is the thin, untestable I/O shell.

use std::io::{self, IsTerminal, Write};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// The result of a picker invocation.
///
/// Separating `Cancelled` from `Unavailable` is what lets a caller honor Escape
/// as "cancel the command" instead of silently proceeding with a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// The user selected a row; the payload is the recovered col1 key.
    Selected(String),
    /// The user confirmed a multi-select; the payload is the recovered col1 key
    /// of every toggled row, in original `rows` order. May be empty (the user
    /// confirmed with nothing toggled) — the caller treats that as "select none".
    /// Only produced by [`run_multi_picker`].
    SelectedMulti(Vec<String>),
    /// The user pressed Escape or Ctrl-C / Ctrl-G — abort.
    Cancelled,
    /// No usable terminal, empty input, or a terminal I/O error — the caller
    /// should degrade to its default path.
    Unavailable,
}

/// Options for one `run_picker` invocation.
///
/// Maps from the old fzf flag set: `prompt` → the query-line label; `display_from`
/// → the 1-based first displayed field (was `--with-nth=N..`); `delimiter` → the
/// field separator (was `--delimiter`, always `\t`). `--reverse` (best on top)
/// and `--no-multi` (single select) are now implicit; `--height` is dropped (the
/// picker uses the alternate screen).
#[derive(Debug, Clone)]
pub struct PickerOpts {
    /// Query-line label (e.g. `"session > "`).
    pub prompt: String,
    /// 1-based index of the first displayed/matched field. `"3.."` → 3, `"2.."` → 2.
    pub display_from: usize,
    /// Field separator (was always `'\t'`).
    pub delimiter: char,
}

impl Default for PickerOpts {
    fn default() -> Self {
        Self {
            prompt: "select > ".to_string(),
            display_from: 2,
            delimiter: '\t',
        }
    }
}

/// True when we can draw an interactive picker on the controlling terminal.
///
/// Mirrors the binary's `is_interactive()` gate (stdin AND stdout are terminals).
/// crossterm can read keys from `/dev/tty` even if stdin is piped, but if stdin
/// is not a tty we are almost certainly non-interactive (CI / pipe / hook), so we
/// degrade rather than surprise the user with a blocking TUI.
pub(crate) fn terminal_available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Project a row to its DISPLAYED text: fields `[display_from-1..]` joined by a
/// single space (mirrors fzf `--with-nth=N..`). col1 is excluded when `N >= 2`.
pub(crate) fn project_display(row: &str, opts: &PickerOpts) -> String {
    let fields: Vec<&str> = row.split(opts.delimiter).collect();
    let start = opts.display_from.saturating_sub(1);
    if start >= fields.len() {
        return String::new();
    }
    fields[start..].join(" ")
}

/// Recover col1 (the hidden recovery key) from a row.
pub(crate) fn recover_col1(row: &str, delimiter: char) -> String {
    row.split(delimiter).next().unwrap_or(row).to_string()
}

/// A candidate the matcher ranks: the original row index + its displayed text.
/// `AsRef<str>` lets `Pattern::match_list` score the display text while we keep
/// the index to recover the row afterwards.
#[derive(Clone)]
struct Candidate {
    idx: usize,
    display: String,
}

impl AsRef<str> for Candidate {
    fn as_ref(&self) -> &str {
        &self.display
    }
}

/// Pure filter + rank: return the indices of matching rows, best-first.
///
/// Empty/whitespace query → all rows in original order (fzf shows everything when
/// the query is empty). Matching is over the projected display text; the returned
/// indices map back into `rows`.
pub(crate) fn rank(
    rows: &[String],
    query: &str,
    opts: &PickerOpts,
    matcher: &mut Matcher,
) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..rows.len()).collect();
    }
    let candidates: Vec<Candidate> = rows
        .iter()
        .enumerate()
        .map(|(idx, r)| Candidate {
            idx,
            display: project_display(r, opts),
        })
        .collect();

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    // `match_list` returns `(item, score)` sorted by score descending.
    pattern
        .match_list(candidates, matcher)
        .into_iter()
        .map(|(c, _score)| c.idx)
        .collect()
}

/// Which flavor of the shared render/input loop is running: single-select
/// (Enter returns one row, no toggle state) or multi-select (Space/Tab/Ctrl-A
/// toggle, Enter confirms the whole selection).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerMode {
    Single,
    Multi,
}

/// Help line drawn under the list in [`PickerMode::Multi`] only.
const HELP_LINE: &str = "  ⏎ confirm · space/tab toggle · ⌃a all · esc cancel";

/// Rows of terminal chrome the loop reserves above the row list: the query
/// line alone for Single, plus a status/help line for Multi.
pub(crate) fn chrome_lines(mode: PickerMode) -> usize {
    match mode {
        PickerMode::Single => 1,
        PickerMode::Multi => 2,
    }
}

/// Render the top query line: the prompt/query alone for Single, with a
/// `[n selected]` suffix for Multi.
pub(crate) fn render_header(
    mode: PickerMode,
    prompt: &str,
    query: &str,
    selected_count: usize,
) -> String {
    match mode {
        PickerMode::Single => format!("{prompt}{query}"),
        PickerMode::Multi => format!("{prompt}{query}    [{selected_count} selected]"),
    }
}

/// Render one row of the list: a 2-char cursor marker for Single, a 6-char
/// cursor+checkbox marker for Multi. `text` is truncated to fit `cols` after
/// the marker (a `cols` of 0, as a fresh pty reports before `TIOCSWINSZ`,
/// truncates to empty rather than panicking).
pub(crate) fn render_row(
    mode: PickerMode,
    is_cursor: bool,
    is_selected: bool,
    text: &str,
    cols: u16,
) -> String {
    match mode {
        PickerMode::Single => {
            let max = (cols as usize).saturating_sub(2);
            let text: String = if text.chars().count() > max {
                text.chars().take(max).collect()
            } else {
                text.to_string()
            };
            let marker = if is_cursor { "> " } else { "  " };
            format!("{marker}{text}")
        }
        PickerMode::Multi => {
            let max = (cols as usize).saturating_sub(6);
            let text: String = if text.chars().count() > max {
                text.chars().take(max).collect()
            } else {
                text.to_string()
            };
            let cursor_mark = if is_cursor { ">" } else { " " };
            let check = if is_selected { "x" } else { " " };
            format!("{cursor_mark} [{check}] {text}")
        }
    }
}

/// Interactive single-select fuzzy picker over `rows`. See the module contract.
///
/// Returns `Unavailable` for empty `rows` or no usable terminal (so the caller
/// degrades), `Cancelled` on Escape/Ctrl-C (so the caller aborts), and
/// `Selected(col1)` on a choice.
pub fn run_picker(rows: &[String], opts: &PickerOpts) -> PickerOutcome {
    if rows.is_empty() {
        return PickerOutcome::Unavailable;
    }
    if !terminal_available() {
        return PickerOutcome::Unavailable;
    }
    // A terminal I/O error degrades to `Unavailable` (caller falls back) rather
    // than bubbling up — a picker failure must never abort the launch.
    run_loop(rows, opts, PickerMode::Single).unwrap_or(PickerOutcome::Unavailable)
}

/// Interactive **multi-select** fuzzy picker over `rows`. See the module contract.
///
/// Same display/recovery rules as [`run_picker`], but the user toggles any number
/// of rows (Space/Tab), can toggle every currently-filtered row at once (Ctrl-A),
/// and confirms with Enter. Returns:
/// - `SelectedMulti(keys)` on Enter — the col1 keys of all toggled rows in
///   original `rows` order (possibly empty if nothing was toggled),
/// - `Cancelled` on Escape / Ctrl-C / Ctrl-G,
/// - `Unavailable` for empty `rows`, no usable terminal, or a terminal I/O error.
///
/// Selection is tracked by **original row index**, so toggles survive query
/// changes (filtering a row out of view never drops its selection).
pub fn run_multi_picker(rows: &[String], opts: &PickerOpts) -> PickerOutcome {
    if rows.is_empty() {
        return PickerOutcome::Unavailable;
    }
    if !terminal_available() {
        return PickerOutcome::Unavailable;
    }
    run_loop(rows, opts, PickerMode::Multi).unwrap_or(PickerOutcome::Unavailable)
}

/// Toggle every row index in `filtered` within `selected` (Ctrl-A semantics).
///
/// If *all* filtered rows are already selected, the action deselects them all;
/// otherwise it selects all of them. This "select-all / clear-all" toggle is the
/// convenience the reaper exposes for "kill everything". Pure for unit testing.
pub(crate) fn toggle_all(filtered: &[usize], selected: &mut std::collections::BTreeSet<usize>) {
    let all_selected = !filtered.is_empty() && filtered.iter().all(|i| selected.contains(i));
    if all_selected {
        for i in filtered {
            selected.remove(i);
        }
    } else {
        for &i in filtered {
            selected.insert(i);
        }
    }
}

/// The shared render/input loop behind both [`run_picker`] and
/// [`run_multi_picker`]. `mode` gates the handful of behaviors that differ
/// between single- and multi-select: `chrome_lines`/`render_header`/
/// `render_row` for drawing, the help line, Enter's two return shapes, and the
/// Space/Tab/Ctrl-A toggle bindings. Everything else — cursor motion, query
/// editing, cancel keys — is unconditional.
fn run_loop(rows: &[String], opts: &PickerOpts, mode: PickerMode) -> io::Result<PickerOutcome> {
    use std::collections::BTreeSet;

    let _guard = TermGuard::enter()?;
    let mut out = io::stderr();
    let mut matcher = Matcher::new(Config::DEFAULT);

    let mut query = String::new();
    let mut filtered: Vec<usize> = rank(rows, &query, opts, &mut matcher);
    let mut cursor_pos: usize = 0; // index into `filtered`
    let mut selected: BTreeSet<usize> = BTreeSet::new(); // original row indices; Multi only

    loop {
        let (cols, term_rows) = terminal::size().unwrap_or((80, 24));
        let list_capacity = (term_rows as usize)
            .saturating_sub(chrome_lines(mode))
            .max(1);
        let visible = filtered.len().min(list_capacity);
        if visible == 0 {
            cursor_pos = 0;
        } else if cursor_pos >= visible {
            cursor_pos = visible - 1;
        }

        // ── render ──────────────────────────────────────────────────────────
        queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
        queue!(
            out,
            Print(render_header(mode, &opts.prompt, &query, selected.len()))
        )?;
        for (screen_row, &row_idx) in filtered.iter().take(list_capacity).enumerate() {
            let text = project_display(&rows[row_idx], opts);
            let is_cursor = screen_row == cursor_pos;
            let is_selected = selected.contains(&row_idx);
            queue!(
                out,
                cursor::MoveTo(0, (screen_row + 1) as u16),
                Print(render_row(mode, is_cursor, is_selected, &text, cols))
            )?;
        }
        if mode == PickerMode::Multi {
            // Help line at the bottom of the list.
            queue!(
                out,
                cursor::MoveTo(0, (visible + 1) as u16),
                Print(HELP_LINE)
            )?;
        }
        out.flush()?;

        // ── input ───────────────────────────────────────────────────────────
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => return Ok(PickerOutcome::Cancelled),
                    KeyCode::Char('c') if ctrl => return Ok(PickerOutcome::Cancelled),
                    KeyCode::Char('g') if ctrl => return Ok(PickerOutcome::Cancelled),
                    KeyCode::Enter => {
                        return Ok(match mode {
                            // Confirm: recover col1 of every selected row, in
                            // original `rows` order (BTreeSet iterates ascending).
                            PickerMode::Multi => {
                                let keys: Vec<String> = selected
                                    .iter()
                                    .map(|&i| recover_col1(&rows[i], opts.delimiter))
                                    .collect();
                                PickerOutcome::SelectedMulti(keys)
                            }
                            PickerMode::Single => match filtered.get(cursor_pos) {
                                Some(&i) => {
                                    PickerOutcome::Selected(recover_col1(&rows[i], opts.delimiter))
                                }
                                // Enter with no match in view → nothing to select; degrade.
                                None => PickerOutcome::Unavailable,
                            },
                        });
                    }
                    // Toggle the row under the cursor (Space or Tab). In Single
                    // mode, Space falls through to the query-char arm below.
                    KeyCode::Char(' ') | KeyCode::Tab if mode == PickerMode::Multi => {
                        if let Some(&row_idx) = filtered.get(cursor_pos) {
                            if !selected.remove(&row_idx) {
                                selected.insert(row_idx);
                            }
                            // Advance so repeated toggles walk down the list.
                            if cursor_pos + 1 < visible {
                                cursor_pos += 1;
                            }
                        }
                    }
                    // Ctrl-A: toggle all currently-filtered rows.
                    KeyCode::Char('a') if ctrl && mode == PickerMode::Multi => {
                        toggle_all(&filtered, &mut selected);
                    }
                    KeyCode::Up => cursor_pos = cursor_pos.saturating_sub(1),
                    KeyCode::Char('p') if ctrl => cursor_pos = cursor_pos.saturating_sub(1),
                    KeyCode::Down => {
                        if cursor_pos + 1 < visible {
                            cursor_pos += 1;
                        }
                    }
                    KeyCode::Char('n') if ctrl => {
                        if cursor_pos + 1 < visible {
                            cursor_pos += 1;
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        filtered = rank(rows, &query, opts, &mut matcher);
                        cursor_pos = 0;
                    }
                    // Printable chars extend the query. In Multi mode Space is
                    // reserved for toggle above, so it never reaches here.
                    KeyCode::Char(c) if !ctrl => {
                        query.push(c);
                        filtered = rank(rows, &query, opts, &mut matcher);
                        cursor_pos = 0;
                    }
                    _ => {}
                }
            }
            // Resize → re-render on the next loop iteration with fresh dimensions.
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

/// RAII terminal guard: enters raw mode + alternate screen on construction and
/// restores both on `Drop` (covers normal return, early `?`, and panic unwinding).
/// Output is drawn to stderr so stdout stays free for the binary's own output.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut w = io::stderr();
        execute!(w, EnterAlternateScreen, cursor::Hide)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut w = io::stderr();
        let _ = execute!(w, cursor::Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(display_from: usize) -> PickerOpts {
        PickerOpts {
            prompt: "p > ".to_string(),
            display_from,
            delimiter: '\t',
        }
    }

    #[test]
    fn project_display_hides_col1() {
        // session row: sid\tmtime\thuman\tmode\tlabel, display_from=3 → human mode label
        let row = "deadbeef\t1700000000\t06-18 12:00\tdefault\tMy session";
        assert_eq!(
            project_display(row, &opts(3)),
            "06-18 12:00 default My session"
        );
        // col1 (the sid) must not appear in the display.
        assert!(!project_display(row, &opts(3)).contains("deadbeef"));
    }

    #[test]
    fn project_display_account_spec() {
        // account row: profile\tdisplay, display_from=2 → display only
        let row = "home\tsession 3%   week 32%";
        assert_eq!(project_display(row, &opts(2)), "session 3%   week 32%");
    }

    #[test]
    fn project_display_out_of_range_is_empty() {
        let row = "only-one-field";
        assert_eq!(project_display(row, &opts(3)), "");
    }

    #[test]
    fn recover_col1_basic() {
        assert_eq!(recover_col1("abc\tx\ty", '\t'), "abc");
        // single field → the whole row
        assert_eq!(recover_col1("solo", '\t'), "solo");
    }

    #[test]
    fn rank_empty_query_returns_all_in_order() {
        let rows = vec!["k\talpha".to_string(), "k\tbeta".to_string()];
        let mut m = Matcher::new(Config::DEFAULT);
        assert_eq!(rank(&rows, "", &opts(2), &mut m), vec![0, 1]);
        assert_eq!(rank(&rows, "   ", &opts(2), &mut m), vec![0, 1]);
    }

    #[test]
    fn rank_orders_by_match_quality_and_excludes_nonmatches() {
        let rows = vec![
            "k\talpha".to_string(),
            "k\tbeta".to_string(),
            "k\talphabet".to_string(),
        ];
        let mut m = Matcher::new(Config::DEFAULT);
        let got = rank(&rows, "alpha", &opts(2), &mut m);
        // "beta" (index 1) does not contain the query → excluded.
        assert!(!got.contains(&1), "beta should not match 'alpha': {got:?}");
        // Both alpha rows match.
        assert!(got.contains(&0) && got.contains(&2), "got: {got:?}");
        // Exact match "alpha" outranks the longer "alphabet".
        assert_eq!(
            got.first(),
            Some(&0),
            "exact match should rank first: {got:?}"
        );
    }

    #[test]
    fn picker_opts_default_values() {
        let o = PickerOpts::default();
        assert_eq!(o.display_from, 2);
        assert_eq!(o.delimiter, '\t');
        assert!(!o.prompt.is_empty());
    }

    #[test]
    fn picker_outcome_cancelled_differs_from_unavailable() {
        // The whole point of the 3-way type: Escape (Cancelled) must stay
        // distinct from a degrade (Unavailable), or "Escape silently proceeds"
        // regresses.
        assert_ne!(PickerOutcome::Cancelled, PickerOutcome::Unavailable);
        assert_ne!(
            PickerOutcome::Selected("x".into()),
            PickerOutcome::Cancelled
        );
    }

    // ── multi-select helpers ──────────────────────────────────────────────────

    #[test]
    fn toggle_all_selects_then_clears() {
        use std::collections::BTreeSet;
        let filtered = vec![0usize, 2, 5];
        let mut sel: BTreeSet<usize> = BTreeSet::new();
        // First toggle: none selected → select all filtered.
        toggle_all(&filtered, &mut sel);
        assert_eq!(sel, BTreeSet::from([0, 2, 5]));
        // Second toggle: all selected → clear them.
        toggle_all(&filtered, &mut sel);
        assert!(sel.is_empty(), "second toggle_all should clear: {sel:?}");
    }

    #[test]
    fn toggle_all_selects_when_partially_selected() {
        use std::collections::BTreeSet;
        let filtered = vec![0usize, 1, 2];
        let mut sel: BTreeSet<usize> = BTreeSet::from([1]); // partial
        // Not all selected → select all (does not clear).
        toggle_all(&filtered, &mut sel);
        assert_eq!(sel, BTreeSet::from([0, 1, 2]));
    }

    #[test]
    fn toggle_all_only_touches_filtered_rows() {
        use std::collections::BTreeSet;
        // A row selected but NOT in the current filter must be left alone.
        let filtered = vec![0usize, 1];
        let mut sel: BTreeSet<usize> = BTreeSet::from([9]); // 9 not in filter
        toggle_all(&filtered, &mut sel);
        assert!(
            sel.contains(&9),
            "out-of-filter selection must persist: {sel:?}"
        );
        assert!(sel.contains(&0) && sel.contains(&1));
    }

    #[test]
    fn selected_multi_recovers_col1_in_row_order() {
        // Simulate what the confirm branch does: a BTreeSet of row indices maps
        // back to col1 keys in ascending (original) order.
        use std::collections::BTreeSet;
        let rows = [
            "100\tchild\tfoo".to_string(),
            "200\tchild\tbar".to_string(),
            "300\tchild\tbaz".to_string(),
        ];
        let selected: BTreeSet<usize> = BTreeSet::from([2, 0]); // out-of-order insert
        let keys: Vec<String> = selected
            .iter()
            .map(|&i| recover_col1(&rows[i], '\t'))
            .collect();
        // BTreeSet iterates ascending → keys are in original row order.
        assert_eq!(keys, vec!["100".to_string(), "300".to_string()]);
    }

    // ── headless render core (PickerMode) ─────────────────────────────────────

    #[test]
    fn chrome_lines_by_mode() {
        assert_eq!(chrome_lines(PickerMode::Single), 1);
        assert_eq!(chrome_lines(PickerMode::Multi), 2);
    }

    #[test]
    fn render_header_by_mode() {
        assert_eq!(
            render_header(PickerMode::Single, "p > ", "abc", 0),
            "p > abc"
        );
        // Single ignores selected_count — it is unused by the format.
        assert_eq!(
            render_header(PickerMode::Single, "p > ", "abc", 5),
            "p > abc"
        );
        assert_eq!(
            render_header(PickerMode::Multi, "p > ", "abc", 0),
            "p > abc    [0 selected]"
        );
        assert_eq!(
            render_header(PickerMode::Multi, "p > ", "abc", 3),
            "p > abc    [3 selected]"
        );
    }

    #[test]
    fn render_row_single_truncates_by_cols() {
        // Marker is 2 chars ("> " / "  "); cols 0 and 2 leave no room for text.
        assert_eq!(
            render_row(PickerMode::Single, true, false, "hello", 0),
            "> "
        );
        assert_eq!(
            render_row(PickerMode::Single, false, false, "hello", 2),
            "  "
        );
        assert_eq!(
            render_row(PickerMode::Single, true, false, "hello", 3),
            "> h"
        );
        assert_eq!(
            render_row(PickerMode::Single, false, false, "hello", 80),
            "  hello"
        );
    }

    #[test]
    fn render_row_multi_truncates_by_cols_and_shows_checkbox() {
        // Marker is 6 chars ("> [x] " / "  [ ] "); cols 0 and 6 leave no room.
        assert_eq!(
            render_row(PickerMode::Multi, true, true, "hello", 0),
            "> [x] "
        );
        assert_eq!(
            render_row(PickerMode::Multi, false, false, "hello", 6),
            "  [ ] "
        );
        assert_eq!(
            render_row(PickerMode::Multi, true, false, "hello", 7),
            "> [ ] h"
        );
        assert_eq!(
            render_row(PickerMode::Multi, false, true, "hello", 80),
            "  [x] hello"
        );
    }
}
