# CLAUDE.md: claude-smart (`csm`)

Working notes for Claude Code in this repo. User-facing docs live in `README.md`.
Design specs live in a private companion repo and are not checked into this
public crate. This file is the orientation map + the rules that must hold.

## What this is

`claude-smart` is a public Rust crate (`github.com/DaveDev42/claude-smart`,
BSD-3-Clause) producing a single cross-platform binary **`csm`** that wraps the
`claude` CLI with: session selection, account switching over Orca's Claude
account list (csm keeps no registry of its own), account scoring and the
limit auto-switch, multi-account usage metering, a limit-detection hook, and
a relaunch/handoff loop. Runs on
macOS, Linux/WSL, and Windows-native from one binary (no shell impl to keep in
sync). It is the Rust port that replaces the legacy zsh+pwsh `claude-smart`.

It is consumed by a private companion deployment repo (the operator's
multi-machine fleet), but the crate itself ships **zero** private identifiers
(see *Invariants*).

## Layout

The module tree is discoverable from `src/`; this lists only what is not obvious from it.

- `src/main.rs`: `e2e::guard()` is the first call in `main()`, then argv[0]/`args[1]` dispatch. Every `cmd_*` handler lives under `src/cmd/`. Two reserved words dispatch outside it: `reap` (`src/reaper/`) and `statusline` (`src/statusline.rs`).
- `src/e2e.rs`: seams for `e2e/run.sh`, compiled in only with the `e2e` cargo feature (inert otherwise). `guard()` exits 97 unless `HOME` (and any `XDG_*_HOME`) lies inside `CSM_E2E_SANDBOX`. The feature also fakes the Keychain runner, Orca process scan, Orca version, and named `point(name)` hooks. Never enable the feature in a release build.
- `src/cmd/`: one module per subcommand. `claude.rs` is the `csm claude <args…>` passthrough. `accounts.rs` keeps a pure parser/decision core over `orca::`.
- `src/launch_context.rs`: how a launch started (`Print`, `OrcaPane`, `OrcaStructured`, `Interactive`, `CSM_ORCA` override), the `CLAUDE_CONFIG_DIR` pin rule (while Orca runs every launch uses Orca's live `D`), and the managed-account auth-env strip list.
- `src/cli/`: `parser.rs` is a hand-rolled `csm run` flag loop, NOT clap, so claude flags forward verbatim (it stops at the first positional, using `carry::arity`). `completions.rs` holds a clap tree used ONLY for `csm completions`, never to parse real argv. `reserved.rs` owns the reserved subcommand consts and `dispatch_subcommand`.
- `src/account/`: `accounts.rs` reads Orca's host accounts read-only. `load` (store) is the only load the hook and statusline use; `load_live` asks Orca over RPC (3 s timeout) because its store lags its memory. `scoring.rs` has `LIMIT_PCT=99`, `SATURATION_PCT=95` and `is_viable_pcts`, the one viability predicate over session and week_all. `pick_best_at` and the hook's target pick route through it; never add a second inline threshold check. `week_fable` does not feed viability.
- `src/orca/`: csm as a second client of Orca's account service. `HostEnv::current()` refuses the real home under `cfg(test)`; tests use `HostEnv::for_test`. `store.rs` holds the store-write protocol (L0/L1/L2 liveness checks, `patch_settings` round-trip gate, `preflight`); for a SQLite profile it delegates to `statedb.rs`, the port of Orca's offline settings writer (`profile-state.db` `settings` row, revision fence, acceptance marker). `keychain.rs` refuses the real `/usr/bin/security` under `cfg(test)`. `switch.rs` is a pure `plan_switch` plus executor and journal.
- `src/usage/`: `transport.rs` `fetch()` order is positive TTL cache, `CSM_USAGE_CMD`, negative cooldown, `local::collect`. `local/api.rs` owns the one `http_client` builder.
- `src/hook/`: two live tiers. Tier-0 `StopFailure` with `error: "rate_limit"` fires when a 429 ends the turn; tier-2 usage-% catches caps crossed during a successful turn. A subscription cap fires no hook (Claude Code parks the turn in an auto-retry wait), so `hook::run_from_statusline` runs the same classification off the statusLine tick; that is the operative path for weekly and model-scoped caps. A `week_fable`-only cap does not switch accounts: `detect::classify_with` relaunches the same session with a `--model` override (`fable_fallback_model`, gated by `CLAUDE_FABLE_FALLBACK` and a one-shot `<sid>.model-fallback` marker). `hook/hops.rs` appends one JSON line per switch (from `limit_switch::run_hop`, where the target is final) and per model fallback (from `stop::commit_and_stop`) to `<state>/hops.jsonl`, best effort, rotated at 1 MiB; Orca ids only.
- `src/platform/child.rs` holds every bounded run-with-timeout helper. State dir is `$XDG_STATE_HOME/csm` or `~/.local/state/csm` (`%LOCALAPPDATA%\csm` on Windows).
- `tests/no_private_names.rs`: the leak guard. `src/**/*.rs` is scanned by all three rules (the `forbidden()` substring list, the `Dave-` host-prefix rule, the quoted-profile-literal rule); `README.md`, `CLAUDE.md`, `Cargo.toml`, `examples/*.sh` by the `forbidden()` rule only.
- `.github/workflows/ci.yml`: fmt, per-target check/clippy (linux-gnu, aarch64-darwin, x86_64-darwin, windows-msvc), native `cargo test` where the runner can run the target, and an msrv (1.95) job. `release-please.yml` drives releases (see Releases).
- `e2e/`: end-to-end harness (`run.sh`, `scenarios.sh`, `lib.sh`, fakes for Orca, Keychain and OAuth run via `/usr/bin/perl` or `/bin/sh`). Excluded from the packaged crate. See `e2e/README.md`.
- `tools/orca-drift.sh`: diffs the Orca files csm mirrors between two Orca releases. Excluded from the packaged crate.

## Commands

Plain cargo (rustup default is `stable`; works out of the box):

```sh
cargo build --bin csm
cargo test                 # unit + the no_private_names leak guard
cargo clippy --all-targets
cargo run --bin csm -- <args>
cargo clippy --all-targets --features e2e
bash e2e/run.sh             # end-to-end: fake Orca/Keychain/claude, sandbox HOME
bash tools/orca-drift.sh v1.4.214 <new-tag>   # when Orca releases
```

When Orca ships a new release, run `tools/orca-drift.sh` from the last
verified tag (README *Verified Orca versions*) to the new one and read the
diff for changes to the store format, stash layout, Keychain encoding,
`accounts.*` RPC methods, `orca-runtime.json` or the liveness signals. Port
what changed, run the e2e harness, and only then add the new `major.minor`
to `orca::version::TESTED` and update the README's verified range.

Run `/verify` before every commit (test + clippy + fmt + leak guard + a
windows-gnu clippy cross-check, in one pass; `rustup target add` is
allow-listed for the windows-gnu step, and `cargo info` is allow-listed for
dependency/MSRV checks).

> Fallback only if a sandbox/PATH issue makes `cargo` resolve wrong: pin the
> toolchain explicitly:
> `TC=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin; PATH="$TC:$PATH" RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin "$TC/cargo" …`
> (and `dangerouslyDisableSandbox: true` on the Bash call). Prefer plain `cargo`.

## Invariants (a violation is a regression: fix, don't ship)

1. **Public crate ships ZERO private identifiers.** No operator tailnet suffix,
   hub/host names, account-profile names, real home paths, or personal email,
   anywhere under `src/`, **including `#[cfg(test)]` fixtures**. Examples use
   neutral placeholders (`work`, `home`, `/Users/example`, `Acme-…`). Usage data
   comes from Anthropic's own OAuth usage API. There is no hub. Two endpoints
   are compiled in: the usage API at `https://api.anthropic.com` (overridable
   via `CSM_USAGE_API_BASE` for tests) and the token endpoint at
   `https://platform.claude.com/v1/oauth/token` (overridable via
   `CSM_OAUTH_TOKEN_URL`), used only by the offline stash refresh
   (`csm usage --refresh` and a limit-switch pick while Orca is stopped;
   any host-naming convention is injected via
   `CSM_HOST_REPLACE`, never compiled in. Account ids come from Orca's account
   list, never literals. `cargo test` runs `tests/no_private_names.rs`,
   which scans every line of `src/` and fails on any leak (its forbidden list is
   assembled from fragments so the guard file itself stays clean).
2. **No collision with `claude`'s CLI.** `csm` treats a word as its own
   subcommand ONLY at `args[1]`, and the reserved set is disjoint from
   claude's: `CLAUDE_RESERVED_SUBCOMMANDS` in `src/cli/reserved.rs` carries
   the full list `claude --help` prints and the disjointness test asserts over
   all of it. Any other first token → implicit `csm run` → forwarded verbatim
   to `claude`. Adding a subcommand whose name collides with a claude
   subcommand is forbidden; `claude` itself is not one of claude's words, which
   is what makes the `csm claude <args…>` passthrough legal. `csm run`
   consuming a NEW claude flag before `--` is forbidden (the `--` boundary
   forwards the rest untouched). Two documented exceptions: `-h`/`--help`
   before any passthru token prints run's own usage (`csm run -- --help` still
   reaches claude), and `-n`/`--new` starts a fresh session. Invoked as
   `claude` (the `csm orca setup` alias), csm reserves none of its own words:
   claude's words and `--version`/`-v`/`--help`/`-h` at `args[1]` go straight
   to claude, everything else is a `csm run` launch.
3. **Orca's store is the single account authority.** Orca's account list,
   its stashes and its active id are the only account state. csm reads them
   (RPC when Orca runs, the store when it does not) and changes them through
   `orca::` only. No csm-side registry, no hardcoded allowlist, no profile
   names. One runtime dir `D` per machine and one active account per
   machine.
4. **Pure core + thin I/O shell** for testable features (`report.rs`,
   `cmd/accounts.rs`): the join/decision logic is a pure fn unit-tested against
   fixtures; network/stdin/stdout/clock live in `main`/the I/O shell only.
5. **Orca store rules.** Orca's files are another app's private state.
   - With Orca running, every change goes through its RPC. csm never writes
     the store, a stash or `D`'s credentials behind a running Orca.
   - Offline writes go through the store-write protocol in `orca::store`
     (liveness at L0, L1 and L2; temp file, then rename; Orca appearing
     mid-write is handled, not ignored) and only when `schemaVersion` is 1,
     the round-trip gate passes and the Orca version is in `TESTED`. A
     profile with a `profile-state.db` (Orca 1.4.214+) is written in that
     database only (`orca::statedb`: one `BEGIN IMMEDIATE` transaction on
     the `settings` row, L1 inside it before `COMMIT`), never in its
     `orca-data.json` export, and only when the database passes Orca's own
     checks (schema 3, WAL, `quick_check`, row hashes, profile id, the
     acceptance marker of a retained export). `store::preflight` runs
     those checks read-only before a switch touches `D`.
   - Byte fidelity: a patch changes only the keys it names; stashes and
     Keychain items keep exact bytes; new records follow Orca's key order.
   - Never select "no account", never delete a credential csm cannot
     attribute (quarantine it), never refresh the grant `D` holds, never
     edit Orca's settings.
   - Never print or log a token, a credential file, Orca's RPC `authToken`
     or its agent hook token. `Debug` impls redact; errors never quote file
     contents.
   - `csm hook` (every event) does no network I/O, no RPC and no Keychain
     access; an Orca-pane launch touches neither before claude starts.
6. **Test hygiene (machine safety).** Tests must never reach the real
   machine's accounts or starve it of processes.
   - Tests never resolve the real home, the real Orca userData, the login
     Keychain, `~/.claude*`, `~/.config/claude-as`, `~/.config/claude-smart`
     or csm's real state dir. The `cfg(test)` guards in `paths::home_dir`,
     `HostEnv::current` and the Keychain runner enforce this; keep the tests
     that prove each guard.
   - Tests talk to Orca RPC only through a fake socket server in a temp
     dir, never the live one.
   - No executable file per test: write fake scripts once and run them
     through an interpreter (`/usr/bin/perl <script>`, `/bin/sh <script>`);
     call `/bin/sleep` directly.
   - Spawned children start in their own process group; on timeout kill
     the group, poll `try_wait` for at most 2 s, and reap every child. No
     test leaves a process behind.
   - Local runs on a developer Mac: one full run at a time, never in a loop
     or in the background, with a timeout and a process limit
     (`timeout -k 10 600 cargo test -q -- --test-threads=4` under
     `ulimit -u`). Repeated runs (flake hunting, soak, e2e loops) go to a
     Linux box.
   - Manual runs of a built `csm` against the real home are limited to
     `--version`/`--help`; anything else runs with `HOME` set to a temp dir
     and `CLAUDE_CONFIG_DIR` and `ORCA_USER_DATA_PATH` unset.

## CLI surface (collision-safe)

`run, hook, accounts {list [--no-usage]|use|add|import|rm|doctor [--fix] [--offline]},
orca {status|setup},
config {show|get|set|unset launch-command|min-claude-version},
usage [--json] [--no-fetch] [--refresh] | usage capture,
statusline, completions, reap, claude <args…>`. There is no csm-global flag. The collision analysis
against claude's own subcommands is Invariant 2 above.

## Git workflow

**Commit directly to `main`** with Conventional Commits (`feat:`/`fix:`/`docs:`/
`refactor:`/`chore:`/`ci:`/`test:` …). `main` must stay green, so run `/verify`
first. release-please watches `main` and maintains a release PR automatically.
Branches/PRs are optional and usually unnecessary for this single-owner repo.
**Push only when the user asks.** Committing locally without pushing is fine.

Never merge-commit into main; rebase. release-please walks history by date
and stops at the previous release commit, so commits behind a merge commit
fall out of the changelog.

## Releases

Cutting a release is mostly automatic; see `/release` for the procedure.
TL;DR: conventional commits on `main` → release-please opens/updates a release PR
that bumps `Cargo.toml` + `Cargo.lock` + CHANGELOG → merging that PR tags
`vX.Y.Z`, runs the 4-target build matrix, attaches assets + `SHA256SUMS.txt`,
publishes the GitHub release, and bumps the Homebrew tap formula. **crates.io
publish is in CI**: the `publish-crate` job authenticates with Trusted
Publishing (OIDC, no static token) after the build matrix succeeds on a
release, and skips when the version is already on the index, so nothing manual
is needed.

## Known gaps

- **Claude Code refresh semantics are unverified.** When `D` switches to
  another account under a running session, csm assumes Claude Code takes
  its refresh lock, re-reads storage and adopts the new grant instead of
  writing the old account's rotated grant back (strings for this exist in
  Claude Code 2.1.283; the behaviour is inferred). The `min-claude-version`
  gate, the follow-at-turn-boundary rule and the read-back profile veto
  bound the damage if the assumption is wrong, but a test with a
  throwaway account (two sessions, a switch, a forced expiry) has not been
  run. A static read of Claude Code 2.1.293 (issue #42) supports the
  assumption: each request re-checks `.credentials.json`'s mtime and drops
  its cache on a change, a refresh runs under `<D>/.oauth_refresh.lock`
  and returns early when the stored access token differs from its own,
  and the write-back is a compare-and-swap on the refresh token, so a
  stale session leaves another account's grant alone. The same read
  suggests a turn parked in a 429 auto-retry keeps its old client and
  bearer, since only auth-type errors rebuild the client. If that holds,
  an in-place switch has to interrupt the parked turn and submit a new
  prompt. Until a throwaway-account run confirms both, the limit switch
  keeps relaunching (#42).
- **Windows with Orca stopped is only partly exercised.** With Orca running, the
  named pipe, telling `Orca.exe` from its helpers (every helper carries
  `--type=` or a `.js` entry, the `daemon-host` copy included) and the
  version from the executable's `VS_VERSIONINFO` were checked on a real
  machine (a raw scan for `0xFEEF04BD` used to hit Chromium code and read a
  garbage version, refusing every offline write). With Orca stopped,
  0.4.8 on a real Windows Orca 1.4.220 (2026-10-05) read the store
  (`accounts 2 (store)`, `offline writes allowed`), switched the active
  account offline and back, and served `usage --json` for both accounts;
  the restarted Orca answered RPC with the original active account. The e2e harness runs on
  macOS and Linux only.
- **Orca format drift.** csm ports Orca's private store format and account
  logic. The port follows Orca v1.4.209 to v1.4.220 (the 1.4.214 to
  1.4.220 drift touches none of the mirrored account, store or SQLite
  files), and offline writes are allowed only for `TESTED` (`1.4`). Linux
  reads the version from `resources/app.asar`'s `package.json`. The
  SQLite write was verified against a real Orca 1.4.218 on Linux and
  1.4.220 on macOS (stop, offline switch and back, on macOS also a
  `doctor --fix` orphan removal, start: Orca loaded the rows and answered
  RPC with the original active account), and the offline switch and back
  on 1.4.220 on Windows (no `doctor --fix` run there). A new Orca release
  needs a `tools/orca-drift.sh` pass before `TESTED` grows.
- **Minimal-store creation is verified against real Orca on Linux only.** With Orca
  stopped and no store at all, `accounts add`/`import` write
  `store::MINIMAL_STORE` plus the account keys (`write_protocol` with
  `allow_create`). Orca 1.4.214's source spreads its defaults under the
  parsed document (`normalizeLoadedProfileState`) and, on first start,
  migrates the JSON into SQLite through the same loader, so the file should
  load. A real Orca 1.4.218 on Linux, started on a sandbox userData after
  an offline `csm accounts import` into an empty one (2026-10-09), migrated
  the minimal JSON into SQLite and served the account over RPC. Not yet
  run against a real Orca on macOS or Windows. Known from the source: a store that
  exists without an `onboarding` block makes Orca skip its first-run wizard
  (`normalizeLoadedOnboardingState`).
- **Windows console-stop is unverified.** Two empirical checks gate the Windows
  relaunch loop and neither runs headlessly: (1) interactive Ctrl-C cancels
  claude's prompt rather than killing the supervisor; (2) after a limit-triggered
  switch the session `.jsonl` is complete, not truncated. Until both pass on a
  real Windows console the relaunch loop stays gated off and Windows falls back
  to launch-without-relaunch. See `src/platform/windows.rs`'s module doc.

## Don't touch / out of scope

- There is no hub-side data source anymore. `csm` collects usage locally per
  account (`src/usage/local/`) from Anthropic's own OAuth usage API. The retired
  hub scrape is out of scope here; `csm` no longer consumes
  `/cc-usage/api/data/limits`.
- The private companion repo's deployment glue (Brewfile/winget/Ansible shims)
  lives there, not here. Changes that span both repos: do the crate side here,
  note the companion edits for the other repo separately.
