//! `profile-state.db`: Orca's SQLite store of record (Orca 1.4.214+), and the
//! offline write of its `settings` document.
//!
//! Orca keeps one row per top-level state domain in
//! `profile_state_documents(domain, payload, domain_version, revision,
//! updated_at, content_hash)` and a profile-wide revision in
//! `profile_state_meta`. The account keys csm changes live in the `settings`
//! row's payload, a compact `JSON.stringify` of the settings object.
//!
//! The write mirrors Orca's own offline settings writer
//! (`updateAgentHookSettingsInProfileState` in
//! `profile-state-offline-settings.ts`, through `writeProfileStateDomains`
//! in `profile-state-domain-writes.ts`), checked against Orca v1.4.214 to
//! v1.4.220, where those files are unchanged:
//! - open the existing database only (never create it), refuse a
//!   `user_version` other than [`DB_SCHEMA_VERSION`], run `quick_check`,
//!   require WAL (Orca's journal mode; csm never sets it), then
//!   `busy_timeout` 5 s and `synchronous = FULL` as Orca does;
//! - `BEGIN IMMEDIATE`, then inside it: the `profile_id` meta row names
//!   this profile, the profile revision, the `settings` row validated the
//!   way Orca validates a row (sha-256 `content_hash`, revision not ahead of
//!   the profile's, positive `domain_version`), and, when `orca-data.json`
//!   sits beside the database, the legacy-JSON acceptance marker matching
//!   that file (`assertAcceptedLegacyJson`: otherwise Orca itself refuses
//!   to start, and csm must not make it worse);
//! - one `UPDATE` of the `settings` row (new payload, `domain_version` 1,
//!   `revision` = profile revision + 1, `updated_at` = now in ms,
//!   `content_hash` = sha-256 hex of the payload) and the `revision` meta
//!   upsert, read back and validated before `COMMIT`.
//!
//! Only the three account keys of the payload change ([`super::store::patch_settings_payload`]);
//! the other rows, the acceptance marker and `orca-data.json` are never
//! touched. Orca loads the result like its own write: the revision fence
//! of its next write starts from the new revision.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use super::OrcaError;
use super::store::{self, StoreView};

/// `PROFILE_STATE_DATABASE_SCHEMA_VERSION`: the only `user_version` csm
/// writes.
pub const DB_SCHEMA_VERSION: i64 = 3;

/// `PROFILE_STATE_DOCUMENT_VERSION`: the `domain_version` Orca writes.
pub const DOCUMENT_VERSION: i64 = 1;

/// `PROFILE_STATE_BUSY_TIMEOUT_MS`.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const META_PROFILE_ID: &str = "profile_id";
const META_REVISION: &str = "revision";
const META_ACCEPTANCE: &str = "legacy_json_acceptance";

/// The domain csm changes.
pub const SETTINGS: &str = "settings";
/// The domain carrying the store's `schemaVersion`.
pub const SCHEMA: &str = "schemaVersion";

/// Cap on a payload.
const PAYLOAD_CAP: usize = 256 * 1024 * 1024;

fn refuse(what: impl std::fmt::Display) -> OrcaError {
    OrcaError::Refused(format!("{}: {what}", super::userdata::STATE_DB))
}

/// Lowercase sha-256 hex (`hashProfileStatePayload`).
pub fn sha256_hex(bytes: &[u8]) -> String {
    super::add::sha256_hex(bytes)
}

// ─── rows ─────────────────────────────────────────────────────────────────────

/// One `profile_state_documents` row.
#[derive(Clone, PartialEq, Eq)]
pub struct DocRow {
    pub payload: String,
    pub domain_version: i64,
    pub revision: i64,
    pub updated_at: i64,
    pub content_hash: String,
}

impl std::fmt::Debug for DocRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocRow")
            .field("payload", &format_args!("<{} bytes>", self.payload.len()))
            .field("domain_version", &self.domain_version)
            .field("revision", &self.revision)
            .field("updated_at", &self.updated_at)
            .field("content_hash", &self.content_hash)
            .finish()
    }
}

