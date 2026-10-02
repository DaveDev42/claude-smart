//! Reads Claude Code's own on-screen state out of a `vt100`-parsed terminal
//! screen: is the input box present, is it empty or holding a draft, what
//! vim mode (if any) is shown, and has a `/compact` run started. Every
//! function here is pure (no I/O, no clock) — the caller supplies an already
//! up-to-date [`vt100::Screen`], typically fed by a relay reading claude's
//! pty. See `tests/fixtures/screens/README.md` for how the fixtures below
//! were captured and what the real UI looks like in each state.
//!
//! Box detection (design spec section 4): scanning bottom-up, the box is the
//! bottom-most row whose first non-blank cell is `❯` (U+276F), PROVIDED the
//! row directly above it is a rule row of `─` (U+2500) only. Rows below that
//! down to (and not including) the next rule row are continuation content
//! rows; rows at or below that closing rule row are footer chrome (statusline,
//! vim indicator) and are ignored. Anything not positively matching this
//! shape — most importantly a `❯` used as a plain list-selection cursor in a
//! dialog or picker, which is never immediately preceded by a rule row — is
//! `NotFound`. This is deliberately zero-tolerance: a rounded dialog border
//! (`╭─…─╮`), a diff-view separator (`╌`), or a `/model` picker's `▔` rule
//! all fail the "every non-blank cell is `─`" check the same way blank chrome
//! does.
//!
//! The idle-compact supervisor (`crate::idle_compact::supervisor`) feeds a
//! [`vt100::Parser`] built with a [`TitleTracker`] and calls these on its
//! screen.
#![cfg_attr(not(unix), allow(dead_code))]

/// Vim mode as shown by Claude Code's mode-indicator row (the terminal's
/// last row when vim keybindings are on). Claude Code v2.1.283 never draws
/// a `-- NORMAL --` marker — Normal mode looks identical to the indicator
/// row being repainted with only the permission-mode marker on it. So
/// `Normal` here means "the indicator row has content, but it isn't the
/// `-- INSERT --` marker", and `Off` means the row is entirely blank — a
/// state never observed in a real capture (vim mode was always on), kept as
/// the conservative fallback for an account with vim keybindings disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimState {
    Insert,
    Normal,
    Off,
}

/// Classification of the input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxState {
    /// Box found, holds no real (non-dim) text — a dim placeholder/suggestion
    /// may still be showing.
    Empty { vim: VimState },
    /// Box found, holds real (non-dim) typed text.
    Draft { vim: VimState },
    /// No box matching the framed shape was found on screen (e.g. a dialog
    /// or picker is covering it, or claude hasn't drawn one yet).
    NotFound,
}

const MARKER: &str = "\u{276f}"; // ❯
const RULE: &str = "\u{2500}"; // ─

/// The bottom-most-first-non-blank-cell-is-`❯` row, provided it is framed by
/// a rule row directly above it and closed by a rule row somewhere below.
/// Returns `(content_start, content_end)`, a half-open row range: rows
/// `content_start..content_end` are the box's content rows (the marker row
/// plus any continuation rows), and `content_end` itself is the closing rule
/// row. `None` when no such framed shape exists.
fn locate(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let content_start = (0..rows)
        .rev()
        .find(|&row| first_visible(screen, row, cols) == Some(MARKER))?;

    // Must be framed by a rule row directly above — this is what tells a
    // real input box apart from a `❯` used as a dialog/picker list cursor.
    if content_start == 0 || !is_rule_row(screen, content_start - 1, cols) {
        return None;
    }

    // Closed by the first rule row at or below the next row; anything in
    // between is a continuation content row. Never observed unclosed in a
    // real capture, but an unclosed box is conservatively NotFound rather
    // than guessed at.
    let content_end = ((content_start + 1)..rows).find(|&row| is_rule_row(screen, row, cols))?;

    Some((content_start, content_end))
}

/// The first cell in `row` (left to right) that isn't blank (neither an
/// empty wide-continuation cell nor a plain space). `None` if the whole row
/// is blank.
fn first_visible(screen: &vt100::Screen, row: u16, cols: u16) -> Option<&str> {
    (0..cols).find_map(|col| {
        let cell = screen.cell(row, col)?;
        let c = cell.contents();
        (!c.is_empty() && c != " ").then_some(c)
    })
}

