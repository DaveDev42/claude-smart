//! Unix pty relay launcher.
//!
//! In direct mode ([`super::posix::PosixLauncher`]) csm hands the real
//! terminal straight to claude. In relay mode csm keeps the real terminal
//! for itself (in raw mode) and puts claude on a *second*, inner pty it
//! owns, with a small re-exec'd copy of itself (the "leader", see
//! [`leader`]) doing the actual job control on that inner pty. Four threads
//! move bytes and signals between the two:
//!
//!   - **input**: outer stdin → master (claude's input), unless
//!     [`RelayIo::hold_input`] is currently held, in which case bytes are
//!     buffered and flushed in order once the hold is released.
//!   - **output**: master (claude's output) → outer stdout, tracking VT
//!     ground state ([`ground::GroundTracker`]) so [`RelayIo::write_terminal`]
//!     can inject bytes only between claude's own sequences.
//!   - **signal**: handlers for {SIGWINCH, SIGCONT, SIGTERM, SIGHUP, SIGINT}
//!     write the signal number to a non-blocking self-pipe (installed by
//!     `SignalGuard` before the leader is spawned, restored afterwards); a
//!     dedicated thread reads the pipe and does the real work. A blocked-mask
//!     plus `sigwait` design was rejected: on macOS a blocked SIGCONT leaves a
//!     stopped process flagged as stopped for its parent (see
//!     `RELAY_SIGNALS`). `SIGTSTP` is not handled, so `kill(getpid(),
//!     SIGTSTP)` below keeps its default (whole-process-stopping) behaviour.
//!   - **leader control**: reads `leader::LeaderReport` lines from the
//!     leader's stdout. On `stopped` it restores the outer terminal to
//!     cooked mode and sends itself `SIGTSTP`, which — which stops this entire process,
//!     this thread included, until something sends `SIGCONT`; the signal
//!     thread is what notices the resume (`sigwait` returning `SIGCONT`),
//!     re-enters raw mode, resyncs the window size and tells the leader
//!     `cont`. On `exit`/`signal` it reports the final status and this hop
//!     is done.
//!
//! Activation ([`try_activate`]) is checked once per `csm` process start
//! (mode not `off`, both stdin and stdout are terminals, csm's own process
//! group is the terminal's foreground group, `CSM_RELAY` is not `"0"`). Any
//! failure *after* that — opening the inner pty, spawning the leader, or the
//! leader never reporting a pid — is a per-hop setup failure: it is logged
//! once and this hop falls back to [`super::posix::PosixLauncher`], exactly
//! as if relay mode had never been picked for this run.

//!
//! ## Windows
//!
//! On Windows the same public surface ([`RelayObserver`], [`RelayIo`],
//! [`InputHold`]) is backed by a ConPTY instead of a pty: see [`conpty`].
//! The pure parts ([`should_activate`], the keystroke classifier and the VT
//! ground tracker) are shared as they are.

mod classify;
#[cfg(any(windows, test))]
pub(crate) mod conpty_logic;
mod ground;
#[cfg(unix)]
pub mod leader;

#[cfg(unix)]
mod pty;
#[cfg(unix)]
use pty as sys;
#[cfg(unix)]
pub(crate) use pty::{RelayLauncher, platform_should_activate, reassert_raw};

#[cfg(windows)]
pub mod conpty;
#[cfg(windows)]
use conpty as sys;
#[cfg(windows)]
pub(crate) use conpty::{ConptyLauncher, platform_should_activate, reassert_raw};

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::IdleCompactMode;
use crate::platform::launcher::ChildEnv;
use ground::GroundTracker;

// ─── activation ───────────────────────────────────────────────────────────

/// Pure activation predicate. `csm_relay_env` is the raw `CSM_RELAY` value if
/// set (`None` = unset). See the module doc for the four conditions.
pub fn should_activate(
    mode: IdleCompactMode,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    is_foreground: bool,
    csm_relay_env: Option<&str>,
) -> bool {
    if mode == IdleCompactMode::Off {
        return false;
    }
    if csm_relay_env == Some("0") {
        return false;
    }
    stdin_is_tty && stdout_is_tty && is_foreground
}


// ─── public API: RelayObserver / RelayIo / InputHold ─────────────────────────

/// Hooks other code (idle-compact's tick/typing glue) registers to be told
/// about a relay session. Both methods have no-op defaults.
pub trait RelayObserver: Send + Sync {
    /// Called once, right after the relay's worker threads start, with the
    /// shared handle used to watch output and type into the session for the
    /// rest of this hop, claude's real pid, and the environment changes csm
    /// launched it with (the config dir pin lives there).
    fn on_start(&self, _io: Arc<RelayIo>, _claude_pid: u32, _env: &ChildEnv) {}
    /// Called from the output thread with each chunk of claude's output,
    /// after it was written to the outer terminal.
    fn on_output(&self, _bytes: &[u8]) {}
    /// Called when the outer terminal's size changed (rows, cols).
    fn on_resize(&self, _rows: u16, _cols: u16) {}
    /// Called once claude has exited and this hop's relay session is torn
    /// down. Never called for a hop that fell back to direct mode.
    fn on_exit(&self) {}
}

/// A [`RelayObserver`] that does nothing (the default for `pick_launcher`
/// until idle-compact's glue registers a real one).
pub struct NoopObserver;
impl RelayObserver for NoopObserver {}

struct OutputState {
    tracker: GroundTracker,
}

