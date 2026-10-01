//! The statusline tick: gates, then hands off to csm's own pty-relay
//! supervisor instead of driving any external terminal tool.
//!
//! ## Decision
//!
//! [`decide`] is the pure core (Invariant 4): given the current time and a
//! [`DecisionInputs`] bundle the I/O shell ([`run_from_raw`]) gathers, it
//! returns a [`Decision`] with no side effects of its own. [`run_from_raw`]
//! gathers those inputs, calls [`decide`], and for every outcome other than
//! `Skip` claims the idle period ([`paths::idle_compacted`]) and writes one
//! log line — `Skip` is silent and leaves no trace, since most statusline
//! ticks land there (mode off, cache not yet low, still busy) and logging
//! every one of them would flood `idle-compact.log`.
//!
//! ## Busy check
//!
//! [`busy_state`] gates on the transcript's mtime first: at or before the
//! `<sid>.idle` stamp, the session is idle and nothing is read. A later
//! mtime does not mean busy by itself — Claude Code keeps appending
//! metadata-only rows well after a `Stop` (`turn_duration`, `away_summary`,
//! `last-prompt`, `mode`, `ai-title`, and more), which used to bump the
//! mtime forever and make every idle session look permanently busy. So a
//! later mtime instead triggers a bounded tail read
//! ([`read_transcript_tail`], at most [`TRANSCRIPT_TAIL_MAX_BYTES`]), and
//! the pure [`classify_tail`] walks it backwards looking for the last
//! `user`/`assistant` row: only that row's own `timestamp` decides busy or
//! not, every other row type is skipped as metadata.
//!
//! An interrupted turn — the newest non-sidechain `user`/`assistant` row in
//! the tail is a `user` row whose text starts with `[Request interrupted by
//! user` — counts as a turn end at that row's own timestamp even with no
//! `Stop` hook: [`interrupted_turn_end`] finds it, and [`classify_tail`]
//! compares against whichever of the `<sid>.idle` stamp and that timestamp
//! is later.
//!
//! ## Hand-off
//!
//! A supervisor is present when [`super::SUPERVISOR_PID_ENV`] holds a
//! number and [`crate::platform::proc::is_running`] confirms that pid is
//! alive ([`supervisor_pid_from_env`]). Present: [`super::request::write_request`]
//! writes the hand-off file and the outcome logged is `handed-off`. Absent:
//! nothing is written and the outcome is `no-delivery-path`. Either way the
//! idle period is claimed first, so a statusline tick that runs again a
//! second later (as it does, repeatedly, while the session stays idle)
//! short-circuits on `already_fired` instead of re-deciding or re-logging.
//!
//! ## Conservative choices where the design left a gap
//!
//! - A missing `<sid>.idle` marker (this session has never been seen to
//!   stop) and an unreadable transcript both count as "cannot confirm the
//!   turn ended", not as "must be idle" — both fall through to `Skip`
//!   rather than handing off.
//! - A cache already past `expires_at` (`remaining_secs <= 0`) never hands
//!   off: the point is to act *before* the cache goes cold, so acting after
//!   is just a wasted `/compact` against a cold cache.
//! - The busy check's cheap path uses `mtime > idle_marker_epoch` (strictly
//!   greater), not `>=`: the Stop hook's own write and the transcript's
//!   last line often land in the same wall-clock second, and `>=` would
//!   call that "busy" on every single Stop.
//! - In the tail, a `user`/`assistant` row with no parseable `timestamp`
//!   classifies as busy rather than not-busy — the conservative guess when
//!   the one signal that would decide it is missing.
//! - A tail with no `user`/`assistant` row at all cannot be classified
//!   either way, so it is its own `Skip` reason
//!   (`transcript-tail-inconclusive`) rather than folded into
//!   `session-busy` or treated as idle.

use std::path::Path;

use crate::config::IdleCompactMode;
use crate::paths;
use crate::usage::local::statusline::StatuslinePayload;

use super::request::{self, Request};
use super::{LogFields, SUPERVISOR_PID_ENV, log_outcome};

// ─── decision ───────────────────────────────────────────────────────────────

/// Fire window: act only when the cache has this many seconds or fewer left.
pub const FIRE_WINDOW_SECS: i64 = 300;

/// Minimum `recache_tokens_if_cold` worth compacting for.
pub const MIN_RECACHE_TOKENS: i64 = 100_000;

/// How many seconds before `expires_at` a hand-off request's deadline sits,
/// so the supervisor stops retrying with enough margin to never cross the
/// cache's real expiry while still mid-attempt.
pub const DEADLINE_MARGIN_SECS: i64 = 20;

/// Every observation [`decide`] needs. The I/O shell ([`run_from_raw`])
/// gathers these and passes them in as plain data (Invariant 4); `now` is a
/// separate argument to [`decide`] rather than a field here, so a test can
/// vary it independently of everything else.
#[derive(Debug, Clone)]
pub struct DecisionInputs {
    pub mode: IdleCompactMode,
    /// `<sid>.idle-compacted` already exists: this idle period was already
    /// evaluated past the "would hand off" threshold once.
    pub already_fired: bool,
    pub cache_warm: Option<bool>,
    /// `prompt_cache.expires_at`, unix epoch seconds.
    pub expires_at: Option<i64>,
    pub recache_tokens_if_cold: Option<i64>,
    /// `<sid>.idle`'s stamp: the epoch `csm hook` last saw a Stop for this
    /// session.
    pub idle_marker_epoch: Option<i64>,
    /// Whether the session is still busy, gathered by [`busy_state`] (see
    /// the module doc's "Busy check" section). `None` only when the
    /// transcript could not be read at all — the same "cannot confirm"
    /// treatment as a missing idle marker.
    pub busy: Option<BusyState>,
    /// A live supervisor to hand a request to
    /// ([`supervisor_pid_from_env`]). `None` means no hand-off path exists
    /// right now.
    pub supervisor_pid: Option<u32>,
}

