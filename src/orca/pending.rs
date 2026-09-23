//! A pending Orca select: `<smart-dir>/orca-pending-select.json`.
//!
//! Written when the user picks a bound profile while Orca is not running
//! (`csm orca use --queue`, and in Slice B `csm profiles use` / `cas -g`).
//! Shape: `{accountId, profile, queuedAt (epoch s), expectedPriorActiveId}`
//! where `expectedPriorActiveId` is Orca's persisted active id at queue time
//! (may be null).
//!
//! Applied only by `csm orca sync`, `csm orca use`, `csm profiles use`, and
//! the detached `csm orca sync --quiet` that `csm run` spawns — never by the
//! print/eval/statusline/hook/usage paths. It is applied only while it is
//! less than 24 h old AND Orca's live effective active id still equals
//! `expectedPriorActiveId` (the user has not picked something else in Orca
//! meanwhile); otherwise it is dropped ([`verdict`]).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How long a queued select stays applicable.
pub const MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// Tolerated clock skew for a `queuedAt` slightly in the future.
const FUTURE_SKEW_SECS: i64 = 300;

/// The pending-select record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSelect {
    #[serde(rename = "accountId")]
    pub account_id: String,
    pub profile: String,
    #[serde(rename = "queuedAt")]
    pub queued_at: i64,
    #[serde(rename = "expectedPriorActiveId", default)]
    pub expected_prior_active_id: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

impl PendingSelect {
    pub fn new(
        account_id: &str,
        profile: &str,
        queued_at: i64,
        expected_prior: Option<&str>,
    ) -> Self {
        PendingSelect {
            account_id: account_id.to_owned(),
            profile: profile.to_owned(),
            queued_at,
            expected_prior_active_id: expected_prior.map(str::to_owned),
            extra: Default::default(),
        }
    }
}

/// What to do with a pending select, given Orca's live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Apply it: fresh, and Orca still shows the expected prior account.
    Valid,
    /// Older than 24 h (or implausibly in the future): drop it.
    Expired,
    /// Orca's active account changed since it was queued: drop it.
    Superseded,
    /// Orca already has the target active: drop it, nothing to do.
    AlreadyApplied,
}

/// Judge `p` against Orca's live effective active id at `now`. Pure.
pub fn verdict(p: &PendingSelect, live_effective: Option<&str>, now: i64) -> Verdict {
    let age = now - p.queued_at;
    if !(-FUTURE_SKEW_SECS..MAX_AGE_SECS).contains(&age) {
        return Verdict::Expired;
    }
    if live_effective == Some(p.account_id.as_str()) {
        return Verdict::AlreadyApplied;
    }
    if live_effective != p.expected_prior_active_id.as_deref() {
        return Verdict::Superseded;
    }
    Verdict::Valid
}

/// Read the pending select at `path`. `Ok(None)` when absent; a malformed
/// file is an `Err` (callers drop it).
pub fn read_from(path: &Path) -> io::Result<Option<PendingSelect>> {
    let Some(text) = super::read_capped(path, 64 * 1024)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is malformed: {e}", path.display()),
        )
    })
}

/// Write `p` to `path` atomically.
pub fn write_to(path: &Path, p: &PendingSelect) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(p).map_err(io::Error::other)?;
    json.push(b'\n');
    super::atomic_write(path, &json)
}

/// Delete the pending select at `path` (absent is fine).
pub fn clear_at(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// [`read_from`] at the canonical path.
pub fn read() -> io::Result<Option<PendingSelect>> {
    read_from(&crate::paths::orca_pending_select())
}

/// [`write_to`] at the canonical path.
pub fn write(p: &PendingSelect) -> io::Result<()> {
    write_to(&crate::paths::orca_pending_select(), p)
}

/// [`clear_at`] at the canonical path.
pub fn clear() -> io::Result<()> {
    clear_at(&crate::paths::orca_pending_select())
}

/// Does a pending-select file exist? (Cheap check for the launch path's
/// "spawn a detached `orca sync`" decision.)
pub fn exists() -> bool {
    crate::paths::orca_pending_select().is_file()
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn p(prior: Option<&str>) -> PendingSelect {
        PendingSelect::new("acct-2", "work", 1_000_000, prior)
    }

    #[test]
    fn verdict_branches() {
        let now = 1_000_000 + 60;
        assert_eq!(
            verdict(&p(Some("acct-1")), Some("acct-1"), now),
            Verdict::Valid
        );
        assert_eq!(
            verdict(&p(None), None, now),
            Verdict::Valid,
            "prior System default"
        );
        assert_eq!(
            verdict(&p(Some("acct-1")), Some("acct-3"), now),
            Verdict::Superseded
        );
        assert_eq!(
            verdict(&p(Some("acct-1")), None, now),
            Verdict::Superseded,
            "Orca went to System default meanwhile"
        );
        assert_eq!(
            verdict(&p(Some("acct-1")), Some("acct-2"), now),
            Verdict::AlreadyApplied
        );
        assert_eq!(
            verdict(&p(Some("acct-1")), Some("acct-1"), 1_000_000 + MAX_AGE_SECS),
            Verdict::Expired
        );
        assert_eq!(
            verdict(&p(Some("acct-1")), Some("acct-1"), 1_000_000 - 3_600),
            Verdict::Expired,
            "queued in the future"
        );
    }

    #[test]
    fn roundtrip_with_wire_names_and_extra() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("orca-pending-select.json");
        assert_eq!(read_from(&path).unwrap(), None);
        write_to(&path, &p(Some("acct-1"))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for key in ["accountId", "profile", "queuedAt", "expectedPriorActiveId"] {
            assert!(text.contains(key), "{key} missing: {text}");
        }
        assert_eq!(read_from(&path).unwrap(), Some(p(Some("acct-1"))));

        std::fs::write(
            &path,
            r#"{"accountId":"a","profile":"work","queuedAt":5,"future":true}"#,
        )
        .unwrap();
        let got = read_from(&path).unwrap().unwrap();
        assert_eq!(got.expected_prior_active_id, None);
        assert!(got.extra.contains_key("future"));

        clear_at(&path).unwrap();
        clear_at(&path).unwrap();
        assert_eq!(read_from(&path).unwrap(), None);

        std::fs::write(&path, "nope").unwrap();
        assert!(read_from(&path).is_err());
    }
}
