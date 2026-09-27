//! Orca's per-account credential stash: the path checks, reads, and writes.
//!
//! Layout (Orca 1.4.209, M:248481-248532): `<userData>/claude-accounts/<id>/auth/`
//! holds the marker `.orca-managed-claude-auth` (`"<id>\n"`), the profile
//! metadata `oauth-account.json` (Claude Code's `oauthAccount` verbatim), and
//! off macOS the credential file `.credentials.json`. On macOS the credential
//! lives in the Keychain instead: service `Orca Claude Code Managed
//! Credentials`, account `<id>`.
//!
//! Before any read, a port of Orca's Q2i must pass ([`verify_auth_dir`]): the
//! auth dir exists and is not a symlink, its realpath lies under the stash
//! root's realpath, the relative path is exactly `<id>/auth`, and the marker
//! is a regular file whose trimmed content is `<id>`. A child file is read
//! only when it is a regular, non-symlink file whose realpath lies inside the
//! auth dir (Orca's e4i, which gates both Y4 reads and X4 writes).
//!
//! Orca adopts a missing marker (writes it, `flag: "wx"`) whenever its
//! assertOwned resolves the stash. csm's read path does not write: plain
//! [`Stash::open`] refuses a missing marker ([`StashError::MarkerMissing`]),
//! [`Stash::open_for_read`] (the read-back) reads such a stash as Orca does
//! after adopting, and the write path ([`Stash::open_for_write`]) adopts it
//! exactly as Orca does.
//!
//! Writes (A9i, M:248485-248543), each after a fresh Q2i:
//! - [`create`]: `mkdir -p <root>` and `<root>/<id>/auth` with 0700, marker
//!   `"<id>\n"` 0600;
//! - [`Stash::write_credentials`]: the Keychain stash item on macOS, else
//!   `.credentials.json` through X4, exactly the given string;
//! - [`Stash::write_oauth_account`]: `oauth-account.json` =
//!   `JSON.stringify(v, null, 2) + "\n"` through X4;
//! - [`Stash::remove`]: `rm -rf <root>/<id>`, then delete the Keychain item,
//!   ignoring its failure as Orca does.
//!
//! X4 refuses to replace an existing child that is not a regular,
//! non-symlink file inside the auth dir, and writes through a
//! `<path>.<pid>.<uuid>.tmp` rename with mode 0600.
//!
//! Credential bytes (design section 2, S14): the Keychain read is trimmed
//! (Orca's T()), the file is read verbatim (Y4). csm keeps exactly that
//! string and never re-serializes it.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::fsx::{self, WriteOpts};
use super::keychain::{self, KeychainError};
use super::userdata::{HostOs, claude_accounts_root};
use super::{OrcaError, SecretString};

/// The ownership marker in every auth dir.
pub const MARKER_FILE: &str = ".orca-managed-claude-auth";
/// Claude Code's `oauthAccount`, as Orca stashed it.
pub const OAUTH_ACCOUNT_FILE: &str = "oauth-account.json";
/// The credential file (off macOS).
pub const CREDENTIALS_FILE: &str = ".credentials.json";

/// Cap on one stash child (a credential is a few kilobytes).
const CHILD_CAP: u64 = 1024 * 1024;

// ─── errors ───────────────────────────────────────────────────────────────────

