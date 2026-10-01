//! The pty-relay supervisor: the I/O shell around [`super::deliver`]'s state
//! machine. It is the [`RelayObserver`] the relay launcher registers.
//!
//! - `on_output` feeds every byte claude prints into a [`vt100::Parser`] (with
//!   a [`TitleTracker`]), `on_resize` resizes it, so the screen model always
//!   matches what the user sees.
//! - `on_start` clears a leftover request file with this process's pid and
//!   starts one watcher thread; `on_exit` stops and joins it, so no thread
//!   outlives the launch.
//! - The watcher takes the hand-off request ([`super::request`]) about once a
//!   second. For an active request it builds an [`Observation`] from the
//!   screen, the relay's keystroke/output clocks and the session-status veto,
//!   steps the [`Delivery`] machine and carries out each [`Action`]. It
//!   types nothing that the machine did not ask for, and the input hold is a
//!   guard dropped on every exit path, panics included.
//!
//! [`observe`] is the pure mapping from a screen to an [`Observation`]; it
//! is where the safety checks the fixtures called for live: the busy veto
//! (window-title spinner or activity line), the vim cross-check against the
//! request's `vim_mode`, and the slash-menu highlight check before Enter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::deliver::{Action, BoxObs, Delivery, Mode, Observation};
use super::request::{self, Request};
use super::{LogFields, log_outcome, status};
use crate::platform::launcher::ChildEnv;
use crate::platform::relay::{InputHold, RelayIo, RelayObserver};
use crate::screen_check::{self, BoxState, TitleTracker, VimState};

/// The one string this supervisor ever types (after the optional `i`).
const COMPACT: &str = "/compact";
/// How often the watcher looks for a new request while none is active.
const POLL: Duration = Duration::from_secs(1);
/// The output must have been quiet this long before an OSC notification is
/// written into the stream.
const NOTIFY_QUIET: Duration = Duration::from_millis(500);
/// A notification not delivered within this long is dropped.
const NOTIFY_GIVE_UP: Duration = Duration::from_secs(5);
/// After typing, wait at most this long for claude to echo something.
const ECHO_WAIT: Duration = Duration::from_millis(1000);

// ─── observation mapping (pure) ───────────────────────────────────────────────

/// What the screen showed, for the log line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObsInfo {
    pub box_state: &'static str,
    pub vim: &'static str,
    pub status: Option<String>,
}

/// Map one screen (plus the window title, the request's `vim_mode`, and the
/// clocks) to the [`Observation`] the state machine steps on.
///
/// - `status_veto` is the session-status veto, else `screen-busy` when the
///   title or an activity line says a turn is running (the box is `Empty`
///   while a reply generates).
/// - The box state cross-checks vim with the request (`req_vim`, the
///   statusline payload's `vim.mode`): no vim reported means plain `Empty`
///   and `i` is never sent; `insert` needs the `-- INSERT --` marker;
///   `normal` needs it absent (and `entered_insert` lets the marker appear
///   once csm itself sent `i`); anything else is `NotFound`, which the state
///   machine retries until the deadline.
/// - `box_text_is_compact` is true only when the box holds exactly
///   `/compact` and Enter would run exactly that (no slash menu, or the
///   highlighted entry is `/compact`).
#[allow(clippy::too_many_arguments)]
pub fn observe(
    screen: &vt100::Screen,
    title: &str,
    req_vim: Option<&str>,
    entered_insert: bool,
    session_veto: Option<String>,
    now_ms: u64,
    last_keystroke_ms: Option<u64>,
    last_output_ms: Option<u64>,
) -> (Observation, ObsInfo) {
    let state = screen_check::input_box(screen);
    let box_obs = box_obs(state, req_vim, entered_insert);
    let busy = screen_check::busy(screen, title);
    let status_veto = session_veto.or_else(|| busy.then(|| "screen-busy".to_owned()));
    let info = ObsInfo {
        box_state: match state {
            BoxState::Empty { .. } => "empty",
            BoxState::Draft { .. } => "draft",
            BoxState::NotFound => "not-found",
        },
        vim: match state {
            BoxState::Empty { vim } | BoxState::Draft { vim } => match vim {
                VimState::Insert => "insert",
                VimState::Normal => "normal",
                VimState::Off => "off",
            },
            BoxState::NotFound => "unknown",
        },
        status: status_veto.clone(),
    };
    let obs = Observation {
        now_ms,
        box_obs,
        box_text_is_compact: screen_check::box_text_is(screen, COMPACT)
            && screen_check::enter_runs(screen, COMPACT),
        compaction_started: screen_check::compaction_started(screen),
        status_veto,
        last_keystroke_ms,
        last_output_ms,
    };
    (obs, info)
}