/// [`busy_state`]'s classification of a session's transcript tail against
/// the `<sid>.idle` stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyState {
    /// A `user`/`assistant` row landed after the stamp (or the last such
    /// row in the tail had no usable timestamp — the conservative guess).
    Busy,
    /// The last `user`/`assistant` row in the tail is at or before the
    /// stamp: whatever wrote after it was metadata, not a new turn.
    NotBusy,
    /// The tail (bounded by [`TRANSCRIPT_TAIL_MAX_BYTES`]) holds no
    /// `user`/`assistant` row at all, so busy-or-not cannot be decided.
    Inconclusive,
}

/// One evaluation's outcome. `Skip` carries a `reason` for tests/debugging
/// but is never logged (see the module doc: most ticks land here, and
/// logging every one would flood `idle-compact.log`). The other two
/// variants are each logged once per idle period by [`run_from_raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip {
        reason: &'static str,
    },
    HandOff {
        supervisor_pid: u32,
        mode: IdleCompactMode,
        remaining_secs: i64,
        recache_tokens: i64,
    },
    NoDeliveryPath {
        remaining_secs: i64,
        recache_tokens: i64,
    },
}

/// The pure decision core (Invariant 4). See the module doc for the
/// conservative choices at the gaps the design left open.
pub fn decide(now: i64, inputs: &DecisionInputs) -> Decision {
    if inputs.mode == IdleCompactMode::Off {
        return Decision::Skip { reason: "mode-off" };
    }
    if inputs.already_fired {
        return Decision::Skip {
            reason: "already-fired-this-idle-period",
        };
    }
    if inputs.cache_warm != Some(true) {
        return Decision::Skip {
            reason: "cache-not-warm",
        };
    }
    let Some(expires_at) = inputs.expires_at else {
        return Decision::Skip {
            reason: "no-expiry-info",
        };
    };
    let remaining_secs = expires_at - now;
    if remaining_secs <= 0 {
        return Decision::Skip {
            reason: "cache-already-expired",
        };
    }
    if remaining_secs > FIRE_WINDOW_SECS {
        return Decision::Skip {
            reason: "remaining-too-long",
        };
    }
    let Some(recache_tokens) = inputs.recache_tokens_if_cold else {
        return Decision::Skip {
            reason: "no-recache-estimate",
        };
    };
    if recache_tokens < MIN_RECACHE_TOKENS {
        return Decision::Skip {
            reason: "recache-tokens-below-threshold",
        };
    }
    if inputs.idle_marker_epoch.is_none() {
        return Decision::Skip {
            reason: "no-idle-marker",
        };
    }
    let Some(busy) = inputs.busy else {
        return Decision::Skip {
            reason: "transcript-unreadable",
        };
    };
    match busy {
        BusyState::Busy => {
            return Decision::Skip {
                reason: "session-busy",
            };
        }
        BusyState::Inconclusive => {
            return Decision::Skip {
                reason: "transcript-tail-inconclusive",
            };
        }
        BusyState::NotBusy => {}
    }

    match inputs.supervisor_pid {
        Some(supervisor_pid) => Decision::HandOff {
            supervisor_pid,
            mode: inputs.mode,
            remaining_secs,
            recache_tokens,
        },
        None => Decision::NoDeliveryPath {
            remaining_secs,
            recache_tokens,
        },
    }
}

// ─── I/O shell ────────────────────────────────────────────────────────────────

/// Called from both `csm usage capture` ([`crate::cmd::usage`]) and
/// `csm statusline` ([`crate::statusline::run_with_capture`]) with the raw
/// statusLine JSON each already has on stdin. Never writes to stdout (both
/// callers rely on that) and never panics: a malformed payload, an absent
/// `session_id`, or an unreadable marker/transcript file all fall through
/// to a silent no-op.
///
/// Cheap-first: when the config read (unavoidable — the mode has to come
/// from somewhere) says the feature is off, this returns immediately without
/// touching any per-session file. Only once the mode is `dry-run`/`on` does
/// it read the idle/idle-compacted markers and stat the transcript.
pub fn run_from_raw(raw: &str) {
    let Ok(payload) = serde_json::from_str::<StatuslinePayload>(raw) else {
        return;
    };
    let Some(sid) = payload.session_id() else {
        return;
    };

    let mode = crate::config::Config::load()
        .unwrap_or_default()
        .idle_compact_mode();
    if mode == IdleCompactMode::Off {
        return;
    }

    let idle_marker_epoch = read_epoch_marker(&paths::idle(&sid));
    // The busy check needs the stamp to compare against, so it only runs
    // once both a transcript path and an idle marker are in hand; `decide`
    // skips on a missing idle marker before ever looking at `busy`, so
    // `None` here in that case is never inspected.
    let busy = match (payload.transcript_path(), idle_marker_epoch) {
        (Some(path), Some(stamp)) => busy_state(&path, stamp),
        _ => None,
    };

    let inputs = DecisionInputs {
        mode,
        already_fired: paths::idle_compacted(&sid).exists(),
        cache_warm: payload.prompt_cache_warm(),
        expires_at: payload.prompt_cache_expires_at(),
        recache_tokens_if_cold: payload.prompt_cache_recache_tokens_if_cold(),
        idle_marker_epoch,
        busy,
        supervisor_pid: supervisor_pid_from_env(),
    };
    let context_window = context_window_summary(payload.context_window.as_ref());
    let vim_mode = payload.vim_mode();

    handle_decision(
        &sid,
        now_epoch(),
        &context_window,
        vim_mode.as_deref(),
        decide(now_epoch(), &inputs),
    );
}

