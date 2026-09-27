//! The system-default snapshot: what `D` held before Orca (or csm) first
//! materialized a managed account over it, so that selecting "system
//! default" later can put it back. Ported from Orca 1.4.209 (H7i
//! M:247279-247331, captureSystemDefaultSnapshotForManagedEntry and
//! captureSystemDefaultSnapshot M:247549-247610).
//!
//! File: `<userData>/claude-runtime-auth/system-default-auth.json`, Orca's
//! writeJson (`JSON.stringify(v, null, 2) + "\n"`, mode 0600, skipped when
//! equal). Fields, in order: `credentialsJson`, `configOauthAccount`,
//! `keychainCredentialsJson`, `scopedKeychainCredentialsJson`,
//! `legacyKeychainCredentialsJson`, `scopedKeychainCredentialsCaptured`,
//! `legacyKeychainCredentialsCaptured`, `capturedAt`.
//!
//! The force/override decision ([`plan_for_managed_entry`]):
//! - the runtime file differs from the target grant: capture, forced, with
//!   the previous snapshot and the target grant;
//! - equal, and a previous snapshot exists: capture, forced, keeping the
//!   previous `credentialsJson`;
//! - equal and no previous snapshot: capture only if no file exists.
//!
//! A Keychain value equal to the target grant is replaced by the previous
//! snapshot's captured value ([`snapshot_keychain_credentials`]): the
//! target is a managed grant, never the system default.
//!
//! Orca deletes a snapshot that fails isSystemDefaultSnapshot and then
//! captures afresh; csm treats it as absent and overwrites it, which ends
//! in the same file, except that a capture that fails leaves the invalid
//! file in place instead of deleting it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::jsjson;
use super::keychain::{self, KeychainUser};
use super::runtime::{OauthIdentity, RuntimeIdentity, RuntimePaths};
use super::userdata::HostOs;
use super::{OrcaError, SecretString};

/// Orca's runtime metadata dir under userData.
pub const DIR: &str = "claude-runtime-auth";
/// The snapshot file.
pub const FILE: &str = "system-default-auth.json";
/// Cap on the snapshot and the runtime files it reads.
const CAP: u64 = 64 * 1024 * 1024;

/// `<late userData>/claude-runtime-auth/system-default-auth.json`. Orca
/// resolves this dir per call after `app.setName('Orca')`
/// (runtime-auth-file-storage.ts), so on a case-sensitive filesystem it
/// sits under `<appData>/Orca`, not under the canonical userData `user_data`
/// names (see [`super::userdata::late_user_data`]).
pub fn snapshot_path(user_data: &Path) -> PathBuf {
    super::userdata::late_user_data(user_data)
        .join(DIR)
        .join(FILE)
}

// ─── shape ────────────────────────────────────────────────────────────────────

/// Which runtime Keychain item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Scoped,
    Legacy,
}

impl Kind {
    fn json_key(self) -> &'static str {
        match self {
            Kind::Scoped => "scopedKeychainCredentialsJson",
            Kind::Legacy => "legacyKeychainCredentialsJson",
        }
    }
    fn captured_key(self) -> &'static str {
        match self {
            Kind::Scoped => "scopedKeychainCredentialsCaptured",
            Kind::Legacy => "legacyKeychainCredentialsCaptured",
        }
    }
}

fn nullable_string(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null) | Some(Value::String(_)))
}

fn optional_bool(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Bool(_)))
}

fn has_valid_keychain_value(m: &Map<String, Value>, kind: Kind) -> bool {
    m.get(kind.captured_key()) == Some(&Value::Bool(false))
        || m.contains_key(kind.json_key())
        || m.contains_key("keychainCredentialsJson")
}

/// isSystemDefaultSnapshot. Pure.
pub fn is_snapshot(v: &Value) -> bool {
    let Some(m) = v.as_object() else {
        return false;
    };
    m.contains_key("credentialsJson")
        && nullable_string(m.get("credentialsJson"))
        && nullable_string(m.get("keychainCredentialsJson"))
        && nullable_string(m.get(Kind::Scoped.json_key()))
        && nullable_string(m.get(Kind::Legacy.json_key()))
        && optional_bool(m.get(Kind::Scoped.captured_key()))
        && optional_bool(m.get(Kind::Legacy.captured_key()))
        && has_valid_keychain_value(m, Kind::Scoped)
        && has_valid_keychain_value(m, Kind::Legacy)
        && matches!(m.get("capturedAt"), None | Some(Value::Number(_)))
}

/// readKeychainSnapshotValue's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KcValue {
    Captured(Option<String>),
    Unknown,
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

/// readKeychainSnapshotValue. Pure.
pub fn read_keychain_value(prev: Option<&Map<String, Value>>, kind: Kind) -> KcValue {
    let Some(p) = prev else {
        return KcValue::Captured(None);
    };
    if p.get(kind.captured_key()) == Some(&Value::Bool(false)) {
        return KcValue::Unknown;
    }
    if p.contains_key(kind.json_key()) {
        return KcValue::Captured(str_of(p.get(kind.json_key())));
    }
    KcValue::Captured(str_of(p.get("keychainCredentialsJson")))
}

