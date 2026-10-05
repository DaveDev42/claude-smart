//! `csm usage [--json] [--no-fetch] [--refresh]` / `csm usage capture`.

use std::ffi::OsString;

use crate::cmd::support::{CAPTURE_STDIN_CAP_BYTES, read_stdin_capped};
use crate::usage::local::{CollectOpts, OrcaUsage};
use crate::{account, hook, paths, usage};

/// `csm usage [--json] [--no-fetch] [--refresh]` / `csm usage capture`
///
/// One row per Orca host account (labelled by email and organization),
/// joined with the local per-account usage store. Offline-aware: serves the
/// stale positive cache with an age header when collection is unreachable.
///
/// Where the numbers come from (design section 8): the account `D` holds
/// from its statusLine captures and its runtime grant; inactive accounts
/// from Orca's cached usage (`accounts.list{refreshUsage:false}`) while Orca
/// runs, else from their stash tokens.
///
/// `--no-fetch` reads only the cache. `--refresh` bypasses the cache and
/// every account's store TTL, asks a running Orca to re-probe its inactive
/// accounts (`refreshUsage:true`, up to 30 s), and — with Orca stopped —
/// refreshes a stash grant that expires within 5 min, as Orca would. The
/// former `--refresh-oauth` / `CSM_OAUTH_REFRESH` opt-in is gone: that
/// refresh is now part of `--refresh`.
///
/// `csm usage capture` is the statusLine-stdin capture path (see
/// [`cmd_usage_capture`]) — a distinct subverb, not a flag.
pub(crate) fn cmd_usage(args: &[OsString]) -> anyhow::Result<()> {
    use usage::report;

    // `csm usage capture` is checked first so the bare positional never falls
    // into the flag loop below (it takes no flags of its own).
    if args.first().map(|a| a.to_string_lossy()).as_deref() == Some("capture") {
        return cmd_usage_capture();
    }

    let mut json = false;
    let mut no_fetch = false;
    let mut refresh = false;
    for a in args {
        match a.to_string_lossy().as_ref() {
            "--json" => json = true,
            "--no-fetch" => no_fetch = true,
            "--refresh" => refresh = true,
            "-h" | "--help" => {
                println!("usage: csm usage [--json] [--no-fetch] [--refresh]");
                println!("       csm usage capture");
                println!("  --json      emit the joined accounts∪local view as JSON");
                println!("  --no-fetch  read only the local cache (no live collection)");
                println!("  --refresh   bypass every cache; ask a running Orca to re-probe, or");
                println!("              (Orca stopped) refresh stash grants that are due");
                println!(
                    "  capture     read statusLine JSON from stdin, merge into the local store"
                );
                return Ok(());
            }
            other => anyhow::bail!(
                "csm usage: unknown flag '{other}' (try --json | --no-fetch | --refresh | capture)"
            ),
        }
    }

    // Orca's live list when it runs (the store lags it on Orca 1.4.214+).
    let accounts = account::AccountSet::load_live();
    let rpt = build_usage_report(&accounts, no_fetch, refresh);

    if json {
        println!("{}", report::render_json(&rpt)?);
    } else {
        print!("{}", report::render_table(&rpt, chrono::Utc::now()));
    }
    Ok(())
}

/// The joined usage report for `accounts`: the one pipeline behind both
/// `csm usage` and `csm accounts list`. See [`cmd_usage`] for the flags.
pub(crate) fn build_usage_report(
    accounts: &account::AccountSet,
    no_fetch: bool,
    refresh: bool,
) -> usage::report::Report {
    use usage::report;
    // "Configured" means Orca lists at least one host account.
    let configured = !accounts.is_empty();
    let ids = accounts.ids_sorted();
    let opts = if refresh {
        CollectOpts {
            force: true,
            orca: OrcaUsage::Refresh(usage::LIMIT_PICK_TIMEOUT),
            refresh_stash: true,
            ..CollectOpts::standard()
        }
    } else {
        CollectOpts {
            orca: OrcaUsage::Cached,
            ..CollectOpts::standard()
        }
    };

    // Resolve usage data + freshness. `--no-fetch` reads the cache directly;
    // `--refresh` forces fetch_with(true) (cache + per-profile TTL bypass);
    // otherwise fetch() runs the full resilience ladder (which itself prefers
    // a fresh cache before any live collection).
    // Staleness age is derived from the DATA's own per-profile `captured_at`
    // timestamps (`oldest_profile_age_secs` — the age of the least-fresh
    // served profile), never from `.usage-cache.json`'s file mtime.
    // `write_positive_cache` refreshes that mtime on every non-total-failure
    // `fetch_with` call — including a round where every profile was
    // `ServeStale`-served from a days-old store record — so the file's mtime
    // no longer reflects how old the served numbers actually are; the "⚠
    // usage data is Nm old" banner would otherwise be unreachable for exactly
    // the offline/expired-token case it exists to surface.
    let (data, stale_secs) = if !configured {
        (None, None)
    } else if no_fetch {
        let cached = read_usage_cache();
        let stale = cached
            .as_ref()
            .and_then(|d| usage::local::oldest_profile_age_secs(d, chrono::Utc::now()));
        (cached, stale)
    } else {
        let fetch_result = usage::fetch_with(&opts);
        match fetch_result {
            Ok(d) => {
                let stale = usage::local::oldest_profile_age_secs(&d, chrono::Utc::now());
                (Some(d), stale)
            }
            Err(_) => {
                // Local collection unreachable — degrade to the last-known cache, if any.
                let cached = read_usage_cache();
                let stale = cached
                    .as_ref()
                    .and_then(|d| usage::local::oldest_profile_age_secs(d, chrono::Utc::now()));
                (cached, stale)
            }
        }
    };

    let mut rpt = report::build_report(&ids, data.as_ref(), configured, stale_secs);
    report::label_rows(&mut rpt, &accounts.accounts);
    rpt.active = accounts.active.clone();
    rpt
}

