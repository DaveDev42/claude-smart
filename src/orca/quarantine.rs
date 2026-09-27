//! The quarantine: where csm files a credential it cannot attribute, so it
//! never discards one (design section 3 step 3, B1).
//!
//! An entry is keyed by a fingerprint of the grant: the first 16 hex chars
//! of sha256 of the trimmed refresh token, or, for a grant without one,
//! `raw-` plus the first 16 hex chars of sha256 of the whole credential
//! text. The secret lives:
//! - on macOS in the Keychain, service `csm quarantined Claude credentials`,
//!   account = the fingerprint (written through `/usr/bin/security` like
//!   every other item csm writes);
//! - elsewhere in `<state>/quarantine/<fp>.json`, mode 0600.
//!
//! Beside it, on every OS, `<state>/quarantine/<fp>.meta.json` holds what
//! `accounts doctor` lists: fingerprint, `expiresAt`, the reason, where it
//! came from, the profile answer if any, when. It never holds a token.
//!
//! Filing a grant whose fingerprint is already quarantined keeps the
//! fresher of the two (by `expiresAt`, Orca's readFreshness); an entry is
//! never replaced by an older or equally fresh one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::fsx::{self, WriteOpts};
use super::keychain::{self, hex_lower};
use super::readback::{read_freshness, refresh_token};
use super::userdata::HostOs;
use super::{OrcaError, SecretString};

/// The Keychain service of quarantined grants (macOS).
pub const SERVICE: &str = "csm quarantined Claude credentials";

/// Cap on one quarantine file.
const ENTRY_CAP: u64 = 1024 * 1024;

/// The fingerprint of a credential JSON. Pure.
pub fn fingerprint(creds: &str) -> String {
    match refresh_token(creds) {
        Some(rt) => hex_lower(&Sha256::digest(rt.as_bytes()))[..16].to_owned(),
        None => format!(
            "raw-{}",
            &hex_lower(&Sha256::digest(creds.as_bytes()))[..16]
        ),
    }
}

/// Why a grant was quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// The matcher found no account.
    NoMatch,
    /// The matcher found more than one, or unverifiable ones.
    Ambiguous,
    /// The profile endpoint named another account.
    ProfileMismatch,
    /// The profile answer carried no account uuid, or the matched stash has
    /// none to compare.
    ProfileUnverifiable,
    /// The access token was rejected (401); filed before any refresh.
    Unauthorized,
    /// The grant a refresh of a quarantined grant returned.
    Rotated,
    /// Accepted by the matcher but a fresher candidate won.
    Superseded,
    /// The stash changed between the match and the write.
    StashChanged,
    /// A refreshed grant csm could not persist to its stash.
    PersistFailed,
    /// Moved out of a retired config dir.
    Retired,
    /// Moved out of a stash no Orca record names (`accounts doctor --fix`).
    Orphaned,
    /// The unscoped runtime item's pre-login grant, kept because something
    /// other than the login changed the item while `accounts add` ran.
    ChangedDuringLogin,
    /// A runtime grant the recovery of a crashed switch from no account
    /// displaced while putting the system default back.
    CrashRecovery,
}

/// The secret-free index entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub fingerprint: String,
    pub expires_at: Option<f64>,
    pub reason: Reason,
    /// `scoped-keychain`, `legacy-keychain`, `file`, `refresh`, a dir...
    pub source: String,
    /// The account the matcher named, when it named one.
    pub matched_account: Option<String>,
    /// The profile endpoint's account uuid, when it answered.
    pub profile_account_uuid: Option<String>,
    /// The profile endpoint's status, when it answered.
    pub profile_status: Option<u16>,
    pub captured_at: i64,
}

/// What filing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filed {
    /// A new entry.
    New(String),
    /// The entry existed and this grant was fresher: replaced.
    Replaced(String),
    /// The entry existed and was at least as fresh: kept.
    Kept(String),
}

impl Filed {
    pub fn fingerprint(&self) -> &str {
        match self {
            Filed::New(f) | Filed::Replaced(f) | Filed::Kept(f) => f,
        }
    }
}