/// snapshotKeychainCredentials. Pure.
pub fn snapshot_keychain_credentials(
    value: Option<&str>,
    prev: Option<&Map<String, Value>>,
    kind: Kind,
    managed: Option<&str>,
) -> Option<String> {
    if let (Some(m), Some(_)) = (managed.filter(|m| !m.is_empty()), prev)
        && value == Some(m)
        && let KcValue::Captured(v) = read_keychain_value(prev, kind)
    {
        return v;
    }
    value.map(str::to_owned)
}

// ─── plan ─────────────────────────────────────────────────────────────────────

/// captureSystemDefaultSnapshot's options.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureOpts {
    pub force: bool,
    /// `Some(x)`: use `x` as `credentialsJson` instead of reading the file.
    pub credentials_override: Option<Option<String>>,
    pub previous: Option<Map<String, Value>>,
    pub managed: Option<String>,
}

/// captureSystemDefaultSnapshotForManagedEntry(e = `runtime_file`, t =
/// `target`) given the previous valid snapshot. Pure.
pub fn plan_for_managed_entry(
    runtime_file: Option<&str>,
    target: &str,
    previous: Option<Map<String, Value>>,
) -> CaptureOpts {
    if runtime_file != Some(target) {
        return CaptureOpts {
            force: true,
            credentials_override: None,
            previous,
            managed: Some(target.to_owned()),
        };
    }
    match previous {
        Some(p) => CaptureOpts {
            force: true,
            credentials_override: Some(str_of(p.get("credentialsJson"))),
            previous: Some(p),
            managed: Some(target.to_owned()),
        },
        None => CaptureOpts {
            force: false,
            credentials_override: None,
            previous: None,
            managed: None,
        },
    }
}

/// What captureSystemDefaultSnapshot read.
#[derive(Debug, Clone)]
pub struct Surfaces {
    /// `D/.credentials.json`, `None` when absent.
    pub file: Option<String>,
    /// De.a over w(D), best effort.
    pub aggregate: Option<String>,
    /// De.o(D); `Err` = the read failed.
    pub scoped: Result<Option<String>, ()>,
    /// De.o() (the unscoped item); `Err` = the read failed.
    pub legacy: Result<Option<String>, ()>,
    /// The runtime `oauthAccount` (`null` for a missing file, key, or an
    /// unparseable file).
    pub config_oauth: Value,
}

/// Build the snapshot object. `None` when a Keychain read failed (Orca
/// throws "Cannot capture current Claude Keychain credentials"). Pure.
pub fn build(opts: &CaptureOpts, s: &Surfaces, now_ms: i64) -> Option<Value> {
    let (Ok(scoped), Ok(legacy)) = (&s.scoped, &s.legacy) else {
        return None;
    };
    let creds = match &opts.credentials_override {
        Some(o) => o.clone(),
        None => s.file.clone(),
    };
    let prev = opts.previous.as_ref();
    let managed = opts.managed.as_deref();
    let opt = |v: Option<String>| v.map(Value::String).unwrap_or(Value::Null);
    let mut m = Map::new();
    m.insert("credentialsJson".into(), opt(creds));
    m.insert("configOauthAccount".into(), s.config_oauth.clone());
    m.insert("keychainCredentialsJson".into(), opt(s.aggregate.clone()));
    m.insert(
        Kind::Scoped.json_key().into(),
        opt(snapshot_keychain_credentials(
            scoped.as_deref(),
            prev,
            Kind::Scoped,
            managed,
        )),
    );
    m.insert(
        Kind::Legacy.json_key().into(),
        opt(snapshot_keychain_credentials(
            legacy.as_deref(),
            prev,
            Kind::Legacy,
            managed,
        )),
    );
    m.insert(Kind::Scoped.captured_key().into(), Value::Bool(true));
    m.insert(Kind::Legacy.captured_key().into(), Value::Bool(true));
    m.insert("capturedAt".into(), Value::Number(now_ms.into()));
    Some(Value::Object(m))
}

// ─── shell ────────────────────────────────────────────────────────────────────

