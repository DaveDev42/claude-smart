---
name: verify
description: Run the full csm verification gate — cargo test (incl. the no_private_names leak guard), clippy --all-targets, and cargo fmt --check. Use before every commit, after any code change, or when the user asks to "verify"/"check"/"검증". Reports a single PASS/FAIL verdict with the failing details.
---

# /verify — the pre-commit gate for claude-smart

Run all three checks and report ONE clear verdict. Do not commit on a FAIL.

## Steps

1. **Tests + leak guard** (one command — the leak guard is `tests/no_private_names.rs`):
   ```sh
   cargo test
   ```
   The run MUST end with `test result: ok` for the unit suite AND
   `no_private_identifiers_in_shipped_source ... ok`. Any `FAILED` or
   `error[` = FAIL.

2. **Lint:**
   ```sh
   cargo clippy --all-targets
   ```
   Must finish with no `warning:`/`error:` lines from the crate. (Warnings are
   failures here — this crate is kept warning-clean.)

3. **Format check:**
   ```sh
   cargo fmt --check
   ```
   Any diff = FAIL (run `cargo fmt` to fix, then re-verify).

Run them in sequence; you may batch the three into one Bash call with `&&` if you
want a single pass, but capture enough output to attribute a failure to the right
step.

## Reporting

- **PASS** — state the test count (e.g. "547 unit + leak guard, clippy clean, fmt
  clean") in one line. The change is safe to commit.
- **FAIL** — name the failing step, paste the relevant failing lines (the assert
  message / clippy lint / fmt diff), and STOP. Do not commit. Offer to fix.

## Invariant reminders (what the checks protect)

- The leak guard enforces invariant #1 (no private identifiers in production
  `src/`). If it fails, a `personal`/`work`/`workstation`/`example-tnet`/email
  literal leaked into a non-`#[cfg(test)]` line — move it behind the registry/env
  contract, don't suppress the guard.
- If `cargo` resolves to the wrong toolchain (rare; sandbox/PATH), fall back to
  the explicit pin documented in `CLAUDE.md` and re-run.
