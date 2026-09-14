//! Local, hub-free usage collection — replaces the hub scrape entirely.
//!
//! Two entry points:
//! - [`collect`] — for every configured profile, serve a fresh store record,
//!   or probe live (`creds::lookup` → `api::fetch_usage`), or serve a stale
//!   record with a rollover, or report an error. Called by
//!   [`crate::usage::fetch`]/[`crate::usage::fetch_with`] as the terminal
//!   layer of the cache ladder.
//! - [`record_statusline_payload`] — `csm usage capture`'s core: merge a
//!   statusLine stdin payload into the active profile's store record.
//!
//! # `collect` algorithm (design spec "collect 알고리즘")
//!
//! For each profile, in order:
//! 1. **Fresh?** `!force && now - rec.api_captured_at < CSM_USAGE_PROFILE_TTL`
//!    → serve `rec.usage` (rolled over for any expired section) as-is. Gated
//!    on `api_captured_at` — the last time this profile actually reached a
//!    *live api probe* — never on the record's blanket `captured_at`. A
//!    statusline capture (`record_statusline_payload`) refreshes
//!    `captured_at` roughly every 10s for the active profile without
//!    touching `api_captured_at`; if the TTL check used `captured_at`
//!    instead, a long-running session would never re-probe the API, and
//!    `week_fable`/`week_model_label` (which only an api probe can refresh —
//!    statusLine stdin never carries per-model-tier data) would freeze
//!    forever at whatever the last real probe produced.
//! 2. **Cooldown?** `rec.cooldown_until > now` → serve the stale record
//!    (rolled over) if one exists, else record an error. No probe attempted.
//! 3. **Probe**: `creds::lookup(dir, now)` → on success, `api::fetch_usage`.
//!    Every terminal outcome (no creds, expired, unreadable, rate-limited,
//!    unauthorized, other API failure, or success) collapses to an [`Event`],
//!    and [`resolve`] — a pure function taking that `Event` plus whether a
//!    stale record exists — decides what `collect()` does next. This keeps
//!    every branch of the decision matrix unit-testable with an injected
//!    `now` and zero real credentials/network/disk I/O; only the thin I/O
//!    shell (`collect` itself) touches `creds`/`api`/`store`.
//!
//! The top-level `UsageData::captured_at` this module returns is the newest
//! `captured_at` actually placed into `profiles` this round — never simply
//! `now` — so `scoring::newest_captured_at`'s staleness gate sees the data's
//! true age even when every profile was `ServeStale`-served from a store
//! record whose real capture time is days old (see `track_newest`).
//!
//! `UsageData::any_probe_attempted`/`any_probe_succeeded` (never serialized)
//! tell `transport.rs` whether this round actually reached the network, so
//! its negative-cooldown stamp isn't defeated by `ServeStale` quietly
//! populating `profiles` even when every live attempt failed.
//!
//! The access token never appears in any error string this module produces —
//! `creds::OauthToken`'s `Debug` impl redacts it, and every error path here
//! only ever carries a diagnostic message, never the token value.
//!
//! # Dead-credential warnings (design spec "맛이 간 프로필은 로그인하라고 경고")
//!
//! `creds::CredError::Expired` carries `refresh_alive`, splitting a dead
//! profile in two: **NeedsRefresh** (access token expired, refresh token
//! still good — Claude Code will silently mint a new one on its next run
//! under this profile) and **NeedsLogin** (refresh token dead/absent, or
//! `CredError::NotFound`, or an API 401/403 — nothing but `claude auth
//! login` recovers it). `resolve` turns either case into an [`Attention`]
//! attached to the served `ProfileUsage` (`Resolution::NeedsRefresh`/
//! `Resolution::NeedsLogin`; see their doc comments), never a one-shot
//! `eprintln!` — the warning must survive a `.usage-cache.json` cache hit,
//! where `collect()` doesn't run at all.
//!
//! **Exclusion mechanism.** `scoring::pick_best_at` already builds its
//! candidate list from `data.profiles` MINUS whatever's in `data.errors` (it
//! had to, long before this feature existed, to keep a profile that errored
//! outright from being auto-picked into a broken session). `NeedsLogin`
//! reuses that exact same map — `Resolution::NeedsLogin`'s `errors_msg` goes
//! into `UsageData::errors[name]` — rather than adding a second exclusion
//! path (e.g. `pick_best_at` checking `attention.kind`). `NeedsRefresh`
//! deliberately does NOT touch `errors`: launching under that profile IS the
//! fix, so it must stay pick-able. This also means a `NeedsLogin` profile's
//! row in `report::build_report` cannot rely on `errors` membership to
//! decide what to render — `errors` says "excluded from scoring", not
//! "no numbers" — so `report::join_one` checks `ProfileUsage::attention`
//! FIRST, before ever consulting `errors`, and a profile that carries both
//! (attention AND an errors entry) still renders as exactly one row, numbers
//! and all.

pub mod api;
pub mod creds;
pub mod display;
pub mod statusline;
pub mod store;

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::account::ProfileMap;
use crate::usage::model::{Attention, AttentionKind, ProfileUsage, UsageData, UsageSection};

// ─── env-overridable defaults ───────────────────────────────────────────────

/// How long a store record is served without a live probe. `CSM_USAGE_PROFILE_TTL`.
const DEFAULT_PROFILE_TTL_SECS: i64 = 300;

/// How long a profile is skipped after a 429 before probing again.
/// `CSM_USAGE_RATE_LIMIT_COOLDOWN`.
const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 900;