/// readSystemDefaultSnapshot: the valid snapshot, else `None` (an invalid
/// one is left in place; see the module doc).
pub fn read_snapshot(user_data: &Path) -> Option<Map<String, Value>> {
    let bytes = super::read_capped_bytes(&snapshot_path(user_data), CAP).ok()??;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    if !is_snapshot(&v) {
        return None;
    }
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn read_text(p: &Path) -> Result<Option<SecretString>, OrcaError> {
    let Some(b) =
        super::read_capped_bytes(p, CAP).map_err(|e| OrcaError::io("cannot read", p, e))?
    else {
        return Ok(None);
    };
    String::from_utf8(b)
        .map(|s| Some(SecretString::new(s)))
        .map_err(|e| {
            let mut b = e.into_bytes();
            super::zero(&mut b);
            OrcaError::Invalid(format!("{} is not UTF-8 text", p.display()))
        })
}

/// Read the surfaces the snapshot records.
pub fn read_surfaces(
    os: HostOs,
    paths: &RuntimePaths,
    user: &KeychainUser,
) -> Result<Surfaces, OrcaError> {
    let file = read_text(&paths.credentials_path)?.map(|s| s.expose().to_owned());
    let config_oauth = match super::runtime::read_json_object(&paths.config_path) {
        Some(m) => m.get("oauthAccount").cloned().unwrap_or(Value::Null),
        None => Value::Null,
    };
    let (aggregate, scoped, legacy) = if os == HostOs::MacOs {
        let dir = paths.config_dir.to_string_lossy().into_owned();
        let own = |r: Result<Option<SecretString>, keychain::KeychainError>| {
            r.map(|o| o.map(|s| s.expose().to_owned())).map_err(|_| ())
        };
        (
            own(keychain::read_runtime_aggregate(Some(&dir), user)).unwrap_or(None),
            own(keychain::read_runtime_scoped(Some(&dir), user)),
            own(keychain::read_runtime_scoped(None, user)),
        )
    } else {
        (None, Ok(None), Ok(None))
    };
    Ok(Surfaces {
        file,
        aggregate,
        scoped,
        legacy,
        config_oauth,
    })
}

/// What a capture did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Captured {
    /// Not forced and a snapshot exists.
    Skipped,
    /// The file already held exactly this text.
    Unchanged,
    Written,
}

/// captureSystemDefaultSnapshot with `opts`.
pub fn capture(
    user_data: &Path,
    os: HostOs,
    paths: &RuntimePaths,
    user: &KeychainUser,
    opts: &CaptureOpts,
    now_ms: i64,
) -> Result<Captured, OrcaError> {
    let path = snapshot_path(user_data);
    // An invalid snapshot counts as absent (Orca deleted it on read).
    if !opts.force && path.exists() && read_snapshot(user_data).is_some() {
        return Ok(Captured::Skipped);
    }
    let surfaces = read_surfaces(os, paths, user)?;
    let v = build(opts, &surfaces, now_ms).ok_or_else(|| {
        OrcaError::Refused("cannot capture the current Claude Keychain credentials".into())
    })?;
    let text = jsjson::write_json_text(&v);
    // The snapshot's own parent: under the late userData, like the file
    // (Orca's getRuntimeMetadataDir creates it there on every access).
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| super::userdata::late_user_data(user_data).join(DIR));
    super::fsx::guard(&dir).map_err(|e| OrcaError::io("refusing", &dir, e))?;
    std::fs::create_dir_all(&dir).map_err(|e| OrcaError::io("cannot create", &dir, e))?;
    if read_text(&path)?.is_some_and(|t| t.expose() == text) {
        return Ok(Captured::Unchanged);
    }
    super::fsx::write_atomic(&path, text.as_bytes(), super::fsx::WriteOpts::PRIVATE)
        .map_err(|e| OrcaError::io("cannot write", &path, e))?;
    Ok(Captured::Written)
}

/// captureSystemDefaultSnapshotForManagedEntry: run when the active id was
/// null and `target` is about to be materialized.
pub fn capture_for_managed_entry(
    user_data: &Path,
    os: HostOs,
    paths: &RuntimePaths,
    user: &KeychainUser,
    target: &str,
    now_ms: i64,
) -> Result<Captured, OrcaError> {
    let file = read_text(&paths.credentials_path)?;
    let opts = plan_for_managed_entry(
        file.as_ref().map(|s| s.expose()),
        target,
        read_snapshot(user_data),
    );
    capture(user_data, os, paths, user, &opts, now_ms)
}

// ─── restore after a crashed switch ───────────────────────────────────────────

/// What the recovery of a crashed switch from no account does with one
/// runtime credential surface. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceAction {
    /// Already the snapshot's value, or the snapshot does not know it.
    Keep,
    /// Put the snapshot's value back. `quarantine`: file the current value
    /// first, because no stash holds it.
    Restore { quarantine: bool },
    /// The surface holds a value the crashed switch did not write (a newer
    /// system-default grant, say, after Claude Code refreshed it): keep it,
    /// and file the snapshot's value in the quarantine, so a later capture
    /// that overwrites the snapshot cannot lose it.
    KeepNewer,
}

