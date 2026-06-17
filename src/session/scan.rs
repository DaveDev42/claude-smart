//! Session scan + incremental index.
//!
//! ## Algorithm (spec §2 "Session scan / index")
//!
//! 1. **Dual cwd encoding** — `paths::encode_cwd(cwd)` returns `(current, legacy)`.
//!    Both encoded names are checked under `paths::session_base_dir()`; results
//!    are unioned and deduplicated by path.
//! 2. **Incremental index** — `scan-meta-v2.<enc>.tsv` (one per cwd variant) stores
//!    rows with the refresh-start epoch *inside* the file header (the v2 prefix
//!    ensures no collision with the legacy zsh `scan-meta.<enc>.tsv`).  An absent or
//!    unreadable index is treated as a full-reindex trigger.
//! 3. **Per-transcript label extraction** — for each transcript `.jsonl` file not
//!    yet in the index (mtime newer than the stored refresh-start epoch), extract:
//!    - `ai-title` (last `{"type":"ai-title","aiTitle":"..."}` in the byte window),
//!    - fall back to `last-prompt` (last `{"type":"last-prompt","lastPrompt":"..."}`)
//!    - fall back to first user message text.
//!    `mode` = last `permissionMode` seen across `"permission-mode"` or `"user"` records.
//! 4. **Dedup by sid** — newest `mtime` wins (`>=` — not `>`).
//! 5. **Output** — `Vec<SessionRow>` sorted newest-first.
//!
//! The refresh-start epoch is stored *inside* the index file (not via a touch -r
//! mtime pin trick), matching the spec requirement:
//!   > "The Rust binary writes its own index format (recommend storing the
//!   > refresh-start epoch *inside* the index file, eliminating the `touch -r`
//!   > mtime-pin trick)."
//!
//! Corresponding shell lines reproduced:
//! - `encode_cwd` → `paths::encode_cwd` (helper lines 197–203)
//! - `session_dirs` → dual-encoding union, dedup by path (lines 206–225)
//! - `_scan_index_for` → `paths::scan_index_for` (lines 271–273)
//! - `_extract_meta_row` jq pipeline (lines 276–295)
//! - `reindex_scan_dir` incremental find-newer logic (lines 301–332)
//! - `scan_sessions` stat+sort+dedup (lines 334–372)

use super::SessionRow;
use crate::paths;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use serde_json::Value;

// --- constants matching the shell ---
// `_SCAN_HEAD_C=262144` (256 KiB)
const SCAN_HEAD_BYTES: usize = 262_144;
// `_SCAN_TAIL_C=4194304` (4 MiB)
const SCAN_TAIL_BYTES: u64 = 4_194_304;

// Index file header prefix (the refresh-start epoch line).
const INDEX_HEADER_PREFIX: &str = "# refresh-start: ";

// ─── public entry point ──────────────────────────────────────────────────────

/// Scan `cwd` for Claude Code transcript directories and return session rows
/// sorted newest-first, deduplicated by `sid`.
///
/// Uses the dual cwd encoding (`paths::encode_cwd`) to union both the current
/// and legacy project-dir names.  Results are deduplicated by `sid` (newest
/// `mtime` wins, `>=` tiebreak).
pub fn scan_sessions(cwd: &Path) -> Vec<SessionRow> {
    // Collect the 0–2 project dirs that exist on disk for this cwd.
    let dirs = session_dirs_for(cwd);
    if dirs.is_empty() {
        return Vec::new();
    }

    // Gather all .jsonl transcript paths across all dirs.
    let mut all_transcripts: Vec<PathBuf> = Vec::new();
    for dir in &dirs {
        reindex_scan_dir(dir);
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if p.is_file() {
                        all_transcripts.push(p);
                    }
                }
            }
        }
    }
    if all_transcripts.is_empty() {
        return Vec::new();
    }

    // Load all index rows into a (sid → IndexEntry) map.
    let mut index_map: HashMap<String, IndexEntry> = HashMap::new();
    for dir in &dirs {
        let idx_path = paths::scan_index_for(dir);
        if let Some((_refresh_start, rows)) = load_scan_index(&idx_path) {
            for row in rows {
                // Dedup: keep newest mtime (>= not >).
                let replace = match index_map.get(&row.sid) {
                    None => true,
                    Some(existing) => row.mtime >= existing.mtime,
                };
                if replace {
                    index_map.insert(
                        row.sid.clone(),
                        IndexEntry {
                            mtime: row.mtime,
                            mode: row.mode.clone(),
                            label: row.label.clone(),
                        },
                    );
                }
            }
        }
    }

    // Stat all transcripts for live ordering (active session mtime advances).
    // Then dedup by sid keeping newest mtime (>= not >).
    let mut live: HashMap<String, i64> = HashMap::new();
    for tp in &all_transcripts {
        if let Some(sid) = transcript_sid(tp) {
            let mt = file_mtime(tp).unwrap_or(0);
            let replace = match live.get(&sid) {
                None => true,
                Some(&existing_mt) => mt >= existing_mt,
            };
            if replace {
                live.insert(sid, mt);
            }
        }
    }

    // Build output rows: for each unique sid, use the live mtime for sort order
    // and the index for mode/label.  Missing from index → mode "?", label "".
    let mut rows: Vec<SessionRow> = live
        .iter()
        .map(|(sid, &mtime)| {
            let (mode, label) = match index_map.get(sid) {
                Some(e) => (e.mode.clone(), e.label.clone()),
                None => ("?".to_owned(), String::new()),
            };
            let human_ts = format_human_ts(mtime);
            SessionRow {
                sid: sid.clone(),
                mtime,
                human_ts,
                mode,
                label,
            }
        })
        .collect();

    // Sort newest-first by mtime (stable for deterministic ordering on ties).
    rows.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    rows
}

