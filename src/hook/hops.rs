//! `<state>/hops.jsonl`: one JSON line per limit switch and per model
//! fallback, the structured twin of `limit-switch.log` for tooling that wants
//! to count or chart hops without parsing a text log.
//!
//! Fields: `at` (RFC3339 UTC), `sid`, `kind` (`switch` | `model-fallback`),
//! `reason` (the classification: `rate_limit`, `session`, `week_all`,
//! `week_fable`), `from_account` and `to_account` (Orca account ids or
//! null, never a name or an email), plus `model` for a fallback.
//!
//! Who emits what, because the facts live in different places. A model
//! fallback is decided by the hook and the same-account relaunch needs no
//! further input, so `stop::commit_and_stop` records it once the sentinel is
//! written. An account switch is only known in `limit_switch::run_hop`: the
//! hook's `target_account` is a proposal, and the supervisor may re-pick it,
//! follow a switch another session made, or stay. `run_hop` records the hop
//! that really switched `D`, from the supervisor's own process.
//!
//! The formatter is pure; [`append`] is the thin shell. It is best effort and
//! returns nothing: a full disk or a read-only state dir must never fail a
//! hop. The hook-side call does no network, RPC or Keychain access
//! (Invariant 6), only a local append.

use std::io::Write as _;

use serde::Serialize;

/// Past this size the next append moves the file to `hops.jsonl.1` (one
/// generation, replaced each time).
pub const MAX_BYTES: u64 = 1024 * 1024;

pub const KIND_SWITCH: &str = "switch";
pub const KIND_MODEL_FALLBACK: &str = "model-fallback";

/// One hop. Field order is the wire order.
#[derive(Debug, Clone, Serialize)]
pub struct Hop<'a> {
    pub at: String,
    pub sid: &'a str,
    pub kind: &'a str,
    pub reason: &'a str,
    pub from_account: Option<&'a str>,
    pub to_account: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
}

/// A sentinel/commit reason (`limit:week_all`) as the classification alone
/// (`week_all`). Pure.
pub fn classification(reason: &str) -> &str {
    reason.strip_prefix("limit:").unwrap_or(reason)
}

/// RFC3339 UTC with second precision for an epoch-seconds stamp. Pure.
pub fn rfc3339_utc(epoch_secs: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The JSON line (no trailing newline) for one hop. Empty ids are null. Pure.
pub fn format_line(
    at_epoch: i64,
    sid: &str,
    kind: &str,
    reason: &str,
    from_account: Option<&str>,
    to_account: Option<&str>,
    model: Option<&str>,
) -> String {
    fn id(a: Option<&str>) -> Option<&str> {
        a.filter(|s| !s.is_empty())
    }
    let hop = Hop {
        at: rfc3339_utc(at_epoch),
        sid,
        kind,
        reason: classification(reason),
        from_account: id(from_account),
        to_account: id(to_account),
        model,
    };
    serde_json::to_string(&hop).unwrap_or_default()
}

/// Append one line to `<state>/hops.jsonl`, rotating a file past
/// [`MAX_BYTES`] first. Best effort: every error is ignored.
pub fn append(line: &str) {
    if line.is_empty() {
        return;
    }
    let Ok(dir) = crate::paths::smart_dir() else {
        return;
    };
    append_in(&dir.join("hops.jsonl"), line, MAX_BYTES);
}

fn append_in(path: &std::path::Path, line: &str, max_bytes: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let rotated = std::path::PathBuf::from(rotated);
        // Windows refuses to rename over an existing file.
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(path, &rotated);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    else {
        return;
    };
    // One write call per line keeps concurrent O_APPEND writers from
    // interleaving inside a line.
    let _ = f.write_all(format!("{line}\n").as_bytes());
}

/// Record a model fallback now.
pub fn record_model_fallback(sid: &str, reason: &str, account: &str, model: &str) {
    let now = crate::epoch::now_secs() as i64;
    append(&format_line(
        now,
        sid,
        KIND_MODEL_FALLBACK,
        reason,
        Some(account),
        Some(account),
        Some(model),
    ));
}

/// Record an account switch now.
pub fn record_switch(sid: &str, reason: &str, from: Option<&str>, to: &str) {
    let now = crate::epoch::now_secs() as i64;
    append(&format_line(
        now,
        sid,
        KIND_SWITCH,
        reason,
        from,
        Some(to),
        None,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_line_has_the_documented_fields_in_order() {
        let line = format_line(
            1_700_000_000,
            "sid-1",
            KIND_SWITCH,
            "limit:week_all",
            Some("acct-a"),
            Some("acct-b"),
            None,
        );
        assert_eq!(
            line,
            r#"{"at":"2023-11-14T22:13:20Z","sid":"sid-1","kind":"switch","reason":"week_all","from_account":"acct-a","to_account":"acct-b"}"#
        );
    }

    #[test]
    fn a_fallback_line_carries_the_model_and_unknown_ids_are_null() {
        let line = format_line(
            0,
            "s",
            KIND_MODEL_FALLBACK,
            "limit:week_fable",
            None,
            Some(""),
            Some("opus"),
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["at"], "1970-01-01T00:00:00Z");
        assert_eq!(v["kind"], "model-fallback");
        assert_eq!(v["reason"], "week_fable");
        assert!(v["from_account"].is_null());
        assert!(v["to_account"].is_null());
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn classification_strips_only_the_limit_prefix() {
        assert_eq!(classification("limit:session"), "session");
        assert_eq!(classification("rate_limit"), "rate_limit");
    }

    #[test]
    fn append_adds_lines_and_rotates_one_generation_past_the_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hops.jsonl");
        append_in(&path, "one", 10);
        append_in(&path, "two", 10);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        // 8 bytes: under the cap of 10, no rotation yet. Push it over.
        append_in(&path, "three", 10);
        append_in(&path, "four", 10);
        let rotated = dir.path().join("hops.jsonl.1");
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "four\n");
        // A second rotation replaces the one generation.
        append_in(&path, "five-five-five", 10);
        append_in(&path, "six", 10);
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "four\nfive-five-five\n"
        );
    }

    #[test]
    fn append_into_a_missing_dir_is_silent() {
        let dir = tempfile::TempDir::new().unwrap();
        append_in(&dir.path().join("nope").join("hops.jsonl"), "x", 10);
    }
}
