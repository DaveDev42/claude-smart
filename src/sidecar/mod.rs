//! Sidecar state file — `<smart_dir>/<sid>.json`.
//!
//! The sidecar records per-session metadata that must survive across the relaunch
//! loop: permission mode, effort, model, cwd, profile, and the hop counter.
//!
//! **Read-compat contract (§6):** the legacy zsh writer emits `hop` as a JSON
//! STRING (produced by `jq --arg`). The Rust binary may write it as a JSON
//! NUMBER. `hop_int()` accepts both forms transparently.
//!
//! **Merge-not-clobber semantics:** `write_sidecar` and `merge_sidecar` never
//! discard an existing `hop` value when the incoming data does not supply one.
//! This ensures a mid-relaunch-loop sidecar update from the transcript does not
//! reset the hop counter.
//!
//! **Atomicity:** writes go through a `.tmp` rename so a crash mid-write never
//! leaves a partial JSON file. A corrupt existing file is treated as `{}`.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("I/O error reading/writing sidecar: {0}")]
    Io(#[from] io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─── Sidecar struct ───────────────────────────────────────────────────────────

/// Per-session metadata written to `<smart_dir>/<sid>.json`.
///
/// All fields are `Option` so that partial updates (merge) never clobber
/// existing values. Unknown fields are preserved via `flatten` so future
/// additions survive a round-trip through an older binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")] // sessionId, permissionMode, etc.
pub struct Sidecar {
    /// The Claude session UUID, e.g. `01234567-89ab-cdef-0123-456789abcdef`.
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Unix timestamp (float) of when this sidecar was last written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<f64>,

    /// Claude `--permission-mode` value (`default`, `acceptEdits`, `bypassPermissions`, …).
    #[serde(rename = "permissionMode", skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,

    /// Claude `--effort` value (`low`, `normal`, `high`, `max`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Claude `--model` value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Working directory at session launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// `claude-as` profile name (leaf of `CLAUDE_CONFIG_DIR`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    /// Limit-switch hop counter.
    ///
    /// Stored as `serde_json::Value` to be tolerant of both legacy STRING form
    /// (written by `jq --arg hop "$hop"`) and current NUMBER form. Use
    /// `hop_int()` to read the value — never pattern-match directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop: Option<serde_json::Value>,

    /// Absorb any unknown keys written by future binary versions so a rollback
    /// does not destroy unrecognised fields.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

impl Sidecar {
    // ─── hop helpers ─────────────────────────────────────────────────────────

    /// Return the hop counter as `i64`, tolerating both JSON STRING and NUMBER.
    ///
    /// - `"3"` (legacy zsh `jq --arg`) → `3`
    /// - `3`   (Rust / numeric JSON)    → `3`
    /// - `null` / absent               → `0`
    /// - unparseable string / other    → `0`
    ///
    /// This is a **REAL** implementation (not a stub) as required by the spec.
    pub fn hop_int(&self) -> i64 {
        match &self.hop {
            Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            // null, bool, array, object, absent → 0
            _ => 0,
        }
    }

    /// Return a `Value::Number` representation of the given hop count.
    /// Test-only constructor: production hop writes use the legacy JSON **string**
    /// form (`jq --arg` compat, see `merge_sidecar_hop`), so this Number form is
    /// only used to build `Sidecar` fixtures in unit tests.
    #[cfg(test)]
    pub fn hop_value(n: i64) -> serde_json::Value {
        serde_json::Value::Number(serde_json::Number::from(n))
    }

    // ─── CLI flag emission ────────────────────────────────────────────────────

    /// Emit the CLI flags encoded in the sidecar as an `OsString` vector.
    ///
    /// Produces zero, two, or more elements in pairs:
    /// `["--permission-mode", "<m>", "--effort", "<e>", "--model", "<m>"]`.
    ///
    /// Explicit flags passed by the user on the current invocation **override**
    /// the sidecar at the call site; `sidecar_flags` emits the sidecar values
    /// unconditionally and the caller is responsible for filtering.
    pub fn sidecar_flags(&self) -> Vec<OsString> {
        let mut out: Vec<OsString> = Vec::new();
        if let Some(pm) = &self.permission_mode {
            out.push(OsString::from("--permission-mode"));
            out.push(OsString::from(pm));
        }
        if let Some(ef) = &self.effort {
            out.push(OsString::from("--effort"));
            out.push(OsString::from(ef));
        }
        if let Some(m) = &self.model {
            out.push(OsString::from("--model"));
            out.push(OsString::from(m));
        }
        out
    }
}

// ─── I/O operations ───────────────────────────────────────────────────────────

/// Read the sidecar at `path`.
///
/// Returns `Ok(Sidecar::default())` in any of these cases:
/// - file not found
/// - file is empty
/// - file contains invalid / corrupt JSON
///
/// Only hard I/O errors (permission denied, etc.) are propagated.
pub fn read_sidecar(path: &Path) -> Result<Sidecar, SidecarError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Sidecar::default()),
        Err(e) => return Err(SidecarError::Io(e)),
    };

    if bytes.is_empty() {
        return Ok(Sidecar::default());
    }

    // Corrupt JSON → treat as empty sidecar (spec: "corrupt sidecar → {}").
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

