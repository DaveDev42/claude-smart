# Windows launcher — 2 BLOCKING empirical checks (manual)

These two checks gate shipping the Windows relaunch loop (design spec §4 / §8).
They **cannot** be run over headless SSH — both require a real interactive
console with a keyboard and a real `claude.exe`. Run them yourself on
**Acme-Win** in a normal terminal (Windows Terminal / pwsh window), not an
SSH session.

Everything else is already verified non-interactively (release build green, 492
tests, supervisor spawn/argv/env/pidfile/exit, born-timing, `.stop` path
compiles + the cooperative-stop code path). What's left is the two things only a
human at a keyboard can confirm.

## Prereqs

```pwsh
cd $env:USERPROFILE\Projects\claude-smart
git pull
cargo build --release
$csm = "$env:USERPROFILE\Projects\claude-smart\target\release\csm.exe"
```

---

## Check #1 — interactive Ctrl-C forwarding

**Claim under test:** because the launcher spawns claude with
`CREATE_NEW_PROCESS_GROUP`, Windows stops delivering keyboard Ctrl-C to claude.
The supervisor's `SetConsoleCtrlHandler` must **forward** the interrupt so that
pressing Ctrl-C cancels *claude's* current prompt — and does **NOT** kill the
csm supervisor itself.

**Steps:**

1. In a real terminal window, launch a real claude session through csm:
   ```pwsh
   & $csm run --no-pick --profile personal
   ```
2. Wait for claude's prompt. Start typing a long response request (e.g. "write a
   very long essay about ...") so claude is actively streaming.
3. Press **Ctrl-C once** while claude is streaming.

**PASS if:**
- claude's current generation is interrupted/cancelled (you return to claude's
  prompt), AND
- the csm process is **still running** — you are still inside the claude
  session, not dumped back to the pwsh prompt.

**FAIL if:**
- Ctrl-C kills the whole thing and you land back at the pwsh prompt (supervisor
  died — forwarding not working), OR
- Ctrl-C does nothing at all (interrupt was swallowed and not forwarded).

> If FAIL: the bug is in `console_ctrl_handler` / `SetConsoleCtrlHandler` in
> `src/platform/windows.rs`. The handler must `GenerateConsoleCtrlEvent(CTRL_C_EVENT,
> pgid)` and return TRUE.

---

## Check #2 — CTRL_BREAK transcript flush

**Claim under test:** when the limit-switch hook drops the `<sid>.stop` flag, the
supervisor sends `CTRL_BREAK_EVENT` to claude's group, and real `claude.exe`
responds by **flushing its transcript `.jsonl` to disk and exiting cleanly**
within the grace window — so the relaunch hop resumes a *complete* session.

**Steps:**

1. Launch a real claude session and note its session id:
   ```pwsh
   & $csm run --no-pick --profile personal --session-id manual-break-test
   ```
2. Have a short exchange with claude (one prompt + answer) so the transcript has
   content.
3. From a **second** terminal window, drop the stop flag:
   ```pwsh
   "1" | Set-Content "$env:USERPROFILE\.claude.shared\smart\manual-break-test.stop"
   ```
4. Watch the first window: csm should send CTRL_BREAK, wait the grace period
   (`CLAUDE_SWITCH_GRACE_MS`, default 5s), and the session should end.
5. Inspect the transcript `.jsonl` for session `manual-break-test` under the
   claude projects dir.

**PASS if:**
- claude exited within the grace window (no hard `TerminateProcess` needed), AND
- the transcript `.jsonl` is **complete and well-formed** (the last exchange is
  fully written, valid JSON lines, not truncated mid-record).

**FAIL if:**
- claude had to be hard-killed after grace (CTRL_BREAK didn't reach it), OR
- the `.jsonl` is truncated / the last record is half-written (flush didn't
  complete before exit).

> If FAIL: revisit the grace window and the `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT,
> pgid)` call in `supervise()`. claude may need a longer grace, or CTRL_BREAK may
> need to be paired with a different signal.

---

## After both pass

Record the result here (date + PASS/PASS) and remove the Windows ship gate:
flip `gw_ai_command` to `csm` on Windows-native (`shared/gw/playbook.yml`) and
let the dave-environment Windows consumption edits land (Phase 5 §7).

| Date | Check #1 (Ctrl-C fwd) | Check #2 (CTRL_BREAK flush) | By |
|------|----------------------|------------------------------|----|
|      |                      |                              |    |
