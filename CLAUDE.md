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

- `src/main.rs`: `GLOBAL` (the process-wide allocator), `e2e::guard()` (the
  first call in `main()`), `main()`'s argv[0]/`args[1]` dispatch, and
  `print_help()`. Every `cmd_*` handler now lives under `src/cmd/`.
- `src/e2e.rs`: the seams `e2e/run.sh` needs, compiled in only with the
  `e2e` cargo feature (`[features] e2e = []`; `ENABLED` is `false`
  otherwise and every function is inert). With it: `guard()` exits 97
  unless `HOME` (and any `XDG_*_HOME`) lies inside `CSM_E2E_SANDBOX`; the
  Keychain runner calls `/usr/bin/perl $CSM_E2E_SECURITY` instead of
  `/usr/bin/security`; the Orca process scan counts only executables under
  the sandbox; `CSM_E2E_ORCA_VERSION` supplies the Orca version; and
  `point(name)` runs `/bin/sh $CSM_E2E_POINT_HOOK <name>` at the store
  writer's `store-L1`/`store-L2` points so a scenario can start the fake
  Orca mid-write. Never enable the feature in a release build.
- `src/cmd/`: one module per subcommand, namely `run.rs`, `hook.rs`, `cas.rs` (a
  compat stub: `--print-default-dir` prints `D`, `--eval` is a quiet no-op),
  `config.rs`, `usage.rs`, `accounts.rs` (`csm accounts`: pure arg parser,
  list render and doctor `findings` core over `orca::`), `orca.rs` (`csm orca
  status|setup`), `migrate.rs` (`csm migrate plan|import|retire`: the move off
  the legacy per-profile layout, with pure classify/merge/gate cores), `scan.rs`,
  `sidecar.rs`, `completions.rs`, `claude.rs` (the `csm claude <args…>`
  passthrough: pure `plan` + a thin unix-`exec` / windows-spawn shell; it runs
  in `D` and sets `CLAUDE_CONFIG_DIR` only when `launch_context::runtime_dir_pin`
  finds the inherited value differs from `D`), plus `support.rs` (uuid / stdin / tty helpers). Three
  reserved words dispatch outside `src/cmd/` instead: `reap` → `reaper::cmd`
  (`src/reaper/mod.rs`), `statusline` → `statusline::run`
  (`src/statusline.rs`), and `newuuid` → inline in `main()` (see the match in
  `src/main.rs`).
- `src/launch_context.rs`: how this launch was started (`Print`, `OrcaPane`,
  `OrcaStructured`, `Interactive`, with the `CSM_ORCA` override), the argv[0]
  `claude` alias check, the `CLAUDE_CONFIG_DIR` pin rule, and the managed-account
  auth-env strip list.
- `src/cli/`: `parser.rs` (hand-rolled `csm run` flag loop, NOT clap, so
  claude flags forward verbatim; it stops at the first positional, using
  `carry::arity` so a claude flag's value is not taken for one; `-h`/`--help`
  and `-n`/`--new` are the two claude-shaped flags it reads), `carry.rs` (pure
  `carry_passthru`: the arity-aware allow-list picking which remembered
  passthru flags a limit-switch hop replays on `claude --resume`, and which it
  drops; also `arity`), `completions.rs` (clap tree used ONLY for
  `csm completions`, never to parse real argv), `reserved.rs` (the reserved
  subcommand consts, `invocation` (argv[0] `csm` / `csm-hook` / `claude`) and
  `dispatch_subcommand` → `Dispatch {subcommand, rest_len}`, read by dispatch,
  completions, and the disjointness test).
