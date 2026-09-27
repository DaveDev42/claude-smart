//! Shared subcommand helpers used across `main.rs`'s `cmd_*` entry points:
//! session-id generation and the capped stdin reader used by `csm usage capture`. Launch classification (Orca
//! pane, print mode, `CSM_ORCA`) lives in [`crate::launch_context`].

use uuid::Uuid;

/// Generate a fresh lowercase UUID v4 for use as `--session-id`.
pub(crate) fn newuuid() -> String {
    Uuid::new_v4().to_string()
}

/// Hard cap on how much of `csm usage capture`'s stdin (a statusLine JSON
/// payload) we will ever read. StatusLine payloads are small (a few KB at
/// most); this is purely a defensive ceiling against a misconfigured or
/// hostile pipe feeding an unbounded stream — see [`read_stdin_capped`].
pub(crate) const CAPTURE_STDIN_CAP_BYTES: u64 = 256 * 1024;

/// Read stdin up to `max_bytes`, lossily decoding as UTF-8. Never blocks past
/// EOF-or-cap; a payload larger than the cap is silently truncated (the
/// caller — a JSON parse — will simply fail on truncated input, which is
/// treated as a no-op by every caller here).
pub(crate) fn read_stdin_capped(max_bytes: u64) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(max_bytes).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── newuuid ───────────────────────────────────────────────────────────────

    #[test]
    fn newuuid_produces_lowercase_uuid() {
        let id = newuuid();
        assert_eq!(id.len(), 36, "UUID must be 36 chars");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        assert_eq!(id, id.to_lowercase(), "UUID must be lowercase");
    }

    #[test]
    fn newuuid_unique_each_call() {
        let a = newuuid();
        let b = newuuid();
        assert_ne!(a, b, "consecutive UUIDs must differ");
    }
}
