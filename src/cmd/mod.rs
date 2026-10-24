//! Subcommand helpers, split out of `main.rs`.
//!
//! Module declarations only — the handlers live in the submodules.
//!
//! `claude` is the odd one out: it is a passthrough, not a csm feature — see
//! [`claude`]'s module doc.

pub mod accounts;
pub mod claude;
pub mod completions;
pub mod config;
pub mod hook;
pub mod orca;
pub mod run;
pub mod support;
pub mod usage;