/// JS `Number.isSafeInteger`.
fn safe_int(n: i64) -> bool {
    n.unsigned_abs() < (1u64 << 53)
}

/// Orca's `validateProfileStateDocumentRow` plus
/// `assertProfileStateDocumentRevision`. Pure.
pub fn validate_row(domain: &str, row: &DocRow, profile_revision: i64) -> Result<(), String> {
    let hex64 = row.content_hash.len() == 64
        && row
            .content_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !safe_int(row.domain_version)
        || row.domain_version < 1
        || !safe_int(row.revision)
        || row.revision < 1
        || !safe_int(row.updated_at)
        || row.updated_at < 0
        || !hex64
    {
        return Err(format!("the {domain} row's metadata is invalid"));
    }
    if sha256_hex(row.payload.as_bytes()) != row.content_hash {
        return Err(format!(
            "the {domain} row's hash does not match its payload"
        ));
    }
    if serde_json::from_str::<serde::de::IgnoredAny>(&row.payload).is_err() {
        return Err(format!("the {domain} row is not JSON"));
    }
    if row.revision > profile_revision {
        return Err(format!("the {domain} row is ahead of the profile revision"));
    }
    Ok(())
}

fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>, OrcaError> {
    conn.query_row(
        "SELECT value FROM profile_state_meta WHERE key = ?1",
        [key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(|_| refuse("cannot read profile_state_meta"))
}

/// `readProfileStateRevision`: absent is 0.
pub fn read_revision(conn: &Connection) -> Result<i64, OrcaError> {
    match read_meta(conn, META_REVISION)? {
        None => Ok(0),
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 0 && safe_int(n) && n.to_string() == v => Ok(n),
            _ => Err(refuse("the profile revision is invalid")),
        },
    }
}

fn read_row(conn: &Connection, domain: &str) -> Result<Option<DocRow>, OrcaError> {
    conn.query_row(
        "SELECT payload, domain_version, revision, updated_at, content_hash
         FROM profile_state_documents WHERE domain = ?1",
        [domain],
        |r| {
            Ok(DocRow {
                payload: r.get(0)?,
                domain_version: r.get(1)?,
                revision: r.get(2)?,
                updated_at: r.get(3)?,
                content_hash: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|_| refuse(format!("the {domain} row has invalid fields")))
}

// ─── the acceptance marker ────────────────────────────────────────────────────

/// Does the retained `orca-data.json` (`json`) match the database's
/// legacy-JSON acceptance marker (`readAcceptedSnapshot`)? Pure over the
/// marker text.
pub fn json_accepted(marker: Option<&str>, json: &[u8], revision: i64) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    let Ok(m) = serde_json::from_str::<serde_json::Value>(marker) else {
        return false;
    };
    let version = |v: &serde_json::Value| -> Option<(String, i64)> {
        let h = v.get("jsonHash")?.as_str()?;
        let r = v.get("acceptedRevision")?.as_i64()?;
        (h.len() == 64 && r >= 1).then(|| (h.to_owned(), r))
    };
    let Some((hash, accepted)) = version(&m) else {
        return false;
    };
    let pending = match m.get("pending") {
        None => None,
        Some(p) => match version(p) {
            Some(p) if p.1 >= accepted => Some(p),
            _ => return false,
        },
    };
    let jh = sha256_hex(json);
    let hash_ok = jh == hash || pending.as_ref().is_some_and(|p| p.0 == jh);
    let floor = pending.as_ref().map_or(accepted, |p| p.1);
    hash_ok && revision >= floor
}

// ─── read ─────────────────────────────────────────────────────────────────────

/// Build the account view of a `settings` payload and its `schemaVersion`
/// payload. The view is writable when the payload passes the round-trip
/// gate, `schemaVersion` is 1, and the settings value is an object.
pub fn view_of(settings: &str, schema: Option<&str>) -> Result<StoreView, OrcaError> {
    let parsed: serde_json::Value =
        serde_json::from_str(settings).map_err(|_| refuse("settings is not JSON"))?;
    if !parsed.is_object() {
        return Err(refuse("settings is not a JSON object"));
    }
    let round_trip_ok = store::round_trips(&parsed, settings.as_bytes());
    let mut doc = serde_json::Map::new();
    if let Some(s) = schema.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
        doc.insert("schemaVersion".into(), s);
    }
    doc.insert("settings".into(), parsed);
    StoreView::from_value(&serde_json::Value::Object(doc), round_trip_ok)
        .map_err(|e| OrcaError::Refused(e.to_string()))
}

/// The account view of `db`, read-only (`SQLITE_OPEN_READ_ONLY`, never
/// `immutable`, so a stopped Orca's `-wal` is replayed). `None` when the
/// file is absent or has no `settings` row.
pub fn load_view(db: &Path) -> Result<Option<StoreView>, OrcaError> {
    match std::fs::symlink_metadata(db) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OrcaError::io("cannot stat", db, e)),
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| refuse("cannot open read-only"))?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let read = |domain: &str| -> Result<Option<String>, OrcaError> {
        match conn.query_row(
            "SELECT payload FROM profile_state_documents WHERE domain = ?1",
            [domain],
            |r| r.get::<_, String>(0),
        ) {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            // No such table (an empty file): nothing to read.
            Err(e) if e.to_string().contains("no such table") => Ok(None),
            Err(_) => Err(refuse("cannot read settings")),
        }
    };
    let Some(settings) = read(SETTINGS)? else {
        return Ok(None);
    };
    if settings.len() > PAYLOAD_CAP {
        return Err(refuse("settings payload too large"));
    }
    let schema = read(SCHEMA).ok().flatten();
    view_of(&settings, schema.as_deref()).map(Some)
}

