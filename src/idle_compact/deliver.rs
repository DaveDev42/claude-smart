//! The typing protocol's pure state machine (design spec section 5): given
//! a sequence of [`Observation`]s, [`Delivery::step`] decides one [`Action`]
//! at a time. All time is injected as milliseconds (`now_ms` on
//! [`Observation`]) — nothing here reads a clock, a screen, a file or a
//! process, and nothing here writes anything; the supervisor (built
//! separately, over `vt100`/the pty relay) is the thin I/O shell that reads
//! a screen into an [`Observation`], calls `step`, and carries out the
//! returned [`Action`].
//!
//! ## Invariants
//!
//! - [`Action::Type`]/[`Action::PressEnter`] are only ever returned from a
//!   phase transition gated on the observation that licenses them (an
//!   `Empty`/`EmptyVimInsert` box for `Type`, a settled `box_text_is_compact`
//!   observation for `PressEnter`) — never on a stale or assumed state.
//! - [`Action::HoldInput`] and [`Action::ReleaseInput`] are emitted in
//!   matched pairs: every phase that can be reached only after `HoldInput`
//!   releases before reaching [`Action::Finish`], on every path (success,
//!   verify failure, or a deadline mid-flight).
//! - A `Draft` box notifies once and finishes without ever typing.
//! - `NotFound` and a transient session-status veto retry
//!   ([`Action::Wait`]) until the deadline, never typing either.
//! - After `/compact` is typed, [`Action::PressEnter`] is returned only
//!   when a post-settle observation's `box_text_is_compact` is `true`;
//!   otherwise the exact bytes csm typed are rolled back (one DEL per
//!   character, plus Esc when csm itself entered insert mode) and a
//!   [`Action::Notify`] explains it.
//! - A deadline crossed at any point finishes safely: nothing typed yet
//!   finishes with no rollback; something typed (or `i` sent for a vim
//!   NORMAL box) rolls back exactly what was sent.
//! - [`Mode::DryRun`] never returns `Type`, `PressEnter`, `Rollback` or
//!   `Notify` — it stops at the same gate `Mode::On` would start typing
//!   from and finishes with a `dry-run-*` [`Outcome`] instead.
//!
//! Every branch below has a direct unit test; [`tests::property_invariants_hold`]
//! drives many pseudo-random [`Observation`] sequences through both modes
//! and asserts the invariants above hold structurally, not just on the
//! hand-picked cases.

#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::VecDeque;

/// No real keystroke may have landed in the last this many ms before typing
/// starts (design spec step 3).
pub const KEYSTROKE_QUIET_MS: u64 = 60_000;
/// No output may have landed in the last this many ms before typing starts,
/// and is also the quiet gap [`Delivery`] waits for after typing before
/// checking `box_text_is_compact` (design spec steps 3 and 5).
pub const OUTPUT_QUIET_MS: u64 = 2_000;
/// Hard cap on the post-type settle wait, even if output never goes quiet.
pub const SETTLE_TIMEOUT_MS: u64 = 3_000;
/// How long to wait for `compaction_started` (or a `busy` status) after
/// Enter before finishing `sent-unconfirmed` instead of `delivered`.
pub const CONFIRM_TIMEOUT_MS: u64 = 10_000;
/// The poll interval [`Action::Wait`] asks for while retrying a gate.
pub const RETRY_MS: u64 = 500;

/// `/compact`, the one constant string this module ever types. The request
/// never carries text of its own (design spec's "Request hand-off"
/// section) — this is it.
pub const COMPACT_TEXT: &[u8] = b"/compact";

/// Whether this delivery may actually type ([`Mode::On`]) or only observe
/// and report what it would have done ([`Mode::DryRun`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    On,
    DryRun,
}

/// The main input box's classification, from the screen check (built
/// separately, over `vt100::Screen` — see the design spec's "Screen check"
/// section). `Empty`/`EmptyVimNormal`/`EmptyVimInsert` are the only states
/// [`Delivery`] will type into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxObs {
    /// Empty, vim off (or not in the picture at all).
    Empty,
    /// Empty, vim NORMAL: `i` must be sent and confirmed before typing.
    EmptyVimNormal,
    /// Empty, vim INSERT: ready to type directly.
    EmptyVimInsert,
    /// Non-blank, non-dim content already in the box.
    Draft,
    /// No input box found on screen at all (a menu, a dialog, ...).
    NotFound,
}

