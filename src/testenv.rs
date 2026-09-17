//! One seam for every test's env-var mutation, across module boundaries.
//!
//! `std::env::set_var`/`remove_var` mutate the whole process's environment,
//! and `cargo test` runs every module's `#[cfg(test)]` tests in parallel
//! threads of that one process. A module-local `static ENV_LOCK: Mutex<()>`
//! only serializes tests *within* that module; it does nothing to protect
//! against a DIFFERENT module's test mutating the same variable
//! concurrently — `CLAUDE_CONFIG_DIR` used to be mutated by tests in three
//! modules (`statusline`, `usage::local`, `cas::eval`) through three
//! independent locks, which left a real interleaving possible: one module's
//! test sets `CLAUDE_CONFIG_DIR` to some path between another module's
//! `remove_var` and its call into config-dir-reading code, silently
//! resolving a directory neither test intended.
//!
//! `lock_for(name)` fixes that by keying the guard on the variable name
//! itself, in one process-wide registry, so every test anywhere in the crate
//! that touches the same variable serializes through the same lock while
//! tests touching different variables still run in parallel.
//!
//! `set_var`/`remove_var` here are the crate's only two TEST-side call sites
//! for `std::env::set_var`/`remove_var` — no fixture touches the raw
//! `std::env` mutators. (Production has exactly one: `main::pin_global_profile`
//! exporting `CLAUDE_CONFIG_DIR` for a csm-global `--profile`, which runs
//! single-threaded before dispatch and so needs no lock.)
//! Most fixtures reach them through `with_env_var`/`with_env_vars`;
//! a few RAII fixtures (`hook`'s `EnvFixture` and `spawn_fake_managed_process`,
//! `usage::local::refresh`'s `TokenUrlEnv`) call `set_var`/`remove_var`
//! directly but still hold `lock_for(name)` for their whole set→act→restore
//! span, so the same per-name serialization holds regardless of call path.
//! That makes a future edition bump (where those `std::env` functions become
//! `unsafe`) a two-function change instead of one scattered across every test
//! module.
//!
//! Every lock is acquired with `.unwrap_or_else(|e| e.into_inner())` so one
//! panicking test never poisons the lock for every other test that touches
//! the same variable.

#[cfg(test)]
fn registry()
-> &'static std::sync::Mutex<std::collections::HashMap<&'static str, &'static std::sync::Mutex<()>>>
{
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, &'static std::sync::Mutex<()>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The per-name lock guard for `name`. Every test (in any module) that
/// sets/removes the same env var must hold this guard for the full
/// set→act→restore sequence.
#[cfg(test)]
pub(crate) fn lock_for(name: &'static str) -> std::sync::MutexGuard<'static, ()> {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mutex: &'static std::sync::Mutex<()> = map
        .entry(name)
        .or_insert_with(|| Box::leak(Box::new(std::sync::Mutex::new(()))));
    drop(map);
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The crate's one `std::env::set_var` call site for tests.
#[cfg(test)]
pub(crate) fn set_var(name: &str, value: &str) {
    // SAFETY: test-only. Every caller — `with_env_var`/`with_env_vars` and
    // the RAII fixtures in `hook` and `usage::local::refresh` — holds
    // `lock_for(name)` for the whole set->act->restore sequence, so no other
    // test mutates or reads this variable concurrently, and nothing but
    // `cargo test` threads runs in this process.
    unsafe { std::env::set_var(name, value) };
}

/// The crate's one `std::env::remove_var` call site for tests.
#[cfg(test)]
pub(crate) fn remove_var(name: &str) {
    // SAFETY: test-only. Every caller — `with_env_var`/`with_env_vars` and
    // the RAII fixtures in `hook` and `usage::local::refresh` — holds
    // `lock_for(name)` for the whole set->act->restore sequence, so no other
    // test mutates or reads this variable concurrently, and nothing but
    // `cargo test` threads runs in this process.
    unsafe { std::env::remove_var(name) };
}

/// Run `f` with `name` set to `value` (or removed, for `None`), holding
/// `lock_for(name)` for the whole set→act→restore sequence and restoring the
/// prior value afterward even if `f` panics.
#[cfg(test)]
pub(crate) fn with_env_var<T>(name: &'static str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = lock_for(name);
    let prior = std::env::var(name).ok();
    match value {
        Some(v) => set_var(name, v),
        None => remove_var(name),
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match &prior {
        Some(v) => set_var(name, v),
        None => remove_var(name),
    }
    match result {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

/// Run `f` with every `(name, value)` pair in `pairs` set (or removed, for
/// `None`), each held under its own `lock_for(name)` for the duration —
/// unrelated variables' locks stay independent so tests on different
/// variables still run in parallel.
#[cfg(test)]
pub(crate) fn with_env_vars<T>(pairs: &[(&'static str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    match pairs.split_first() {
        None => f(),
        Some((&(name, value), rest)) => with_env_var(name, value, || with_env_vars(rest, f)),
    }
}

// `HOME`/`USERPROFILE` fixture override — thread-local, not process-global.
//
// On Windows, `dirs::home_dir()` never reads `HOME` or `USERPROFILE`; it
// calls `SHGetKnownFolderPath(FOLDERID_Profile)` unconditionally. A fixture
// that does `set_var("HOME", tmpdir)` therefore gives zero isolation there —
// the code under test still resolves the real runner profile dir. All
// `dirs::home_dir()` call sites in this crate route through
// `crate::paths::home_dir` instead, whose `#[cfg(test)]` body consults this
// thread-local override first.
//
// Thread-local, not a `Mutex`-guarded static: every test runs on its own
// thread and the paths under test never cross threads, so fixtures need no
// lock here and cannot interfere with each other the way a shared
// process-global env var would.

#[cfg(test)]
thread_local! {
    static TEST_HOME: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Set (or clear, with `None`) this thread's home-dir override for tests.
#[cfg(test)]
pub(crate) fn set_test_home(home: Option<std::path::PathBuf>) {
    TEST_HOME.with(|h| *h.borrow_mut() = home);
}

/// This thread's home-dir override, if a fixture has set one.
#[cfg(test)]
pub(crate) fn test_home() -> Option<std::path::PathBuf> {
    TEST_HOME.with(|h| h.borrow().clone())
}

/// Run `f` with this thread's home-dir override pointed at `tmp`, restoring
/// the prior override afterward even if `f` panics. Thread-local, so this
/// needs no lock — callers just fold it into the same closure-based fixture
/// pattern as `with_env_var`.
#[cfg(test)]
pub(crate) fn with_test_home<T>(tmp: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let prior = test_home();
    set_test_home(Some(tmp.to_path_buf()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    set_test_home(prior);
    match result {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}
