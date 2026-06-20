# CLAUDE.md — claude-smart (`csm`)

Working notes for Claude Code in this repo. User-facing docs live in `README.md`;
design specs in `docs/`. Keep design detail in the specs, not inlined here — this
file is the orientation map + the rules that must hold.

## What this is

`claude-smart` is a public Rust crate (`github.com/DaveDev42/claude-smart`,
BSD-3-Clause) producing a single cross-platform binary **`csm`** that wraps the
`claude` CLI with: smart session selection, a user-configurable profile registry
(`CLAUDE_CONFIG_DIR` switching), account scoring + auto-switch, multi-profile
usage metering, a limit-detection hook, and a relaunch/handoff loop. Runs on
macOS, Linux/WSL, and Windows-native from one binary (no shell impl to keep in
sync). It is the Rust port that replaces the legacy zsh+pwsh `claude-smart`.

It is consumed by the **private** `dave-environment` Ansible repo (the operator's
4-machine fleet), but the crate itself ships **zero** private identifiers — see
*Invariants*.

## Layout

- `src/main.rs` — entry + argv[0]/subcommand dispatch (the collision-avoidance
  core). Each `cmd_*` fn handles one subcommand.
- `src/cli/` — `parser.rs` (hand-rolled `csm run` flag loop, NOT clap, so claude
  flags forward verbatim), `completions.rs` (clap tree used ONLY for
  `csm completions`, never to parse real argv).
- `src/account/` — `profiles.rs` (`ProfileMap` = the registry authority),
  `scoring.rs` (pick-best thresholds: `LIMIT_PCT=99`, `SATURATION_PCT=95`),
  `reset.rs`, `mod.rs` (`pick_account`, `current_usage`).
- `src/cas/` — profile switcher: `mod.rs` (`Op` enum, `eval_emit` for the shell
  shim contract, `manage_emit` for registry verbs), `edit.rs` (interactive
  editor: pure `apply_edit_action` + thin `run_interactive`), `platform.rs`
  (`apply_global`: launchctl/HKCU floor).
- `src/usage/` — `model.rs` (`UsageData` serde), `transport.rs` (`fetch()` —
  5-layer resilience ladder: hub-local → positive TTL cache → negative cooldown
  → HTTP → SSH[unix]), `report.rs` (`csm usage`: pure `build_report` +
  `render_table`/`render_json`).
- `src/session/`, `src/picker/`, `src/hook/`, `src/sidecar/`, `src/platform/`,
  `src/statusline.rs`, `src/paths.rs` — session scan/index, fzf picker, the
  Stop/SubagentStop/SessionEnd hook, sidecar store, OS launch/relaunch/proc
  checks, statusline, canonical state paths.
- `tests/no_private_names.rs` — CI leak guard (recursively greps `src/`).
- `docs/` — design specs (dated). `.github/workflows/release-please.yml` — CI.

## Commands

Plain cargo (rustup default is `stable`; works out of the box):

```sh
cargo build --bin csm
cargo test                 # unit + the no_private_names leak guard
cargo clippy --all-targets
cargo run --bin csm -- <args>
```

Run `/verify` before every commit (test + clippy + leak guard, in one pass).

> Fallback only if a sandbox/PATH issue makes `cargo` resolve wrong: pin the
> toolchain explicitly —
> `TC=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin; PATH="$TC:$PATH" RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin "$TC/cargo" …`
> (and `dangerouslyDisableSandbox: true` on the Bash call). Prefer plain `cargo`.

## Invariants (a violation is a regression — fix, don't ship)

1. **Public crate ships ZERO private identifiers.** No tailnet suffix
   (`example-tnet.ts.net`), no hub hostname (`workstation`), no account names
   (`personal`/`work`), no personal email — in **production** (`#[cfg(test)]`
   fixtures are exempt and skipped by the guard). The hub is reached only via the
   env contract `CLAUDE_USAGE_URL` + `CLAUDE_HUB_HOSTNAME` (both empty/unset =
   disabled). Profile names come from `ProfileMap` (the registry), never literals.
   `cargo test` runs `tests/no_private_names.rs` which enforces this; `docs/` is
   excluded from the crate tarball (`Cargo.toml` `exclude`).
2. **No collision with `claude`'s CLI.** `csm` treats a word as its own
   subcommand ONLY at `args[1]`, and the reserved set is disjoint from claude's
   (`agents/auth/auto-mode/doctor/install/mcp/plugin(s)/project/setup-token/
   ultrareview/update`). Any other first token → implicit `csm run` → forwarded
   verbatim to `claude`. Adding a subcommand whose name collides with a claude
   subcommand is forbidden. `csm run` consuming a NEW claude flag before `--` is
   forbidden (the `--` boundary forwards the rest untouched).
3. **`ProfileMap` is the single registry authority.** Validity/default/dir
   resolution all go through it. No second source of profile truth, no hardcoded
   allowlist.
4. **Pure core + thin I/O shell** for testable features (`report.rs`,
   `cas/edit.rs`): the join/decision logic is a pure fn unit-tested against
   fixtures; network/stdin/stdout/clock live in `main`/the I/O shell only.
5. **`cas` eval-class is a machine interface** (`csm cas --eval --shell … `,
   `csm cas --print-default-dir`). Don't rename it — the dave-environment shell
   shims depend on the exact contract. Human-facing verbs live under `csm
   profiles …` (which reuses the same handlers).

## CLI surface (collision-safe)

`run, hook, profiles {list|add|set|rm|use|edit|dir}, usage [--json|--no-fetch],
pick-account, scan, sidecar, statusline, completions, newuuid` + machine
interface `cas` (+ back-compat `cas <verb>` aliases, `current-usage`). Full
design + collision analysis: `docs/2026-06-19-csm-cli-surface-design.md`.

## Git workflow

**Commit directly to `main`** with Conventional Commits (`feat:`/`fix:`/`docs:`/
`refactor:`/`chore:`/`ci:`/`test:` …). `main` must stay green — run `/verify`
first. release-please watches `main` and maintains a release PR automatically.
Branches/PRs are optional and usually unnecessary for this single-owner repo.
**Push only when the user asks.** Committing locally without pushing is fine.

## Releases

Cutting a release is mostly automatic — see `/release` for the procedure.
TL;DR: conventional commits on `main` → release-please opens/updates a release PR
that bumps `Cargo.toml` + `Cargo.lock` + CHANGELOG → merging that PR tags
`vX.Y.Z`, runs the 4-target build matrix, attaches assets + `SHA256SUMS.txt`,
publishes the GitHub release, and bumps the Homebrew tap formula. **crates.io
publish is NOT in CI** — the first publish is a local `cargo publish` (interactive
crates.io login), then Trusted Publishing is registered and an OIDC job re-added.

## Don't touch / out of scope

- The **hub-side data source** is NOT in this repo — it lives in `dave-environment`
  (`shared/claude-code-usage/`). `csm` only *consumes* `/cc-usage/api/data/limits`.
- dave-environment deployment glue (Brewfile/winget/Ansible shims) lives there,
  not here. Changes that span both repos: do the crate side here, note the
  companion edits for dave-environment separately.