/// One screen/clock/status reading, injected by the supervisor's I/O shell.
/// Every field is exactly what [`Delivery::step`] needs for that one
/// decision — nothing here is read from anywhere but this struct.
#[derive(Debug, Clone)]
pub struct Observation {
    /// The current time, milliseconds, on whatever clock the caller's
    /// `deadline_ms`/`last_keystroke_ms`/`last_output_ms` also use.
    pub now_ms: u64,
    pub box_obs: BoxObs,
    /// Only meaningful once `/compact` has been typed and output has
    /// settled: whether the box's non-dim text is exactly `/compact`.
    pub box_text_is_compact: bool,
    /// Whether the screen shows compaction has started (design spec's
    /// `compaction_started`).
    pub compaction_started: bool,
    /// [`super::status::check`]'s veto reason, when the session status
    /// vetoes typing right now.
    pub status_veto: Option<String>,
    pub last_keystroke_ms: Option<u64>,
    pub last_output_ms: Option<u64>,
}

/// One step's instruction to the I/O shell. Exactly one per
/// [`Delivery::step`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Poll again in about `ms`, taking no other action meanwhile.
    Wait { ms: u64 },
    /// Close the input gate: buffer real keystrokes instead of forwarding
    /// them, until [`Action::ReleaseInput`].
    HoldInput,
    /// Write these bytes to the pty master.
    Type(Vec<u8>),
    /// Write a bare `\r`.
    PressEnter,
    /// Write these bytes to undo a `Type` that did not verify (DEL 0x7f
    /// per typed character, plus a trailing Esc 0x1b when csm itself
    /// entered insert mode for this attempt).
    Rollback(Vec<u8>),
    /// Reopen the input gate and flush whatever was buffered, in order.
    ReleaseInput,
    /// Write an OSC 777 notification. Pure strings, built here — see the
    /// module doc.
    Notify { title: String, body: String },
    /// This request is done; log `outcome.log_word()` and stop calling
    /// `step` again.
    Finish(Outcome),
}

/// How one delivery attempt ended. [`Outcome::log_word`] is the exact
/// `outcome=` value the log line uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    SentUnconfirmed,
    Draft,
    VerifyFailed,
    Expired,
    /// The deadline passed while a session-status veto was the reason
    /// nothing ever started — a more specific diagnostic than a bare
    /// `Expired` for the common "session got busy and stayed busy" case.
    Vetoed(String),
    DryRunWouldType,
    DryRunDraft,
    DryRunExpired,
}

