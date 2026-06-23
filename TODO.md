# TODO

Tracked gaps and improvements for `csm`. Scoped to the binary itself — no
consumer-specific deployment details. See `CLAUDE.md` for invariants and
`README.md` for user docs.

Status legend: 🔴 blocking · 🟠 should-do · 🟡 nice-to-have

## Blocking

### 🔴 Windows console-stop: two unverified empirical checks

The Windows relaunch loop must not be shipped until both checks below pass on a
real interactive `claude` session in a Windows console. They cannot be verified
in a headless/SSH session. See the gating note in `src/platform/windows.rs`
(module doc, "Two BLOCKING empirical checks").

- [ ] **Interactive Ctrl-C forwarding** — pressing Ctrl-C cancels claude's
      prompt, not the supervisor process. Without correct forwarding the
      supervisor dies and the relaunch loop is lost.
- [ ] **`CTRL_BREAK_EVENT` transcript flush** — after a limit-triggered switch,
      the session `.jsonl` transcript is complete (not truncated). A truncated
      transcript means session loss on relaunch.

Until both are confirmed, callers should keep the Windows relaunch loop gated
off and fall back to launch-without-relaunch on Windows.

## Should-do

### 🟢 Pluggable usage source via `CSM_USAGE_CMD` *(done)*

The hub was the only fresh-usage source; a machine without a hub had no way to
meter usage. Add an operator-injected command source + a configurable cache TTL.

- [x] `CSM_USAGE_CMD` — a shell command whose stdout is a `UsageData` JSON blob,
      run ahead of the hub HTTP/SSH transports (the explicit "check via my own
      script" path), cached like a network fetch, with a `CSM_USAGE_CMD_TIMEOUT`
      (default 10 s) hard deadline. Runs *before* the negative cooldown so a hub
      outage can't suppress an explicit command. `is_configured()` counts it.
- [x] Configurable positive-cache TTL via `CSM_USAGE_TTL_SECS` (alias of the
      legacy `CLAUDE_USAGE_TTL`; legacy wins if both set).
- [x] Documented in `README.md` ("Custom usage command"), including why a plain
      `claude -p`-based recipe is *not* recommended (PoC found `claude`'s
      `/usage` does not reliably emit the session/week gauges non-interactively).
- [x] `FetchError::Command` variant + 40 unit tests (valid/empty/non-JSON/
      non-zero-exit/timeout/alias/is_configured). Scoring reaches it for free
      (both `pick_account` and `current_usage` go through `usage::fetch()`).

### 🟠 Graceful degraded mode when the profile registry is absent

When `~/.config/claude-as/profiles.json` is missing, account scoring / auto-switch
/ pick-account silently disable (CAS-disabled mode), and `csm profiles list` /
`csm cas status` report nothing useful. This is correct fail-safe behavior but it
is easy to be in this state without noticing.

- [x] Emit a clear one-line hint pointing at `csm profiles add` when a
      registry-dependent command runs with no `profiles.json`. *(done: `csm
      usage` already prints the hint via `report.rs`; `csm pick-account` now
      bails gracefully with the same hint instead of a raw hub-fetch error, and
      `csm profiles list` no longer renders an empty `global default:` line —
      commit 703bdb8.)*
- [x] Document the degraded-mode contract (what works without a registry vs.
      what needs one) in `README.md`. *(done: "Without a registry (degraded
      mode)" table under Profiles.)*

### 🟠 Statusline is implemented but dormant

`src/statusline.rs` produces `<profile>@<host>` but is not the default render
path for any consumer yet (callers still use a shell statusline). Decide whether
`csm statusline` should be promoted to the documented default and benchmark its
cold-start latency against the shell version first (it sits on the prompt hot
path).

- [ ] Measure `csm statusline` cold-start latency.
- [ ] If acceptable, document it as the recommended statusline command.

## Nice-to-have

### 🟡 Mid-upgrade state read-compatibility

A binary upgrade can happen mid-session, so `csm` must read state files
(`<sid>.json`, `.relaunch`, `.pid`, sidecar store) written by an older version
(including the legacy shell implementation it replaces). Read-compat is claimed
in the code but not covered by an explicit cross-version fixture test.

- [x] Add a regression test that loads legacy-format state fixtures and asserts
      they parse without data loss. *(done: 6 fixtures — `sidecar` round-trips a
      full zsh-written sidecar (string `hop`), preserves unknown future fields
      via `#[flatten] extra` (rollback safety), and reads `{}` as all-None; the
      `scan` index preserves tabs in the label column, skips truncated rows
      without aborting the load, and round-trips write→load with every field
      intact.)*

### 🟡 Document the hub-down picker behavior

When the usage hub is unreachable, `csm` falls back to an interactive fzf account
picker (showing stale cached usage) rather than silently keeping the current
account. Make sure this behavior is documented where account selection is
described in `README.md`.

- [x] Document the hub-down fallback (fzf picker with stale-cache annotation).
      *(done: "Hub-down account selection" paragraph under Hub.)*