/// A rule row is one whose only non-blank cells are `─` — zero tolerance, so
/// rounded dialog corners, `╌`-style separators and the `/model` picker's
/// `▔` border all fail this even though they visually resemble a rule.
fn is_rule_row(screen: &vt100::Screen, row: u16, cols: u16) -> bool {
    // Claude Code draws a session name into the top rule, right-aligned:
    // `──────── name ─`. Accept one such title: a long leading run of `─`,
    // a blank, text, a blank, and a single closing `─`. Anything else that
    // is not `─` or blank still fails (zero tolerance for dialog borders).
    let mut cells: Vec<&str> = Vec::with_capacity(cols as usize);
    for col in 0..cols {
        let c = screen.cell(row, col).map_or("", |cell| cell.contents());
        cells.push(if c.is_empty() { " " } else { c });
    }
    let Some(first) = cells.iter().position(|c| *c != " ") else {
        return false;
    };
    let Some(last) = cells.iter().rposition(|c| *c != " ") else {
        return false;
    };
    if cells[first] != RULE {
        return false;
    }
    let lead = cells[first..=last]
        .iter()
        .take_while(|c| **c == RULE)
        .count();
    let tail = &cells[first + lead..=last];
    if tail.is_empty() || tail.iter().all(|c| *c == RULE || *c == " ") {
        return true;
    }
    // Titled: the run, one blank, the title, one blank, a final `─`.
    lead >= TITLED_RULE_MIN_RUN
        && tail.len() >= 4
        && tail[0] == " "
        && tail[tail.len() - 1] == RULE
        && tail[tail.len() - 2] == " "
}

/// Shortest `─` run that may lead a titled rule (a title never takes most of
/// the row's width, and a short run is more likely prose).
const TITLED_RULE_MIN_RUN: usize = 8;

/// Non-dim text of the box's content rows (`content_start..content_end`),
/// with the leading `❯` marker dropped, joined across continuation rows and
/// trimmed. Dim text (Claude Code's placeholder/suggestion) is excluded, so
/// this is empty whenever the box holds only a placeholder.
///
/// Claude Code redraws the box by cursor-addressing each glyph run rather
/// than printing literal space bytes between words, so an inter-word gap is
/// an unwritten (empty-contents) cell, not a cell holding `" "` — this
/// reconstructs those gaps as spaces the same way `vt100::Row::write_contents`
/// does internally (spacing = the column distance since the last emitted
/// cell), so plain multi-word text round-trips correctly.
fn extract_text(screen: &vt100::Screen, content_start: u16, content_end: u16) -> String {
    let (_, cols) = screen.size();
    let mut rows_text = Vec::with_capacity((content_end - content_start) as usize);
    for row in content_start..content_end {
        let mut s = String::new();
        let mut prev_col: u16 = 0;
        let mut prev_was_wide = false;
        let mut marker_pending = row == content_start;
        for col in 0..cols {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            prev_was_wide = cell.is_wide();
            if !cell.has_contents() {
                continue;
            }
            let c = cell.contents();
            if marker_pending {
                marker_pending = false;
                if c == MARKER {
                    prev_col = col + u16::from(cell.is_wide()) + 1;
                    continue; // drop the marker itself, keep scanning this row
                }
                // Unexpected (locate() already confirmed the marker is here);
                // fall through and keep whatever this cell actually holds.
            }
            if cell.dim() {
                continue;
            }
            for _ in 0..col.saturating_sub(prev_col) {
                s.push(' ');
            }
            s.push_str(c);
            prev_col = col + u16::from(cell.is_wide()) + 1;
        }
        rows_text.push(s.trim().to_string());
    }
    rows_text.join(" ").trim().to_string()
}

/// Plain-text (attributes ignored) contents of one row.
fn row_plain_text(screen: &vt100::Screen, row: u16, cols: u16) -> String {
    let mut s = String::new();
    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            s.push_str(cell.contents());
        }
    }
    s
}

/// Vim mode from the screen's last row (see [`VimState`] for the Normal/Off
/// caveat).
fn vim_state(screen: &vt100::Screen, below: u16) -> VimState {
    let (rows, cols) = screen.size();
    if rows == 0 {
        return VimState::Off;
    }
    let last = row_plain_text(screen, rows - 1, cols);
    // Rows may be drawn under the mode line (a background-agent panel), so
    // the marker is looked for in every row below the box's closing rule.
    let marker =
        ((below + 1)..rows).any(|r| row_plain_text(screen, r, cols).contains("-- INSERT --"));
    if marker {
        VimState::Insert
    } else if last.trim().is_empty() {
        VimState::Off
    } else {
        VimState::Normal
    }
}