impl Outcome {
    pub fn log_word(&self) -> String {
        match self {
            Outcome::Delivered => "delivered".to_owned(),
            Outcome::SentUnconfirmed => "sent-unconfirmed".to_owned(),
            Outcome::Draft => "draft".to_owned(),
            Outcome::VerifyFailed => "verify-failed".to_owned(),
            Outcome::Expired => "expired".to_owned(),
            Outcome::Vetoed(reason) => format!("vetoed-{reason}"),
            Outcome::DryRunWouldType => "dry-run-would-type".to_owned(),
            Outcome::DryRunDraft => "dry-run-draft".to_owned(),
            Outcome::DryRunExpired => "dry-run-expired".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
enum Phase {
    /// Steps 1-4: polling for deadline / status veto / keystroke+output
    /// quiet / box state, retrying until one of them lets step 5 begin (or
    /// the deadline or a `Draft` box ends the request first).
    Gating,
    /// Vim was NORMAL: `i` has been sent; the NEXT observation must show
    /// `EmptyVimInsert` before `/compact` is typed.
    ConfirmInsert,
    /// `/compact` has been sent; waiting for output to settle (at most
    /// [`SETTLE_TIMEOUT_MS`]) before checking `box_text_is_compact`.
    Settling {
        since_ms: u64,
    },
    /// A rollback was just sent (with its own first `Notify` already
    /// queued); the NEXT observation decides whether a second "needs a
    /// look" notify is warranted, based on whether the box is back to an
    /// empty-ish state.
    RollbackCheck {
        outcome: Outcome,
    },
    /// Enter was sent and input released; waiting up to
    /// [`CONFIRM_TIMEOUT_MS`] for `compaction_started` or a `busy` status.
    Confirm {
        since_ms: u64,
    },
    Done(Outcome),
}

/// The delivery state machine for one hand-off request. Construct with
/// [`Delivery::new`], then call [`Delivery::step`] once per fresh
/// [`Observation`] until it returns [`Action::Finish`].
#[derive(Debug, Clone)]
pub struct Delivery {
    mode: Mode,
    deadline_ms: u64,
    remaining_secs: i64,
    recache_tokens: i64,
    phase: Phase,
    entered_insert: bool,
    typed: Vec<u8>,
    last_veto: Option<String>,
    pending: VecDeque<Action>,
}

impl Delivery {
    /// `deadline_ms` is the request's deadline expressed on the same clock
    /// as `Observation::now_ms` (the supervisor converts the request's
    /// epoch-seconds `deadline` once at the start of this attempt).
    /// `remaining_secs`/`recache_tokens` are carried through only to build
    /// notification bodies.
    pub fn new(mode: Mode, deadline_ms: u64, remaining_secs: i64, recache_tokens: i64) -> Self {
        Self {
            mode,
            deadline_ms,
            remaining_secs,
            recache_tokens,
            phase: Phase::Gating,
            entered_insert: false,
            typed: Vec::new(),
            last_veto: None,
            pending: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done(_))
    }

    fn is_dry_run(&self) -> bool {
        matches!(self.mode, Mode::DryRun)
    }

    /// Decide and return exactly one [`Action`] for `obs`. Once
    /// [`Action::Finish`] has been returned, every further call returns the
    /// same `Finish` again (idempotent, so a caller that calls once more
    /// after seeing it does not panic).
    pub fn step(&mut self, obs: &Observation) -> Action {
        if let Some(action) = self.pending.pop_front() {
            return action;
        }
        if let Phase::Done(outcome) = &self.phase {
            return Action::Finish(outcome.clone());
        }
        match self.phase.clone() {
            Phase::Gating => self.advance_gating(obs),
            Phase::ConfirmInsert => self.advance_confirm_insert(obs),
            Phase::Settling { since_ms } => self.advance_settling(obs, since_ms),
            Phase::RollbackCheck { outcome } => self.advance_rollback_check(obs, outcome),
            Phase::Confirm { since_ms } => self.advance_confirm(obs, since_ms),
            Phase::Done(_) => unreachable!("handled above"),
        }
    }

    /// Queue every action in `actions` and return the first. Panics only if
    /// called with an empty list, which would be a bug in this module, not
    /// something a caller can trigger.
    fn emit(&mut self, actions: Vec<Action>) -> Action {
        self.pending.extend(actions);
        self.pending
            .pop_front()
            .expect("emit called with a non-empty action list")
    }

    fn finish(&mut self, outcome: Outcome) -> Action {
        self.phase = Phase::Done(outcome.clone());
        Action::Finish(outcome)
    }

    fn quiet(&self, obs: &Observation) -> bool {
        let keystroke_quiet = obs
            .last_keystroke_ms
            .is_none_or(|t| obs.now_ms.saturating_sub(t) >= KEYSTROKE_QUIET_MS);
        let output_quiet = obs
            .last_output_ms
            .is_none_or(|t| obs.now_ms.saturating_sub(t) >= OUTPUT_QUIET_MS);
        keystroke_quiet && output_quiet
    }

    fn advance_gating(&mut self, obs: &Observation) -> Action {
        if let Some(reason) = &obs.status_veto {
            self.last_veto = Some(reason.clone());
        } else {
            self.last_veto = None;
        }

        if obs.now_ms > self.deadline_ms {
            let outcome = if self.is_dry_run() {
                Outcome::DryRunExpired
            } else if let Some(reason) = self.last_veto.clone() {
                Outcome::Vetoed(reason)
            } else {
                Outcome::Expired
            };
            return self.finish(outcome);
        }
        if obs.status_veto.is_some() {
            return Action::Wait { ms: RETRY_MS };
        }
        if !self.quiet(obs) {
            return Action::Wait { ms: RETRY_MS };
        }
        match obs.box_obs {
            BoxObs::NotFound => Action::Wait { ms: RETRY_MS },
            BoxObs::Draft => {
                if self.is_dry_run() {
                    self.finish(Outcome::DryRunDraft)
                } else {
                    self.phase = Phase::Done(Outcome::Draft);
                    let (title, body) = self.notify_strings("draft");
                    self.emit(vec![
                        Action::Notify { title, body },
                        Action::Finish(Outcome::Draft),
                    ])
                }
            }
            BoxObs::Empty | BoxObs::EmptyVimInsert => {
                if self.is_dry_run() {
                    return self.finish(Outcome::DryRunWouldType);
                }
                self.typed = COMPACT_TEXT.to_vec();
                self.phase = Phase::Settling {
                    since_ms: obs.now_ms,
                };
                self.emit(vec![Action::HoldInput, Action::Type(self.typed.clone())])
            }
            BoxObs::EmptyVimNormal => {
                if self.is_dry_run() {
                    return self.finish(Outcome::DryRunWouldType);
                }
                self.entered_insert = true;
                self.phase = Phase::ConfirmInsert;
                self.emit(vec![Action::HoldInput, Action::Type(vec![b'i'])])
            }
        }
    }

    fn advance_confirm_insert(&mut self, obs: &Observation) -> Action {
        if obs.now_ms > self.deadline_ms {
            return self.begin_rollback(Outcome::Expired);
        }
        match obs.box_obs {
            BoxObs::EmptyVimInsert => {
                self.typed = COMPACT_TEXT.to_vec();
                self.phase = Phase::Settling {
                    since_ms: obs.now_ms,
                };
                self.emit(vec![Action::Type(self.typed.clone())])
            }
            // Insert was never confirmed (still Normal, or the screen
            // changed to something else entirely): safe rollback, Esc
            // only, since `typed` is still empty here.
            _ => self.begin_rollback(Outcome::VerifyFailed),
        }
    }

    fn advance_settling(&mut self, obs: &Observation, since_ms: u64) -> Action {
        if obs.now_ms > self.deadline_ms {
            return self.begin_rollback(Outcome::Expired);
        }
        let quiet_since = obs.last_output_ms.unwrap_or(since_ms);
        let settled = obs.now_ms.saturating_sub(quiet_since) >= OUTPUT_QUIET_MS
            || obs.now_ms.saturating_sub(since_ms) >= SETTLE_TIMEOUT_MS;
        if !settled {
            self.phase = Phase::Settling { since_ms };
            return Action::Wait { ms: RETRY_MS };
        }
        if obs.box_text_is_compact {
            self.phase = Phase::Confirm {
                since_ms: obs.now_ms,
            };
            self.emit(vec![Action::PressEnter, Action::ReleaseInput])
        } else {
            self.begin_rollback(Outcome::VerifyFailed)
        }
    }

    /// Queue the rollback bytes (possibly empty — see [`Self::rollback_bytes`])
    /// plus the first notify, and move to [`Phase::RollbackCheck`] to
    /// decide on the next observation whether a second notify is needed.
    fn begin_rollback(&mut self, outcome: Outcome) -> Action {
        let bytes = self.rollback_bytes();
        self.phase = Phase::RollbackCheck {
            outcome: outcome.clone(),
        };
        let (title, body) = self.notify_strings(match outcome {
            Outcome::Expired => "expired",
            _ => "verify-failed",
        });
        let mut actions = Vec::new();
        if !bytes.is_empty() {
            actions.push(Action::Rollback(bytes));
        }
        actions.push(Action::Notify { title, body });
        self.emit(actions)
    }

    fn advance_rollback_check(&mut self, obs: &Observation, outcome: Outcome) -> Action {
        let restored = matches!(
            obs.box_obs,
            BoxObs::Empty | BoxObs::EmptyVimNormal | BoxObs::EmptyVimInsert
        );
        self.phase = Phase::Done(outcome.clone());
        let mut actions = Vec::new();
        if !restored {
            let (title, body) = self.notify_strings("needs-a-look");
            actions.push(Action::Notify { title, body });
        }
        actions.push(Action::ReleaseInput);
        actions.push(Action::Finish(outcome));
        self.emit(actions)
    }

    fn advance_confirm(&mut self, obs: &Observation, since_ms: u64) -> Action {
        let confirmed = obs.compaction_started || obs.status_veto.as_deref() == Some("busy");
        if confirmed {
            return self.finish(Outcome::Delivered);
        }
        if obs.now_ms.saturating_sub(since_ms) >= CONFIRM_TIMEOUT_MS {
            return self.finish(Outcome::SentUnconfirmed);
        }
        self.phase = Phase::Confirm { since_ms };
        Action::Wait { ms: RETRY_MS }
    }

    /// DEL (0x7f) once per typed byte, plus a trailing Esc (0x1b) when csm
    /// itself entered insert mode for this attempt (`i` sent, whether or
    /// not `/compact` ever followed it).
    fn rollback_bytes(&self) -> Vec<u8> {
        let mut v = vec![0x7fu8; self.typed.len()];
        if self.entered_insert {
            v.push(0x1b);
        }
        v
    }

    /// Pure notification strings. `reason` picks the body; every body
    /// includes the remaining seconds and the recache token estimate, and
    /// none of them ever include a hostname, path or account name.
    fn notify_strings(&self, reason: &str) -> (String, String) {
        let tail = format!(
            "cache expires in {}s, next request would re-write ~{} tokens",
            self.remaining_secs, self.recache_tokens
        );
        let body = match reason {
            "draft" => format!("idle-compact: input box has a draft, skipped ({tail})"),
            "expired" => format!("idle-compact: ran out of time, rolled back ({tail})"),
            "needs-a-look" => {
                "idle-compact: the input box may still need a look after a rollback".to_owned()
            }
            _ => format!("idle-compact: could not confirm /compact, rolled back ({tail})"),
        };
        ("idle-compact".to_owned(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(now_ms: u64, box_obs: BoxObs) -> Observation {
        Observation {
            now_ms,
            box_obs,
            box_text_is_compact: false,
            compaction_started: false,
            status_veto: None,
            last_keystroke_ms: None,
            last_output_ms: None,
        }
    }

    fn delivery(mode: Mode, deadline_ms: u64) -> Delivery {
        Delivery::new(mode, deadline_ms, 120, 150_000)
    }

    // ── Gating ─────────────────────────────────────────────────────────────

    #[test]
    fn deadline_passed_before_anything_typed_finishes_expired() {
        let mut d = delivery(Mode::On, 1_000);
        assert_eq!(
            d.step(&obs(1_001, BoxObs::NotFound)),
            Action::Finish(Outcome::Expired)
        );
        assert!(d.is_done());
    }

    #[test]
    fn deadline_passed_while_vetoed_finishes_vetoed_with_reason() {
        let mut d = delivery(Mode::On, 1_000);
        let mut o = obs(500, BoxObs::NotFound);
        o.status_veto = Some("busy".to_owned());
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
        let mut o2 = obs(1_001, BoxObs::NotFound);
        o2.status_veto = Some("busy".to_owned());
        assert_eq!(
            d.step(&o2),
            Action::Finish(Outcome::Vetoed("busy".to_owned()))
        );
    }

    #[test]
    fn deadline_passed_in_dry_run_finishes_dry_run_expired() {
        let mut d = delivery(Mode::DryRun, 1_000);
        assert_eq!(
            d.step(&obs(1_001, BoxObs::NotFound)),
            Action::Finish(Outcome::DryRunExpired)
        );
    }

    #[test]
    fn status_veto_retries() {
        let mut d = delivery(Mode::On, 100_000);
        let mut o = obs(0, BoxObs::Empty);
        o.status_veto = Some("busy".to_owned());
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
        assert!(!d.is_done());
    }

    #[test]
    fn recent_keystroke_retries() {
        let mut d = delivery(Mode::On, 100_000);
        let mut o = obs(1_000, BoxObs::Empty);
        o.last_keystroke_ms = Some(999);
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
    }

    #[test]
    fn recent_output_retries() {
        let mut d = delivery(Mode::On, 100_000);
        let mut o = obs(1_000, BoxObs::Empty);
        o.last_output_ms = Some(999);
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
    }

    #[test]
    fn not_found_retries() {
        let mut d = delivery(Mode::On, 100_000);
        assert_eq!(
            d.step(&obs(0, BoxObs::NotFound)),
            Action::Wait { ms: RETRY_MS }
        );
    }

    #[test]
    fn draft_notifies_once_and_finishes_draft_on_mode_on() {
        let mut d = delivery(Mode::On, 100_000);
        assert_eq!(
            d.step(&obs(0, BoxObs::Draft)),
            Action::Notify {
                title: "idle-compact".to_owned(),
                body: "idle-compact: input box has a draft, skipped (cache expires in 120s, next request would re-write ~150000 tokens)".to_owned(),
            }
        );
        assert_eq!(
            d.step(&obs(0, BoxObs::Draft)),
            Action::Finish(Outcome::Draft)
        );
        assert!(d.is_done());
    }

    #[test]
    fn draft_in_dry_run_never_notifies() {
        let mut d = delivery(Mode::DryRun, 100_000);
        assert_eq!(
            d.step(&obs(0, BoxObs::Draft)),
            Action::Finish(Outcome::DryRunDraft)
        );
    }

    #[test]
    fn empty_box_holds_input_and_types_compact_directly() {
        let mut d = delivery(Mode::On, 100_000);
        assert_eq!(d.step(&obs(0, BoxObs::Empty)), Action::HoldInput);
        assert_eq!(
            d.step(&obs(0, BoxObs::Empty)),
            Action::Type(COMPACT_TEXT.to_vec())
        );
    }

    #[test]
    fn empty_vim_insert_types_compact_directly_too() {
        let mut d = delivery(Mode::On, 100_000);
        assert_eq!(d.step(&obs(0, BoxObs::EmptyVimInsert)), Action::HoldInput);
        assert_eq!(
            d.step(&obs(0, BoxObs::EmptyVimInsert)),
            Action::Type(COMPACT_TEXT.to_vec())
        );
    }

    #[test]
    fn empty_vim_normal_sends_i_first() {
        let mut d = delivery(Mode::On, 100_000);
        assert_eq!(d.step(&obs(0, BoxObs::EmptyVimNormal)), Action::HoldInput);
        assert_eq!(
            d.step(&obs(0, BoxObs::EmptyVimNormal)),
            Action::Type(vec![b'i'])
        );
    }

    #[test]
    fn empty_box_in_dry_run_never_holds_or_types() {
        let mut d = delivery(Mode::DryRun, 100_000);
        assert_eq!(
            d.step(&obs(0, BoxObs::Empty)),
            Action::Finish(Outcome::DryRunWouldType)
        );
        let mut d2 = delivery(Mode::DryRun, 100_000);
        assert_eq!(
            d2.step(&obs(0, BoxObs::EmptyVimNormal)),
            Action::Finish(Outcome::DryRunWouldType)
        );
    }

    // ── ConfirmInsert ──────────────────────────────────────────────────────

    #[test]
    fn confirm_insert_success_types_compact_then_settles() {
        let mut d = delivery(Mode::On, 100_000);
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        assert_eq!(
            d.step(&obs(10, BoxObs::EmptyVimInsert)),
            Action::Type(COMPACT_TEXT.to_vec())
        );
    }

    #[test]
    fn confirm_insert_failure_rolls_back_esc_only() {
        let mut d = delivery(Mode::On, 100_000);
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        // Insert never confirmed: still Normal.
        assert_eq!(
            d.step(&obs(10, BoxObs::EmptyVimNormal)),
            Action::Rollback(vec![0x1b])
        );
        assert!(matches!(
            d.step(&obs(10, BoxObs::EmptyVimNormal)),
            Action::Notify { .. }
        ));
    }

    #[test]
    fn confirm_insert_deadline_rolls_back_and_expires() {
        let mut d = delivery(Mode::On, 5);
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        let _ = d.step(&obs(0, BoxObs::EmptyVimNormal));
        assert_eq!(
            d.step(&obs(6, BoxObs::EmptyVimNormal)),
            Action::Rollback(vec![0x1b])
        );
        let _ = d.step(&obs(6, BoxObs::EmptyVimNormal)); // notify
        let _ = d.step(&obs(6, BoxObs::Empty)); // rollback-check observation, restored
        assert_eq!(
            d.step(&obs(6, BoxObs::Empty)),
            Action::Finish(Outcome::Expired)
        );
    }

    // ── Settling / verify ──────────────────────────────────────────────────

    fn typed_delivery() -> (Delivery, u64) {
        let mut d = delivery(Mode::On, 100_000);
        let _ = d.step(&obs(0, BoxObs::Empty));
        let _ = d.step(&obs(0, BoxObs::Empty));
        (d, 0)
    }

    #[test]
    fn settling_waits_until_settled() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + 100, BoxObs::Empty);
        o.last_output_ms = Some(since + 50); // output 50ms ago: not quiet yet
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
    }

    #[test]
    fn settling_confirms_and_presses_enter() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + OUTPUT_QUIET_MS + 10, BoxObs::Empty);
        o.box_text_is_compact = true;
        assert_eq!(d.step(&o), Action::PressEnter);
        assert_eq!(d.step(&o), Action::ReleaseInput);
    }

    #[test]
    fn settling_times_out_at_settle_cap_even_with_recent_output() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + SETTLE_TIMEOUT_MS, BoxObs::Empty);
        o.last_output_ms = Some(since + SETTLE_TIMEOUT_MS - 1); // still "recent"
        o.box_text_is_compact = true;
        assert_eq!(d.step(&o), Action::PressEnter);
    }