/// [`super::SUPERVISOR_PID_ENV`] holds a number, and
/// [`crate::platform::proc::is_running`] confirms that pid is alive right
/// now. Both conditions, checked fresh on every tick — a supervisor that
/// exited between ticks stops being a hand-off target immediately, with no
/// separate liveness cache to go stale.
fn supervisor_pid_from_env() -> Option<u32> {
    let raw = std::env::var(SUPERVISOR_PID_ENV).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    (pid != 0 && crate::platform::proc::is_running(pid)).then_some(pid)
}

/// Act on one [`Decision`]: claim the idle period and log, for every
/// variant but `Skip`. `HandOff` writes the request file
/// ([`super::request::write_request`]); if that write itself fails, the
/// outcome logged falls back to `no-delivery-path` — a hand-off that could
/// not be written is, from the session's point of view, no hand-off at all.
fn handle_decision(
    sid: &str,
    now: i64,
    context_window: &str,
    vim_mode: Option<&str>,
    decision: Decision,
) {
    match decision {
        Decision::Skip { .. } => {}
        Decision::HandOff {
            supervisor_pid,
            mode,
            remaining_secs,
            recache_tokens,
        } => {
            claim_idle_period(sid);
            let req = Request {
                v: request::REQUEST_V,
                mode: mode.as_str().to_owned(),
                sid: sid.to_owned(),
                written_at: now,
                deadline: now + remaining_secs - DEADLINE_MARGIN_SECS,
                recache_tokens,
                remaining_secs,
                vim_mode: vim_mode.map(str::to_owned),
            };
            let dir = paths::idle_compact_requests_dir();
            let outcome = match request::write_request(&dir, supervisor_pid, &req) {
                Ok(()) => "handed-off",
                Err(_) => "no-delivery-path",
            };
            log_outcome(
                sid,
                outcome,
                &LogFields {
                    remaining_secs: Some(remaining_secs),
                    recache_tokens: Some(recache_tokens),
                    context_window: Some(context_window),
                    vim: vim_mode,
                    ..Default::default()
                },
            );
        }
        Decision::NoDeliveryPath {
            remaining_secs,
            recache_tokens,
        } => {
            claim_idle_period(sid);
            log_outcome(
                sid,
                "no-delivery-path",
                &LogFields {
                    remaining_secs: Some(remaining_secs),
                    recache_tokens: Some(recache_tokens),
                    context_window: Some(context_window),
                    ..Default::default()
                },
            );
        }
    }
}

fn now_epoch() -> i64 {
    crate::epoch::now_secs() as i64
}

/// Claim `<sid>.idle-compacted` so the rest of this idle period (every
/// statusline tick until the next Stop) short-circuits on
/// `already_fired` instead of re-logging. Best-effort.
fn claim_idle_period(sid: &str) {
    let _ = super::write_marker(&paths::idle_compacted(sid), &now_epoch().to_string());
}

/// Read `<sid>.idle`'s bare-epoch content. `None` on any I/O or parse
/// failure — [`decide`] treats that the same as "never seen a Stop".
fn read_epoch_marker(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

// ─── busy check ───────────────────────────────────────────────────────────────

/// How much of the transcript's tail [`read_transcript_tail`] reads at
/// most, so a very long session can never turn one statusline tick into a
/// large read.
const TRANSCRIPT_TAIL_MAX_BYTES: u64 = 1024 * 1024;

/// Whether the session at `transcript_path` is still busy relative to the
/// `<sid>.idle` `stamp`. `None` only on a read error (missing file,
/// permission, I/O), same as an unreadable transcript has always meant.
/// The cheap path — mtime at or before the stamp — never opens the file a
/// second time to read its tail; only a later mtime does.
fn busy_state(transcript_path: &str, stamp: i64) -> Option<BusyState> {
    let modified = std::fs::metadata(transcript_path).ok()?.modified().ok()?;
    let mtime = crate::epoch::from_systemtime(modified) as i64;
    if mtime <= stamp {
        return Some(BusyState::NotBusy);
    }
    let lines = read_transcript_tail(transcript_path)?;
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    Some(classify_tail(&refs, stamp))
}

/// Read up to the last [`TRANSCRIPT_TAIL_MAX_BYTES`] of `path` and split it
/// into lines, via [`tail_lines`]. `None` on any I/O error.
fn read_transcript_tail(path: &str) -> Option<Vec<String>> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_MAX_BYTES);
    if start > 0 {
        f.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    Some(tail_lines(&text, start > 0))
}

/// Split `text` into lines, dropping the first when `may_be_partial`. A
/// seek that did not start at byte 0 can land in the middle of a line —
/// either the tail end of a line whose start was cut off, or (rarely) a row
/// Claude Code was still writing — so that first line is unreliable and is
/// dropped rather than fed to the parser. Pure, so the "partial first line"
/// behavior is testable without writing a multi-megabyte fixture file.
fn tail_lines(text: &str, may_be_partial: bool) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if may_be_partial && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

