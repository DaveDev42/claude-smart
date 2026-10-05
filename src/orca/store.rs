//! `orca-data.json`: load, typed views of the Claude account keys, and the
//! pure patch.
//!
//! Orca writes the file with a compact `JSON.stringify` (no indentation, no
//! trailing newline, `schemaVersion` first), reads it once at start, and
//! later rewrites its whole in-memory state after any mutation. csm parses
//! it with serde_json's `preserve_order` (key order) and
//! `arbitrary_precision` (number text) features, so an unmodified parse
//! re-serializes to the exact original bytes. That equality is the
//! round-trip gate: a file that does not round-trip (a hand edit, a format
//! csm does not understand) is readable but never patched.
//!
//! [`patch_settings`] changes only `settings.claudeManagedAccounts`,
//! `settings.activeClaudeManagedAccountId` and
//! `settings.activeClaudeManagedAccountIdsByRuntime`; every other byte of the
//! output equals the input, because serialization is deterministic and the
//! gate proved it reproduces the original. Encrypted fields and secret slots
//! are never read or touched.
//!
//! The store-write protocol ([`write_protocol`], design section 2, B3) is
//! the shell over the patch, run under `switch.lock` by the caller:
//! 1. L0 liveness check; Orca running means "do it over RPC" ([`StoreWrite::OrcaAtL0`]).
//! 2. Load, gate, build the new bytes, save csm's pre-image in its state
//!    dir (Orca's `.bak.*` rotation is never touched or extended), write
//!    the tmp file with Orca's name pattern and the existing mode, fsync.
//! 3. L1 immediately before the rename, then the mtime/size/inode re-check.
//!    Orca came up: the tmp is deleted and the store is untouched
//!    ([`StoreWrite::OrcaAtL1`]); the file changed: refuse.
//! 4. Rename, fsync the dir.
//! 5. L2, also comparing the lock and runtime-file fingerprint against L0.
//!    Orca appeared: it may hold either version in memory, so csm never
//!    touches the file again ([`StoreWrite::OrcaAtL2`]) and the caller
//!    redoes the operation over RPC ([`redo_over_rpc`]), which re-reads
//!    `accounts.list` and reissues only an operation whose effect is
//!    missing. No answer is [`RedoOutcome::Uncertain`].
//!
//! From Orca 1.4.214 on, a profile's store of record may be SQLite
//! (`profile-state.db` beside `orca-data.json`, which is then an export
//! pinned by a hash marker). The protocol refuses such a profile before it
//! reads and again before the rename ([`sqlite_gate`]); those writes go
//! through Orca's RPC only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use super::OrcaError;
use super::fsx::{self, TmpStyle, WriteOpts};
use super::live::{LiveMark, Liveness};
use super::record::{AccountRecord, ActiveIds, IdentityKey, d3, find_by_identity, parse_records};
use super::rpc::{self, ClaudeSnapshot, RpcError};
use super::userdata::DataFileChoice;

/// The only `schemaVersion` csm will patch.
pub const SCHEMA_VERSION: i64 = 1;

/// The three settings keys csm may change.
pub const KEY_ACCOUNTS: &str = "claudeManagedAccounts";
pub const KEY_ACTIVE_ID: &str = "activeClaudeManagedAccountId";
pub const KEY_ACTIVE_BY_RUNTIME: &str = "activeClaudeManagedAccountIdsByRuntime";
const PATCH_KEYS: [&str; 3] = [KEY_ACCOUNTS, KEY_ACTIVE_ID, KEY_ACTIVE_BY_RUNTIME];

/// Cap on the store file.
const STORE_CAP: u64 = 256 * 1024 * 1024;

// ─── errors ───────────────────────────────────────────────────────────────────

/// Why a store could not be read or patched. Never quotes content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("orca-data.json is not valid JSON")]
    NotJson,
    #[error("orca-data.json is not a JSON object")]
    NotObject,
    #[error("orca-data.json does not re-serialize to its exact bytes; refusing to patch")]
    RoundTrip,
    #[error("orca-data.json has schemaVersion {0:?}, csm patches only {SCHEMA_VERSION}")]
    Schema(Option<String>),
    #[error("orca-data.json has no settings object")]
    NoSettings,
    #[error("orca-data.json account data: {0}")]
    Shape(String),
    #[error("patch rejected: {0}")]
    BadPatch(String),
}

// ─── file load ────────────────────────────────────────────────────────────────

/// Identity of a file version, for the write protocol's re-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime: Option<SystemTime>,
    pub ino: Option<u64>,
    pub mode: Option<u32>,
}

impl FileStamp {
    pub fn of(meta: &std::fs::Metadata) -> FileStamp {
        #[cfg(unix)]
        let (ino, mode) = {
            use std::os::unix::fs::MetadataExt;
            (Some(meta.ino()), Some(meta.mode()))
        };
        #[cfg(not(unix))]
        let (ino, mode) = (None, None);
        FileStamp {
            len: meta.len(),
            mtime: meta.modified().ok(),
            ino,
            mode,
        }
    }
}

/// A loaded store file. `bytes` is the exact file content.
#[derive(Clone)]
pub struct StoreFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub stamp: FileStamp,
    /// Read from the legacy root file (Orca migrates it at start); never
    /// patched.
    pub legacy: bool,
}

impl std::fmt::Debug for StoreFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreFile")
            .field("path", &self.path)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("stamp", &self.stamp)
            .field("legacy", &self.legacy)
            .finish()
    }
}

/// Load `path`; `Ok(None)` when absent.
pub fn load(path: &Path) -> Result<Option<StoreFile>, OrcaError> {
    let io = |e| OrcaError::io("cannot read", path, e);
    let Some(bytes) = super::read_capped_bytes(path, STORE_CAP).map_err(io)? else {
        return Ok(None);
    };
    let stamp = std::fs::metadata(path)
        .map(|m| FileStamp::of(&m))
        .map_err(io)?;
    Ok(Some(StoreFile {
        path: path.to_path_buf(),
        bytes,
        stamp,
        legacy: false,
    }))
}

/// Load the store Orca would load for `choice`: the profile file, else (for
/// `local-default`) the legacy root file, marked `legacy`.
pub fn load_choice(choice: &DataFileChoice) -> Result<Option<StoreFile>, OrcaError> {
    if choice.index_unreadable {
        return Err(OrcaError::Refused(INDEX_REFUSAL.into()));
    }
    if let Some(f) = load(&choice.path)? {
        return Ok(Some(f));
    }
    match &choice.legacy_root {
        Some(legacy) => Ok(load(legacy)?.map(|f| StoreFile { legacy: true, ..f })),
        None => Ok(None),
    }
}

// ─── SQLite store of record (Orca 1.4.214+) ─────────────────────────────────

/// Table and domain of the `settings` document inside `profile-state.db`.
const STATE_TABLE_QUERY: &str =
    "SELECT payload FROM profile_state_documents WHERE domain = 'settings'";