    #[test]
    fn settling_verify_failed_rolls_back_8_dels() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + OUTPUT_QUIET_MS + 10, BoxObs::Empty);
        o.box_text_is_compact = false;
        assert_eq!(d.step(&o), Action::Rollback(vec![0x7f; 8]));
        assert!(matches!(d.step(&o), Action::Notify { .. }));
    }

    #[test]
    fn settling_deadline_mid_flight_rolls_back_and_expires() {
        let mut d = delivery(Mode::On, 10);
        let _ = d.step(&obs(0, BoxObs::Empty));
        let _ = d.step(&obs(0, BoxObs::Empty));
        assert_eq!(
            d.step(&obs(11, BoxObs::Empty)),
            Action::Rollback(vec![0x7f; 8])
        );
    }

    // ── RollbackCheck ──────────────────────────────────────────────────────

    #[test]
    fn rollback_check_restored_skips_second_notify() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + OUTPUT_QUIET_MS + 10, BoxObs::Empty);
        o.box_text_is_compact = false;
        let _ = d.step(&o); // rollback
        let _ = d.step(&o); // first notify
        let restored = obs(since + OUTPUT_QUIET_MS + 20, BoxObs::Empty);
        assert_eq!(d.step(&restored), Action::ReleaseInput);
        assert_eq!(d.step(&restored), Action::Finish(Outcome::VerifyFailed));
    }

    #[test]
    fn rollback_check_not_restored_sends_second_notify() {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + OUTPUT_QUIET_MS + 10, BoxObs::Empty);
        o.box_text_is_compact = false;
        let _ = d.step(&o);
        let _ = d.step(&o);
        let not_restored = obs(since + OUTPUT_QUIET_MS + 20, BoxObs::Draft);
        assert!(matches!(d.step(&not_restored), Action::Notify { .. }));
        assert_eq!(d.step(&not_restored), Action::ReleaseInput);
        assert_eq!(d.step(&not_restored), Action::Finish(Outcome::VerifyFailed));
    }

    // ── Confirm ────────────────────────────────────────────────────────────

    fn enter_pressed_delivery() -> (Delivery, u64) {
        let (mut d, since) = typed_delivery();
        let mut o = obs(since + OUTPUT_QUIET_MS + 10, BoxObs::Empty);
        o.box_text_is_compact = true;
        let _ = d.step(&o); // PressEnter
        let _ = d.step(&o); // ReleaseInput
        (d, since + OUTPUT_QUIET_MS + 10)
    }

    #[test]
    fn confirm_compaction_started_delivers() {
        let (mut d, since) = enter_pressed_delivery();
        let mut o = obs(since + 10, BoxObs::Empty);
        o.compaction_started = true;
        assert_eq!(d.step(&o), Action::Finish(Outcome::Delivered));
    }

    #[test]
    fn confirm_busy_status_delivers() {
        let (mut d, since) = enter_pressed_delivery();
        let mut o = obs(since + 10, BoxObs::Empty);
        o.status_veto = Some("busy".to_owned());
        assert_eq!(d.step(&o), Action::Finish(Outcome::Delivered));
    }

    #[test]
    fn confirm_times_out_sent_unconfirmed() {
        let (mut d, since) = enter_pressed_delivery();
        let o = obs(since + CONFIRM_TIMEOUT_MS, BoxObs::Empty);
        assert_eq!(d.step(&o), Action::Finish(Outcome::SentUnconfirmed));
    }

    #[test]
    fn confirm_waits_before_timeout() {
        let (mut d, since) = enter_pressed_delivery();
        let o = obs(since + CONFIRM_TIMEOUT_MS - 1, BoxObs::Empty);
        assert_eq!(d.step(&o), Action::Wait { ms: RETRY_MS });
    }

    // ── Outcome::log_word ──────────────────────────────────────────────────

    #[test]
    fn log_word_covers_every_variant() {
        assert_eq!(Outcome::Delivered.log_word(), "delivered");
        assert_eq!(Outcome::SentUnconfirmed.log_word(), "sent-unconfirmed");
        assert_eq!(Outcome::Draft.log_word(), "draft");
        assert_eq!(Outcome::VerifyFailed.log_word(), "verify-failed");
        assert_eq!(Outcome::Expired.log_word(), "expired");
        assert_eq!(Outcome::Vetoed("busy".to_owned()).log_word(), "vetoed-busy");
        assert_eq!(Outcome::DryRunWouldType.log_word(), "dry-run-would-type");
        assert_eq!(Outcome::DryRunDraft.log_word(), "dry-run-draft");
        assert_eq!(Outcome::DryRunExpired.log_word(), "dry-run-expired");
    }

    #[test]
    fn finish_is_idempotent() {
        let mut d = delivery(Mode::On, 1_000);
        assert_eq!(
            d.step(&obs(1_001, BoxObs::NotFound)),
            Action::Finish(Outcome::Expired)
        );
        assert_eq!(
            d.step(&obs(2_000, BoxObs::NotFound)),
            Action::Finish(Outcome::Expired)
        );
    }

    // ── property-style: many pseudo-random observation sequences ───────────
    //
    // No external `rand` crate in this workspace's dev-dependencies — a
    // small xorshift PRNG stands in, seeded per-run so a failure is
    // reproducible from the printed seed.

    struct Xorshift(u64);
    impl Xorshift {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn choose<T: Copy>(&mut self, options: &[T]) -> T {
            options[(self.next_u64() as usize) % options.len()]
        }
        fn bool(&mut self) -> bool {
            self.next_u64().is_multiple_of(2)
        }
    }

    fn random_obs(rng: &mut Xorshift, now_ms: u64) -> Observation {
        let box_obs = rng.choose(&[
            BoxObs::Empty,
            BoxObs::EmptyVimNormal,
            BoxObs::EmptyVimInsert,
            BoxObs::Draft,
            BoxObs::NotFound,
        ]);
        Observation {
            now_ms,
            box_obs,
            box_text_is_compact: rng.bool(),
            compaction_started: rng.bool(),
            status_veto: if rng.bool() {
                None
            } else {
                Some(rng.choose(&["busy", "waiting", "screen-busy"]).to_owned())
            },
            last_keystroke_ms: if rng.bool() {
                None
            } else {
                Some(now_ms.saturating_sub(rng.next_u64() % 120_000))
            },
            last_output_ms: if rng.bool() {
                None
            } else {
                Some(now_ms.saturating_sub(rng.next_u64() % 5_000))
            },
        }
    }

    #[test]
    fn property_invariants_hold() {
        for seed in 1u64..=200 {
            for mode in [Mode::On, Mode::DryRun] {
                let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                let deadline_ms = 20_000 + rng.next_u64() % 20_000;
                let mut d = Delivery::new(mode, deadline_ms, 120, 150_000);
                let mut now_ms = 0u64;
                let mut held = false;
                let mut hold_count = 0u32;
                let mut release_count = 0u32;
                let mut typed_anything = false;
                let mut finished = None;

                // Bounded iterations: the machine must terminate on its own
                // well before this, since `now_ms` always advances and the
                // deadline (plus the fixed settle/confirm caps) bounds every
                // phase.
                for _ in 0..2_000 {
                    let o = random_obs(&mut rng, now_ms);
                    let action = d.step(&o);
                    match &action {
                        Action::HoldInput => {
                            assert!(!held, "seed={seed}: HoldInput while already held");
                            held = true;
                            hold_count += 1;
                        }
                        Action::ReleaseInput => {
                            assert!(held, "seed={seed}: ReleaseInput while not held");
                            held = false;
                            release_count += 1;
                        }
                        Action::Type(bytes) => {
                            assert!(held, "seed={seed}: Type while input not held");
                            assert!(
                                !finished_dry_run(mode),
                                "seed={seed}: dry-run must never Type"
                            );
                            if bytes.as_slice() == COMPACT_TEXT {
                                typed_anything = true;
                            }
                        }
                        Action::PressEnter => {
                            assert!(
                                !finished_dry_run(mode),
                                "seed={seed}: dry-run must never PressEnter"
                            );
                            assert!(typed_anything, "seed={seed}: PressEnter before Type");
                        }
                        Action::Rollback(_) => {
                            assert!(
                                !finished_dry_run(mode),
                                "seed={seed}: dry-run must never Rollback"
                            );
                        }
                        Action::Notify { .. } => {
                            assert!(
                                !finished_dry_run(mode),
                                "seed={seed}: dry-run must never Notify"
                            );
                        }
                        Action::Finish(outcome) => {
                            assert!(
                                !held,
                                "seed={seed}: Finish returned while input still held (no matching ReleaseInput)"
                            );
                            finished = Some(outcome.clone());
                        }
                        Action::Wait { .. } => {}
                    }
                    // The check above is keyed on the actually-returned
                    // `action`, not `d.is_done()`: `self.phase` can flip to
                    // `Phase::Done` internally before every queued action
                    // (e.g. a rollback's `Notify`/`ReleaseInput` ahead of
                    // `Finish`) has been drained one `step()` call at a
                    // time, so `is_done()` can go true a call or two before
                    // `Action::Finish` itself is returned. The real
                    // contract callers rely on is the returned action
                    // sequence, so that is what this loop's termination and
                    // this assertion both key on.
                    if matches!(action, Action::Finish(_)) {
                        break;
                    }
                    now_ms += 250;
                }

                assert!(
                    d.is_done(),
                    "seed={seed} mode={mode:?}: did not terminate within the iteration cap"
                );
                assert!(finished.is_some(), "seed={seed}: never reached Finish");
                assert_eq!(
                    hold_count, release_count,
                    "seed={seed}: HoldInput/ReleaseInput must balance"
                );
                if finished_dry_run(mode) {
                    // Already asserted per-action above; restated for clarity.
                    assert_eq!(hold_count, 0, "seed={seed}: dry-run must never hold input");
                }
            }
        }
    }

    fn finished_dry_run(mode: Mode) -> bool {
        matches!(mode, Mode::DryRun)
    }
}