/// The pure classifier (Invariant 4): given the transcript tail's already-
/// read `lines` and the idle `stamp`, decide busy/not-busy/inconclusive by
/// walking backwards for the last `user`/`assistant` row. Every other row
/// type — Claude Code's post-Stop metadata rows among them — is skipped
/// without being inspected further; only that one row's own `timestamp`
/// decides the outcome (see the module doc's "Busy check" section), against
/// whichever of `stamp` and an interrupted-turn timestamp
/// ([`interrupted_turn_end`]) is later.
fn classify_tail(lines: &[&str], stamp: i64) -> BusyState {
    let effective_stamp = match interrupted_turn_end(lines) {
        Some(ts) => stamp.max(ts),
        None => stamp,
    };
    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        if kind != Some("user") && kind != Some("assistant") {
            continue;
        }
        let epoch = value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso8601_floor_secs);
        return match epoch {
            None => BusyState::Busy,
            Some(ts) if ts > effective_stamp => BusyState::Busy,
            Some(_) => BusyState::NotBusy,
        };
    }
    BusyState::Inconclusive
}

/// A row's own inline text: `message.content` as a plain string, or the
/// first `type: "text"` block when it is an array of typed content blocks
/// (tool_result/tool_use/text mixed in). `None` for every other shape,
/// including a pure tool_result array with no text block.
fn row_text(value: &serde_json::Value) -> Option<&str> {
    let content = value.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s);
    }
    content.as_array()?.iter().find_map(|block| {
        if block.get("type")?.as_str()? == "text" {
            block.get("text")?.as_str()
        } else {
            None
        }
    })
}

/// A `user` row is an "interrupted turn" marker when its own text starts
/// with Claude Code's interrupt prefix. Never true for a tool_result row
/// (its content is `tool_result` blocks, not a `text` block, so
/// [`row_text`] returns `None` for it).
fn is_interrupted_row(value: &serde_json::Value) -> bool {
    row_text(value)
        .map(|t| t.starts_with("[Request interrupted by user"))
        .unwrap_or(false)
}

/// The newest non-sidechain `user`/`assistant` row's own timestamp, when
/// that row is an interrupted-turn marker ([`is_interrupted_row`]).
/// `isSidechain: true` rows (subagent transcript rows interleaved into the
/// same file) are skipped when looking for this "newest" row, exactly as
/// the design spec's interrupt rule scopes it — a separate, dedicated
/// backward walk from [`classify_tail`]'s own (which does not skip
/// sidechain rows, unchanged from before this rule existed).
fn interrupted_turn_end(lines: &[&str]) -> Option<i64> {
    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        if kind != Some("user") && kind != Some("assistant") {
            continue;
        }
        // Found the newest non-sidechain turn row: decide right here,
        // never looking further back — an older interrupt must not
        // resurrect a turn a later row already moved past.
        if kind != Some("user") || !is_interrupted_row(&value) {
            return None;
        }
        return value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso8601_floor_secs);
    }
    None
}