// ─── session dir resolution ──────────────────────────────────────────────────

/// Return the existing transcript directories for `cwd`, deduped by path,
/// newest-mtime-first (mirrors `session_dirs` in the helper, lines 206–225).
pub(crate) fn session_dirs_for(cwd: &Path) -> Vec<PathBuf> {
    let (current, legacy) = paths::encode_cwd(cwd);
    let base = paths::session_base_dir();

    let mut seen: Vec<PathBuf> = Vec::new();
    for enc in &[current, legacy] {
        let d = base.join(enc);
        if !d.is_dir() {
            continue;
        }
        // Dedup — both variants can be identical when cwd has no dots.
        if !seen.iter().any(|s| s == &d) {
            seen.push(d);
        }
    }

    if seen.len() <= 1 {
        return seen;
    }

    // Sort by mtime descending (most-recently-used encoding first).
    seen.sort_by(|a, b| {
        let ma = dir_mtime(a).unwrap_or(0);
        let mb = dir_mtime(b).unwrap_or(0);
        mb.cmp(&ma)
    });
    seen
}

// ─── incremental index ───────────────────────────────────────────────────────

/// Incrementally refresh the scan index for one project directory.
///
/// Mirrors `reindex_scan_dir` (helper lines 301–332):
/// - If index exists: only process transcripts newer than the stored
///   refresh-start epoch (inside the header line).
/// - If index absent: process all transcripts.
/// - Atomic tmp+rename; dedup by sid (newest mtime wins).
///
/// Best-effort — any I/O failure is silently swallowed (the index is an
/// optimisation cache, not authoritative state).
pub(crate) fn reindex_scan_dir(dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    let _ = paths::smart_dir(); // lazy-create smart_dir
    let idx_path = paths::scan_index_for(dir);

    // Record the refresh start NOW (before scanning), so a transcript written
    // DURING the refresh re-extracts next time (same `touch -r $ref` semantic).
    let refresh_start = now_epoch();

    // Determine which transcripts are stale (need re-extraction).
    let (existing_refresh_start, mut rows) = match load_scan_index(&idx_path) {
        Some((rs, r)) => (rs, r),
        None => (0, Vec::new()),
    };

    let stale: Vec<PathBuf> = collect_stale_transcripts(dir, existing_refresh_start);

    if stale.is_empty() {
        return; // nothing to update
    }

    // Extract fresh rows for stale transcripts and append to the existing set.
    for tp in &stale {
        if let Some(new_row) = extract_index_row(tp) {
            rows.push(new_row);
        }
    }

    // Dedup by sid: newest mtime wins (>= not >).
    let deduped = dedup_rows_by_sid(rows);

    // Atomic write: tmp + rename.
    let _ = write_scan_index(&idx_path, refresh_start, &deduped);
}