fn same(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

/// Decide one surface. `want` is the snapshot's value (`None`: unknown,
/// e.g. a Keychain item the snapshot could not capture); `managed` is the
/// grant the crashed switch materialized (the target's stash holds it).
/// `identity_owned` is [`crash_owns_identity`]'s answer.
///
/// With the identity owned, the crashed switch finished its materialize
/// (or was inside the neutral window), so every value in `D` descends from
/// the target's grant: every surface goes back to the snapshot, and a value
/// no stash holds (the target's grant rotated by a claude run since) is
/// filed first. Without it, a surface is restored only while it still holds
/// `managed`, the crashed switch's own write, as Orca's
/// restoreSystemDefaultSnapshot gates each surface on
/// hasUnchangedRuntimeCredentials; any other value is the user's own and
/// stays. Pure.
pub fn surface_action(
    current: Option<&str>,
    want: Option<Option<&str>>,
    managed: Option<&str>,
    identity_owned: bool,
) -> SurfaceAction {
    let Some(want) = want else {
        return SurfaceAction::Keep;
    };
    match (current, want) {
        (None, None) => SurfaceAction::Keep,
        (Some(c), Some(w)) if same(c, w) => SurfaceAction::Keep,
        (Some(c), _) if managed.is_some_and(|m| same(m, c)) => {
            SurfaceAction::Restore { quarantine: false }
        }
        _ if !identity_owned => SurfaceAction::KeepNewer,
        (None, Some(_)) => SurfaceAction::Restore { quarantine: false },
        (Some(c), _) => SurfaceAction::Restore {
            quarantine: !c.trim().is_empty(),
        },
    }
}

/// Whether `D`'s `oauthAccount` shows the crashed switch's own write: it
/// names the target's account (and not the system default's), or it is
/// missing while the system default had one (only the neutral window
/// deletes it). A switch that died before its materialize leaves the
/// system default's identity, or whatever a login since wrote, and then
/// nothing in `D` is presumed to be the switch's. Pure.
pub fn crash_owns_identity(
    current: &RuntimeIdentity,
    target_oauth: Option<&Value>,
    snapshot_oauth: Option<&Value>,
) -> bool {
    let uuid = |v: Option<&Value>| v.and_then(|v| OauthIdentity::from_value(v).account_uuid);
    match current {
        RuntimeIdentity::Unreadable => false,
        RuntimeIdentity::None => snapshot_oauth.is_some(),
        RuntimeIdentity::Present(i) => {
            let target = uuid(target_oauth);
            i.account_uuid.is_some()
                && i.account_uuid == target
                && i.account_uuid != uuid(snapshot_oauth)
        }
    }
}

/// What [`restore_after_crash`] did.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// A valid snapshot existed (else `D` was only made neutral).
    pub had_snapshot: bool,
    /// The surfaces put back (names only).
    pub restored: Vec<String>,
    /// The surfaces kept because they hold a value the crashed switch did
    /// not write (names only).
    pub kept: Vec<String>,
    /// Fingerprints of displaced grants filed in the quarantine.
    pub quarantined: Vec<String>,
}