// ─── write ────────────────────────────────────────────────────────────────────

/// What one database write saw and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbWrite {
    /// Committed at this profile revision.
    Committed { revision: i64 },
    /// The patch changed nothing; the transaction was rolled back.
    Unchanged,
    /// `before_commit` said no: rolled back, nothing changed.
    Aborted,
}

/// The pre-image hook: gets the old settings payload and its profile
/// revision before anything is written.
pub type PreimageFn<'a> = dyn FnMut(&str, i64) -> Result<(), OrcaError> + 'a;

fn require_main_file(db: &Path) -> Result<(), OrcaError> {
    match std::fs::metadata(db) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(refuse("is not a file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(refuse(
            "only its -wal/-shm/-journal files exist (an orphaned sidecar); start Orca so it recovers",
        )),
        Err(e) => Err(OrcaError::io("cannot stat", db, e)),
    }
}

/// The header checks Orca's open makes before it writes: `user_version`,
/// `quick_check`, and WAL (which csm reads, never sets).
fn check_header(conn: &Connection) -> Result<(), OrcaError> {
    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|_| refuse("is not a SQLite database"))?;
    if user_version != DB_SCHEMA_VERSION {
        return Err(refuse(format!(
            "schema {user_version}, csm writes only schema {DB_SCHEMA_VERSION}; start Orca"
        )));
    }
    let check: String = conn
        .pragma_query_value(None, "quick_check", |r| r.get(0))
        .map_err(|_| refuse("integrity check failed"))?;
    if check != "ok" {
        return Err(refuse("integrity check failed; start Orca so it recovers"));
    }
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(|_| refuse("cannot read journal_mode"))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(refuse(format!(
            "journal mode {mode}, Orca's is WAL; start Orca"
        )));
    }
    Ok(())
}

/// Open `db` for a write the way Orca does, refusing anything it would not
/// write through. Never creates the file.
pub fn open_for_write(db: &Path) -> Result<Connection, OrcaError> {
    require_main_file(db)?;
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| refuse("cannot open"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|_| refuse("cannot set busy_timeout"))?;
    check_header(&conn)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| refuse("cannot set synchronous"))?;
    Ok(conn)
}

/// What [`inspect`] read and validated.
struct Inspected {
    revision: i64,
    settings: DocRow,
    view: StoreView,
}

