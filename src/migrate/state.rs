//! `<state>/migration.json`, the migration's progress marker, and
//! `<state>/migrate.lock`.
//!
//! Every decision is recomputed from disk on each run; the marker caches
//! progress and keeps the facts that cannot be recomputed: the legacy
//! registry as first seen (so the dirs stay known once the registry is
//! gone), the cutover's time and boot id, and when each rate-limited note
//! was last shown. A marker that does not parse counts as absent, which
//! only costs a rerun of the probe.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::orca::fsx::{self, WriteOpts};

// ─── the marker ───────────────────────────────────────────────────────────────

/// The marker's file name in csm's state dir.
pub(crate) const MARKER: &str = "migration.json";

/// The try-lock's file name in csm's state dir.
pub(crate) const LOCK: &str = "migrate.lock";

/// The marker's format version.
pub(crate) const VERSION: u32 = 1;

/// A marker larger than this is not csm's: it counts as absent.
const MARKER_CAP: u64 = 256 * 1024;

/// How long a pending note stays quiet after it was shown (one per reason
/// per day).
pub(crate) const NOTE_EVERY_SECS: i64 = 24 * 3600;

/// How long the automatic runs leave a network-dependent step alone after
/// it failed on the network.
pub(crate) const NETWORK_BACKOFF_SECS: i64 = 15 * 60;

/// Where the migration stands. The order is the order the phases run in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    /// Every legacy login becomes an Orca account; Orca's active host
    /// account is a managed one.
    #[default]
    Adopt,
    /// The shared dirs and config move into `~/.claude`.
    Carry,
    /// D moves to `~/.claude`; the floor is cleared.
    Cutover,
    /// The legacy dirs are renamed `<dir>.retired`.
    Retire,
    /// Nothing legacy is left.
    Done,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Phase::Adopt => "adopt",
            Phase::Carry => "carry",
            Phase::Cutover => "cutover",
            Phase::Retire => "retire",
            Phase::Done => "done",
        }
    }
}

/// One registry entry as first seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapProfile {
    pub name: String,
    pub dir: PathBuf,
}

/// The legacy registry as first seen, grown by any profile a later run
/// finds registered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub profiles: Vec<SnapProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<String>,
    /// When the migration was first recorded (unix seconds; the start
    /// time of the csm that recorded it): a csm process started before it
    /// is the old binary ([`super::cutover::supervision`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<i64>,
}

impl Snapshot {
    /// The recorded legacy dirs.
    pub(crate) fn dirs(&self) -> Vec<PathBuf> {
        self.profiles.iter().map(|p| p.dir.clone()).collect()
    }
}

/// When the cutover cleared the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cutover {
    /// Unix seconds.
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
}

/// How a step last ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StepStatus {
    Done,
    /// Waits for a condition (Orca running or stopped, a lock, the network).
    Pending,
    Error,
}

/// One step's last outcome, keyed `<step>` or `<step>:<profile>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StepRecord {
    pub status: StepStatus,
    /// The error class (`network`, `rpc`, `refused`, `io`, …), never the
    /// message: a message may quote a path, never a secret, but the class
    /// is all a rerun needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `migration.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MigrationState {
    pub v: u32,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutover: Option<Cutover>,
    pub steps: BTreeMap<String, StepRecord>,
    /// Note key → unix seconds it was last shown.
    pub notes: BTreeMap<String, i64>,
    /// Unix seconds before which the automatic runs skip network steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<i64>,
    /// A line from a run nobody watched (after a spawn, in a pane), shown
    /// once at the next terminal launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Default for MigrationState {
    fn default() -> Self {
        MigrationState {
            v: VERSION,
            phase: Phase::Adopt,
            legacy: None,
            cutover: None,
            steps: BTreeMap::new(),
            notes: BTreeMap::new(),
            next_attempt_at: None,
            summary: None,
        }
    }
}

impl MigrationState {
    /// A marker for a machine with nothing legacy.
    pub(crate) fn done() -> MigrationState {
        MigrationState {
            phase: Phase::Done,
            ..MigrationState::default()
        }
    }

    /// Record a step's outcome.
    pub(crate) fn set_step(&mut self, key: &str, status: StepStatus, error: Option<&str>) {
        self.steps.insert(
            key.to_owned(),
            StepRecord {
                status,
                error: error.map(str::to_owned),
            },
        );
    }

    /// May an automatic run try a network step now? Pure.
    pub(crate) fn network_due(&self, now: i64) -> bool {
        self.next_attempt_at.is_none_or(|t| now >= t)
    }
}

/// Parse a marker. `None` when it does not parse or names a format this
/// csm does not know. Pure.
pub(crate) fn parse(bytes: &[u8]) -> Option<MigrationState> {
    let m: MigrationState = serde_json::from_slice(bytes).ok()?;
    (m.v == VERSION).then_some(m)
}

/// `<state>/migration.json`.
pub(crate) fn marker_path(state: &Path) -> PathBuf {
    state.join(MARKER)
}

/// Read the marker: one small read. Absent, unreadable or corrupt counts
/// as absent.
pub(crate) fn load(state: &Path) -> Option<MigrationState> {
    let bytes = crate::orca::read_capped(&marker_path(state), MARKER_CAP)
        .ok()
        .flatten()?;
    parse(bytes.as_bytes())
}