/// Read the `settings` document of `profile-state.db` read-only.
///
/// The connection is `SQLITE_OPEN_READ_ONLY` (the `mode=ro` of a URI open),
/// never `immutable`: a stopped Orca leaves its newest rows in the `-wal`
/// file and only a normal read-only connection replays them. csm never
/// writes the database. `None` when the file is absent or has no `settings`
/// document; an unreadable or malformed database is an error.
pub fn load_state_db_settings(db: &Path) -> Result<Option<Value>, OrcaError> {
    use rusqlite::{Connection, OpenFlags, types::ValueRef};
    match std::fs::symlink_metadata(db) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OrcaError::io("cannot stat", db, e)),
    }
    let bad = |what: &str| OrcaError::Refused(format!("{}: {what}", STATE_DB_NAME));
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| bad("cannot open read-only"))?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let mut stmt = match conn.prepare(STATE_TABLE_QUERY) {
        Ok(s) => s,
        // No such table (an empty file): nothing to read. Anything else
        // (not a database, I/O error) is an error.
        Err(e) if e.to_string().contains("no such table") => return Ok(None),
        Err(_) => return Err(bad("cannot read settings")),
    };
    let mut rows = stmt.query([]).map_err(|_| bad("cannot read settings"))?;
    let Some(row) = rows.next().map_err(|_| bad("cannot read settings"))? else {
        return Ok(None);
    };
    let payload: Vec<u8> = match row.get_ref(0).map_err(|_| bad("cannot read settings"))? {
        ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
        _ => return Err(bad("settings payload is not text")),
    };
    if payload.len() as u64 > STORE_CAP {
        return Err(bad("settings payload too large"));
    }
    let v: Value = serde_json::from_slice(&payload).map_err(|_| bad("settings is not JSON"))?;
    if !v.is_object() {
        return Err(bad("settings is not a JSON object"));
    }
    Ok(Some(v))
}

const STATE_DB_NAME: &str = super::userdata::STATE_DB;

/// The account view csm READS for `choice`: the SQLite store of record when
/// the profile has `profile-state.db` (Orca 1.4.214+, where `orca-data.json`
/// is only an export written at quit and may be stale or absent), else the
/// JSON file. Read-only; every write path still goes through
/// [`write_protocol`] and refuses a SQLite profile.
pub fn load_view_choice(choice: &DataFileChoice) -> Result<Option<(StoreView, bool)>, OrcaError> {
    if choice.index_unreadable {
        return Err(OrcaError::Refused(INDEX_REFUSAL.into()));
    }
    let mut db_error = None;
    if let Some(db) = choice.state_db_files().first() {
        match load_state_db_settings(db) {
            Ok(Some(settings)) => {
                let mut doc = Map::new();
                doc.insert("settings".into(), settings);
                return StoreView::from_value(&Value::Object(doc), false)
                    .map(|v| Some((v, false)))
                    .map_err(|e| OrcaError::Refused(e.to_string()));
            }
            Ok(None) => {}
            Err(e) => db_error = Some(e),
        }
    }
    match load_choice(choice)? {
        Some(f) => StoreView::from_bytes(&f.bytes)
            .map(|v| Some((v, f.legacy)))
            .map_err(|e| OrcaError::Refused(e.to_string())),
        None => db_error.map_or(Ok(None), Err),
    }
}

// ─── parse + gate ─────────────────────────────────────────────────────────────

/// Parse `bytes` as a JSON object with order and number text preserved.
pub fn parse(bytes: &[u8]) -> Result<Value, StoreError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| StoreError::NotJson)?;
    if !v.is_object() {
        return Err(StoreError::NotObject);
    }
    Ok(v)
}

/// Does `v` re-serialize to exactly `bytes`? Pure.
pub fn round_trips(v: &Value, bytes: &[u8]) -> bool {
    serde_json::to_vec(v).is_ok_and(|out| out == bytes)
}

/// [`parse`] plus the round-trip gate.
pub fn parse_gated(bytes: &[u8]) -> Result<Value, StoreError> {
    let v = parse(bytes)?;
    if !round_trips(&v, bytes) {
        return Err(StoreError::RoundTrip);
    }
    Ok(v)
}