/// Why a stash was refused. Never quotes file content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StashError {
    #[error("the stash root or the auth dir does not exist")]
    Missing,
    #[error("the auth dir is a symlink")]
    Symlink,
    #[error("the auth dir is not inside the stash root")]
    Outside,
    #[error("the auth dir is not <id>/auth under the stash root")]
    BadShape,
    #[error("the ownership marker is missing (Orca adopts it on its next use)")]
    MarkerMissing,
    #[error("the ownership marker does not name this account")]
    MarkerMismatch,
    #[error("stash file {0} is not a regular file inside the auth dir")]
    ChildNotOwned(&'static str),
    #[error("stash file {0} is not valid JSON")]
    ChildNotJson(&'static str),
    #[error("cannot read the stash: {0}")]
    Io(String),
    #[error("cannot write the stash: {0}")]
    Write(String),
    #[error("a stash for this account id already exists")]
    Exists,
    #[error(transparent)]
    Keychain(#[from] KeychainError),
}

impl From<StashError> for OrcaError {
    fn from(e: StashError) -> Self {
        OrcaError::Refused(format!("stash: {e}"))
    }
}

fn io_err(e: std::io::Error) -> StashError {
    StashError::Io(e.kind().to_string())
}

// ─── Q2i / e4i ────────────────────────────────────────────────────────────────

/// `<userData>/claude-accounts/<id>/auth`, the path Orca's `create()` makes.
pub fn default_auth_dir(user_data: &Path, id: &str) -> PathBuf {
    claude_accounts_root(user_data).join(id).join("auth")
}

/// The `managedAuthPath` a record stores for a verified auth dir: the path
/// Orca's own `create()` would store, which is Node's JavaScript
/// `realpathSync` output. On Windows Rust's `canonicalize` returns the
/// verbatim form (`\\?\C:\…`, `\\?\UNC\server\share\…`) that Node never
/// produces, and Orca's ownership check (`resolveOwnedClaudeManagedAuthPath`
/// compares the candidate's realpath against a plain `C:\…` root by
/// prefix) would reject such a record: listed, but never selectable,
/// readable or removable. Strip the verbatim prefix. A path without one is
/// returned as is, so this is a no-op off Windows. Pure.
pub fn record_path(real: &str) -> String {
    if let Some(rest) = real.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match real.strip_prefix(r"\\?\") {
        // Only a drive path has a plain spelling (`C:\…`); any other
        // verbatim form (a volume GUID) is kept.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => real.to_owned(),
    }
}

/// An account id usable as one path component.
fn id_is_component(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0'])
        && !id.chars().any(char::is_control)
}

fn is_symlink(p: &Path) -> Result<bool, StashError> {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .map_err(io_err)
}

/// Orca's $2i: the marker is a regular non-symlink file whose trimmed text
/// is `id`.
fn check_marker(auth: &Path, id: &str) -> Result<(), StashError> {
    let marker = auth.join(MARKER_FILE);
    let meta = match std::fs::symlink_metadata(&marker) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StashError::MarkerMissing);
        }
        Err(e) => return Err(io_err(e)),
    };
    if !meta.file_type().is_file() {
        return Err(StashError::MarkerMismatch);
    }
    let text = super::read_capped(&marker, 4096).map_err(|_| StashError::MarkerMismatch)?;
    match text {
        Some(t) if t.trim() == id => Ok(()),
        _ => Err(StashError::MarkerMismatch),
    }
}

/// Port of Orca's Q2i (without the marker adoption, which writes): the
/// verified realpath of `id`'s auth dir under `root`.
pub fn verify_auth_dir(root: &Path, id: &str, auth_dir: &Path) -> Result<PathBuf, StashError> {
    verify_auth_dir_with(root, id, auth_dir, false)
}

/// [`verify_auth_dir`]; `allow_missing_marker` accepts a stash whose only
/// defect is a missing marker (the one Orca adopts).
fn verify_auth_dir_with(
    root: &Path,
    id: &str,
    auth_dir: &Path,
    allow_missing_marker: bool,
) -> Result<PathBuf, StashError> {
    if !id_is_component(id) || !auth_dir.is_absolute() {
        return Err(StashError::BadShape);
    }
    if !auth_dir.exists() || !root.exists() {
        return Err(StashError::Missing);
    }
    if is_symlink(auth_dir)? {
        return Err(StashError::Symlink);
    }
    let real = std::fs::canonicalize(auth_dir).map_err(io_err)?;
    let real_root = std::fs::canonicalize(root).map_err(io_err)?;
    let rel = match real.strip_prefix(&real_root) {
        Ok(r) if !r.as_os_str().is_empty() => r,
        _ => return Err(StashError::Outside),
    };
    let parts: Vec<Component<'_>> = rel.components().collect();
    let shape_ok = matches!(
        parts.as_slice(),
        [Component::Normal(a), Component::Normal(b)] if *a == std::ffi::OsStr::new(id) && *b == "auth"
    );
    if !shape_ok {
        return Err(StashError::BadShape);
    }
    match check_marker(&real, id) {
        Err(StashError::MarkerMissing) if allow_missing_marker => {}
        other => other?,
    }
    Ok(real)
}

/// Orca's e4i + Y4 for one child: `Ok(None)` when absent, the bytes when it
/// is a regular non-symlink file inside `auth`, else a refusal.
fn read_child(auth: &Path, name: &'static str) -> Result<Option<Vec<u8>>, StashError> {
    let path = auth.join(name);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(e)),
    };
    if !meta.file_type().is_file() {
        return Err(StashError::ChildNotOwned(name));
    }
    let real_auth = std::fs::canonicalize(auth).map_err(io_err)?;
    let real = std::fs::canonicalize(&path).map_err(io_err)?;
    if real.parent() != Some(real_auth.as_path()) {
        return Err(StashError::ChildNotOwned(name));
    }
    super::read_capped_bytes(&real, CHILD_CAP).map_err(io_err)
}

