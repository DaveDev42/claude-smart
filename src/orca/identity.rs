//! Claude account identities, read-only, from the two places they live:
//!
//! - a csm profile: `<dir>/.claude.json` → `oauthAccount` (Claude Code's own
//!   record of who is logged into that config dir);
//! - an Orca account: `<userData>/claude-accounts/<uuid>/auth/
//!   oauth-account.json` (the same `oauthAccount` shape, written by Orca at
//!   add and reauth).
//!
//! Only the three identifying fields are kept (`accountUuid`,
//! `emailAddress`, `organizationUuid`); nothing secret is read. Both files are
//! capped and must be regular files ([`super::read_capped`]).

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// Cap on `.claude.json`. It also carries per-project history and grows to
/// several MiB on a long-lived profile; a file over the cap reads as an
/// error, which callers treat as "unbound", so the cap is generous (it only
/// guards against a runaway or hostile file, not normal growth).
const CLAUDE_JSON_CAP: u64 = 64 * 1024 * 1024;

/// Cap on `oauth-account.json` (a few hundred bytes in practice).
const OAUTH_ACCOUNT_CAP: u64 = 256 * 1024;

/// Who a config dir / an Orca account is logged in as.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Identity {
    #[serde(rename = "emailAddress")]
    pub email: Option<String>,
    #[serde(rename = "accountUuid")]
    pub account_uuid: Option<String>,
    #[serde(rename = "organizationUuid")]
    pub organization_uuid: Option<String>,
}

impl Identity {
    /// `true` when there is nothing to match on.
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.account_uuid.is_none()
    }

    /// The lowercased email, for matching.
    pub fn email_key(&self) -> Option<String> {
        self.email.as_deref().map(str::to_lowercase)
    }
}

fn field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse an `oauthAccount`-shaped object — or a document wrapping one under
/// `oauthAccount` (`.claude.json`). Pure. `None` when neither an account uuid
/// nor an email is present.
pub fn parse_identity(v: &Value) -> Option<Identity> {
    let obj = v.get("oauthAccount").unwrap_or(v);
    obj.as_object()?;
    let id = Identity {
        email: field(obj, "emailAddress"),
        account_uuid: field(obj, "accountUuid"),
        organization_uuid: field(obj, "organizationUuid"),
    };
    (!id.is_empty()).then_some(id)
}

fn read_identity_file(path: &Path, cap: u64) -> io::Result<Option<Identity>> {
    let Some(text) = super::read_capped(path, cap)? else {
        return Ok(None);
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON", path.display()),
        )
    })?;
    Ok(parse_identity(&v))
}

/// The identity logged into the profile dir `dir` (`<dir>/.claude.json` →
/// `oauthAccount`, located by [`profile_claude_json`]). `Ok(None)` when
/// absent or logged out.
pub fn read_profile_identity(dir: &Path) -> io::Result<Option<Identity>> {
    read_identity_file(&profile_claude_json(dir), CLAUDE_JSON_CAP)
}

/// Where the `.claude.json` for config dir `dir` lives. Mirrors Orca's (and
/// Claude Code's) `resolveConfigPath`: `<dir>/.claude.json` when it exists;
/// otherwise, for the DEFAULT dir `~/.claude` only, `~/.claude.json` — the
/// file an app started without `CLAUDE_CONFIG_DIR` actually reads. Any other
/// dir keeps `<dir>/.claude.json` (absent → no identity).
pub fn profile_claude_json(dir: &Path) -> PathBuf {
    let own = dir.join(".claude.json");
    if own.exists() {
        return own;
    }
    match crate::paths::home_dir() {
        Some(home) if is_default_config_dir(dir, &home) => home.join(".claude.json"),
        _ => own,
    }
}

/// Is `dir` the default config dir `<home>/.claude`? Pure.
fn is_default_config_dir(dir: &Path, home: &Path) -> bool {
    crate::cas::platform::dirs_equal(
        &dir.to_string_lossy(),
        &home.join(".claude").to_string_lossy(),
    )
}

/// The identity recorded in an Orca account's `auth` dir
/// (`oauth-account.json`). `Ok(None)` when absent.
pub fn read_account_identity(auth_dir: &Path) -> io::Result<Option<Identity>> {
    read_identity_file(&auth_dir.join("oauth-account.json"), OAUTH_ACCOUNT_CAP)
}

/// Is `id` safe to use as a path component? Orca's account ids are UUIDs;
/// anything else (separators, `..`) is refused before touching the FS.
pub fn is_safe_account_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `<userData>/claude-accounts/<id>/auth`, or `None` for an unsafe id.
pub fn account_auth_dir(user_data: &Path, account_id: &str) -> Option<PathBuf> {
    is_safe_account_id(account_id).then(|| {
        user_data
            .join("claude-accounts")
            .join(account_id)
            .join("auth")
    })
}

