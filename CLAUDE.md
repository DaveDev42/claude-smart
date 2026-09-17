# CLAUDE.md — claude-smart (`csm`)

Working notes for Claude Code in this repo. User-facing docs live in `README.md`.
Design specs live in a private companion repo and are not checked into this
public crate. This file is the orientation map + the rules that must hold.

## What this is

`claude-smart` is a public Rust crate (`github.com/DaveDev42/claude-smart`,
BSD-3-Clause) producing a single cross-platform binary **`csm`** that wraps the
`claude` CLI with: smart session selection, a user-configurable profile registry
(`CLAUDE_CONFIG_DIR` switching), account scoring + auto-switch, multi-profile
usage metering, a limit-detection hook, and a relaunch/handoff loop. Runs on
macOS, Linux/WSL, and Windows-native from one binary (no shell impl to keep in
sync). It is the Rust port that replaces the legacy zsh+pwsh `claude-smart`.

It is consumed by a private companion deployment repo (the operator's
multi-machine fleet), but the crate itself ships **zero** private identifiers —
see *Invariants*.

## Layout

- `src/main.rs` — `GLOBAL` (the process-wide allocator), `main()`'s
  argv[0]/`args[1]` dispatch, and `print_help()`. Every `cmd_*` handler now
  lives under `src/cmd/`.
- `src/cmd/` — one module per subcommand: `run.rs`, `hook.rs`, `cas.rs`,
  `config.rs`, `profiles.rs`, `usage.rs`, `pick_account.rs`, `scan.rs`,
  `sidecar.rs`, `completions.rs`, plus `support.rs` (profile-dir / stdin / tty
  helpers shared across subcommands). Four reserved words dispatch outside
  `src/cmd/` instead: `reap` → `reaper::cmd` (`src/reaper/mod.rs`),
  `statusline` → `statusline::run` (`src/statusline.rs`), `current-usage` →
  `cmd::pick_account::cmd_current_usage`, and `newuuid` → inline in `main()`
  (see the match in `src/main.rs`).
- `src/cli/` — `parser.rs` (hand-rolled `csm run` flag loop, NOT clap, so
  claude flags forward verbatim; `Flags::help` is the one claude-shaped flag it
  intercepts, and only before any passthru token), `carry.rs` (pure
  `carry_passthru`: the arity-aware allow-list picking which remembered
  passthru flags a limit-switch hop replays on `claude --resume`, and which it
  drops), `completions.rs` (clap tree used ONLY for
  `csm completions`, never to parse real argv), `reserved.rs` (the reserved
  subcommand consts + `dispatch_subcommand` → `Dispatch {subcommand, rest_len,
  profile}`, which also peels the ONE csm-global flag allowed in front of a
  subcommand word — `--profile <name>` / `--profile=<name>` — read by dispatch,
  completions, and the disjointness test).
- `src/account/` — `profiles.rs` (`ProfileMap` = the registry authority),
  `scoring.rs` (pick-best thresholds: `LIMIT_PCT=99`, `SATURATION_PCT=95`;
  `is_viable_pcts` is the ONE viability predicate over session / week_all /
  week_fable — `pick_best_at`, `cmd::run::account_row_rank`, and the hook's
  target pick all route through it; never add a second inline threshold check;
  also the shared `effective_reset_epoch`), `reset.rs` (compat parser for
  `resets`-only payloads), `mod.rs` (`pick_account`, `pick_account_gated` —
  what the hook's target pick calls, see `src/hook/detect.rs` —
  `current_usage`).
- `src/cas/` — profile switcher: `types.rs`, `eval.rs` (`eval_emit` for the
  shell-shim machine interface), `manage.rs` (`manage_emit` for registry
  verbs), `edit.rs` (interactive editor: pure `apply_edit_action` + thin
  `run_interactive`), `platform.rs` (`apply_global`: launchctl/HKCU floor),
  `mod.rs` (re-exports + `default_state_file`).
- `src/usage/` — `model.rs` (`UsageData` serde), `transport.rs` (`fetch()` —
  positive TTL cache → `CSM_USAGE_CMD` → negative cooldown → `local::collect`),
  `report.rs` (`csm usage`: pure `build_report` + `render_table`/`render_json`),
  `local/` (the collector: `mod.rs` orchestrates per-profile fresh/stale/probe
  resolution, `creds.rs` reads each profile's own Claude Code OAuth credentials
  read-only, `api.rs` calls Anthropic's `/api/oauth/usage` and owns the one
  `http_client` builder, `refresh.rs` the opt-in OAuth token refresh (off by
  default), `store.rs` persists `<smart-dir>/usage/<profile>.json`,
  `statusline.rs` merges the statusLine stdin capture, `display.rs` formats
  reset times).