// ─── the stash view ───────────────────────────────────────────────────────────

/// A stash whose auth dir passed Q2i.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    pub id: String,
    /// The verified realpath of the auth dir.
    pub auth_dir: PathBuf,
}

impl Stash {
    /// Verify `id`'s stash under `user_data`. `managed_auth_path` is the
    /// record's `managedAuthPath`; `None` (an RPC record carries none) uses
    /// the path Orca creates, which Q2i then checks the same way.
    pub fn open(
        user_data: &Path,
        id: &str,
        managed_auth_path: Option<&str>,
    ) -> Result<Stash, StashError> {
        let auth = managed_auth_path
            .map(PathBuf::from)
            .unwrap_or_else(|| default_auth_dir(user_data, id));
        let real = verify_auth_dir(&claude_accounts_root(user_data), id, &auth)?;
        Ok(Stash {
            id: id.to_owned(),
            auth_dir: real,
        })
    }

    /// `oauth-account.json`: `Ok(None)` when absent, empty, or JSON `null`.
    /// That is Orca's `readManagedOauthAccount`: `contents ? JSON.parse(..)
    /// : null`, and a parsed `null` is the same "no identity" every caller
    /// sees. It matters to materialize: `writeRuntimeOauthAccount(null)`
    /// deletes the runtime `oauthAccount` key instead of writing a `null`.
    pub fn oauth_account(&self) -> Result<Option<Value>, StashError> {
        let Some(bytes) = read_child(&self.auth_dir, OAUTH_ACCOUNT_FILE)? else {
            return Ok(None);
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice::<Value>(&bytes)
            .map(|v| (!v.is_null()).then_some(v))
            .map_err(|_| StashError::ChildNotJson(OAUTH_ACCOUNT_FILE))
    }

    /// The stashed credential JSON, exactly as Orca reads it: the Keychain
    /// item trimmed on macOS, the file verbatim elsewhere. `Ok(None)` when
    /// there is none.
    pub fn credentials(&self, os: HostOs) -> Result<Option<SecretString>, StashError> {
        match os {
            HostOs::MacOs => Ok(keychain::find_stash(&self.id)?),
            HostOs::Linux | HostOs::Windows => {
                let Some(bytes) = read_child(&self.auth_dir, CREDENTIALS_FILE)? else {
                    return Ok(None);
                };
                match String::from_utf8(bytes) {
                    Ok(text) => Ok(Some(SecretString::new(text))),
                    Err(e) => {
                        let mut b = e.into_bytes();
                        super::zero(&mut b);
                        Err(StashError::ChildNotJson(CREDENTIALS_FILE))
                    }
                }
            }
        }
    }
}

/// Orca's isValidCredentialsJsonObject: `claudeAiOauth.accessToken` is a
/// string that is non-blank after trimming. Pure.
pub fn credentials_are_valid(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return false;
    };
    v.get("claudeAiOauth")
        .filter(|o| o.is_object())
        .and_then(|o| o.get("accessToken"))
        .and_then(Value::as_str)
        .is_some_and(|t| !t.trim().is_empty())
}

// ─── the write side ───────────────────────────────────────────────────────────

fn write_err(e: std::io::Error) -> StashError {
    StashError::Write(e.kind().to_string())
}

/// Orca's getRoot: `<userData>/claude-accounts`, created 0700.
pub fn ensure_root(user_data: &Path) -> Result<PathBuf, StashError> {
    let root = claude_accounts_root(user_data);
    fsx::create_dir_all(&root, 0o700).map_err(write_err)?;
    Ok(root)
}

/// Orca's A9i.create for a host account: the auth dir and its marker. A
/// stash dir that already exists for `id` is refused (csm always creates
/// with a fresh id).
pub fn create(user_data: &Path, id: &str) -> Result<Stash, StashError> {
    if !id_is_component(id) {
        return Err(StashError::BadShape);
    }
    let root = ensure_root(user_data)?;
    let account_dir = root.join(id);
    if std::fs::symlink_metadata(&account_dir).is_ok() {
        return Err(StashError::Exists);
    }
    let auth = account_dir.join("auth");
    fsx::create_dir_all(&auth, 0o700).map_err(write_err)?;
    fsx::write_new(&auth.join(MARKER_FILE), format!("{id}\n").as_bytes(), 0o600)
        .map_err(write_err)?;
    Stash::open(user_data, id, None)
}