/// The quarantine of one machine.
#[derive(Debug, Clone)]
pub struct Quarantine {
    pub os: HostOs,
    /// `<state>/quarantine`.
    pub dir: PathBuf,
}

impl Quarantine {
    pub fn new(os: HostOs, state: &Path) -> Quarantine {
        Quarantine {
            os,
            dir: state.join("quarantine"),
        }
    }

    fn secret_path(&self, fp: &str) -> PathBuf {
        self.dir.join(format!("{fp}.json"))
    }

    fn meta_path(&self, fp: &str) -> PathBuf {
        self.dir.join(format!("{fp}.meta.json"))
    }

    /// The quarantined grant `fp`, when present.
    pub fn get(&self, fp: &str) -> Result<Option<SecretString>, OrcaError> {
        match self.os {
            HostOs::MacOs => Ok(keychain::find_password(SERVICE, fp)?),
            HostOs::Linux | HostOs::Windows => {
                let p = self.secret_path(fp);
                let bytes = super::read_capped_bytes(&p, ENTRY_CAP)
                    .map_err(|e| OrcaError::io("cannot read", &p, e))?;
                Ok(bytes.map(|b| SecretString::new(String::from_utf8_lossy(&b).into_owned())))
            }
        }
    }

    fn put_secret(&self, fp: &str, creds: &str) -> Result<(), OrcaError> {
        match self.os {
            HostOs::MacOs => Ok(keychain::add_password(SERVICE, fp, creds)?),
            HostOs::Linux | HostOs::Windows => {
                let p = self.secret_path(fp);
                fsx::write_atomic(&p, creds.as_bytes(), WriteOpts::PRIVATE)
                    .map_err(|e| OrcaError::io("cannot write", &p, e))
            }
        }
    }

    /// File `creds`. The secret is written (and, on macOS, read back) before
    /// the index entry, so a crash leaves at worst a secret without an index
    /// line, never an index line without its secret.
    pub fn file(
        &self,
        creds: &str,
        reason: Reason,
        source: &str,
        matched_account: Option<&str>,
        profile: Option<(u16, Option<&str>)>,
        now_ms: i64,
    ) -> Result<Filed, OrcaError> {
        fsx::create_dir_all(&self.dir, 0o700)
            .map_err(|e| OrcaError::io("cannot create", &self.dir, e))?;
        let fp = fingerprint(creds);
        let existing = self.get(&fp)?;
        let outcome = match &existing {
            Some(old) if old.expose() == creds => return Ok(Filed::Kept(fp)),
            Some(old) => {
                let fresher = match (read_freshness(creds), read_freshness(old.expose())) {
                    (Some(n), Some(o)) => n > o,
                    (Some(_), None) => true,
                    _ => false,
                };
                if !fresher {
                    return Ok(Filed::Kept(fp));
                }
                Filed::Replaced(fp.clone())
            }
            None => Filed::New(fp.clone()),
        };
        self.put_secret(&fp, creds)?;
        let meta = Meta {
            fingerprint: fp.clone(),
            expires_at: read_freshness(creds),
            reason,
            source: source.to_owned(),
            matched_account: matched_account.map(str::to_owned),
            profile_account_uuid: profile.and_then(|(_, u)| u.map(str::to_owned)),
            profile_status: profile.map(|(s, _)| s),
            captured_at: now_ms,
        };
        let text = serde_json::to_vec_pretty(&meta)
            .map_err(|e| OrcaError::Invalid(format!("quarantine index: {e}")))?;
        let mp = self.meta_path(&fp);
        fsx::write_atomic(&mp, &text, WriteOpts::PRIVATE)
            .map_err(|e| OrcaError::io("cannot write", &mp, e))?;
        Ok(outcome)
    }