/// The handle a `RelayObserver` receives for a running relay session:
/// timestamps for idle detection, and ways to type into or write onto
/// claude's session from outside the normal input path.
pub struct RelayIo {
    master: sys::Master,
    master_write_lock: Mutex<()>,
    last_keystroke: Mutex<Option<Instant>>,
    last_output: Mutex<Option<Instant>>,
    hold_depth: Mutex<u32>,
    held_buffer: Mutex<Vec<u8>>,
    output: Mutex<OutputState>,
    size: Mutex<(u16, u16)>,
}

impl RelayIo {
    fn new(master: sys::Master, initial_size: (u16, u16)) -> Self {
        RelayIo {
            master,
            master_write_lock: Mutex::new(()),
            last_keystroke: Mutex::new(None),
            last_output: Mutex::new(None),
            hold_depth: Mutex::new(0),
            held_buffer: Mutex::new(Vec::new()),
            output: Mutex::new(OutputState {
                tracker: GroundTracker::new(),
            }),
            size: Mutex::new(initial_size),
        }
    }

    /// When the last real keystroke was classified (see [`classify`]), if any.
    pub fn last_keystroke(&self) -> Option<Instant> {
        *self.last_keystroke.lock().unwrap()
    }

    /// When claude last produced output, if any.
    pub fn last_output(&self) -> Option<Instant> {
        *self.last_output.lock().unwrap()
    }

    /// The current window size (rows, cols).
    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    /// Hold the outer terminal's real input: bytes the user types are
    /// buffered (in order) instead of reaching claude, until the returned
    /// guard is dropped, at which point they are flushed in order. Nested
    /// holds are supported (input flushes only once the outermost is
    /// dropped). [`inject`](Self::inject) bypasses the hold entirely.
    pub fn hold_input(&self) -> InputHold<'_> {
        *self.hold_depth.lock().unwrap() += 1;
        InputHold { io: self }
    }

    fn release_hold(&self) {
        let mut depth = self.hold_depth.lock().unwrap();
        if *depth > 0 {
            *depth -= 1;
        }
        if *depth == 0 {
            let buffered = std::mem::take(&mut *self.held_buffer.lock().unwrap());
            drop(depth);
            if !buffered.is_empty() {
                let _ = self.write_master(&buffered);
            }
        }
    }

    fn is_held(&self) -> bool {
        *self.hold_depth.lock().unwrap() > 0
    }

    fn buffer_held(&self, bytes: &[u8]) {
        self.held_buffer.lock().unwrap().extend_from_slice(bytes);
    }

    fn write_master(&self, bytes: &[u8]) -> io::Result<()> {
        let _guard = self.master_write_lock.lock().unwrap();
        sys::write_master(&self.master, bytes)
    }

    /// Type bytes directly into claude's input, bypassing any active
    /// [`hold_input`](Self::hold_input) hold. Used for programmatic typing
    /// (e.g. idle-compact sending `/compact\n`).
    pub fn inject(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_master(bytes)
    }

    /// Relay one chunk of claude's own output (from the output thread):
    /// write it verbatim and feed the ground tracker. Not part of the public
    /// API — outside code never has raw claude output to relay.
    fn relay_output(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.output.lock().unwrap();
        sys::write_stdout(bytes)?;
        state.tracker.feed(bytes);
        Ok(())
    }

    /// Write bytes into the outer terminal's output stream as if claude had
    /// written them, but only while the stream is at VT ground state (never
    /// mid-sequence, so an injected notification can never land inside one
    /// of claude's own escape sequences). Returns `Ok(true)` if written,
    /// `Ok(false)` if the stream was not at ground and nothing was written.
    pub fn write_terminal(&self, bytes: &[u8]) -> io::Result<bool> {
        let mut state = self.output.lock().unwrap();
        if !state.tracker.is_ground() {
            return Ok(false);
        }
        sys::write_stdout(bytes)?;
        state.tracker.feed(bytes);
        Ok(true)
    }

    fn note_keystroke(&self) {
        *self.last_keystroke.lock().unwrap() = Some(Instant::now());
    }

    fn note_output(&self) {
        *self.last_output.lock().unwrap() = Some(Instant::now());
    }

    fn set_size(&self, rows: u16, cols: u16) {
        *self.size.lock().unwrap() = (rows, cols);
    }
}

/// Guard returned by [`RelayIo::hold_input`]. Dropping it releases the hold
/// (flushing any buffered input once the outermost hold is released).
pub struct InputHold<'a> {
    io: &'a RelayIo,
}

impl Drop for InputHold<'_> {
    fn drop(&mut self) {
        self.io.release_hold();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(off: bool) -> IdleCompactMode {
        if off {
            IdleCompactMode::Off
        } else {
            IdleCompactMode::On
        }
    }

    #[test]
    fn off_mode_never_activates() {
        assert!(!should_activate(mode(true), true, true, true, None));
    }

    #[test]
    fn requires_both_tty() {
        assert!(!should_activate(mode(false), false, true, true, None));
        assert!(!should_activate(mode(false), true, false, true, None));
        assert!(should_activate(mode(false), true, true, true, None));
    }

    #[test]
    fn requires_foreground() {
        assert!(!should_activate(mode(false), true, true, false, None));
    }

    #[test]
    fn csm_relay_zero_disables() {
        assert!(!should_activate(mode(false), true, true, true, Some("0")));
        assert!(should_activate(mode(false), true, true, true, Some("1")));
        assert!(should_activate(mode(false), true, true, true, None));
    }

    #[test]
    fn dry_run_mode_activates_like_on() {
        assert!(should_activate(
            IdleCompactMode::DryRun,
            true,
            true,
            true,
            None
        ));
    }
}