/// Write the marker (temp file, then rename).
pub(crate) fn save(state: &Path, m: &MigrationState) -> io::Result<()> {
    fsx::create_dir_all(state, 0o700)?;
    let mut bytes = serde_json::to_vec_pretty(m).map_err(io::Error::other)?;
    bytes.push(b'\n');
    fsx::write_atomic(&marker_path(state), &bytes, WriteOpts::PRIVATE)
}

// ─── rate-limited notes ───────────────────────────────────────────────────────

/// Where the NOTE line keeps its day while no marker exists.
pub(crate) const NOTE_FILE: &str = "migration.note";

/// How long a pending or error line's note key is kept after it was last
/// shown. Longer than [`NOTE_EVERY_SECS`], so pruning never makes a line
/// due early.
pub(crate) const NOTE_KEEP_SECS: i64 = 7 * 24 * 3600;

/// When the NOTE line was last shown on a machine without a marker.
pub(crate) fn load_note_file(state: &Path) -> Option<i64> {
    crate::orca::read_capped(&state.join(NOTE_FILE), 64)
        .ok()
        .flatten()?
        .trim()
        .parse()
        .ok()
}

/// Record when the NOTE line was shown on a machine without a marker.
pub(crate) fn save_note_file(state: &Path, at: i64) -> io::Result<()> {
    fsx::create_dir_all(state, 0o700)?;
    fsx::write_atomic(
        &state.join(NOTE_FILE),
        format!("{at}\n").as_bytes(),
        WriteOpts::PRIVATE,
    )
}

/// Drop the pending and error lines' note keys (`line:…`) last shown more
/// than [`NOTE_KEEP_SECS`] before `now`. Named notes stay. Pure.
pub(crate) fn prune_notes(notes: &mut BTreeMap<String, i64>, now: i64) {
    notes.retain(|k, t| !k.starts_with("line:") || now.saturating_sub(*t) < NOTE_KEEP_SECS);
}

/// Is note `key` due: never shown, or last shown at least a day before
/// `now` (unix seconds)? A clock that went back counts as due only once
/// the day has passed again. Pure.
pub(crate) fn note_due(notes: &BTreeMap<String, i64>, key: &str, now: i64) -> bool {
    match notes.get(key) {
        None => true,
        Some(&last) if last > now => false,
        Some(&last) => now - last >= NOTE_EVERY_SECS,
    }
}

// ─── migrate.lock ─────────────────────────────────────────────────────────────

/// A held `migrate.lock`, released on drop. Taken outside `switch.lock`
/// (the store, stash and `D` writers take that one inside).
#[derive(Debug)]
pub(crate) struct MigrateLock {
    file: File,
}

impl MigrateLock {
    /// Take `<state>/migrate.lock` without waiting: `Ok(None)` when another
    /// csm holds it (it is migrating; this run skips).
    pub(crate) fn try_acquire(state: &Path) -> io::Result<Option<MigrateLock>> {
        fsx::create_dir_all(state, 0o700)?;
        let path = state.join(LOCK);
        fsx::guard(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(MigrateLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

impl Drop for MigrateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_is_due_once_a_day() {
        let mut notes = BTreeMap::new();
        assert!(note_due(&notes, "pending", 1_000));
        notes.insert("pending".to_owned(), 1_000);
        assert!(!note_due(&notes, "pending", 1_000));
        assert!(!note_due(&notes, "pending", 1_000 + NOTE_EVERY_SECS - 1));
        assert!(note_due(&notes, "pending", 1_000 + NOTE_EVERY_SECS));
        // Another reason has its own clock.
        assert!(note_due(&notes, "stale-pin", 1_000));
        // A clock that went back does not repeat the note.
        assert!(!note_due(&notes, "pending", 10));
    }

    #[test]
    fn marker_round_trips_and_a_corrupt_one_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        assert!(load(&state).is_none());
        let mut m = MigrationState {
            phase: Phase::Carry,
            legacy: Some(Snapshot {
                profiles: vec![SnapProfile {
                    name: "work".into(),
                    dir: PathBuf::from("/Users/example/.claude.work"),
                }],
                floor: Some("work".into()),
                seen_at: Some(1_700_000_000),
            }),
            ..MigrationState::default()
        };
        m.set_step("A1:work", StepStatus::Done, None);
        m.notes.insert("pending".into(), 5);
        save(&state, &m).unwrap();
        assert_eq!(load(&state), Some(m.clone()));
        assert_eq!(
            m.legacy.as_ref().unwrap().dirs(),
            vec![PathBuf::from("/Users/example/.claude.work")]
        );
        std::fs::write(marker_path(&state), b"{not json").unwrap();
        assert!(load(&state).is_none());
        std::fs::write(marker_path(&state), br#"{"v":99,"phase":"done"}"#).unwrap();
        assert!(load(&state).is_none());
        // Unknown keys from a later csm and missing keys both parse.
        std::fs::write(marker_path(&state), br#"{"v":1,"phase":"done","x":1}"#).unwrap();
        assert_eq!(load(&state).unwrap().phase, Phase::Done);
    }

    #[test]
    fn network_backoff_gates_only_until_its_time() {
        let mut m = MigrationState::default();
        assert!(m.network_due(0));
        m.next_attempt_at = Some(100);
        assert!(!m.network_due(99));
        assert!(m.network_due(100));
    }

    #[test]
    fn migrate_lock_is_a_try_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let held = MigrateLock::try_acquire(&state).unwrap();
        assert!(held.is_some());
        assert!(MigrateLock::try_acquire(&state).unwrap().is_none());
        drop(held);
        assert!(MigrateLock::try_acquire(&state).unwrap().is_some());
    }
}