/// Parse an ISO 8601 / RFC 3339 timestamp (with or without a fractional
/// seconds part — `chrono` accepts both) and floor it to whole epoch
/// seconds (`DateTime::timestamp` already truncates any fractional part
/// away, which is exactly the floor the busy check wants). `None` on
/// anything unparseable.
fn parse_iso8601_floor_secs(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Best-effort rendering of the payload's `context_window` value for the log
/// line: a bare number renders as-is; any other shape (object, string,
/// absent) renders as `unknown` rather than guessing at an undocumented
/// nested shape (see `StatuslinePayload::context_window`'s doc).
fn context_window_summary(v: Option<&serde_json::Value>) -> String {
    match v.and_then(serde_json::Value::as_i64) {
        Some(n) => n.to_string(),
        None => "unknown".to_string(),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── context_window_summary (pure) ─────────────────────────────────────────

    #[test]
    fn context_window_summary_renders_a_bare_number() {
        assert_eq!(
            context_window_summary(Some(&serde_json::json!(200_000))),
            "200000"
        );
    }

    #[test]
    fn context_window_summary_unknown_for_object_string_or_absent() {
        assert_eq!(
            context_window_summary(Some(&serde_json::json!({"used": 1}))),
            "unknown"
        );
        assert_eq!(
            context_window_summary(Some(&serde_json::json!("200k"))),
            "unknown"
        );
        assert_eq!(context_window_summary(None), "unknown");
    }

    // ── decide (pure core) ─────────────────────────────────────────────────────

    const NOW: i64 = 1_000_000;

    /// Every condition holds: mode on, cache warm with 200s left, a
    /// comfortably-above-threshold re-write estimate, a not-busy session,
    /// and a supervisor is present.
    fn passing_inputs() -> DecisionInputs {
        DecisionInputs {
            mode: IdleCompactMode::On,
            already_fired: false,
            cache_warm: Some(true),
            expires_at: Some(NOW + 200),
            recache_tokens_if_cold: Some(150_000),
            idle_marker_epoch: Some(NOW - 1_000),
            busy: Some(BusyState::NotBusy),
            supervisor_pid: Some(4242),
        }
    }

    #[test]
    fn decide_hands_off_when_every_condition_holds() {
        assert_eq!(
            decide(NOW, &passing_inputs()),
            Decision::HandOff {
                supervisor_pid: 4242,
                mode: IdleCompactMode::On,
                remaining_secs: 200,
                recache_tokens: 150_000,
            }
        );
    }

    #[test]
    fn decide_hands_off_with_dry_run_mode_carried_through() {
        let inputs = DecisionInputs {
            mode: IdleCompactMode::DryRun,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::HandOff {
                supervisor_pid: 4242,
                mode: IdleCompactMode::DryRun,
                remaining_secs: 200,
                recache_tokens: 150_000,
            }
        );
    }

    #[test]
    fn decide_has_no_delivery_path_when_no_supervisor_is_present() {
        let inputs = DecisionInputs {
            supervisor_pid: None,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::NoDeliveryPath {
                remaining_secs: 200,
                recache_tokens: 150_000,
            }
        );
    }

    #[test]
    fn decide_skips_when_mode_is_off() {
        let inputs = DecisionInputs {
            mode: IdleCompactMode::Off,
            ..passing_inputs()
        };
        assert_eq!(decide(NOW, &inputs), Decision::Skip { reason: "mode-off" });
    }

    #[test]
    fn decide_skips_when_already_fired_this_idle_period() {
        let inputs = DecisionInputs {
            already_fired: true,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "already-fired-this-idle-period"
            }
        );
    }

    #[test]
    fn decide_skips_when_cache_is_not_warm() {
        for warm in [None, Some(false)] {
            let inputs = DecisionInputs {
                cache_warm: warm,
                ..passing_inputs()
            };
            assert_eq!(
                decide(NOW, &inputs),
                Decision::Skip {
                    reason: "cache-not-warm"
                },
                "cache_warm={warm:?}"
            );
        }
    }

    #[test]
    fn decide_skips_when_expiry_info_is_absent() {
        let inputs = DecisionInputs {
            expires_at: None,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "no-expiry-info"
            }
        );
    }

    #[test]
    fn decide_skips_when_cache_already_expired() {
        let inputs = DecisionInputs {
            expires_at: Some(NOW),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "cache-already-expired"
            }
        );
    }

    #[test]
    fn decide_skips_when_remaining_time_is_too_long() {
        let inputs = DecisionInputs {
            expires_at: Some(NOW + FIRE_WINDOW_SECS + 1),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "remaining-too-long"
            }
        );
    }

    #[test]
    fn decide_hands_off_at_exactly_the_fire_window_boundary() {
        let inputs = DecisionInputs {
            expires_at: Some(NOW + FIRE_WINDOW_SECS),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::HandOff {
                supervisor_pid: 4242,
                mode: IdleCompactMode::On,
                remaining_secs: FIRE_WINDOW_SECS,
                recache_tokens: 150_000,
            }
        );
    }

    #[test]
    fn decide_skips_when_recache_estimate_is_absent() {
        let inputs = DecisionInputs {
            recache_tokens_if_cold: None,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "no-recache-estimate"
            }
        );
    }

    #[test]
    fn decide_skips_when_recache_tokens_are_below_threshold() {
        let inputs = DecisionInputs {
            recache_tokens_if_cold: Some(MIN_RECACHE_TOKENS - 1),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "recache-tokens-below-threshold"
            }
        );
    }

    #[test]
    fn decide_hands_off_at_exactly_the_recache_token_threshold() {
        let inputs = DecisionInputs {
            recache_tokens_if_cold: Some(MIN_RECACHE_TOKENS),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::HandOff {
                supervisor_pid: 4242,
                mode: IdleCompactMode::On,
                remaining_secs: 200,
                recache_tokens: MIN_RECACHE_TOKENS,
            }
        );
    }

    #[test]
    fn decide_skips_when_no_idle_marker_exists() {
        let inputs = DecisionInputs {
            idle_marker_epoch: None,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "no-idle-marker"
            }
        );
    }

    #[test]
    fn decide_skips_when_busy_state_is_unreadable() {
        let inputs = DecisionInputs {
            busy: None,
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "transcript-unreadable"
            }
        );
    }

    #[test]
    fn decide_skips_when_session_is_busy() {
        let inputs = DecisionInputs {
            busy: Some(BusyState::Busy),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "session-busy"
            }
        );
    }

    #[test]
    fn decide_skips_when_busy_state_is_inconclusive() {
        let inputs = DecisionInputs {
            busy: Some(BusyState::Inconclusive),
            ..passing_inputs()
        };
        assert_eq!(
            decide(NOW, &inputs),
            Decision::Skip {
                reason: "transcript-tail-inconclusive"
            }
        );
    }

    // ── classify_tail / tail_lines / parse_iso8601_floor_secs (pure) ─────────────
    //
    // No real transcript content anywhere here (Invariant 1): every row is
    // hand-written. STAMP is 2023-11-14T22:13:20Z as an epoch; the rows use
    // that same instant, one second before it, and one second after it.

    const STAMP: i64 = 1_700_000_000;

    #[test]
    fn classify_tail_not_busy_when_metadata_rows_follow_a_turn_row_before_stamp() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2023-11-14T22:13:19.000Z"}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"system","subtype":"away_summary"}"#,
            r#"{"type":"mode"}"#,
        ];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::NotBusy);
    }

    #[test]
    fn classify_tail_busy_when_a_user_row_follows_the_stamp() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2023-11-14T22:13:15.000Z"}"#,
            r#"{"type":"user","timestamp":"2023-11-14T22:13:21.000Z"}"#,
        ];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::Busy);
    }

    #[test]
    fn classify_tail_busy_when_an_assistant_row_follows_the_stamp() {
        let lines = [
            r#"{"type":"user","timestamp":"2023-11-14T22:13:15.000Z"}"#,
            r#"{"type":"assistant","timestamp":"2023-11-14T22:13:21.000Z"}"#,
        ];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::Busy);
    }

    #[test]
    fn classify_tail_not_busy_at_exactly_the_same_second_as_the_stamp() {
        let lines = [r#"{"type":"assistant","timestamp":"2023-11-14T22:13:20.000Z"}"#];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::NotBusy);
    }

    #[test]
    fn classify_tail_busy_when_the_last_turn_row_has_no_timestamp() {
        let lines = [r#"{"type":"assistant"}"#];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::Busy);
    }

    #[test]
    fn classify_tail_inconclusive_when_only_metadata_rows_are_present() {
        let lines = [
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"last-prompt"}"#,
            r#"{"type":"mode"}"#,
        ];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::Inconclusive);
    }

    #[test]
    fn classify_tail_skips_unparseable_lines() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2023-11-14T22:13:19.000Z"}"#,
            "not json at all",
            r#"{"type":"system","subtype":"mode""#, // truncated, invalid JSON
        ];
        assert_eq!(classify_tail(&lines, STAMP), BusyState::NotBusy);
    }

    #[test]
    fn tail_lines_keeps_every_line_when_the_read_started_at_offset_zero() {
        let text = "{\"type\":\"assistant\"}\n{\"type\":\"user\"}\n";
        assert_eq!(
            tail_lines(text, false),
            vec![
                r#"{"type":"assistant"}"#.to_owned(),
                r#"{"type":"user"}"#.to_owned(),
            ]
        );
    }

    #[test]
    fn tail_lines_drops_a_possibly_partial_first_line() {
        let text = "-tail end of a cut-off prior line\n{\"type\":\"assistant\"}\n";
        assert_eq!(
            tail_lines(text, true),
            vec![r#"{"type":"assistant"}"#.to_owned()]
        );
    }

    #[test]
    fn parse_iso8601_floor_secs_floors_milliseconds() {
        let with_ms = parse_iso8601_floor_secs("2023-11-14T22:13:20.999Z").unwrap();
        let without_ms = parse_iso8601_floor_secs("2023-11-14T22:13:20Z").unwrap();
        assert_eq!(with_ms, STAMP);
        assert_eq!(without_ms, STAMP);
    }

    #[test]
    fn parse_iso8601_floor_secs_none_for_garbage() {
        assert_eq!(parse_iso8601_floor_secs("not a timestamp"), None);
        assert_eq!(parse_iso8601_floor_secs(""), None);
    }

    // ── interrupted turns (pure) ────────────────────────────────────────────────

    fn interrupt_row(ts: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"[Request interrupted by user]"}}}}"#
        )
    }

    fn user_text_row(ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn tool_result_row(ts: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok"}}]}}}}"#
        )
    }

    fn assistant_row(ts: &str) -> String {
        format!(r#"{{"type":"assistant","timestamp":"{ts}"}}"#)
    }

    /// Interrupt after Stop: an interrupt row newer than the Stop marker,
    /// nothing after it. The turn must be classified not-busy (idle) even
    /// though the interrupt row is a `user` row newer than the stamp.
    #[test]
    fn classify_tail_interrupt_after_stop_is_not_busy() {
        let stop_stamp = STAMP;
        let interrupt = interrupt_row("2023-11-14T22:15:00.000Z"); // well after STAMP
        let lines = [assistant_row("2023-11-14T22:13:19.000Z"), interrupt];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(classify_tail(&refs, stop_stamp), BusyState::NotBusy);
    }

    /// Stop after interrupt: the Stop marker is newer than the interrupt
    /// row, so ordinary comparison already covers it (the interrupt-turn
    /// logic must not somehow regress this into Busy).
    #[test]
    fn classify_tail_stop_after_interrupt_is_not_busy() {
        let interrupt = interrupt_row("2023-11-14T22:10:00.000Z"); // before STAMP
        let lines = [interrupt];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(classify_tail(&refs, STAMP), BusyState::NotBusy);
    }

    /// Interrupt followed by a new user prompt: the session is busy again
    /// on the new prompt's own timestamp, regardless of the earlier
    /// interrupt.
    #[test]
    fn classify_tail_interrupt_then_new_prompt_is_busy() {
        let interrupt = interrupt_row("2023-11-14T22:10:00.000Z");
        let new_prompt = user_text_row("2023-11-14T22:20:00.000Z", "let's keep going");
        let lines = [interrupt, new_prompt];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(classify_tail(&refs, STAMP), BusyState::Busy);
    }

    /// A tool_result `user` row (Claude Code's own shape for a tool result)
    /// must never be mistaken for an interrupt row — it classifies purely
    /// on its own timestamp, exactly as any other turn row would.
    #[test]
    fn classify_tail_tool_result_user_row_is_not_an_interrupt() {
        let row = tool_result_row("2023-11-14T22:15:00.000Z"); // after STAMP
        let lines = [row];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(
            classify_tail(&refs, STAMP),
            BusyState::Busy,
            "a tool_result row after the stamp is ordinary busy evidence, not an interrupt"
        );
    }

    #[test]
    fn is_interrupted_row_true_only_for_the_exact_prefix() {
        let v: serde_json::Value =
            serde_json::from_str(&interrupt_row("2023-11-14T22:10:00Z")).unwrap();
        assert!(is_interrupted_row(&v));
        let v2: serde_json::Value =
            serde_json::from_str(&user_text_row("2023-11-14T22:10:00Z", "hello")).unwrap();
        assert!(!is_interrupted_row(&v2));
    }

    #[test]
    fn interrupted_turn_end_skips_sidechain_rows() {
        let sidechain = r#"{"type":"user","isSidechain":true,"timestamp":"2023-11-14T22:19:00.000Z","message":{"role":"user","content":"[Request interrupted by user]"}}"#;
        let real_last = assistant_row("2023-11-14T22:18:00.000Z");
        let lines = [real_last, sidechain.to_owned()];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        // The newest NON-sidechain row is the assistant row, not an
        // interrupt, so this must be None.
        assert_eq!(interrupted_turn_end(&refs), None);
    }

    // ── run_from_raw (I/O shell) ───────────────────────────────────────────────

    #[test]
    fn run_from_raw_is_a_noop_when_mode_is_off() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            // No config.json written at all — default mode is "off".
            let raw = serde_json::json!({
                "session_id": "run-raw-sid-off",
                "prompt_cache": {
                    "warm": true,
                    "expires_at": now_epoch() + 10,
                    "recache_tokens_if_cold": 999_999
                }
            })
            .to_string();
            run_from_raw(&raw);
            assert!(
                !paths::smart_dir_no_create().exists(),
                "mode=off must touch nothing under smart_dir"
            );
        });
    }

    #[test]
    fn run_from_raw_is_a_noop_on_unparseable_json() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            run_from_raw("{not json");
            assert!(!paths::smart_dir_no_create().exists());
        });
    }

    #[test]
    fn run_from_raw_is_a_noop_without_a_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            run_from_raw(r#"{"prompt_cache": {"warm": true}}"#);
            assert!(!paths::smart_dir_no_create().exists());
        });
    }

    /// No `CSM_SUPERVISOR_PID` set: every gate passes but there is nothing
    /// to hand off to, so the tick logs `no-delivery-path` once and still
    /// claims the idle period.
    #[test]
    fn run_from_raw_logs_no_delivery_path_without_a_supervisor() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, None)], || {
                let sid = "run-raw-sid-no-supervisor";
                crate::config::Config {
                    idle_compact: Some("on".to_owned()),
                    ..Default::default()
                }
                .save()
                .unwrap();

                let now = now_epoch();
                super::super::write_marker(&paths::idle(sid), &now.to_string()).unwrap();

                let transcript = tmp.path().join("session.jsonl");
                std::fs::write(&transcript, "{}").unwrap();
                filetime::set_file_mtime(
                    &transcript,
                    filetime::FileTime::from_unix_time(now - 10, 0),
                )
                .unwrap();

                let raw = serde_json::json!({
                    "session_id": sid,
                    "transcript_path": transcript.to_string_lossy(),
                    "prompt_cache": {
                        "warm": true,
                        "expires_at": now + 200,
                        "recache_tokens_if_cold": 150_000
                    },
                    "context_window": 200_000
                })
                .to_string();

                run_from_raw(&raw);

                assert!(
                    paths::idle_compacted(sid).exists(),
                    "the idle period must still be claimed"
                );
                let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
                assert!(log.contains("outcome=no-delivery-path"), "log: {log}");
                assert!(log.contains("context_window=200000"), "log: {log}");
                assert!(log.contains(&format!("sid={sid}")), "log: {log}");
                assert!(
                    !paths::idle_compact_requests_dir().exists()
                        || std::fs::read_dir(paths::idle_compact_requests_dir())
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(true),
                    "no request file must be written without a supervisor"
                );
            });
        });
    }

    /// A dead pid in `CSM_SUPERVISOR_PID` (a fixed, never-valid pid on any
    /// real system: 0 is filtered explicitly, and this test does not claim
    /// any other specific pid is dead — it only asserts that
    /// `supervisor_pid_from_env` requires liveness, exercised indirectly
    /// through `is_running`'s own behavior for an obviously-not-running
    /// value) is treated exactly like no supervisor at all.
    #[test]
    fn supervisor_pid_from_env_rejects_pid_zero() {
        crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, Some("0"))], || {
            assert_eq!(supervisor_pid_from_env(), None);
        });
    }

    #[test]
    fn supervisor_pid_from_env_none_when_unset_or_unparseable() {
        crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, None)], || {
            assert_eq!(supervisor_pid_from_env(), None);
        });
        crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, Some("not-a-number"))], || {
            assert_eq!(supervisor_pid_from_env(), None);
        });
    }

    /// The current test process's own pid is, definitionally, alive: this
    /// is the one liveness case a test can assert without faking
    /// `platform::proc::is_running`.
    #[test]
    fn supervisor_pid_from_env_accepts_the_current_process() {
        let pid = std::process::id().to_string();
        crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, Some(pid.as_str()))], || {
            assert_eq!(supervisor_pid_from_env(), Some(std::process::id()));
        });
    }

    /// A live supervisor pid (this test process's own pid): the tick must
    /// hand off — write a request file with the right fields, claim the
    /// marker, and log `handed-off`.
    #[test]
    fn run_from_raw_hands_off_to_a_live_supervisor() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let pid = std::process::id().to_string();
            crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, Some(pid.as_str()))], || {
                let sid = "run-raw-sid-hand-off";
                crate::config::Config {
                    idle_compact: Some("on".to_owned()),
                    ..Default::default()
                }
                .save()
                .unwrap();

                let now = now_epoch();
                super::super::write_marker(&paths::idle(sid), &now.to_string()).unwrap();

                let transcript = tmp.path().join("session.jsonl");
                std::fs::write(&transcript, "{}").unwrap();
                filetime::set_file_mtime(
                    &transcript,
                    filetime::FileTime::from_unix_time(now - 10, 0),
                )
                .unwrap();

                let raw = serde_json::json!({
                    "session_id": sid,
                    "transcript_path": transcript.to_string_lossy(),
                    "prompt_cache": {
                        "warm": true,
                        "expires_at": now + 200,
                        "recache_tokens_if_cold": 150_000
                    },
                    "vim": {"mode": "insert"},
                    "context_window": 200_000
                })
                .to_string();

                run_from_raw(&raw);

                assert!(
                    paths::idle_compacted(sid).exists(),
                    "the idle period must be claimed"
                );
                let req_path = paths::idle_compact_request(std::process::id());
                let bytes = std::fs::read(&req_path).expect("request file must exist");
                let req: Request = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(req.sid, sid);
                assert_eq!(req.mode, "on");
                // run_from_raw reads the clock again; a slow runner can
                // cross a second boundary after `now` was taken.
                assert!(
                    (199..=200).contains(&req.remaining_secs),
                    "remaining_secs: {}",
                    req.remaining_secs
                );
                assert_eq!(req.recache_tokens, 150_000);
                assert_eq!(req.deadline, now + 200 - DEADLINE_MARGIN_SECS);
                assert_eq!(req.vim_mode.as_deref(), Some("insert"));

                let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
                assert!(log.contains("outcome=handed-off"), "log: {log}");
                assert!(log.contains("vim=insert"), "log: {log}");
            });
        });
    }

    #[test]
    fn run_from_raw_is_silent_when_already_fired_this_idle_period() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, None)], || {
                let sid = "run-raw-sid-already-fired";
                crate::config::Config {
                    idle_compact: Some("on".to_owned()),
                    ..Default::default()
                }
                .save()
                .unwrap();
                std::fs::create_dir_all(paths::smart_dir_no_create()).unwrap();
                std::fs::write(paths::idle_compacted(sid), "1").unwrap();

                let now = now_epoch();
                super::super::write_marker(&paths::idle(sid), &now.to_string()).unwrap();
                let transcript = tmp.path().join("session.jsonl");
                std::fs::write(&transcript, "{}").unwrap();
                filetime::set_file_mtime(
                    &transcript,
                    filetime::FileTime::from_unix_time(now - 10, 0),
                )
                .unwrap();

                let raw = serde_json::json!({
                    "session_id": sid,
                    "transcript_path": transcript.to_string_lossy(),
                    "prompt_cache": {
                        "warm": true,
                        "expires_at": now + 200,
                        "recache_tokens_if_cold": 150_000
                    }
                })
                .to_string();

                run_from_raw(&raw);

                assert!(
                    !paths::idle_compact_log().exists(),
                    "an already-fired idle period must not be re-logged"
                );
            });
        });
    }

    /// An interrupted turn with no `Stop` hook still hands off: the busy
    /// check must classify not-busy from the interrupt row alone.
    #[test]
    fn run_from_raw_hands_off_after_an_interrupted_turn_with_no_stop() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let pid = std::process::id().to_string();
            crate::testenv::with_env_vars(&[(SUPERVISOR_PID_ENV, Some(pid.as_str()))], || {
                let sid = "run-raw-sid-interrupted-turn";
                crate::config::Config {
                    idle_compact: Some("on".to_owned()),
                    ..Default::default()
                }
                .save()
                .unwrap();

                // An old Stop stamp: the interrupt row alone must be
                // enough to classify this session not-busy despite the
                // transcript's mtime being much newer than that stamp.
                let old_stop = now_epoch() - 10_000;
                super::super::write_marker(&paths::idle(sid), &old_stop.to_string()).unwrap();

                let transcript = tmp.path().join("session.jsonl");
                let interrupted_at = crate::epoch::now_secs() as i64 - 5;
                let ts = chrono::DateTime::from_timestamp(interrupted_at, 0)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                std::fs::write(
                    &transcript,
                    format!(
                        r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"[Request interrupted by user]"}}}}"#
                    ),
                )
                .unwrap();
                // mtime after old_stop, so the cheap path alone would call
                // this busy; the tail read (with the interrupt rule) must
                // not.
                filetime::set_file_mtime(
                    &transcript,
                    filetime::FileTime::from_unix_time(interrupted_at, 0),
                )
                .unwrap();

                let now = now_epoch();
                let raw = serde_json::json!({
                    "session_id": sid,
                    "transcript_path": transcript.to_string_lossy(),
                    "prompt_cache": {
                        "warm": true,
                        "expires_at": now + 200,
                        "recache_tokens_if_cold": 150_000
                    }
                })
                .to_string();

                run_from_raw(&raw);

                let log = std::fs::read_to_string(paths::idle_compact_log()).unwrap();
                assert!(
                    log.contains("outcome=handed-off"),
                    "an interrupted turn with no Stop must still hand off; log: {log}"
                );
            });
        });
    }
}
