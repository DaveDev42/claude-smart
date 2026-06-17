//! CLI module: argument parsing and shell completion generation.
//!
//! - [`parser`] — hand-rolled positional flag loop for `csm run` (never clap).
//! - [`completions`] — clap-based shell completion generation for
//!   `csm completions {zsh|bash|pwsh}`.
//!
//! Re-exports for callers:
//! ```ignore
//! use crate::cli::{ParsedArgs, Flags};
//! use crate::cli::completions::generate;
//! ```

pub mod completions;
pub mod parser;

// Re-export the primary types so callers can write `crate::cli::ParsedArgs`
// without knowing the sub-module layout.
pub use parser::{Flags, ParsedArgs};
