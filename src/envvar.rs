//! One place to parse csm's numeric environment knobs. The NAMES are an
//! external contract (README documents them all); this module owns only how
//! their values are parsed.

/// Read `primary`, falling back to `alias`, falling back to `default`.
///
/// Both names are tried as whole strings first (matching the legacy
/// `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` pair's original behavior): if
/// `primary` is set but its value fails to parse as `u64`, the alias is
/// still consulted rather than falling straight to `default`. Each value is
/// `.trim()`med before parsing.
pub(crate) fn u64_with_alias(primary: &str, alias: &str, default: u64) -> u64 {
    std::env::var(primary)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| {
            std::env::var(alias)
                .ok()
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(default)
}

/// Read `name` as a `u64`, or `default` when unset/unparseable. `.trim()`med
/// before parsing.
pub(crate) fn u64_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Read `name` as an `i64`, or `default` when unset/unparseable. `.trim()`med
/// before parsing.
pub(crate) fn i64_or(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_vars<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var(name).ok()))
            .collect();
        for (name, value) in vars {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        f();
        for (name, saved_value) in saved {
            match saved_value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn u64_with_alias_falls_through_on_unparseable_primary() {
        with_vars(
            &[
                ("CSM_TEST_ENVVAR_PRIMARY", Some("not-a-number")),
                ("CSM_TEST_ENVVAR_ALIAS", Some("42")),
            ],
            || {
                assert_eq!(
                    u64_with_alias("CSM_TEST_ENVVAR_PRIMARY", "CSM_TEST_ENVVAR_ALIAS", 7),
                    42,
                    "unparseable primary should fall through to a valid alias"
                );
            },
        );
    }

    #[test]
    fn u64_with_alias_prefers_valid_primary() {
        with_vars(
            &[
                ("CSM_TEST_ENVVAR_PRIMARY", Some(" 5 ")),
                ("CSM_TEST_ENVVAR_ALIAS", Some("42")),
            ],
            || {
                assert_eq!(
                    u64_with_alias("CSM_TEST_ENVVAR_PRIMARY", "CSM_TEST_ENVVAR_ALIAS", 7),
                    5
                );
            },
        );
    }

    #[test]
    fn u64_with_alias_default_when_both_unset() {
        with_vars(
            &[
                ("CSM_TEST_ENVVAR_PRIMARY", None),
                ("CSM_TEST_ENVVAR_ALIAS", None),
            ],
            || {
                assert_eq!(
                    u64_with_alias("CSM_TEST_ENVVAR_PRIMARY", "CSM_TEST_ENVVAR_ALIAS", 7),
                    7
                );
            },
        );
    }

    #[test]
    fn u64_or_trims_and_parses() {
        with_vars(&[("CSM_TEST_ENVVAR_U64", Some(" 9 "))], || {
            assert_eq!(u64_or("CSM_TEST_ENVVAR_U64", 1), 9);
        });
    }

    #[test]
    fn u64_or_default_on_unparseable() {
        with_vars(&[("CSM_TEST_ENVVAR_U64", Some("nope"))], || {
            assert_eq!(u64_or("CSM_TEST_ENVVAR_U64", 1), 1);
        });
    }

    #[test]
    fn i64_or_trims_and_parses() {
        with_vars(&[("CSM_TEST_ENVVAR_I64", Some(" -3 "))], || {
            assert_eq!(i64_or("CSM_TEST_ENVVAR_I64", 1), -3);
        });
    }

    #[test]
    fn i64_or_default_on_unset() {
        with_vars(&[("CSM_TEST_ENVVAR_I64", None)], || {
            assert_eq!(i64_or("CSM_TEST_ENVVAR_I64", 1), 1);
        });
    }
}
