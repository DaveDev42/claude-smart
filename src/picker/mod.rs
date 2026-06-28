//! Picker module — interactive selectors backed by an in-process fuzzy picker.
//!
//! Two public pickers:
//!
//! - [`SessionPicker`] — select/resume a past session or start fresh.
//!   Used by `csm run` when 2+ sessions exist for the cwd.
//!
//! - [`AccountPicker`] — select a Claude account (profile) when the hub usage
//!   fetch fails and the user is in an interactive context (the hub-down picker,
//!   spec §4a Decision #1).
//!
//! Both pickers use the shared [`engine`] machinery: delimiter-separated rows
//! with a hidden col1 recovery key, `display_from` to show/match the rest, and a
//! nucleo + crossterm picker rendered on the controlling terminal. The picker
//! returns an [`engine::PickerOutcome`] so Escape (Cancelled) stays distinct from
//! a degrade (Unavailable).

pub mod account;
pub mod engine;
pub mod session;

pub use account::AccountPicker;
// SessionPicker is imported directly via `picker::session::SessionPicker`.
