//! Per-profile local usage record — `<smart_dir>/usage/<profile>.json`
//! ([`paths::usage_store`]).
//!
//! One file per profile (not a single combined file) so the statusline
//! recorder's frequent writes for the *active* profile never race the
//! fetch-all writer's writes for every OTHER profile — only same-profile
//! writers can collide, and that collision is an accepted last-writer-wins
//! (design spec, "스토어 레코드").

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::usage::model::ProfileUsage;

/// One profile's local usage-collection record.
///
/// `captured_at`/`source` mirror the wrapped `usage.captured_at`/`usage.source`
/// at write time — kept at the top level too so a caller checking freshness
/// or provenance doesn't have to reach into `usage` (which is itself absent
/// when the profile has never been successfully probed but already carries a
/// `cooldown_until` from a rate-limited attempt — see [`set_cooldown`]).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoreRecord {
    pub profile: String,
    /// When this record was last written, by ANY writer — a live api probe,
    /// a statusline capture, or a rate-limit-cooldown stamp that preserved
    /// prior `usage`. Diagnostic/display only; `collect()`'s freshness gate
    /// reads [`Self::api_captured_at`] instead — see its doc.
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// When this profile last actually reached a **live api probe**
    /// (`local::api::fetch_usage` succeeding), distinct from
    /// [`Self::captured_at`] — which a statusline capture also bumps, ~every
    /// 10s, for the active profile. `collect()`'s freshness gate (`Fresh` vs
    /// `NeedsProbe`) is keyed on THIS field precisely so a long-running
    /// statusline-fed session cannot perpetually suppress the periodic api
    /// probe that is the only path able to refresh
    /// `week_fable`/`week_model_label` (statusLine stdin never carries
    /// per-model-tier data) or decay a section whose `resets_at` has passed.
    /// `record_statusline_payload` never writes this field itself — it
    /// carries the prior value forward unchanged.
    #[serde(default)]
    pub api_captured_at: Option<String>,
    /// Unix epoch after which a rate-limited profile may be probed again.
    /// `None` = no active cooldown.
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub usage: Option<ProfileUsage>,
}

/// Load `profile`'s record, or `None` when absent/unreadable/corrupt — every
/// failure mode collapses to "no stale data available", which is exactly how
/// `local::collect` already needs to treat a missing record.
pub fn load(profile: &str) -> Option<StoreRecord> {
    load_from(&paths::usage_store(profile))
}

/// Atomically write `record` to `<smart_dir>/usage/<profile>.json`
/// (tmp + rename — the same pattern as `transport.rs::write_positive_cache`).
pub fn save(profile: &str, record: &StoreRecord) -> std::io::Result<()> {
    save_to(&paths::usage_store(profile), record)
}

/// Stamp `profile`'s record with a rate-limit cooldown until `until_epoch`,
/// preserving any existing `usage` so `collect()`'s stale-serving path still
/// has something to fall back to. Creates a bare record (no `usage`) if none
/// existed yet — a profile that has NEVER succeeded but got rate-limited on
/// its very first probe still needs a cooldown recorded somewhere.
pub fn set_cooldown(profile: &str, until_epoch: i64) -> std::io::Result<()> {
    set_cooldown_at(&paths::usage_store(profile), profile, until_epoch)
}

// ─── path-injected cores (the testable seam — see `cas::edit`'s pattern) ───

fn load_from(path: &Path) -> Option<StoreRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_to(path: &Path, record: &StoreRecord) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("usage-record"),
        std::process::id()
    );
    let tmp = parent.join(tmp_name);

    let bytes = serde_json::to_vec(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &bytes)?;

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn set_cooldown_at(path: &Path, profile: &str, until_epoch: i64) -> std::io::Result<()> {
    let mut rec = load_from(path).unwrap_or_else(|| StoreRecord {
        profile: profile.to_string(),
        ..Default::default()
    });
    rec.cooldown_until = Some(until_epoch);
    save_to(path, &rec)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::model::UsageSection;

    fn sample_usage() -> ProfileUsage {
        ProfileUsage {
            captured_at: Some("2026-09-02T06:10:00Z".to_string()),
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: Some(1_788_339_599),
            }),
            week_all: None,
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: Some("api".to_string()),
            attention: None,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.json");
        let rec = StoreRecord {
            profile: "home".to_string(),
            captured_at: Some("2026-09-02T06:10:00Z".to_string()),
            source: Some("api".to_string()),
            cooldown_until: None,
            usage: Some(sample_usage()),
            ..Default::default()
        };
        save_to(&path, &rec).unwrap();

        let loaded = load_from(&path).expect("must load what was just saved");
        assert_eq!(loaded.profile, "home");
        assert_eq!(loaded.source.as_deref(), Some("api"));
        assert_eq!(loaded.usage.unwrap().session.unwrap().pct, 42);
    }

    #[test]
    fn save_to_is_atomic_no_tmp_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.json");
        let rec = StoreRecord {
            profile: "work".to_string(),
            ..Default::default()
        };
        save_to(&path, &rec).unwrap();
        assert!(path.exists());

        // No stray tmp file with our naming scheme should remain.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp file left behind: {leftovers:?}");
    }

    #[test]
    fn load_from_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load_from(&path).is_none());
    }

    #[test]
    fn load_from_corrupt_json_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_from(&path).is_none());
    }

    #[test]
    fn set_cooldown_creates_bare_record_when_none_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.json");
        set_cooldown_at(&path, "fresh", 1_800_000_000).unwrap();

        let loaded = load_from(&path).expect("record must now exist");
        assert_eq!(loaded.profile, "fresh");
        assert_eq!(loaded.cooldown_until, Some(1_800_000_000));
        assert!(loaded.usage.is_none());
    }

    #[test]
    fn set_cooldown_preserves_existing_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.json");
        let rec = StoreRecord {
            profile: "home".to_string(),
            captured_at: Some("2026-09-02T06:10:00Z".to_string()),
            source: Some("api".to_string()),
            cooldown_until: None,
            usage: Some(sample_usage()),
            ..Default::default()
        };
        save_to(&path, &rec).unwrap();

        set_cooldown_at(&path, "home", 1_800_000_000).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.cooldown_until, Some(1_800_000_000));
        assert_eq!(
            loaded
                .usage
                .expect("usage must be preserved")
                .session
                .unwrap()
                .pct,
            42
        );
    }

    #[test]
    fn set_cooldown_preserves_api_captured_at() {
        // A cooldown stamp round-trips the WHOLE record through
        // load_from/save_to — this pins that `api_captured_at` survives that
        // round-trip like every other field, so a rate-limited profile with
        // a stale `usage` doesn't lose its last-live-probe marker.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.json");
        let rec = StoreRecord {
            profile: "home".to_string(),
            captured_at: Some("2026-09-02T06:10:00Z".to_string()),
            source: Some("api".to_string()),
            api_captured_at: Some("2026-09-02T06:10:00Z".to_string()),
            cooldown_until: None,
            usage: Some(sample_usage()),
        };
        save_to(&path, &rec).unwrap();

        set_cooldown_at(&path, "home", 1_800_000_000).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(
            loaded.api_captured_at.as_deref(),
            Some("2026-09-02T06:10:00Z")
        );
    }

    #[test]
    fn set_cooldown_overwrites_prior_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.json");
        set_cooldown_at(&path, "home", 100).unwrap();
        set_cooldown_at(&path, "home", 200).unwrap();
        assert_eq!(load_from(&path).unwrap().cooldown_until, Some(200));
    }
}