/// Every check a write makes before it changes a byte, over an open
/// connection (inside the caller's transaction, when it has one).
fn inspect(
    conn: &Connection,
    profile_id: &str,
    json: Option<&[u8]>,
) -> Result<Inspected, OrcaError> {
    match read_meta(conn, META_PROFILE_ID)? {
        Some(id) if id == profile_id => {}
        _ => return Err(refuse("belongs to another profile")),
    }
    let revision = read_revision(conn)?;
    let Some(row) = read_row(conn, SETTINGS)? else {
        return Err(refuse("has no settings document; start Orca once"));
    };
    validate_row(SETTINGS, &row, revision).map_err(refuse)?;
    if let Some(json) = json
        && !json_accepted(read_meta(conn, META_ACCEPTANCE)?.as_deref(), json, revision)
    {
        return Err(refuse(
            "orca-data.json does not match the database's acceptance marker; \
             Orca will not start on this profile as it is, so csm leaves it alone",
        ));
    }
    let schema = match read_row(conn, SCHEMA)? {
        Some(r) => {
            validate_row(SCHEMA, &r, revision).map_err(refuse)?;
            Some(r.payload)
        }
        None => None,
    };
    let view = view_of(&row.payload, schema.as_deref())?;
    view.writable()
        .map_err(|e| OrcaError::Refused(e.to_string()))?;
    Ok(Inspected {
        revision,
        settings: row,
        view,
    })
}

/// Would [`write_settings`] accept `db`? The same checks over a read-only
/// connection, so a caller can refuse before it changes anything else (a
/// switch checks this before it touches `D`). Writes nothing.
pub fn preflight(db: &Path, profile_id: &str, json: Option<&[u8]>) -> Result<(), OrcaError> {
    require_main_file(db)?;
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| refuse("cannot open read-only"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|_| refuse("cannot set busy_timeout"))?;
    check_header(&conn)?;
    inspect(&conn, profile_id, json).map(|_| ())
}