/// Classify the input box: absent, empty (possibly showing a dim
/// placeholder), or holding a draft.
pub fn input_box(screen: &vt100::Screen) -> BoxState {
    match locate(screen) {
        None => BoxState::NotFound,
        Some((start, end)) => {
            let vim = vim_state(screen, end);
            if extract_text(screen, start, end).is_empty() {
                BoxState::Empty { vim }
            } else {
                BoxState::Draft { vim }
            }
        }
    }
}

/// The box's non-dim content, trimmed. `None` when [`input_box`] would
/// return [`BoxState::NotFound`]; `Some("")` for an empty box (a dim
/// placeholder does not count as content).
pub fn box_text(screen: &vt100::Screen) -> Option<String> {
    let (start, end) = locate(screen)?;
    Some(extract_text(screen, start, end))
}

/// `true` iff the box holds exactly `text` (after trimming), i.e.
/// `box_text(screen) == Some(text)`.
pub fn box_text_is(screen: &vt100::Screen, text: &str) -> bool {
    box_text(screen).is_some_and(|t| t == text)
}

/// A hash of the screen content that matters for "has anything changed":
/// every row from the top through the input box's closing rule (formatted
/// rows, so colours and attributes count), plus the window `title`. Rows
/// below the closing rule (the statusline, mode and footer lines) and the
/// cursor position are left out, so a clock or countdown that redraws below
/// the box does not look like activity. When no box is found the whole
/// screen is hashed, the same as treating every byte as a change.
pub fn content_fingerprint(screen: &vt100::Screen, title: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let (rows, cols) = screen.size();
    let upto = match locate(screen) {
        Some((_, end)) => end + 1,
        None => rows,
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    for row in screen.rows_formatted(0, cols).take(upto as usize) {
        row.hash(&mut h);
    }
    h.finish()
}

/// `true` while a `/compact` run is in progress. Detected independently of
/// [`input_box`] because the box is `Empty` both while idle and while
/// compacting — the only on-screen signal is the "Compacting…" status line
/// drawn above the box.
pub fn compaction_started(screen: &vt100::Screen) -> bool {
    screen.contents().to_lowercase().contains("compacting")
}

/// [`vt100::Callbacks`] that remembers the window title. Claude Code puts a
/// spinner glyph (`◐`/`◑`) in front of its OSC title while a turn is running
/// and `✳` while idle; the title is the one busy signal that is drawn even in
/// frames where the activity line is not on screen yet.
#[derive(Debug, Default)]
pub struct TitleTracker {
    title: String,
}

impl TitleTracker {
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl vt100::Callbacks for TitleTracker {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

/// `true` while Claude Code is working on a turn (or compacting), from two
/// independent signals: the window `title` starts with a spinner glyph (the
/// half-circle frames `◐◑◒◓` or a braille frame), or a row on screen carries
/// the activity line (`esc to interrupt`, or the `(3s · thinking)` timer). The input box is `Empty` throughout a reply, so this is the only
/// way to tell "idle" from "generating" on screen.
pub fn busy(screen: &vt100::Screen, title: &str) -> bool {
    if title_is_busy(title) {
        return true;
    }
    let (rows, cols) = screen.size();
    (0..rows).any(|row| is_activity_line(&row_plain_text(screen, row, cols)))
}

fn title_is_busy(title: &str) -> bool {
    title
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '\u{25d0}'..='\u{25d3}' | '\u{2800}'..='\u{28ff}'))
}

fn is_activity_line(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("esc to interrupt") {
        return true;
    }
    // `(3s · thinking)` / `(7s · ↓ 91 tokens)` / `(1m 3s · ↑ 2k tokens)`: a
    // timer made of digits and h/m/s units, the middle dot, then a status.
    let mut rest = lower.as_str();
    while let Some(i) = rest.find('(') {
        let inner = &rest[i + 1..];
        let head: String = inner.chars().take_while(|c| *c != ')').collect();
        if let Some((timer, _)) = head.split_once('\u{b7}') {
            let timer = timer.trim();
            if timer.ends_with('s')
                && timer.chars().next().is_some_and(|c| c.is_ascii_digit())
                && timer
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, 'h' | 'm' | 's' | ' '))
            {
                return true;
            }
        }
        rest = inner;
    }
    false
}

/// What the slash-command menu above the box shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Menu {
    /// No command menu is drawn above the box.
    Absent,
    /// A menu is drawn; `highlighted` is the entry Enter would run (`None`
    /// when zero or several entries look highlighted); `entries` is how many
    /// command rows were found (logged on verify-failed).
    Open {
        highlighted: Option<String>,
        entries: usize,
    },
}

