//! Local usage collection over Orca's host accounts: each account's own
//! OAuth usage API, read directly, with no shared remote service in the path.
//!
//! Two entry points:
//! - [`collect`] — for every host account, serve a fresh store record, or
//!   probe live, or serve a stale record with a rollover, or report an
//!   error. Called by [`crate::usage::fetch`]/[`crate::usage::fetch_with`]
//!   as the terminal layer of the cache ladder.
//! - [`record_statusline_payload`] — `csm usage capture`'s core: merge a
//!   statusLine stdin payload into the store record of the account the
//!   session runs on (attribution by identity-change events, design
//!   section 4).
//!
//! # Where each account's numbers come from (design section 8)
//!
//! - **The account `D` holds** ([`AccountSet::current`]): the statusLine
//!   capture keeps its record current; a stale record is re-probed with the
//!   runtime grant read in Orca's order ([`creds::lookup_runtime`]) when
//!   [`CollectOpts::probe_active`] allows it. csm never refreshes the active
//!   grant: claude and Orca own it.
//! - **Inactive, Orca running**: Orca owns the stashes, so csm touches none.
//!   [`OrcaUsage::Cached`] reads Orca's cached
//!   `rateLimits.inactiveClaudeAccounts` over `accounts.list`;
//!   [`OrcaUsage::Refresh`] asks Orca to re-probe first (a limit pick or
//!   `usage --refresh` only, never the statusline or the hook). With
//!   [`OrcaUsage::Off`] the stored record is served.
//! - **Inactive, Orca stopped**: the stash token. With
//!   [`CollectOpts::refresh_stash`] a grant that expires within 5 min is
//!   refreshed first exactly as Orca does, under `switch.lock`, into the
//!   stash only — and never while an unretired legacy `~/.claude.<name>`
//!   still holds the account (design section 10 item 5), nor for the store's
//!   active account or a grant `D` also holds ([`stash_refresh_guard`]).
//!   This replaces the former `CSM_OAUTH_REFRESH` opt-in, which is gone.
//!
//! # `collect` algorithm
//!
//! For each account that may be probed, in order:
//! 1. **Fresh?** `!force && now - rec.api_captured_at < CSM_USAGE_PROFILE_TTL`
//!    → serve `rec.usage` (rolled over for any expired section) as-is. Gated
//!    on `api_captured_at` — the last live api reading — never on the
//!    record's blanket `captured_at`, which a statusline capture refreshes
//!    every few seconds without carrying `week_fable`.
//! 2. **Cooldown?** `rec.cooldown_until > now` → serve the stale record
//!    (rolled over) if one exists, else record an error. No probe attempted.
//! 3. **Probe**: read the grant → `api::fetch_usage`. Every terminal outcome
//!    collapses to an [`Event`], and [`resolve`] — a pure function taking
//!    that `Event` plus whether a stale record exists — decides what
//!    `collect()` does next.
//!
//! The top-level `UsageData::captured_at` this module returns is the newest
//! `captured_at` actually placed into `profiles` this round — never simply
//! `now` (see `track_newest`). `UsageData::any_probe_attempted`/
//! `any_probe_succeeded` (never serialized) tell `transport.rs` whether this
//! round actually reached the network.
//!
//! The access token never appears in any error string this module produces.
//!
//! # Dead-credential warnings
//!
//! `creds::CredError::Expired` carries `refresh_alive`, splitting a dead
//! account in two: **NeedsRefresh** (switching to it, or Orca, refreshes the
//! grant; the account stays a scoring candidate) and **NeedsLogin** (refresh
//! token dead/absent, no credentials, or an API 401/403 — log in again in
//! Orca). `resolve` turns either case into an [`Attention`] attached to the
//! served `ProfileUsage`. `NeedsLogin` also lands in `UsageData::errors`,
//! which is what keeps it out of `scoring::pick_best_at`'s candidates.

pub mod api;
pub mod creds;
pub mod display;
pub mod statusline;
pub mod store;

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::account::AccountSet;
use crate::account::accounts::is_valid_key;
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
    if !force
        && let Some(captured) = captured_at_epoch
        && now_epoch - captured < ttl_secs
    {
        return Freshness::Fresh;
    }
    if let Some(cooldown) = cooldown_until
        && cooldown > now_epoch
    {
        return Freshness::Cooldown(cooldown);
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

/// The pure branch table. `has_stale` = the account already has a stored
/// `usage` to fall back on (read by `apply_resolution`, not here). `label`
/// names the account in the `NeedsRefresh` action (`csm accounts use
/// <label>`). Returns the [`Resolution`] plus whether the caller
/// should stamp a rate-limit cooldown — the pure fn decides *that* a cooldown
/// applies, never the timestamp itself (that needs `now` + the configured
/// cooldown duration, which belong to the I/O shell).
///
/// Credential-trouble events never look at `has_stale` — every one of them
/// becomes [`Resolution::NeedsLogin`]/[`Resolution::NeedsRefresh`]
/// unconditionally (design spec "맛이 간 프로필은 로그인하라고 경고" §자격증명
/// 상태 3분류); `apply_resolution` is what decides, from `has_stale`, whether
/// the row it builds carries real numbers or is bare.
fn resolve(has_stale: bool, event: Event, label: &str) -> (Resolution, bool) {
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
                errors_msg:
                    "not logged in (no Claude Code credentials for this account) — login required"
                        .to_string(),
                attention: Attention {
                    kind: AttentionKind::NeedsLogin,
                    message: "not logged in".to_string(),
                    action: login_action(),
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
                    action: refresh_action(label),
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
                    action: login_action(),
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
                    action: login_action(),
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

/// What resolves a NeedsLogin account: Orca owns logins, so log in again
/// there (or add the account afresh through csm, which runs Orca's flow).
fn login_action() -> String {
    "log in again in Orca (or run `csm accounts add`)".to_string()
}

/// The exact, copy-pasteable command that resolves a NeedsRefresh account:
/// switching to it refreshes its grant (design §3), and so does Orca.
fn refresh_action(label: &str) -> String {
    format!("csm accounts use {label}")
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
        if let Some(epoch) = s.resets_at
            && epoch < now_epoch
        {
            *s = UsageSection {
                pct: 0,
                resets: None,
                resets_at: None,
            };
        }
    }
    out
}

// ─── options ────────────────────────────────────────────────────────────────

/// How an inactive account's usage is read while Orca runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrcaUsage {
    /// Never open Orca's socket: serve the stored record (hook, statusline).
    #[default]
    Off,
    /// `accounts.list{refreshUsage:false}`: Orca's cached
    /// `rateLimits.inactiveClaudeAccounts`.
    Cached,
    /// `accounts.list{refreshUsage:true}` with this timeout, falling back to
    /// `Cached` on a timeout or an error (a limit pick or `usage --refresh`).
    Refresh(std::time::Duration),
}

/// What one [`collect`] pass may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectOpts {
    /// Bypass every account's store TTL.
    pub force: bool,
    /// Probe the active account's runtime grant when its record is stale.
    /// Off for anything on an `OrcaPane`/`Print` launch path.
    pub probe_active: bool,
    /// Inactive accounts while Orca runs (it owns their stashes then).
    pub orca: OrcaUsage,
    /// Inactive accounts while Orca is stopped: probe with the stash token.
    pub probe_stash: bool,
    /// ...and refresh a due stash grant first (switch candidates and
    /// `usage --refresh` only, design section 8).
    pub refresh_stash: bool,
    /// The caller already holds `switch.lock` (the limit-switch leader), so
    /// a stash refresh runs under it instead of taking it again: the lock
    /// is per open file, and a second take in the same process would wait
    /// out its timeout against the caller.
    pub switch_lock_held: bool,
}