- `src/hook/` — `mod.rs` (`run` / `run_from_statusline`), `detect.rs`,
  `stop.rs`, `notify.rs`. Two live tiers remain: tier-0 `StopFailure` with
  `error: "rate_limit"` fires when a 429 ends the turn, and tier-2 usage-%
  catches caps crossed during a successful turn. A subscription cap fires NO
  hook at all (Claude Code parks the turn in an auto-retry wait), so
  `hook::run_from_statusline` runs the same classification off the statusLine
  tick (`csm usage capture` / `csm statusline`) — that is the operative switch
  path for the weekly and model-scoped caps. The former tier-1
  (transcript-text) and tier-3 (malformed-in-tail) checks have been deleted.
- `src/picker/` — `engine.rs`, `account.rs`, `session.rs`: in-process fuzzy
  picker (nucleo + crossterm).
- `src/reaper/` — `mod.rs`, `scan.rs`, `kill.rs`: the `csm reap` orphan killer.
- `src/config.rs` — csm's own `config.json` behind `csm config`.
- `src/envvar.rs`, `src/epoch.rs`, `src/testenv.rs` — one small helper each.
- `src/session/`, `src/sidecar/`, `src/platform/`, `src/statusline.rs`,
  `src/paths.rs` — session scan/index, sidecar store, OS launch/relaunch/proc
  checks, statusline, canonical state paths.
- `src/provision.rs` — profile provisioning SSOT: `ensure_profile_provisioned`
  (dir + `plugins/` → `~/.claude.shared/plugins` symlink + `projects/` →
  `~/.claude.shared/projects` symlink) and the read-only `diagnose_profile`
  core behind `csm profiles doctor`. Called implicitly on every
  launch/switch/register so csm maintains its own invariants; explicit via
  `bootstrap`/`doctor`. Unix-only symlink (non-unix = OS-side junction).
- `src/homeguard.rs` — the `~/.claude` compatibility shim: keeps
  `~/.claude/projects` → `~/.claude.shared/projects` so tools that hardcode the
  default home see every profile's transcripts. Launch-time create-only
  (`ensure_home_shim_soft`, opt-out `CSM_NO_HOME_SHIM`); `csm profiles doctor
  --fix-home` repairs (merges a real `projects` dir, repoints a wrong link);
  never touches credentials or settings in `~/.claude`.
- `tests/no_private_names.rs` — the leak guard: `src/**/*.rs` is scanned by all
  three rules (the `forbidden()` substring list, the `Dave-` host-prefix rule,
  and the quoted-profile-literal rule); `README.md`, `CLAUDE.md`, `Cargo.toml`,
  and `examples/*.sh` are scanned by the `forbidden()` substring rule only.
- `.github/workflows/ci.yml` — the push/PR gate: fmt, per-target check/clippy
  (4 targets: linux-gnu, aarch64-darwin, x86_64-darwin, windows-msvc) with
  native `cargo test` where the runner can execute the target, and an msrv
  (1.95) job.
- `.github/workflows/release-please.yml` — release automation (conventional
  commits → release PR → tag → build matrix + Homebrew bump; see Releases).
- `e2e/` — the limit-switch end-to-end harness: `run.sh` builds `csm` and a
  fake, sleeping `claude` (`fake-claude/claude.c`) and drives both through an
  isolated sandbox HOME with no network access; `lib.sh` + `scenarios.sh` hold
  the 10 scenarios. Excluded from the packaged crate (see `Cargo.toml`
  `exclude`). See *Testing* in `README.md` and `e2e/README.md`.

## Commands

Plain cargo (rustup default is `stable`; works out of the box):

```sh
cargo build --bin csm
cargo test                 # unit + the no_private_names leak guard
cargo clippy --all-targets
cargo run --bin csm -- <args>
bash e2e/run.sh             # limit-switch end-to-end, fake claude, isolated HOME
```

Run `/verify` before every commit (test + clippy + fmt + leak guard + a
windows-gnu clippy cross-check, in one pass; `rustup target add` is
allow-listed for the windows-gnu step, and `cargo info` is allow-listed for
dependency/MSRV checks).

> Fallback only if a sandbox/PATH issue makes `cargo` resolve wrong: pin the
> toolchain explicitly —
> `TC=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin; PATH="$TC:$PATH" RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin "$TC/cargo" …`
> (and `dangerouslyDisableSandbox: true` on the Bash call). Prefer plain `cargo`.

## Invariants (a violation is a regression — fix, don't ship)

1. **Public crate ships ZERO private identifiers.** No operator tailnet suffix,
   hub/host names, account-profile names, real home paths, or personal email —
   anywhere under `src/`, **including `#[cfg(test)]` fixtures**. Examples use
   neutral placeholders (`work`, `home`, `/Users/example`, `Acme-…`). Usage data
   comes from Anthropic's own OAuth usage API. There is no hub. Two endpoints
   are compiled in: the usage API at `https://api.anthropic.com` (overridable
   via `CSM_USAGE_API_BASE` for tests) and, only when the opt-in OAuth refresh
   (`CSM_OAUTH_REFRESH`) is on, the token endpoint at
   `https://platform.claude.com/v1/oauth/token` (overridable via
   `CSM_OAUTH_TOKEN_URL`); any host-naming convention is injected via
   `CSM_HOST_REPLACE`, never compiled in. Profile names come from `ProfileMap`
   (the registry), never literals. `cargo test` runs `tests/no_private_names.rs`,
   which scans every line of `src/` and fails on any leak (its forbidden list is
   assembled from fragments so the guard file itself stays clean).