/// One credential surface of `D`: (name, current value, the snapshot's
/// value; `None` = unknown, `Some(None)` = absent).
type Surface = (&'static str, Option<SecretString>, Option<Option<String>>);

/// Put `D` back to the system default after a switch from no account died
/// half way (design section 3 recovery: "materialize `from`", where `from`
/// is the system default). A port of Orca's restoreSystemDefaultSnapshot
/// (runtime-auth-snapshot-restore.ts): each surface is decided by
/// [`surface_action`], ownership-gated as Orca is unless `D`'s identity
/// shows the crashed write ([`crash_owns_identity`]). `managed` and
/// `managed_oauth` are the target's stashed grant and identity. Nothing is
/// written until every quarantine filing succeeded, so no value that is
/// overwritten, and no snapshot value that is not put back, exists only in
/// one place. The identity goes back first, as Orca restores it, when the
/// identity or a restored surface proves the crashed write. Without a
/// valid snapshot, every grant no stash holds is quarantined, the
/// credential surfaces are left as they are, and the identity is cleared
/// only when the crashed write owns it or a surface.
#[allow(clippy::too_many_arguments)]
pub fn restore_after_crash(
    user_data: &Path,
    os: HostOs,
    paths: &RuntimePaths,
    user: &KeychainUser,
    managed: Option<&str>,
    managed_oauth: Option<&Value>,
    quarantine: &super::quarantine::Quarantine,
    now_ms: i64,
) -> Result<RestoreReport, OrcaError> {
    use super::runtime::{read_runtime_identity, restore_credentials_file, restore_identity};
    let snap = read_snapshot(user_data);
    let dir = paths.config_dir.to_string_lossy().into_owned();
    let scoped_is_legacy = keychain::runtime_service(Some(&dir)) == keychain::RUNTIME_SERVICE;
    let snap_oauth = snap
        .as_ref()
        .and_then(|s| s.get("configOauthAccount"))
        .filter(|v| !v.is_null());
    let identity_owned =
        crash_owns_identity(&read_runtime_identity(paths), managed_oauth, snap_oauth);

    let mut surfaces: Vec<Surface> = Vec::new();
    let want_of = |kind: Option<Kind>| -> Option<Option<String>> {
        let s = snap.as_ref()?;
        match kind {
            None => Some(str_of(s.get("credentialsJson"))),
            Some(k) => match read_keychain_value(Some(s), k) {
                KcValue::Captured(v) => Some(v),
                KcValue::Unknown => None,
            },
        }
    };
    surfaces.push(("file", read_text(&paths.credentials_path)?, want_of(None)));
    if os == HostOs::MacOs {
        surfaces.push((
            "scoped-keychain",
            keychain::read_runtime_scoped(Some(&dir), user)?,
            want_of(Some(Kind::Scoped)),
        ));
        if !scoped_is_legacy {
            surfaces.push((
                "legacy-keychain",
                keychain::read_runtime_scoped(None, user)?,
                want_of(Some(Kind::Legacy)),
            ));
        }
    }

    let mut report = RestoreReport {
        had_snapshot: snap.is_some(),
        ..RestoreReport::default()
    };
    let file = |value: &str, source: &str, report: &mut RestoreReport| {
        quarantine
            .file(
                value,
                super::quarantine::Reason::CrashRecovery,
                source,
                None,
                None,
                now_ms,
            )
            .map(|fp| report.quarantined.push(fp.fingerprint().to_owned()))
    };
    // Every filing first: a failure writes nothing.
    let mut plan = Vec::new();
    let mut surface_owned = false;
    for (name, cur, want) in &surfaces {
        let cur = cur.as_ref().map(|s| s.expose());
        if cur.is_some_and(|c| managed.is_some_and(|m| same(m, c))) {
            surface_owned = true;
        }
        let action = if snap.is_some() {
            surface_action(
                cur,
                want.as_ref().map(|w| w.as_deref()),
                managed,
                identity_owned,
            )
        } else {
            // No snapshot: keep every surface, but file what no stash holds
            // (a later capture or materialize may overwrite it).
            if let Some(c) = cur
                && !c.trim().is_empty()
                && !managed.is_some_and(|m| same(m, c))
            {
                file(c, name, &mut report)?;
            }
            SurfaceAction::Keep
        };
        match (action, cur) {
            (SurfaceAction::Restore { quarantine: true }, Some(c)) => file(c, name, &mut report)?,
            (SurfaceAction::KeepNewer, _) => {
                if let Some(w) = want.as_ref().and_then(|w| w.as_deref())
                    && !w.trim().is_empty()
                {
                    file(w, &format!("snapshot-{name}"), &mut report)?;
                }
            }
            _ => {}
        }
        plan.push((*name, action, want.clone().flatten()));
    }

    let Some(snap) = snap.as_ref() else {
        if identity_owned || surface_owned {
            super::runtime::clear_identity(paths)?;
        }
        return Ok(report);
    };
    let restoring = plan
        .iter()
        .any(|(_, a, _)| matches!(a, SurfaceAction::Restore { .. }));
    if identity_owned || restoring {
        let oauth = snap.get("configOauthAccount").filter(|v| !v.is_null());
        restore_identity(paths, oauth)?;
    }
    for (name, action, want) in plan {
        match action {
            SurfaceAction::Keep => continue,
            SurfaceAction::KeepNewer => {
                report.kept.push(name.to_owned());
                continue;
            }
            SurfaceAction::Restore { .. } => {}
        }
        let want = want.as_deref().filter(|w| !w.trim().is_empty());
        match name {
            "file" => restore_credentials_file(paths, want)?,
            "scoped-keychain" => match want {
                Some(v) => keychain::write_runtime_scoped(v, Some(&dir), user)?,
                None => keychain::delete_runtime_scoped(Some(&dir), user)?,
            },
            _ => match want {
                Some(v) => keychain::add_password(keychain::RUNTIME_SERVICE, &user.acct, v)?,
                None => keychain::delete_runtime_scoped(None, user)?,
            },
        }
        report.restored.push(name.to_owned());
    }
    Ok(report)
}

// ─── preserve while Orca runs ─────────────────────────────────────────────────

/// The snapshot's credential values worth keeping: every non-blank value
/// the snapshot captured (file, scoped and legacy item) that is not
/// `managed`, the grant the crashed switch materialized (its stash holds
/// that one). Deduplicated, in the order file, scoped, legacy. Pure.
pub fn snapshot_values(
    snap: &Map<String, Value>,
    managed: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut push = |name: &'static str, v: Option<String>| {
        if let Some(v) = v
            && !v.trim().is_empty()
            && !managed.is_some_and(|m| same(m, &v))
            && !out.iter().any(|(_, o)| same(o, &v))
        {
            out.push((name, v));
        }
    };
    push("snapshot-file", str_of(snap.get("credentialsJson")));
    for (name, kind) in [
        ("snapshot-scoped-keychain", Kind::Scoped),
        ("snapshot-legacy-keychain", Kind::Legacy),
    ] {
        if let KcValue::Captured(v) = read_keychain_value(Some(snap), kind) {
            push(name, v);
        }
    }
    out
}

