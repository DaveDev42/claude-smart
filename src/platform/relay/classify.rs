//! Input classifier: which bytes arriving on the outer terminal's stdin count
//! as a real user keystroke for idle-detection purposes.
//!
//! Excluded (do NOT update `last_keystroke`): focus reports (`CSI I`/`CSI O`),
//! mouse reports (SGR `CSI < ... M/m` and X10 `CSI M` + 3 raw bytes), and
//! terminal replies (DA primary `CSI ? ... c`, DA secondary `CSI > ... c`,
//! kitty keyboard flags `CSI ? ... u`, cursor position `CSI ... R`, DECRPM
//! `CSI ? ... $ y`, and any OSC/DCS/other-string sequence — these are the
//! terminal answering a query claude sent, not something the user typed).
//! Everything else — plain bytes, control chars, arrow/function keys, an
//! Alt/Meta+key combo — counts.
//!
//! This is a stateful scanner, NOT a byte-by-byte pure function: a sequence
//! can split across two `read()` calls, so state is carried in `Classifier`
//! across [`Classifier::feed`] calls (explicitly required — "buffer across
//! reads so a sequence split over two reads is classified correctly"). The
//! classifier only decides whether to bump a timestamp; it never gates,
//! delays, or mutates what the input thread relays to the master — the
//! caller relays every byte immediately regardless of what `feed` returns.
//!
//! Known limitation: a lone ESC keypress with no byte ever following it again
//! for the rest of the session never resolves (stays in `Esc` state forever)
//! and so never counts as a keystroke. In practice more input always follows
//! within an idle-compact session; see the module tests for the resolved
//! cases (ESC alone followed by more input, Meta+key, etc.).

/// One step of the classifier's internal state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// Collecting a CSI sequence's parameter/intermediate bytes, waiting for
    /// the final byte (0x40..=0x7E). Empty `body` + byte `M` is the X10 mouse
    /// special case, handled before generic accumulation.
    Csi(Vec<u8>),
    /// X10 mouse report: `n` more raw bytes to swallow unconditionally
    /// (starts at 3; any byte value, not CSI-parsed).
    X10Tail(u8),
    /// Inside an OSC/DCS/SOS/PM/APC string, collecting until ST (`ESC \`) or
    /// BEL. `saw_esc` is true right after seeing the ESC of a possible ST.
    Str {
        saw_esc: bool,
    },
}

/// Stateful input classifier. See the module doc.
#[derive(Debug, Clone)]
pub struct Classifier {
    state: State,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            state: State::Ground,
        }
    }
}

impl Classifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one read()'s worth of bytes. Returns `true` iff at least one real
    /// keystroke event completed during this call (the caller should then
    /// bump `last_keystroke` once, not once per byte).
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut keystroke = false;
        for &b in bytes {
            if self.step(b) {
                keystroke = true;
            }
        }
        keystroke
    }

    /// One byte through the state machine. Returns `true` iff this byte
    /// completed a real-keystroke event.
    fn step(&mut self, b: u8) -> bool {
        match &mut self.state {
            State::Ground => {
                if b == 0x1b {
                    self.state = State::Esc;
                    false
                } else {
                    // Every other ground-state byte (printable, control,
                    // UTF-8 continuation) is an ordinary keystroke.
                    true
                }
            }
            State::Esc => {
                match b {
                    b'[' => {
                        self.state = State::Csi(Vec::new());
                        false
                    }
                    b']' | b'P' | b'X' | b'^' | b'_' => {
                        self.state = State::Str { saw_esc: false };
                        false
                    }
                    _ => {
                        // ESC followed by something that isn't a recognised
                        // introducer: a Meta/Alt+key combo, or a bare ESC
                        // immediately followed by ordinary input. Both bytes
                        // count as real input.
                        self.state = State::Ground;
                        true
                    }
                }
            }
            State::Csi(body) => {
                if body.is_empty() && b == b'M' {
                    // X10 mouse: CSI M + 3 raw bytes, no param grammar at all.
                    self.state = State::X10Tail(3);
                    return false;
                }
                if (0x20..=0x3f).contains(&b) {
                    body.push(b);
                    return false;
                }
                if (0x40..=0x7e).contains(&b) {
                    let excluded = is_excluded_csi(body, b);
                    self.state = State::Ground;
                    return !excluded;
                }
                // Malformed/interrupted CSI (e.g. a raw control byte mid-
                // sequence): abandon it and treat this byte fresh in Ground.
                self.state = State::Ground;
                self.step(b)
            }
            State::X10Tail(remaining) => {
                *remaining -= 1;
                if *remaining == 0 {
                    self.state = State::Ground;
                }
                false
            }
            State::Str { saw_esc } => {
                if *saw_esc {
                    // Only `ESC \` (ST) closes the string; any other byte
                    // after ESC inside a string is unusual — treat the
                    // whole thing as still-excluded content and keep
                    // scanning from Ground (lenient: a stray ESC here is
                    // vanishingly rare on a real terminal reply).
                    self.state = State::Ground;
                    return false;
                }
                match b {
                    0x07 => {
                        // BEL also terminates (xterm-style OSC leniency,
                        // extended here to every string type).
                        self.state = State::Ground;
                        false
                    }
                    0x1b => {
                        *saw_esc = true;
                        false
                    }
                    _ => false,
                }
            }
        }
    }
}