fn schema_text(v: &Value) -> Option<String> {
    v.get("schemaVersion").map(|s| match s {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn schema_is_supported(v: &Value) -> bool {
    v.get("schemaVersion").and_then(Value::as_i64) == Some(SCHEMA_VERSION)
}

// ─── typed view ───────────────────────────────────────────────────────────────

/// The account-related parts of a store.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreView {
    /// `schemaVersion` as text, `None` when absent.
    pub schema_version: Option<String>,
    pub accounts: Vec<AccountRecord>,
    /// The same entries as stored (key order and number text kept), for a
    /// patch that must carry the other records over unchanged.
    pub accounts_raw: Vec<Value>,
    /// d3 over `settings` (not normalized).
    pub active: ActiveIds,
    /// The raw `activeClaudeManagedAccountId`.
    pub active_id_raw: Option<String>,
    /// The file passed the round-trip gate.
    pub round_trip_ok: bool,
    /// `settings` exists as an object.
    pub has_settings: bool,
}

impl StoreView {
    /// Parse the account keys of `bytes`. A file that fails the round-trip
    /// gate still reads; [`StoreView::writable`] then refuses.
    pub fn from_bytes(bytes: &[u8]) -> Result<StoreView, StoreError> {
        let v = parse(bytes)?;
        let round_trip_ok = round_trips(&v, bytes);
        StoreView::from_value(&v, round_trip_ok)
    }

    pub fn from_value(v: &Value, round_trip_ok: bool) -> Result<StoreView, StoreError> {
        let settings = match v.get("settings") {
            None | Some(Value::Null) => None,
            Some(Value::Object(s)) => Some(s),
            Some(_) => return Err(StoreError::NoSettings),
        };
        let empty = Map::new();
        let s = settings.unwrap_or(&empty);
        let accounts = parse_records(s.get(KEY_ACCOUNTS)).map_err(StoreError::Shape)?;
        let accounts_raw = match s.get(KEY_ACCOUNTS) {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let active = d3(s).map_err(StoreError::Shape)?;
        let active_id_raw = s
            .get(KEY_ACTIVE_ID)
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(StoreView {
            schema_version: schema_text(v),
            accounts,
            accounts_raw,
            active,
            active_id_raw,
            round_trip_ok,
            has_settings: settings.is_some(),
        })
    }

    /// The effective host active id (d3.host).
    pub fn active_host_id(&self) -> Option<&str> {
        self.active.host.as_deref()
    }

    pub fn account(&self, id: &str) -> Option<&AccountRecord> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// May csm patch this store? Round-trip gate, schemaVersion 1, and a
    /// settings object. (The Orca-version gate lives in [`super::version`].)
    pub fn writable(&self) -> Result<(), StoreError> {
        if !self.round_trip_ok {
            return Err(StoreError::RoundTrip);
        }
        if self.schema_version.as_deref() != Some("1") {
            return Err(StoreError::Schema(self.schema_version.clone()));
        }
        if !self.has_settings {
            return Err(StoreError::NoSettings);
        }
        Ok(())
    }
}

// ─── patch ────────────────────────────────────────────────────────────────────

/// A change to the three account keys. `None` leaves a key untouched.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    /// The full new `claudeManagedAccounts` array.
    pub accounts: Option<Vec<Value>>,
    /// The new `activeClaudeManagedAccountId` (`Some(None)` writes null).
    pub active_id: Option<Option<String>>,
    /// The new `activeClaudeManagedAccountIdsByRuntime`.
    pub active_by_runtime: Option<ActiveIds>,
}

impl Patch {
    fn is_empty(&self) -> bool {
        self.accounts.is_none() && self.active_id.is_none() && self.active_by_runtime.is_none()
    }
}

fn check_accounts(accounts: &[Value]) -> Result<(), StoreError> {
    let mut ids = std::collections::HashSet::new();
    for a in accounts {
        let r = AccountRecord::from_value(a).map_err(StoreError::BadPatch)?;
        if !ids.insert(r.id) {
            return Err(StoreError::BadPatch("duplicate account id".into()));
        }
    }
    Ok(())
}

fn strip_patch_keys(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(s) = v.get_mut("settings").and_then(Value::as_object_mut) {
        for k in PATCH_KEYS {
            s.shift_remove(k);
        }
    }
    v
}

/// Apply `patch` to the store `bytes`. Pure. Refuses unless the input passes
/// the round-trip gate, has `schemaVersion` 1 and a `settings` object.
/// Existing keys keep their position; a key the store lacks is appended to
/// `settings`. The output is compact JSON with no trailing newline, and
/// differs from the input only inside the three account keys.
pub fn patch_settings(bytes: &[u8], patch: &Patch) -> Result<Vec<u8>, StoreError> {
    let original = parse_gated(bytes)?;
    if !schema_is_supported(&original) {
        return Err(StoreError::Schema(schema_text(&original)));
    }
    if patch.is_empty() {
        return Ok(bytes.to_vec());
    }
    if let Some(a) = &patch.accounts {
        check_accounts(a)?;
    }
    let mut v = original.clone();
    let settings = match v.get_mut("settings") {
        Some(Value::Object(s)) => s,
        _ => return Err(StoreError::NoSettings),
    };
    if let Some(a) = &patch.accounts {
        settings.insert(KEY_ACCOUNTS.into(), Value::Array(a.clone()));
    }
    if let Some(id) = &patch.active_id {
        settings.insert(
            KEY_ACTIVE_ID.into(),
            id.clone().map_or(Value::Null, Value::String),
        );
    }
    if let Some(by) = &patch.active_by_runtime {
        settings.insert(KEY_ACTIVE_BY_RUNTIME.into(), by.to_value());
    }
    let out = serde_json::to_vec(&v).map_err(|_| StoreError::NotJson)?;

    // Belt and braces: the result parses, round-trips, and equals the input
    // once the three keys are removed from both.
    let reparsed = parse_gated(&out)?;
    if strip_patch_keys(&reparsed) != strip_patch_keys(&original) {
        return Err(StoreError::BadPatch(
            "patch changed a key outside the account keys".into(),
        ));
    }
    StoreView::from_value(&reparsed, true)?;
    Ok(out)
}

// ─── the store-write protocol ─────────────────────────────────────────────────

/// The minimal store csm creates when none exists (design section 2).
pub const MINIMAL_STORE: &[u8] = br#"{"schemaVersion":1,"settings":{}}"#;

/// How many pre-images csm keeps.
const PREIMAGES_KEPT: usize = 5;

/// What a protocol write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreWrite {
    /// L2 passed: the new bytes are on disk and no Orca saw the old ones.
    Written,
    /// The patch changed nothing; nothing was written.
    Unchanged,
    /// Orca ran at L0: nothing was written. Do it over RPC.
    OrcaAtL0,
    /// Orca came up before the rename: the tmp was deleted and the store is
    /// untouched. Redo over RPC.
    OrcaAtL1,
    /// Orca came up (or its instance changed) around the rename: the file
    /// holds csm's bytes but Orca may hold either version in memory. csm
    /// never touches the file again; the caller redoes over RPC.
    OrcaAtL2,
}

/// The pre-image dir in csm's state dir.
pub fn preimage_dir(state: &Path) -> PathBuf {
    state.join("preimages")
}

/// Save `bytes` as the newest store pre-image (0600) and prune old ones.
fn save_preimage(state: &Path, bytes: &[u8]) -> Result<PathBuf, OrcaError> {
    let dir = preimage_dir(state);
    fsx::create_dir_all(&dir, 0o700).map_err(|e| OrcaError::io("cannot create", &dir, e))?;
    let path = dir.join(format!(
        "orca-data.{}.{}.json",
        super::now_ms(),
        std::process::id()
    ));
    fsx::write_atomic(&path, bytes, WriteOpts::PRIVATE)
        .map_err(|e| OrcaError::io("cannot write", &path, e))?;
    let mut old: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("orca-data.") && n.ends_with(".json"))
                })
                .collect()
        })
        .unwrap_or_default();
    old.sort();
    while old.len() > PREIMAGES_KEPT {
        let p = old.remove(0);
        let _ = fsx::remove_file(&p);
    }
    Ok(path)
}

fn stamp_now(path: &Path) -> Result<Option<FileStamp>, OrcaError> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some(FileStamp::of(&m))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OrcaError::io("cannot stat", path, e)),
    }
}

/// The refusal for a profile index that exists but does not parse.
pub const INDEX_REFUSAL: &str = "Orca's profile index (orca-profile-index.json) is unreadable; \
     Orca will not start until it is repaired, and csm cannot tell which profile's store is Orca's";

/// The message every offline write refuses with when the profile keeps its
/// state in SQLite (Orca 1.4.214 and later).
pub const SQLITE_REFUSAL: &str = "Orca keeps this profile's state in SQLite (profile-state.db); \
     orca-data.json is only its export, so csm changes it only through Orca: start Orca";

/// Refuse an offline write to a profile whose store of record is SQLite
/// ([`DataFileChoice::has_state_db`]).
pub fn sqlite_gate(choice: &DataFileChoice) -> Result<(), OrcaError> {
    if choice.has_state_db() {
        return Err(OrcaError::Refused(SQLITE_REFUSAL.into()));
    }
    Ok(())
}

