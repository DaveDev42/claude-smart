//! PID file helpers: `<sid>.pid` — `"<pid> <born>\n"`.
//!
//! Format is preserved for cutover read-compat with the legacy zsh
//! `write_pid` function (spec §6 / §2 Foreground launch):
//!
//! ```text
//! <pid_decimal> <born_epoch_decimal>
//! ```
//!
//! One space separator; optional trailing newline (the reader uses
//! `split_whitespace` so both forms parse correctly).
//!
//! Parse failure == "no live managed session" — the caller should treat
//! a `None` return from `read_pid_file` exactly like a missing file.

use std::io;
use std::path::Path;

/// Write `"<pid> <born>\n"` atomically via a temp file + rename.
///
/// The temp file is placed in the same directory as `path` to guarantee
/// rename is on the same filesystem.
pub fn write_pid_file(path: &Path, pid: u32, born: i64) -> io::Result<()> {
    let tmp = path.with_extension("pid.tmp");
    std::fs::write(&tmp, format!("{pid} {born}\n"))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read `"<pid> <born>"` from `path`.
///
/// Returns `Some((pid, born))` on success, `None` on:
/// - File absent (`NotFound`)
/// - Any parse error (treated as "no live managed session" per spec §6)
/// - Malformed content
pub fn read_pid_file(path: &Path) -> io::Result<Option<(u32, i64)>> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut parts = content.split_whitespace();
    let pid: u32 = match parts.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(None),
    };
    let born: i64 = match parts.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(None),
    };
    Ok(Some((pid, born)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_raw(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn roundtrip_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pid_file(&path, 12345, 1_718_000_000).unwrap();
        let (pid, born) = read_pid_file(&path).unwrap().expect("should be Some");
        assert_eq!(pid, 12345_u32);
        assert_eq!(born, 1_718_000_000_i64);
    }

    #[test]
    fn absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.pid");
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_with_trailing_newline() {
        let (_dir, path) = write_raw("99 1700000000\n");
        let (pid, born) = read_pid_file(&path).unwrap().unwrap();
        assert_eq!(pid, 99);
        assert_eq!(born, 1_700_000_000_i64);
    }

    #[test]
    fn parse_without_trailing_newline() {
        // The legacy zsh `write_pid` did not always add a newline.
        let (_dir, path) = write_raw("42 1000000001");
        let (pid, born) = read_pid_file(&path).unwrap().unwrap();
        assert_eq!(pid, 42);
        assert_eq!(born, 1_000_000_001_i64);
    }

    #[test]
    fn parse_failure_returns_none() {
        let (_dir, path) = write_raw("not_a_number 0");
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none(), "bad PID should yield None");
    }

    #[test]
    fn parse_missing_born_returns_none() {
        // Only one token — no born field.
        let (_dir, path) = write_raw("12345");
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none(), "missing born should yield None");
    }

    #[test]
    fn parse_empty_file_returns_none() {
        let (_dir, path) = write_raw("");
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn atomic_write_replaces_old_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        // Write first, then overwrite.
        write_pid_file(&path, 100, 1_000).unwrap();
        write_pid_file(&path, 200, 2_000).unwrap();
        let (pid, born) = read_pid_file(&path).unwrap().unwrap();
        assert_eq!(pid, 200);
        assert_eq!(born, 2_000);
    }
}