impl CollectOpts {
    /// The default read: store TTL, the active grant, stash probes while
    /// Orca is stopped, no RPC.
    pub fn standard() -> CollectOpts {
        CollectOpts {
            force: false,
            probe_active: true,
            orca: OrcaUsage::Off,
            probe_stash: true,
            refresh_stash: false,
            switch_lock_held: false,
        }
    }

    /// csm's own store only (the hook and the statusline, design decision
    /// 8): no probe of any grant, no Orca socket, no liveness check.
    pub fn cache_only() -> CollectOpts {
        CollectOpts {
            force: false,
            probe_active: false,
            orca: OrcaUsage::Off,
            probe_stash: false,
            refresh_stash: false,
            switch_lock_held: false,
        }
    }

    /// Can this pass reach anything beyond the store?
    fn may_probe(&self) -> bool {
        self.probe_active || self.probe_stash || self.orca != OrcaUsage::Off
    }
}

// ─── I/O shell ──────────────────────────────────────────────────────────────

/// Collect (or serve cached) usage for every host account in `accounts`.
///
/// `now` is threaded through explicitly so callers get one consistent
/// instant across every account in the pass. See [`CollectOpts`] for what a
/// pass may reach; the pure [`resolve`]/[`check_freshness`]/
/// [`roll_over_expired_sections`] functions above decide everything else.
pub fn collect(accounts: &AccountSet, now: DateTime<Utc>, opts: &CollectOpts) -> UsageData {
    let ttl_secs = profile_ttl_secs();
    let rl_cooldown_secs = rate_limit_cooldown_secs();
    let base = api::resolve_base();
    if opts.may_probe() && base != api::DEFAULT_BASE {
        // A redirected token destination must never be silent.
        crate::usage::warn(format!(
            "csm: warning: CSM_USAGE_API_BASE overrides the usage API base to \
             {base} — every account's OAuth token will be sent there as a \
             Bearer credential; verify this host is trusted"
        ));
    }
    let now_epoch = now.timestamp();

    let mut out = UsageData {
        captured_at: None,
        profiles: HashMap::new(),
        errors: None,
        ..Default::default()
    };
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut newest_captured: Option<(i64, String)> = None;
    let mut any_probe_attempted = false;
    let mut any_probe_succeeded = false;
    let mut inactive = Inactive::new(accounts, opts);

    for id in accounts.ids_sorted() {
        // `paths::usage_store`'s documented precondition: the key must be
        // validated before it reaches any store call.
        if !is_valid_key(id) {
            errors.insert(id.to_string(), "invalid account id".to_string());
            continue;
        }
        let rec = store::load(id);
        let label = accounts.label(id);
        let active = accounts.current.as_deref() == Some(id);

        // Which grant (if any) this pass may probe for `id`.
        let source = if active {
            opts.probe_active.then_some(Source::Runtime)
        } else {
            match inactive.route(id, now) {
                InactiveRoute::Orca(usage) => {
                    let usage = *usage;
                    persist_orca(id, rec.as_ref(), &usage);
                    track_newest(&mut newest_captured, usage.captured_at.as_deref());
                    out.profiles.insert(id.to_string(), usage);
                    continue;
                }
                InactiveRoute::Stored => None,
                InactiveRoute::Stash => Some(Source::Stash),
            }
        };
        let Some(source) = source else {
            if let Some(usage) = rec.as_ref().and_then(|r| r.usage.clone()) {
                let rolled = roll_over_expired_sections(&usage, now);
                track_newest(&mut newest_captured, rolled.captured_at.as_deref());
                out.profiles.insert(id.to_string(), rolled);
            }
            continue;
        };

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
            opts.force,
            ttl_secs,
        ) {
            Freshness::Fresh => {
                if let Some(usage) = rec.as_ref().and_then(|r| r.usage.clone()) {
                    let rolled = roll_over_expired_sections(&usage, now);
                    track_newest(&mut newest_captured, rolled.captured_at.as_deref());
                    out.profiles.insert(id.to_string(), rolled);
                }
            }
            Freshness::Cooldown(until) => {
                let (resolution, _) = resolve(has_stale, Event::InCooldown { until }, &label);
                apply_resolution(
                    id,
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
                let event = match source {
                    Source::Runtime => probe_runtime(accounts, now, &base),
                    Source::Stash => probe_stash(
                        accounts,
                        id,
                        now,
                        &base,
                        opts.refresh_stash,
                        opts.switch_lock_held,
                    ),
                };
                if matches!(event, Event::ApiOk(_)) {
                    any_probe_succeeded = true;
                }
                let (resolution, needs_cooldown) = resolve(has_stale, event, &label);
                let cooldown = needs_cooldown.then_some(now_epoch + rl_cooldown_secs);
                apply_resolution(
                    id,
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

/// Which grant a probe reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The account `D` holds: the runtime grant, never refreshed.
    Runtime,
    /// An inactive account's Orca stash (Orca stopped).
    Stash,
}

/// How one inactive account is served this pass.
enum InactiveRoute {
    /// Orca's cached reading (Orca runs).
    Orca(Box<ProfileUsage>),
    /// The stored record only.
    Stored,
    /// Probe with the stash token (Orca stopped).
    Stash,
}

/// Per-pass facts about inactive accounts, resolved lazily: whether Orca
/// runs (one liveness check) and, when it does, one `accounts.list`.
struct Inactive<'a> {
    accounts: &'a AccountSet,
    opts: &'a CollectOpts,
    running: Option<bool>,
    snapshot: Option<Option<crate::orca::rpc::AccountsSnapshot>>,
}

impl<'a> Inactive<'a> {
    fn new(accounts: &'a AccountSet, opts: &'a CollectOpts) -> Self {
        Inactive {
            accounts,
            opts,
            running: None,
            snapshot: None,
        }
    }

    fn running(&mut self) -> bool {
        if let Some(r) = self.running {
            return r;
        }
        // Unknown host = treat as running: never touch a stash then.
        let r = match &self.accounts.host {
            Some(h) => {
                crate::orca::live::check(h.os, &h.user_data, &crate::orca::live::SystemProcs)
                    .running
            }
            None => true,
        };
        self.running = Some(r);
        r
    }

    fn snapshot(&mut self) -> Option<&crate::orca::rpc::AccountsSnapshot> {
        if self.snapshot.is_none() {
            let snap = self
                .accounts
                .host
                .as_ref()
                .and_then(|h| list_usage(&h.user_data, self.opts.orca));
            self.snapshot = Some(snap);
        }
        self.snapshot.as_ref().and_then(Option::as_ref)
    }

    fn route(&mut self, id: &str, now: DateTime<Utc>) -> InactiveRoute {
        // Nothing but the store is allowed: skip the liveness sweep too.
        if self.opts.orca == OrcaUsage::Off && !self.opts.probe_stash {
            return InactiveRoute::Stored;
        }
        if self.running() {
            if self.opts.orca == OrcaUsage::Off {
                return InactiveRoute::Stored;
            }
            let usage = self.snapshot().and_then(|s| {
                s.inactive_usage
                    .iter()
                    .find(|u| u.account_id == id)
                    .and_then(|u| orca_usage(u.limits.as_ref()?, u.updated_at, now))
            });
            return match usage {
                Some(u) => InactiveRoute::Orca(Box::new(u)),
                None => InactiveRoute::Stored,
            };
        }
        if self.opts.probe_stash {
            InactiveRoute::Stash
        } else {
            InactiveRoute::Stored
        }
    }
}

/// `accounts.list` per [`OrcaUsage`]; `None` when Orca did not answer.
fn list_usage(user_data: &Path, mode: OrcaUsage) -> Option<crate::orca::rpc::AccountsSnapshot> {
    use crate::orca::rpc;
    match mode {
        OrcaUsage::Off => None,
        OrcaUsage::Cached => rpc::accounts_list(user_data, false, rpc::LIST_TIMEOUT).ok(),
        OrcaUsage::Refresh(timeout) => rpc::accounts_list(user_data, true, timeout)
            .ok()
            .or_else(|| rpc::accounts_list(user_data, false, rpc::LIST_TIMEOUT).ok()),
    }
}

/// Map Orca's cached limits for one account to a [`ProfileUsage`]. `None`
/// when Orca holds no percent at all. Pure.
pub(crate) fn orca_usage(
    limits: &crate::orca::rpc::ProviderLimits,
    updated_at_ms: Option<i64>,
    now: DateTime<Utc>,
) -> Option<ProfileUsage> {
    let section = |w: Option<&crate::orca::rpc::RateWindow>| {
        w.map(|w| {
            let resets_at = w.resets_at.map(|ms| ms / 1000);
            UsageSection {
                pct: w.used_percent.round() as i64,
                resets: resets_at.map(display::format_resets),
                resets_at,
            }
        })
    };
    let session = section(limits.session.as_ref());
    let week_all = section(limits.weekly.as_ref());
    let week_fable = section(limits.fable_weekly.as_ref());
    if session.is_none() && week_all.is_none() && week_fable.is_none() {
        return None;
    }
    let at = limits
        .updated_at
        .or(updated_at_ms)
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or(now);
    Some(ProfileUsage {
        captured_at: Some(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        week_model_label: week_fable.as_ref().map(|_| "Fable".to_string()),
        session,
        week_all,
        week_fable,
        session_stats: Vec::new(),
        source: Some("orca".to_string()),
        attention: None,
    })
}

/// Persist Orca's reading as a store record. Orca's own probe is a live api
/// reading, so it stamps `api_captured_at` too.
fn persist_orca(id: &str, prior: Option<&store::StoreRecord>, usage: &ProfileUsage) {
    let rec = store::StoreRecord {
        profile: id.to_string(),
        captured_at: usage.captured_at.clone(),
        source: usage.source.clone(),
        api_captured_at: usage.captured_at.clone(),
        cooldown_until: prior.and_then(|r| r.cooldown_until),
        usage: Some(usage.clone()),
    };
    if let Err(e) = store::save(id, &rec) {
        crate::usage::warn(format!(
            "csm: warning: could not write usage store for {id}: {e}"
        ));
    }
}

/// Probe the runtime grant (the account `D` holds). Read-only.
fn probe_runtime(accounts: &AccountSet, now: DateTime<Utc>, base: &str) -> Event {
    let Some(h) = &accounts.host else {
        return Event::CredsOther("no host context".into());
    };
    call_api(
        creds::lookup_runtime(h.os, &h.paths, &h.keychain, now),
        now,
        base,
    )
}

/// Probe an inactive account with its stash token (Orca stopped). With
/// `refresh`, a due grant is refreshed first, under `switch.lock`, into the
/// stash only (design section 3 step 6 / section 8).
fn probe_stash(
    accounts: &AccountSet,
    id: &str,
    now: DateTime<Utc>,
    base: &str,
    refresh: bool,
    lock_held: bool,
) -> Event {
    use crate::orca::stash::Stash;
    let Some(h) = &accounts.host else {
        return Event::CredsOther("no host context".into());
    };
    let managed = accounts
        .get(id)
        .and_then(|a| a.managed_auth_path.as_deref());
    let stash = match Stash::open(&h.user_data, id, managed) {
        Ok(s) => s,
        Err(e) => return Event::CredsOther(format!("stash: {e}")),
    };
    let mut json = match stash.credentials(h.os) {
        Ok(Some(j)) => j,
        Ok(None) => return Event::NoCreds,
        Err(e) => return Event::CredsOther(format!("stash: {e}")),
    };
    let now_ms = now.timestamp_millis();
    if refresh && crate::orca::refresh::needs_refresh(json.expose(), now_ms) {
        match refresh_stash(accounts, h, &stash, id, now_ms, lock_held) {
            Ok(Some(fresh)) => json = fresh,
            Ok(None) => {}
            Err(why) => append_usage_log(&format!("usage: stash refresh skipped: {why}")),
        }
    }
    call_api(creds::parse_blob(json.expose(), now), now, base)
}

/// Refresh `id`'s stash grant under `switch.lock`: re-read it there (the
/// compare half of the compare-and-swap), refuse while Orca runs, while an
/// unretired legacy profile dir still holds this account, or when the grant
/// may be the one `D` holds ([`stash_refresh_guard`]), then run Orca's
/// refresh into the stash only. `Ok(None)` = nothing new. With
/// `lock_held` the caller already holds `switch.lock`.
fn refresh_stash(
    accounts: &AccountSet,
    h: &crate::account::accounts::HostCtx,
    stash: &crate::orca::stash::Stash,
    id: &str,
    now_ms: i64,
    lock_held: bool,
) -> Result<Option<crate::orca::SecretString>, String> {
    use crate::orca::refresh::{StashRefresh, refresh_stash_if_needed};
    use crate::orca::{live, quarantine::Quarantine};

    let email = accounts.get(id).and_then(|a| a.email.as_deref());
    let uuid = stash
        .oauth_account()
        .ok()
        .flatten()
        .and_then(|v| crate::orca::runtime::OauthIdentity::from_value(&v).account_uuid);
    if legacy_dir_holds(&h.home, uuid.as_deref(), email) {
        return Err("an unretired legacy profile dir still holds this account".into());
    }
    let state = crate::paths::smart_dir().map_err(|e| format!("state dir: {}", e.kind()))?;
    let _lock = lock_for_refresh(&state, lock_held, std::time::Duration::from_secs(10))?;
    if live::check(h.os, &h.user_data, &live::SystemProcs).running {
        return Err("Orca is running".into());
    }
    let json = match stash.credentials(h.os) {
        Ok(Some(j)) => j,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("stash: {e}")),
    };
    let runtime = runtime_refresh_tokens(h);
    stash_refresh_guard(
        id,
        accounts.active.as_deref(),
        crate::orca::readback::refresh_token(json.expose()).as_deref(),
        runtime.as_deref().map_err(|_| ()),
    )
    .map_err(str::to_owned)?;
    let http = crate::orca::http::SystemHttp::from_env();
    let q = Quarantine::new(h.os, &state);
    match refresh_stash_if_needed(&h.user_data, h.os, stash, json.expose(), &http, &q, now_ms) {
        Ok(StashRefresh::Refreshed(fresh)) => Ok(Some(fresh)),
        Ok(StashRefresh::NotDue) => Ok(Some(json)),
        Ok(StashRefresh::Failed(f)) => Err(format!("refresh failed: {f:?}")),
        Ok(StashRefresh::Quarantined(fp)) => Err(format!(
            "the refreshed grant could not be stored; quarantined as {fp}"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// `switch.lock` for a stash refresh: `None` when the caller already holds
/// it (taking it again in this process would block against the caller).
fn lock_for_refresh(
    state: &Path,
    held: bool,
    wait: std::time::Duration,
) -> Result<Option<crate::orca::fsx::SwitchLock>, String> {
    if held {
        return Ok(None);
    }
    crate::orca::fsx::SwitchLock::acquire(state, wait)
        .map(Some)
        .map_err(|e| format!("switch.lock: {}", e.kind()))
}

/// May the stash grant of `id` be refreshed? Orca refreshes a stash only
/// for an account that is not live (refreshManagedAccountTokenIfNeeded's
/// caller guarantee): a refresh rotates the single-use refresh token, and
/// every other copy of the grant dies with it. csm cannot always name the
/// account `D` holds (`AccountSet::current` is `None` for a neutral `D` or
/// one user in two orgs), so it refuses the store's active account and any
/// stash whose refresh token `D` also holds. `runtime_rts` is `Err` when
/// `D`'s credentials could not be read (refused: fail safe). Pure.
pub(crate) fn stash_refresh_guard(
    id: &str,
    store_active: Option<&str>,
    stash_rt: Option<&str>,
    runtime_rts: Result<&[String], ()>,
) -> Result<(), &'static str> {
    if store_active == Some(id) {
        return Err("Orca's store names this account active");
    }
    let Ok(rts) = runtime_rts else {
        return Err("the runtime credentials in D could not be read");
    };
    if stash_rt.is_some_and(|rt| rts.iter().any(|r| r == rt)) {
        return Err("D holds this grant");
    }
    Ok(())
}

/// The refresh tokens of every runtime grant in `D`: on macOS the scoped
/// and the unscoped Keychain item, then `D/.credentials.json`. Holds
/// secrets in memory only; the error never quotes one.
fn runtime_refresh_tokens(h: &crate::account::accounts::HostCtx) -> Result<Vec<String>, String> {
    use crate::orca::keychain;
    use crate::orca::readback::refresh_token;
    let mut out = Vec::new();
    if h.os == crate::orca::HostOs::MacOs {
        let dir = h.paths.config_dir.to_string_lossy().into_owned();
        for dir in [Some(dir.as_str()), None] {
            match keychain::read_runtime_scoped(dir, &h.keychain) {
                Ok(Some(v)) => out.extend(refresh_token(v.expose())),
                Ok(None) => {}
                Err(e) => return Err(format!("Keychain: {e}")),
            }
        }
    }
    let p = &h.paths.credentials_path;
    match crate::orca::read_capped_bytes(p, 1024 * 1024) {
        Ok(Some(mut b)) => {
            out.extend(refresh_token(&String::from_utf8_lossy(&b)));
            crate::orca::zero(&mut b);
        }
        Ok(None) => {}
        Err(e) => return Err(format!("{}: {}", p.display(), e.kind())),
    }
    Ok(out)
}

/// Does an unretired `~/.claude.<name>` from the legacy registry still hold
/// this account (design section 10 item 5)? Matched by `accountUuid`, else
/// by email. A listed dir whose identity cannot be read counts as holding
/// it (fail safe: no offline refresh).
fn legacy_dir_holds(home: &Path, uuid: Option<&str>, email: Option<&str>) -> bool {
    let Ok(legacy) = crate::migrate::load_legacy(home) else {
        return true;
    };
    legacy.profiles.iter().any(|p| {
        if std::fs::symlink_metadata(&p.dir).is_err() {
            return false;
        }
        let paths = crate::orca::runtime::runtime_paths(Some(&p.dir.to_string_lossy()), home, |x| {
            x.exists()
        });
        match crate::orca::runtime::read_runtime_identity(&paths) {
            crate::orca::runtime::RuntimeIdentity::Present(i) => {
                let by_uuid = matches!((uuid, i.account_uuid.as_deref()), (Some(a), Some(b)) if a == b);
                let by_email = matches!((email, i.email.as_deref()), (Some(a), Some(b)) if a.trim().eq_ignore_ascii_case(b.trim()));
                by_uuid || by_email
            }
            crate::orca::runtime::RuntimeIdentity::None => false,
            crate::orca::runtime::RuntimeIdentity::Unreadable => true,
        }
    })
}

/// One line to csm's log (never a secret).
fn append_usage_log(msg: &str) {
    let _ = crate::hook::notify::append_log("usage", msg);
}

/// Send the token to the usage API and map the answer to an [`Event`].
fn call_api(
    lookup: Result<creds::OauthToken, creds::CredError>,
    now: DateTime<Utc>,
    base: &str,
) -> Event {
    let token = match lookup {
        Ok(t) => t,
        Err(creds::CredError::NotFound) => return Event::NoCreds,
        Err(creds::CredError::Expired {
            refresh_alive,
            expired_at_ms,
        }) => {
            return Event::CredsExpired {
                refresh_alive,
                expired_at_ms,
            };
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
                crate::usage::warn(format!(
                    "csm: warning: could not write usage store for {name}: {e}"
                ));
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
                crate::usage::warn(format!("csm: usage ({name}): {h}"));
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

    if let Some(until) = set_cooldown
        && let Err(e) = store::set_cooldown(name, until)
    {
        crate::usage::warn(format!(
            "csm: warning: could not write usage cooldown for {name}: {e}"
        ));
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

/// Merge a statusLine stdin payload into the store record of the account the
/// session runs on.
///
/// Returns `Ok(None)` for every legitimate no-op (payload has no usable
/// `rate_limits`, the session cannot be attributed to an account, the
/// account id is not a valid key) — never an `Err` for those; `Err` is
/// reserved for a malformed payload (`raw` isn't even valid JSON).
///
/// Attribution (design section 4, last paragraph): every tick first records
/// `D`'s `accountUuid` in `<state>/last-identity`; a change, whoever caused
/// it, is a switch event and re-stamps `.last-switch`. The capture then
/// counts for the session's `account_id` only when the session was born at
/// or after the last switch event; an older session's numbers may be the
/// previous account's, so they are dropped. See [`attribute_capture`].
///
/// Throttle: if the current record's source is already `"statusline"`, was
/// captured under 10s ago, and this payload would produce identical
/// session/week_all percentages, skip the write.
pub fn record_statusline_payload(raw: &str) -> Result<Option<StatuslineCapture>, LocalError> {
    record_statusline_payload_in(raw, &AccountSet::load())
}

/// [`record_statusline_payload`] over an already-loaded [`AccountSet`] (the
/// statusline loads it once for its label too).
pub fn record_statusline_payload_in(
    raw: &str,
    accounts: &AccountSet,
) -> Result<Option<StatuslineCapture>, LocalError> {
    let payload: statusline::StatuslinePayload = serde_json::from_str(raw)?;
    let now = Utc::now();

    let last_switch = note_identity(
        &accounts.runtime_dir,
        accounts.current_uuid.as_deref(),
        now.timestamp(),
    );
    let sidecar = payload
        .session_id()
        .filter(|sid| crate::session::alias::looks_like_uuid(sid))
        .and_then(|sid| crate::sidecar::read_sidecar(&crate::paths::sidecar(&sid)).ok());
    let Some(id) = attribute_capture(&CaptureFacts {
        sidecar_account: sidecar.as_ref().and_then(|s| s.account_id.as_deref()),
        sidecar_born: sidecar.as_ref().and_then(|s| s.born),
        duration_secs: payload.duration_secs(),
        d_account: accounts.current.as_deref(),
        last_switch,
        now: now.timestamp(),
    }) else {
        return Ok(None);
    };
    if !is_valid_key(&id) {
        return Ok(None);
    }

    let prior_rec = store::load(&id);
    let prior_usage = prior_rec.as_ref().and_then(|r| r.usage.as_ref());

    let Some(new_usage) = statusline::to_profile_usage(&payload, prior_usage, now) else {
        return Ok(None);
    };

    if should_throttle(prior_usage, &new_usage, now) {
        // Throttled: the store already holds this reading (written within
        // the last 10 s); the reading itself is still current.
        return Ok(Some(StatuslineCapture {
            account_id: id,
            usage: new_usage,
        }));
    }

    let new_rec = build_statusline_record(&id, new_usage, prior_rec.as_ref());
    store::save(&id, &new_rec)?;
    Ok(Some(StatuslineCapture {
        account_id: id,
        usage: new_rec.usage.expect("record built from Some(usage)"),
    }))
}

/// What one statusline tick learned, handed back to the caller so the
/// limit-switch trigger (`hook::run_from_statusline`) can act on the same
/// merged reading the store just received.
#[derive(Debug, Clone)]
pub struct StatuslineCapture {
    /// The Orca account id the reading was attributed to.
    pub account_id: String,
    /// This tick's `session`/`week_all` merged with the store's carried-forward
    /// `week_fable`/`week_model_label`. Returned whether or not the
    /// identical-within-10-s throttle skipped the store write.
    pub usage: ProfileUsage,
}

/// The inputs of [`attribute_capture`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CaptureFacts<'a> {
    /// The session's launch-time account (its csm sidecar), if supervised.
    pub sidecar_account: Option<&'a str>,
    /// When the session was (re)launched on that account.
    pub sidecar_born: Option<i64>,
    /// `cost.total_duration_ms` / 1000 from the payload, if present.
    pub duration_secs: Option<i64>,
    /// The account `D` holds right now.
    pub d_account: Option<&'a str>,
    /// The last switch event (epoch), if any.
    pub last_switch: Option<i64>,
    pub now: i64,
}

/// Which account a capture counts for, or `None` to drop it. Pure.
///
/// A csm-supervised session has a sidecar `account_id` and `born`; it counts
/// when born at or after the last switch event. A session csm did not
/// launch counts for `D`'s account when its own elapsed time says it
/// started after the last switch event, or when there never was one.
pub(crate) fn attribute_capture(f: &CaptureFacts<'_>) -> Option<String> {
    let after_switch = |born: i64| f.last_switch.is_none_or(|s| born >= s);
    if let (Some(id), Some(born)) = (f.sidecar_account, f.sidecar_born) {
        return after_switch(born).then(|| id.to_owned());
    }
    let d = f.d_account?;
    match (f.last_switch, f.duration_secs) {
        (None, _) => Some(d.to_owned()),
        (Some(_), Some(elapsed)) => after_switch(f.now - elapsed).then(|| d.to_owned()),
        (Some(_), None) => None,
    }
}

/// Record config dir `dir`'s `accountUuid` in its own
/// `<state>/last-identity-<tag>`; a change in that dir is a switch event and
/// re-stamps `.last-switch` (the cooldown) and `.last-identity-switch-<tag>`
/// (attribution). Returns that dir's last identity switch event (epoch), if
/// any. The first sighting of a dir records the identity without counting as
/// a switch. Keyed per dir so ticks from a straggler on a legacy dir and
/// sessions on `D` never look like a switch to each other.
pub(crate) fn note_identity(dir: &Path, uuid: Option<&str>, now: i64) -> Option<i64> {
    let path = crate::paths::last_identity(dir);
    let seen = std::fs::read_to_string(&path).ok();
    if let Some(uuid) = uuid.filter(|u| !u.is_empty())
        && seen.as_deref().map(str::trim) != Some(uuid)
    {
        let _ = crate::orca::fsx::write_atomic(
            &path,
            uuid.as_bytes(),
            crate::orca::fsx::WriteOpts::PRIVATE,
        );
        if seen.is_some() {
            let _ = std::fs::write(crate::paths::last_switch(), now.to_string());
            let _ = std::fs::write(crate::paths::last_identity_switch(dir), now.to_string());
        }
    }
    std::fs::read_to_string(crate::paths::last_identity_switch(dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// The `born` stamp of a (re)launch on `D`'s identity `uuid`: note the
/// identity first, so a switch that moved `D` since the last statusLine
/// tick (a pre-launch switch, `accounts use`, Orca's GUI) is a switch event
/// at or before this launch, not one the session's own first tick stamps
/// after its `born` (which would drop every capture it makes). Returns the
/// epoch to record as `born`.
pub fn launch_born(accounts: &AccountSet) -> i64 {
    let now = crate::epoch::now_secs() as i64;
    let _ = note_identity(&accounts.runtime_dir, accounts.current_uuid.as_deref(), now);
    now
}

/// Build the `StoreRecord` a statusline capture writes. Pure (no I/O) so the
/// one rule that matters here — `api_captured_at` carries the PRIOR record's
/// value forward unchanged, never `now` — is unit-testable directly.
///
/// A statusline capture is NOT a live api probe. If this instead bumped
/// `api_captured_at` to `now`, `collect()`'s freshness gate would treat the
/// account as freshly api-probed and never re-probe, freezing
/// `week_fable`/`week_model_label` (only an api probe can refresh them).
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
        let (res, needs_cooldown) = resolve(true, Event::InCooldown { until: 123 }, "home");
        assert!(matches!(res, Resolution::ServeStale(None)));
        assert!(!needs_cooldown);
    }

    #[test]
    fn resolve_in_cooldown_without_stale_fails_with_until() {
        let (res, _) = resolve(false, Event::InCooldown { until: 999 }, "home");
        match res {
            Resolution::Fail(msg) => assert!(msg.contains("999"), "{msg}"),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn resolve_no_creds_is_needs_login_pointing_at_orca() {
        let (res, needs_cooldown) = resolve(false, Event::NoCreds, "work");
        assert!(!needs_cooldown);
        match res {
            Resolution::NeedsLogin {
                errors_msg,
                attention,
            } => {
                assert!(errors_msg.contains("not logged in"), "{errors_msg}");
                assert!(errors_msg.contains("login required"), "{errors_msg}");
                assert_eq!(attention.kind, AttentionKind::NeedsLogin);
                assert_eq!(attention.message, "not logged in");
                assert_eq!(attention.action, login_action());
                assert!(attention.action.contains("Orca"), "{}", attention.action);
                assert!(attention.since_epoch.is_none(), "no known expiry instant");
            }
            other => panic!("expected NeedsLogin, got {other:?}"),
        }
    }

    #[test]
    fn resolve_no_creds_is_needs_login_regardless_of_stale() {
        // NoCreds is unconditionally NeedsLogin — `has_stale` only changes
        // what `apply_resolution` attaches the attention to, not the verdict.
        let (res, _) = resolve(true, Event::NoCreds, "work");
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
        );
        assert!(!needs_cooldown);
        match res {
            Resolution::NeedsRefresh { attention } => {
                assert_eq!(attention.kind, AttentionKind::NeedsRefresh);
                assert_eq!(attention.message, "access token expired");
                assert_eq!(attention.action, "csm accounts use home");
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
            resolve(true, event(), "home").0,
            Resolution::NeedsRefresh { .. }
        ));
        assert!(matches!(
            resolve(false, event(), "home").0,
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
        );
        match res {
            Resolution::NeedsLogin {
                errors_msg,
                attention,
            } => {
                assert!(errors_msg.contains("login required"), "{errors_msg}");
                assert_eq!(attention.kind, AttentionKind::NeedsLogin);
                assert_eq!(attention.message, "credentials expired");
                assert_eq!(attention.action, login_action());
                assert!(attention.action.contains("Orca"), "{}", attention.action);
                assert_eq!(attention.since_epoch, Some(1_756_000_000));
            }
            other => panic!("expected NeedsLogin, got {other:?}"),
        }
    }

    #[test]
    fn resolve_api_unauthorized_is_needs_login_conservatively() {
        // 401/403: refresh-liveness is unknown, so this is NeedsLogin — see
        // the module doc's "Dead-credential warnings" section.
        let (with_stale, _) = resolve(true, Event::ApiUnauthorized, "work");
        let (without_stale, _) = resolve(false, Event::ApiUnauthorized, "work");
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
        let (r1, _) = resolve(true, Event::CredsOther("boom".into()), "home");
        assert!(matches!(r1, Resolution::ServeStale(Some(m)) if m == "boom"));
        let (r2, _) = resolve(false, Event::ApiOther("kaboom".into()), "home");
        assert!(matches!(r2, Resolution::Fail(m) if m == "kaboom"));
    }

    #[test]
    fn resolve_api_ok_always_persists_regardless_of_stale() {
        let usage = ProfileUsage {
            source: Some("api".to_string()),
            ..Default::default()
        };
        let (res, needs_cooldown) = resolve(true, Event::ApiOk(Box::new(usage.clone())), "home");
        assert!(matches!(res, Resolution::Persist(_)));
        assert!(!needs_cooldown);
    }

    #[test]
    fn resolve_rate_limited_with_stale_serves_stale_and_signals_cooldown() {
        let (res, needs_cooldown) = resolve(true, Event::ApiRateLimited, "home");
        assert!(matches!(res, Resolution::ServeStale(None)));
        assert!(needs_cooldown);
    }

    #[test]
    fn resolve_rate_limited_without_stale_fails_and_signals_cooldown() {
        let (res, needs_cooldown) = resolve(false, Event::ApiRateLimited, "home");
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
        let (resolution, _) = resolve(false, Event::NoCreds, "work");

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

    /// The limit pick runs this collector from an Orca pane's supervisor:
    /// inside `capture_warnings` a failed probe's hint is handed back (the
    /// hop logs it) instead of being printed into the pane.
    #[test]
    fn a_stale_serve_hint_is_captured_not_printed_under_capture_warnings() {
        let (resolution, _) = resolve(true, Event::ApiOther("boom".into()), "work");
        assert!(matches!(resolution, Resolution::ServeStale(Some(_))));
        let mut out = UsageData::default();
        let mut errors: HashMap<String, String> = HashMap::new();
        let mut newest_captured: Option<(i64, String)> = None;
        let ((), warnings) = crate::usage::capture_warnings(|| {
            apply_resolution(
                "work",
                &None,
                resolution,
                None,
                now(),
                &mut out,
                &mut errors,
                &mut newest_captured,
            )
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("boom"), "{warnings:?}");
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
                action: "csm accounts use home".to_string(),
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

    // ── attribute_capture / note_identity ───────────────────────────────────

    #[test]
    fn a_supervised_session_counts_when_born_after_the_last_switch() {
        let f = CaptureFacts {
            sidecar_account: Some("id-a"),
            sidecar_born: Some(200),
            d_account: Some("id-b"),
            last_switch: Some(100),
            now: 300,
            ..Default::default()
        };
        assert_eq!(attribute_capture(&f).as_deref(), Some("id-a"));
        let stale = CaptureFacts {
            sidecar_born: Some(50),
            ..f
        };
        assert_eq!(
            attribute_capture(&stale),
            None,
            "launched before the switch"
        );
    }

    #[test]
    fn an_unsupervised_session_counts_for_d_by_its_own_elapsed_time() {
        let base = CaptureFacts {
            d_account: Some("id-b"),
            now: 1_000,
            ..Default::default()
        };
        assert_eq!(
            attribute_capture(&base).as_deref(),
            Some("id-b"),
            "no switch event ever: D's account"
        );
        let after = CaptureFacts {
            last_switch: Some(900),
            duration_secs: Some(50),
            ..base
        };
        assert_eq!(attribute_capture(&after).as_deref(), Some("id-b"));
        let before = CaptureFacts {
            duration_secs: Some(500),
            ..after
        };
        assert_eq!(
            attribute_capture(&before),
            None,
            "started before the switch"
        );
        let unknown = CaptureFacts {
            duration_secs: None,
            ..after
        };
        assert_eq!(attribute_capture(&unknown), None, "no elapsed time: drop");
        let no_d = CaptureFacts {
            d_account: None,
            ..base
        };
        assert_eq!(attribute_capture(&no_d), None);
    }

    fn set(dir: &Path, uuid: &str, id: &str) -> AccountSet {
        AccountSet {
            current: Some(id.into()),
            current_uuid: Some(uuid.into()),
            runtime_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    const TICK: &str = r#"{"rate_limits":{"five_hour":{"used_percentage":42.0,"resets_at":1788339599},"seven_day":{"used_percentage":31.0,"resets_at":1788339599}}}"#;

    /// A straggler on a legacy dir ticks next to a session on `D`, each on
    /// its own identity: neither tick is a switch, so nothing stamps the
    /// cooldown and both captures count.
    #[test]
    fn interleaved_ticks_from_two_dirs_are_not_a_switch() {
        let home = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(home.path(), || {
            std::fs::create_dir_all(crate::paths::smart_dir_no_create()).unwrap();
            let a = set(&home.path().join("legacy"), "uuid-a", "id-a");
            let b = set(&home.path().join("orca-d"), "uuid-b", "id-b");
            for _ in 0..3 {
                let ca = record_statusline_payload_in(TICK, &a).unwrap();
                let cb = record_statusline_payload_in(TICK, &b).unwrap();
                assert_eq!(ca.unwrap().account_id, "id-a");
                assert_eq!(cb.unwrap().account_id, "id-b");
            }
            assert!(!crate::paths::last_switch().exists(), "no switch stamped");
            assert!(store::load("id-a").is_some_and(|r| r.usage.is_some()));
            assert!(store::load("id-b").is_some_and(|r| r.usage.is_some()));
            // A real change inside one dir still stamps, and only for it.
            let a2 = set(&home.path().join("legacy"), "uuid-c", "id-c");
            record_statusline_payload_in(TICK, &a2).unwrap();
            assert!(crate::paths::last_switch().exists());
            assert!(!crate::paths::last_identity_switch(&b.runtime_dir).exists());
        });
    }

    #[test]
    fn an_identity_change_stamps_a_switch_event() {
        let home = tempfile::tempdir().unwrap();
        let d = home.path().join("d");
        crate::testenv::with_test_home(home.path(), || {
            std::fs::create_dir_all(crate::paths::smart_dir_no_create()).unwrap();
            assert_eq!(
                note_identity(&d, Some("uuid-a"), 100),
                None,
                "first sighting"
            );
            assert_eq!(note_identity(&d, Some("uuid-a"), 150), None, "unchanged");
            assert_eq!(note_identity(&d, Some("uuid-b"), 200), Some(200), "changed");
            assert_eq!(
                note_identity(&d, None, 300),
                Some(200),
                "unknown keeps the last"
            );
            assert_eq!(
                std::fs::read_to_string(crate::paths::last_identity(&d)).unwrap(),
                "uuid-b"
            );
            // A cooldown claim re-stamps `.last-switch` without a switch:
            // attribution does not move.
            std::fs::write(crate::paths::last_switch(), "400").unwrap();
            assert_eq!(note_identity(&d, Some("uuid-b"), 450), Some(200));
        });
    }

    /// A launch after a switch no statusLine tick saw (a pre-launch switch,
    /// `accounts use`, Orca's GUI): the launch notes `D`'s identity before
    /// stamping `born`, so the session's own first tick finds no new switch
    /// event and its captures count.
    #[test]
    fn a_launch_after_an_unseen_switch_keeps_its_captures() {
        let home = tempfile::tempdir().unwrap();
        let d = home.path().join("d");
        crate::testenv::with_test_home(home.path(), || {
            std::fs::create_dir_all(crate::paths::smart_dir_no_create()).unwrap();
            // An earlier session ticked on a.
            assert_eq!(note_identity(&d, Some("uuid-a"), 100), None);
            // D moved to b with no tick in between; the new session launches.
            let born = launch_born(&set(&d, "uuid-b", "id-b"));
            // Its first tick, at or after born, sees b.
            let last_switch = note_identity(&d, Some("uuid-b"), born + 5);
            assert!(last_switch.is_some_and(|s| s <= born), "{last_switch:?}");
            let f = CaptureFacts {
                sidecar_account: Some("id-b"),
                sidecar_born: Some(born),
                d_account: Some("id-b"),
                last_switch,
                now: born + 5,
                ..Default::default()
            };
            assert_eq!(attribute_capture(&f).as_deref(), Some("id-b"));
        });
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
    // `should_throttle`/`resolve_account_key`/`statusline::to_profile_usage`
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
        crate::testenv::with_env_var("CLAUDE_CONFIG_DIR", None, || {
            let result = record_statusline_payload(
                r#"{"rate_limits": {"five_hour": {"used_percentage": 1.0}}}"#,
            );
            assert!(result.unwrap().is_none());
        });
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
    fn collect_skips_invalid_account_id_without_touching_store() {
        let set = AccountSet {
            accounts: vec![crate::account::accounts::AccountEntry {
                id: "../evil".to_string(),
                email: None,
                organization_name: None,
                managed_auth_path: None,
            }],
            current: Some("../evil".to_string()),
            runtime_dir: std::path::PathBuf::from("/tmp/does-not-matter"),
            ..AccountSet::default()
        };

        let home = tempfile::tempdir().unwrap();
        let data = crate::testenv::with_test_home(home.path(), || {
            collect(&set, now(), &CollectOpts::standard())
        });

        assert!(
            data.profiles.is_empty(),
            "an invalid id must never produce a served account"
        );
        let errors = data.errors.expect("invalid id must record an error");
        assert!(errors.contains_key("../evil"));
    }
    // ── cache-only collection (design decision 8) ─────────────────────────

    /// Two host accounts in a temp Orca store, `D` holding `id-a`, and no
    /// store record for either (so a probing pass would probe both).
    fn two_account_set(home: &std::path::Path) -> AccountSet {
        use crate::orca::userdata::HostOs;
        let mut env = crate::orca::HostEnv::for_test(home, HostOs::Linux);
        let d = home.join("claude-d");
        env.claude_config_dir = Some(d.to_string_lossy().into_owned());
        let ud = home.join(".config/orca");
        let file = ud.join("profiles/local-default/orca-data.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            concat!(
                r#"{"schemaVersion":1,"settings":{"claudeManagedAccounts":["#,
                r#"{"id":"id-a","email":"alice@example.com","managedAuthRuntime":"host"},"#,
                r#"{"id":"id-b","email":"bob@example.com","managedAuthRuntime":"host"}"#,
                r#"],"activeClaudeManagedAccountId":"id-a","activeClaudeManagedAccountIdsByRuntime":{"host":"id-a","wsl":{}}}}"#
            ),
        )
        .unwrap();
        crate::orca::testsupport::make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"u-a"}}"#,
        )
        .unwrap();
        crate::account::AccountSet::load_with(&env)
    }

    /// `cache_only` reads the store and nothing else, while the standard
    /// pass over the same set reaches the Keychain for the active grant (a
    /// macOS host; the cfg(test) runner refuses the real binary).
    #[test]
    fn cache_only_collect_reaches_no_probe_step() {
        let home = tempfile::tempdir().unwrap();
        let mut set = two_account_set(home.path());
        assert_eq!(set.current.as_deref(), Some("id-a"));
        if let Some(h) = set.host.as_mut() {
            h.os = crate::orca::userdata::HostOs::MacOs;
        }
        crate::usage::reach::take();
        let (cached, standard) = crate::testenv::with_test_home(home.path(), || {
            store::save(
                "id-b",
                &store::StoreRecord {
                    captured_at: Some("2026-09-01T00:00:00Z".into()),
                    usage: Some(ProfileUsage {
                        captured_at: Some("2026-09-01T00:00:00Z".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            let cached = collect(&set, now(), &CollectOpts::cache_only());
            let seen = crate::usage::reach::take();
            let _ = collect(&set, now(), &CollectOpts::standard());
            ((cached, seen), crate::usage::reach::take())
        });
        let (cached, seen) = cached;
        assert_eq!(seen, Vec::<&str>::new(), "cache_only reached a probe step");
        assert!(
            cached.profiles.contains_key("id-b"),
            "the stored record is served"
        );
        assert!(
            !cached.profiles.contains_key("id-a"),
            "nothing stored, nothing probed"
        );
        assert!(!cached.any_probe_attempted);
        assert!(standard.contains(&"keychain"), "control: {standard:?}");
    }

    #[test]
    fn a_stash_refresh_never_targets_the_grant_d_holds() {
        let d = vec!["rt-d".to_owned()];
        // A plain inactive account.
        assert!(stash_refresh_guard("id-b", Some("id-a"), Some("rt-b"), Ok(&d)).is_ok());
        assert!(stash_refresh_guard("id-b", None, None, Ok(&[])).is_ok());
        // The store's active account, even when D could not be mapped.
        assert!(stash_refresh_guard("id-a", Some("id-a"), Some("rt-a"), Ok(&[])).is_err());
        // D holds this very grant (a neutral D, one user in two orgs).
        assert!(stash_refresh_guard("id-b", Some("id-a"), Some("rt-d"), Ok(&d)).is_err());
        // D unreadable: refused.
        assert!(stash_refresh_guard("id-b", Some("id-a"), Some("rt-b"), Err(())).is_err());
    }

    #[test]
    fn runtime_refresh_tokens_read_d_off_macos() {
        let home = tempfile::tempdir().unwrap();
        let set = two_account_set(home.path());
        let mut h = set.host.clone().unwrap();
        h.os = crate::orca::userdata::HostOs::Linux;
        std::fs::write(
            &h.paths.credentials_path,
            r#"{"claudeAiOauth":{"accessToken":"at-x","refreshToken":"rt-x"}}"#,
        )
        .unwrap();
        assert_eq!(runtime_refresh_tokens(&h).unwrap(), vec!["rt-x".to_owned()]);
        std::fs::remove_file(&h.paths.credentials_path).unwrap();
        assert!(runtime_refresh_tokens(&h).unwrap().is_empty());
    }

    /// The limit-switch leader holds `switch.lock` while it collects: the
    /// refresh must reuse it, not wait out a second take against itself.
    #[test]
    fn a_held_switch_lock_is_reused_by_the_stash_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let held =
            crate::orca::fsx::SwitchLock::acquire(tmp.path(), std::time::Duration::from_secs(2))
                .unwrap();
        let t = std::time::Instant::now();
        assert!(
            lock_for_refresh(tmp.path(), true, std::time::Duration::from_secs(10))
                .unwrap()
                .is_none()
        );
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        // Taking it again in the same process blocks against the holder.
        assert!(lock_for_refresh(tmp.path(), false, std::time::Duration::from_millis(50)).is_err());
        drop(held);
        assert!(
            lock_for_refresh(tmp.path(), false, std::time::Duration::from_secs(2))
                .unwrap()
                .is_some()
        );
        let o = crate::usage::transport::limit_pick_opts();
        assert!(o.refresh_stash && o.switch_lock_held);
        assert!(!CollectOpts::standard().switch_lock_held);
    }
}