/// Run one store mutation through the protocol. `build` gets the current
/// view and returns the patch to apply; it runs after L0 on the bytes that
/// will be replaced. `l0` is the caller's own L0 mark, when it took one
/// earlier (the switch's step 1): the protocol then also requires that
/// nothing about Orca's instance changed since. A missing store is created
/// as [`MINIMAL_STORE`] only with `allow_create`.
pub fn write_protocol(
    choice: &DataFileChoice,
    allow_create: bool,
    live: &dyn Liveness,
    l0: Option<&LiveMark>,
    state: &Path,
    build: &mut dyn FnMut(&StoreView) -> Result<Patch, OrcaError>,
) -> Result<StoreWrite, OrcaError> {
    // 1. L0
    let m0 = live.mark();
    let base = l0.cloned().unwrap_or_else(|| m0.clone());
    if !m0.still_clear_of(&base) {
        return Ok(StoreWrite::OrcaAtL0);
    }

    // 2. load, gate, build. A SQLite-backed profile is never written, and
    // never created beside its database.
    sqlite_gate(choice)?;
    let loaded = load_choice(choice)?;
    let (path, bytes, stamp) = match loaded {
        Some(f) if f.legacy => {
            return Err(OrcaError::Refused(
                "Orca's store is still the legacy root file; start Orca once to migrate it".into(),
            ));
        }
        Some(f) => (f.path, f.bytes, Some(f.stamp)),
        None if allow_create => {
            if let Some(evidence) = choice.recovery_evidence() {
                return Err(OrcaError::Refused(format!(
                    "Orca's store {} is missing but its recovery files remain ({evidence}); \
                     start Orca so it restores them, or restore a backup, before csm adds to it",
                    choice.path.display()
                )));
            }
            (choice.path.clone(), MINIMAL_STORE.to_vec(), None)
        }
        None => {
            return Err(OrcaError::Refused(format!(
                "no Orca store at {}",
                choice.path.display()
            )));
        }
    };
    let view = StoreView::from_bytes(&bytes).map_err(|e| OrcaError::Refused(e.to_string()))?;
    view.writable()
        .map_err(|e| OrcaError::Refused(e.to_string()))?;
    let patch = build(&view)?;
    let out = patch_settings(&bytes, &patch).map_err(|e| OrcaError::Refused(e.to_string()))?;
    if stamp.is_some() && out == bytes {
        return Ok(StoreWrite::Unchanged);
    }
    if stamp.is_some() {
        save_preimage(state, &bytes)?;
    } else if let Some(dir) = path.parent() {
        fsx::create_dir_all(dir, 0o700).map_err(|e| OrcaError::io("cannot create", dir, e))?;
    }
    let mode = stamp
        .as_ref()
        .and_then(|s| s.mode)
        .map(|m| m & 0o7777)
        .unwrap_or(0o600);
    let tmp = fsx::write_tmp(
        &path,
        &out,
        WriteOpts {
            mode,
            tmp: TmpStyle::Store,
            durable: true,
        },
    )
    .map_err(|e| OrcaError::io("cannot write a temp file beside", &path, e))?;

    // 3. L1, then the stamp re-check
    crate::e2e::point("store-L1");
    let m1 = live.mark();
    if !m1.still_clear_of(&base) {
        tmp.discard();
        return Ok(StoreWrite::OrcaAtL1);
    }
    let now = stamp_now(&path)?;
    let same = match (&stamp, &now) {
        (Some(a), Some(b)) => a.len == b.len && a.mtime == b.mtime && a.ino == b.ino,
        (None, None) => true,
        _ => false,
    };
    if !same {
        tmp.discard();
        return Err(OrcaError::Refused(
            "orca-data.json changed since csm read it; nothing was written".into(),
        ));
    }
    if let Err(e) = sqlite_gate(choice) {
        tmp.discard();
        return Err(e);
    }

    // 4. rename
    tmp.commit()
        .map_err(|e| OrcaError::io("cannot replace", &path, e))?;

    // 5. L2
    crate::e2e::point("store-L2");
    let m2 = live.mark();
    if !m2.still_clear_of(&base) {
        return Ok(StoreWrite::OrcaAtL2);
    }
    Ok(StoreWrite::Written)
}

// ─── redo over RPC ────────────────────────────────────────────────────────────

/// An operation to redo over RPC after Orca appeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedoOp {
    /// `selectClaude`: effect = the host active id is `id`.
    Select { id: String },
    /// `addClaudeFromConfigDir`: effect = an account with `identity` exists.
    Add {
        config_dir: String,
        previous_legacy_sha256: Option<String>,
        identity: IdentityKey,
    },
    /// `removeClaude`: effect = no account `id`.
    Remove { id: String },
}

/// Is `op`'s effect visible in `snap`? Pure.
pub fn effect_present(op: &RedoOp, snap: &ClaudeSnapshot) -> bool {
    match op {
        RedoOp::Select { id } => snap.active_by_runtime.host.as_deref() == Some(id.as_str()),
        RedoOp::Add { identity, .. } => find_by_identity(&snap.accounts, identity).is_some(),
        RedoOp::Remove { id } => !snap.accounts.iter().any(|a| &a.id == id),
    }
}

/// How a redo ended.
#[derive(Debug, Clone, PartialEq)]
pub enum RedoOutcome {
    /// Orca already shows the effect (its in-memory state has csm's write,
    /// or it redid the same thing).
    AlreadyDone(ClaudeSnapshot),
    /// csm reissued the operation and Orca answered.
    Reissued(Value),
    /// Orca answered with a refusal: the operation did not happen.
    Failed(String),
    /// No answer, or an answer that may have been lost: csm cannot tell.
    /// `accounts doctor` reconciles later.
    Uncertain(String),
}

/// Timing of a redo.
#[derive(Debug, Clone, Copy)]
pub struct RedoOpts {
    /// How long to wait for Orca's socket to answer `accounts.list`.
    pub wait: Duration,
    pub poll: Duration,
}

/// How long a redo waits for Orca's socket to answer (design §2 step 5).
pub const REDO_WAIT: Duration = Duration::from_secs(15);

impl Default for RedoOpts {
    fn default() -> Self {
        RedoOpts {
            wait: REDO_WAIT,
            poll: Duration::from_millis(250),
        }
    }
}

/// Wait for Orca's socket, read `accounts.list{refreshUsage:false}`, and
/// reissue `op` only when its effect is missing.
pub fn redo_over_rpc(user_data: &Path, op: &RedoOp, opts: RedoOpts) -> RedoOutcome {
    let deadline = Instant::now() + opts.wait;
    let snap = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rpc::accounts_list(
            user_data,
            false,
            rpc::LIST_TIMEOUT.min(left.max(Duration::from_millis(100))),
        ) {
            Ok(s) => break s.claude,
            Err(e) if Instant::now() >= deadline => {
                return RedoOutcome::Uncertain(format!("Orca did not answer: {e}"));
            }
            Err(_) => std::thread::sleep(opts.poll),
        }
    };
    if effect_present(op, &snap) {
        return RedoOutcome::AlreadyDone(snap);
    }
    let r = match op {
        RedoOp::Select { id } => {
            rpc::select_claude(user_data, id, rpc::SELECT_TIMEOUT).map(|_| Value::Null)
        }
        RedoOp::Add {
            config_dir,
            previous_legacy_sha256,
            ..
        } => rpc::add_claude_from_config_dir(
            user_data,
            config_dir,
            previous_legacy_sha256.as_deref(),
            rpc::ADD_TIMEOUT,
        ),
        RedoOp::Remove { id } => rpc::remove_claude(user_data, id, rpc::ADD_TIMEOUT),
    };
    match r {
        Ok(v) => RedoOutcome::Reissued(v),
        Err(e) => classify_redo_error(&e),
    }
}

