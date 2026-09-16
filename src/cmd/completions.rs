//! `csm completions {zsh|bash|pwsh}`.

use std::ffi::OsString;

use crate::cli;

/// `csm completions {zsh|bash|pwsh}`
pub(crate) fn cmd_completions(args: &[OsString]) -> anyhow::Result<()> {
    use clap_complete::Shell;

    let shell_str = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Accept `pwsh` as an alias for clap's `powershell` token — both csm's
    // `--help` and the shim contract speak of `pwsh`, so the completions verb
    // must too. (`clap_complete::Shell::from_str` only knows `powershell`.)
    let normalized = if shell_str.eq_ignore_ascii_case("pwsh") {
        "powershell".to_owned()
    } else {
        shell_str.clone()
    };

    let shell: Shell = normalized.parse().map_err(|_| {
        anyhow::anyhow!(
            "csm completions: unknown shell {shell_str:?} — use zsh, bash, pwsh, or powershell"
        )
    })?;

    cli::completions::generate(shell, &mut std::io::stdout());
    Ok(())
}