2. **No collision with `claude`'s CLI.** `csm` treats a word as its own
   subcommand ONLY at `args[1]` (or at the token right after a csm-global
   `--profile <name>`, the one flag `dispatch_subcommand` peels), and the
   reserved set is disjoint from claude's
   (`agents/auth/auto-mode/doctor/install/mcp/plugin(s)/project/setup-token/
   ultrareview/update`). Any other first token → implicit `csm run` → forwarded
   verbatim to `claude`. Adding a subcommand whose name collides with a claude
   subcommand is forbidden. `csm run` consuming a NEW claude flag before `--` is
   forbidden (the `--` boundary forwards the rest untouched). The single
   documented exception is `-h`/`--help` before any passthru token, which prints
   run's own usage; `csm run -- --help` still reaches claude.
3. **`ProfileMap` is the single registry authority.** Validity/default/dir
   resolution all go through it. No second source of profile truth, no hardcoded
   allowlist.
4. **Pure core + thin I/O shell** for testable features (`report.rs`,
   `cas/edit.rs`): the join/decision logic is a pure fn unit-tested against
   fixtures; network/stdin/stdout/clock live in `main`/the I/O shell only.
5. **`cas` eval-class is a machine interface** (`csm cas --eval --shell … `,
   `csm cas --print-default-dir`). Don't rename it — external shell shims
   depend on the exact contract. Human-facing verbs live under `csm
   profiles …` (which reuses the same handlers).

## CLI surface (collision-safe)

`run, hook, profiles {list|add|set|rm|use|edit|dir|bootstrap|doctor},
config {show|get|set|unset launch-command},
usage [--json] [--no-fetch] [--refresh] [--refresh-oauth] | usage capture,
pick-account, scan, sidecar, statusline, completions, reap, newuuid` + machine
interface `cas` (+ back-compat `cas <verb>` aliases, `current-usage`). Any of
them may be preceded by the csm-global `csm --profile <name> …`, which pins
`CLAUDE_CONFIG_DIR` for the subcommand (`run` gets the flag re-injected instead,
so `cli::parser` stays the one place a launch resolves its pin). The
collision analysis against claude's own subcommands is Invariant 2 above.

## Git workflow

**Commit directly to `main`** with Conventional Commits (`feat:`/`fix:`/`docs:`/
`refactor:`/`chore:`/`ci:`/`test:` …). `main` must stay green — run `/verify`
first. release-please watches `main` and maintains a release PR automatically.
Branches/PRs are optional and usually unnecessary for this single-owner repo.
**Push only when the user asks.** Committing locally without pushing is fine.

Never merge-commit into main; rebase. release-please walks history by date
and stops at the previous release commit, so commits behind a merge commit
fall out of the changelog (this happened on 2026-09-17 for 0.3.3).

## Releases

Cutting a release is mostly automatic — see `/release` for the procedure.
TL;DR: conventional commits on `main` → release-please opens/updates a release PR
that bumps `Cargo.toml` + `Cargo.lock` + CHANGELOG → merging that PR tags
`vX.Y.Z`, runs the 4-target build matrix, attaches assets + `SHA256SUMS.txt`,
publishes the GitHub release, and bumps the Homebrew tap formula. **crates.io
publish is in CI**: the `publish-crate` job authenticates with Trusted
Publishing (OIDC, no static token) after the build matrix succeeds on a
release, and skips when the version is already on the index — nothing manual
is needed.

## Known gaps

- **Windows console-stop is unverified.** Two empirical checks gate the Windows
  relaunch loop and neither runs headlessly: (1) interactive Ctrl-C cancels
  claude's prompt rather than killing the supervisor; (2) after a limit-triggered
  switch the session `.jsonl` is complete, not truncated. Until both pass on a
  real Windows console the relaunch loop stays gated off and Windows falls back
  to launch-without-relaunch. See `src/platform/windows.rs`'s module doc.
- **`csm statusline` cold-start latency is unmeasured.** The *capture* side is
  live and operative — the statusline tick is the switch path for subscription
  caps (`hook::run_from_statusline`). The *render* side (`<profile>@<host>`) is
  not yet recommended as the default `statusLine` command: it sits on the prompt
  hot path and has never been benchmarked against a shell statusline.
- **Windows `is_interactive()`** uses an env-var heuristic, not `GetConsoleMode`.

## Don't touch / out of scope

- There is no hub-side data source anymore. `csm` collects usage locally per
  profile (`src/usage/local/`) from Anthropic's own OAuth usage API. The retired
  hub scrape is out of scope here; `csm` no longer consumes
  `/cc-usage/api/data/limits`.
- The private companion repo's deployment glue (Brewfile/winget/Ansible shims)
  lives there, not here. Changes that span both repos: do the crate side here,
  note the companion edits for the other repo separately.
