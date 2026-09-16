//! Unix epoch helpers, shared by every module that needs "now" as seconds
//! since `UNIX_EPOCH` or a file's mtime converted the same way. Before this
//! module, four call sites each hand-rolled the same
//! `duration_since(UNIX_EPOCH)` dance with slightly different fallback and
//! cast conventions; this collapses them onto two functions.
//!
//! Two different operations live here — do not fuse them:
//! - [`now_secs`]: the current wall-clock time.
//! - [`from_systemtime`]: an already-obtained [`SystemTime`] (typically a
//!   file's mtime) converted to the same epoch-seconds representation.
//!
//! `hook::detect::now_epoch` keeps its own `#[cfg(test)]` twin with a
//! thread-local time override — that injection point is intentionally not
//! folded in here, since only the non-test body needs the real clock.

use std::time::SystemTime;

/// Current Unix epoch in seconds. Falls back to `0` if the system clock is
/// somehow set before `UNIX_EPOCH`.
pub(crate) fn now_secs() -> u64 {
    from_systemtime(SystemTime::now())
}

/// Convert a [`SystemTime`] (e.g. a file's mtime) to Unix epoch seconds.
/// Falls back to `0` if `t` is before `UNIX_EPOCH`.
pub(crate) fn from_systemtime(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_secs_is_reasonable() {
        let now = now_secs();
        // 2023-01-01T00:00:00Z, as a sanity floor against a broken clock.
        assert!(
            now > 1_672_531_200,
            "now_secs should return a sane epoch, got {now}"
        );
    }

    #[test]
    fn from_systemtime_epoch_is_zero() {
        assert_eq!(from_systemtime(SystemTime::UNIX_EPOCH), 0);
    }

    #[test]
    fn from_systemtime_before_epoch_saturates_to_zero() {
        let before = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(10);
        assert_eq!(from_systemtime(before), 0);
    }

    #[test]
    fn from_systemtime_roundtrips_now() {
        let t = SystemTime::now();
        let expected = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        assert_eq!(from_systemtime(t), expected);
    }
}