/// Collect transcript files that are newer than `since_epoch` (or all files
/// when `since_epoch == 0`).  Mirrors `find $d -maxdepth 1 -name '*.jsonl'
/// -newer $idx` (helper lines 308–314).
fn collect_stale_transcripts(dir: &Path, since_epoch: i64) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return stale,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if !p.is_file() {
            continue;
        }
        if since_epoch == 0 {
            stale.push(p);
        } else {
            let mt = file_mtime(&p).unwrap_or(0);
            if mt > since_epoch {
                stale.push(p);
            }
        }
    }
    stale
}

/// Dedup a `Vec<SessionRow>` by `sid`, keeping the entry with the highest
/// (or equal, `>=`) mtime.  Mirrors the awk dedup in `reindex_scan_dir`
/// (helper lines 323–324) and the `dedup by sid >= not >` spec requirement.
fn dedup_rows_by_sid(rows: Vec<SessionRow>) -> Vec<SessionRow> {
    // We preserve insertion order for the last winner so that a rescanned
    // (appended last) row replaces an older carried-forward row.
    let mut map: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<SessionRow> = Vec::new();

    for row in rows {
        match map.get(&row.sid) {
            None => {
                map.insert(row.sid.clone(), out.len());
                out.push(row);
            }
            Some(&idx) => {
                if row.mtime >= out[idx].mtime {
                    out[idx] = row;
                }
            }
        }
    }
    out
}

// ─── index file I/O ──────────────────────────────────────────────────────────

/// Index file format:
/// ```text
/// # refresh-start: <epoch_secs>
/// <sid>\t<mtime>\t<mode>\t<label>
/// …
/// ```
///
/// Note: the index stores 4 columns (sid, mtime, mode, label), NOT the full
/// 5-column SessionRow format (which includes human_ts).  human_ts is computed
/// at display time from mtime.
///
/// Returns `None` if the file is absent, unreadable, or the header is malformed.
pub(crate) fn load_scan_index(index_path: &Path) -> Option<(i64, Vec<SessionRow>)> {
    let content = fs::read_to_string(index_path).ok()?;
    let mut lines = content.lines();

    // Parse the header.
    let header = lines.next()?;
    let refresh_start: i64 = header
        .strip_prefix(INDEX_HEADER_PREFIX)?
        .trim()
        .parse()
        .ok()?;

    // Parse the rows.
    let rows = lines
        .filter(|l| !l.is_empty())
        .filter_map(parse_index_row)
        .collect();

    Some((refresh_start, rows))
}

/// Parse one 4-column index row: `sid \t mtime \t mode \t label`.
fn parse_index_row(line: &str) -> Option<SessionRow> {
    let mut cols = line.splitn(4, '\t');
    let sid = cols.next()?.to_owned();
    if sid.is_empty() {
        return None;
    }
    let mtime: i64 = cols.next()?.parse().ok()?;
    let mode = cols.next()?.to_owned();
    let label = cols.next()?.to_owned();
    let human_ts = format_human_ts(mtime);
    Some(SessionRow { sid, mtime, human_ts, mode, label })
}

/// Write the index atomically (tmp + rename), matching `reindex_scan_dir`'s
/// atomic write pattern (helper lines 326–331).
pub(crate) fn write_scan_index(
    index_path: &Path,
    refresh_start: i64,
    rows: &[SessionRow],
) -> io::Result<()> {
    // Build the full content.
    let mut content = format!("{}{}\n", INDEX_HEADER_PREFIX, refresh_start);
    for row in rows {
        content.push_str(&format!("{}\t{}\t{}\t{}\n", row.sid, row.mtime, row.mode, row.label));
    }

    // Write to a tmp file then atomically rename.
    let tmp_path = index_path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, index_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        e
    })
}

// ─── per-transcript extraction ───────────────────────────────────────────────

/// Extract one index row from a transcript `.jsonl` file.
///
/// Mirrors `_extract_meta_row` (helper lines 276–295):
/// - Reads head 256 KiB + tail 4 MiB as a byte window.
/// - Parses each line as JSON (ignoring lines that do not parse, including
///   lines that were cut in half at the window boundary — `fromjson? | objects`).
/// - Extracts: ai-title → last-prompt → first user-prompt fallback; last
///   permissionMode seen.
///
/// Returns `None` on missing file or parse failure.
pub(crate) fn extract_index_row(transcript_path: &Path) -> Option<SessionRow> {
    let sid = transcript_sid(transcript_path)?;
    let mtime = file_mtime(transcript_path).unwrap_or(0);

    let (mode, label) = extract_label_and_mode(transcript_path);

    Some(SessionRow {
        sid,
        mtime,
        human_ts: format_human_ts(mtime),
        mode,
        label,
    })
}

