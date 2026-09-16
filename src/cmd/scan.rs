//! `csm scan <cwd>`.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::session;

/// `csm scan <cwd>`
///
/// Print TSV rows (newest-first) to stdout.
pub(crate) fn cmd_scan(args: &[OsString]) -> anyhow::Result<()> {
    let cwd = match args.first() {
        Some(a) => PathBuf::from(a),
        None => std::env::current_dir().context("csm scan: cannot determine cwd")?,
    };
    for row in session::scan(&cwd) {
        println!("{}", row.to_tsv());
    }
    Ok(())
}