/// The vim cross-check: see [`observe`].
pub fn box_obs(state: BoxState, req_vim: Option<&str>, entered_insert: bool) -> BoxObs {
    match state {
        BoxState::NotFound => BoxObs::NotFound,
        BoxState::Draft { .. } => BoxObs::Draft,
        BoxState::Empty { vim } => match req_vim.map(|v| v.trim().to_ascii_lowercase()) {
            None => BoxObs::Empty,
            Some(v) if v == "insert" => match vim {
                VimState::Insert => BoxObs::EmptyVimInsert,
                _ => BoxObs::NotFound,
            },
            Some(v) if v == "normal" => match vim {
                VimState::Normal => BoxObs::EmptyVimNormal,
                VimState::Insert if entered_insert => BoxObs::EmptyVimInsert,
                _ => BoxObs::NotFound,
            },
            Some(_) => BoxObs::NotFound,
        },
    }
}

/// `ESC ] 777 ; notify ; <title> ; <body> BEL`, control characters and `;`
/// stripped so a body can never end the sequence early.
fn osc777(title: &str, body: &str) -> Vec<u8> {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control() && *c != ';')
            .collect::<String>()
    };
    format!("\x1b]777;notify;{};{}\x07", clean(title), clean(body)).into_bytes()
}

/// Where claude runs: the config dir csm launched it with (the child env's
/// `CLAUDE_CONFIG_DIR`, set when csm pins one), else this process's
/// `CLAUDE_CONFIG_DIR`, else `~/.claude`. An env that removes the variable
/// reads as unset.
pub fn config_dir_for(env: &ChildEnv) -> PathBuf {
    let key = std::ffi::OsStr::new("CLAUDE_CONFIG_DIR");
    if let Some(v) = env.set.get(key)
        && !v.is_empty()
    {
        return PathBuf::from(v);
    }
    if env.remove.iter().any(|k| k == key) {
        return crate::paths::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude");
    }
    crate::paths::runtime_dir()
}

// ─── the observer ────────────────────────────────────────────────────────────