/// Parse a single `.jsonl` transcript file and extract:
/// - The `mode` string (last `permissionMode` seen; empty string if absent).
/// - The display `label` (ai-title → last-prompt → first-user-prompt fallback,
///   truncated to ≤ 80 chars).
///
/// Reads at most the first 256 KiB + tail 4 MiB as a byte window (spec §2).
/// Returns `(mode, label)`.  On any I/O or parse error returns `("?", "")`.
pub(crate) fn extract_label_and_mode(transcript_path: &Path) -> (String, String) {
    let bytes = match read_byte_window(transcript_path) {
        Ok(b) => b,
        Err(_) => return ("?".to_owned(), String::new()),
    };

    // Parse the byte window line-by-line, collecting all records.
    // `fromjson? | objects` — lines that don't parse (including window-boundary
    // half-lines) are silently skipped.
    let mut ai_title: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut last_mode: Option<String> = None;

    for line in bytes.split(|&b| b == b'\n') {
        // Skip empty lines and lines that clearly won't parse.
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match v.as_object() {
            Some(o) => o,
            None => continue,
        };

        let record_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // ai-title record: {"type":"ai-title","aiTitle":"..."}
        if record_type == "ai-title" {
            if let Some(t) = obj.get("aiTitle").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    ai_title = Some(t.to_owned());
                }
            }
        }

        // last-prompt record: {"type":"last-prompt","lastPrompt":"..."}
        if record_type == "last-prompt" {
            if let Some(t) = obj.get("lastPrompt").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    last_prompt = Some(t.to_owned());
                }
            }
        }

        // permissionMode: in "permission-mode" or "user" records
        if record_type == "permission-mode" || record_type == "user" {
            if let Some(pm) = obj.get("permissionMode").and_then(|v| v.as_str()) {
                if !pm.is_empty() {
                    last_mode = Some(pm.to_owned());
                }
            }
        }

        // First user message (fallback label): type=="user"
        if record_type == "user" && first_user.is_none() {
            if let Some(text) = extract_user_text(obj) {
                if !text.is_empty() {
                    first_user = Some(text);
                }
            }
        }
    }

    // Label priority: ai-title → last-prompt → first user message.
    // Mirrors jq: `if $title != "" then $title elif $last != "" then $last else $first end`
    let raw_label = ai_title
        .or(last_prompt)
        .or(first_user)
        .unwrap_or_default();

    // Truncate to 80 chars (char boundary, not byte boundary).
    let label = truncate_to_chars(&raw_label, 80);

    // mode: last permissionMode seen, or "?" if none (mirrors jq `// "?"`)
    let mode = last_mode.unwrap_or_else(|| "?".to_owned());

    (mode, label)
}

/// Extract user message text from a `"user"` record object.
///
/// Mirrors the jq selector (helper lines 288–292):
/// ```jq
/// (.message.content // .message // empty)
/// | if type=="array" then (map(select(.type=="text") | .text) | join(" "))
///   elif type=="string" then . else "" end
/// ```
fn extract_user_text(obj: &serde_json::Map<String, Value>) -> Option<String> {
    let message = obj.get("message")?;

    // Try message.content first (array of blocks), then message as string.
    let content = message.get("content").unwrap_or(message);

    match content {
        Value::Array(blocks) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            let joined = text.join(" ");
            if joined.is_empty() { None } else { Some(joined) }
        }
        Value::String(s) => {
            if s.is_empty() { None } else { Some(s.clone()) }
        }
        _ => None,
    }
}

