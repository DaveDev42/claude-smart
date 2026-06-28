---
name: verify
description: Run the full csm verification gate — cargo test (incl. the no_private_names leak guard), clippy --all-targets, cargo fmt --check, and a Windows cross-target clippy (catches #[cfg(windows)] breakage the macOS-native steps can't see). Use before every commit, after any code change, or when the user asks to "verify"/"check"/"검증". Reports a single PASS/FAIL verdict with the failing details.
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

4. **Windows cross-target lint** (catches `#[cfg(windows)]` breakage):
   ```sh
   rustup target add x86_64-pc-windows-gnu   # idempotent; installs the std once
   cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
   ```
   The macOS- (and Linux-) native steps above NEVER compile code behind
   `#[cfg(windows)]`, so a Windows-only type/lint error sails through them and
   only blows up in CI on the `x86_64-pc-windows-msvc` job (this is exactly how
   the `HANDLE.is_null()` break in `reaper/kill.rs` shipped to a release).
   The `-gnu` target needs no MSVC linker — `clippy`/`check` type-check and lint
   without producing the final binary — and it shares the same `HANDLE = isize`
   and `windows-sys 0.52` as the MSVC release target, so it reproduces the same
   class of bug. Run it locally; do not wait for CI to find it.
   - A `warning:`/`error[`/`error:` from the crate against this target = **FAIL**
     (same warnings-as-errors bar as step 2).
   - If the target genuinely cannot be installed (offline / no `rustup`), say so
     explicitly and mark this step **SKIPPED (Windows unverified)** — that is a
     soft degrade, not a PASS. Never silently drop it.

Run them in sequence; you may batch the four into one Bash call with `&&` if you
want a single pass, but capture enough output to attribute a failure to the right
step. (Steps 1–3 share the host target's build cache; step 4 builds a separate
target dir the first time, so it is the slow one — that is expected.)

## Reporting

- **PASS** — state the test count and the four steps in one line (e.g. "611 unit
  + leak guard, clippy clean, fmt clean, windows-gnu clippy clean"). The change
  is safe to commit. If step 4 was skipped, say **PASS (Windows unverified —
  cross-target unavailable)** so the gap is visible.
- **FAIL** — name the failing step, paste the relevant failing lines (the assert
  message / clippy lint / fmt diff / Windows compile error), and STOP. Do not
  commit. Offer to fix. A Windows-only error still blocks the commit — it would
  break the release build matrix.

## Invariant reminders (what the checks protect)

- The leak guard enforces invariant #1 (no private identifiers anywhere in
  `src/`, including `#[cfg(test)]` fixtures). If it fails, an operator
  account-profile name, hub/host name, real home path, tailnet suffix, or email
  literal leaked in — replace it with a neutral placeholder (`work`, `home`,
  `/Users/example`, `Acme-…`) or move it behind the registry/env contract.
  Never suppress the guard or weaken its forbidden list.
- If `cargo` resolves to the wrong toolchain (rare; sandbox/PATH), fall back to
  the explicit pin documented in `CLAUDE.md` and re-run. The same pinned-cargo
  fallback applies to step 4 — `rustup target add` writes the `-gnu` std into
  that toolchain's target list, and the pinned `cargo` picks it up with
  `--target x86_64-pc-windows-gnu`.
- Step 4 reproduces CI's `x86_64-pc-windows-msvc` job closely enough to catch
  compile/lint breakage, but it is not byte-identical (gnu vs msvc ABI, no final
  link). It is a strong pre-commit smoke test, not a replacement for the CI
  matrix — CI is still the source of truth for the MSVC target.