/// An Orca account's identity: `oauth-account.json` when readable, with any
/// missing email/org filled from the account record itself (so an account
/// still binds by email when its auth file is missing).
pub fn account_identity(user_data: &Path, account: &super::Account) -> Identity {
    let mut id = account_auth_dir(user_data, &account.id)
        .and_then(|d| read_account_identity(&d).ok().flatten())
        .unwrap_or_default();
    if id.email.is_none() && !account.email.is_empty() {
        id.email = Some(account.email.clone());
    }
    if id.organization_uuid.is_none() {
        id.organization_uuid = account.organization_uuid.clone();
    }
    id
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bare_and_wrapped_oauth_account() {
        let bare = json!({"accountUuid": "u-1", "emailAddress": "alice@example.com",
                          "organizationUuid": "org-1", "ccOnboardingFlags": {}});
        let want = Identity {
            email: Some("alice@example.com".into()),
            account_uuid: Some("u-1".into()),
            organization_uuid: Some("org-1".into()),
        };
        assert_eq!(parse_identity(&bare), Some(want.clone()));
        let wrapped = json!({"numStartups": 3, "oauthAccount": bare});
        assert_eq!(parse_identity(&wrapped), Some(want));
    }

    #[test]
    fn logged_out_profile_has_no_identity() {
        assert_eq!(parse_identity(&json!({"numStartups": 3})), None);
        assert_eq!(parse_identity(&json!({"oauthAccount": {}})), None);
        assert_eq!(parse_identity(&json!({"oauthAccount": null})), None);
        assert_eq!(parse_identity(&json!([1, 2])), None);
    }

    #[test]
    fn email_key_is_lowercased() {
        let id = Identity {
            email: Some("Alice@Example.COM".into()),
            ..Default::default()
        };
        assert_eq!(id.email_key().as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn reads_profile_identity_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_profile_identity(dir.path()).unwrap(), None);
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"alice@example.com"}}"#,
        )
        .unwrap();
        let id = read_profile_identity(dir.path()).unwrap().unwrap();
        assert_eq!(id.account_uuid.as_deref(), Some("u-1"));
        std::fs::write(dir.path().join(".claude.json"), "{not json").unwrap();
        assert!(read_profile_identity(dir.path()).is_err());
    }

    #[test]
    fn default_dir_falls_back_to_the_home_claude_json_like_orca() {
        let home = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let default_dir = home.path().join(".claude");
            std::fs::create_dir_all(&default_dir).unwrap();
            std::fs::write(
                home.path().join(".claude.json"),
                r#"{"oauthAccount":{"accountUuid":"u-home","emailAddress":"alice@example.com"}}"#,
            )
            .unwrap();
            // `~/.claude/.claude.json` absent → `~/.claude.json`.
            let id = read_profile_identity(&default_dir).unwrap().unwrap();
            assert_eq!(id.account_uuid.as_deref(), Some("u-home"));
            // Trailing separator still names the default dir.
            let slash = format!("{}/", default_dir.display());
            assert_eq!(
                profile_claude_json(Path::new(&slash)),
                home.path().join(".claude.json")
            );

            // `~/.claude/.claude.json` present → it wins.
            std::fs::write(
                default_dir.join(".claude.json"),
                r#"{"oauthAccount":{"accountUuid":"u-own"}}"#,
            )
            .unwrap();
            let id = read_profile_identity(&default_dir).unwrap().unwrap();
            assert_eq!(id.account_uuid.as_deref(), Some("u-own"));

            // A non-default dir never falls back to the home file.
            let other = home.path().join(".claude.work");
            std::fs::create_dir_all(&other).unwrap();
            assert_eq!(read_profile_identity(&other).unwrap(), None);
        });
    }

    #[test]
    fn unsafe_account_ids_never_reach_the_fs() {
        let ud = Path::new("/Users/example/orca");
        assert!(account_auth_dir(ud, "../../etc").is_none());
        assert!(account_auth_dir(ud, "a/b").is_none());
        assert!(account_auth_dir(ud, "").is_none());
        assert_eq!(
            account_auth_dir(ud, "0f8e-11aa"),
            Some(ud.join("claude-accounts").join("0f8e-11aa").join("auth"))
        );
    }

    #[test]
    fn account_identity_fills_from_the_record() {
        let ud = tempfile::tempdir().unwrap();
        let acct = crate::orca::Account {
            id: "acct-1".into(),
            email: "alice@example.com".into(),
            organization_uuid: Some("org-1".into()),
            organization_name: None,
            runtime: "host".into(),
        };
        let id = account_identity(ud.path(), &acct);
        assert_eq!(id.account_uuid, None);
        assert_eq!(id.email.as_deref(), Some("alice@example.com"));
        assert_eq!(id.organization_uuid.as_deref(), Some("org-1"));

        let auth = ud.path().join("claude-accounts/acct-1/auth");
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(
            auth.join("oauth-account.json"),
            r#"{"accountUuid":"u-1","emailAddress":"alice@example.com"}"#,
        )
        .unwrap();
        let id = account_identity(ud.path(), &acct);
        assert_eq!(id.account_uuid.as_deref(), Some("u-1"));
        assert_eq!(
            id.organization_uuid.as_deref(),
            Some("org-1"),
            "org filled in"
        );
    }
}