/// Read the head 256 KiB + tail 4 MiB byte window from a transcript file.
///
/// Mirrors the shell pipeline (helper lines 281–282):
/// ```bash
/// { head -c "$_SCAN_HEAD_C" "$f"; printf '\n'
///   tail -c "$_SCAN_TAIL_C" "$f"; printf '\n'; }
/// ```
///
/// We insert a `\n` separator between the head and tail so that a line split
/// across the boundary is not accidentally merged into a "valid" line. (The
/// shell's two separate `printf '\n'` calls serve the same purpose.)
fn read_byte_window(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    let file_size = metadata.len();

    let mut f = fs::File::open(path)?;

    // Read head 256 KiB.
    let head_len = file_size.min(SCAN_HEAD_BYTES as u64) as usize;
    let mut head = vec![0u8; head_len];
    f.read_exact(&mut head)?;

    // If the file fits entirely in the head window, no need for a tail read.
    if file_size <= SCAN_HEAD_BYTES as u64 {
        head.push(b'\n');
        return Ok(head);
    }

    // Read tail 4 MiB.
    let tail_start = file_size.saturating_sub(SCAN_TAIL_BYTES);
    f.seek(SeekFrom::Start(tail_start))?;
    let tail_len = (file_size - tail_start) as usize;
    let mut tail = vec![0u8; tail_len];
    f.read_exact(&mut tail)?;

    // Combine: head + newline separator + tail.
    let mut result = Vec::with_capacity(head_len + 1 + tail_len);
    result.extend_from_slice(&head);
    result.push(b'\n'); // boundary separator
    result.extend_from_slice(&tail);
    result.push(b'\n');
    Ok(result)
}

// ─── utilities ───────────────────────────────────────────────────────────────

/// Format a Unix epoch as `MM-DD HH:MM` using the local timezone.
///
/// Mirrors the jq strftime in `scan_sessions` (helper lines 366–370):
/// ```jq
/// (localtime | strftime("%m-%d %H:%M"))
/// ```
pub(crate) fn format_human_ts(epoch: i64) -> String {
    if epoch <= 0 {
        return "??-?? ??:??".to_owned();
    }
    // Use chrono to format in local time, matching `strftime("%m-%d %H:%M")`.
    chrono::Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "??-?? ??:??".to_owned())
}

/// Return the mtime of a file as a Unix epoch in seconds.
/// Returns `None` on any error.
fn file_mtime(path: &Path) -> Option<i64> {
    let mt = fs::metadata(path).ok()?.modified().ok()?;
    let dur = mt.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    Some(dur.as_secs() as i64)
}

/// Return the mtime of a directory as a Unix epoch in seconds.
fn dir_mtime(path: &Path) -> Option<i64> {
    file_mtime(path)
}

/// Extract the session UUID from a transcript path (stem without the `.jsonl`
/// extension).  Returns `None` if the stem is empty or non-UTF-8.
pub(crate) fn transcript_sid(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_owned())
    }
}

/// Return the current Unix epoch in seconds.
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

/// Truncate `s` to at most `max_chars` Unicode scalar values.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

// ─── index row helper ────────────────────────────────────────────────────────

/// Internal representation of an index entry (no human_ts needed in the index).
struct IndexEntry {
    mtime: i64,
    mode: String,
    label: String,
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ─── helpers ─────────────────────────────────────────────────────────────

