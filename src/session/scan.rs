//! Session scan + incremental index.
//!
//! ## Algorithm (spec §2 "Session scan / index")
//!
//! 1. **Dual cwd encoding** — `paths::encode_cwd(cwd)` returns `(current, legacy)`.
//!    Both encoded names are checked under `paths::session_base_dir()`; results
//!    are unioned and deduplicated by path.
//! 2. **Incremental index** — `scan-meta-v2.<enc>.tsv` (one per cwd variant) stores
//!    rows with the refresh-start epoch *inside* the file header.  The v2 prefix
//!    ensures no collision with the legacy zsh `scan-meta.<enc>.tsv`.  An absent or
//!    unreadable index is treated as a full-reindex trigger.
//! 3. **Per-transcript label extraction** — for each transcript `.jsonl` file not
//!    yet in the index (mtime newer than the stored refresh-start epoch), extract:
//!    - `ai-title` from the last `title` assistant message in the first 256 KiB,
//!    - fall back to the last human `message.content` in the tail 4 MiB,
//!    - fall back to the first human message in the whole file.
//!    `mode` = last `permissionMode` seen in the whole file.
//! 4. **Dedup by sid** — newest `mtime` wins (`>=` — not `>`).
//! 5. **Output** — `Vec<SessionRow>` sorted newest-first.
//!
//! This file's public entry-point is [`scan_sessions`], called by
//! `session::scan()` in `mod.rs`.

use super::SessionRow;
use std::path::Path;

/// Scan `cwd` for Claude Code transcript directories and return session rows
/// sorted newest-first, deduplicated by `sid`.
///
/// ## Implementation status
///
/// Phase 0: the signature, types, and wiring are final.  The body is
/// `unimplemented!()` — full walkdir + mtime comparison + JSON label extraction
/// is implemented in Phase 5 (spec scaffold §6 step 5).
pub fn scan_sessions(_cwd: &Path) -> Vec<SessionRow> {
    unimplemented!(
        "scan_sessions: walkdir + mtime index + label extraction \
         (Phase 5 — see spec §2 'Session scan / index' and scaffold §6 step 5)"
    )
}

/// Parse a single `.jsonl` transcript file and extract:
/// - The `mode` string (last `permissionMode` seen, empty if absent).
/// - The display `label` (ai-title → last-prompt → first-user-prompt fallback,
///   ≤ 80 chars).
///
/// Reads at most the first 256 KiB + tail 4 MiB as a byte window (spec §2).
/// Returns `(mode, label)`.  On any I/O or parse error returns `("", "")`.
///
/// Phase 0: body is `unimplemented!()`.
#[allow(dead_code)]
pub(crate) fn extract_label_and_mode(_transcript_path: &Path) -> (String, String) {
    unimplemented!(
        "extract_label_and_mode: head 256KiB + tail 4MiB byte window, \
         ai-title/last-prompt/first-user fallback chain (Phase 5)"
    )
}

/// Load a `scan-meta-v2.<enc>.tsv` index and return its refresh-start epoch
/// plus the cached rows.  Returns `None` if the file is absent, unreadable, or
/// its header is malformed (treating all those cases as "full reindex needed").
///
/// Index file format (one-line header + TSV body rows):
/// ```text
/// # refresh-start: <epoch_secs>
/// <sid>\t<mtime>\t<human_ts>\t<mode>\t<label>
/// …
/// ```
///
/// Phase 0: body is `unimplemented!()`.
#[allow(dead_code)]
pub(crate) fn load_scan_index(_index_path: &Path) -> Option<(i64, Vec<SessionRow>)> {
    unimplemented!(
        "load_scan_index: read scan-meta-v2 header + rows (Phase 5)"
    )
}

/// Write a `scan-meta-v2.<enc>.tsv` index atomically (tmp + rename).
///
/// Phase 0: body is `unimplemented!()`.
#[allow(dead_code)]
pub(crate) fn write_scan_index(
    _index_path: &Path,
    _refresh_start: i64,
    _rows: &[SessionRow],
) -> std::io::Result<()> {
    unimplemented!(
        "write_scan_index: atomic tmp+rename of scan-meta-v2 (Phase 5)"
    )
}
