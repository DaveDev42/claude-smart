//! CLI module: argument parsing and shell completion generation.
//!
//! - [`parser`] — hand-rolled positional flag loop for `csm run` (never clap).
//! - [`carry`] — the pure allow-list deciding which passthru flags a
//!   limit-switch relaunch replays.
//! - [`completions`] — clap-based shell completion generation for
//!   `csm completions {zsh|bash|pwsh}`.
//!
//! Callers import directly from the sub-modules:
//! ```ignore
//! use crate::cli::parser::{parse, ResumeArg};
//! use crate::cli::completions::generate;
//! ```

pub mod carry;
pub mod completions;
pub mod parser;
pub mod reserved;