/// Is the completed CSI sequence (`body` = bytes between `[` and the final
/// byte, `fin` = the final byte) one of the excluded terminal-reply/report
/// shapes? Pure.
fn is_excluded_csi(body: &[u8], fin: u8) -> bool {
    // Focus reports: CSI I / CSI O, no body at all.
    if body.is_empty() {
        return matches!(fin, b'I' | b'O');
    }
    // SGR mouse: CSI < ... M/m
    if body[0] == b'<' && matches!(fin, b'M' | b'm') {
        return true;
    }
    // DA primary/secondary: CSI ? ... c / CSI > ... c
    if (body[0] == b'?' || body[0] == b'>') && fin == b'c' {
        return true;
    }
    // Kitty keyboard flags reply: CSI ? ... u
    if body[0] == b'?' && fin == b'u' {
        return true;
    }
    // DECRPM: CSI ? ... $ y
    if body[0] == b'?' && fin == b'y' && body.last() == Some(&b'$') {
        return true;
    }
    // Cursor position report: CSI ... R (no private-marker prefix)
    if fin == b'R' && body[0] != b'?' && body[0] != b'<' && body[0] != b'>' {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(bytes: &[u8]) -> bool {
        Classifier::new().feed(bytes)
    }

    // ── plain input always counts ─────────────────────────────────────────
    #[test]
    fn plain_bytes_are_keystrokes() {
        assert!(classify(b"a"));
        assert!(classify(b"hello"));
        assert!(classify(&[0x03])); // Ctrl-C byte, relayed raw, still a keystroke
        assert!(classify(&[0x0d])); // Enter
        assert!(classify(&[0x7f])); // Backspace/DEL
    }

    #[test]
    fn arrow_and_function_keys_count() {
        // Arrow keys (CSI A/B/C/D) and similar are NOT in the excluded set.
        for fin in *b"ABCDHF" {
            let mut c = Classifier::new();
            assert!(c.feed(&[0x1b, b'[', fin]), "CSI {}", fin as char);
        }
    }

    #[test]
    fn meta_key_combo_counts() {
        // ESC followed by a plain letter (Alt+key in many terminals).
        assert!(classify(&[0x1b, b'a']));
    }

    #[test]
    fn lone_esc_with_more_input_after_resolves_both_as_keystrokes() {
        let mut c = Classifier::new();
        assert!(!c.feed(&[0x1b])); // pending, not yet resolved
        assert!(c.feed(b"a")); // resolves: ESC+'a' = Meta+a, a keystroke
    }

    // ── focus reports excluded ──────────────────────────────────────────────
    #[test]
    fn focus_in_out_excluded() {
        assert!(!classify(&[0x1b, b'[', b'I']));
        assert!(!classify(&[0x1b, b'[', b'O']));
    }

    // ── mouse reports excluded ───────────────────────────────────────────────
    #[test]
    fn sgr_mouse_excluded() {
        assert!(!classify(b"\x1b[<0;10;20M"));
        assert!(!classify(b"\x1b[<0;10;20m"));
    }

    #[test]
    fn x10_mouse_excluded_and_consumes_exactly_three_raw_bytes() {
        let mut c = Classifier::new();
        // CSI M + 3 arbitrary bytes (including one that looks like ESC).
        assert!(!c.feed(&[0x1b, b'[', b'M', 0x20, 0x1b, 0x20]));
        // The classifier must be back in Ground: the next plain byte counts.
        assert!(c.feed(b"x"));
    }

    #[test]
    fn x10_mouse_split_across_two_reads() {
        let mut c = Classifier::new();
        assert!(!c.feed(&[0x1b, b'[', b'M', 0x20]));
        assert!(!c.feed(&[0x20, 0x20])); // remaining 2 raw bytes
        assert!(c.feed(b"x"));
    }

    // ── terminal replies excluded ────────────────────────────────────────────
    #[test]
    fn da_primary_and_secondary_excluded() {
        assert!(!classify(b"\x1b[?1;2c"));
        assert!(!classify(b"\x1b[>0;10;1c"));
    }

    #[test]
    fn kitty_keyboard_flags_reply_excluded() {
        assert!(!classify(b"\x1b[?1u"));
    }

    #[test]
    fn cursor_position_report_excluded() {
        assert!(!classify(b"\x1b[24;80R"));
    }

    #[test]
    fn decrpm_excluded() {
        assert!(!classify(b"\x1b[?2004;1$y"));
    }

    #[test]
    fn osc_colour_reply_excluded_st_and_bel_terminated() {
        // ST-terminated
        assert!(!classify(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
        // BEL-terminated
        assert!(!classify(b"\x1b]11;rgb:0000/0000/0000\x07"));
    }

    #[test]
    fn osc_split_across_reads() {
        let mut c = Classifier::new();
        assert!(!c.feed(b"\x1b]11;rgb:0000"));
        assert!(!c.feed(b"/0000/0000\x1b\\"));
        assert!(c.feed(b"x"));
    }

    #[test]
    fn dcs_reply_excluded() {
        // DECRQSS-style DCS reply.
        assert!(!classify(b"\x1bP1$r0\"q\x1b\\"));
    }

    // ── never delays, never stalls on a full CSI in one call ────────────────
    #[test]
    fn a_complete_sequence_in_one_call_is_classified_immediately() {
        let mut c = Classifier::new();
        assert!(!c.feed(b"\x1b[I"));
        assert!(c.feed(b"y")); // back to Ground right after
    }

    #[test]
    fn split_csi_classified_correctly_once_final_byte_arrives() {
        let mut c = Classifier::new();
        assert!(!c.feed(&[0x1b, b'[']));
        assert!(!c.feed(b"<0;1;1"));
        assert!(!c.feed(b"M"));
    }

    #[test]
    fn malformed_csi_interrupted_by_control_byte_is_abandoned_and_reprocessed() {
        let mut c = Classifier::new();
        // ESC [ then a stray control byte (not param/intermediate/final): the
        // pending CSI is abandoned and the control byte counts fresh.
        assert!(c.feed(&[0x1b, b'[', 0x01]));
    }
}