/// Orca's e4i as a predicate: an existing child is a regular non-symlink
/// file inside `auth`.
fn child_is_owned(auth: &Path, path: &Path) -> Result<bool, StashError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(io_err(e)),
    };
    if !meta.file_type().is_file() {
        return Ok(false);
    }
    let real_auth = std::fs::canonicalize(auth).map_err(io_err)?;
    let real = std::fs::canonicalize(path).map_err(io_err)?;
    Ok(real.parent() == Some(real_auth.as_path()))
}

impl Stash {
    /// Orca's read-side resolve with `adoptLegacyMarker: true`
    /// (resolveOwnedClaudeManagedAuthPath, as its read-back and usage
    /// readers call it), minus the write: a stash whose only defect is a
    /// missing marker is read as Orca reads it right after adopting one.
    /// For reads only; every write goes through [`Stash::open_for_write`],
    /// which adopts the marker exactly as Orca does.
    pub fn open_for_read(
        user_data: &Path,
        id: &str,
        managed_auth_path: Option<&str>,
    ) -> Result<Stash, StashError> {
        let auth = managed_auth_path
            .map(PathBuf::from)
            .unwrap_or_else(|| default_auth_dir(user_data, id));
        let real = verify_auth_dir_with(&claude_accounts_root(user_data), id, &auth, true)?;
        Ok(Stash {
            id: id.to_owned(),
            auth_dir: real,
        })
    }