/// Inspect the slash-command menu Claude Code draws directly above the box's
/// top rule while a `/` command is being typed. Entries are rows whose first
/// visible cell is `/` at column 2; the highlighted one is drawn in the accent
/// colour, the others grey (see `compact-menu-typed-120x40`).
/// Menu rows are the contiguous non-blank rows above the top rule.
pub fn command_menu(screen: &vt100::Screen) -> Menu {
    let Some((start, _)) = locate(screen) else {
        return Menu::Absent;
    };
    let (_, cols) = screen.size();
    let mut entries: Vec<(String, Option<vt100::Color>)> = Vec::new(); // (command, colour)
    let mut row = start.saturating_sub(1); // the top rule
    while row > 0 {
        row -= 1;
        let text = row_plain_text(screen, row, cols);
        if text.trim().is_empty() {
            break;
        }
        let Some(col) = (0..cols).find(|&c| {
            screen
                .cell(row, c)
                .is_some_and(|cell| cell.has_contents() && cell.contents() != " ")
        }) else {
            continue;
        };
        if col != 2 || screen.cell(row, col).map(|c| c.contents()) != Some("/") {
            continue;
        }
        // The command word: the run of written, non-space cells from `col`
        // (unwritten cells separate it from the description).
        let name: String = (col..cols)
            .map_while(|c| {
                screen
                    .cell(row, c)
                    .map(|cell| cell.contents())
                    .filter(|t| !t.is_empty() && *t != " ")
            })
            .collect();
        let fg = screen.cell(row, col).map(|cell| cell.fgcolor());
        entries.push((name, fg));
    }
    if entries.is_empty() {
        return Menu::Absent;
    }
    // The selected row is drawn in the accent colour (name and description),
    // every other row in grey. Bold is not the marker: it only emphasises the
    // substring the user typed, in every entry that contains it. A lone entry
    // is the selected one by construction. With three or more, the selected
    // one is the single row whose colour differs from all the others. With
    // two, "the one that differs" is symmetric, so the accent is taken from
    // the typed command in the box, which Claude Code draws in the same accent
    // colour whichever row is selected (real captures `compact-menu-two-*`):
    // the selected entry is the one drawn in that colour while the other is
    // not. Anything unclear (no box command, default colour, both or neither
    // row in the accent) stays `None`, so the caller rolls back.
    let highlighted = if let [(name, _)] = entries.as_slice() {
        Some(name.clone())
    } else if entries.len() >= 3 {
        let odd: Vec<&(String, Option<vt100::Color>)> = entries
            .iter()
            .filter(|(_, c)| entries.iter().filter(|(_, o)| o == c).count() == 1)
            .collect();
        match odd.as_slice() {
            [(name, _)] => Some(name.clone()),
            _ => None,
        }
    } else {
        let accent = (1..cols)
            .find(|&c| {
                screen
                    .cell(start, c)
                    .is_some_and(|cell| cell.contents() == "/")
            })
            .and_then(|c| screen.cell(start, c))
            .map(|cell| cell.fgcolor())
            .filter(|c| *c != vt100::Color::Default);
        accent.and_then(|accent| {
            let matching: Vec<&(String, Option<vt100::Color>)> =
                entries.iter().filter(|(_, c)| *c == Some(accent)).collect();
            match matching.as_slice() {
                [(name, _)] => Some(name.clone()),
                _ => None,
            }
        })
    };
    Menu::Open {
        highlighted,
        entries: entries.len(),
    }
}

