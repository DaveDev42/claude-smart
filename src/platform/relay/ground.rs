//! VT "ground state" tracker over claude's own output stream.
//!
//! [`RelayIo::write_terminal`](super::RelayIo::write_terminal) must only ever
//! write into the output stream while it is at the terminal's VT ground state
//! (not in the middle of an escape/control sequence claude itself is
//! writing) — otherwise an injected write (e.g. an OSC 777 notification) can
//! land inside one of claude's own sequences and corrupt both. This module is
//! fed every byte the output thread relays, in order, and answers "is the
//! stream at ground right now?" — it does not classify or interpret what the
//! sequences MEAN (that is `vt100::Screen`'s job elsewhere in the project);
//! it only tracks enough shape (ground / ESC / CSI / OSC / DCS / other
//! string) to find the boundaries.
//!
//! Same X10-mouse raw-byte-tail handling as the input classifier
//! ([`super::classify`]) for robustness: a coincidental `ESC [ M` in output
//! is vanishingly unlikely from claude itself, but treating it as three
//! unconditional raw bytes (rather than possibly mis-triggering on one of
//! them, e.g. a `0x1b` byte used as a coordinate) avoids a state machine that
//! can get stuck away from ground.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi { empty_body: bool },
    X10Tail(u8),
    Str { saw_esc: bool },
}

/// Tracks whether the output stream is currently at VT ground state.
#[derive(Debug, Clone)]
pub struct GroundTracker {
    state: State,
}

impl Default for GroundTracker {
    fn default() -> Self {
        Self {
            state: State::Ground,
        }
    }
}

impl GroundTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the stream at ground state right now (no pending sequence)?
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground
    }

    /// Feed one chunk of output bytes, in order.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        self.state = match self.state {
            State::Ground => {
                if b == 0x1b {
                    State::Esc
                } else {
                    State::Ground
                }
            }
            State::Esc => match b {
                b'[' => State::Csi { empty_body: true },
                b']' | b'P' | b'X' | b'^' | b'_' => State::Str { saw_esc: false },
                _ => State::Ground,
            },
            State::Csi { empty_body } => {
                if empty_body && b == b'M' {
                    State::X10Tail(3)
                } else if (0x20..=0x3f).contains(&b) {
                    State::Csi { empty_body: false }
                } else if (0x40..=0x7e).contains(&b) {
                    State::Ground
                } else {
                    // Malformed/interrupted CSI: abandon it back to ground
                    // and reprocess this byte fresh, same as the classifier.
                    self.state = State::Ground;
                    return self.step(b);
                }
            }
            State::X10Tail(remaining) => {
                if remaining <= 1 {
                    State::Ground
                } else {
                    State::X10Tail(remaining - 1)
                }
            }
            State::Str { saw_esc } => {
                if saw_esc {
                    // Only ESC \ (ST) closes it; anything else after ESC
                    // inside a string is treated leniently as closed too, so
                    // a malformed string can never wedge the tracker away
                    // from ground forever.
                    State::Ground
                } else {
                    match b {
                        0x07 => State::Ground,
                        0x1b => State::Str { saw_esc: true },
                        _ => State::Str { saw_esc: false },
                    }
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ground_after(bytes: &[u8]) -> bool {
        let mut t = GroundTracker::new();
        t.feed(bytes);
        t.is_ground()
    }

    #[test]
    fn starts_at_ground() {
        assert!(GroundTracker::new().is_ground());
    }

    #[test]
    fn plain_text_stays_at_ground() {
        assert!(ground_after(b"hello world\n"));
    }

    #[test]
    fn mid_csi_is_not_ground() {
        assert!(!ground_after(b"\x1b[3"));
    }

    #[test]
    fn a_complete_csi_returns_to_ground() {
        assert!(ground_after(b"\x1b[2J"));
        assert!(ground_after(b"\x1b[38;5;200m"));
    }

    #[test]
    fn mid_osc_is_not_ground_st_terminated_returns() {
        assert!(!ground_after(b"\x1b]0;title"));
        assert!(ground_after(b"\x1b]0;title\x1b\\"));
        assert!(ground_after(b"\x1b]0;title\x07"));
    }

    #[test]
    fn mid_dcs_is_not_ground_terminated_returns() {
        assert!(!ground_after(b"\x1bP1$r"));
        assert!(ground_after(b"\x1bP1$r0\"q\x1b\\"));
    }

    #[test]
    fn x10_mouse_shaped_bytes_consumed_as_three_raw_bytes() {
        // Even a 0x1b in the raw tail must not re-trigger ESC parsing.
        assert!(ground_after(&[0x1b, b'[', b'M', 0x20, 0x1b, 0x20]));
    }

    #[test]
    fn split_across_feeds_tracks_correctly() {
        let mut t = GroundTracker::new();
        t.feed(&[0x1b, b'[']);
        assert!(!t.is_ground());
        t.feed(b"38;5;1");
        assert!(!t.is_ground());
        t.feed(b"m");
        assert!(t.is_ground());
    }

    #[test]
    fn interrupted_csi_recovers_to_ground_on_next_plain_byte() {
        let mut t = GroundTracker::new();
        t.feed(&[0x1b, b'[', 0x01]); // malformed: abandoned, 0x01 reprocessed in ground
        assert!(t.is_ground());
    }
}