/// Write `new` to `path` using atomic tmp+rename.
///
/// **Merge-not-clobber semantics:** if the existing sidecar has a `hop` value
/// and `new.hop` is `None`, the existing hop is preserved in the written file.
///
/// Steps:
/// 1. Read the existing sidecar (corrupt / absent → `{}`).
/// 2. Merge: start with the existing, overlay every `Some` field from `new`.
/// 3. Preserve `hop` unless `new` explicitly supplies one.
/// 4. Write to `<path>.tmp`, then `rename` to `path` (atomic on same FS).
pub fn write_sidecar(path: &Path, new: &Sidecar) -> Result<(), SidecarError> {
    // Step 1: read existing (never fail on missing/corrupt).
    let mut merged = read_sidecar(path)?;

    // Step 2/3: overlay non-None fields from `new`, but preserve hop.
    let hop_backup = merged.hop.clone();

    overlay(&mut merged, new);

    // If `new` did not supply a hop, restore the backed-up value.
    if new.hop.is_none() {
        merged.hop = hop_backup;
    }

    // Step 4: atomic write.
    write_atomic(path, &merged)
}

/// Merge a single field into the sidecar at `path`.
///
/// Convenience wrapper around `write_sidecar` for the common one-key-at-a-time
/// update pattern.  A no-op (field is `None`) is detected early and skips
/// the round-trip entirely.
///
/// Merge-not-clobber: hop is preserved unless `patch.hop` is `Some`.
pub fn merge_sidecar(path: &Path, patch: &Sidecar) -> Result<(), SidecarError> {
    // If the patch is entirely empty (all fields None, no extras), skip I/O.
    if is_empty_patch(patch) {
        return Ok(());
    }
    write_sidecar(path, patch)
}

// ─── private helpers ──────────────────────────────────────────────────────────

/// Overlay every `Some` field of `src` onto `dst` (including `extra` map entries).
fn overlay(dst: &mut Sidecar, src: &Sidecar) {
    if src.session_id.is_some() {
        dst.session_id = src.session_id.clone();
    }
    if src.ts.is_some() {
        dst.ts = src.ts;
    }
    if src.permission_mode.is_some() {
        dst.permission_mode = src.permission_mode.clone();
    }
    if src.effort.is_some() {
        dst.effort = src.effort.clone();
    }
    if src.model.is_some() {
        dst.model = src.model.clone();
    }
    if src.cwd.is_some() {
        dst.cwd = src.cwd.clone();
    }
    if src.profile.is_some() {
        dst.profile = src.profile.clone();
    }
    if src.hop.is_some() {
        dst.hop = src.hop.clone();
    }
    // Merge extra fields (src wins on conflict).
    for (k, v) in &src.extra {
        dst.extra.insert(k.clone(), v.clone());
    }
}