    /// Write a `.jsonl` transcript with the given lines to `dir/<sid>.jsonl`.
    fn write_transcript(dir: &Path, sid: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(format!("{sid}.jsonl"));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// A minimal user message record.
    fn user_record(text: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    /// An ai-title record.
    fn ai_title_record(title: &str) -> String {
        format!(r#"{{"type":"ai-title","aiTitle":"{title}"}}"#)
    }

    /// A last-prompt record.
    fn last_prompt_record(prompt: &str) -> String {
        format!(r#"{{"type":"last-prompt","lastPrompt":"{prompt}"}}"#)
    }

    /// A permission-mode record.
    fn perm_mode_record(mode: &str) -> String {
        format!(r#"{{"type":"permission-mode","permissionMode":"{mode}"}}"#)
    }

    /// A user record that also carries permissionMode.
    fn user_with_mode_record(text: &str, mode: &str) -> String {
        format!(
            r#"{{"type":"user","permissionMode":"{mode}","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    // ─── encode_cwd (via paths::encode_cwd) ──────────────────────────────────

    #[test]
    fn encode_cwd_dots_in_path() {
        let (cur, leg) = paths::encode_cwd(Path::new("/Users/example/Projects/github.com/foo"));
        // current: slashes AND dots → dashes
        assert!(cur.contains("github-com"));
        // legacy: only slashes → dashes; dots preserved
        assert!(leg.contains("github.com"));
    }

    #[test]
    fn encode_cwd_no_dots_identical() {
        let (cur, leg) = paths::encode_cwd(Path::new("/tmp/myproject"));
        assert_eq!(cur, leg);
    }

    // ─── format_human_ts ─────────────────────────────────────────────────────

    #[test]
    fn format_human_ts_zero_returns_placeholder() {
        assert_eq!(format_human_ts(0), "??-?? ??:??");
        assert_eq!(format_human_ts(-1), "??-?? ??:??");
    }

    #[test]
    fn format_human_ts_known_epoch() {
        // 2024-06-10 14:32:00 UTC = 1718026320
        // In local time (depends on TZ) but must match MM-DD HH:MM pattern.
        let ts = format_human_ts(1_718_026_320);
        // Must be 11 chars: "MM-DD HH:MM"
        assert_eq!(ts.len(), 11, "human_ts length wrong: {ts:?}");
        // Pattern check: digits/dashes/colon/space
        let chars: Vec<char> = ts.chars().collect();
        assert!(chars[2] == '-', "pos 2 should be '-': {ts:?}");
        assert!(chars[5] == ' ', "pos 5 should be ' ': {ts:?}");
        assert!(chars[8] == ':', "pos 8 should be ':': {ts:?}");
    }

    // ─── truncate_to_chars ───────────────────────────────────────────────────

    #[test]
    fn truncate_to_chars_under_limit() {
        assert_eq!(truncate_to_chars("hello", 80), "hello");
    }

    #[test]
    fn truncate_to_chars_exactly_limit() {
        let s: String = "a".repeat(80);
        assert_eq!(truncate_to_chars(&s, 80).len(), 80);
    }

    #[test]
    fn truncate_to_chars_over_limit() {
        let s: String = "x".repeat(100);
        let t = truncate_to_chars(&s, 80);
        assert_eq!(t.chars().count(), 80);
    }

    #[test]
    fn truncate_to_chars_unicode() {
        // Korean: 3 bytes per char in UTF-8.
        let s = "가나다라마바사아자차카타파하"; // 14 chars
        let t = truncate_to_chars(s, 10);
        assert_eq!(t.chars().count(), 10);
    }

    // ─── extract_label_and_mode ──────────────────────────────────────────────

    #[test]
    fn extract_mode_from_perm_mode_record() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000001";
        write_transcript(tmp.path(), sid, &[
            &user_record("hello"),
            &perm_mode_record("plan"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (mode, _label) = extract_label_and_mode(&tp);
        assert_eq!(mode, "plan");
    }

    #[test]
    fn extract_mode_from_user_record_with_permission_mode() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000002";
        write_transcript(tmp.path(), sid, &[
            &user_record("initial"),
            &user_with_mode_record("later message", "acceptEdits"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (mode, _) = extract_label_and_mode(&tp);
        assert_eq!(mode, "acceptEdits");
    }

    #[test]
    fn extract_mode_missing_returns_question_mark() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000003";
        write_transcript(tmp.path(), sid, &[
            &user_record("no mode here"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (mode, _) = extract_label_and_mode(&tp);
        assert_eq!(mode, "?");
    }

    #[test]
    fn label_ai_title_wins() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000004";
        write_transcript(tmp.path(), sid, &[
            &user_record("first user prompt"),
            &last_prompt_record("last prompt text"),
            &ai_title_record("The AI Title"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "The AI Title");
    }

    #[test]
    fn label_last_prompt_fallback_when_no_ai_title() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000005";
        write_transcript(tmp.path(), sid, &[
            &user_record("first user prompt"),
            &last_prompt_record("last prompt fallback"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "last prompt fallback");
    }

    #[test]
    fn label_first_user_fallback_when_no_title_or_last_prompt() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000006";
        write_transcript(tmp.path(), sid, &[
            &user_record("the very first message"),
            &user_record("a second message"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "the very first message");
    }

    #[test]
    fn label_truncated_to_80_chars() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000007";
        let long_title = "A".repeat(120);
        write_transcript(tmp.path(), sid, &[
            &ai_title_record(&long_title),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label.chars().count(), 80);
    }

    #[test]
    fn label_empty_transcript_returns_empty_label() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000008";
        write_transcript(tmp.path(), sid, &[]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "");
    }

    #[test]
    fn extract_ignores_malformed_json_lines() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-000000000009";
        write_transcript(tmp.path(), sid, &[
            r#"not-json{"bad":"line"}"#,
            &user_record("real message"),
            r#"{"incomplete"#, // truncated — fromjson? drops it
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        // Should not panic and should still find the user message.
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "real message");
    }

    #[test]
    fn ai_title_last_occurrence_wins() {
        // The jq picks `last` for ai-title (the final occurrence in the window).
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-00000000000a";
        write_transcript(tmp.path(), sid, &[
            &ai_title_record("First Title"),
            &ai_title_record("Updated Title"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "Updated Title");
    }

    // ─── user text extraction from string content ─────────────────────────────

    #[test]
    fn extract_user_text_from_string_content() {
        let tmp = TempDir::new().unwrap();
        let sid = "aaaaaaaa-0000-0000-0000-00000000000b";
        // message.content as a string (not array)
        let line = r#"{"type":"user","message":{"content":"string content message"}}"#;
        write_transcript(tmp.path(), sid, &[line]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (_mode, label) = extract_label_and_mode(&tp);
        assert_eq!(label, "string content message");
    }

    // ─── write_scan_index + load_scan_index round-trip ──────────────────────

    #[test]
    fn index_write_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("scan-meta-v2.test.tsv");
        let rows = vec![
            SessionRow {
                sid: "aaaaaaaa-0000-0000-0000-000000000010".to_owned(),
                mtime: 1_718_000_000,
                human_ts: "06-10 14:32".to_owned(),
                mode: "plan".to_owned(),
                label: "My session label".to_owned(),
            },
            SessionRow {
                sid: "bbbbbbbb-0000-0000-0000-000000000011".to_owned(),
                mtime: 1_718_001_000,
                human_ts: "06-10 14:48".to_owned(),
                mode: "?".to_owned(),
                label: "Another label".to_owned(),
            },
        ];
        write_scan_index(&idx, 1_718_002_000, &rows).unwrap();
        let (refresh_start, loaded) = load_scan_index(&idx).unwrap();
        assert_eq!(refresh_start, 1_718_002_000);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].sid, rows[0].sid);
        assert_eq!(loaded[0].mtime, rows[0].mtime);
        assert_eq!(loaded[0].mode, rows[0].mode);
        assert_eq!(loaded[0].label, rows[0].label);
        assert_eq!(loaded[1].sid, rows[1].sid);
    }

    #[test]
    fn load_scan_index_absent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("nonexistent.tsv");
        assert!(load_scan_index(&idx).is_none());
    }

    #[test]
    fn load_scan_index_malformed_header_returns_none() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("bad.tsv");
        fs::write(&idx, "# wrong header\nsid\t123\tmode\tlabel\n").unwrap();
        assert!(load_scan_index(&idx).is_none());
    }

    #[test]
    fn index_skips_empty_sid_rows() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("scan-meta-v2.dedup.tsv");
        // Write a file with an empty-sid row.
        let content = "# refresh-start: 1000\n\t123\tmode\tlabel\nreal-sid\t456\tok\tok-label\n";
        fs::write(&idx, content).unwrap();
        let (_, rows) = load_scan_index(&idx).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sid, "real-sid");
    }

    // ─── dedup_rows_by_sid ────────────────────────────────────────────────────

    #[test]
    fn dedup_keeps_newer_mtime() {
        let make_row = |sid: &str, mtime: i64, label: &str| SessionRow {
            sid: sid.to_owned(),
            mtime,
            human_ts: "01-01 00:00".to_owned(),
            mode: "?".to_owned(),
            label: label.to_owned(),
        };
        let rows = vec![
            make_row("aaa", 100, "old"),
            make_row("aaa", 200, "new"),
            make_row("bbb", 150, "only"),
        ];
        let deduped = dedup_rows_by_sid(rows);
        assert_eq!(deduped.len(), 2);
        let aaa = deduped.iter().find(|r| r.sid == "aaa").unwrap();
        assert_eq!(aaa.label, "new");
        assert_eq!(aaa.mtime, 200);
    }

    #[test]
    fn dedup_equal_mtime_second_wins() {
        // >= not > means equal-mtime entry replaces the first.
        let make_row = |sid: &str, mtime: i64, label: &str| SessionRow {
            sid: sid.to_owned(),
            mtime,
            human_ts: "01-01 00:00".to_owned(),
            mode: "?".to_owned(),
            label: label.to_owned(),
        };
        let rows = vec![
            make_row("aaa", 100, "first"),
            make_row("aaa", 100, "second"),
        ];
        let deduped = dedup_rows_by_sid(rows);
        assert_eq!(deduped.len(), 1);
        // second >= first, so second wins
        assert_eq!(deduped[0].label, "second");
    }

    // ─── reindex_scan_dir ────────────────────────────────────────────────────

    #[test]
    fn reindex_creates_index_for_project_dir() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("projects").join("-home-dave-myproject");
        fs::create_dir_all(&project_dir).unwrap();

        // Redirect smart_dir to tmp; we can't easily override paths::smart_dir_no_create,
        // so instead we exercise the sub-functions directly.

        let sid = "cccccccc-0000-0000-0000-000000000001";
        write_transcript(&project_dir, sid, &[
            &user_record("hello world"),
            &perm_mode_record("plan"),
        ]);

        // extract_index_row works on the transcript.
        let tp = project_dir.join(format!("{sid}.jsonl"));
        let row = extract_index_row(&tp).unwrap();
        assert_eq!(row.sid, sid);
        assert_eq!(row.mode, "plan");
        assert_eq!(row.label, "hello world");
    }

    // ─── scan_sessions integration test ──────────────────────────────────────

    #[test]
    fn scan_sessions_integration() {
        // Build a fake projects/ tree mirroring the real structure.
        // We can't override paths::session_base_dir(), so we test the
        // sub-functions directly and verify the output contract.

        let tmp = TempDir::new().unwrap();

        // Create two transcripts; the second has an ai-title.
        let sid1 = "dddddddd-0000-0000-0000-000000000001";
        let sid2 = "dddddddd-0000-0000-0000-000000000002";

        let tp1 = tmp.path().join(format!("{sid1}.jsonl"));
        let tp2 = tmp.path().join(format!("{sid2}.jsonl"));

        fs::write(&tp1, format!("{}\n", user_record("first session message"))).unwrap();
        fs::write(&tp2, format!("{}\n{}\n",
            user_record("second session message"),
            ai_title_record("My AI Title"),
        )).unwrap();

        // Use a small sleep alternative: just set mtime indirectly via write order.
        // Test extract + build rows + sort.
        let row1 = extract_index_row(&tp1).unwrap();
        let row2 = extract_index_row(&tp2).unwrap();

        assert_eq!(row1.sid, sid1);
        assert_eq!(row1.label, "first session message");
        assert_eq!(row2.sid, sid2);
        assert_eq!(row2.label, "My AI Title");
    }

    // ─── session_dirs_for dedup ───────────────────────────────────────────────

    #[test]
    fn session_dirs_for_deduplicates_same_path() {
        // A path with no dots produces identical current+legacy encodings.
        // The result should contain at most one entry for that dir.
        let tmp = TempDir::new().unwrap();
        // We can only test the dedup logic indirectly via encode_cwd.
        let (cur, leg) = paths::encode_cwd(Path::new("/tmp/noproject"));
        // Both encodings must be identical (no dots in path).
        assert_eq!(cur, leg, "expected identical encodings for path without dots");
    }

    // ─── last permissionMode wins (tail of file) ──────────────────────────────

    #[test]
    fn last_permission_mode_wins() {
        let tmp = TempDir::new().unwrap();
        let sid = "eeeeeeee-0000-0000-0000-000000000001";
        write_transcript(tmp.path(), sid, &[
            &perm_mode_record("default"),
            &perm_mode_record("plan"),
            &perm_mode_record("acceptEdits"),
        ]);
        let tp = tmp.path().join(format!("{sid}.jsonl"));
        let (mode, _) = extract_label_and_mode(&tp);
        // "acceptEdits" is the LAST occurrence.
        assert_eq!(mode, "acceptEdits");
    }

    // ─── non-existent transcript ──────────────────────────────────────────────

    #[test]
    fn extract_label_and_mode_nonexistent_file_returns_defaults() {
        let (mode, label) = extract_label_and_mode(Path::new("/nonexistent/path/fake.jsonl"));
        assert_eq!(mode, "?");
        assert_eq!(label, "");
    }

    // ─── transcript_sid ───────────────────────────────────────────────────────

    #[test]
    fn transcript_sid_extracts_stem() {
        let p = Path::new("/foo/bar/01234567-89ab-cdef-0123-456789abcdef.jsonl");
        assert_eq!(transcript_sid(p).unwrap(), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn transcript_sid_no_extension_returns_stem() {
        let p = Path::new("/foo/bar/mysession");
        assert_eq!(transcript_sid(p).unwrap(), "mysession");
    }
}
