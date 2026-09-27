//! Picker module — the interactive session selector backed by an in-process
//! fuzzy picker.
//!
//! - [`session::SessionPicker`] — select/resume a past session or start
//!   fresh. `csm run` opens it for `-i` or a bare `-r`/`--resume`.
//!
//! (The account picker left with the profile registry: Orca owns the account
//! list, and `csm accounts use` switches it.)
//!
//! The picker uses the shared [`engine`] machinery: delimiter-separated rows
//! with a hidden col1 recovery key, `display_from` to show/match the rest, and a
//! nucleo + crossterm picker rendered on the controlling terminal. The picker
//! returns an [`engine::PickerOutcome`] so Escape (Cancelled) stays distinct from
//! a degrade (Unavailable).

pub mod engine;
pub mod session;