- `src/account/`: `accounts.rs` (`AccountSet`: Orca's host accounts, the
  active id, `D`'s account and `D`, all read-only: `load` / `load_with`
  read Orca's store (`orca::store::load_choice`) plus `D`'s runtime
  identity and are the only loads the hook and the statusline use (design
  decision 8); `load_pinned` is `load` with a launch's `CLAUDE_CONFIG_DIR`
  pin applied; `load_live` / `load_live_with` take Orca's own list over RPC
  (`accounts.list`, 3 s `LIVE_LIST_TIMEOUT`) while Orca runs, because its
  store lags its memory, and fall back to the store otherwise (`from_orca`
  says which), for `csm usage`, usage collection and the limit leader;
  `find` resolves an id, id prefix or email), `limit_switch.rs` (the
  supervisor's side of a limit switch: lead or follow, the follow files for
  peers, the unsupervised-session `min-claude-version` gate), `scoring.rs` (pick-best
  thresholds: `LIMIT_PCT=99`, `SATURATION_PCT=95`;
  `is_viable_pcts` is the ONE viability predicate over session and week_all
  (`week_fable` no longer feeds it: a model-scoped-only cap leaves the
  account itself viable, and the hook's `fable_fallback_model` in
  `src/hook/detect.rs` handles that case with a same-account model swap
  instead); `pick_best_at` and the hook's account-switch target pick route
  through it; never add a second inline threshold check; also the shared
  `effective_reset_epoch`), `reset.rs` (compat parser for `resets`-only
  payloads), `mod.rs` (`pick_account_gated`, which the hook's target pick
  calls (see `src/hook/detect.rs`), and `current_usage`).
- `src/orca/`: csm as a second client of Orca's account service. `mod.rs`
  (`HostEnv`, `OrcaError`; `HostEnv::current()` refuses the real home under
  `cfg(test)`, tests build one with `HostEnv::for_test`), `context.rs` (one
  command's resolved context: userData, data file, `D` paths, state dir,
  version gate), `userdata.rs` (userData location, the WSL rule, the
  profile index), `store.rs` (`orca-data.json`: typed views, the pure
  `patch_settings` with its round-trip gate, and the store-write protocol
  with its L0/L1/L2 liveness checks), `jsjson.rs` (`JSON.stringify` byte
  for byte), `record.rs` (the account record and Orca's identity/active-id
  helpers), `stash.rs` (per-account stashes, exact bytes), `keychain.rs`
  (pure `-i`/argv `-X` builders plus the one `run_security` shell; refuses
  the real `/usr/bin/security` under `cfg(test)`), `runtime.rs` (`D`: paths,
  identity, the credential surfaces, the `D/sessions` registry scan),
  `readback.rs` (attribute the grant in `D` before overwriting it),
  `quarantine.rs`, `http.rs` (the profile and token calls), `refresh.rs`
  (the stash refresh), `sysdefault.rs` (the system-default snapshot),
  `rpc.rs` (NDJSON client over the socket or pipe named in
  `orca-runtime.json`), `live.rs` (is Orca running, fail closed),
  `procenv.rs` (Orca main's own `CLAUDE_CONFIG_DIR`), `version.rs` (installed
  version and `TESTED`), `snapshot.rs` (one read-only view), `switch.rs`
  (pure `plan_switch` plus the executor and its journal), `add.rs`
  (add/import/remove), `forward.rs` (the statusLine forward to Orca), `fsx.rs`
  (guarded file primitives, the state dir, `switch.lock`), and
  `testsupport.rs` (fixtures: a fake `security`, a fake store and socket).
- `src/usage/`: `model.rs` (`UsageData` serde), `transport.rs` (`fetch()`:
  positive TTL cache → `CSM_USAGE_CMD` → negative cooldown → `local::collect`),
  `report.rs` (`csm usage`: pure `build_report` + `render_table`/`render_json`),
  `local/` (the collector: `mod.rs` orchestrates per-account fresh/stale/probe
  resolution over Orca's host accounts, `creds.rs` reads the runtime or
  stashed grant read-only, `api.rs` calls Anthropic's `/api/oauth/usage` and owns the one
  `http_client` builder, `store.rs` persists `<state>/usage/<account-id>.json`,
  `statusline.rs` merges the statusLine stdin capture, `display.rs` formats
  reset times).
- `src/hook/`: `mod.rs` (`run` / `run_from_statusline`), `detect.rs`,
  `stop.rs`, `notify.rs`. Two live tiers remain: tier-0 `StopFailure` with
  `error: "rate_limit"` fires when a 429 ends the turn, and tier-2 usage-%
  catches caps crossed during a successful turn. A subscription cap fires NO
  hook at all (Claude Code parks the turn in an auto-retry wait), so
  `hook::run_from_statusline` runs the same classification off the statusLine
  tick (`csm usage capture` / `csm statusline`), which is the operative switch
  path for the weekly and model-scoped caps. The former tier-1
  (transcript-text) and tier-3 (malformed-in-tail) checks have been deleted.
  A `week_fable`-only cap does not switch accounts: `detect::classify_with`
  relaunches the same session on the same account with a `--model` override
  instead (`fable_fallback_model`, gated by `CLAUDE_FABLE_FALLBACK` and a
  one-shot `<sid>.model-fallback` marker), since the account itself still has
  headroom on every other model.
