//! Shared process-global env-var locks for tests, across module boundaries.
//!
//! `std::env::set_var`/`remove_var` mutate the whole process's environment,
//! and `cargo test` runs every module's `#[cfg(test)]` tests in parallel
//! threads of that one process. A module-local `static ENV_LOCK: Mutex<()>`
//! (the pattern used throughout this crate — `statusline.rs`,
//! `usage/transport.rs`, `hook/detect.rs`) only serializes tests *within*
//! that module; it does nothing to protect against a DIFFERENT module's test
//! mutating the same variable concurrently.
//!
//! `CLAUDE_CONFIG_DIR` is mutated by tests in two modules —
//! `crate::statusline` and `crate::usage::local` (`record_statusline_payload`'s
//! own tests) — so both must serialize through the *same* lock, not two
//! independent ones. Without this, a real interleaving is possible: one
//! module's test sets `CLAUDE_CONFIG_DIR` to some path between the other
//! module's `remove_var` and its call into config-dir-reading code, and the
//! latter test silently resolves a directory neither test intended — in the
//! `record_statusline_payload` case, this could land a write in the
//! developer's real `~/.claude.shared/smart/usage/<profile>.json`.
//!
//! Every test (in any module) that sets/removes `CLAUDE_CONFIG_DIR` must hold
//! this lock's guard for the full set→act→restore sequence.

#[cfg(test)]
pub(crate) static CLAUDE_CONFIG_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