/// Read the positive usage cache file directly (no network, no TTL gate). Used
/// by `--no-fetch` and the offline-degrade path. Returns `None` when absent or
/// unparseable.
pub(crate) fn read_usage_cache() -> Option<usage::UsageData> {
    let raw = std::fs::read_to_string(paths::usage_cache()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `csm usage capture` — read a statusLine JSON payload from stdin and merge
/// its `rate_limits` into the store record of the account the session runs
/// on (see
/// `usage::local::record_statusline_payload`).
///
/// This is meant to run silently as a fire-and-forget tail of a
/// `statusline-command.sh`/`.ps1` (e.g. `printf '%s' "$input" | csm usage
/// capture &`), so it swallows every error — a malformed/partial payload, an
/// unattributable session, a throttled write — and
/// unconditionally prints nothing and exits 0. A statusLine command that fires
/// roughly once a second must never let a transient capture failure surface
/// as prompt noise or a non-zero exit.
fn cmd_usage_capture() -> anyhow::Result<()> {
    let raw = read_stdin_capped(CAPTURE_STDIN_CAP_BYTES);
    if let Ok(Some(capture)) = usage::local::record_statusline_payload(&raw) {
        hook::run_from_statusline(&raw, &capture);
    }
    // idle_compact reads the payload's own session_id/prompt_cache/
    // transcript_path directly — it runs regardless of whether this
    // session's usage capture could be attributed to an account above.
    crate::idle_compact::run_from_raw(&raw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_data_full_payload_maps_fields() {
        let json = serde_json::json!({
            "profiles": {
                "home": {
                    "session": { "pct": 3 },
                    "week_all": { "pct": 32, "resets": "Jun 18 at 9pm (Asia/Seoul)", "resets_at": 1_781_000_000_i64 }
                },
                "work": {
                    "session": null,
                    "week_all": { "pct": 80, "resets": null }
                }
            },
            "errors": {
                "broken": "no credentials"
            }
        });
        let data: usage::UsageData = serde_json::from_value(json).unwrap();
        assert_eq!(data.profiles.len(), 2);
        let home = &data.profiles["home"];
        assert_eq!(home.session.as_ref().map(|s| s.pct), Some(3));
        let home_week_all = home.week_all.as_ref().unwrap();
        assert_eq!(home_week_all.pct, 32);
        assert_eq!(
            home_week_all.resets.as_deref(),
            Some("Jun 18 at 9pm (Asia/Seoul)")
        );
        assert_eq!(home_week_all.resets_at, Some(1_781_000_000));

        let work = &data.profiles["work"];
        assert!(work.session.is_none());
        let work_week_all = work.week_all.as_ref().unwrap();
        assert_eq!(work_week_all.pct, 80);
        assert_eq!(
            work_week_all.resets_at, None,
            "resets_at absent in cache JSON must parse as None"
        );

        assert_eq!(data.errors.unwrap()["broken"], "no credentials");
    }

    #[test]
    fn usage_data_absent_errors_key_parses_none() {
        let json = serde_json::json!({
            "profiles": {
                "home": {
                    "week_all": { "pct": 50 }
                }
            }
        });
        let data: usage::UsageData = serde_json::from_value(json).unwrap();
        assert_eq!(data.profiles.len(), 1);
        assert!(data.errors.is_none());
    }

    #[test]
    fn usage_data_empty_object_parses_to_defaults() {
        let data: usage::UsageData = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(data.profiles.is_empty());
        assert!(data.errors.is_none());
    }
}