/// File the system-default snapshot's grants in the quarantine without
/// touching `D` or anything of Orca's. This is what csm does about a
/// crashed switch from no account while Orca runs: csm never writes `D`
/// behind a running Orca (Invariant 6), and Orca's next select would
/// force-capture the half-written `D` over the snapshot, which may hold the
/// only copy of the user's own login. Returns the fingerprints filed (or
/// already there); nothing when there is no valid snapshot.
pub fn preserve_snapshot(
    user_data: &Path,
    managed: Option<&str>,
    quarantine: &super::quarantine::Quarantine,
    now_ms: i64,
) -> Result<Vec<String>, OrcaError> {
    let Some(snap) = read_snapshot(user_data) else {
        return Ok(Vec::new());
    };
    snapshot_values(&snap, managed)
        .into_iter()
        .map(|(source, v)| {
            quarantine
                .file(
                    &v,
                    super::quarantine::Reason::CrashRecovery,
                    source,
                    None,
                    None,
                    now_ms,
                )
                .map(|f| f.fingerprint().to_owned())
        })
        .collect()
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crash_restore_quarantines_only_what_no_stash_holds() {
        use SurfaceAction::*;
        for owned in [true, false] {
            // Already the snapshot's, or unknown to it: keep.
            assert_eq!(
                surface_action(Some("S"), Some(Some("S\n")), Some("B"), owned),
                Keep
            );
            assert_eq!(surface_action(Some("B"), None, Some("B"), owned), Keep);
            assert_eq!(surface_action(None, Some(None), Some("B"), owned), Keep);
            // The crashed switch's own write: restore, the stash holds it.
            assert_eq!(
                surface_action(Some("B"), Some(Some("S")), Some("B"), owned),
                Restore { quarantine: false }
            );
            assert_eq!(
                surface_action(Some("B"), Some(None), Some("B"), owned),
                Restore { quarantine: false }
            );
        }
        // The identity shows the crashed write: missing goes back, anything
        // else is filed first.
        assert_eq!(
            surface_action(None, Some(Some("S")), Some("B"), true),
            Restore { quarantine: false }
        );
        assert_eq!(
            surface_action(Some("X"), Some(Some("S")), Some("B"), true),
            Restore { quarantine: true }
        );
        assert_eq!(
            surface_action(Some("X"), Some(Some("S")), None, true),
            Restore { quarantine: true }
        );
        // It does not: a value the switch did not write is the user's own
        // (a refreshed S', a new login) and stays.
        assert_eq!(
            surface_action(Some("S2"), Some(Some("S")), Some("B"), false),
            KeepNewer
        );
        assert_eq!(
            surface_action(None, Some(Some("S")), Some("B"), false),
            KeepNewer
        );
        assert_eq!(
            surface_action(Some("C"), Some(None), None, false),
            KeepNewer
        );
    }

    #[test]
    fn snapshot_values_skip_blank_managed_and_duplicate_values() {
        let snap = json!({
            "credentialsJson": "S",
            "configOauthAccount": null,
            "scopedKeychainCredentialsJson": "S\n",
            "legacyKeychainCredentialsJson": "L",
            "scopedKeychainCredentialsCaptured": true,
            "legacyKeychainCredentialsCaptured": true,
            "capturedAt": 1
        });
        let m = snap.as_object().unwrap();
        assert_eq!(
            snapshot_values(m, Some("B")),
            vec![
                ("snapshot-file", "S".to_owned()),
                ("snapshot-legacy-keychain", "L".to_owned())
            ]
        );
        // The crashed switch's own grant is in its stash already.
        assert_eq!(
            snapshot_values(m, Some("L")),
            vec![("snapshot-file", "S".to_owned())]
        );
        // An uncaptured item is unknown, a blank value is nothing.
        let snap = json!({
            "credentialsJson": " ",
            "configOauthAccount": null,
            "legacyKeychainCredentialsJson": "L",
            "legacyKeychainCredentialsCaptured": false,
            "capturedAt": 1
        });
        assert!(snapshot_values(snap.as_object().unwrap(), None).is_empty());
    }

    #[test]
    fn the_crashed_write_owns_the_identity_only_when_it_shows() {
        let b = json!({"accountUuid": "u-b"});
        let s = json!({"accountUuid": "u-s"});
        let present = |u: &str| {
            RuntimeIdentity::Present(OauthIdentity::from_value(&json!({"accountUuid": u})))
        };
        assert!(crash_owns_identity(&present("u-b"), Some(&b), Some(&s)));
        assert!(!crash_owns_identity(&present("u-s"), Some(&b), Some(&s)));
        assert!(!crash_owns_identity(&present("u-c"), Some(&b), Some(&s)));
        // The system default is the target's account too: not provable.
        assert!(!crash_owns_identity(&present("u-b"), Some(&b), Some(&b)));
        // Missing: the neutral window, when the system default had one.
        assert!(crash_owns_identity(
            &RuntimeIdentity::None,
            Some(&b),
            Some(&s)
        ));
        assert!(!crash_owns_identity(&RuntimeIdentity::None, Some(&b), None));
        assert!(!crash_owns_identity(
            &RuntimeIdentity::Unreadable,
            Some(&b),
            Some(&s)
        ));
    }

    use crate::orca::runtime::runtime_paths;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn snapshot_shape_rules() {
        assert!(is_snapshot(
            &json!({"credentialsJson": null, "keychainCredentialsJson": null})
        ));
        assert!(is_snapshot(
            &json!({"credentialsJson": "x", "scopedKeychainCredentialsCaptured": false, "legacyKeychainCredentialsJson": null})
        ));
        assert!(!is_snapshot(&json!({"credentialsJson": null})));
        assert!(!is_snapshot(&json!({"keychainCredentialsJson": null})));
        assert!(!is_snapshot(
            &json!({"credentialsJson": 1, "keychainCredentialsJson": null})
        ));
        assert!(!is_snapshot(
            &json!({"credentialsJson": null, "keychainCredentialsJson": null, "capturedAt": "1"})
        ));
        assert!(!is_snapshot(
            &json!({"credentialsJson": null, "keychainCredentialsJson": null, "legacyKeychainCredentialsCaptured": 1})
        ));
        assert!(!is_snapshot(&json!([])));
    }

    #[test]
    fn keychain_values_prefer_the_previous_capture_for_the_managed_grant() {
        let prev = obj(
            json!({"credentialsJson": null, "keychainCredentialsJson": "agg", "scopedKeychainCredentialsJson": null, "legacyKeychainCredentialsCaptured": false}),
        );
        assert_eq!(
            read_keychain_value(Some(&prev), Kind::Scoped),
            KcValue::Captured(None)
        );
        assert_eq!(
            read_keychain_value(Some(&prev), Kind::Legacy),
            KcValue::Unknown
        );
        let old = obj(json!({"credentialsJson": null, "keychainCredentialsJson": "agg"}));
        assert_eq!(
            read_keychain_value(Some(&old), Kind::Legacy),
            KcValue::Captured(Some("agg".into()))
        );
        assert_eq!(
            read_keychain_value(None, Kind::Scoped),
            KcValue::Captured(None)
        );
        // Equal to the managed grant: the previous captured value.
        assert_eq!(
            snapshot_keychain_credentials(Some("M"), Some(&old), Kind::Scoped, Some("M")),
            Some("agg".into())
        );
        // Unknown in the previous: keep the live value.
        assert_eq!(
            snapshot_keychain_credentials(Some("M"), Some(&prev), Kind::Legacy, Some("M")),
            Some("M".into())
        );
        // No previous, or a different value: the live value.
        assert_eq!(
            snapshot_keychain_credentials(Some("M"), None, Kind::Scoped, Some("M")),
            Some("M".into())
        );
        assert_eq!(
            snapshot_keychain_credentials(Some("X"), Some(&old), Kind::Scoped, Some("M")),
            Some("X".into())
        );
        assert_eq!(
            snapshot_keychain_credentials(None, Some(&old), Kind::Scoped, Some("M")),
            None
        );
    }

    #[test]
    fn force_and_override_rules() {
        let prev = obj(json!({"credentialsJson": "sys", "keychainCredentialsJson": null}));
        let p = plan_for_managed_entry(Some("other"), "T", Some(prev.clone()));
        assert!(p.force && p.credentials_override.is_none() && p.managed.as_deref() == Some("T"));
        let p = plan_for_managed_entry(None, "T", None);
        assert!(p.force && p.previous.is_none());
        let p = plan_for_managed_entry(Some("T"), "T", Some(prev));
        assert!(p.force);
        assert_eq!(p.credentials_override, Some(Some("sys".into())));
        let p = plan_for_managed_entry(Some("T"), "T", None);
        assert!(!p.force && p.managed.is_none());
    }

    #[test]
    fn build_fails_closed_on_a_keychain_error_and_orders_fields() {
        let s = Surfaces {
            file: Some("F".into()),
            aggregate: Some("A".into()),
            scoped: Ok(Some("S".into())),
            legacy: Ok(None),
            config_oauth: json!({"accountUuid": "u"}),
        };
        let opts = plan_for_managed_entry(Some("F"), "T", None);
        let v = build(
            &CaptureOpts {
                force: true,
                ..opts
            },
            &s,
            42,
        )
        .unwrap();
        assert_eq!(
            jsjson::stringify(&v),
            r#"{"credentialsJson":"F","configOauthAccount":{"accountUuid":"u"},"keychainCredentialsJson":"A","scopedKeychainCredentialsJson":"S","legacyKeychainCredentialsJson":null,"scopedKeychainCredentialsCaptured":true,"legacyKeychainCredentialsCaptured":true,"capturedAt":42}"#
        );
        assert!(is_snapshot(&v));
        let bad = Surfaces {
            legacy: Err(()),
            ..s
        };
        assert!(build(&opts_force(), &bad, 1).is_none());
    }

    fn opts_force() -> CaptureOpts {
        CaptureOpts {
            force: true,
            credentials_override: None,
            previous: None,
            managed: None,
        }
    }

    #[test]
    fn capture_writes_once_then_skips_unless_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path().join("ud");
        let d = tmp.path().join("D");
        std::fs::create_dir_all(&d).unwrap();
        let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
        std::fs::write(&paths.credentials_path, "SYS").unwrap();
        std::fs::write(
            &paths.config_path,
            r#"{"oauthAccount":{"accountUuid":"u-sys"}}"#,
        )
        .unwrap();
        let user = KeychainUser {
            acct: "t".into(),
            delete_accts: vec!["t".into()],
        };
        // Runtime equals the target and no snapshot: a plain capture.
        let r = capture_for_managed_entry(&ud, HostOs::Linux, &paths, &user, "SYS", 1).unwrap();
        assert_eq!(r, Captured::Written);
        let text = std::fs::read_to_string(snapshot_path(&ud)).unwrap();
        assert!(text.ends_with("}\n") && text.contains("\"credentialsJson\": \"SYS\""));
        #[cfg(unix)]
        assert_eq!(crate::orca::fsx::mode_of(&snapshot_path(&ud)), Some(0o600));
        // Again: forced (a snapshot exists), keeping the old credentialsJson.
        std::fs::write(&paths.credentials_path, "SYS").unwrap();
        let r = capture_for_managed_entry(&ud, HostOs::Linux, &paths, &user, "SYS", 2).unwrap();
        assert_eq!(r, Captured::Written);
        let r = capture(
            &ud,
            HostOs::Linux,
            &paths,
            &user,
            &CaptureOpts {
                force: false,
                ..opts_force()
            },
            3,
        )
        .unwrap();
        assert_eq!(r, Captured::Skipped);
        // An invalid snapshot counts as absent.
        std::fs::write(snapshot_path(&ud), "[]").unwrap();
        let r = capture(
            &ud,
            HostOs::Linux,
            &paths,
            &user,
            &CaptureOpts {
                force: false,
                ..opts_force()
            },
            4,
        )
        .unwrap();
        assert_eq!(r, Captured::Written);
        assert!(read_snapshot(&ud).is_some());
    }

    /// A userData named `orca` on a case-sensitive filesystem: the snapshot
    /// goes under the late sibling `Orca`, and the capture creates that
    /// dir itself (Orca may never have touched it, for example when csm
    /// added every account offline). Nothing lands under the canonical dir.
    /// On a case-insensitive filesystem both names are one dir and the
    /// capture still writes the file.
    #[cfg(unix)]
    #[test]
    fn capture_creates_the_late_snapshot_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path().join("orca");
        std::fs::create_dir_all(ud.join("profiles")).unwrap();
        let d = tmp.path().join("D");
        std::fs::create_dir_all(&d).unwrap();
        let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
        std::fs::write(&paths.credentials_path, "SYS").unwrap();
        let user = KeychainUser {
            acct: "t".into(),
            delete_accts: vec!["t".into()],
        };
        let late = crate::orca::userdata::late_user_data(&ud);
        let r = capture_for_managed_entry(&ud, HostOs::Linux, &paths, &user, "SYS", 1).unwrap();
        assert_eq!(r, Captured::Written);
        assert_eq!(snapshot_path(&ud), late.join(DIR).join(FILE));
        assert!(snapshot_path(&ud).is_file());
        if late != ud {
            assert!(
                !ud.join(DIR).exists(),
                "no stray snapshot dir under the canonical userData"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_capture_reads_the_runtime_items_and_refuses_on_error() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path().join("ud");
        let d = tmp.path().join("D");
        std::fs::create_dir_all(&d).unwrap();
        let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
        let user = KeychainUser {
            acct: "t".into(),
            delete_accts: vec!["t".into()],
        };
        let dir = d.to_str().unwrap().to_owned();
        keychain::write_runtime_scoped("SCOPED", Some(&dir), &user).unwrap();
        let r = capture_for_managed_entry(&ud, HostOs::MacOs, &paths, &user, "T", 5).unwrap();
        assert_eq!(r, Captured::Written);
        let snap = read_snapshot(&ud).unwrap();
        assert_eq!(snap["scopedKeychainCredentialsJson"], json!("SCOPED"));
        assert_eq!(snap["keychainCredentialsJson"], json!("SCOPED"));
        assert_eq!(snap["legacyKeychainCredentialsJson"], Value::Null);
        assert_eq!(snap["credentialsJson"], Value::Null);
        fake.fail_find(keychain::RUNTIME_SERVICE, true);
        assert!(matches!(
            capture_for_managed_entry(&ud, HostOs::MacOs, &paths, &user, "T", 6),
            Err(OrcaError::Refused(_))
        ));
    }
}