    /// Every index entry, sorted by fingerprint. Unreadable entries are
    /// skipped.
    pub fn list(&self) -> Vec<Meta> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Meta> = rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".meta.json"))
            })
            .filter_map(|e| {
                let b = super::read_capped_bytes(&e.path(), ENTRY_CAP).ok()??;
                serde_json::from_slice(&b).ok()
            })
            .collect();
        out.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        out
    }

    /// Delete entry `fp` (after `accounts doctor` attributed it).
    pub fn remove(&self, fp: &str) -> Result<(), OrcaError> {
        match self.os {
            HostOs::MacOs => {
                keychain::delete_password(SERVICE, fp)?;
            }
            HostOs::Linux | HostOs::Windows => {
                let p = self.secret_path(fp);
                fsx::remove_file(&p).map_err(|e| OrcaError::io("cannot remove", &p, e))?;
            }
        }
        let mp = self.meta_path(fp);
        fsx::remove_file(&mp).map_err(|e| OrcaError::io("cannot remove", &mp, e))?;
        Ok(())
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::creds_json;

    #[test]
    fn fingerprints_follow_the_refresh_token() {
        let a = creds_json("at-1", "rt-1", 1);
        let b = creds_json("at-2", " rt-1 ", 2);
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a).len(), 16);
        assert_ne!(fingerprint(&a), fingerprint(&creds_json("at-1", "rt-2", 1)));
        let no_rt = r#"{"claudeAiOauth":{"accessToken":"at"}}"#;
        assert!(fingerprint(no_rt).starts_with("raw-"));
        assert!(!fingerprint(&a).contains("rt-1"));
    }

    #[test]
    fn file_keeps_the_fresher_grant_and_a_secret_free_index() {
        let tmp = tempfile::tempdir().unwrap();
        let q = Quarantine::new(HostOs::Linux, tmp.path());
        let old = creds_json("at-old", "rt-1", 1000);
        let new = creds_json("at-new", "rt-1", 2000);
        let f = q
            .file(&new, Reason::NoMatch, "file", None, None, 5)
            .unwrap();
        assert!(matches!(f, Filed::New(_)));
        assert_eq!(
            q.file(&old, Reason::NoMatch, "file", None, None, 6)
                .unwrap(),
            Filed::Kept(f.fingerprint().to_owned())
        );
        assert_eq!(q.get(f.fingerprint()).unwrap().unwrap().expose(), new);
        let newer = creds_json("at-newer", "rt-1", 3000);
        assert!(matches!(
            q.file(
                &newer,
                Reason::Rotated,
                "refresh",
                Some("id-a"),
                Some((200, Some("u-1"))),
                7
            )
            .unwrap(),
            Filed::Replaced(_)
        ));
        assert_eq!(q.get(f.fingerprint()).unwrap().unwrap().expose(), newer);
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].expires_at, Some(3000.0));
        assert_eq!(list[0].reason, Reason::Rotated);
        assert_eq!(list[0].profile_account_uuid.as_deref(), Some("u-1"));
        let meta_text = std::fs::read_to_string(q.meta_path(f.fingerprint())).unwrap();
        assert!(!meta_text.contains("at-") && !meta_text.contains("rt-1"));
        #[cfg(unix)]
        {
            assert_eq!(
                crate::orca::fsx::mode_of(&q.secret_path(f.fingerprint())),
                Some(0o600)
            );
            assert_eq!(crate::orca::fsx::mode_of(&q.dir), Some(0o700));
        }
        q.remove(f.fingerprint()).unwrap();
        assert!(q.list().is_empty() && q.get(f.fingerprint()).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn macos_entries_live_in_the_keychain() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let tmp = tempfile::tempdir().unwrap();
        let q = Quarantine::new(HostOs::MacOs, tmp.path());
        let c = creds_json("at", "rt", 1);
        let f = q
            .file(&c, Reason::Ambiguous, "scoped-keychain", None, None, 1)
            .unwrap();
        assert_eq!(
            fake.get(SERVICE, f.fingerprint()).as_deref(),
            Some(c.as_bytes())
        );
        assert!(!q.secret_path(f.fingerprint()).exists());
        assert_eq!(q.list().len(), 1);
        // A failing Keychain write files nothing (no index without secret).
        fake.fail_add(SERVICE, true);
        let other = creds_json("at2", "rt2", 1);
        assert!(
            q.file(&other, Reason::NoMatch, "file", None, None, 2)
                .is_err()
        );
        assert_eq!(q.list().len(), 1);
    }
}