- `src/picker/`: `engine.rs` and `session.rs`, the in-process fuzzy session picker
  (nucleo + crossterm). There is no account picker.
- `src/reaper/`: `mod.rs`, `scan.rs`, `kill.rs`: the `csm reap` orphan killer.
- `src/config.rs`: csm's own `config.json` behind `csm config`.
- `src/envvar.rs`, `src/epoch.rs`, `src/testenv.rs`: one small helper each.
- `src/session/`, `src/sidecar/`, `src/platform/`, `src/statusline.rs`,
  `src/paths.rs`: session scan/index, sidecar store (with `account_id` and
  `born`), OS launch/relaunch/proc checks (`platform/child.rs` holds every
  bounded run-with-timeout helper), statusline, canonical state paths (state
  dir `$XDG_STATE_HOME/csm` or `~/.local/state/csm`, `%LOCALAPPDATA%\csm` on
  Windows).
- `tests/no_private_names.rs`: the leak guard. `src/**/*.rs` is scanned by all
  three rules (the `forbidden()` substring list, the `Dave-` host-prefix rule,
  and the quoted-profile-literal rule); `README.md`, `CLAUDE.md`, `Cargo.toml`,
  and `examples/*.sh` are scanned by the `forbidden()` substring rule only.
- `.github/workflows/ci.yml`: the push/PR gate (fmt, per-target check/clippy
  (4 targets: linux-gnu, aarch64-darwin, x86_64-darwin, windows-msvc) with
  native `cargo test` where the runner can execute the target, and an msrv
  (1.95) job).
- `.github/workflows/release-please.yml`: release automation (conventional
  commits → release PR → tag → build matrix + Homebrew bump; see Releases).
- `e2e/`: the end-to-end harness. `run.sh` builds `csm --features e2e`
  and the fake `claude` (`fake-claude/claude.c`, which also plays Orca's main
  process) once, lays out a sandbox under `/tmp`, and runs the 30 scenarios
  in `scenarios.sh` (helpers in `lib.sh`), each in a subshell with a time
  limit and a sweep for leftover processes. `fakes/` holds the fake Orca
  (`orca.pl` + `World.pm`: store, stashes, NDJSON socket), the fake Keychain
  (`security.pl`), the loopback OAuth stand-in (`http.pl`) and small
  helpers, all run through `/usr/bin/perl` or `/bin/sh`. Excluded from the
  packaged crate. See `e2e/README.md`.
- `tools/orca-drift.sh`: `bash tools/orca-drift.sh <old-tag> <new-tag>`
  diffs the Orca files csm mirrors between two Orca releases (blobless clone
  cached under `${XDG_CACHE_HOME:-~/.cache}/csm/orca-src`, or `--repo`).
  Excluded from the packaged crate.

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
   the former `CSM_OAUTH_REFRESH` opt-in is removed); any host-naming convention is injected via
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
   `cmd/accounts.rs`, `cmd/migrate.rs`): the join/decision logic is a pure fn unit-tested against
   fixtures; network/stdin/stdout/clock live in `main`/the I/O shell only.
5. **Deprecated `cas` compat stays quiet.** `csm cas --print-default-dir`
   prints `D`; `csm cas --eval …` prints nothing on stdout, one note on
   stderr, and exits 0, so a leftover shell shim does not break a shell.
   Every other `cas` verb is retired and fails with a pointer to
   `csm accounts`. Do not grow it back.
6. **Orca store rules.** Orca's files are another app's private state.
   - With Orca running, every change goes through its RPC. csm never writes
     the store, a stash or `D`'s credentials behind a running Orca.
   - Offline writes go through the store-write protocol in `orca::store`
     (liveness at L0, L1 and L2; temp file, then rename; Orca appearing
     mid-write is handled, not ignored) and only when `schemaVersion` is 1,
     the round-trip gate passes, the Orca version is in `TESTED`, and the
     profile has no `profile-state.db` (`store::sqlite_gate`: from Orca
     1.4.214 `orca-data.json` is then only an export, so those changes go
     over RPC only).
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
7. **Test hygiene (machine safety).** Tests must never reach the real
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