fn profile_ttl_secs() -> i64 {
    std::env::var("CSM_USAGE_PROFILE_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PROFILE_TTL_SECS)
}

fn rate_limit_cooldown_secs() -> i64 {
    std::env::var("CSM_USAGE_RATE_LIMIT_COOLDOWN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─── pure decision core ─────────────────────────────────────────────────────

/// Whether a store record can be served without a live probe this call.
enum Freshness {
    /// `rec.usage` is within TTL and `force` was not requested — use it as-is.
    Fresh,
    /// The record's `cooldown_until` is still in the future — no probe
    /// attempted; go straight to [`resolve`] with [`Event::InCooldown`].
    Cooldown(i64),
    /// Neither of the above — a live probe is needed.
    NeedsProbe,
}

/// Pure freshness/cooldown check. `captured_at_epoch` is the store record's
/// `captured_at` already parsed to an epoch by the caller (so this function
/// stays a plain comparison, easy to hit every branch of in a test).
fn check_freshness(
    captured_at_epoch: Option<i64>,
    cooldown_until: Option<i64>,
    now_epoch: i64,
    force: bool,
    ttl_secs: i64,
) -> Freshness {
    if !force {
        if let Some(captured) = captured_at_epoch {
            if now_epoch - captured < ttl_secs {
                return Freshness::Fresh;
            }
        }
    }
    if let Some(cooldown) = cooldown_until {
        if cooldown > now_epoch {
            return Freshness::Cooldown(cooldown);
        }
    }
    Freshness::NeedsProbe
}

/// Every terminal event `collect()` can observe for one profile once a live
/// probe is warranted (or skipped because of an active cooldown). The I/O
/// shell's only job is to produce one of these; everything about what
/// happens next lives in [`resolve`].
enum Event {
    /// Still cooling down from a prior 429 — no probe attempted.
    InCooldown { until: i64 },
    /// `creds::lookup` found no credentials at all for this profile's dir.
    NoCreds,
    /// `creds::lookup` found an expired access token. `refresh_alive` decides
    /// NeedsRefresh vs NeedsLogin (see [`creds::CredError::Expired`]);
    /// `expired_at_ms` becomes `Attention::since_epoch`.
    CredsExpired {
        refresh_alive: bool,
        expired_at_ms: i64,
    },
    /// `creds::lookup` failed for any other reason.
    CredsOther(String),
    /// `api::fetch_usage` succeeded and was already mapped. Boxed —
    /// `ProfileUsage` is the largest field in this enum by far, and boxing it
    /// keeps every other (common, no-live-reading) variant small.
    ApiOk(Box<ProfileUsage>),
    /// The API returned 429.
    ApiRateLimited,
    /// The API returned 401/403 — treated identically to `CredsExpired`.
    ApiUnauthorized,
    /// Any other API failure (network, non-2xx, malformed body).
    ApiOther(String),
}

/// What `collect()` should do for one profile, decided purely from whether a
/// stale record exists and the observed [`Event`] — no I/O, no clock reads
/// beyond the `now` the caller already threaded through.
#[derive(Debug)]
enum Resolution {
    /// A live reading was obtained — persist it (source="api") and serve it.
    Persist(Box<ProfileUsage>),
    /// Serve the existing stale record (rolled over for expired sections);
    /// `hint`, if present, is a stderr diagnostic for the caller to print.
    /// Never produced for a credential-expiry event any more — see
    /// [`Resolution::NeedsRefresh`]/[`Resolution::NeedsLogin`].
    ServeStale(Option<String>),
    /// No usable data — record this message in `UsageData::errors`.
    Fail(String),
    /// Access token expired but the refresh token is alive — Claude Code
    /// will silently refresh on its next run under this profile. NEVER
    /// recorded in `errors` (design spec: "후보 유지 — 그 프로필로 띄우는
    /// 것이 곧 해결책") — the profile stays a scoring candidate, since its
    /// stale numbers (if any) are still meaningful. `apply_resolution` serves
    /// the existing stale record (rolled over), or — if there is none — a
    /// bare `ProfileUsage` carrying only `attention`, so the table/footer can
    /// still surface the warning for a profile that has never been
    /// successfully probed.
    NeedsRefresh { attention: Attention },
    /// Refresh token dead/absent, `NotFound`, or an API 401/403. `errors_msg`
    /// is recorded in `UsageData::errors[name]` — see the module doc's
    /// "exclusion mechanism" note for why that (rather than a new field
    /// `scoring::pick_best_at` would have to check) is what keeps this
    /// profile out of auto-pick. `apply_resolution` still attaches `usage`
    /// (stale-if-any, else bare) the same way as `NeedsRefresh` so the table
    /// keeps showing last-known numbers alongside the LOGIN REQUIRED status.
    NeedsLogin {
        errors_msg: String,
        attention: Attention,
    },
}

/// The pure branch table. `has_stale` = the profile already has a stored
/// `usage` to fall back on (read by `apply_resolution`, not here). `name` is
/// this profile's registry key (used for the `NeedsRefresh` action, `csm
/// --profile <name>`); `dir_display` is its config dir (used for the
/// `NeedsLogin` action, `CLAUDE_CONFIG_DIR=<dir> claude auth login`, and the
/// `NoCreds` message). Returns the [`Resolution`] plus whether the caller
/// should stamp a rate-limit cooldown — the pure fn decides *that* a cooldown
/// applies, never the timestamp itself (that needs `now` + the configured
/// cooldown duration, which belong to the I/O shell).
///
/// Credential-trouble events never look at `has_stale` — every one of them
/// becomes [`Resolution::NeedsLogin`]/[`Resolution::NeedsRefresh`]
/// unconditionally (design spec "맛이 간 프로필은 로그인하라고 경고" §자격증명
/// 상태 3분류); `apply_resolution` is what decides, from `has_stale`, whether
/// the row it builds carries real numbers or is bare.
fn resolve(has_stale: bool, event: Event, name: &str, dir_display: &str) -> (Resolution, bool) {
    match event {
        Event::InCooldown { until } => {
            if has_stale {
                (Resolution::ServeStale(None), false)
            } else {
                (
                    Resolution::Fail(format!("rate limited until {until}")),
                    false,
                )
            }
        }
        Event::NoCreds => (
            Resolution::NeedsLogin {
                errors_msg: format!(
                    "not logged in (no Claude Code credentials for {dir_display}) — login required"
                ),
                attention: Attention {
                    kind: AttentionKind::NeedsLogin,
                    message: "not logged in".to_string(),
                    action: login_action(dir_display),
                    since_epoch: None,
                },
            },
            false,
        ),
        Event::CredsExpired {
            refresh_alive: true,
            expired_at_ms,
        } => (
            Resolution::NeedsRefresh {
                attention: Attention {
                    kind: AttentionKind::NeedsRefresh,
                    message: "access token expired".to_string(),
                    action: refresh_action(name),
                    since_epoch: Some(expired_at_ms / 1000),
                },
            },
            false,
        ),
        Event::CredsExpired {
            refresh_alive: false,
            expired_at_ms,
        } => (
            Resolution::NeedsLogin {
                errors_msg: "credentials expired — login required".to_string(),
                attention: Attention {
                    kind: AttentionKind::NeedsLogin,
                    message: "credentials expired".to_string(),
                    action: login_action(dir_display),
                    since_epoch: Some(expired_at_ms / 1000),
                },
            },
            false,
        ),
        // API 401/403: the access token we sent was rejected server-side.
        // We have no local expiry instant for it (our own clock thought it
        // was still live), and no way to tell whether the refresh token is
        // still good — treated conservatively as NeedsLogin (design spec:
        // "refresh 생존 여부를 알 수 없으므로 NeedsLogin 으로 취급").
        Event::ApiUnauthorized => (
            Resolution::NeedsLogin {
                errors_msg: "credentials rejected by the server — login required".to_string(),
                attention: Attention {
                    kind: AttentionKind::NeedsLogin,
                    message: "credentials rejected by the server".to_string(),
                    action: login_action(dir_display),
                    since_epoch: None,
                },
            },
            false,
        ),
        Event::CredsOther(msg) | Event::ApiOther(msg) => {
            if has_stale {
                (Resolution::ServeStale(Some(msg)), false)
            } else {
                (Resolution::Fail(msg), false)
            }
        }
        Event::ApiOk(usage) => (Resolution::Persist(usage), false),
        Event::ApiRateLimited => {
            if has_stale {
                (Resolution::ServeStale(None), true)
            } else {
                (Resolution::Fail("rate limited".to_string()), true)
            }
        }
    }
}

/// The exact, copy-pasteable command that resolves a NeedsLogin profile.
fn login_action(dir_display: &str) -> String {
    format!("CLAUDE_CONFIG_DIR={dir_display} claude auth login")
}

/// The exact, copy-pasteable command that resolves a NeedsRefresh profile —
/// `--profile` (confirmed at `cli/parser.rs`) launches under `name`, which is
/// enough for Claude Code to silently mint a fresh access token.
fn refresh_action(name: &str) -> String {
    format!("csm --profile {name}")
}

/// Roll over any section whose `resets_at` has already passed `now` to
/// `{pct: 0, resets: None, resets_at: None}` — a window rollover means usage
/// resets to 0, and a stale reading must not keep reporting last window's
/// percentage forever. `captured_at`/`source` are left untouched so scoring's
/// "how stale is this really" check still sees the true capture time.
fn roll_over_expired_sections(usage: &ProfileUsage, now: DateTime<Utc>) -> ProfileUsage {
    let mut out = usage.clone();
    let now_epoch = now.timestamp();
    for s in [&mut out.session, &mut out.week_all, &mut out.week_fable]
        .into_iter()
        .flatten()
    {
        if let Some(epoch) = s.resets_at {
            if epoch < now_epoch {
                *s = UsageSection {
                    pct: 0,
                    resets: None,
                    resets_at: None,
                };
            }
        }
    }
    out
}

// ─── I/O shell ──────────────────────────────────────────────────────────────

/// Collect (or serve cached) usage for every profile in `profiles`.
///
/// `now` is threaded through explicitly (rather than read internally) so
/// callers get one consistent instant across every profile in the pass, and
/// so the whole thing is exercisable deterministically in tests that inject
/// a fixed `now` alongside a temp store dir — though the dominant coverage is
/// on the pure [`resolve`]/[`check_freshness`]/[`roll_over_expired_sections`]
/// functions above, since `collect` itself is the thin shell wiring them to
/// real credentials/network/disk.
pub fn collect(profiles: &ProfileMap, now: DateTime<Utc>, force: bool) -> UsageData {
    let ttl_secs = profile_ttl_secs();
    let rl_cooldown_secs = rate_limit_cooldown_secs();
    let base = api::resolve_base();
    if base != api::DEFAULT_BASE {
        // A redirected token destination must never be silent — every
        // profile's live OAuth access token is about to be sent to `base` as
        // a Bearer credential.
        eprintln!(
            "csm: warning: CSM_USAGE_API_BASE overrides the usage API base to \
             {base} — every configured profile's OAuth token will be sent \
             there as a Bearer credential; verify this host is trusted"
        );
    }
    let now_epoch = now.timestamp();

    let mut out = UsageData {
        captured_at: None,
        profiles: HashMap::new(),
        errors: None,
        ..Default::default()
    };
    let mut errors: HashMap<String, String> = HashMap::new();
    // Newest captured_at actually placed into `out.profiles`, tracked as
    // (epoch, original-string) so the top-level field can be set from it
    // after the loop instead of unconditionally stamping `now` — see the
    // module doc and `track_newest`.
    let mut newest_captured: Option<(i64, String)> = None;
    let mut any_probe_attempted = false;
    let mut any_probe_succeeded = false;

    for name in profiles.names_sorted() {
        let Some(dir_str) = profiles.get(name) else {
            continue;
        };
        // `paths::usage_store`'s documented precondition: `profile` must be
        // validated before it reaches any store call. `profiles.json` is
        // hand-editable/template-rendered, so a key is not guaranteed valid
        // even though `cas add`/`set` themselves only ever write valid ones.
        if !ProfileMap::is_valid_name(name) {
            errors.insert(name.to_string(), "invalid profile name".to_string());
            continue;
        }
        let dir = Path::new(dir_str);
        let rec = store::load(name);
        let has_stale = rec.as_ref().and_then(|r| r.usage.as_ref()).is_some();
        // Gate freshness on the last *live api probe* time, not the record's
        // blanket `captured_at` (which a statusline capture also refreshes —
        // see the module doc's point 1).
        let api_captured_at_epoch = rec
            .as_ref()
            .and_then(|r| r.api_captured_at.as_deref())
            .and_then(parse_rfc3339_epoch);
        let cooldown_until = rec.as_ref().and_then(|r| r.cooldown_until);

        match check_freshness(
            api_captured_at_epoch,
            cooldown_until,
            now_epoch,
            force,
            ttl_secs,
        ) {
            Freshness::Fresh => {
                if let Some(usage) = rec.as_ref().and_then(|r| r.usage.clone()) {
                    // Roll over even on the cache-hit path — an expired
                    // resets_at must decay to 0 whether we just probed or are
                    // serving a still-within-TTL cache hit.
                    let rolled = roll_over_expired_sections(&usage, now);
                    track_newest(&mut newest_captured, rolled.captured_at.as_deref());
                    out.profiles.insert(name.to_string(), rolled);
                }
                continue;
            }
            Freshness::Cooldown(until) => {
                let (resolution, _) =
                    resolve(has_stale, Event::InCooldown { until }, name, dir_str);
                apply_resolution(
                    name,
                    &rec,
                    resolution,
                    None,
                    now,
                    &mut out,
                    &mut errors,
                    &mut newest_captured,
                );
            }
            Freshness::NeedsProbe => {
                any_probe_attempted = true;
                let event = probe(dir, now, &base);
                if matches!(event, Event::ApiOk(_)) {
                    any_probe_succeeded = true;
                }
                let (resolution, needs_cooldown) = resolve(has_stale, event, name, dir_str);
                let cooldown = needs_cooldown.then_some(now_epoch + rl_cooldown_secs);
                apply_resolution(
                    name,
                    &rec,
                    resolution,
                    cooldown,
                    now,
                    &mut out,
                    &mut errors,
                    &mut newest_captured,
                );
            }
        }
    }

    if !errors.is_empty() {
        out.errors = Some(errors);
    }
    out.captured_at = newest_captured.map(|(_, s)| s);
    out.any_probe_attempted = any_probe_attempted;
    out.any_probe_succeeded = any_probe_succeeded;
    out
}

/// Run the live probe: credential lookup, then (on success) the API call.
/// Collapses every real failure mode into an [`Event`] — the only place in
/// this module that touches `creds`/`api`.
fn probe(dir: &Path, now: DateTime<Utc>, base: &str) -> Event {
    let token = match creds::lookup(dir, now) {
        Ok(t) => t,
        Err(creds::CredError::NotFound) => return Event::NoCreds,
        Err(creds::CredError::Expired {
            refresh_alive,
            expired_at_ms,
        }) => {
            return Event::CredsExpired {
                refresh_alive,
                expired_at_ms,
            }
        }
        Err(creds::CredError::Unreadable(msg)) => return Event::CredsOther(msg),
    };
    match api::fetch_usage(&token.access_token, base) {
        Ok(u) => Event::ApiOk(Box::new(api::to_profile_usage(&u, now))),
        Err(api::ApiError::RateLimited) => Event::ApiRateLimited,
        Err(api::ApiError::Unauthorized) => Event::ApiUnauthorized,
        Err(e) => Event::ApiOther(e.to_string()),
    }
}

/// Apply a [`Resolution`] to `out`/`errors`, and persist to the store
/// whatever the resolution implies (a fresh reading, or a rate-limit
/// cooldown, or both — never neither, since `resolve` only ever returns a
/// `Some` cooldown alongside `ApiRateLimited`, which never also returns
/// `Persist`).
#[allow(clippy::too_many_arguments)]
fn apply_resolution(
    name: &str,
    rec: &Option<store::StoreRecord>,
    resolution: Resolution,
    set_cooldown: Option<i64>,
    now: DateTime<Utc>,
    out: &mut UsageData,
    errors: &mut HashMap<String, String>,
    newest_captured: &mut Option<(i64, String)>,
) {
    match resolution {
        Resolution::Persist(usage) => {
            let new_rec = store::StoreRecord {
                profile: name.to_string(),
                captured_at: usage.captured_at.clone(),
                source: usage.source.clone(),
                // This IS a fresh live api probe — record it as the new
                // `api_captured_at` so the freshness gate above sees it.
                api_captured_at: usage.captured_at.clone(),
                cooldown_until: None,
                usage: Some((*usage).clone()),
            };
            if let Err(e) = store::save(name, &new_rec) {
                eprintln!("csm: warning: could not write usage store for {name}: {e}");
            }
            track_newest(newest_captured, usage.captured_at.as_deref());
            out.profiles.insert(name.to_string(), *usage);
        }
        Resolution::ServeStale(hint) => {
            if let Some(usage) = rec.as_ref().and_then(|r| r.usage.clone()) {
                let rolled = roll_over_expired_sections(&usage, now);
                // `captured_at` is the rolled record's OWN (preserved, true)
                // capture time — never `now` — so scoring's staleness gate
                // sees how old this data really is instead of always seeing
                // "just collected" (see the module doc).
                track_newest(newest_captured, rolled.captured_at.as_deref());
                out.profiles.insert(name.to_string(), rolled);
            }
            if let Some(h) = hint {
                eprintln!("csm: usage ({name}): {h}");
            }
        }
        Resolution::Fail(msg) => {
            errors.insert(name.to_string(), msg);
        }
        Resolution::NeedsRefresh { attention } => {
            attach_attention(name, rec, attention, now, out, newest_captured);
        }
        Resolution::NeedsLogin {
            errors_msg,
            attention,
        } => {
            attach_attention(name, rec, attention, now, out, newest_captured);
            errors.insert(name.to_string(), errors_msg);
        }
    }

    if let Some(until) = set_cooldown {
        if let Err(e) = store::set_cooldown(name, until) {
            eprintln!("csm: warning: could not write usage cooldown for {name}: {e}");
        }
    }
}

/// Shared by [`Resolution::NeedsRefresh`]/[`Resolution::NeedsLogin`]: build
/// the `ProfileUsage` `out.profiles[name]` gets — the existing stale record
/// (rolled over for any expired section), or, when there is none, a bare
/// `ProfileUsage::default()` — and attach `attention` to it either way. A
/// bare row (no prior probe ever succeeded) still needs to land in
/// `out.profiles` so the table/footer/`attention_lines` have *something* to
/// key the warning off — see the module doc and the design spec's
/// "profiles[name] 에 남겨" note.
///
/// Deliberately does NOT call `store::save` — unlike `Resolution::Persist`,
/// neither NeedsRefresh nor NeedsLogin represents a fresh live reading, so
/// there is nothing new worth persisting to the per-profile store; the next
/// `collect()` call recomputes `attention` from scratch from a live
/// `creds::lookup`/`api::fetch_usage` attempt regardless. The warning still
/// survives the positive cache because `transport.rs::fetch_with` serializes
/// this round's whole in-memory `UsageData` — attention included — to
/// `.usage-cache.json` after `collect()` returns.
fn attach_attention(
    name: &str,
    rec: &Option<store::StoreRecord>,
    attention: Attention,
    now: DateTime<Utc>,
    out: &mut UsageData,
    newest_captured: &mut Option<(i64, String)>,
) {
    let mut usage = rec
        .as_ref()
        .and_then(|r| r.usage.clone())
        .map(|u| roll_over_expired_sections(&u, now))
        .unwrap_or_default();
    usage.attention = Some(attention);
    track_newest(newest_captured, usage.captured_at.as_deref());
    out.profiles.insert(name.to_string(), usage);
}

/// Fold `captured_at` (an RFC-3339 string, if it parses) into `newest`,
/// keeping whichever of the two is later. Used to derive `UsageData`'s
/// top-level `captured_at` from what was actually served for each profile
/// this round, rather than stamping `now` unconditionally (see the module
/// doc + `collect`).
fn track_newest(newest: &mut Option<(i64, String)>, captured_at: Option<&str>) {
    let Some(s) = captured_at else { return };
    let Some(epoch) = parse_rfc3339_epoch(s) else {
        return;
    };
    let replace = match newest {
        Some((cur, _)) => epoch > *cur,
        None => true,
    };
    if replace {
        *newest = Some((epoch, s.to_string()));
    }
}

fn parse_rfc3339_epoch(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// How old, in seconds relative to `now`, the **most stale** of `data`'s
/// per-profile readings is — the true age signal `csm usage`'s "⚠ usage data
/// is Nm old" banner should key off.
///
/// `write_positive_cache` refreshes `.usage-cache.json`'s mtime on every
/// `fetch_with` call that isn't a total failure — including a round where
/// every profile was `ServeStale`-served from a days-old store record — so
/// the cache file's mtime is no longer a valid staleness proxy once local
/// collection can legitimately "succeed" while serving old data. This reads
/// the actual `captured_at` this module stamped on each served profile
/// instead.
///
/// A profile with no parseable `captured_at` (never successfully captured —
/// a bare `NeedsLogin`/`NeedsRefresh` row with no store record, a record
/// predating the field, or a foreign `CSM_USAGE_CMD` payload that omits it)
/// renders as dashes in the table: there is no "last-known" data being shown
/// for it, so — unlike a profile whose numbers ARE on screen and merely old —
/// it contributes nothing to "how stale is the data we're showing" and is
/// skipped rather than counted as maximally stale. Returns `None` when NO
/// profile has a parseable `captured_at` (nothing displayed anywhere has a
/// known age — e.g. every profile is a fresh registry entry that has never
/// logged in) or `data.profiles` is empty; otherwise the max age among only
/// the profiles that do have one.
pub fn oldest_profile_age_secs(data: &UsageData, now: DateTime<Utc>) -> Option<u64> {
    let now_epoch = now.timestamp();
    let mut oldest: Option<u64> = None;
    for pu in data.profiles.values() {
        if let Some(epoch) = pu.captured_at.as_deref().and_then(parse_rfc3339_epoch) {
            let age = (now_epoch - epoch).max(0) as u64;
            oldest = Some(oldest.map_or(age, |cur| cur.max(age)));
        }
    }
    oldest
}

// ─── statusline capture (`csm usage capture`) ──────────────────────────────

/// Merge a statusLine stdin payload into the active profile's store record.
///
/// Returns `Ok(true)` when the store was written, `Ok(false)` for every
/// legitimate no-op (payload has no usable `rate_limits`, `CLAUDE_CONFIG_DIR`
/// unset, profile name can't be resolved/validated, or the throttle below
/// fired) — never an `Err` for those; `Err` is reserved for a malformed
/// payload (`raw` isn't even valid JSON).
///
/// Throttle: if the current record's source is already `"statusline"`, was
/// captured under 10s ago, and this payload would produce identical
/// session/week_all percentages, skip the write — the statusLine command can
/// fire roughly once a second, and there is nothing to gain from rewriting
/// the file every tick when nothing changed.
pub fn record_statusline_payload(raw: &str) -> Result<Option<StatuslineCapture>, LocalError> {
    let payload: statusline::StatuslinePayload = serde_json::from_str(raw)?;

    let Some(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    let Some(name) = resolve_profile_name(&dir) else {
        return Ok(None);
    };
    if !ProfileMap::is_valid_name(&name) {
        return Ok(None);
    }

    let now = Utc::now();
    let prior_rec = store::load(&name);
    let prior_usage = prior_rec.as_ref().and_then(|r| r.usage.as_ref());

    let Some(new_usage) = statusline::to_profile_usage(&payload, prior_usage, now) else {
        return Ok(None);
    };

    if should_throttle(prior_usage, &new_usage, now) {
        // Throttled: the store already holds this reading (written within
        // the last 10 s); the reading itself is still current.
        return Ok(Some(StatuslineCapture {
            profile_dir: dir,
            usage: new_usage,
        }));
    }

    let new_rec = build_statusline_record(&name, new_usage, prior_rec.as_ref());
    store::save(&name, &new_rec)?;
    Ok(Some(StatuslineCapture {
        profile_dir: dir,
        usage: new_rec.usage.expect("record built from Some(usage)"),
    }))
}

/// What one statusline tick learned, handed back to the caller so the
/// limit-switch trigger (`hook::run_from_statusline`) can act on the same
/// merged reading the store just received instead of re-reading a cache
/// that may be up to a minute behind. `None` from
/// [`record_statusline_payload`] means the tick carried nothing usable.
#[derive(Debug, Clone)]
pub struct StatuslineCapture {
    /// `CLAUDE_CONFIG_DIR` as the tick saw it — the owning profile's dir,
    /// the same value `csm hook --owner` receives.
    pub profile_dir: String,
    /// This tick's `session`/`week_all` merged with the store's carried-forward
    /// `week_fable`/`week_model_label`. Returned whether or not the
    /// identical-within-10-s throttle skipped the store write.
    pub usage: ProfileUsage,
}

/// Build the `StoreRecord` a statusline capture writes. Pure (no I/O) so the
/// one rule that matters here — `api_captured_at` carries the PRIOR record's
/// value forward unchanged, never `now` — is unit-testable directly.
///
/// A statusline capture is NOT a live api probe. If this instead bumped
/// `api_captured_at` to `now` (the way `captured_at` does), `collect()`'s
/// freshness gate — keyed on `api_captured_at` precisely to avoid this —
/// would treat the profile as freshly api-probed and never re-probe,
/// freezing `week_fable`/`week_model_label` (only an api probe can refresh
/// them; statusLine stdin never carries per-model-tier data) for as long as
/// the statusline keeps writing.
fn build_statusline_record(
    name: &str,
    new_usage: ProfileUsage,
    prior_rec: Option<&store::StoreRecord>,
) -> store::StoreRecord {
    store::StoreRecord {
        profile: name.to_string(),
        captured_at: new_usage.captured_at.clone(),
        source: new_usage.source.clone(),
        api_captured_at: prior_rec.and_then(|r| r.api_captured_at.clone()),
        cooldown_until: prior_rec.and_then(|r| r.cooldown_until),
        usage: Some(new_usage),
    }
}

/// `true` when the prior record is itself a recent (`< 10s`) statusline
/// capture with identical session/week_all percentages to `new_usage` — see
/// [`record_statusline_payload`]'s throttle note.
fn should_throttle(
    prior: Option<&ProfileUsage>,
    new_usage: &ProfileUsage,
    now: DateTime<Utc>,
) -> bool {
    let Some(prior) = prior else {
        return false;
    };
    if prior.source.as_deref() != Some("statusline") {
        return false;
    }
    let Some(captured) = prior.captured_at.as_deref().and_then(parse_rfc3339_epoch) else {
        return false;
    };
    if now.timestamp() - captured >= 10 {
        return false;
    }
    pct(&prior.session) == pct(&new_usage.session)
        && pct(&prior.week_all) == pct(&new_usage.week_all)
}

fn pct(section: &Option<UsageSection>) -> Option<i64> {
    section.as_ref().map(|s| s.pct)
}

/// Resolve a `CLAUDE_CONFIG_DIR` value to a profile name: reverse-lookup in
/// [`ProfileMap`] (comparing paths as strings with trailing separators
/// trimmed), else the directory's basename with a `.claude.` prefix stripped
/// (basename unchanged if it has no such prefix).
fn resolve_profile_name(dir: &str) -> Option<String> {
    let trimmed_dir = trim_trailing_sep(dir);
    if let Ok(pm) = ProfileMap::load() {
        for (name, d) in pm.iter() {
            if trim_trailing_sep(d) == trimmed_dir {
                return Some(name.to_string());
            }
        }
    }
    let base = Path::new(dir).file_name()?.to_str()?;
    Some(base.strip_prefix(".claude.").unwrap_or(base).to_string())
}

fn trim_trailing_sep(s: &str) -> &str {
    s.trim_end_matches(['/', '\\'])
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::model::UsageSection;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    // ── check_freshness ─────────────────────────────────────────────────────

    #[test]
    fn fresh_within_ttl_and_not_forced() {
        let now_epoch = now().timestamp();
        let captured = now_epoch - 100; // 100s old, TTL 300
        assert!(matches!(
            check_freshness(Some(captured), None, now_epoch, false, 300),
            Freshness::Fresh
        ));
    }

    #[test]
    fn stale_past_ttl_needs_probe_when_no_cooldown() {
        let now_epoch = now().timestamp();
        let captured = now_epoch - 400; // past 300s TTL
        assert!(matches!(
            check_freshness(Some(captured), None, now_epoch, false, 300),
            Freshness::NeedsProbe
        ));
    }

    #[test]
    fn force_ignores_freshness_but_still_honors_cooldown() {
        let now_epoch = now().timestamp();
        let captured = now_epoch - 10; // would be Fresh without force
        assert!(matches!(
            check_freshness(Some(captured), None, now_epoch, true, 300),
            Freshness::NeedsProbe
        ));
        let cooldown_until = now_epoch + 500;
        assert!(matches!(
            check_freshness(Some(captured), Some(cooldown_until), now_epoch, true, 300),
            Freshness::Cooldown(u) if u == cooldown_until
        ));
    }

    #[test]
    fn no_record_needs_probe() {
        let now_epoch = now().timestamp();
        assert!(matches!(
            check_freshness(None, None, now_epoch, false, 300),
            Freshness::NeedsProbe
        ));
    }

    #[test]
    fn expired_cooldown_needs_probe() {
        let now_epoch = now().timestamp();
        let captured = now_epoch - 400; // stale
        let cooldown_until = now_epoch - 10; // already passed
        assert!(matches!(
            check_freshness(Some(captured), Some(cooldown_until), now_epoch, false, 300),
            Freshness::NeedsProbe
        ));
    }

    #[test]
    fn active_cooldown_after_ttl_expiry_is_cooldown() {
        let now_epoch = now().timestamp();
        let captured = now_epoch - 400;
        let cooldown_until = now_epoch + 500;
        assert!(matches!(
            check_freshness(Some(captured), Some(cooldown_until), now_epoch, false, 300),
            Freshness::Cooldown(u) if u == cooldown_until
        ));
    }

    // ── resolve ──────────────────────────────────────────────────────────────

    #[test]
    fn resolve_in_cooldown_with_stale_serves_stale() {
        let (res, needs_cooldown) = resolve(true, Event::InCooldown { until: 123 }, "home", "/x");
        assert!(matches!(res, Resolution::ServeStale(None)));
        assert!(!needs_cooldown);
    }

    #[test]
    fn resolve_in_cooldown_without_stale_fails_with_until() {
        let (res, _) = resolve(false, Event::InCooldown { until: 999 }, "home", "/x");
        match res {
            Resolution::Fail(msg) => assert!(msg.contains("999"), "{msg}"),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn resolve_no_creds_is_needs_login_with_dir_in_errors_msg() {
        let (res, needs_cooldown) =
            resolve(false, Event::NoCreds, "work", "/Users/example/.claude.work");
        assert!(!needs_cooldown);
        match res {
            Resolution::NeedsLogin {
                errors_msg,
                attention,
            } => {
                assert!(
                    errors_msg.contains("/Users/example/.claude.work"),
                    "{errors_msg}"
                );
                assert!(errors_msg.contains("not logged in"), "{errors_msg}");
                assert!(errors_msg.contains("login required"), "{errors_msg}");
                assert_eq!(attention.kind, AttentionKind::NeedsLogin);
                assert_eq!(attention.message, "not logged in");
                assert_eq!(
                    attention.action,
                    "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"
                );
                assert!(attention.since_epoch.is_none(), "no known expiry instant");
            }
            other => panic!("expected NeedsLogin, got {other:?}"),
        }
    }

    #[test]
    fn resolve_no_creds_is_needs_login_regardless_of_stale() {
        // NoCreds is unconditionally NeedsLogin — `has_stale` only changes
        // what `apply_resolution` attaches the attention to, not the verdict.
        let (res, _) = resolve(true, Event::NoCreds, "work", "/x");
        assert!(matches!(res, Resolution::NeedsLogin { .. }));
    }

    #[test]
    fn resolve_creds_expired_refresh_alive_is_needs_refresh_never_in_errors() {
        let (res, needs_cooldown) = resolve(
            true,
            Event::CredsExpired {
                refresh_alive: true,
                expired_at_ms: 1_756_000_000_000,
            },
            "home",
            "/Users/example/.claude.home",
        );
        assert!(!needs_cooldown);
        match res {
            Resolution::NeedsRefresh { attention } => {
                assert_eq!(attention.kind, AttentionKind::NeedsRefresh);
                assert_eq!(attention.message, "access token expired");
                assert_eq!(attention.action, "csm --profile home");
                assert_eq!(attention.since_epoch, Some(1_756_000_000));
            }
            other => panic!("expected NeedsRefresh, got {other:?}"),
        }
    }

    #[test]
    fn resolve_creds_expired_refresh_alive_ignores_has_stale() {
        // NeedsRefresh is produced whether or not a stale record exists —
        // `apply_resolution` (not `resolve`) decides whether the row it
        // builds carries real numbers or is bare.
        let event = || Event::CredsExpired {
            refresh_alive: true,
            expired_at_ms: 1,
        };
        assert!(matches!(
            resolve(true, event(), "home", "/x").0,
            Resolution::NeedsRefresh { .. }
        ));
        assert!(matches!(
            resolve(false, event(), "home", "/x").0,
            Resolution::NeedsRefresh { .. }
        ));
    }

    #[test]
    fn resolve_creds_expired_refresh_dead_is_needs_login() {
        let (res, _) = resolve(
            true,
            Event::CredsExpired {
                refresh_alive: false,
                expired_at_ms: 1_756_000_000_000,
            },
            "work",
            "/Users/example/.claude.work",
        );
        match res {
            Resolution::NeedsLogin {
                errors_msg,
                attention,
            } => {
                assert!(errors_msg.contains("login required"), "{errors_msg}");
                assert_eq!(attention.kind, AttentionKind::NeedsLogin);
                assert_eq!(attention.message, "credentials expired");
                assert_eq!(
                    attention.action,
                    "CLAUDE_CONFIG_DIR=/Users/example/.claude.work claude auth login"
                );
                assert_eq!(attention.since_epoch, Some(1_756_000_000));
            }
            other => panic!("expected NeedsLogin, got {other:?}"),
        }
    }

    #[test]
    fn resolve_api_unauthorized_is_needs_login_conservatively() {
        // 401/403: refresh-liveness is unknown, so this is NeedsLogin — see
        // the module doc's "Dead-credential warnings" section.
        let (with_stale, _) = resolve(true, Event::ApiUnauthorized, "work", "/x");
        let (without_stale, _) = resolve(false, Event::ApiUnauthorized, "work", "/x");
        for res in [with_stale, without_stale] {
            match res {
                Resolution::NeedsLogin { attention, .. } => {
                    assert_eq!(attention.kind, AttentionKind::NeedsLogin);
                    assert!(
                        attention.since_epoch.is_none(),
                        "no local expiry instant known"
                    );
                }
                other => panic!("expected NeedsLogin, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_creds_other_and_api_other_carry_message_through() {
        let (r1, _) = resolve(true, Event::CredsOther("boom".into()), "home", "/x");
        assert!(matches!(r1, Resolution::ServeStale(Some(m)) if m == "boom"));
        let (r2, _) = resolve(false, Event::ApiOther("kaboom".into()), "home", "/x");
        assert!(matches!(r2, Resolution::Fail(m) if m == "kaboom"));
    }

    #[test]
    fn resolve_api_ok_always_persists_regardless_of_stale() {
        let usage = ProfileUsage {
            source: Some("api".to_string()),
            ..Default::default()
        };
        let (res, needs_cooldown) =
            resolve(true, Event::ApiOk(Box::new(usage.clone())), "home", "/x");
        assert!(matches!(res, Resolution::Persist(_)));
        assert!(!needs_cooldown);
    }

    #[test]
    fn resolve_rate_limited_with_stale_serves_stale_and_signals_cooldown() {
        let (res, needs_cooldown) = resolve(true, Event::ApiRateLimited, "home", "/x");
        assert!(matches!(res, Resolution::ServeStale(None)));
        assert!(needs_cooldown);
    }

    #[test]
    fn resolve_rate_limited_without_stale_fails_and_signals_cooldown() {
        let (res, needs_cooldown) = resolve(false, Event::ApiRateLimited, "home", "/x");
        assert!(matches!(res, Resolution::Fail(_)));
        assert!(needs_cooldown);
    }

    // ── track_newest ─────────────────────────────────────────────────────────

    #[test]
    fn track_newest_starts_none_takes_first_value() {
        let mut newest: Option<(i64, String)> = None;
        track_newest(&mut newest, Some("2026-09-01T00:00:00Z"));
        assert_eq!(newest.unwrap().1, "2026-09-01T00:00:00Z");
    }

    #[test]
    fn track_newest_keeps_later_of_two() {
        let mut newest: Option<(i64, String)> = None;
        track_newest(&mut newest, Some("2026-08-30T00:00:00Z"));
        track_newest(&mut newest, Some("2026-09-01T00:00:00Z"));
        assert_eq!(newest.as_ref().unwrap().1, "2026-09-01T00:00:00Z");
        // An older value arriving later must NOT overwrite the newer one.
        track_newest(&mut newest, Some("2026-08-01T00:00:00Z"));
        assert_eq!(newest.unwrap().1, "2026-09-01T00:00:00Z");
    }

    #[test]
    fn track_newest_ignores_none_and_unparseable() {
        let mut newest: Option<(i64, String)> = None;
        track_newest(&mut newest, None);
        assert!(newest.is_none());
        track_newest(&mut newest, Some("not a timestamp"));
        assert!(newest.is_none());
    }

    // ── apply_resolution: captured_at tracking ──────────────────────────────
    //
    // Regression coverage for the finding that `collect()` used to stamp
    // `out.captured_at = Some(now)` unconditionally, which made
    // `scoring::newest_captured_at` always see "just collected" even when
    // every profile was `ServeStale`-served from a days-old record — silently
    // defeating the 1800s staleness gate for the expired-token/offline case.

    #[test]
    fn apply_resolution_needs_login_tracks_the_stale_records_own_captured_at_not_now() {
        let now_ = now(); // 2026-09-02T00:00:00Z
        let old_captured = "2026-08-30T00:00:00Z".to_string();
        let rec = Some(store::StoreRecord {
            profile: "work".to_string(),
            captured_at: Some(old_captured.clone()),
            source: Some("api".to_string()),
            api_captured_at: Some(old_captured.clone()),
            cooldown_until: None,
            usage: Some(ProfileUsage {
                captured_at: Some(old_captured.clone()),
                session: Some(UsageSection {
                    pct: 12,
                    resets: None,
                    resets_at: None,
                }),
                week_all: Some(UsageSection {
                    pct: 12,
                    resets: None,
                    resets_at: None,
                }),
                week_fable: None,
                week_model_label: None,
                session_stats: vec![],
                source: Some("api".to_string()),
                attention: None,
            }),
        });
        let (resolution, _) = resolve(
            true,
            Event::CredsExpired {
                refresh_alive: false,
                expired_at_ms: 1,
            },
            "work",
            "/x",
        );

        let mut out = UsageData {
            captured_at: None,
            profiles: HashMap::new(),
            errors: None,
            ..Default::default()
        };
        let mut errors: HashMap<String, String> = HashMap::new();
        let mut newest_captured: Option<(i64, String)> = None;

        apply_resolution(
            "work",
            &rec,
            resolution,
            None,
            now_,
            &mut out,
            &mut errors,
            &mut newest_captured,
        );

        // The served profile IS present (NeedsLogin still attaches the stale
        // record's numbers) ...
        assert!(out.profiles.contains_key("work"));
        // ... with the actual stale percentages intact — a LOGIN REQUIRED
        // row must still show last-known numbers, not dashes (bug B) ...
        assert_eq!(
            out.profiles["work"].session.as_ref().map(|s| s.pct),
            Some(12)
        );
        assert_eq!(
            out.profiles["work"].week_all.as_ref().map(|s| s.pct),
            Some(12)
        );
        // ... carrying the attention that got it here ...
        assert!(out.profiles["work"].attention.is_some());
        // ... and is ALSO excluded from scoring via `errors` (the chosen
        // exclusion mechanism — see the module doc).
        assert!(errors.contains_key("work"));
        // ... but the tracked captured_at is the record's OWN old timestamp,
        // never `now_` — this is what `collect()` now sets `out.captured_at`
        // from, restoring the staleness gate.
        assert_eq!(
            newest_captured.map(|(_, s)| s),
            Some(old_captured),
            "attach_attention must track the stale record's true captured_at, not now"
        );
    }

    #[test]
    fn apply_resolution_needs_login_with_no_stale_record_inserts_bare_profile_usage() {
        // NotFound (never logged in): no prior record at all. The profile
        // must still land in `out.profiles` (bare — no sections) so the
        // table/footer have something to key the warning off, per the
        // module doc's "profiles[name] 에 남겨" note.
        let (resolution, _) = resolve(false, Event::NoCreds, "work", "/Users/example/.claude.work");

        let mut out = UsageData::default();
        let mut errors: HashMap<String, String> = HashMap::new();
        let mut newest_captured: Option<(i64, String)> = None;

        apply_resolution(
            "work",
            &None,
            resolution,
            None,
            now(),
            &mut out,
            &mut errors,
            &mut newest_captured,
        );

        let pu = out.profiles.get("work").expect("bare row must be inserted");
        assert!(pu.session.is_none());
        assert!(pu.week_all.is_none());
        let att = pu.attention.as_ref().expect("attention must be attached");
        assert_eq!(att.kind, AttentionKind::NeedsLogin);
        assert!(errors.contains_key("work"));
    }

    #[test]
    fn apply_resolution_needs_refresh_with_no_stale_record_never_touches_errors() {
        let (resolution, _) = resolve(
            false,
            Event::CredsExpired {
                refresh_alive: true,
                expired_at_ms: 1,
            },
            "home",
            "/x",
        );

        let mut out = UsageData::default();
        let mut errors: HashMap<String, String> = HashMap::new();
        let mut newest_captured: Option<(i64, String)> = None;

        apply_resolution(
            "home",
            &None,
            resolution,
            None,
            now(),
            &mut out,
            &mut errors,
            &mut newest_captured,
        );

        assert!(out.profiles["home"].attention.is_some());
        assert!(
            errors.is_empty(),
            "NeedsRefresh must never be recorded in errors — it stays a scoring candidate"
        );
    }

    // NOTE: `Resolution::Persist`'s `apply_resolution` branch is not exercised
    // here directly — it unconditionally calls `store::save`, which resolves
    // through the REAL `paths::usage_store` (keyed off the real `$HOME`), so
    // driving it from a test would write into whatever machine runs the test
    // suite. `track_newest`'s own unit tests above cover the tracking logic
    // that branch calls; the one-line `track_newest(usage.captured_at)` call
    // site is otherwise reviewed, not integration-tested — consistent with
    // this module's "collect itself is the thin I/O shell" convention (see
    // the module doc).

    // ── roll_over_expired_sections ──────────────────────────────────────────

    #[test]
    fn rollover_zeroes_expired_section_keeps_unexpired() {
        let now_ = now();
        let past = now_.timestamp() - 10;
        let future = now_.timestamp() + 10;
        let usage = ProfileUsage {
            captured_at: Some("2026-09-01T00:00:00Z".to_string()),
            session: Some(UsageSection {
                pct: 90,
                resets: Some("stale display".to_string()),
                resets_at: Some(past),
            }),
            week_all: Some(UsageSection {
                pct: 50,
                resets: Some("still valid".to_string()),
                resets_at: Some(future),
            }),
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: Some("api".to_string()),
            attention: None,
        };

        let rolled = roll_over_expired_sections(&usage, now_);

        let session = rolled.session.expect("session section still present");
        assert_eq!(session.pct, 0);
        assert!(session.resets.is_none());
        assert!(session.resets_at.is_none());

        let week_all = rolled.week_all.expect("week_all section still present");
        assert_eq!(week_all.pct, 50, "unexpired section must be untouched");

        // captured_at is preserved verbatim (scoring needs the true capture time).
        assert_eq!(rolled.captured_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    }

    #[test]
    fn rollover_leaves_section_without_resets_at_untouched() {
        // A section that has no resets_at at all (e.g. from an old record
        // predating the field) must never be treated as "expired".
        let usage = ProfileUsage {
            session: Some(UsageSection {
                pct: 77,
                resets: None,
                resets_at: None,
            }),
            ..Default::default()
        };
        let rolled = roll_over_expired_sections(&usage, now());
        assert_eq!(rolled.session.unwrap().pct, 77);
    }

    #[test]
    fn rollover_preserves_attention() {
        let usage = ProfileUsage {
            attention: Some(Attention {
                kind: AttentionKind::NeedsRefresh,
                message: "access token expired".to_string(),
                action: "csm --profile home".to_string(),
                since_epoch: Some(1),
            }),
            ..Default::default()
        };
        let rolled = roll_over_expired_sections(&usage, now());
        assert_eq!(
            rolled.attention.map(|a| a.kind),
            Some(AttentionKind::NeedsRefresh)
        );
    }

    // ── resolve_profile_name ────────────────────────────────────────────────

    #[test]
    fn resolve_profile_name_basename_strips_claude_prefix() {
        assert_eq!(
            resolve_profile_name("/Users/example/.claude.work"),
            Some("work".to_string())
        );
    }

    #[test]
    fn resolve_profile_name_basename_without_prefix_kept_verbatim() {
        assert_eq!(
            resolve_profile_name("/Users/example/some-other-dir"),
            Some("some-other-dir".to_string())
        );
    }

    #[test]
    fn resolve_profile_name_trims_trailing_separators() {
        assert_eq!(trim_trailing_sep("/a/b/"), "/a/b");
        assert_eq!(trim_trailing_sep("/a/b"), "/a/b");
        assert_eq!(trim_trailing_sep(r"C:\a\b\"), r"C:\a\b");
    }

    // ── should_throttle ──────────────────────────────────────────────────────

    #[test]
    fn should_throttle_no_prior_is_false() {
        let new_usage = ProfileUsage::default();
        assert!(!should_throttle(None, &new_usage, now()));
    }

    #[test]
    fn should_throttle_recent_same_source_same_pcts_is_true() {
        let prior = ProfileUsage {
            captured_at: Some(now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            source: Some("statusline".to_string()),
            ..Default::default()
        };
        let new_usage = ProfileUsage {
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            ..Default::default()
        };
        assert!(should_throttle(Some(&prior), &new_usage, now()));
    }

    #[test]
    fn should_throttle_different_pct_is_false() {
        let prior = ProfileUsage {
            captured_at: Some(now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            source: Some("statusline".to_string()),
            ..Default::default()
        };
        let new_usage = ProfileUsage {
            session: Some(UsageSection {
                pct: 43,
                resets: None,
                resets_at: None,
            }),
            ..Default::default()
        };
        assert!(!should_throttle(Some(&prior), &new_usage, now()));
    }

    #[test]
    fn should_throttle_old_capture_is_false() {
        let old_now = now() - chrono::Duration::seconds(30);
        let prior = ProfileUsage {
            captured_at: Some(old_now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            source: Some("statusline".to_string()),
            ..Default::default()
        };
        let new_usage = ProfileUsage {
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            ..Default::default()
        };
        assert!(!should_throttle(Some(&prior), &new_usage, now()));
    }

    #[test]
    fn should_throttle_different_source_is_false() {
        let prior = ProfileUsage {
            captured_at: Some(now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            source: Some("api".to_string()),
            ..Default::default()
        };
        let new_usage = ProfileUsage {
            session: Some(UsageSection {
                pct: 42,
                resets: None,
                resets_at: None,
            }),
            ..Default::default()
        };
        assert!(!should_throttle(Some(&prior), &new_usage, now()));
    }

    // ── record_statusline_payload ───────────────────────────────────────────
    //
    // These exercise the pure/parse edges directly reachable without env/disk
    // coordination; the full env+store integration is covered indirectly by
    // `should_throttle`/`resolve_profile_name`/`statusline::to_profile_usage`
    // above, and by the wiring stage's end-to-end test once `csm usage
    // capture` exists.

    #[test]
    fn record_statusline_payload_rejects_invalid_json() {
        let result = record_statusline_payload("not json");
        assert!(matches!(result, Err(LocalError::Json(_))));
    }

    #[test]
    fn record_statusline_payload_no_config_dir_env_is_false() {
        // Shared lock (see `crate::testenv`) — `crate::statusline`'s tests
        // mutate this same process-global var, and without a shared guard
        // that interleaving can flake this test or, worse, resolve some
        // other test's `CLAUDE_CONFIG_DIR` and write into the real store.
        let _guard = crate::testenv::CLAUDE_CONFIG_DIR_ENV_LOCK.lock().unwrap();
        // Guard other tests in this process from a leaked env value.
        let saved = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let result = record_statusline_payload(
            r#"{"rate_limits": {"five_hour": {"used_percentage": 1.0}}}"#,
        );
        assert!(result.unwrap().is_none());
        if let Some(v) = saved {
            std::env::set_var("CLAUDE_CONFIG_DIR", v);
        }
    }

    // ── build_statusline_record ─────────────────────────────────────────────

    #[test]
    fn build_statusline_record_preserves_prior_api_captured_at() {
        let prior = store::StoreRecord {
            profile: "home".to_string(),
            captured_at: Some("2026-08-20T00:00:00Z".to_string()),
            source: Some("api".to_string()),
            api_captured_at: Some("2026-08-20T00:00:00Z".to_string()),
            cooldown_until: None,
            usage: None,
        };
        let new_usage = ProfileUsage {
            captured_at: Some("2026-09-02T00:00:00Z".to_string()),
            source: Some("statusline".to_string()),
            ..Default::default()
        };
        let rec = build_statusline_record("home", new_usage, Some(&prior));
        // captured_at DOES advance to this capture's time...
        assert_eq!(rec.captured_at.as_deref(), Some("2026-09-02T00:00:00Z"));
        // ...but api_captured_at carries the prior value forward unchanged —
        // this is what keeps `collect()`'s freshness gate honest for a
        // statusline-only write (see the finding this fixes).
        assert_eq!(rec.api_captured_at.as_deref(), Some("2026-08-20T00:00:00Z"));
    }

    #[test]
    fn build_statusline_record_no_prior_leaves_api_captured_at_none() {
        let new_usage = ProfileUsage {
            captured_at: Some("2026-09-02T00:00:00Z".to_string()),
            source: Some("statusline".to_string()),
            ..Default::default()
        };
        let rec = build_statusline_record("home", new_usage, None);
        assert!(rec.api_captured_at.is_none());
    }

    #[test]
    fn build_statusline_record_preserves_prior_cooldown() {
        let prior = store::StoreRecord {
            profile: "home".to_string(),
            cooldown_until: Some(1_800_000_000),
            ..Default::default()
        };
        let new_usage = ProfileUsage::default();
        let rec = build_statusline_record("home", new_usage, Some(&prior));
        assert_eq!(rec.cooldown_until, Some(1_800_000_000));
    }

    // ── oldest_profile_age_secs ─────────────────────────────────────────────

    #[test]
    fn oldest_profile_age_secs_none_when_no_profiles() {
        let data = UsageData::default();
        assert!(oldest_profile_age_secs(&data, now()).is_none());
    }

    #[test]
    fn oldest_profile_age_secs_is_the_max_across_profiles() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "fresh".to_string(),
            ProfileUsage {
                captured_at: Some(now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                ..Default::default()
            },
        );
        profiles.insert(
            "stale".to_string(),
            ProfileUsage {
                captured_at: Some("2026-08-30T00:00:00Z".to_string()), // 3 days old
                ..Default::default()
            },
        );
        let data = UsageData {
            profiles,
            ..Default::default()
        };
        let age = oldest_profile_age_secs(&data, now()).expect("must compute an age");
        // now() is 2026-09-02T00:00:00Z; the 3-day-old profile drives the max.
        assert_eq!(age, 3 * 24 * 3600);
    }

    #[test]
    fn oldest_profile_age_secs_ignores_profiles_with_no_captured_at() {
        // A profile with no captured_at (e.g. LOGIN REQUIRED, never
        // successfully captured) renders as dashes — no "last-known" data is
        // being shown for it — so it must not inflate the staleness age of
        // the OTHER profile whose real numbers ARE on screen. Earlier this
        // counted such a profile as maximally stale (`u64::MAX`), which
        // produced the same absurd "213503982334601d old" banner bug A fixes
        // as soon as any other profile had a real captured_at (see the repro
        // in the module's PR: a `--refresh` after a statusline capture on
        // one of two profiles).
        let mut profiles = HashMap::new();
        profiles.insert(
            "fresh".to_string(),
            ProfileUsage {
                captured_at: Some(now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                ..Default::default()
            },
        );
        profiles.insert(
            "no_timestamp".to_string(),
            ProfileUsage {
                captured_at: None,
                ..Default::default()
            },
        );
        let data = UsageData {
            profiles,
            ..Default::default()
        };
        assert_eq!(oldest_profile_age_secs(&data, now()), Some(0));
    }

    #[test]
    fn oldest_profile_age_secs_none_when_no_profile_ever_captured() {
        // Every profile is NeedsLogin/NotFound with no store record at all —
        // nothing has EVER been captured, so there is nothing to call
        // "stale". Before this fix, the naive max-across-profiles reported
        // Some(u64::MAX), which rendered as an absurd
        // "⚠ usage data is 213503982334601d old" banner (bug A).
        let mut profiles = HashMap::new();
        profiles.insert(
            "home".to_string(),
            ProfileUsage {
                captured_at: None,
                ..Default::default()
            },
        );
        profiles.insert(
            "work".to_string(),
            ProfileUsage {
                captured_at: None,
                ..Default::default()
            },
        );
        let data = UsageData {
            profiles,
            ..Default::default()
        };
        assert_eq!(
            oldest_profile_age_secs(&data, now()),
            None,
            "no profile has ever captured data — stale_secs must be None, not u64::MAX"
        );
    }

    // ── collect(): invalid profile names are skipped before any store I/O ──
    //
    // `paths::usage_store`'s documented contract requires `profile` to be
    // validated before it reaches any store call. This exercises the real
    // `collect()` shell (not just a pure helper) because the whole point is
    // that store::load/save are never reached for the bad name — so this is
    // safe to run against the real store paths: nothing on disk is ever
    // touched for the one (invalid) profile in this map.

    #[test]
    fn collect_skips_invalid_profile_name_without_touching_store() {
        let mut map = HashMap::new();
        map.insert("../evil".to_string(), "/tmp/does-not-matter".to_string());
        let profiles = ProfileMap(map);

        let data = collect(&profiles, now(), false);

        assert!(
            data.profiles.is_empty(),
            "an invalid name must never produce a served profile"
        );
        let errors = data.errors.expect("invalid name must record an error");
        assert!(errors.contains_key("../evil"));
    }
}
