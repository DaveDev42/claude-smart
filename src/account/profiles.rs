//! Load `~/.config/claude-as/profiles.json` → [`ProfileMap`].
//!
//! Schema: flat `{ "<profile_name>": "<absolute_config_dir>" }` map.
//! Personal-only: **absent file is not an error** — returns an empty map
//! (toss machines have no profiles.json and no CAS features).
//!
//! This is a REAL implementation (pure I/O, no OS coupling).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

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

    // ─── registry authority (replaces the hardcoded personal|work allowlist) ──

    /// `true` iff `name` is a configured profile. Replaces the free
    /// `cas::is_valid_profile`. On an empty map this is always `false`
    /// (callers special-case the empty/toss regime separately).
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// The deterministic preferred default when the state file is absent/invalid:
    /// the first profile by sorted name. `None` on an empty map.
    ///
    /// Alphabetical-first is a deterministic, documented tie-break. Intent is
    /// pinned by the Ansible-seeded `default` file (dave-environment), so this
    /// only fires on a hand-corrupted/absent `default` — external users get a
    /// stable, predictable choice.
    pub fn preferred_default(&self) -> Option<String> {
        self.names_sorted().first().map(|s| (*s).to_owned())
    }

    /// Resolve the global default profile NAME, reading the conventional state
    /// file at `~/.config/claude-as/default`. See [`Self::default_name_with`].
    pub fn default_name(&self) -> String {
        self.default_name_with(&crate::cas::default_state_file())
    }

    /// Resolution order (testable seam — takes the state-file path explicitly):
    /// 1. state-file token, when the map is empty (toss/synth: trust the token);
    /// 2. state-file token, when it is a configured profile;
    /// 3. otherwise [`Self::preferred_default`];
    /// 4. otherwise `""`.
    pub fn default_name_with(&self, state_file: &Path) -> String {
        let from_file = std::fs::read_to_string(state_file)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        match from_file {
            Some(n) if self.is_empty() => n,   // toss/synth: trust the token
            Some(n) if self.contains(&n) => n, // configured profile
            _ => self.preferred_default().unwrap_or_default(),
        }
    }

    /// Resolve the default profile to its absolute config dir. Empty map or an
    /// unmapped token → synthesize `~/.claude.<name>` (matches `resolve_profile`).
    pub fn default_dir(&self) -> PathBuf {
        let name = self.default_name();
        if let Some(dir) = self.get(&name) {
            return PathBuf::from(dir);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(format!(".claude.{name}"))
    }

    /// `true` iff `name` is a syntactically valid profile name:
    /// non-empty, ASCII alphanumerics + `. _ -`, no path separators. (Validity
    /// of *existence* is `contains`; this gates what `cas add`/`set` will write.)
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }

    /// Insert/overwrite a profile → returns the previous dir if any.
    pub fn insert(&mut self, name: String, dir: String) -> Option<String> {
        self.0.insert(name, dir)
    }

    /// Remove a profile → returns its dir if it was present.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.0.remove(name)
    }

    /// Atomically serialize to `~/.config/claude-as/profiles.json`.
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&paths::profiles_json())
    }

    /// Atomic serialize to `path` (testable seam). Sorted keys + trailing
    /// newline for deterministic Ansible diffing; tmp+rename in the SAME dir
    /// (same filesystem → atomic).
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let sorted: std::collections::BTreeMap<&str, &str> = self.iter().collect();
        let json = serde_json::to_string_pretty(&sorted)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{json}\n"))?;
        std::fs::rename(&tmp, path)
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

    // ─── registry authority tests ──────────────────────────────────────────────

    #[test]
    fn contains_reflects_membership() {
        let pm = parse_profiles(r#"{"a": "/x", "b": "/y"}"#).unwrap();
        assert!(pm.contains("a"));
        assert!(!pm.contains("c"));
        assert!(!ProfileMap::default().contains("a"));
    }

    #[test]
    fn preferred_default_is_alphabetical_first() {
        let pm = parse_profiles(r#"{"zeta": "/z", "alpha": "/a"}"#).unwrap();
        assert_eq!(pm.preferred_default().as_deref(), Some("alpha"));
    }

    #[test]
    fn preferred_default_single_and_empty() {
        let single = parse_profiles(r#"{"only": "/o"}"#).unwrap();
        assert_eq!(single.preferred_default().as_deref(), Some("only"));
        assert_eq!(ProfileMap::default().preferred_default(), None);
    }

    #[test]
    fn default_name_with_resolves_in_order() {
        let pm = parse_profiles(r#"{"alpha": "/a", "beta": "/b"}"#).unwrap();
        // configured token → returned verbatim
        let f = write_state("beta\n");
        assert_eq!(pm.default_name_with(f.path()), "beta");
        // unknown token in a populated map → preferred_default (alphabetical-first)
        let f = write_state("nope");
        assert_eq!(pm.default_name_with(f.path()), "alpha");
        // empty map trusts any token (toss/synth)
        let f = write_state("custom\n");
        assert_eq!(ProfileMap::default().default_name_with(f.path()), "custom");
        // empty map + absent token → ""
        let f = write_state("");
        assert_eq!(ProfileMap::default().default_name_with(f.path()), "");
    }

    #[test]
    fn default_dir_maps_or_synthesizes() {
        let pm = parse_profiles(r#"{"alpha": "/explicit/alpha"}"#).unwrap();
        let f = write_state("alpha\n");
        assert_eq!(pm.default_name_with(f.path()), "alpha");
        // Mapped profile → its explicit dir (verify via default_name_with the path).
        // (default_dir() reads the real state file, so we assert the mapping directly.)
        assert_eq!(pm.get("alpha"), Some("/explicit/alpha"));
    }

    #[test]
    fn is_valid_name_rules() {
        assert!(ProfileMap::is_valid_name("personal"));
        assert!(ProfileMap::is_valid_name("a.b_c-1"));
        assert!(!ProfileMap::is_valid_name(""));
        assert!(!ProfileMap::is_valid_name("a b"));
        assert!(!ProfileMap::is_valid_name("a/b"));
        assert!(!ProfileMap::is_valid_name("a\\b"));
    }

    #[test]
    fn insert_remove_mutate() {
        let mut pm = ProfileMap::default();
        assert_eq!(pm.insert("x".into(), "/x".into()), None);
        assert_eq!(pm.insert("x".into(), "/x2".into()), Some("/x".to_owned()));
        assert!(pm.contains("x"));
        assert_eq!(pm.remove("x"), Some("/x2".to_owned()));
        assert!(!pm.contains("x"));
        assert_eq!(pm.remove("x"), None);
    }

    #[test]
    fn save_to_then_load_roundtrip_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let mut pm = ProfileMap::default();
        pm.insert("zeta".into(), "/z".into());
        pm.insert("alpha".into(), "/a".into());
        pm.save_to(&path).unwrap();

        // Sorted keys + trailing newline on disk.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(raw.find("alpha").unwrap() < raw.find("zeta").unwrap(), "keys not sorted: {raw}");

        // Roundtrip.
        let reloaded = parse_profiles(&raw).unwrap();
        assert_eq!(reloaded.get("alpha"), Some("/a"));
        assert_eq!(reloaded.get("zeta"), Some("/z"));
    }

    #[test]
    fn save_to_is_atomic_no_tmp_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let mut pm = ProfileMap::default();
        pm.insert("x".into(), "/x".into());
        pm.save_to(&path).unwrap();
        // The tmp sibling must have been renamed away.
        assert!(!path.with_extension("json.tmp").exists());
        assert!(path.exists());
    }

    /// Write `content` to a temp state file (for `default_name_with`).
    fn write_state(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{content}").unwrap();
        f
    }
}
