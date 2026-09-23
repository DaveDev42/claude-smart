//! Offline reader for Orca's persisted state:
//! `<userData>/profiles/local-default/orca-data.json`.
//!
//! Read-only, 16 MiB cap, tolerant (every field optional; unknown shapes are
//! ignored). Used for display (`csm orca status` / `accounts` while Orca is
//! not running) and for a pending select's `expectedPriorActiveId` — never to
//! drive a launch. csm never writes this file.
//!
//! Fields read, all under `settings`: `claudeManagedAccounts` (the account
//! record shape plus `managedAuthPath`), `activeClaudeManagedAccountId`,
//! `activeClaudeManagedAccountIdsByRuntime.host`, `agentCmdOverrides.claude`,
//! `agentDefaultArgs.claude`.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Selection;

/// Cap on `orca-data.json`.
const DATA_FILE_CAP: u64 = 16 * 1024 * 1024;

/// What csm reads from `orca-data.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OfflineData {
    pub selection: Selection,
    /// `settings.agentCmdOverrides.claude` (what Orca types instead of
    /// `claude` in a pane), rendered as a display string.
    pub agent_cmd_override: Option<String>,
    /// `settings.agentDefaultArgs.claude`, rendered as a display string.
    pub agent_default_args: Option<String>,
}

/// The one data file Orca 1.4.209 writes.
pub fn data_file_path(user_data: &Path) -> PathBuf {
    user_data
        .join("profiles")
        .join("local-default")
        .join("orca-data.json")
}

fn display_value(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Array(a) if a.is_empty() => None,
        other => Some(other.to_string()),
    }
}

/// Parse `orca-data.json`'s text. Pure.
pub fn parse(text: &str) -> io::Result<OfflineData> {
    let v: Value = serde_json::from_str(text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "orca-data.json is not valid JSON",
        )
    })?;
    let Some(settings) = v.get("settings") else {
        return Ok(OfflineData::default());
    };
    let str_at = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Ok(OfflineData {
        selection: Selection {
            accounts: super::parse_accounts(settings.get("claudeManagedAccounts")),
            active_id: str_at(settings.get("activeClaudeManagedAccountId")),
            host_active_id: str_at(
                settings
                    .get("activeClaudeManagedAccountIdsByRuntime")
                    .and_then(|r| r.get("host")),
            ),
            rate_limits: None,
        },
        agent_cmd_override: display_value(
            settings
                .get("agentCmdOverrides")
                .and_then(|o| o.get("claude")),
        ),
        agent_default_args: display_value(
            settings
                .get("agentDefaultArgs")
                .and_then(|o| o.get("claude")),
        ),
    })
}

/// Read `orca-data.json` under `user_data`. `Ok(None)` when absent.
pub fn read_in(user_data: &Path) -> io::Result<Option<OfflineData>> {
    match super::read_capped(&data_file_path(user_data), DATA_FILE_CAP)? {
        Some(text) => parse(&text).map(Some),
        None => Ok(None),
    }
}

/// Every `profiles/*/orca-data.json` other than `local-default` (diagnostic
/// only: Orca 1.4.209 writes just the one, so another means a newer Orca).
pub fn other_data_files(user_data: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(user_data.join("profiles")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != "local-default")
        .map(|e| e.path().join("orca-data.json"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "settings": {
        "claudeManagedAccounts": [
          {"id": "acct-1", "email": "alice@example.com", "managedAuthRuntime": "host",
           "managedAuthPath": "/Users/example/orca/claude-accounts/acct-1/auth",
           "organizationUuid": "org-1"},
          {"id": "acct-2", "email": "bob@example.com"}
        ],
        "activeClaudeManagedAccountId": "acct-2",
        "activeClaudeManagedAccountIdsByRuntime": {"host": "acct-1", "wsl": {}},
        "agentCmdOverrides": {"claude": "csm"},
        "agentDefaultArgs": {"claude": ["--verbose"]},
        "somethingNew": 1
      },
      "repos": []
    }"#;

    #[test]
    fn parses_accounts_active_ids_and_agent_settings() {
        let d = parse(FIXTURE).unwrap();
        assert_eq!(d.selection.accounts.len(), 2);
        assert_eq!(d.selection.effective_active_id(), Some("acct-1"));
        assert_eq!(d.agent_cmd_override.as_deref(), Some("csm"));
        assert_eq!(d.agent_default_args.as_deref(), Some(r#"["--verbose"]"#));
    }

    #[test]
    fn tolerates_missing_settings_and_fields() {
        assert_eq!(parse("{}").unwrap(), OfflineData::default());
        let d = parse(r#"{"settings": {"claudeManagedAccounts": "not a list"}}"#).unwrap();
        assert!(d.selection.accounts.is_empty());
        assert_eq!(d.selection.effective_active_id(), None);
        assert!(parse("{").is_err());
    }

    #[test]
    fn reads_from_the_local_default_profile() {
        let ud = tempfile::tempdir().unwrap();
        assert!(read_in(ud.path()).unwrap().is_none());
        let p = data_file_path(ud.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, FIXTURE).unwrap();
        assert_eq!(
            read_in(ud.path())
                .unwrap()
                .unwrap()
                .selection
                .accounts
                .len(),
            2
        );
        assert!(other_data_files(ud.path()).is_empty());

        let other = ud.path().join("profiles/second");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("orca-data.json"), "{}").unwrap();
        assert_eq!(other_data_files(ud.path()).len(), 1);
    }
}
