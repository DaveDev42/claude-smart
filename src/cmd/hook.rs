//! `csm hook [--owner <profile_dir>]` — the Stop/StopFailure/SubagentStop/
//! SessionEnd hook entry point.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::{account, hook};

/// `csm hook [--owner <profile_dir>]`
///
/// Parses `--owner <dir>` and calls `hook::run`.  Defaults to `$CLAUDE_CONFIG_DIR`
/// when `--owner` is absent (non-interactive / missing shim).
pub(crate) fn cmd_hook(args: &[OsString]) -> anyhow::Result<()> {
    let owner_dir: PathBuf = parse_owner_flag(args)
        .or_else(|| {
            std::env::var("CLAUDE_CONFIG_DIR")
                .ok()
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            // Last resort (no --owner, no $CLAUDE_CONFIG_DIR): the registry default.
            account::ProfileMap::load()
                .unwrap_or_default()
                .default_dir()
        });

    hook::run(&owner_dir)
}

/// Parse `--owner <value>` or `--owner=<value>` from an arg slice.
fn parse_owner_flag(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();
        if s == "--owner" {
            if let Some(next) = iter.next() {
                return Some(PathBuf::from(next));
            }
        } else if let Some(val) = s.strip_prefix("--owner=") {
            return Some(PathBuf::from(val));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn parse_owner_flag_space_form() {
        let args = argv(&["--owner", "/Users/example/.claude.home"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.home")));
    }

    #[test]
    fn parse_owner_flag_equals_form() {
        let args = argv(&["--owner=/Users/example/.claude.home"]);
        let result = parse_owner_flag(&args);
        assert_eq!(result, Some(PathBuf::from("/Users/example/.claude.home")));
    }

    #[test]
    fn parse_owner_flag_absent_returns_none() {
        let args = argv(&["--other", "value"]);
        assert!(parse_owner_flag(&args).is_none());
    }

    #[test]
    fn parse_owner_flag_empty_slice() {
        assert!(parse_owner_flag(&[]).is_none());
    }
}
