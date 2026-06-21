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

### 🟠 Publish to crates.io

`README.md` still says `cargo install claude-smart` works "once published," and
`cargo install` from the registry is the documented happy path. Confirm the
crate is actually published (and that the release CI's publish job succeeds), then
drop the "once published" caveat from the README.

- [ ] Verify `claude-smart` 0.2.0 (or later) is live on crates.io and the index
      carries it (so `cargo install` / `cargo binstall` resolve without `--git`).
- [ ] Once confirmed, remove the "(once published)" note in `README.md`.

### 🟠 Graceful degraded mode when the profile registry is absent

When `~/.config/claude-as/profiles.json` is missing, account scoring / auto-switch
/ pick-account silently disable (CAS-disabled mode), and `csm profiles list` /
`csm cas status` report nothing useful. This is correct fail-safe behavior but it
is easy to be in this state without noticing.

- [ ] Emit a clear one-line hint pointing at `csm profiles add` when a
      registry-dependent command runs with no `profiles.json`.
- [ ] Document the degraded-mode contract (what works without a registry vs.
      what needs one) in `README.md`.

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

- [ ] Add a regression test that loads legacy-format state fixtures and asserts
      they parse without data loss.

### 🟡 Document the hub-down picker behavior

When the usage hub is unreachable, `csm` falls back to an interactive fzf account
picker (showing stale cached usage) rather than silently keeping the current
account. Make sure this behavior is documented where account selection is
described in `README.md`.

- [ ] Document the hub-down fallback (fzf picker with stale-cache annotation).