/// `true` when pressing Enter would run exactly `command`: either no menu is
/// drawn (Enter submits the typed text) or the menu's highlighted entry is
/// `command`.
pub fn enter_runs(screen: &vt100::Screen, command: &str) -> bool {
    match command_menu(screen) {
        Menu::Absent => true,
        Menu::Open { highlighted, .. } => highlighted.as_deref() == Some(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::path::PathBuf;

    #[derive(Deserialize)]
    struct FixtureMeta {
        name: String,
        rows: u16,
        cols: u16,
        expect: String,
        vim: Option<String>,
        box_text: Option<String>,
        compaction_started: bool,
        #[serde(default)]
        busy: bool,
        description: String,
    }

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/screens")
    }

    fn load_index() -> Vec<FixtureMeta> {
        let path = fixtures_dir().join("index.json");
        let data = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&data).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    fn replay(fx: &FixtureMeta) -> vt100::Screen {
        replay_with_title(fx).0
    }

    fn replay_with_title(fx: &FixtureMeta) -> (vt100::Screen, String) {
        let path = fixtures_dir().join(format!("{}.bin", fx.name));
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut parser =
            vt100::Parser::new_with_callbacks(fx.rows, fx.cols, 0, TitleTracker::default());
        parser.process(&raw);
        (
            parser.screen().clone(),
            parser.callbacks().title().to_owned(),
        )
    }

    fn vim_state_of(s: &str) -> VimState {
        match s {
            "Insert" => VimState::Insert,
            "Normal" => VimState::Normal,
            "Off" => VimState::Off,
            other => panic!("unknown vim state {other:?} in index.json"),
        }
    }

    fn expected_state(fx: &FixtureMeta) -> BoxState {
        match fx.expect.as_str() {
            "Empty" => BoxState::Empty {
                vim: vim_state_of(
                    fx.vim
                        .as_deref()
                        .expect("Empty fixture needs vim in index.json"),
                ),
            },
            "Draft" => BoxState::Draft {
                vim: vim_state_of(
                    fx.vim
                        .as_deref()
                        .expect("Draft fixture needs vim in index.json"),
                ),
            },
            "NotFound" => BoxState::NotFound,
            other => panic!("unknown expect {other:?} in index.json"),
        }
    }

    #[test]
    fn every_fixture_classifies_as_recorded_in_index() {
        let fixtures = load_index();
        assert!(!fixtures.is_empty(), "index.json has no fixtures");
        for fx in &fixtures {
            let screen = replay(fx);

            assert_eq!(
                input_box(&screen),
                expected_state(fx),
                "{}: {}",
                fx.name,
                fx.description
            );
            assert_eq!(
                box_text(&screen),
                fx.box_text,
                "{}: box_text mismatch ({})",
                fx.name,
                fx.description
            );
            assert_eq!(
                compaction_started(&screen),
                fx.compaction_started,
                "{}: compaction_started mismatch ({})",
                fx.name,
                fx.description
            );
        }
    }

    #[test]
    fn box_text_is_matches_only_the_compact_draft() {
        let fixtures = load_index();
        let mut saw_positive = false;
        for fx in &fixtures {
            let screen = replay(fx);
            let expected = fx.box_text.as_deref() == Some("/compact");
            assert_eq!(
                box_text_is(&screen, "/compact"),
                expected,
                "{}: box_text_is(\"/compact\") mismatch",
                fx.name
            );
            saw_positive |= expected;
        }
        assert!(
            saw_positive,
            "no fixture covers box_text_is(\"/compact\") == true"
        );
    }

    #[test]
    fn busy_matches_the_index_for_every_fixture() {
        for fx in &load_index() {
            let (screen, title) = replay_with_title(fx);
            assert_eq!(
                busy(&screen, &title),
                fx.busy,
                "{}: busy mismatch (title {title:?}): {}",
                fx.name,
                fx.description
            );
        }
    }

    #[test]
    fn busy_generating_is_refused_although_the_box_looks_empty() {
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "busy-generating-120x40")
            .unwrap();
        let (screen, title) = replay_with_title(&fx);
        assert_eq!(
            input_box(&screen),
            BoxState::Empty {
                vim: VimState::Insert
            }
        );
        assert!(title_is_busy(&title), "title {title:?}");
        assert!(busy(&screen, &title));
    }

    #[test]
    fn activity_line_alone_marks_busy_without_a_title() {
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "busy-activity-line-120x40")
            .unwrap();
        let screen = replay(&fx);
        assert!(
            busy(&screen, "\u{2733} Claude Code"),
            "the row text alone must do it"
        );
    }

    #[test]
    fn idle_screens_are_not_busy_by_their_text() {
        // 'Churned for 2s · done 1:39 AM' is a finished-turn line, not a timer.
        assert!(!is_activity_line(
            "\u{273b} Churned for 2s \u{b7} done 1:39 AM"
        ));
        assert!(!is_activity_line("plain (text) with parens"));
        assert!(is_activity_line(
            "\u{273b} Compacting conversation\u{2026} (7s \u{b7} \u{2193} 91 tokens)"
        ));
        assert!(is_activity_line("(1m 3s \u{b7} thinking)"));
        assert!(is_activity_line("Working (esc to interrupt)"));
    }

    #[test]
    fn compact_menu_highlight_is_read_from_the_real_capture() {
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-typed-120x40")
            .unwrap();
        let screen = replay(&fx);
        assert_eq!(
            command_menu(&screen),
            Menu::Open {
                highlighted: Some("/compact".to_owned()),
                entries: 3
            }
        );
        assert!(enter_runs(&screen, "/compact"));
        assert!(!enter_runs(&screen, "/autocompact"));
    }

    fn menu_of(name: &str) -> Menu {
        let fx = load_index().into_iter().find(|f| f.name == name).unwrap();
        command_menu(&replay(&fx))
    }

    #[test]
    fn two_entry_menu_highlight_follows_the_accent_of_the_box_command() {
        // Real capture, `/compact` typed: `/compact` is drawn in the accent
        // (b1b9f9), `/autocompact` in grey (999999); the box command is accent.
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-two-typed-160x50")
            .unwrap();
        let screen = replay(&fx);
        assert_eq!(
            command_menu(&screen),
            Menu::Open {
                highlighted: Some("/compact".to_owned()),
                entries: 2
            }
        );
        assert!(enter_runs(&screen, "/compact"));
        // After Down the accent moves to `/autocompact`; the box stays accent.
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-two-down-160x50")
            .unwrap();
        let screen = replay(&fx);
        assert_eq!(
            command_menu(&screen),
            Menu::Open {
                highlighted: Some("/autocompact".to_owned()),
                entries: 2
            }
        );
        assert!(!enter_runs(&screen, "/compact"));
    }

    #[test]
    fn three_entry_menu_highlight_from_real_captures() {
        assert_eq!(
            menu_of("compact-menu-three-typed-160x50"),
            Menu::Open {
                highlighted: Some("/compact".to_owned()),
                entries: 3
            }
        );
        assert_eq!(
            menu_of("compact-menu-three-down-160x50"),
            Menu::Open {
                highlighted: Some("/autocompact".to_owned()),
                entries: 3
            }
        );
    }

    #[test]
    fn two_entry_menu_without_a_decidable_accent_is_refused() {
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-two-typed-160x50")
            .unwrap();
        let path = fixtures_dir().join(format!("{}.bin", fx.name));
        let base = std::fs::read(path).unwrap();
        let check = |extra: &[u8]| {
            let mut raw = base.clone();
            raw.extend_from_slice(extra);
            let mut parser = vt100::Parser::new(fx.rows, fx.cols, 0);
            parser.process(&raw);
            enter_runs(parser.screen(), "/compact")
        };
        // Sanity: the unmodified capture passes.
        assert!(check(b""));
        // Box command in the default colour: no accent to compare against.
        assert!(!check(b"\x1b[48;3H\x1b[39m/compact"));
        // Both rows in the accent: ambiguous.
        assert!(!check(b"\x1b[46;3H\x1b[38;2;177;185;249m/autocompact"));
        // Neither row in the accent.
        assert!(!check(b"\x1b[45;3H\x1b[38;2;153;153;153m/compact"));
    }

    #[test]
    fn no_menu_on_screens_without_one() {
        for fx in &load_index() {
            if fx.name.starts_with("compact-menu-") {
                continue;
            }
            let screen = replay(fx);
            assert_eq!(command_menu(&screen), Menu::Absent, "{}", fx.name);
        }
    }

    #[test]
    fn a_menu_whose_highlight_moved_is_refused() {
        // Same rows as the capture, but the bold accent sits on the second
        // entry: Enter would run `/autocompact`.
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-typed-120x40")
            .unwrap();
        let path = fixtures_dir().join(format!("{}.bin", fx.name));
        let mut raw = std::fs::read(path).unwrap();
        raw.extend_from_slice(
            b"\x1b[32;3H\x1b[22m\x1b[38;2;153;153;153m/compact\x1b[33;3H\x1b[1m\x1b[38;2;177;185;249m/autocompact\x1b[22m\x1b[37;3H",
        );
        let mut parser = vt100::Parser::new(fx.rows, fx.cols, 0);
        parser.process(&raw);
        let screen = parser.screen();
        assert_eq!(
            command_menu(screen),
            Menu::Open {
                highlighted: Some("/autocompact".to_owned()),
                entries: 3
            }
        );
        assert!(!enter_runs(screen, "/compact"));
    }

    #[test]
    fn a_menu_with_no_highlight_is_refused() {
        let fx = load_index()
            .into_iter()
            .find(|f| f.name == "compact-menu-typed-120x40")
            .unwrap();
        let path = fixtures_dir().join(format!("{}.bin", fx.name));
        let mut raw = std::fs::read(path).unwrap();
        raw.extend_from_slice(b"\x1b[32;3H\x1b[22m\x1b[38;2;153;153;153m/compact\x1b[37;3H");
        let mut parser = vt100::Parser::new(fx.rows, fx.cols, 0);
        parser.process(&raw);
        assert!(!enter_runs(parser.screen(), "/compact"));
    }

    #[test]
    fn drafts_are_never_empty() {
        for name in [
            "draft-multiline-120x40",
            "draft-wrapped-120x40",
            "draft-paste-placeholder-120x40",
        ] {
            let fx = load_index().into_iter().find(|f| f.name == name).unwrap();
            let screen = replay(&fx);
            assert!(
                matches!(input_box(&screen), BoxState::Draft { .. }),
                "{name}: {:?}",
                input_box(&screen)
            );
        }
    }

    // --- synthetic edge cases not backed by a real capture ---

    /// Build a screen by absolute-positioning each `(row, col, text, dim)`
    /// write (1-based cursor addressing under the hood); rows/cols are
    /// 0-based to match [`vt100::Screen::cell`].
    fn synth(rows: u16, cols: u16, writes: &[(u16, u16, &str, bool)]) -> vt100::Screen {
        let mut buf = Vec::new();
        for (row, col, text, dim) in writes {
            buf.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
            if *dim {
                buf.extend_from_slice(b"\x1b[2m");
            }
            buf.extend_from_slice(text.as_bytes());
            if *dim {
                buf.extend_from_slice(b"\x1b[22m");
            }
        }
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&buf);
        parser.screen().clone()
    }

    fn rule(width: usize) -> String {
        RULE.repeat(width)
    }

    #[test]
    fn dialog_list_cursor_is_not_the_input_box() {
        // `❯` used as a dialog/picker list cursor: no rule row directly
        // above it (row 6 is blank), so this must not be mistaken for a box.
        let screen = synth(
            24,
            80,
            &[
                (5, 0, "Pick one:", false),
                (7, 0, "\u{276f} 1. Red", false),
                (8, 0, "  2. Blue", false),
            ],
        );
        assert_eq!(input_box(&screen), BoxState::NotFound);
        assert_eq!(box_text(&screen), None);
        assert!(!box_text_is(&screen, "1. Red"));
    }

    #[test]
    fn box_without_a_closing_rule_row_is_not_found() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f} hello", false),
                // rows 12.. stay blank: no closing rule row ever appears
            ],
        );
        assert_eq!(input_box(&screen), BoxState::NotFound);
        assert_eq!(box_text(&screen), None);
    }

    #[test]
    fn dim_only_content_is_empty_not_draft() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f}", false),
                (11, 2, "Try \"how do I ...\"", true),
                (12, 0, dashes.as_str(), false),
            ],
        );
        assert_eq!(input_box(&screen), BoxState::Empty { vim: VimState::Off });
        assert_eq!(box_text(&screen), Some(String::new()));
    }

    #[test]
    fn wide_characters_in_the_box_round_trip() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f} 안녕", false), // "❯ 안녕"
                (12, 0, dashes.as_str(), false),
            ],
        );
        assert_eq!(input_box(&screen), BoxState::Draft { vim: VimState::Off });
        assert_eq!(box_text(&screen).as_deref(), Some("안녕"));
    }

    #[test]
    fn draft_that_looks_like_compact_with_extra_text_is_not_an_exact_match() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f} /compact now", false),
                (12, 0, dashes.as_str(), false),
            ],
        );
        assert_eq!(box_text(&screen).as_deref(), Some("/compact now"));
        assert!(!box_text_is(&screen, "/compact"));
        assert!(box_text_is(&screen, "/compact now"));
    }

    #[test]
    fn vim_off_when_the_indicator_row_is_blank() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f}", false),
                (12, 0, dashes.as_str(), false),
                // row 23 (last row) never written: blank => Off
            ],
        );
        assert_eq!(input_box(&screen), BoxState::Empty { vim: VimState::Off });
    }

    #[test]
    fn vim_normal_when_indicator_has_content_but_not_insert() {
        let dashes = rule(78);
        let screen = synth(
            24,
            80,
            &[
                (10, 0, dashes.as_str(), false),
                (11, 0, "\u{276f}", false),
                (12, 0, dashes.as_str(), false),
                (23, 0, "\u{23f8} manual mode on", false),
            ],
        );
        assert_eq!(
            input_box(&screen),
            BoxState::Empty {
                vim: VimState::Normal
            }
        );
    }

    #[test]
    fn no_box_at_all_is_not_found() {
        let screen = synth(24, 80, &[(5, 0, "just a transcript line", false)]);
        assert_eq!(input_box(&screen), BoxState::NotFound);
        assert_eq!(box_text(&screen), None);
        assert!(!compaction_started(&screen));
    }

    // --- diagnosis repros (idle-compact real-session failures, 2026-10-01) ---

    /// Orca/Claude draw the session name right-aligned in the rule above the
    /// box: `──── reduce-token-usage ─`. A named session is NotFound today.
    #[test]
    fn titled_rule_above_the_box_still_finds_it() {
        let titled = format!("{} reduce-token-usage \u{2500}", rule(60));
        let screen = synth(
            12,
            82,
            &[
                (5, 0, titled.as_str(), false),
                (6, 0, "\u{276f}", false),
                (7, 0, rule(82).as_str(), false),
                (8, 0, "  dave@MBP16 ~/x [Opus 5.5]", false),
                (9, 0, "  -- INSERT -- bypass permissions on", false),
            ],
        );
        assert_eq!(
            input_box(&screen),
            BoxState::Empty {
                vim: VimState::Insert
            }
        );
    }

    #[test]
    fn titled_rule_with_wide_title_still_finds_it() {
        let titled = format!(
            "{} \u{d074}\u{b85c}\u{b4dc} \u{c815}\u{b9ac} \u{2500}",
            rule(50)
        );
        let screen = synth(
            12,
            82,
            &[
                (5, 0, titled.as_str(), false),
                (6, 0, "\u{276f}", false),
                (7, 0, rule(82).as_str(), false),
            ],
        );
        assert!(matches!(input_box(&screen), BoxState::Empty { .. }));
    }

    #[test]
    fn prose_after_a_dash_run_is_not_a_rule() {
        // No closing `─` after the text: an ordinary line, not a rule.
        let line = format!("{} see below", rule(30));
        let screen = synth(
            12,
            82,
            &[
                (5, 0, line.as_str(), false),
                (6, 0, "\u{276f}", false),
                (7, 0, rule(82).as_str(), false),
            ],
        );
        assert_eq!(input_box(&screen), BoxState::NotFound);
    }

    /// With a background-agent panel drawn under the vim line, the
    /// `-- INSERT --` row is not the terminal's last row.
    #[test]
    fn insert_marker_above_an_agent_panel_still_reads_as_insert() {
        let screen = synth(
            12,
            82,
            &[
                (5, 0, rule(82).as_str(), false),
                (6, 0, "\u{276f}", false),
                (7, 0, rule(82).as_str(), false),
                (8, 0, "  dave@MBP16 ~/x [Opus 5.5]", false),
                (9, 0, "  -- INSERT -- bypass permissions on", false),
                (10, 0, "  \u{23fa} main", false),
                (11, 0, "  \u{25ef} general-purpose  Listing files", false),
            ],
        );
        assert_eq!(
            input_box(&screen),
            BoxState::Empty {
                vim: VimState::Insert
            }
        );
    }

    // --- content_fingerprint ---

    fn boxed(status: &str, above: &str) -> vt100::Screen {
        let r = rule(40);
        synth(
            12,
            60,
            &[
                (0, 0, above, false),
                (5, 0, &r, false),
                (6, 0, "\u{276f}", false),
                (7, 0, &r, false),
                (8, 0, status, false),
                (9, 0, "-- INSERT --", false),
            ],
        )
    }

    #[test]
    fn fingerprint_ignores_rows_below_the_box() {
        let a = content_fingerprint(&boxed("[⏱ 4m29s]", "hello"), "t");
        let b = content_fingerprint(&boxed("[⏱ 4m28s]", "hello"), "t");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_sees_changes_above_the_box_and_in_the_title() {
        let a = content_fingerprint(&boxed("x", "hello"), "t");
        assert_ne!(a, content_fingerprint(&boxed("x", "hellp"), "t"));
        assert_ne!(a, content_fingerprint(&boxed("x", "hello"), "u"));
    }

    #[test]
    fn fingerprint_sees_typing_in_the_box() {
        let r = rule(40);
        let with = |t: &str| {
            synth(
                12,
                60,
                &[
                    (5, 0, &r, false),
                    (6, 0, &format!("\u{276f} {t}"), false),
                    (7, 0, &r, false),
                ],
            )
        };
        assert_ne!(
            content_fingerprint(&with(""), ""),
            content_fingerprint(&with("/compact"), "")
        );
    }

    #[test]
    fn fingerprint_hashes_the_whole_screen_without_a_box() {
        let a = synth(12, 60, &[(0, 0, "menu", false), (9, 0, "clock 1", false)]);
        let b = synth(12, 60, &[(0, 0, "menu", false), (9, 0, "clock 2", false)]);
        assert_ne!(content_fingerprint(&a, ""), content_fingerprint(&b, ""));
    }
}