struct Stop {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Stop {
    fn new() -> Self {
        Stop {
            flag: AtomicBool::new(false),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let _g = self.lock.lock().unwrap();
        self.cv.notify_all();
    }

    fn stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Sleep up to `d`, waking early when stopped.
    fn sleep(&self, d: Duration) {
        let g = self.lock.lock().unwrap();
        if self.stopped() {
            return;
        }
        let _ = self.cv.wait_timeout(g, d).unwrap();
    }
}

struct Watcher {
    stop: Arc<Stop>,
    handle: JoinHandle<()>,
}

/// The [`RelayObserver`] the relay launcher registers.
pub struct Supervisor {
    parser: Arc<Mutex<vt100::Parser<TitleTracker>>>,
    watcher: Mutex<Option<Watcher>>,
}

impl Supervisor {
    pub fn new() -> Self {
        Supervisor {
            parser: Arc::new(Mutex::new(new_parser(24, 80))),
            watcher: Mutex::new(None),
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// Smallest grid the screen model is built with. `vt100` panics on grids of a
/// row or a column or two, and an outer terminal that has not reported a size
/// yet reads as 0x0; a screen this small cannot show the input box anyway, so
/// the supervisor just sees no box.
pub(crate) const MIN_ROWS: u16 = 5;
pub(crate) const MIN_COLS: u16 = 20;

fn new_parser(rows: u16, cols: u16) -> vt100::Parser<TitleTracker> {
    vt100::Parser::new_with_callbacks(
        rows.max(MIN_ROWS),
        cols.max(MIN_COLS),
        0,
        TitleTracker::default(),
    )
}

/// Lock a mutex even if a panic once poisoned it: the relay must never die
/// because the observer did.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RelayObserver for Supervisor {
    fn on_start(&self, io: Arc<RelayIo>, claude_pid: u32, env: &ChildEnv) {
        let (rows, cols) = io.size();
        *lock(&self.parser) = new_parser(rows, cols);
        let dir = crate::paths::idle_compact_requests_dir();
        let own_pid = std::process::id();
        request::clear_own_request(&dir, own_pid);

        let ctx = Ctx {
            io,
            parser: Arc::clone(&self.parser),
            claude_pid,
            config_dir: config_dir_for(env),
            requests_dir: dir,
            supervisor_pid: own_pid,
            t0: Instant::now(),
        };
        let stop = Arc::new(Stop::new());
        let thread_stop = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name("csm-idle-compact".to_owned())
            .spawn(move || watch(ctx, thread_stop));
        if let Ok(handle) = spawned {
            *lock(&self.watcher) = Some(Watcher { stop, handle });
        }
    }

    fn on_output(&self, bytes: &[u8]) {
        // The screen model is a convenience; if the parser ever panics on a
        // byte stream it is replaced by a blank one (no box found, so
        // nothing gets typed) and the relay carries on.
        let mut parser = lock(&self.parser);
        let fed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parser.process(bytes);
        }));
        if fed.is_err() {
            let (rows, cols) = parser.screen().size();
            *parser = new_parser(rows, cols);
            crate::platform::relay::reassert_raw();
        }
    }

    fn on_resize(&self, rows: u16, cols: u16) {
        let mut parser = lock(&self.parser);
        let (rows, cols) = (rows.max(MIN_ROWS), cols.max(MIN_COLS));
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parser.screen_mut().set_size(rows, cols);
        }));
        if done.is_err() {
            *parser = new_parser(rows, cols);
            crate::platform::relay::reassert_raw();
        }
    }

    fn on_exit(&self) {
        if let Some(w) = lock(&self.watcher).take() {
            w.stop.stop();
            let _ = w.handle.join();
        }
    }
}

// ─── the watcher ─────────────────────────────────────────────────────────────

struct Ctx {
    io: Arc<RelayIo>,
    parser: Arc<Mutex<vt100::Parser<TitleTracker>>>,
    claude_pid: u32,
    config_dir: PathBuf,
    requests_dir: PathBuf,
    supervisor_pid: u32,
    t0: Instant,
}

impl Ctx {
    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn ms_of(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.t0).as_millis() as u64
    }
}

fn watch(ctx: Ctx, stop: Arc<Stop>) {
    while !stop.stopped() {
        let now = crate::epoch::now_secs() as i64;
        if let Some(req) = request::take_request(&ctx.requests_dir, ctx.supervisor_pid, now) {
            // A panic inside one delivery must not end the watcher; the
            // input-hold guard is released while it unwinds.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_request(&ctx, &stop, req);
            }));
        }
        stop.sleep(POLL);
    }
}

struct PendingNotify {
    bytes: Vec<u8>,
    since: Instant,
}

/// Write the pending notification once the stream is at a sequence boundary
/// and quiet; drop it when it has waited too long.
fn flush_notify(io: &RelayIo, pending: &mut Option<PendingNotify>) {
    let Some(p) = pending.as_ref() else { return };
    if p.since.elapsed() > NOTIFY_GIVE_UP {
        *pending = None;
        return;
    }
    let quiet = io.last_output().is_none_or(|t| t.elapsed() >= NOTIFY_QUIET);
    if !quiet {
        return;
    }
    match io.write_terminal(&p.bytes) {
        Ok(false) => {}
        Ok(true) | Err(_) => *pending = None,
    }
}