`run, hook, accounts {list|use|add|import|rm|doctor [--fix] [--offline]},
orca {status|setup}, migrate {plan|import|retire} [--dry-run],
config {show|get|set|unset launch-command|min-claude-version},
usage [--json] [--no-fetch] [--refresh] | usage capture,
scan, sidecar, statusline, completions, reap, newuuid, claude <args…>` + the
hidden compat `cas`. There is no csm-global flag. The collision analysis
against claude's own subcommands is Invariant 2 above.

## Git workflow

**Commit directly to `main`** with Conventional Commits (`feat:`/`fix:`/`docs:`/
`refactor:`/`chore:`/`ci:`/`test:` …). `main` must stay green, so run `/verify`
first. release-please watches `main` and maintains a release PR automatically.
Branches/PRs are optional and usually unnecessary for this single-owner repo.
**Push only when the user asks.** Committing locally without pushing is fine.

Never merge-commit into main; rebase. release-please walks history by date
and stops at the previous release commit, so commits behind a merge commit
fall out of the changelog (this happened on 2026-09-17 for 0.3.3).

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
  run.
- **Windows Orca detection is inferred.** Windows has no `SingletonLock`;
  the named-pipe framing, the plain-file stash and telling `Orca.exe` from
  its helpers follow Orca's source and have not been exercised on a real
  machine. The e2e harness runs on macOS and Linux only.
- **Orca format drift.** csm ports Orca's private store format and account
  logic. The port follows Orca v1.4.209 to v1.4.214, and offline writes are
  allowed only for `TESTED` (`1.4`). Linux has no version source yet, so
  Linux never writes the store offline (RPC only). A new Orca release needs
  a `tools/orca-drift.sh` pass before `TESTED` grows.
- **Minimal-store creation is unverified against real Orca.** With Orca
  stopped and no store at all, `accounts add`/`import` write
  `store::MINIMAL_STORE` plus the account keys (`write_protocol` with
  `allow_create`). Orca 1.4.214's source spreads its defaults under the
  parsed document (`normalizeLoadedProfileState`) and, on first start,
  migrates the JSON into SQLite through the same loader, so the file should
  load. Design section 2 gates this on starting a real Orca (1.4.212 and
  1.4.214) against a sandbox userData, which has not been done; the e2e
  harness uses the fake Orca only. Known from the source: a store that
  exists without an `onboarding` block makes Orca skip its first-run wizard
  (`normalizeLoadedOnboardingState`).

- **Windows console-stop is unverified.** Two empirical checks gate the Windows
  relaunch loop and neither runs headlessly: (1) interactive Ctrl-C cancels
  claude's prompt rather than killing the supervisor; (2) after a limit-triggered
  switch the session `.jsonl` is complete, not truncated. Until both pass on a
  real Windows console the relaunch loop stays gated off and Windows falls back
  to launch-without-relaunch. See `src/platform/windows.rs`'s module doc.
- **`csm statusline` render-side latency, measured.** A measurement of a
  release build on an Apple-silicon laptop (macOS, no host name; n=200 after
  a 10-run warmup): `csm statusline` with a real statusLine payload on stdin
  ran p50 11.48 ms / p95 22.64 ms, only ~1.3 ms above the bare process-spawn
  floor (`csm --version` p50 10.18 ms) and faster than a naive shell statusline
  (`zsh -c 'echo "..."'` p50 18.87 ms). These numbers are macOS-only; Linux,
  WSL and Windows are unmeasured, and the numbers predate the Orca forward
  and the statusLine switch check (both run after the segment is printed).

## Don't touch / out of scope

- There is no hub-side data source anymore. `csm` collects usage locally per
  account (`src/usage/local/`) from Anthropic's own OAuth usage API. The retired
  hub scrape is out of scope here; `csm` no longer consumes
  `/cc-usage/api/data/limits`.
- The private companion repo's deployment glue (Brewfile/winget/Ansible shims)
  lives there, not here. Changes that span both repos: do the crate side here,
  note the companion edits for the other repo separately.