    /// Orca's assertOwned for a write: Q2i with the legacy-marker adoption
    /// (a missing marker is written with `flag: "wx"`, then re-checked).
    pub fn open_for_write(
        user_data: &Path,
        id: &str,
        managed_auth_path: Option<&str>,
    ) -> Result<Stash, StashError> {
        match Stash::open(user_data, id, managed_auth_path) {
            Err(StashError::MarkerMissing) => {
                let auth = managed_auth_path
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_auth_dir(user_data, id));
                let real = std::fs::canonicalize(&auth).map_err(io_err)?;
                fsx::write_new(&real.join(MARKER_FILE), format!("{id}\n").as_bytes(), 0o600)
                    .map_err(write_err)?;
                Stash::open(user_data, id, managed_auth_path)
            }
            other => other,
        }
    }

    /// Re-run Q2i on this stash (right before every write).
    fn recheck(&self, user_data: &Path) -> Result<PathBuf, StashError> {
        verify_auth_dir(&claude_accounts_root(user_data), &self.id, &self.auth_dir)
    }

    /// Orca's X4: write one child with mode 0600 through a tmp rename.
    fn write_child(
        &self,
        user_data: &Path,
        name: &'static str,
        bytes: &[u8],
    ) -> Result<(), StashError> {
        let auth = self.recheck(user_data)?;
        let path = auth.join(name);
        if !child_is_owned(&auth, &path)? {
            return Err(StashError::ChildNotOwned(name));
        }
        fsx::write_atomic(&path, bytes, WriteOpts::PRIVATE).map_err(write_err)
    }

    /// Orca's writeCredentials: exactly `json`, never re-serialized.
    pub fn write_credentials(
        &self,
        user_data: &Path,
        os: HostOs,
        json: &str,
    ) -> Result<(), StashError> {
        match os {
            HostOs::MacOs => {
                self.recheck(user_data)?;
                Ok(keychain::write_stash(&self.id, json)?)
            }
            HostOs::Linux | HostOs::Windows => {
                self.write_child(user_data, CREDENTIALS_FILE, json.as_bytes())
            }
        }
    }

    /// Orca's writeOauthAccount: `JSON.stringify(v, null, 2) + "\n"`.
    pub fn write_oauth_account(&self, user_data: &Path, v: &Value) -> Result<(), StashError> {
        let text = super::jsjson::write_json_text(v);
        self.write_child(user_data, OAUTH_ACCOUNT_FILE, text.as_bytes())
    }

    /// Orca's writeAuth: credentials, then the profile metadata.
    pub fn write_auth(
        &self,
        user_data: &Path,
        os: HostOs,
        credentials: &str,
        oauth_account: &Value,
    ) -> Result<(), StashError> {
        self.write_credentials(user_data, os, credentials)?;
        self.write_oauth_account(user_data, oauth_account)
    }

    /// Orca's remove: `rm -rf <root>/<id>` after Q2i, then the Keychain item
    /// (macOS), whose failure Orca ignores. Returns that ignored failure so
    /// callers can report it.
    pub fn remove(self, user_data: &Path, os: HostOs) -> Result<Option<KeychainError>, StashError> {
        let auth = self.recheck(user_data)?;
        let account_dir = auth.parent().ok_or(StashError::BadShape)?;
        fsx::remove_dir_all(account_dir).map_err(write_err)?;
        if os == HostOs::MacOs {
            return Ok(keychain::delete_stash(&self.id).err());
        }
        Ok(None)
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::orca::testsupport::FakeSecurity;
    use crate::orca::testsupport::make_stash;

    /// The record keeps Node's realpath spelling: Rust's Windows verbatim
    /// prefix goes, everything else is untouched.
    #[test]
    fn record_path_drops_the_windows_verbatim_prefix() {
        assert_eq!(
            record_path(r"\\?\C:\Users\example\AppData\Roaming\orca\claude-accounts\id\auth"),
            r"C:\Users\example\AppData\Roaming\orca\claude-accounts\id\auth"
        );
        assert_eq!(
            record_path(r"\\?\UNC\server\share\orca\claude-accounts\id\auth"),
            r"\\server\share\orca\claude-accounts\id\auth"
        );
        // A volume GUID path has no plain spelling: kept.
        let guid = r"\\?\Volume{0000}\orca\claude-accounts\id\auth";
        assert_eq!(record_path(guid), guid);
        assert_eq!(
            record_path("/Users/example/Library/Application Support/orca/claude-accounts/id/auth"),
            "/Users/example/Library/Application Support/orca/claude-accounts/id/auth"
        );
        assert_eq!(record_path(r"C:\x\auth"), r"C:\x\auth");
    }

    #[test]
    fn a_well_formed_stash_passes_q2i() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        make_stash(
            ud,
            "id-1",
            Some(br#"{"accountUuid":"u-1"}"#),
            Some(b"{\"x\":1}"),
        );
        let s = Stash::open(ud, "id-1", None).unwrap();
        assert_eq!(s.id, "id-1");
        assert!(s.auth_dir.ends_with("claude-accounts/id-1/auth"));
        let explicit = default_auth_dir(ud, "id-1");
        let s2 = Stash::open(ud, "id-1", Some(explicit.to_str().unwrap())).unwrap();
        assert_eq!(s, s2);
    }

    /// When the disk shows Orca keeping stashes under the late userData
    /// (`<appData>/Orca`) and the store under the canonical one
    /// (`<appData>/orca`), two dirs on a case-sensitive filesystem, csm must
    /// look where Orca put the stash; on a case-insensitive one they are one
    /// dir. (The canonical-only layout is `stashes_under_the_canonical_dir_stay_there`.)
    #[cfg(unix)]
    #[test]
    fn stashes_live_under_the_late_userdata() {
        use crate::orca::userdata::late_user_data;
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join(".config").join("orca");
        std::fs::create_dir_all(&ud).unwrap();
        std::fs::write(ud.join("orca-data.json"), "{}").unwrap();
        let late = dir.path().join(".config").join("Orca");
        let case_sensitive = !late.exists();

        // Orca's own layout: the stash under the late dir, the record's
        // managedAuthPath spelled with it.
        let auth = late.join("claude-accounts").join("id-1").join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(auth.join(MARKER_FILE), "id-1\n").unwrap();
        let s = Stash::open(&ud, "id-1", Some(auth.to_str().unwrap())).unwrap();
        assert_eq!(Stash::open(&ud, "id-1", None).unwrap(), s);

        if case_sensitive {
            assert_eq!(late_user_data(&ud), late);
            assert!(default_auth_dir(&ud, "id-1").starts_with(&late));
            // A stash under the canonical dir is not where Orca looks.
            let stray = ud.join("claude-accounts").join("id-2").join("auth");
            std::fs::create_dir_all(&stray).unwrap();
            std::fs::write(stray.join(MARKER_FILE), "id-2\n").unwrap();
            assert_eq!(
                Stash::open(&ud, "id-2", Some(stray.to_str().unwrap())),
                Err(StashError::Outside)
            );
            assert_eq!(
                crate::orca::sysdefault::snapshot_path(&ud),
                late.join(crate::orca::sysdefault::DIR)
                    .join(crate::orca::sysdefault::FILE)
            );
        } else {
            assert_eq!(late_user_data(&ud), ud);
        }

        // A sandbox userData not named `orca` never moves.
        let sandbox = dir.path().join("sandbox-ud");
        std::fs::create_dir_all(&sandbox).unwrap();
        assert_eq!(late_user_data(&sandbox), sandbox);
    }

    #[test]
    fn q2i_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        assert_eq!(Stash::open(ud, "id-1", None), Err(StashError::Missing));
        make_stash(ud, "id-1", None, None);

        // Wrong id for the path, and a marker naming another id.
        let p = default_auth_dir(ud, "id-1");
        assert_eq!(
            Stash::open(ud, "id-2", Some(p.to_str().unwrap())),
            Err(StashError::BadShape)
        );
        std::fs::write(p.join(MARKER_FILE), "id-9\n").unwrap();
        assert_eq!(
            Stash::open(ud, "id-1", None),
            Err(StashError::MarkerMismatch)
        );
        std::fs::remove_file(p.join(MARKER_FILE)).unwrap();
        assert_eq!(
            Stash::open(ud, "id-1", None),
            Err(StashError::MarkerMissing)
        );
        std::fs::write(p.join(MARKER_FILE), "  id-1  \n").unwrap();
        assert!(
            Stash::open(ud, "id-1", None).is_ok(),
            "the marker is trimmed"
        );

        // Ids that are not one path component, and relative paths.
        for bad in ["", "..", "a/b", "a\\b"] {
            assert_eq!(
                Stash::open(ud, bad, None),
                Err(StashError::BadShape),
                "{bad:?}"
            );
        }
        assert_eq!(
            Stash::open(ud, "id-1", Some("claude-accounts/id-1/auth")),
            Err(StashError::BadShape)
        );

        // A dir outside the root, and one level too deep.
        let outside = ud.join("elsewhere").join("id-1").join("auth");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(MARKER_FILE), "id-1\n").unwrap();
        assert_eq!(
            Stash::open(ud, "id-1", Some(outside.to_str().unwrap())),
            Err(StashError::Outside)
        );
        let deep = claude_accounts_root(ud).join("x").join("id-1").join("auth");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            Stash::open(ud, "id-1", Some(deep.to_str().unwrap())),
            Err(StashError::BadShape)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_for_the_dir_the_marker_and_children() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        make_stash(ud, "real", None, None);
        // The auth dir itself a symlink to a valid stash.
        let link_parent = claude_accounts_root(ud).join("id-l");
        std::fs::create_dir_all(&link_parent).unwrap();
        symlink(default_auth_dir(ud, "real"), link_parent.join("auth")).unwrap();
        assert_eq!(Stash::open(ud, "id-l", None), Err(StashError::Symlink));

        // A marker that is a symlink.
        make_stash(ud, "id-m", None, None);
        let m = default_auth_dir(ud, "id-m").join(MARKER_FILE);
        std::fs::remove_file(&m).unwrap();
        let target = ud.join("marker-target");
        std::fs::write(&target, "id-m\n").unwrap();
        symlink(&target, &m).unwrap();
        assert_eq!(
            Stash::open(ud, "id-m", None),
            Err(StashError::MarkerMismatch)
        );

        // A child that is a symlink is never read.
        make_stash(ud, "id-c", None, None);
        let secret = ud.join("outside.json");
        std::fs::write(&secret, "{}").unwrap();
        symlink(
            &secret,
            default_auth_dir(ud, "id-c").join(OAUTH_ACCOUNT_FILE),
        )
        .unwrap();
        let s = Stash::open(ud, "id-c", None).unwrap();
        assert_eq!(
            s.oauth_account(),
            Err(StashError::ChildNotOwned(OAUTH_ACCOUNT_FILE))
        );
    }

    #[test]
    fn file_credentials_are_read_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let raw = b"{\"claudeAiOauth\":{\"accessToken\":\"tok\"}}\n  ";
        make_stash(ud, "id-1", Some(b"{\"accountUuid\":\"u-1\"}\n"), Some(raw));
        let s = Stash::open(ud, "id-1", None).unwrap();
        let c = s.credentials(HostOs::Linux).unwrap().unwrap();
        assert_eq!(c.expose().as_bytes(), raw, "no trim, no added newline");
        assert!(!format!("{c:?}").contains("tok"));
        assert_eq!(s.oauth_account().unwrap().unwrap()["accountUuid"], "u-1");

        make_stash(ud, "id-2", None, None);
        let s = Stash::open(ud, "id-2", None).unwrap();
        assert!(s.credentials(HostOs::Windows).unwrap().is_none());
        assert!(s.oauth_account().unwrap().is_none());
        std::fs::write(
            default_auth_dir(ud, "id-2").join(OAUTH_ACCOUNT_FILE),
            "{nope",
        )
        .unwrap();
        assert_eq!(
            s.oauth_account(),
            Err(StashError::ChildNotJson(OAUTH_ACCOUNT_FILE))
        );

        // A capture with no oauthAccount persists `null` (Orca and csm both
        // write it); like Orca's readManagedOauthAccount that reads as none,
        // so materialize deletes the runtime key instead of writing a null.
        for body in ["null\n", ""] {
            std::fs::write(default_auth_dir(ud, "id-2").join(OAUTH_ACCOUNT_FILE), body).unwrap();
            assert_eq!(s.oauth_account(), Ok(None), "{body:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn keychain_credentials_are_trimmed() {
        let fake = FakeSecurity::install();
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        make_stash(ud, "id-1", None, Some(b"{\"file\":\"ignored on macOS\"}"));
        fake.put(
            keychain::STASH_SERVICE,
            "id-1",
            b"  {\"claudeAiOauth\":{}}\n",
        );
        let s = Stash::open(ud, "id-1", None).unwrap();
        let c = s.credentials(HostOs::MacOs).unwrap().unwrap();
        assert_eq!(c.expose(), "{\"claudeAiOauth\":{}}");
        make_stash(ud, "id-2", None, None);
        let s = Stash::open(ud, "id-2", None).unwrap();
        assert!(s.credentials(HostOs::MacOs).unwrap().is_none());
    }

    #[test]
    fn credential_validity_follows_orca() {
        assert!(credentials_are_valid(
            r#"{"claudeAiOauth":{"accessToken":" t "}}"#
        ));
        for bad in [
            r#"{"claudeAiOauth":{"accessToken":"  "}}"#,
            r#"{"claudeAiOauth":{"accessToken":1}}"#,
            r#"{"claudeAiOauth":[]}"#,
            r#"{}"#,
            "not json",
        ] {
            assert!(!credentials_are_valid(bad), "{bad}");
        }
    }

    #[test]
    fn create_makes_orcas_layout_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let s = create(ud, "id-new").unwrap();
        let root = claude_accounts_root(ud);
        assert!(root.join("id-new").is_dir());
        assert_eq!(
            std::fs::read(s.auth_dir.join(MARKER_FILE)).unwrap(),
            b"id-new\n"
        );
        #[cfg(unix)]
        {
            use crate::orca::fsx::mode_of;
            assert_eq!(mode_of(&root), Some(0o700));
            assert_eq!(mode_of(&root.join("id-new")), Some(0o700));
            assert_eq!(mode_of(&s.auth_dir), Some(0o700));
            assert_eq!(mode_of(&s.auth_dir.join(MARKER_FILE)), Some(0o600));
        }
        assert_eq!(create(ud, "id-new").unwrap_err(), StashError::Exists);
        assert_eq!(create(ud, "../x").unwrap_err(), StashError::BadShape);
    }

    #[test]
    fn file_writes_are_exact_and_x4_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let s = create(ud, "id-1").unwrap();
        let creds = "{\"claudeAiOauth\":{\"accessToken\":\"t\"}}";
        s.write_credentials(ud, HostOs::Linux, creds).unwrap();
        assert_eq!(
            std::fs::read(s.auth_dir.join(CREDENTIALS_FILE)).unwrap(),
            creds.as_bytes(),
            "no newline, no re-serialization"
        );
        let oauth = serde_json::json!({"accountUuid": "u-1", "emailAddress": "alice@example.com"});
        s.write_oauth_account(ud, &oauth).unwrap();
        assert_eq!(
            std::fs::read_to_string(s.auth_dir.join(OAUTH_ACCOUNT_FILE)).unwrap(),
            "{\n  \"accountUuid\": \"u-1\",\n  \"emailAddress\": \"alice@example.com\"\n}\n"
        );
        s.write_oauth_account(ud, &Value::Null).unwrap();
        assert_eq!(
            std::fs::read_to_string(s.auth_dir.join(OAUTH_ACCOUNT_FILE)).unwrap(),
            "null\n"
        );
        #[cfg(unix)]
        assert_eq!(
            crate::orca::fsx::mode_of(&s.auth_dir.join(CREDENTIALS_FILE)),
            Some(0o600)
        );
        assert_eq!(
            s.credentials(HostOs::Linux).unwrap().unwrap().expose(),
            creds
        );
        std::fs::remove_file(s.auth_dir.join(CREDENTIALS_FILE)).unwrap();
        assert!(s.credentials(HostOs::Linux).unwrap().is_none());

        // A child that is a dir (or a symlink) is never replaced.
        std::fs::create_dir(s.auth_dir.join(CREDENTIALS_FILE)).unwrap();
        assert_eq!(
            s.write_credentials(ud, HostOs::Linux, creds).unwrap_err(),
            StashError::ChildNotOwned(CREDENTIALS_FILE)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_child_is_never_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let s = create(ud, "id-1").unwrap();
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, s.auth_dir.join(CREDENTIALS_FILE)).unwrap();
        assert!(s.write_credentials(ud, HostOs::Linux, "{}").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn writes_recheck_q2i_first() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let s = create(ud, "id-1").unwrap();
        std::fs::write(s.auth_dir.join(MARKER_FILE), b"someone-else\n").unwrap();
        assert_eq!(
            s.write_credentials(ud, HostOs::Linux, "{}").unwrap_err(),
            StashError::MarkerMismatch
        );
    }

    #[test]
    fn open_for_write_adopts_only_a_missing_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        make_stash(ud, "id-1", None, None);
        let auth = default_auth_dir(ud, "id-1");
        std::fs::remove_file(auth.join(MARKER_FILE)).unwrap();
        assert_eq!(
            Stash::open(ud, "id-1", None).unwrap_err(),
            StashError::MarkerMissing
        );
        Stash::open_for_write(ud, "id-1", None).unwrap();
        assert_eq!(std::fs::read(auth.join(MARKER_FILE)).unwrap(), b"id-1\n");
        std::fs::write(auth.join(MARKER_FILE), b"other\n").unwrap();
        assert_eq!(
            Stash::open_for_write(ud, "id-1", None).unwrap_err(),
            StashError::MarkerMismatch
        );
        assert_eq!(std::fs::read(auth.join(MARKER_FILE)).unwrap(), b"other\n");
    }

    #[test]
    fn open_for_read_reads_an_unmarked_stash_without_writing_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        make_stash(ud, "id-1", None, None);
        let auth = default_auth_dir(ud, "id-1");
        std::fs::remove_file(auth.join(MARKER_FILE)).unwrap();
        let s = Stash::open_for_read(ud, "id-1", None).unwrap();
        assert_eq!(s.id, "id-1");
        assert!(!auth.join(MARKER_FILE).exists());
        // A wrong marker is still a refusal, as in Orca ('wx' fails).
        std::fs::write(auth.join(MARKER_FILE), b"other\n").unwrap();
        assert_eq!(
            Stash::open_for_read(ud, "id-1", None).unwrap_err(),
            StashError::MarkerMismatch
        );
    }

    #[test]
    fn remove_deletes_the_account_dir_only() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let a = create(ud, "id-a").unwrap();
        create(ud, "id-b").unwrap();
        assert_eq!(a.remove(ud, HostOs::Linux).unwrap(), None);
        let root = claude_accounts_root(ud);
        assert!(!root.join("id-a").exists());
        assert!(root.join("id-b").join("auth").join(MARKER_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn keychain_stash_writes_and_removal() {
        let fake = FakeSecurity::install();
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        let s = create(ud, "id-k").unwrap();
        let creds = "{\"claudeAiOauth\":{\"accessToken\":\"t\"}}";
        s.write_credentials(ud, HostOs::MacOs, creds).unwrap();
        assert_eq!(
            fake.get(keychain::STASH_SERVICE, "id-k").as_deref(),
            Some(creds.as_bytes())
        );
        assert!(!s.auth_dir.join(CREDENTIALS_FILE).exists());
        assert_eq!(
            s.credentials(HostOs::MacOs).unwrap().unwrap().expose(),
            creds
        );
        fake.fail_add(keychain::STASH_SERVICE, true);
        assert!(matches!(
            s.write_credentials(ud, HostOs::MacOs, "{\"x\":1}"),
            Err(StashError::Keychain(_))
        ));
        fake.fail_add(keychain::STASH_SERVICE, false);
        assert_eq!(s.clone().remove(ud, HostOs::MacOs).unwrap(), None);
        assert!(fake.get(keychain::STASH_SERVICE, "id-k").is_none());
    }
}
