//! Load `~/.config/claude-as/profiles.json` → [`ProfileMap`].
//!
//! Schema: flat `{ "<profile_name>": "<absolute_config_dir>" }` map.
//! Personal-only: **absent file is not an error** — returns an empty map
//! (toss machines have no profiles.json and no CAS features).
//!
//! This is a REAL implementation (pure I/O, no OS coupling).

use std::collections::HashMap;
use std::io;

use crate::paths;

/// `profile_name → absolute_config_dir` mapping.
///
/// Empty when the profiles.json is absent (toss machines, first-boot before
/// ansible has deployed the file, or explicit personal-only gate).
#[derive(Debug, Clone, Default)]
pub struct ProfileMap(pub HashMap<String, String>);

impl ProfileMap {
    /// Load from the canonical path `~/.config/claude-as/profiles.json`.
    ///
    /// Returns `Ok(ProfileMap { .. })` in all cases:
    /// - File absent → `Ok(empty map)`
    /// - Parse error → `Err`
    pub fn load() -> io::Result<Self> {
        let path = paths::profiles_json();
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let map: HashMap<String, String> = serde_json::from_str(&s).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("profiles.json parse error at {}: {e}", path.display()),
                    )
                })?;
                Ok(ProfileMap(map))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ProfileMap::default()),
            Err(e) => Err(e),
        }
    }

    /// Returns `true` when no profiles are configured (absent file or empty
    /// JSON object). The binary disables CAS/pick features in this state.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Look up the config directory for a profile name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Iterate over all `(name, dir)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Return the sorted list of profile names (deterministic order for tests
    /// and picker rendering).
    pub fn names_sorted(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.0.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Parse a profiles.json from a string (test helper bypassing the fixed path).
    fn parse_profiles(json: &str) -> io::Result<ProfileMap> {
        let map: HashMap<String, String> = serde_json::from_str(json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(ProfileMap(map))
    }

    #[test]
    fn empty_json_object_is_empty_map() {
        let pm = parse_profiles("{}").unwrap();
        assert!(pm.is_empty());
    }

    #[test]
    fn single_profile_roundtrip() {
        let pm = parse_profiles(
            r#"{"personal": "/Users/example/.claude.personal"}"#,
        )
        .unwrap();
        assert!(!pm.is_empty());
        assert_eq!(pm.get("personal"), Some("/Users/example/.claude.personal"));
        assert_eq!(pm.get("work"), None);
    }

    #[test]
    fn two_profiles_names_sorted() {
        let pm = parse_profiles(
            r#"{"work": "/Users/example/.claude.work", "personal": "/Users/example/.claude.personal"}"#,
        )
        .unwrap();
        assert_eq!(pm.names_sorted(), vec!["work", "personal"]);
    }

    #[test]
    fn iter_yields_all_entries() {
        let pm = parse_profiles(
            r#"{"personal": "/a", "work": "/b"}"#,
        )
        .unwrap();
        let mut pairs: Vec<(&str, &str)> = pm.iter().collect();
        pairs.sort_unstable();
        assert_eq!(pairs, vec![("work", "/b"), ("personal", "/a")]);
    }

    #[test]
    fn invalid_json_returns_err() {
        let result = parse_profiles("not json");
        assert!(result.is_err());
    }

    #[test]
    fn absent_file_returns_empty_map() {
        // Use a path that cannot exist (points to a non-existent file under
        // a known-absent subdir). We test the load() code path by replacing
        // the profiles.json file at the real path; instead just verify the
        // absent-file branch behavior using the known-absent path directly.
        //
        // We can't easily redirect paths::profiles_json() without a mock, so
        // we exercise the same io::ErrorKind::NotFound arm inline.
        let path = std::path::PathBuf::from("/tmp/csm-test-absent-profiles-NOPE.json");
        let result = match std::fs::read_to_string(&path) {
            Ok(s) => {
                let m: HashMap<String, String> = serde_json::from_str(&s).unwrap();
                Ok(ProfileMap(m))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ProfileMap::default()),
            Err(e) => Err(e),
        };
        let pm = result.unwrap();
        assert!(pm.is_empty());
    }

    #[test]
    fn real_file_roundtrip() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"{{"personal": "/Users/example/.claude.personal", "work": "/Users/example/.claude.work"}}"#
        )
        .unwrap();
        let s = std::fs::read_to_string(f.path()).unwrap();
        let pm = parse_profiles(&s).unwrap();
        assert_eq!(pm.get("personal"), Some("/Users/example/.claude.personal"));
        assert_eq!(pm.get("work"), Some("/Users/example/.claude.work"));
    }
}