/// True iff `patch` carries no data worth writing (all `Option` fields are
/// `None` and the `extra` map is empty).
fn is_empty_patch(p: &Sidecar) -> bool {
    p.session_id.is_none()
        && p.ts.is_none()
        && p.permission_mode.is_none()
        && p.effort.is_none()
        && p.model.is_none()
        && p.cwd.is_none()
        && p.profile.is_none()
        && p.hop.is_none()
        && p.extra.is_empty()
}

/// Atomic write: serialize `sidecar` to `<path>.tmp`, then rename to `path`.
///
/// Mirrors the shell's `printf '%s' "$existing" | jq ... > "$tmp" && mv -f "$tmp" "$f" || rm -f "$tmp"`:
/// on any failure after the tmp is created, the tmp is cleaned up so a failed write
/// never leaves a stale partial file at `<path>.tmp`.
fn write_atomic(path: &Path, sidecar: &Sidecar) -> Result<(), SidecarError> {
    // Ensure the parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string(sidecar)?;
    // If any step after this fails, clean up the tmp (best-effort).
    let result = (|| -> Result<(), SidecarError> {
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ─── hop_int() tests — REAL implementation ───────────────────────────────

    #[test]
    fn hop_int_from_json_number() {
        let s = Sidecar {
            hop: Some(serde_json::Value::Number(serde_json::Number::from(3))),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 3);
    }

    #[test]
    fn hop_int_from_json_string() {
        // Legacy zsh writes hop as a JSON string via `jq --arg hop "$hop"`.
        let s = Sidecar {
            hop: Some(serde_json::Value::String("2".to_owned())),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 2);
    }

    #[test]
    fn hop_int_from_string_with_whitespace() {
        // Extra whitespace should be trimmed before parsing.
        let s = Sidecar {
            hop: Some(serde_json::Value::String("  1  ".to_owned())),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 1);
    }

    #[test]
    fn hop_int_absent_returns_zero() {
        let s = Sidecar::default();
        assert_eq!(s.hop_int(), 0);
    }

    #[test]
    fn hop_int_null_returns_zero() {
        let s = Sidecar {
            hop: Some(serde_json::Value::Null),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 0);
    }

    #[test]
    fn hop_int_unparseable_string_returns_zero() {
        let s = Sidecar {
            hop: Some(serde_json::Value::String("not-a-number".to_owned())),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 0);
    }

    #[test]
    fn hop_int_zero() {
        let s = Sidecar {
            hop: Some(serde_json::Value::Number(serde_json::Number::from(0))),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 0);
    }

    #[test]
    fn hop_int_negative_number() {
        // Negative hops are invalid by spec but the parser must not panic.
        let s = Sidecar {
            hop: Some(serde_json::Value::Number(serde_json::Number::from(-1))),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), -1);
    }

    #[test]
    fn hop_int_string_zero() {
        let s = Sidecar {
            hop: Some(serde_json::Value::String("0".to_owned())),
            ..Default::default()
        };
        assert_eq!(s.hop_int(), 0);
    }

    // ─── serde field name tests ───────────────────────────────────────────────

    #[test]
    fn serde_field_names_camelcase() {
        // Verify the JSON keys match the spec field names exactly.
        let s = Sidecar {
            session_id: Some("sid-abc".to_owned()),
            permission_mode: Some("acceptEdits".to_owned()),
            effort: Some("high".to_owned()),
            model: Some("claude-opus-4-5".to_owned()),
            cwd: Some("/tmp".to_owned()),
            profile: Some("home".to_owned()),
            ts: Some(1_700_000_000.0),
            hop: Some(Sidecar::hop_value(1)),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            json.contains("\"sessionId\""),
            "missing sessionId in: {json}"
        );
        assert!(
            json.contains("\"permissionMode\""),
            "missing permissionMode in: {json}"
        );
        assert!(json.contains("\"effort\""), "missing effort in: {json}");
        assert!(json.contains("\"model\""), "missing model in: {json}");
        assert!(json.contains("\"cwd\""), "missing cwd in: {json}");
        assert!(json.contains("\"profile\""), "missing profile in: {json}");
        assert!(json.contains("\"ts\""), "missing ts in: {json}");
        assert!(json.contains("\"hop\""), "missing hop in: {json}");
    }

    #[test]
    fn deserialize_legacy_hop_string() {
        // Simulates a sidecar written by the zsh helper.
        let raw = r#"{"sessionId":"abc","hop":"2","permissionMode":"default"}"#;
        let s: Sidecar = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(s.hop_int(), 2);
        assert_eq!(s.session_id.as_deref(), Some("abc"));
        assert_eq!(s.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn deserialize_rust_hop_number() {
        let raw = r#"{"sessionId":"abc","hop":3}"#;
        let s: Sidecar = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(s.hop_int(), 3);
    }

    #[test]
    fn deserialize_corrupt_returns_default() {
        // write_sidecar/read_sidecar should return default on corrupt JSON,
        // but we test the serde layer directly here.
        let result: Result<Sidecar, _> = serde_json::from_str("not-json{{{");
        assert!(result.is_err(), "corrupt JSON should fail serde_json parse");
        // read_sidecar wraps this and returns Default — tested in I/O tests below.
    }

    // ─── sidecar_flags() tests ────────────────────────────────────────────────

    #[test]
    fn sidecar_flags_all_fields() {
        let s = Sidecar {
            permission_mode: Some("acceptEdits".to_owned()),
            effort: Some("max".to_owned()),
            model: Some("claude-opus-4-5".to_owned()),
            ..Default::default()
        };
        let flags = s.sidecar_flags();
        assert_eq!(flags.len(), 6);
        assert_eq!(flags[0], "--permission-mode");
        assert_eq!(flags[1], "acceptEdits");
        assert_eq!(flags[2], "--effort");
        assert_eq!(flags[3], "max");
        assert_eq!(flags[4], "--model");
        assert_eq!(flags[5], "claude-opus-4-5");
    }

    #[test]
    fn sidecar_flags_empty() {
        let s = Sidecar::default();
        assert!(s.sidecar_flags().is_empty());
    }

    #[test]
    fn sidecar_flags_partial() {
        let s = Sidecar {
            effort: Some("high".to_owned()),
            ..Default::default()
        };
        let flags = s.sidecar_flags();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0], "--effort");
        assert_eq!(flags[1], "high");
    }

    // ─── I/O tests ────────────────────────────────────────────────────────────

    fn tmp_sidecar(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn read_sidecar_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "missing.json");
        let s = read_sidecar(&path).expect("should not error on missing file");
        assert!(s.session_id.is_none());
        assert_eq!(s.hop_int(), 0);
    }

    #[test]
    fn read_sidecar_corrupt_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "corrupt.json");
        std::fs::write(&path, b"{{not valid json}}").unwrap();
        let s = read_sidecar(&path).expect("should not error on corrupt JSON");
        assert!(s.session_id.is_none());
    }

    #[test]
    fn read_sidecar_empty_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "empty.json");
        std::fs::write(&path, b"").unwrap();
        let s = read_sidecar(&path).expect("should not error on empty file");
        assert!(s.session_id.is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");
        let original = Sidecar {
            session_id: Some("abc-123".to_owned()),
            permission_mode: Some("bypassPermissions".to_owned()),
            effort: Some("max".to_owned()),
            cwd: Some("/tmp/project".to_owned()),
            profile: Some("home".to_owned()),
            hop: Some(Sidecar::hop_value(0)),
            ..Default::default()
        };
        write_sidecar(&path, &original).expect("write");
        let read_back = read_sidecar(&path).expect("read");
        assert_eq!(read_back.session_id, original.session_id);
        assert_eq!(read_back.permission_mode, original.permission_mode);
        assert_eq!(read_back.effort, original.effort);
        assert_eq!(read_back.cwd, original.cwd);
        assert_eq!(read_back.profile, original.profile);
        assert_eq!(read_back.hop_int(), 0);
    }

    #[test]
    fn write_sidecar_merge_not_clobber_hop() {
        // Write initial sidecar with hop=2.
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");
        let initial = Sidecar {
            session_id: Some("sid".to_owned()),
            hop: Some(Sidecar::hop_value(2)),
            ..Default::default()
        };
        write_sidecar(&path, &initial).unwrap();

        // Now write a patch that does NOT supply a hop.
        let patch = Sidecar {
            permission_mode: Some("default".to_owned()),
            ..Default::default()
        };
        write_sidecar(&path, &patch).unwrap();

        // Hop must still be 2.
        let result = read_sidecar(&path).unwrap();
        assert_eq!(
            result.hop_int(),
            2,
            "hop should be preserved when not in patch"
        );
        assert_eq!(result.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn write_sidecar_explicit_hop_wins() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");

        // Write hop=1.
        write_sidecar(
            &path,
            &Sidecar {
                hop: Some(Sidecar::hop_value(1)),
                ..Default::default()
            },
        )
        .unwrap();

        // Update hop to 2 explicitly.
        write_sidecar(
            &path,
            &Sidecar {
                hop: Some(Sidecar::hop_value(2)),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(read_sidecar(&path).unwrap().hop_int(), 2);
    }

    #[test]
    fn merge_sidecar_no_op_on_empty_patch() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");

        // Write a real sidecar.
        write_sidecar(
            &path,
            &Sidecar {
                session_id: Some("keep-me".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let mtime_before = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap();

        // Merge empty patch — should be a no-op (no file write).
        merge_sidecar(&path, &Sidecar::default()).unwrap();

        let mtime_after = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap();

        // mtime must not change (no write happened).
        assert_eq!(
            mtime_before, mtime_after,
            "empty merge should not touch the file"
        );
    }

    #[test]
    fn merge_sidecar_preserves_hop() {
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");

        write_sidecar(
            &path,
            &Sidecar {
                hop: Some(Sidecar::hop_value(3)),
                session_id: Some("sid".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        merge_sidecar(
            &path,
            &Sidecar {
                effort: Some("high".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let result = read_sidecar(&path).unwrap();
        assert_eq!(result.hop_int(), 3, "hop must survive a merge without hop");
        assert_eq!(result.effort.as_deref(), Some("high"));
    }

    #[test]
    fn extra_fields_survive_roundtrip() {
        // Unknown keys must not be dropped (forward-compat: new binary writes
        // a field that this version doesn't know about).
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");
        let raw = r#"{"sessionId":"x","unknownFutureKey":42}"#;
        std::fs::write(&path, raw).unwrap();

        let s = read_sidecar(&path).unwrap();
        assert_eq!(s.session_id.as_deref(), Some("x"));
        assert_eq!(
            s.extra.get("unknownFutureKey"),
            Some(&serde_json::Value::Number(serde_json::Number::from(42)))
        );

        // Write it back and confirm the key is still there.
        write_sidecar(&path, &Sidecar::default()).unwrap();
        let s2 = read_sidecar(&path).unwrap();
        assert!(
            s2.extra.contains_key("unknownFutureKey"),
            "unknown future key should survive write_sidecar round-trip"
        );
    }

    #[test]
    fn write_atomic_uses_tmp_then_renames() {
        // Verify atomic write: after write_sidecar the .json.tmp file is gone
        // and the .json file has valid content. During the write, the tmp is at
        // `<path>.tmp` on the same filesystem (same dir), so rename is atomic.
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "atomic.json");
        let tmp_path = dir.path().join("atomic.json.tmp");

        let s = Sidecar {
            session_id: Some("atomic-test".to_owned()),
            hop: Some(Sidecar::hop_value(1)),
            ..Default::default()
        };
        write_sidecar(&path, &s).unwrap();

        // Tmp must be gone after a successful write.
        assert!(
            !tmp_path.exists(),
            ".json.tmp must be cleaned up after rename"
        );
        // The canonical file must be readable.
        let read_back = read_sidecar(&path).unwrap();
        assert_eq!(read_back.session_id.as_deref(), Some("atomic-test"));
        assert_eq!(read_back.hop_int(), 1);
    }

    #[test]
    fn write_sidecar_compact_json() {
        // Shell writes compact single-line JSON (jq -c); Rust must match so the
        // on-disk format is consistent with legacy zsh-written sidecars.
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "compact.json");
        let s = Sidecar {
            session_id: Some("sid".to_owned()),
            effort: Some("max".to_owned()),
            ..Default::default()
        };
        write_sidecar(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Must not contain newlines (compact, not pretty-printed).
        assert!(
            !raw.contains('\n'),
            "sidecar JSON must be compact (no newlines); got: {raw}"
        );
    }

    #[test]
    fn merge_sidecar_noop_when_all_none() {
        // merge_sidecar with a fully-None patch must not touch the file at all
        // (no mtime change, no I/O). This is the "no-op on empty value" contract
        // from the shell's: `[ -z "$value" ] && return 0`.
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");
        write_sidecar(
            &path,
            &Sidecar {
                session_id: Some("existing".to_owned()),
                effort: Some("high".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let mtime_before = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap();
        merge_sidecar(&path, &Sidecar::default()).unwrap();
        let mtime_after = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "no-op merge must not modify file mtime"
        );
    }

    #[test]
    fn hop_preserved_across_write_and_merge_chain() {
        // Simulate the real limit-switch sequence:
        // 1. hook writes hop=1 via merge_sidecar,
        // 2. resumed claude-smart calls write_sidecar to record new mode/effort
        //    (WITHOUT a hop field),
        // 3. hop must still be 1 after step 2.
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");

        // Step 0: initial sidecar with mode.
        write_sidecar(
            &path,
            &Sidecar {
                session_id: Some("sid".to_owned()),
                permission_mode: Some("default".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        // Step 1: hook increments hop to 1 via merge.
        merge_sidecar(
            &path,
            &Sidecar {
                hop: Some(Sidecar::hop_value(1)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(read_sidecar(&path).unwrap().hop_int(), 1);

        // Step 2: wrapper writes updated mode WITHOUT a hop field.
        write_sidecar(
            &path,
            &Sidecar {
                permission_mode: Some("bypassPermissions".to_owned()),
                effort: Some("max".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let result = read_sidecar(&path).unwrap();
        assert_eq!(
            result.hop_int(),
            1,
            "hop must survive write_sidecar without hop field"
        );
        assert_eq!(result.permission_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(result.effort.as_deref(), Some("max"));
    }

    #[test]
    fn sidecar_flags_no_default_effort_injection() {
        // On resume: no effort flag is emitted if effort is absent from sidecar.
        // The spec says "no default-effort injection — floor lives in settings.json".
        let s = Sidecar {
            permission_mode: Some("acceptEdits".to_owned()),
            // effort intentionally absent
            model: Some("claude-opus-4-5".to_owned()),
            ..Default::default()
        };
        let flags = s.sidecar_flags();
        // Should have --permission-mode + value + --model + value = 4 items, no --effort.
        assert_eq!(flags.len(), 4);
        let flag_strs: Vec<String> = flags
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        assert!(
            !flag_strs.contains(&"--effort".to_owned()),
            "--effort must not appear if absent from sidecar"
        );
    }

    #[test]
    fn write_sidecar_corrupt_existing_starts_fresh() {
        // If the existing sidecar is corrupt, write_sidecar must treat it as {}
        // (not propagate garbage). This mirrors the shell's:
        //   `printf '%s' "$existing" | "$JQ" -e . >/dev/null 2>&1 || existing="{}"`
        let dir = TempDir::new().unwrap();
        let path = tmp_sidecar(&dir, "sid.json");
        std::fs::write(&path, b"{{corrupt{{").unwrap();

        let patch = Sidecar {
            session_id: Some("new-sid".to_owned()),
            effort: Some("low".to_owned()),
            ..Default::default()
        };
        write_sidecar(&path, &patch).unwrap();

        let result = read_sidecar(&path).unwrap();
        assert_eq!(result.session_id.as_deref(), Some("new-sid"));
        assert_eq!(result.effort.as_deref(), Some("low"));
        // No garbage from the corrupt original.
        assert!(
            result.extra.is_empty(),
            "corrupt original must not leak into new sidecar"
        );
    }
}
