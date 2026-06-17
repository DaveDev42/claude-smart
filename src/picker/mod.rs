//! Picker module — fzf-backed interactive selectors.
//!
//! Two public pickers:
//!
//! - [`SessionPicker`] — select/resume a past session or start fresh.
//!   Used by `csm run` when 2+ sessions exist for the cwd and fzf is available.
//!
//! - [`AccountPicker`] — select a Claude account (profile) when the hub usage
//!   fetch fails and the user is in an interactive context (the hub-down picker,
//!   spec §4a Decision #1).
//!
//! Both pickers use the shared [`fzf`] machinery: tab-delimited rows piped to
//! `fzf`, a hidden col1 recovery key, `--with-nth` to display the rest.

pub mod account;
pub mod fzf;
pub mod session;

pub use account::AccountPicker;
pub use session::SessionPicker;
