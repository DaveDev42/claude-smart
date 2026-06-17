//! Shell completion generation.
//!
//! This module uses `clap` **only** for generating completions (`csm completions
//! {zsh|bash|pwsh}`). It does not touch `csm`'s own argv — that is handled by
//! the hand-rolled parser in `cli/parser.rs`.

use clap::CommandFactory;
use clap_complete::Shell;

/// Clap-derived struct used exclusively for `csm completions` — never for
/// parsing `csm run` arguments.
#[derive(clap::Parser)]
#[command(name = "csm", about = "Cross-platform Claude Code smart session manager")]
pub struct CsmCompletionsApp {
    #[command(subcommand)]
    pub command: CompletionsCommand,
}

#[derive(clap::Subcommand)]
pub enum CompletionsCommand {
    /// Emit shell completions for the given shell to stdout.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

/// Generate completions for `shell` and write them to `out`.
///
/// Phase 0 stub: wired to the real `clap_complete::generate` call signature
/// but the `CsmCompletionsApp` command tree is minimal (only `completions`
/// itself). Full subcommand tree population is deferred to Phase 1.
pub fn generate(shell: Shell, out: &mut impl std::io::Write) {
    let mut cmd = CsmCompletionsApp::command();
    clap_complete::generate(shell, &mut cmd, "csm", out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_zsh_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut buf);
        assert!(!buf.is_empty(), "zsh completions should not be empty");
    }

    #[test]
    fn generate_bash_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut buf);
        assert!(!buf.is_empty(), "bash completions should not be empty");
    }

    #[test]
    fn generate_powershell_completions_is_non_empty() {
        let mut buf = Vec::new();
        generate(Shell::PowerShell, &mut buf);
        assert!(!buf.is_empty(), "powershell completions should not be empty");
    }
}
