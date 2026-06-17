# claude-smart

Cross-platform Claude Code smart session manager.

`csm` is a single binary that absorbs the entire `claude-smart` system:
argument parsing, session scan/index, the fzf picker, account scoring, usage
transport and caching, the limit-detection hook, the foreground launch
supervisor, and the relaunch/handoff loop. It runs on macOS, WSL Ubuntu, and
Windows-native, replacing the prior split zsh/pwsh implementation with full
4-machine parity.

Private repo.