/// Type `bytes` and give claude a moment to echo before the next look at the
/// screen, so the following observation sees the effect of this write.
fn inject_and_settle(io: &RelayIo, stop: &Stop, bytes: &[u8]) {
    let before = io.last_output();
    if io.inject(bytes).is_err() {
        return;
    }
    let start = Instant::now();
    while start.elapsed() < ECHO_WAIT && io.last_output() == before && !stop.stopped() {
        stop.sleep(Duration::from_millis(20));
    }
    stop.sleep(Duration::from_millis(50));
}

fn run_request(ctx: &Ctx, stop: &Stop, req: Request) {
    let mode = match req.mode.as_str() {
        "on" => Mode::On,
        "dry-run" => Mode::DryRun,
        _ => return,
    };
    let io: &RelayIo = &ctx.io;
    let now_epoch = crate::epoch::now_secs() as i64;
    let remaining_ms = (req.deadline - now_epoch).max(0) as u64 * 1000;
    let mut delivery = Delivery::new(
        mode,
        ctx.now_ms() + remaining_ms,
        req.remaining_secs,
        req.recache_tokens,
    );
    let mut hold: Option<InputHold<'_>> = None;
    let mut entered_insert = false;
    let mut notify: Option<PendingNotify> = None;
    let mut info: ObsInfo;
    // The screen that justified Enter. The log reports it rather than the
    // last screen, which shows claude already busy with the compaction.
    let mut at_enter: Option<ObsInfo> = None;

    let outcome = loop {
        if stop.stopped() {
            // The launch is over; nothing to log, the guard releases below.
            return;
        }
        flush_notify(io, &mut notify);
        let session_veto = status::check(&ctx.config_dir, ctx.claude_pid);
        let (obs, seen) = {
            let parser = lock(&ctx.parser);
            observe(
                parser.screen(),
                parser.callbacks().title(),
                req.vim_mode.as_deref(),
                entered_insert,
                session_veto,
                ctx.now_ms(),
                io.last_keystroke().map(|t| ctx.ms_of(t)),
                io.last_output().map(|t| ctx.ms_of(t)),
            )
        };
        info = seen;
        match delivery.step(&obs) {
            Action::Wait { ms } => stop.sleep(Duration::from_millis(ms)),
            Action::HoldInput => hold = Some(io.hold_input()),
            Action::Type(bytes) => {
                if bytes == b"i" {
                    entered_insert = true;
                }
                inject_and_settle(io, stop, &bytes);
            }
            Action::Rollback(bytes) => inject_and_settle(io, stop, &bytes),
            Action::PressEnter => {
                at_enter = Some(info.clone());
                inject_and_settle(io, stop, b"\r");
            }
            Action::ReleaseInput => drop(hold.take()),
            Action::Notify { title, body } => {
                notify = Some(PendingNotify {
                    bytes: osc777(&title, &body),
                    since: Instant::now(),
                });
            }
            Action::Finish(outcome) => break outcome,
        }
    };
    drop(hold);
    let info = at_enter.unwrap_or(info);

    log_outcome(
        &req.sid,
        &outcome.log_word(),
        &LogFields {
            remaining_secs: Some(req.remaining_secs),
            recache_tokens: Some(req.recache_tokens),
            box_state: Some(info.box_state),
            vim: Some(info.vim),
            status: info.status.as_deref(),
            ..Default::default()
        },
    );

    // The notification may still be waiting for a quiet moment.
    let give_up = Instant::now() + NOTIFY_GIVE_UP;
    while notify.is_some() && Instant::now() < give_up && !stop.stopped() {
        flush_notify(io, &mut notify);
        if notify.is_some() {
            stop.sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Meta {
        name: String,
        rows: u16,
        cols: u16,
    }

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/screens")
    }

    fn replay(name: &str) -> (vt100::Screen, String) {
        let index: Vec<Meta> =
            serde_json::from_slice(&std::fs::read(fixtures_dir().join("index.json")).unwrap())
                .unwrap();
        let m = index.iter().find(|m| m.name == name).expect(name);
        let raw = std::fs::read(fixtures_dir().join(format!("{name}.bin"))).unwrap();
        let mut p = vt100::Parser::new_with_callbacks(m.rows, m.cols, 0, TitleTracker::default());
        p.process(&raw);
        (p.screen().clone(), p.callbacks().title().to_owned())
    }

    fn obs_of(name: &str, req_vim: Option<&str>, entered: bool) -> (Observation, ObsInfo) {
        let (screen, title) = replay(name);
        observe(&screen, &title, req_vim, entered, None, 10_000, None, None)
    }

    #[test]
    fn busy_generating_is_vetoed_though_the_box_is_empty() {
        let (obs, info) = obs_of("busy-generating-120x40", Some("insert"), false);
        assert_eq!(obs.box_obs, BoxObs::EmptyVimInsert);
        assert_eq!(obs.status_veto.as_deref(), Some("screen-busy"));
        assert_eq!(info.status.as_deref(), Some("screen-busy"));
        // ... and the machine therefore never types.
        let mut d = Delivery::new(Mode::On, 1_000_000, 100, 200_000);
        assert!(matches!(d.step(&obs), Action::Wait { .. }));
    }

    #[test]
    fn activity_line_screen_is_vetoed() {
        let (obs, _) = obs_of("busy-activity-line-120x40", Some("insert"), false);
        assert_eq!(obs.status_veto.as_deref(), Some("screen-busy"));
    }

    #[test]
    fn idle_screen_has_no_veto() {
        let (obs, info) = obs_of("idle-after-turn-120x40", Some("insert"), false);
        assert_eq!(obs.status_veto, None);
        assert_eq!(obs.box_obs, BoxObs::EmptyVimInsert);
        assert_eq!((info.box_state, info.vim), ("empty", "insert"));
    }

    #[test]
    fn session_veto_wins_over_the_screen_reason() {
        let (screen, title) = replay("busy-generating-120x40");
        let (obs, _) = observe(
            &screen,
            &title,
            None,
            false,
            Some("waiting".to_owned()),
            1,
            None,
            None,
        );
        assert_eq!(obs.status_veto.as_deref(), Some("waiting"));
    }

    #[test]
    fn no_vim_in_the_request_means_plain_empty() {
        for name in ["idle-after-turn-120x40", "vim-normal-120x40"] {
            let (obs, _) = obs_of(name, None, false);
            assert_eq!(obs.box_obs, BoxObs::Empty, "{name}");
        }
    }

    #[test]
    fn request_insert_needs_the_insert_marker() {
        assert_eq!(
            obs_of("vim-insert-120x40", Some("insert"), false).0.box_obs,
            BoxObs::EmptyVimInsert
        );
        assert_eq!(
            obs_of("vim-normal-120x40", Some("insert"), false).0.box_obs,
            BoxObs::NotFound
        );
    }

    #[test]
    fn request_normal_needs_no_insert_marker() {
        assert_eq!(
            obs_of("vim-normal-120x40", Some("NORMAL"), false).0.box_obs,
            BoxObs::EmptyVimNormal
        );
        assert_eq!(
            obs_of("vim-insert-120x40", Some("normal"), false).0.box_obs,
            BoxObs::NotFound,
            "INSERT drawn while the request says NORMAL is a mismatch"
        );
    }

    #[test]
    fn insert_marker_is_accepted_once_csm_sent_i() {
        assert_eq!(
            obs_of("vim-insert-120x40", Some("normal"), true).0.box_obs,
            BoxObs::EmptyVimInsert
        );
    }

    #[test]
    fn unknown_request_vim_value_is_a_mismatch() {
        assert_eq!(
            obs_of("vim-insert-120x40", Some("visual"), false).0.box_obs,
            BoxObs::NotFound
        );
    }

    #[test]
    fn drafts_are_drafts_whatever_the_request_says() {
        for name in [
            "draft-hello-120x40",
            "draft-multiline-120x40",
            "draft-wrapped-120x40",
            "draft-paste-placeholder-120x40",
        ] {
            for vim in [None, Some("insert"), Some("normal")] {
                assert_eq!(obs_of(name, vim, false).0.box_obs, BoxObs::Draft, "{name}");
            }
        }
    }

    #[test]
    fn dialogs_are_not_found() {
        for name in [
            "ask-user-question-dialog-120x40",
            "permission-prompt-dialog-120x40",
            "model-picker-dialog-120x40",
        ] {
            assert_eq!(
                obs_of(name, None, false).0.box_obs,
                BoxObs::NotFound,
                "{name}"
            );
        }
    }

    #[test]
    fn typed_compact_with_matching_menu_is_pressable() {
        let (obs, info) = obs_of("compact-menu-typed-120x40", Some("insert"), true);
        assert!(obs.box_text_is_compact);
        assert_eq!(info.box_state, "draft");
    }

    #[test]
    fn typed_compact_with_a_moved_highlight_is_not_pressable() {
        let raw = std::fs::read(fixtures_dir().join("compact-menu-typed-120x40.bin")).unwrap();
        let mut moved = raw.clone();
        moved.extend_from_slice(
            b"\x1b[32;3H\x1b[38;2;153;153;153m/compact\x1b[33;3H\x1b[38;2;177;185;249m/autocompact\x1b[37;3H",
        );
        let mut p = vt100::Parser::new_with_callbacks(40, 120, 0, TitleTracker::default());
        p.process(&moved);
        let (obs, _) = observe(p.screen(), "", Some("insert"), true, None, 1, None, None);
        assert!(!obs.box_text_is_compact, "Enter would run /autocompact");
    }

    #[test]
    fn plain_draft_that_is_not_compact_is_not_pressable() {
        let (obs, _) = obs_of("draft-hello-120x40", Some("insert"), false);
        assert!(!obs.box_text_is_compact);
    }

    #[test]
    fn compaction_started_comes_through() {
        let (obs, _) = obs_of("compact-started-120x40", Some("insert"), false);
        assert!(obs.compaction_started);
    }

    #[test]
    fn osc777_is_one_well_formed_sequence() {
        let b = osc777("idle;compact", "body\x07with\x1bcontrols; and semicolons");
        let s = String::from_utf8(b).unwrap();
        assert!(s.starts_with("\x1b]777;notify;"));
        assert!(s.ends_with('\x07'));
        assert_eq!(s.matches('\x07').count(), 1);
        assert_eq!(s.matches('\x1b').count(), 1);
        assert_eq!(s.matches(';').count(), 3, "{s:?}");
    }

    #[test]
    fn config_dir_prefers_the_launch_pin() {
        let mut env = ChildEnv::default();
        env.set
            .insert("CLAUDE_CONFIG_DIR".into(), "/somewhere/pinned".into());
        assert_eq!(config_dir_for(&env), PathBuf::from("/somewhere/pinned"));
    }

    #[test]
    fn config_dir_falls_back_to_the_runtime_dir() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            assert_eq!(
                config_dir_for(&ChildEnv::default()),
                crate::paths::runtime_dir()
            );
        });
    }

    #[test]
    fn config_dir_with_the_variable_removed_is_the_home_default() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let mut env = ChildEnv::default();
            env.remove.push("CLAUDE_CONFIG_DIR".into());
            assert_eq!(config_dir_for(&env), tmp.path().join(".claude"));
        });
    }

    #[test]
    fn a_zero_sized_terminal_does_not_crash_the_screen_model() {
        let sup = Supervisor::new();
        sup.on_resize(0, 0);
        sup.on_output(b"READY 1234\r\nARGV --session-id abc\r\n\xe2\x9d\xaf x\r\n");
        sup.on_resize(1, 1);
        sup.on_output(b"\xe4\xb8\x96\x1b[5;5Hmore\r\nlines\r\n");
        assert!(lock(&sup.parser).screen().size().0 >= MIN_ROWS);
    }

    #[test]
    fn new_parser_clamps_a_tiny_or_unknown_size() {
        for (r, c) in [(0u16, 0u16), (1, 1), (2, 1), (1, 2)] {
            let mut p = new_parser(r, c);
            p.process(b"READY 1\r\n\xe2\x9d\xaf x\r\n");
            assert_eq!(p.screen().size(), (MIN_ROWS, MIN_COLS));
        }
    }
}