/// Patch the account keys of the `settings` row of `db` in one
/// `BEGIN IMMEDIATE` transaction (see the module doc). `json` is the
/// retained `orca-data.json` beside the database, when one exists.
/// `build` gets the current view and returns the patch; `preimage` runs
/// before the write; `before_commit` is the caller's last liveness check
/// (false rolls back).
pub fn write_settings(
    db: &Path,
    profile_id: &str,
    json: Option<&[u8]>,
    build: &mut dyn FnMut(&StoreView) -> Result<store::Patch, OrcaError>,
    preimage: &mut PreimageFn<'_>,
    before_commit: &mut dyn FnMut() -> bool,
) -> Result<DbWrite, OrcaError> {
    let mut conn = open_for_write(db)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| refuse("busy (another writer holds it); nothing was written"))?;

    let Inspected {
        revision,
        settings: row,
        view,
    } = inspect(&tx, profile_id, json)?;
    let patch = build(&view)?;
    let out = store::patch_settings_payload(row.payload.as_bytes(), &patch)
        .map_err(|e| OrcaError::Refused(e.to_string()))?;
    if out == row.payload.as_bytes() {
        return Ok(DbWrite::Unchanged);
    }
    let payload = String::from_utf8(out).map_err(|_| refuse("patched settings is not UTF-8"))?;
    preimage(&row.payload, revision)?;

    let next = revision + 1;
    let updated_at = super::now_ms();
    let hash = sha256_hex(payload.as_bytes());
    let n = tx
        .execute(
            "UPDATE profile_state_documents
             SET payload = ?1, domain_version = ?2, revision = ?3, updated_at = ?4, content_hash = ?5
             WHERE domain = ?6",
            rusqlite::params![payload, DOCUMENT_VERSION, next, updated_at, hash, SETTINGS],
        )
        .map_err(|_| refuse("cannot update settings"))?;
    if n != 1 {
        return Err(refuse("cannot update settings"));
    }
    tx.execute(
        "INSERT INTO profile_state_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_REVISION, next.to_string()],
    )
    .map_err(|_| refuse("cannot update the profile revision"))?;

    // Read back what Orca will load and validate it the way Orca does.
    let back = read_row(&tx, SETTINGS)?.ok_or_else(|| refuse("settings vanished"))?;
    let back_rev = read_revision(&tx)?;
    if back_rev != next || back.payload != payload {
        return Err(refuse("read-back differs; rolled back"));
    }
    validate_row(SETTINGS, &back, back_rev).map_err(refuse)?;

    if !before_commit() {
        // Dropping `tx` rolls back.
        return Ok(DbWrite::Aborted);
    }
    tx.commit()
        .map_err(|_| refuse("commit failed; the database rolled back"))?;
    Ok(DbWrite::Committed { revision: next })
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::record::{RuntimeTarget, p3};
    use crate::orca::testsupport::{OrcaDb, write_orca_db};

    const SETTINGS_JSON: &str = r#"{"theme":"dark","opencodeSessionCookie":"orca-secret-slot-00000000-0000-4000-8000-000000000000","claudeManagedAccounts":[{"id":"id-a","email":"alice@example.com","managedAuthRuntime":"host"},{"id":"id-b","email":"bob@example.com","managedAuthRuntime":"host"}],"zoom":1.25,"activeClaudeManagedAccountId":"id-a","label":"Café ☃","activeClaudeManagedAccountIdsByRuntime":{"host":"id-a","wsl":{}},"big":1e+21}"#;

    fn select(id: &'static str) -> impl FnMut(&StoreView) -> Result<store::Patch, OrcaError> {
        move |v: &StoreView| {
            Ok(store::Patch {
                active_id: Some(Some(id.into())),
                active_by_runtime: Some(p3(&v.active, Some(id), &RuntimeTarget::Host)),
                ..store::Patch::default()
            })
        }
    }

    fn write(
        f: &OrcaDb,
        json: Option<&[u8]>,
        build: &mut dyn FnMut(&StoreView) -> Result<store::Patch, OrcaError>,
    ) -> Result<DbWrite, OrcaError> {
        write_settings(
            &f.db,
            "local-default",
            json,
            build,
            &mut |_, _| Ok(()),
            &mut || true,
        )
    }

    /// Orca's loader over the database: every row validates, the revision
    /// fence holds, and the JSON projection (`readProfileStateSnapshot`)
    /// parses.
    fn orca_loads(db: &Path) -> serde_json::Value {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let rev = read_revision(&conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT domain, payload, domain_version, revision, updated_at, content_hash
                 FROM profile_state_documents ORDER BY rowid",
            )
            .unwrap();
        let rows: Vec<(String, DocRow)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    DocRow {
                        payload: r.get(1)?,
                        domain_version: r.get(2)?,
                        revision: r.get(3)?,
                        updated_at: r.get(4)?,
                        content_hash: r.get(5)?,
                    },
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (d, r) in &rows {
            validate_row(d, r, rev).unwrap();
        }
        let json = format!(
            "{{{}}}",
            rows.iter()
                .map(|(d, r)| format!("{}:{}", serde_json::to_string(d).unwrap(), r.payload))
                .collect::<Vec<_>>()
                .join(",")
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn a_switch_writes_the_settings_row_like_orca() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 41);
        let before = f.rows();
        let r = write(&f, None, &mut select("id-b")).unwrap();
        assert_eq!(r, DbWrite::Committed { revision: 42 });

        let after = f.rows();
        // Only the settings row changed; every other row keeps its bytes
        // and its old revision (rows may lag the profile revision).
        for (d, r) in &before {
            if d != SETTINGS {
                assert_eq!(after.iter().find(|(x, _)| x == d).unwrap().1, *r, "{d}");
            }
        }
        let s = &after.iter().find(|(d, _)| d == SETTINGS).unwrap().1;
        assert_eq!(s.revision, 42);
        assert_eq!(s.domain_version, DOCUMENT_VERSION);
        assert_eq!(s.content_hash, sha256_hex(s.payload.as_bytes()));
        // The payload differs from the old one only in the two active keys.
        assert_eq!(
            s.payload,
            SETTINGS_JSON
                .replace(
                    r#""activeClaudeManagedAccountId":"id-a""#,
                    r#""activeClaudeManagedAccountId":"id-b""#
                )
                .replace(r#"{"host":"id-a","wsl":{}}"#, r#"{"host":"id-b","wsl":{}}"#)
        );
        assert_eq!(f.meta("revision").as_deref(), Some("42"));
        assert_eq!(f.meta("profile_id").as_deref(), Some("local-default"));

        let loaded = orca_loads(&f.db);
        assert_eq!(loaded["settings"]["activeClaudeManagedAccountId"], "id-b");
        assert_eq!(loaded["schemaVersion"], 1);
        // And csm's own reader sees it.
        let v = load_view(&f.db).unwrap().unwrap();
        assert_eq!(v.active_host_id(), Some("id-b"));
        assert!(v.writable().is_ok());

        // The same switch again changes nothing, and no revision is spent.
        assert_eq!(
            write(&f, None, &mut select("id-b")).unwrap(),
            DbWrite::Unchanged
        );
        assert_eq!(f.meta("revision").as_deref(), Some("42"));
    }

    #[test]
    fn an_add_appends_a_record_and_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 3);
        let mut add = |v: &StoreView| {
            let mut accounts = v.accounts_raw.clone();
            accounts.push(serde_json::json!({"id":"id-c","email":"carol@example.com","managedAuthRuntime":"host"}));
            Ok(store::Patch {
                accounts: Some(accounts),
                ..store::Patch::default()
            })
        };
        write(&f, None, &mut add).unwrap();
        let v = load_view(&f.db).unwrap().unwrap();
        let ids: Vec<_> = v.accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["id-a", "id-b", "id-c"]);
        assert_eq!(v.active_host_id(), Some("id-a"));
        orca_loads(&f.db);
    }

    #[test]
    fn the_preimage_runs_before_the_write_and_can_veto_it() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 7);
        let mut seen = None;
        let r = write_settings(
            &f.db,
            "local-default",
            None,
            &mut select("id-b"),
            &mut |p, rev| {
                seen = Some((p.to_owned(), rev));
                Err(OrcaError::Refused("no room".into()))
            },
            &mut || true,
        );
        assert!(r.is_err());
        assert_eq!(seen, Some((SETTINGS_JSON.to_owned(), 7)));
        assert_eq!(f.meta("revision").as_deref(), Some("7"));
        assert_eq!(
            load_view(&f.db).unwrap().unwrap().active_host_id(),
            Some("id-a")
        );
    }

    #[test]
    fn a_false_last_check_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 7);
        let before = f.rows();
        let r = write_settings(
            &f.db,
            "local-default",
            None,
            &mut select("id-b"),
            &mut |_, _| Ok(()),
            &mut || false,
        )
        .unwrap();
        assert_eq!(r, DbWrite::Aborted);
        assert_eq!(f.rows(), before);
        assert_eq!(f.meta("revision").as_deref(), Some("7"));
    }

    #[test]
    fn the_acceptance_marker_must_match_a_retained_export() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 5);
        let export = br#"{"schemaVersion":1,"settings":{}}"#;
        // No marker: refused.
        let e = write(&f, Some(export), &mut select("id-b")).unwrap_err();
        assert!(e.to_string().contains("acceptance"), "{e}");
        // The marker for these bytes at revision 1: accepted.
        f.set_meta(
            "legacy_json_acceptance",
            &format!(
                r#"{{"jsonHash":"{}","acceptedRevision":1}}"#,
                sha256_hex(export)
            ),
        );
        write(&f, Some(export), &mut select("id-b")).unwrap();
        // Different bytes: refused, nothing written.
        let e = write(&f, Some(b"{}"), &mut select("id-a")).unwrap_err();
        assert!(e.to_string().contains("acceptance"), "{e}");
        assert_eq!(
            load_view(&f.db).unwrap().unwrap().active_host_id(),
            Some("id-b")
        );
    }

    #[test]
    fn json_acceptance_mirrors_orca() {
        let j = b"{}";
        let h = sha256_hex(j);
        let other = sha256_hex(b"x");
        assert!(json_accepted(
            Some(&format!(r#"{{"jsonHash":"{h}","acceptedRevision":3}}"#)),
            j,
            3
        ));
        assert!(!json_accepted(
            Some(&format!(r#"{{"jsonHash":"{h}","acceptedRevision":4}}"#)),
            j,
            3
        ));
        assert!(!json_accepted(
            Some(&format!(r#"{{"jsonHash":"{other}","acceptedRevision":1}}"#)),
            j,
            3
        ));
        // A pending export is accepted too; its revision is the floor.
        let m = format!(
            r#"{{"jsonHash":"{other}","acceptedRevision":1,"pending":{{"jsonHash":"{h}","acceptedRevision":2}}}}"#
        );
        assert!(json_accepted(Some(&m), j, 2));
        assert!(!json_accepted(Some(&m), j, 1));
        let bad = format!(
            r#"{{"jsonHash":"{h}","acceptedRevision":3,"pending":{{"jsonHash":"{h}","acceptedRevision":2}}}}"#
        );
        assert!(!json_accepted(Some(&bad), j, 9));
        assert!(!json_accepted(None, j, 9));
        assert!(!json_accepted(Some("not json"), j, 9));
    }

    #[test]
    fn databases_orca_would_not_write_are_refused_untouched() {
        type Mutate = fn(&OrcaDb);
        let cases: [(&str, Mutate); 6] = [
            ("schema", |f| f.exec("PRAGMA user_version = 4")),
            ("schema", |f| f.exec("PRAGMA user_version = 2")),
            ("another profile", |f| f.set_meta("profile_id", "other")),
            ("hash", |f| {
                f.exec("UPDATE profile_state_documents SET content_hash = lower(hex(randomblob(32))) WHERE domain = 'settings'")
            }),
            ("ahead", |f| {
                f.exec("UPDATE profile_state_documents SET revision = 99 WHERE domain = 'settings'")
            }),
            ("no settings", |f| {
                f.exec("DELETE FROM profile_state_documents WHERE domain = 'settings'")
            }),
        ];
        for (want, mutate) in cases {
            let dir = tempfile::tempdir().unwrap();
            let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 9);
            mutate(&f);
            let before = f.rows();
            let rev = f.meta("revision");
            let e = write(&f, None, &mut select("id-b")).unwrap_err();
            assert!(e.to_string().contains(want), "{want}: {e}");
            assert_eq!(f.rows(), before, "{want}");
            assert_eq!(f.meta("revision"), rev, "{want}");
        }
    }

    #[test]
    fn a_settings_payload_that_does_not_round_trip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", r#"{"a": 1}"#, 2);
        let e = write(&f, None, &mut select("id-b")).unwrap_err();
        assert!(e.to_string().contains("re-serialize"), "{e}");
        assert_eq!(f.meta("revision").as_deref(), Some("2"));
    }

    #[test]
    fn a_missing_main_file_or_a_rollback_journal_db_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("profile-state.db");
        std::fs::write(dir.path().join("profile-state.db-wal"), b"").unwrap();
        let e = open_for_write(&p).unwrap_err();
        assert!(e.to_string().contains("orphaned"), "{e}");
        assert!(!p.exists(), "never created");
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch("PRAGMA user_version = 3; CREATE TABLE t(x);")
            .unwrap();
        drop(conn);
        let e = open_for_write(&p).unwrap_err();
        assert!(e.to_string().contains("WAL"), "{e}");
    }

    #[test]
    fn a_reader_sees_the_view_and_its_gate() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_orca_db(dir.path(), "local-default", SETTINGS_JSON, 1);
        let v = load_view(&f.db).unwrap().unwrap();
        assert_eq!(v.schema_version.as_deref(), Some("1"));
        assert!(v.round_trip_ok && v.writable().is_ok());
        assert_eq!(v.active_host_id(), Some("id-a"));
        assert!(
            !format!(
                "{:?}",
                read_row(&Connection::open(&f.db).unwrap(), SETTINGS).unwrap()
            )
            .contains("alice")
        );
    }
}