/// A failed reissue: a definite refusal, or uncertain. Pure.
pub fn classify_redo_error(e: &RpcError) -> RedoOutcome {
    if e.maybe_delivered() {
        RedoOutcome::Uncertain(e.to_string())
    } else {
        RedoOutcome::Failed(e.to_string())
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::record::{RuntimeTarget, p3};

    /// A store as Orca's compact `JSON.stringify` writes it: key order that
    /// is not sorted, escapes, unicode, big and float numbers, a secret-slot
    /// sentinel, and the account keys in the middle of `settings`.
    const STORE: &str = concat!(
        r#"{"schemaVersion":1,"repos":[{"id":"r1","path":"/Users/example/src/app","n":12345678901234567890123}],"#,
        r#""settings":{"theme":"dark","opencodeSessionCookie":"orca-secret-slot-00000000-0000-4000-8000-000000000000","#,
        r#""claudeManagedAccounts":[{"id":"id-a","email":"alice@example.com","managedAuthPath":"/Users/example/Library/Application Support/orca/claude-accounts/id-a/auth","managedAuthRuntime":"host","wslDistro":null,"wslLinuxAuthPath":null,"authMethod":"subscription-oauth","organizationUuid":null,"organizationName":null,"createdAt":1700000000000,"updatedAt":1700000000000,"lastAuthenticatedAt":1700000000000}],"#,
        r#""zoom":1.25,"activeClaudeManagedAccountId":"id-a","label":"Café \"quoted\" \\ tab\t line\n ctl\u001f snow☃ 😀","#,
        r#""activeClaudeManagedAccountIdsByRuntime":{"host":"id-a","wsl":{}},"big":1e+21,"neg":-0.5},"#,
        r#""ui":{"sidebar":true}}"#
    );

    fn store() -> Vec<u8> {
        // Non-ASCII stays raw (as JS writes it); only control chars, quotes
        // and backslashes are JSON-escaped.
        STORE.as_bytes().to_vec()
    }

    #[test]
    fn a_js_style_store_round_trips_byte_exact() {
        let b = store();
        let v = parse_gated(&b).unwrap();
        assert_eq!(serde_json::to_vec(&v).unwrap(), b);
    }

    #[test]
    fn non_round_tripping_files_are_refused() {
        for bad in [
            &br#"{"schemaVersion":1, "settings":{}}"#[..], // a space
            &b"{\"schemaVersion\":1,\"settings\":{}}\n"[..], // trailing newline
            &br#"{"schemaVersion":1,"settings":{"a":"\u00e9"}}"#[..], // escaped non-ASCII
            &br#"{"schemaVersion":1,"settings":{"a":1,"a":2}}"#[..], // duplicate key
            &b"{\n  \"schemaVersion\": 1,\n  \"settings\": {}\n}"[..], // indented
        ] {
            assert_eq!(
                parse_gated(bad),
                Err(StoreError::RoundTrip),
                "{}",
                String::from_utf8_lossy(bad)
            );
            assert_eq!(
                patch_settings(
                    bad,
                    &Patch {
                        active_id: Some(Some("x".into())),
                        ..Patch::default()
                    }
                ),
                Err(StoreError::RoundTrip)
            );
            // Still readable.
            let view = StoreView::from_bytes(bad).unwrap();
            assert_eq!(view.writable(), Err(StoreError::RoundTrip));
        }
        assert_eq!(parse_gated(b"[]"), Err(StoreError::NotObject));
        assert_eq!(parse_gated(b"{not json"), Err(StoreError::NotJson));
    }

    #[test]
    fn view_reads_the_account_keys() {
        let v = StoreView::from_bytes(&store()).unwrap();
        assert_eq!(v.schema_version.as_deref(), Some("1"));
        assert!(v.round_trip_ok && v.has_settings);
        assert_eq!(v.accounts.len(), 1);
        assert_eq!(v.accounts[0].email.as_deref(), Some("alice@example.com"));
        assert_eq!(v.active_host_id(), Some("id-a"));
        assert_eq!(v.writable(), Ok(()));
        assert!(v.account("id-a").is_some() && v.account("nope").is_none());
    }

    #[test]
    fn view_without_settings_is_empty_and_not_writable() {
        let v = StoreView::from_bytes(br#"{"schemaVersion":1}"#).unwrap();
        assert!(v.accounts.is_empty() && v.active.host.is_none());
        assert_eq!(v.writable(), Err(StoreError::NoSettings));
        let v = StoreView::from_bytes(br#"{"schemaVersion":2,"settings":{}}"#).unwrap();
        assert_eq!(v.writable(), Err(StoreError::Schema(Some("2".into()))));
    }

    fn second_account() -> Value {
        crate::orca::record::new_record(
            &crate::orca::record::NewRecord {
                id: "id-b",
                email: "bob@example.com",
                managed_auth_path: "/Users/example/Library/Application Support/orca/claude-accounts/id-b/auth",
                organization_uuid: Some("org-acme"),
                organization_name: Some("Acme"),
            },
            1_700_000_100_000,
        )
    }

    #[test]
    fn patch_changes_only_the_three_keys_and_keeps_their_positions() {
        let b = store();
        let orig = parse_gated(&b).unwrap();
        let mut accounts = orig["settings"][KEY_ACCOUNTS].as_array().unwrap().clone();
        accounts.push(second_account());
        let view = StoreView::from_bytes(&b).unwrap();
        let active = p3(&view.active, Some("id-b"), &RuntimeTarget::Host);
        let patch = Patch {
            accounts: Some(accounts),
            active_id: Some(Some("id-b".into())),
            active_by_runtime: Some(active),
        };
        let out = patch_settings(&b, &patch).unwrap();
        let out_s = String::from_utf8(out.clone()).unwrap();

        // Everything before the accounts key is byte-identical…
        let start = STORE.find(r#""claudeManagedAccounts""#).unwrap();
        assert_eq!(&out_s[..start], &STORE[..start]);
        // …and so is everything after the by-runtime key.
        let tail = r#","big":1e+21,"neg":-0.5},"ui":{"sidebar":true}}"#;
        assert!(STORE.ends_with(tail) && out_s.ends_with(tail));
        // The keys between keep their bytes too.
        assert!(
            out_s.contains(r#""zoom":1.25,"activeClaudeManagedAccountId":"id-b","label":"Café"#)
        );
        assert!(
            out_s.contains(r#""activeClaudeManagedAccountIdsByRuntime":{"host":"id-b","wsl":{}}"#)
        );
        assert!(!out_s.ends_with('\n'));

        let v = StoreView::from_bytes(&out).unwrap();
        assert_eq!(v.accounts.len(), 2);
        assert_eq!(v.active_host_id(), Some("id-b"));
        assert!(v.round_trip_ok);
    }

    #[test]
    fn patch_appends_missing_keys_at_the_end_of_settings() {
        let b = br#"{"schemaVersion":1,"settings":{"theme":"dark"},"ui":{}}"#;
        let patch = Patch {
            accounts: Some(vec![second_account()]),
            active_id: Some(None),
            active_by_runtime: Some(ActiveIds::default()),
        };
        let out = String::from_utf8(patch_settings(b, &patch).unwrap()).unwrap();
        assert!(out.starts_with(r#"{"schemaVersion":1,"settings":{"theme":"dark","claudeManagedAccounts":[{"id":"id-b""#), "{out}");
        assert!(out.ends_with(r#""activeClaudeManagedAccountId":null,"activeClaudeManagedAccountIdsByRuntime":{"host":null,"wsl":{}}},"ui":{}}"#), "{out}");
    }

    #[test]
    fn patch_refuses_bad_inputs() {
        let ok = store();
        let bad_schema = br#"{"schemaVersion":2,"settings":{}}"#;
        let p = Patch {
            active_id: Some(Some("x".into())),
            ..Patch::default()
        };
        assert_eq!(
            patch_settings(bad_schema, &p),
            Err(StoreError::Schema(Some("2".into())))
        );
        assert_eq!(
            patch_settings(br#"{"settings":{}}"#, &p),
            Err(StoreError::Schema(None))
        );
        assert_eq!(
            patch_settings(br#"{"schemaVersion":1}"#, &p),
            Err(StoreError::NoSettings)
        );
        let dup = Patch {
            accounts: Some(vec![second_account(), second_account()]),
            ..Patch::default()
        };
        assert!(matches!(
            patch_settings(&ok, &dup),
            Err(StoreError::BadPatch(_))
        ));
        let junk = Patch {
            accounts: Some(vec![serde_json::json!(1)]),
            ..Patch::default()
        };
        assert!(matches!(
            patch_settings(&ok, &junk),
            Err(StoreError::BadPatch(_))
        ));
        // An empty patch returns the input unchanged.
        assert_eq!(patch_settings(&ok, &Patch::default()).unwrap(), ok);
    }

    #[test]
    fn store_errors_never_quote_content() {
        let secret = br#"{"schemaVersion":1,"settings":{"x":"sk-ant-secret"} }"#;
        let e = patch_settings(
            secret,
            &Patch {
                active_id: Some(None),
                ..Patch::default()
            },
        )
        .unwrap_err();
        assert!(!e.to_string().contains("sk-ant"), "{e}");
        let e = parse(b"{\"sk-ant-secret\"").unwrap_err();
        assert!(!e.to_string().contains("sk-ant"), "{e}");
    }

    #[test]
    fn load_prefers_the_profile_file_then_the_legacy_root() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let choice = crate::orca::userdata::data_file(ud);
        assert!(load_choice(&choice).unwrap().is_none());
        std::fs::write(ud.join("orca-data.json"), store()).unwrap();
        let f = load_choice(&choice).unwrap().unwrap();
        assert!(f.legacy);
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(&choice.path, br#"{"schemaVersion":1,"settings":{}}"#).unwrap();
        let f = load_choice(&choice).unwrap().unwrap();
        assert!(!f.legacy);
        assert_eq!(f.stamp.len, f.bytes.len() as u64);
        assert!(
            !format!("{f:?}").contains("schemaVersion"),
            "Debug prints no content"
        );
    }

    // ─── SQLite store of record ───────────────────────────────────────────

    const DB_SETTINGS: &str = r#"{"claudeManagedAccounts":[{"id":"id-a","email":"alice@example.com","managedAuthRuntime":"host"},{"id":"id-b","email":"bob@example.com","managedAuthRuntime":"host"}],"activeClaudeManagedAccountId":"id-b","activeClaudeManagedAccountIdsByRuntime":{"host":"id-b","wsl":null},"localAccountRuntime":"host"}"#;

    #[test]
    fn a_sqlite_profile_reads_accounts_with_no_export_present() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        crate::orca::testsupport::write_state_db(ud, "local-default", DB_SETTINGS);
        let choice = crate::orca::userdata::data_file(ud);
        assert!(!choice.path.exists(), "no orca-data.json");
        assert!(choice.has_state_db());
        let (v, legacy) = load_view_choice(&choice).unwrap().unwrap();
        assert!(!legacy);
        let ids: Vec<_> = v.accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["id-a", "id-b"]);
        assert_eq!(v.active_host_id(), Some("id-b"));
        assert_eq!(v.active_id_raw.as_deref(), Some("id-b"));
    }

    #[test]
    fn the_database_wins_over_a_stale_export() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        crate::orca::testsupport::write_state_db(ud, "local-default", DB_SETTINGS);
        let choice = crate::orca::userdata::data_file(ud);
        std::fs::write(
            &choice.path,
            br#"{"schemaVersion":1,"settings":{"claudeManagedAccounts":[{"id":"id-old","email":"old@example.com","managedAuthRuntime":"host"}],"activeClaudeManagedAccountId":"id-old"}}"#,
        )
        .unwrap();
        let (v, _) = load_view_choice(&choice).unwrap().unwrap();
        assert_eq!(v.accounts.len(), 2);
        assert!(v.account("id-old").is_none());
        assert_eq!(v.active_host_id(), Some("id-b"));
    }

    #[test]
    fn an_empty_or_settingless_database_falls_back_to_the_export() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let choice = crate::orca::userdata::data_file(ud);
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(choice.path.with_file_name("profile-state.db"), b"").unwrap();
        assert!(load_view_choice(&choice).unwrap().is_none());
        std::fs::write(&choice.path, store()).unwrap();
        assert!(load_view_choice(&choice).unwrap().is_some());
    }

    #[test]
    fn a_garbage_database_without_an_export_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let choice = crate::orca::userdata::data_file(dir.path());
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(
            choice.path.with_file_name("profile-state.db"),
            b"this is not a sqlite database at all, just text padding.....",
        )
        .unwrap();
        assert!(load_view_choice(&choice).is_err());
    }

    #[test]
    fn reading_the_database_never_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let db = crate::orca::testsupport::write_state_db(ud, "local-default", DB_SETTINGS);
        let before = std::fs::read(&db).unwrap();
        let choice = crate::orca::userdata::data_file(ud);
        load_view_choice(&choice).unwrap().unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }

    // ─── the write protocol ───────────────────────────────────────────────

    use crate::orca::live::LiveMark;
    #[cfg(unix)]
    use crate::orca::testsupport::{FakeOrca, OrcaModel, model_handler, running_mark};
    use crate::orca::testsupport::{ScriptedLiveness, record_json, write_store};

    struct Fx {
        _dir: tempfile::TempDir,
        state: PathBuf,
        choice: DataFileChoice,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let state = dir.path().join("state");
        let accounts = vec![
            record_json(&ud, "id-a", "alice@example.com", None),
            record_json(&ud, "id-b", "bob@example.com", Some("org-1")),
        ];
        let choice = write_store(&ud, &accounts, Some("id-a"));
        Fx {
            _dir: dir,
            state,
            choice,
        }
    }

    fn select_b(v: &StoreView) -> Result<Patch, OrcaError> {
        Ok(Patch {
            active_id: Some(Some("id-b".into())),
            active_by_runtime: Some(p3(&v.active, Some("id-b"), &RuntimeTarget::Host)),
            ..Patch::default()
        })
    }

    fn dir_names(p: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn protocol_writes_with_three_checks_a_preimage_and_the_mode_kept() {
        let f = fx();
        #[cfg(unix)]
        crate::orca::fsx::set_mode(&f.choice.path, 0o644).unwrap();
        let before = std::fs::read(&f.choice.path).unwrap();
        let live = ScriptedLiveness::stopped();
        let r = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap();
        assert_eq!(r, StoreWrite::Written);
        assert_eq!(live.checks(), 3, "L0, L1, L2");
        let after = std::fs::read(&f.choice.path).unwrap();
        let v = StoreView::from_bytes(&after).unwrap();
        assert_eq!(v.active_host_id(), Some("id-b"));
        assert_eq!(
            strip_patch_keys(&parse(&after).unwrap()),
            strip_patch_keys(&parse(&before).unwrap())
        );
        #[cfg(unix)]
        assert_eq!(crate::orca::fsx::mode_of(&f.choice.path), Some(0o644));
        // The pre-image is the old bytes; no tmp and no .bak beside the store.
        let pre = preimage_dir(&f.state);
        let names = dir_names(&pre);
        assert_eq!(names.len(), 1);
        assert_eq!(std::fs::read(pre.join(&names[0])).unwrap(), before);
        assert_eq!(
            dir_names(f.choice.path.parent().unwrap()),
            vec!["orca-data.json"]
        );
        // Same patch again: unchanged, nothing written.
        let r = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap();
        assert_eq!(r, StoreWrite::Unchanged);
    }

    #[test]
    fn orca_at_l0_writes_nothing() {
        let f = fx();
        let before = std::fs::read(&f.choice.path).unwrap();
        let live = ScriptedLiveness::appears_at(0);
        let mut called = false;
        let r = write_protocol(&f.choice, false, &live, None, &f.state, &mut |v| {
            called = true;
            select_b(v)
        })
        .unwrap();
        assert_eq!(r, StoreWrite::OrcaAtL0);
        assert!(!called);
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
        assert!(dir_names(&preimage_dir(&f.state)).is_empty());
    }

    #[test]
    fn an_instance_change_since_the_callers_l0_counts_as_orca() {
        let f = fx();
        let before = std::fs::read(&f.choice.path).unwrap();
        let caller_l0 = LiveMark {
            runtime: Some(("rt-old".into(), 7, Some(1))),
            ..LiveMark::stopped()
        };
        let live = ScriptedLiveness::stopped();
        let r = write_protocol(
            &f.choice,
            false,
            &live,
            Some(&caller_l0),
            &f.state,
            &mut select_b,
        )
        .unwrap();
        assert_eq!(r, StoreWrite::OrcaAtL0);
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
    }

    #[test]
    fn orca_at_l1_deletes_the_tmp_and_leaves_the_store() {
        let f = fx();
        let before = std::fs::read(&f.choice.path).unwrap();
        let live = ScriptedLiveness::appears_at(1);
        let r = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap();
        assert_eq!(r, StoreWrite::OrcaAtL1);
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
        assert_eq!(
            dir_names(f.choice.path.parent().unwrap()),
            vec!["orca-data.json"]
        );
    }

    #[test]
    fn orca_at_l2_reports_the_written_file_as_untouchable() {
        let f = fx();
        let live = ScriptedLiveness::appears_at(2);
        let r = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap();
        assert_eq!(r, StoreWrite::OrcaAtL2);
        let v = StoreView::from_bytes(&std::fs::read(&f.choice.path).unwrap()).unwrap();
        assert_eq!(v.active_host_id(), Some("id-b"));
    }

    #[test]
    fn a_store_changed_before_the_rename_is_refused() {
        let f = fx();
        let path = f.choice.path.clone();
        let changed = br#"{"schemaVersion":1,"settings":{"theme":"light"}}"#;
        let live = ScriptedLiveness::stopped().on_check(1, move || {
            std::fs::write(&path, changed).unwrap();
        });
        let e = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap_err();
        assert!(e.to_string().contains("changed"), "{e}");
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), changed);
        assert_eq!(
            dir_names(f.choice.path.parent().unwrap()),
            vec!["orca-data.json"]
        );
    }

    #[test]
    fn legacy_missing_and_ungated_stores_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&ud).unwrap();
        let choice = crate::orca::userdata::data_file(&ud);
        let live = ScriptedLiveness::stopped();
        assert!(write_protocol(&choice, false, &live, None, &state, &mut select_b).is_err());
        std::fs::write(ud.join("orca-data.json"), MINIMAL_STORE).unwrap();
        let e = write_protocol(&choice, true, &live, None, &state, &mut select_b).unwrap_err();
        assert!(e.to_string().contains("legacy"), "{e}");
        std::fs::remove_file(ud.join("orca-data.json")).unwrap();
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(&choice.path, b"{\"schemaVersion\":1, \"settings\":{}}").unwrap();
        let e = write_protocol(&choice, false, &live, None, &state, &mut select_b).unwrap_err();
        assert!(e.to_string().contains("re-serialize"), "{e}");
    }

    #[test]
    fn a_missing_store_is_created_minimal_only_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let state = dir.path().join("state");
        let choice = crate::orca::userdata::data_file(&ud);
        let live = ScriptedLiveness::stopped();
        let rec = record_json(&ud, "id-a", "alice@example.com", None);
        let r = write_protocol(&choice, true, &live, None, &state, &mut |_| {
            Ok(Patch {
                accounts: Some(vec![rec.clone()]),
                active_id: Some(None),
                active_by_runtime: Some(ActiveIds::default()),
            })
        })
        .unwrap();
        assert_eq!(r, StoreWrite::Written);
        let bytes = std::fs::read(&choice.path).unwrap();
        assert!(bytes.starts_with(
            br#"{"schemaVersion":1,"settings":{"claudeManagedAccounts":[{"id":"id-a""#
        ));
        assert!(bytes.ends_with(br#""activeClaudeManagedAccountId":null,"activeClaudeManagedAccountIdsByRuntime":{"host":null,"wsl":{}}}}"#));
        #[cfg(unix)]
        assert_eq!(crate::orca::fsx::mode_of(&choice.path), Some(0o600));
    }

    /// Orca 1.4.214 refuses to start over an index that exists but does not
    /// parse (readExistingProfileIndex): csm never creates or patches a
    /// store in `local-default` then, since Orca may use another profile once
    /// the index is repaired.
    #[test]
    fn an_unreadable_profile_index_refuses_every_offline_write() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&ud).unwrap();
        std::fs::write(ud.join(crate::orca::userdata::PROFILE_INDEX_FILE), b"{").unwrap();
        let choice = crate::orca::userdata::data_file(&ud);
        assert!(choice.index_unreadable);
        let live = ScriptedLiveness::stopped();
        for allow_create in [true, false] {
            let e = write_protocol(&choice, allow_create, &live, None, &state, &mut select_b)
                .unwrap_err();
            assert!(e.to_string().contains("profile index"), "{e}");
        }
        assert!(!ud.join("profiles").exists());
        // An existing local-default store is not patched either.
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(&choice.path, MINIMAL_STORE).unwrap();
        let e = write_protocol(&choice, false, &live, None, &state, &mut select_b).unwrap_err();
        assert!(e.to_string().contains("profile index"), "{e}");
        assert_eq!(std::fs::read(&choice.path).unwrap(), MINIMAL_STORE);
    }

    /// Orca restores a missing primary from its backups (1.4.212) or
    /// refuses to start over them (1.4.214): csm never creates a fresh store
    /// that would hide them.
    #[test]
    fn a_missing_store_with_recovery_files_is_never_created() {
        let evidence = [
            "orca-data.json.bak.0",
            "orca-data.json.bak.4",
            "orca-data.json.sqlite-export.3.json",
            "profile-state.db.backup.1700000000000-0f8fad5b-d9cb-469f-a165-70867728950e.db",
        ];
        for name in evidence {
            let dir = tempfile::tempdir().unwrap();
            let ud = dir.path().join("ud");
            let state = dir.path().join("state");
            let choice = crate::orca::userdata::data_file(&ud);
            let pdir = choice.path.parent().unwrap();
            std::fs::create_dir_all(pdir).unwrap();
            std::fs::write(pdir.join(name), b"{}").unwrap();
            assert!(choice.recovery_evidence().is_some(), "{name}");
            let live = ScriptedLiveness::stopped();
            let e = write_protocol(&choice, true, &live, None, &state, &mut select_b).unwrap_err();
            assert!(e.to_string().contains("recovery files"), "{name}: {e}");
            assert!(!choice.path.exists(), "{name}");
        }
        // The legacy root's backups count for local-default too.
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        std::fs::create_dir_all(&ud).unwrap();
        std::fs::write(ud.join("orca-data.json.bak.1"), b"{}").unwrap();
        let choice = crate::orca::userdata::data_file(&ud);
        assert!(choice.recovery_evidence().is_some());
        // Unrelated neighbours are not evidence.
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let choice = crate::orca::userdata::data_file(&ud);
        let pdir = choice.path.parent().unwrap();
        std::fs::create_dir_all(pdir).unwrap();
        for n in [
            "orca-data.json.bak.5",
            "orca-data.json.sqlite-export.x.json",
            "orca-data.json.123.456.ab.tmp",
        ] {
            std::fs::write(pdir.join(n), b"{}").unwrap();
        }
        assert_eq!(choice.recovery_evidence(), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_unlistable_profile_dir_counts_as_recovery_evidence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let choice = crate::orca::userdata::data_file(&ud);
        let pdir = choice.path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::set_permissions(&pdir, std::fs::Permissions::from_mode(0o300)).unwrap();
        let listable = std::fs::read_dir(&pdir).is_ok();
        let ev = choice.recovery_evidence();
        std::fs::set_permissions(&pdir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root lists anything; the check is only meaningful when it cannot.
        if !listable {
            assert!(ev.is_some_and(|e| e.contains("cannot be listed")));
        }
    }

    #[test]
    fn a_sqlite_backed_profile_is_never_written_or_created() {
        // Orca 1.4.214's "both" state: any change to the JSON breaks its
        // acceptance marker, so the store stays byte-identical.
        for sfx in ["", "-wal", "-shm", "-journal"] {
            let f = fx();
            let before = std::fs::read(&f.choice.path).unwrap();
            let db = f
                .choice
                .path
                .with_file_name(format!("{}{sfx}", crate::orca::userdata::STATE_DB));
            std::fs::write(&db, b"").unwrap();
            assert!(f.choice.has_state_db());
            let live = ScriptedLiveness::stopped();
            let e =
                write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap_err();
            assert!(e.to_string().contains("SQLite"), "{e}");
            assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
            assert!(!preimage_dir(&f.state).exists());
        }
        // The sqlite-only state: no JSON is created beside the database.
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("ud");
        let choice = crate::orca::userdata::data_file(&ud);
        std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
        std::fs::write(
            choice.path.with_file_name(crate::orca::userdata::STATE_DB),
            b"",
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let e = write_protocol(
            &choice,
            true,
            &live,
            None,
            &dir.path().join("s"),
            &mut select_b,
        )
        .unwrap_err();
        assert!(e.to_string().contains("SQLite"), "{e}");
        assert!(!choice.path.exists());
    }

    #[test]
    fn a_database_appearing_before_the_rename_aborts_the_write() {
        let f = fx();
        let before = std::fs::read(&f.choice.path).unwrap();
        let db = f
            .choice
            .path
            .with_file_name(crate::orca::userdata::STATE_DB);
        let live = ScriptedLiveness::stopped().on_check(1, move || {
            std::fs::write(&db, b"").unwrap();
        });
        let e = write_protocol(&f.choice, false, &live, None, &f.state, &mut select_b).unwrap_err();
        assert!(e.to_string().contains("SQLite"), "{e}");
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
        assert_eq!(
            dir_names(f.choice.path.parent().unwrap()),
            vec!["orca-data.json", crate::orca::userdata::STATE_DB]
        );
    }

    #[test]
    fn build_errors_abort_before_any_write() {
        let f = fx();
        let before = std::fs::read(&f.choice.path).unwrap();
        let live = ScriptedLiveness::stopped();
        let e = write_protocol(&f.choice, false, &live, None, &f.state, &mut |_| {
            Err(OrcaError::Refused("no".into()))
        })
        .unwrap_err();
        assert!(matches!(e, OrcaError::Refused(_)));
        assert_eq!(std::fs::read(&f.choice.path).unwrap(), before);
    }

    #[test]
    fn effects_are_read_from_the_snapshot() {
        let ud = Path::new("/Users/example/orca");
        let snap = crate::orca::rpc::parse_claude_snapshot(&serde_json::json!({
            "accounts": [record_json(ud, "id-a", "alice@example.com", None)],
            "activeAccountId": "id-a",
            "activeAccountIdsByRuntime": {"host": "id-a", "wsl": {}}
        }))
        .unwrap();
        assert!(effect_present(&RedoOp::Select { id: "id-a".into() }, &snap));
        assert!(!effect_present(
            &RedoOp::Select { id: "id-b".into() },
            &snap
        ));
        assert!(effect_present(&RedoOp::Remove { id: "id-b".into() }, &snap));
        let ident = |e: &str| {
            crate::orca::record::IdentityKey::new(
                Some(e),
                None,
                crate::orca::record::AuthRuntime::Host,
                None,
            )
            .unwrap()
        };
        let add = |e: &str| RedoOp::Add {
            config_dir: "/tmp/x".into(),
            previous_legacy_sha256: None,
            identity: ident(e),
        };
        assert!(effect_present(&add(" Alice@Example.com "), &snap));
        assert!(!effect_present(&add("bob@example.com"), &snap));
        assert_eq!(
            classify_redo_error(&RpcError::Timeout),
            RedoOutcome::Uncertain(RpcError::Timeout.to_string())
        );
        assert!(matches!(
            classify_redo_error(&RpcError::NotSent("x".into())),
            RedoOutcome::Failed(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn redo_reads_the_effect_then_reissues_only_what_is_missing() {
        let model = std::sync::Arc::new(std::sync::Mutex::new(OrcaModel::default()));
        let orca = FakeOrca::start(model_handler(model.clone()));
        let ud = orca.user_data().to_path_buf();
        {
            let mut m = model.lock().unwrap();
            m.accounts = vec![
                record_json(&ud, "id-a", "alice@example.com", None),
                record_json(&ud, "id-b", "bob@example.com", None),
            ];
            m.active = Some("id-a".into());
        }
        let opts = RedoOpts {
            wait: Duration::from_secs(2),
            poll: Duration::from_millis(20),
        };
        let r = redo_over_rpc(&ud, &RedoOp::Select { id: "id-a".into() }, opts);
        assert!(matches!(r, RedoOutcome::AlreadyDone(_)), "{r:?}");
        let r = redo_over_rpc(&ud, &RedoOp::Select { id: "id-b".into() }, opts);
        assert!(matches!(r, RedoOutcome::Reissued(_)), "{r:?}");
        assert_eq!(model.lock().unwrap().active.as_deref(), Some("id-b"));
        let methods: Vec<String> = orca
            .requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            methods,
            vec!["accounts.list", "accounts.list", "accounts.selectClaude"]
        );
        assert_eq!(orca.requests()[0]["params"]["refreshUsage"], false);
        model.lock().unwrap().fail = Some(("bad".into(), "Account not found".into()));
        let r = redo_over_rpc(&ud, &RedoOp::Remove { id: "id-a".into() }, opts);
        assert!(matches!(r, RedoOutcome::Failed(_)), "{r:?}");
    }

    #[cfg(unix)]
    #[test]
    fn redo_without_an_answer_is_uncertain() {
        let orca = FakeOrca::start(|_| Vec::new()).silent();
        let r = redo_over_rpc(
            orca.user_data(),
            &RedoOp::Select { id: "id-a".into() },
            RedoOpts {
                wait: Duration::from_millis(300),
                poll: Duration::from_millis(20),
            },
        );
        assert!(matches!(r, RedoOutcome::Uncertain(_)), "{r:?}");
        let dir = tempfile::tempdir().unwrap();
        let r = redo_over_rpc(
            dir.path(),
            &RedoOp::Select { id: "id-a".into() },
            RedoOpts {
                wait: Duration::from_millis(100),
                poll: Duration::from_millis(20),
            },
        );
        assert!(matches!(r, RedoOutcome::Uncertain(_)), "{r:?}");
        let _ = running_mark();
    }
}
