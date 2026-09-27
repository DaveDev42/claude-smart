//! Read-back: attribute the grant Claude Code left in `D` before csm
//! overwrites it (design section 3 step 3), ported from Orca 1.4.209
//! G7i.readBackRefreshedTokens (M:247472-247536) as a freshly started Orca
//! runs it (`lastWrittenCredentialsJson = null`).
//!
//! Pure core:
//! - the credential readers (R7i, M:247107-247200): [`identity_from_credentials`],
//!   [`read_freshness`] (readNumber with JS `Number()` semantics,
//!   [`js_number_from_str`]), [`refresh_token`], [`compare_refresh_tokens`];
//! - the matcher runtimeCredentialsMatchAccount (M:247446-247459), verbatim
//!   ([`match_account`]) and findManagedAccountForRuntimeCredentials
//!   ([`find_managed`]);
//! - acceptance ([`accepts`], both `lastWritten` branches) and
//!   chooseFreshestReadBackCandidate ([`choose_freshest`]);
//! - [`classify`], which runs all of it over the candidates.
//!
//! Shell ([`read_back`]): gathers the candidates (macOS: the scoped item, the
//! unscoped item, `D/.credentials.json`; elsewhere the file), drops those
//! equal to the last-synced account's stash, classifies, and then applies
//! csm's profile veto to the chosen candidate (`GET /api/oauth/profile`, its
//! `account.uuid` must equal the matched stash's `accountUuid`):
//! - equal: re-read the stash, write the candidate only if the stash is
//!   still what the matcher saw;
//! - another uuid, no uuid, or a stash without one: quarantine;
//! - 401: with no live claude in `D`, quarantine, refresh once as Orca does,
//!   quarantine the rotated grant, profile it and file it by that answer;
//!   with a live claude, quarantine only;
//! - no HTTP answer, or a status other than 200/401: abort with
//!   [`OrcaError::Network`] before anything else is written.
//!
//! Everything the matcher could not attribute (`none`, `ambiguous`), and
//! every accepted candidate that lost to a fresher one, goes to the
//! quarantine; csm never drops a grant whose refresh token it has not
//! stored. Orca ignores both (the grant is lost at the next materialize).

use std::path::Path;

use serde_json::Value;

use super::http::{self, OauthHttp, ProfileAnswer};
use super::keychain::{self, KeychainUser};
use super::quarantine::{Filed, Quarantine, Reason};
use super::record::AccountRecord;
use super::refresh;
use super::runtime::{OauthIdentity, RuntimePaths, read_json_object};
use super::stash::{self, Stash, StashError};
use super::userdata::HostOs;
use super::{OrcaError, SecretString};

/// Cap on `D/.credentials.json`.
const CREDS_CAP: u64 = 1024 * 1024;

// ─── credential readers ───────────────────────────────────────────────────────

/// A JS exception the ported code would throw (`null.claudeAiOauth`). Orca
/// catches it at the top of read-back and rejects the whole read-back; csm
/// aborts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a credential is the JSON value null")]
pub struct JsThrow;

/// readIdentityFromCredentials.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CredIdentity {
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

/// normalizeField: trimmed, blank as null.
fn normalize(s: Option<&str>) -> Option<String> {
    let t = s?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// `a ?? b` over readString: a present string (even empty) wins.
fn read_str2<'a>(o: Option<&'a Value>, a: &str, b: &str) -> Option<&'a str> {
    let o = o?;
    o.get(a)
        .and_then(Value::as_str)
        .or_else(|| o.get(b).and_then(Value::as_str))
}

/// asRecord(parsed.claudeAiOauth); `Err` when `parsed` is `null`.
fn oauth_record(parsed: &Value) -> Result<Option<&Value>, JsThrow> {
    if parsed.is_null() {
        return Err(JsThrow);
    }
    Ok(parsed.get("claudeAiOauth").filter(|o| o.is_object()))
}

/// readIdentityFromCredentials: `Ok(None)` when `json` does not parse.
/// Pure.
pub fn identity_from_credentials(json: &str) -> Result<Option<CredIdentity>, JsThrow> {
    let Ok(parsed) = serde_json::from_str::<Value>(json) else {
        return Ok(None);
    };
    let n = oauth_record(&parsed)?;
    Ok(Some(CredIdentity {
        account_uuid: normalize(read_str2(n, "accountUuid", "accountId")),
        email: normalize(n.and_then(|o| o.get("email")).and_then(Value::as_str)),
        organization_uuid: normalize(read_str2(n, "organizationUuid", "organizationId")),
    }))
}

/// JS `Number(s)` for a string: whitespace trimmed, `""` is 0, `0x`/`0o`/
/// `0b` integers, `Infinity`, else a decimal literal; `None` for NaN and
/// the infinities (readNumber wants a finite number). Pure.
pub fn js_number_from_str(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return Some(0.0);
    }
    let radix = |p: &str, r: u32| -> Option<f64> {
        if p.is_empty() || !p.chars().all(|c| c.is_digit(r)) {
            return None;
        }
        let mut acc = 0f64;
        for c in p.chars() {
            acc = acc * r as f64 + c.to_digit(r)? as f64;
        }
        acc.is_finite().then_some(acc)
    };
    for (pre, r) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(rest) = t.strip_prefix(pre) {
            return radix(rest, r);
        }
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int, frac) = match mant.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (mant, None),
    };
    let digits = |x: &str| x.chars().all(|c| c.is_ascii_digit());
    if !digits(int)
        || !frac.is_none_or(digits)
        || (int.is_empty() && frac.is_none_or(str::is_empty))
    {
        return None;
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !digits(e) {
            return None;
        }
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// readNumber. Pure.
fn read_number(o: Option<&Value>, key: &str) -> Option<f64> {
    match o?.get(key)? {
        Value::Number(n) => n.to_string().parse::<f64>().ok().filter(|f| f.is_finite()),
        Value::String(s) => js_number_from_str(s),
        _ => None,
    }
}

/// readFreshnessFromCredentials: the first of `expiresAt`, `expires_at`,
/// `expiry`, `expires`. `None` for unparseable text or `null`. Pure.
pub fn read_freshness(json: &str) -> Option<f64> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    let n = oauth_record(&parsed).ok()?;
    ["expiresAt", "expires_at", "expiry", "expires"]
        .iter()
        .find_map(|k| read_number(n, k))
}

/// readRefreshTokenFromCredentials: trimmed, blank as `None`. Pure.
pub fn refresh_token(json: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    let n = oauth_record(&parsed).ok()?;
    normalize(n?.get("refreshToken").and_then(Value::as_str))
}

/// compareRefreshTokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtCompare {
    Missing,
    Same,
    Different,
}

/// Pure.
pub fn compare_refresh_tokens(a: &str, b: &str) -> RtCompare {
    match (refresh_token(a), refresh_token(b)) {
        (Some(x), Some(y)) if x == y => RtCompare::Same,
        (Some(_), Some(_)) => RtCompare::Different,
        _ => RtCompare::Missing,
    }
}

/// The access token of a grant, trimmed.
pub fn access_token(json: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    let n = oauth_record(&parsed).ok()?;
    normalize(n?.get("accessToken").and_then(Value::as_str))
}

// ─── the matcher ──────────────────────────────────────────────────────────────

/// runtimeCredentialsMatchAccount's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Match,
    Mismatch,
    Unverifiable,
}

/// The record fields the matcher reads.
#[derive(Debug, Clone, Copy)]
pub struct RecordFacts<'a> {
    pub email: Option<&'a str>,
    pub organization_uuid: Option<&'a str>,
}

impl<'a> RecordFacts<'a> {
    pub fn of(r: &'a AccountRecord) -> RecordFacts<'a> {
        RecordFacts {
            email: r.email.as_deref(),
            organization_uuid: r.organization_uuid.as_deref(),
        }
    }
}

/// runtimeCredentialsMatchAccount(e = `cand`, t = `runtime_oauth`, n =
/// `record`, r = `record_creds`, i = `record_oauth`), transcribed from
/// M:247446-247459. Pure.
pub fn match_account(
    cand: &str,
    runtime_oauth: Option<&Value>,
    record: RecordFacts<'_>,
    record_creds: &str,
    record_oauth: Option<&Value>,
) -> Result<Verdict, JsThrow> {
    use Verdict::*;
    let Some(a) = identity_from_credentials(cand)? else {
        return Ok(Mismatch);
    };
    let o = identity_from_credentials(record_creds)?;
    let s = record_oauth
        .map(OauthIdentity::from_value)
        .unwrap_or_default();
    let c = runtime_oauth
        .map(OauthIdentity::from_value)
        .unwrap_or_default();
    let clash =
        |x: &Option<String>, y: &Option<String>| matches!((x, y), (Some(x), Some(y)) if x != y);
    if clash(&a.account_uuid, &c.account_uuid)
        || clash(&a.email, &c.email)
        || clash(&a.organization_uuid, &c.organization_uuid)
    {
        return Ok(Mismatch);
    }
    let l = match record.organization_uuid {
        Some(v) => normalize(Some(v)),
        None => o
            .as_ref()
            .and_then(|o| o.organization_uuid.clone())
            .or_else(|| s.organization_uuid.clone()),
    };
    let u = s.account_uuid.is_some()
        && s.account_uuid == c.account_uuid
        && (c.email.is_some() || c.organization_uuid.is_some());
    let d = a.email.clone().or_else(|| c.email.clone());
    let f = a
        .organization_uuid
        .clone()
        .or_else(|| c.organization_uuid.clone());
    let p = compare_refresh_tokens(cand, record_creds);
    let same = p == RtCompare::Same;
    Ok(if let Some(d) = d {
        let email_differs = record
            .email
            .filter(|e| !e.is_empty())
            .is_some_and(|e| normalize(Some(e)).as_deref() != Some(d.as_str()));
        if email_differs {
            Mismatch
        } else if l.is_some() && f.is_none() {
            if same || u { Match } else { Unverifiable }
        } else if l.is_some() && f.is_some() && l != f {
            Mismatch
        } else if l.is_none() && f.is_some() {
            if same { Match } else { Unverifiable }
        } else {
            Match
        }
    } else if same {
        Match
    } else if let Some(ao) = &a.organization_uuid {
        if l.as_ref().is_some_and(|l| l != ao) {
            Mismatch
        } else {
            Unverifiable
        }
    } else if u {
        Match
    } else if f.is_none() && p == RtCompare::Different {
        Mismatch
    } else {
        Unverifiable
    })
}

/// One record as findManagedAccountForRuntimeCredentials sees it.
#[derive(Debug)]
pub struct RecordInput<'a> {
    pub record: &'a AccountRecord,
    /// The stashed grant; `None` skips the record (Orca's `if (!a)
    /// continue`).
    pub creds: Option<SecretString>,
    pub oauth: Option<Value>,
    /// csm cannot read this record's stash (a WSL account): it counts as
    /// unverifiable whenever it has a grant, so it can never be silently
    /// left out of a unique match.
    pub opaque: bool,
}

/// findManagedAccountForRuntimeCredentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The index into the records.
    Matched(usize),
    None,
    Ambiguous,
}

/// Pure.
pub fn find_managed(
    cand: &str,
    runtime_oauth: Option<&Value>,
    records: &[RecordInput<'_>],
) -> Result<Found, JsThrow> {
    let mut hits = Vec::new();
    let mut unverifiable = 0;
    for (i, r) in records.iter().enumerate() {
        if r.opaque {
            unverifiable += 1;
            continue;
        }
        let Some(creds) = &r.creds else { continue };
        match match_account(
            cand,
            runtime_oauth,
            RecordFacts::of(r.record),
            creds.expose(),
            r.oauth.as_ref(),
        )? {
            Verdict::Match => hits.push(i),
            Verdict::Unverifiable => unverifiable += 1,
            Verdict::Mismatch => {}
        }
    }
    Ok(match (hits.len(), unverifiable) {
        (1, 0) => Found::Matched(hits[0]),
        (0, 0) => Found::None,
        _ => Found::Ambiguous,
    })
}

// ─── acceptance ───────────────────────────────────────────────────────────────

fn fresher(a: &str, b: &str) -> bool {
    matches!((read_freshness(a), read_freshness(b)), (Some(x), Some(y)) if x > y)
}

fn older(a: &str, b: &str) -> bool {
    matches!((read_freshness(a), read_freshness(b)), (Some(x), Some(y)) if x < y)
}

/// Whether a matched candidate is persisted. With `last_written_null` (a
/// fresh Orca, always csm's case): fresher, or a different refresh token
/// that is not older. Otherwise: anything not older. Pure.
pub fn accepts(cand: &str, managed: &str, last_written_null: bool) -> bool {
    if last_written_null {
        let r = compare_refresh_tokens(cand, managed) == RtCompare::Different;
        fresher(cand, managed) || (r && !older(cand, managed))
    } else {
        !older(cand, managed)
    }
}

/// chooseFreshestReadBackCandidate: a later candidate wins only when its
/// freshness is known and the current pick's is unknown or lower. Returns
/// the index. `None` for an empty list. Pure.
pub fn choose_freshest<S: AsRef<str>>(cands: &[S]) -> Option<usize> {
    let mut best = 0;
    if cands.is_empty() {
        return None;
    }
    for (i, c) in cands.iter().enumerate().skip(1) {
        let n = read_freshness(c.as_ref());
        let r = read_freshness(cands[best].as_ref());
        if n.is_some_and(|n| r.is_none_or(|r| n > r)) {
            best = i;
        }
    }
    Some(best)
}

// ─── classification ───────────────────────────────────────────────────────────

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    ScopedKeychain,
    LegacyKeychain,
    File,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::ScopedKeychain => "scoped-keychain",
            Source::LegacyKeychain => "legacy-keychain",
            Source::File => "file",
        }
    }
}

/// One runtime grant.
#[derive(Debug)]
pub struct Candidate {
    pub source: Source,
    pub json: SecretString,
}

/// Add in order, dropping exact duplicates and anything equal to `exclude`
/// (the last-synced account's stash). Pure.
pub fn dedupe(raw: Vec<Candidate>, exclude: Option<&str>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for c in raw {
        if c.json.is_empty() || out.iter().any(|o| o.json.expose() == c.json.expose()) {
            continue;
        }
        out.push(c);
    }
    out.retain(|c| Some(c.json.expose()) != exclude);
    out
}

/// What the matcher made of one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Not a valid credential (Orca skips it).
    Invalid,
    None,
    Ambiguous,
    /// Matched record `rec` and accepted.
    Accepted {
        rec: usize,
    },
    /// Matched record `rec` but not accepted (older, or no newer and same
    /// refresh token).
    Stale {
        rec: usize,
    },
}

/// The pure read-back decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// One per candidate, in order.
    pub outcomes: Vec<Outcome>,
    /// The candidate chooseFreshest picks among the accepted ones.
    pub chosen: Option<usize>,
}

/// Run the matcher and the acceptance rules over `cands` as a fresh Orca
/// would. Pure.
pub fn classify(
    cands: &[&str],
    runtime_oauth: Option<&Value>,
    records: &[RecordInput<'_>],
) -> Result<Classified, JsThrow> {
    let mut outcomes = Vec::with_capacity(cands.len());
    for c in cands {
        if !stash::credentials_are_valid(c) {
            outcomes.push(Outcome::Invalid);
            continue;
        }
        outcomes.push(match find_managed(c, runtime_oauth, records)? {
            Found::None => Outcome::None,
            Found::Ambiguous => Outcome::Ambiguous,
            Found::Matched(rec) => {
                let managed = records[rec]
                    .creds
                    .as_ref()
                    .map(|s| s.expose())
                    .unwrap_or("");
                if accepts(c, managed, true) {
                    Outcome::Accepted { rec }
                } else {
                    Outcome::Stale { rec }
                }
            }
        });
    }
    let accepted: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Outcome::Accepted { .. }))
        .map(|(i, _)| i)
        .collect();
    let chosen = choose_freshest(&accepted.iter().map(|&i| cands[i]).collect::<Vec<_>>())
        .map(|k| accepted[k]);
    Ok(Classified { outcomes, chosen })
}

// ─── the shell ────────────────────────────────────────────────────────────────

/// Everything [`read_back`] needs.
pub struct ReadBack<'a> {
    pub os: HostOs,
    pub user_data: &'a Path,
    pub paths: &'a RuntimePaths,
    pub keychain_user: &'a KeychainUser,
    pub records: &'a [AccountRecord],
    /// The last-synced account's stashed grant (Orca's `e`).
    pub exclude: Option<&'a str>,
    /// A live claude is registered in `D` (no refresh then).
    pub live_claude: bool,
    pub http: &'a dyn OauthHttp,
    pub quarantine: &'a Quarantine,
    pub now_ms: i64,
    /// Migration read-back of a retired config dir (design §9 step 4):
    /// every Keychain spelling of the dir is a candidate
    /// ([`keychain::dir_spellings`]) and the shared unscoped item is not,
    /// since it may belong to any profile.
    pub migration: bool,
}

/// What [`read_back`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadBackReport {
    /// Candidates considered after dedupe.
    pub candidates: usize,
    /// The account whose stash now holds a runtime grant.
    pub persisted: Option<String>,
    /// Quarantine entries this run filed.
    pub quarantined: Vec<(String, Reason)>,
}

/// Gather the runtime grants in Orca's order.
pub fn gather(rb: &ReadBack<'_>) -> Result<Vec<Candidate>, OrcaError> {
    let mut raw = Vec::new();
    if rb.os == HostOs::MacOs && rb.migration {
        let dir = rb.paths.config_dir.to_string_lossy().into_owned();
        for spelling in keychain::dir_spellings(&dir) {
            let svc = keychain::runtime_service(Some(&spelling));
            if let Some(v) = keychain::find_password(&svc, &rb.keychain_user.acct)? {
                raw.push(Candidate {
                    source: Source::ScopedKeychain,
                    json: v,
                });
            }
        }
    } else if rb.os == HostOs::MacOs {
        let dir = rb.paths.config_dir.to_string_lossy().into_owned();
        if let Some(v) = keychain::read_runtime_scoped(Some(&dir), rb.keychain_user)? {
            raw.push(Candidate {
                source: Source::ScopedKeychain,
                json: v,
            });
        }
        if let Some(v) = keychain::read_runtime_scoped(None, rb.keychain_user)? {
            raw.push(Candidate {
                source: Source::LegacyKeychain,
                json: v,
            });
        }
    }
    let p = &rb.paths.credentials_path;
    if let Some(bytes) =
        super::read_capped_bytes(p, CREDS_CAP).map_err(|e| OrcaError::io("cannot read", p, e))?
    {
        match String::from_utf8(bytes) {
            Ok(text) => raw.push(Candidate {
                source: Source::File,
                json: SecretString::new(text),
            }),
            Err(e) => {
                let mut b = e.into_bytes();
                super::zero(&mut b);
                return Err(OrcaError::Invalid(format!(
                    "{} is not UTF-8 text",
                    p.display()
                )));
            }
        }
    }
    Ok(dedupe(raw, rb.exclude))
}

fn stash_err(e: StashError) -> OrcaError {
    match e {
        StashError::Keychain(k) => OrcaError::Keychain(k),
        other => OrcaError::Invalid(format!("stash: {other}")),
    }
}

/// Read every record's stash the way findManagedAccountForRuntimeCredentials
/// does. A Keychain failure aborts (Orca's throw rejects the read-back).
fn record_inputs<'a>(rb: &ReadBack<'a>) -> Result<Vec<RecordInput<'a>>, OrcaError> {
    let mut out = Vec::new();
    for r in rb.records {
        if !r.is_host() {
            out.push(RecordInput {
                record: r,
                creds: None,
                oauth: None,
                opaque: true,
            });
            continue;
        }
        // getOwnedManagedAuthPath null (Q2i fails) skips the record. Orca
        // adopts a missing marker there and reads the stash, so csm reads it
        // too (without writing the marker).
        let Ok(s) = Stash::open_for_read(rb.user_data, &r.id, r.managed_auth_path.as_deref())
        else {
            out.push(RecordInput {
                record: r,
                creds: None,
                oauth: None,
                opaque: false,
            });
            continue;
        };
        let creds = match s.credentials(rb.os) {
            Ok(c) => c,
            Err(StashError::Keychain(k)) => return Err(OrcaError::Keychain(k)),
            // An unreadable file is Y4's throw: the read-back rejects.
            Err(e) => return Err(stash_err(e)),
        };
        out.push(RecordInput {
            record: r,
            creds,
            oauth: s.oauth_account().ok().flatten(),
            opaque: false,
        });
    }
    Ok(out)
}

/// Where the profile veto sends a grant.
enum Veto {
    /// The profile names the matched stash's account.
    Owner,
    /// Filed under this reason, with the answer.
    Quarantine(Reason, Option<(u16, Option<String>)>),
    /// 401.
    Unauthorized,
}

fn profile_veto(
    http: &dyn OauthHttp,
    grant: &str,
    stash_uuid: Option<&str>,
) -> Result<Veto, OrcaError> {
    let Some(at) = access_token(grant) else {
        return Ok(Veto::Quarantine(Reason::ProfileUnverifiable, None));
    };
    let reply = http
        .get_profile(&at)
        .map_err(|e| OrcaError::Network(e.to_string()))?;
    match http::parse_profile(&reply) {
        ProfileAnswer::Unauthorized => Ok(Veto::Unauthorized),
        ProfileAnswer::Other(s) => Err(OrcaError::Network(format!(
            "the profile endpoint answered {s}"
        ))),
        ProfileAnswer::Ok { account_uuid, .. } => Ok(match (account_uuid, stash_uuid) {
            (Some(p), Some(s)) if p == s => Veto::Owner,
            (Some(p), Some(_)) => Veto::Quarantine(Reason::ProfileMismatch, Some((200, Some(p)))),
            (p, _) => Veto::Quarantine(Reason::ProfileUnverifiable, Some((200, p))),
        }),
    }
}

impl ReadBack<'_> {
    fn file(
        &self,
        report: &mut ReadBackReport,
        grant: &str,
        reason: Reason,
        source: &str,
        account: Option<&str>,
        profile: Option<(u16, Option<String>)>,
    ) -> Result<Filed, OrcaError> {
        let filed = self.quarantine.file(
            grant,
            reason,
            source,
            account,
            profile.as_ref().map(|(s, u)| (*s, u.as_deref())),
            self.now_ms,
        )?;
        report
            .quarantined
            .push((filed.fingerprint().to_owned(), reason));
        Ok(filed)
    }

    /// Write `grant` into `rec`'s stash if the stash still holds `seen`;
    /// else quarantine it. Returns whether it was written.
    fn store_if_unchanged(
        &self,
        report: &mut ReadBackReport,
        rec: &AccountRecord,
        seen: &str,
        grant: &str,
        source: &str,
    ) -> Result<bool, OrcaError> {
        let s = Stash::open_for_write(self.user_data, &rec.id, rec.managed_auth_path.as_deref())
            .map_err(stash_err)?;
        let now = s.credentials(self.os).map_err(stash_err)?;
        if now.as_ref().map(|n| n.expose()) != Some(seen) {
            self.file(
                report,
                grant,
                Reason::StashChanged,
                source,
                Some(&rec.id),
                None,
            )?;
            return Ok(false);
        }
        s.write_credentials(self.user_data, self.os, grant)
            .map_err(stash_err)?;
        report.persisted = Some(rec.id.clone());
        Ok(true)
    }
}

/// Run the read-back. `Err(OrcaError::Network)` means the profile endpoint
/// gave no usable answer: nothing was written (a 401 path may already have
/// quarantined grants; the quarantine is csm's own state).
pub fn read_back(rb: &ReadBack<'_>) -> Result<ReadBackReport, OrcaError> {
    let cands = gather(rb)?;
    let mut report = ReadBackReport {
        candidates: cands.len(),
        ..Default::default()
    };
    if cands.is_empty() {
        return Ok(report);
    }
    let runtime = read_json_object(&rb.paths.config_path);
    let runtime_oauth = runtime.as_ref().and_then(|m| m.get("oauthAccount"));
    let records = record_inputs(rb)?;
    let texts: Vec<&str> = cands.iter().map(|c| c.json.expose()).collect();
    let cl =
        classify(&texts, runtime_oauth, &records).map_err(|e| OrcaError::Invalid(e.to_string()))?;

    // The chosen candidate first: its veto may abort before any write.
    if let Some(ci) = cl.chosen {
        let Outcome::Accepted { rec } = cl.outcomes[ci] else {
            unreachable!("chosen is accepted")
        };
        let input = &records[rec];
        let seen = input.creds.as_ref().map(|s| s.expose()).unwrap_or("");
        let stash_uuid = input
            .oauth
            .as_ref()
            .map(OauthIdentity::from_value)
            .and_then(|i| i.account_uuid);
        let grant = texts[ci];
        let source = cands[ci].source.as_str();
        match profile_veto(rb.http, grant, stash_uuid.as_deref())? {
            Veto::Owner => {
                rb.store_if_unchanged(&mut report, input.record, seen, grant, source)?;
            }
            Veto::Quarantine(reason, answer) => {
                rb.file(
                    &mut report,
                    grant,
                    reason,
                    source,
                    Some(&input.record.id),
                    answer,
                )?;
            }
            Veto::Unauthorized => {
                let dead = rb.file(
                    &mut report,
                    grant,
                    Reason::Unauthorized,
                    source,
                    Some(&input.record.id),
                    Some((401, None)),
                )?;
                if !rb.live_claude
                    && let Ok(rotated) = refresh::refresh_grant(grant, rb.http, rb.now_ms)
                {
                    let r = rb.file(
                        &mut report,
                        rotated.expose(),
                        Reason::Rotated,
                        "refresh",
                        Some(&input.record.id),
                        None,
                    )?;
                    match profile_veto(rb.http, rotated.expose(), stash_uuid.as_deref())? {
                        Veto::Owner => {
                            if rb.store_if_unchanged(
                                &mut report,
                                input.record,
                                seen,
                                rotated.expose(),
                                "refresh",
                            )? {
                                // Stored: both entries are attributed now.
                                rb.quarantine.remove(r.fingerprint())?;
                                rb.quarantine.remove(dead.fingerprint())?;
                                report.quarantined.retain(|(fp, _)| {
                                    fp != r.fingerprint() && fp != dead.fingerprint()
                                });
                            }
                        }
                        Veto::Quarantine(reason, answer) => {
                            rb.file(
                                &mut report,
                                rotated.expose(),
                                reason,
                                "refresh",
                                Some(&input.record.id),
                                answer,
                            )?;
                        }
                        Veto::Unauthorized => {}
                    }
                }
            }
        }
    }

    // Everything else that could hold a grant csm has not stored.
    for (i, o) in cl.outcomes.iter().enumerate() {
        if Some(i) == cl.chosen {
            continue;
        }
        let grant = texts[i];
        let source = cands[i].source.as_str();
        match *o {
            Outcome::Invalid => {
                if refresh_token(grant).is_some() {
                    rb.file(&mut report, grant, Reason::NoMatch, source, None, None)?;
                }
            }
            Outcome::None => {
                rb.file(&mut report, grant, Reason::NoMatch, source, None, None)?;
            }
            Outcome::Ambiguous => {
                rb.file(&mut report, grant, Reason::Ambiguous, source, None, None)?;
            }
            Outcome::Accepted { rec } | Outcome::Stale { rec } => {
                let managed = records[rec]
                    .creds
                    .as_ref()
                    .map(|s| s.expose())
                    .unwrap_or("");
                let chosen_same = cl
                    .chosen
                    .is_some_and(|c| compare_refresh_tokens(grant, texts[c]) == RtCompare::Same);
                if compare_refresh_tokens(grant, managed) != RtCompare::Same && !chosen_same {
                    rb.file(
                        &mut report,
                        grant,
                        Reason::Superseded,
                        source,
                        Some(&records[rec].record.id),
                        None,
                    )?;
                }
            }
        }
    }
    Ok(report)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::http::FakeHttp;
    use crate::orca::runtime::runtime_paths;
    use crate::orca::testsupport::{creds_json, oauth_json, record_json};
    use serde_json::json;

    fn grant(access: &str, refresh: &str, exp: i64, email: Option<&str>) -> String {
        let mut o = json!({"accessToken": access, "refreshToken": refresh, "expiresAt": exp});
        if let Some(e) = email {
            o["email"] = json!(e);
        }
        json!({ "claudeAiOauth": o }).to_string()
    }

    fn facts<'a>(email: Option<&'a str>, org: Option<&'a str>) -> RecordFacts<'a> {
        RecordFacts {
            email,
            organization_uuid: org,
        }
    }

    #[test]
    fn js_number_semantics() {
        for (s, want) in [
            ("", Some(0.0)),
            ("  12 ", Some(12.0)),
            ("0x1f", Some(31.0)),
            ("0b101", Some(5.0)),
            ("0o17", Some(15.0)),
            ("-1.5e3", Some(-1500.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("+7", Some(7.0)),
            ("Infinity", None),
            ("inf", None),
            ("nan", None),
            ("1e400", None),
            ("-0x1", None),
            ("1_000", None),
            (".", None),
            ("1e", None),
            ("12abc", None),
        ] {
            assert_eq!(js_number_from_str(s), want, "{s:?}");
        }
        assert_eq!(
            read_freshness(r#"{"claudeAiOauth":{"expiresAt":"0x10"}}"#),
            Some(16.0)
        );
        assert_eq!(
            read_freshness(r#"{"claudeAiOauth":{"expiresAt":null,"expiry":3}}"#),
            Some(3.0)
        );
        assert_eq!(
            read_freshness(r#"{"claudeAiOauth":{"expiresAt":"x","expires":4}}"#),
            Some(4.0)
        );
        assert_eq!(read_freshness("null"), None);
    }

    #[test]
    fn identity_and_refresh_token_readers() {
        let id = identity_from_credentials(
            r#"{"claudeAiOauth":{"accountUuid":"","accountId":"x","email":" a@example.com ","organizationId":"o"}}"#,
        )
        .unwrap()
        .unwrap();
        // An empty primary blocks the fallback, as `??` does.
        assert_eq!(id.account_uuid, None);
        assert_eq!(id.email.as_deref(), Some("a@example.com"));
        assert_eq!(id.organization_uuid.as_deref(), Some("o"));
        assert_eq!(identity_from_credentials("{").unwrap(), None);
        assert_eq!(identity_from_credentials("null"), Err(JsThrow));
        assert_eq!(
            identity_from_credentials("[1]").unwrap(),
            Some(CredIdentity::default())
        );
        assert_eq!(
            compare_refresh_tokens(&grant("a", " r ", 1, None), &grant("b", "r", 2, None)),
            RtCompare::Same
        );
        assert_eq!(
            compare_refresh_tokens(&grant("a", "r1", 1, None), &grant("b", "r2", 2, None)),
            RtCompare::Different
        );
        assert_eq!(
            compare_refresh_tokens(&grant("a", "", 1, None), &grant("b", "r2", 2, None)),
            RtCompare::Missing
        );
    }

    #[test]
    fn matcher_truth_table() {
        use Verdict::*;
        let org_a = Some("org-a");
        let rt1 = grant("a1", "r1", 1, None);
        let rt1b = grant("a2", "r1", 2, None);
        let rt2 = grant("a3", "r2", 3, None);
        let with_email = |e: &str, rt: &str| grant("ax", rt, 5, Some(e));
        let rt_alice = |rt: &str| with_email("alice@example.com", rt);
        let runtime_alice = json!({"accountUuid":"u-a","emailAddress":"alice@example.com"});
        let runtime_bob = json!({"accountUuid":"u-b","emailAddress":"bob@example.com"});
        let stash_a = json!({"accountUuid":"u-a","emailAddress":"alice@example.com","organizationUuid":"org-a"});

        let m = |cand: &str, rt: Option<&Value>, f: RecordFacts, rc: &str, ro: Option<&Value>| {
            match_account(cand, rt, f, rc, ro).unwrap()
        };
        // Unparseable candidate.
        assert_eq!(m("{", None, facts(None, None), &rt1, None), Mismatch);
        // Candidate identity clashes with the runtime identity.
        assert_eq!(
            m(
                &rt_alice("r1"),
                Some(&runtime_bob),
                facts(None, None),
                &rt1,
                None
            ),
            Mismatch
        );
        // d present: record email differs.
        assert_eq!(
            m(
                &rt_alice("r1"),
                None,
                facts(Some("bob@example.com"), None),
                &rt1,
                None
            ),
            Mismatch
        );
        // d present, l set, f unknown: same RT matches, else u decides.
        assert_eq!(
            m(
                &rt_alice("r1"),
                None,
                facts(Some("alice@example.com"), org_a),
                &rt1,
                None
            ),
            Match
        );
        assert_eq!(
            m(
                &rt_alice("r9"),
                None,
                facts(Some("alice@example.com"), org_a),
                &rt1,
                None
            ),
            Unverifiable
        );
        assert_eq!(
            m(
                &rt_alice("r9"),
                Some(&runtime_alice),
                facts(Some("alice@example.com"), org_a),
                &rt1,
                Some(&stash_a)
            ),
            Match
        );
        // d present, l and f differ.
        let alice_org_b = grant("ax", "r1", 5, None).replace(
            r#""accessToken""#,
            r#""email":"alice@example.com","organizationUuid":"org-b","accessToken""#,
        );
        assert_eq!(
            m(
                &alice_org_b,
                None,
                facts(Some("alice@example.com"), org_a),
                &rt1,
                None
            ),
            Mismatch
        );
        // d present, no l, f known: RT equality decides.
        assert_eq!(m(&alice_org_b, None, facts(None, None), &rt1, None), Match);
        assert_eq!(
            m(&alice_org_b, None, facts(None, None), &rt2, None),
            Unverifiable
        );
        // d present, nothing else: match.
        assert_eq!(
            m(
                &rt_alice("r9"),
                None,
                facts(Some("alice@example.com"), None),
                &rt2,
                None
            ),
            Match
        );
        // An empty record email is falsy; a blank one normalizes to null.
        assert_eq!(
            m(&rt_alice("r9"), None, facts(Some(""), None), &rt2, None),
            Match
        );
        assert_eq!(
            m(&rt_alice("r9"), None, facts(Some("  "), None), &rt2, None),
            Mismatch
        );
        // d absent: same RT matches.
        assert_eq!(m(&rt1b, None, facts(None, org_a), &rt1, None), Match);
        // d absent, candidate org vs l.
        let org_only = |o: &str, rt: &str| {
            json!({"claudeAiOauth":{"accessToken":"a","refreshToken":rt,"organizationUuid":o}})
                .to_string()
        };
        assert_eq!(
            m(
                &org_only("org-b", "r9"),
                None,
                facts(None, org_a),
                &rt1,
                None
            ),
            Mismatch
        );
        assert_eq!(
            m(
                &org_only("org-a", "r9"),
                None,
                facts(None, org_a),
                &rt1,
                None
            ),
            Unverifiable
        );
        // d absent, u.
        let runtime_uuid_org = json!({"accountUuid":"u-a","organizationUuid":"org-a"});
        assert_eq!(
            m(
                &rt2,
                Some(&runtime_uuid_org),
                facts(None, None),
                &rt1,
                Some(&json!({"accountUuid":"u-a"}))
            ),
            Match
        );
        // d absent, f unknown, RT differs: mismatch; RT missing: unverifiable.
        assert_eq!(m(&rt2, None, facts(None, None), &rt1, None), Mismatch);
        assert_eq!(
            m(
                &grant("a", "", 1, None),
                None,
                facts(None, None),
                &rt1,
                None
            ),
            Unverifiable
        );
        // d absent, f from the runtime, RT differs: unverifiable.
        assert_eq!(
            m(
                &rt2,
                Some(&json!({"organizationUuid":"org-z"})),
                facts(None, None),
                &rt1,
                None
            ),
            Unverifiable
        );
        // l from the record creds when the record has no org; a record org of
        // "" blocks the fallback.
        let rc_org = org_only("org-a", "r1");
        assert_eq!(
            m(
                &org_only("org-b", "r9"),
                None,
                facts(None, None),
                &rc_org,
                None
            ),
            Mismatch
        );
        assert_eq!(
            m(
                &org_only("org-b", "r9"),
                None,
                facts(None, Some("")),
                &rc_org,
                None
            ),
            Unverifiable
        );
        // A record grant of JSON null throws.
        assert_eq!(
            match_account(&rt1, None, facts(None, None), "null", None),
            Err(JsThrow)
        );
    }

    #[test]
    fn acceptance_and_choose_freshest() {
        let m = grant("a", "r1", 100, None);
        assert!(accepts(&grant("b", "r1", 101, None), &m, true));
        assert!(!accepts(&grant("b", "r1", 100, None), &m, true));
        assert!(accepts(&grant("b", "r2", 100, None), &m, true));
        assert!(!accepts(&grant("b", "r2", 99, None), &m, true));
        // No freshness: a different RT is not "older".
        let nofresh = r#"{"claudeAiOauth":{"accessToken":"b","refreshToken":"r2"}}"#;
        assert!(accepts(nofresh, &m, true));
        assert!(!accepts(
            r#"{"claudeAiOauth":{"accessToken":"b","refreshToken":"r1"}}"#,
            &m,
            true
        ));
        // lastWritten set: anything not older.
        assert!(accepts(&grant("b", "r1", 100, None), &m, false));
        assert!(!accepts(&grant("b", "r1", 99, None), &m, false));

        let list = [
            r#"{"claudeAiOauth":{"accessToken":"x"}}"#.to_owned(),
            grant("b", "r", 5, None),
            grant("c", "r", 5, None),
            grant("d", "r", 7, None),
            r#"{"claudeAiOauth":{"accessToken":"y"}}"#.to_owned(),
        ];
        assert_eq!(choose_freshest(&list), Some(3));
        assert_eq!(choose_freshest(&list[..3]), Some(1));
        assert_eq!(choose_freshest::<String>(&[]), None);
    }

    #[test]
    fn dedupe_keeps_first_and_drops_the_excluded() {
        let c = |s: Source, j: &str| Candidate {
            source: s,
            json: SecretString::new(j.into()),
        };
        let out = dedupe(
            vec![
                c(Source::ScopedKeychain, "A"),
                c(Source::LegacyKeychain, "A"),
                c(Source::File, "B"),
                c(Source::File, "C"),
            ],
            Some("C"),
        );
        assert_eq!(
            out.iter()
                .map(|c| (c.source, c.json.expose().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                (Source::ScopedKeychain, "A".to_owned()),
                (Source::File, "B".to_owned())
            ]
        );
    }

    // ─── shell ────────────────────────────────────────────────────────────────

    struct World {
        _tmp: tempfile::TempDir,
        ud: std::path::PathBuf,
        paths: RuntimePaths,
        records: Vec<AccountRecord>,
        q: Quarantine,
        user: KeychainUser,
    }

    impl World {
        fn new(os: HostOs) -> World {
            let tmp = tempfile::tempdir().unwrap();
            let ud = tmp.path().join("ud");
            let d = tmp.path().join("D");
            std::fs::create_dir_all(&d).unwrap();
            let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
            let mut records = Vec::new();
            for (id, email, uuid, rt) in [
                ("id-a", "alice@example.com", "u-a", "rt-a"),
                ("id-b", "bob@example.com", "u-b", "rt-b"),
            ] {
                let s = stash::create(&ud, id).unwrap();
                s.write_auth(
                    &ud,
                    os,
                    &creds_json(&format!("at-{id}"), rt, 1000),
                    &oauth_json(uuid, email, None),
                )
                .unwrap();
                records
                    .push(AccountRecord::from_value(&record_json(&ud, id, email, None)).unwrap());
            }
            let q = Quarantine::new(os, &tmp.path().join("state"));
            World {
                ud,
                paths,
                records,
                q,
                user: KeychainUser {
                    acct: "tester".into(),
                    delete_accts: vec!["tester".into()],
                },
                _tmp: tmp,
            }
        }

        fn runtime(&self, creds: &str, oauth: Option<Value>) {
            std::fs::write(&self.paths.credentials_path, creds).unwrap();
            let cfg = match oauth {
                Some(o) => json!({ "oauthAccount": o }),
                None => json!({}),
            };
            std::fs::write(&self.paths.config_path, cfg.to_string()).unwrap();
        }

        fn run(
            &self,
            os: HostOs,
            http: &FakeHttp,
            live: bool,
            exclude: Option<&str>,
        ) -> Result<ReadBackReport, OrcaError> {
            read_back(&ReadBack {
                os,
                user_data: &self.ud,
                paths: &self.paths,
                keychain_user: &self.user,
                records: &self.records,
                exclude,
                live_claude: live,
                http,
                quarantine: &self.q,
                now_ms: 1_700_000_000_000,
                migration: false,
            })
        }

        fn stash_creds(&self, id: &str, os: HostOs) -> String {
            Stash::open(&self.ud, id, None)
                .unwrap()
                .credentials(os)
                .unwrap()
                .unwrap()
                .expose()
                .to_owned()
        }
    }

    const OS: HostOs = HostOs::Linux;

    fn alice_runtime() -> Value {
        oauth_json("u-a", "alice@example.com", None)
    }

    #[test]
    fn a_fresher_grant_is_filed_into_its_owner_after_the_profile_agrees() {
        let w = World::new(OS);
        let fresh = creds_json("at-new", "rt-a2", 2000);
        w.runtime(&fresh, Some(alice_runtime()));
        let http = FakeHttp::default().profile_uuid("at-new", "u-a");
        let rep = w.run(OS, &http, false, None).unwrap();
        assert_eq!(rep.persisted.as_deref(), Some("id-a"));
        assert!(rep.quarantined.is_empty());
        assert_eq!(w.stash_creds("id-a", OS), fresh);
        assert_eq!(
            w.stash_creds("id-b", OS),
            creds_json("at-id-b", "rt-b", 1000)
        );
    }

    #[test]
    fn candidates_equal_to_the_last_synced_stash_are_ignored() {
        let w = World::new(OS);
        let same = w.stash_creds("id-a", OS);
        w.runtime(&same, Some(alice_runtime()));
        let http = FakeHttp::default();
        let rep = w.run(OS, &http, false, Some(&same)).unwrap();
        assert_eq!(rep, ReadBackReport::default());
        assert!(http.profile_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_profile_naming_another_account_quarantines() {
        let w = World::new(OS);
        let fresh = creds_json("at-new", "rt-a2", 2000);
        w.runtime(&fresh, Some(alice_runtime()));
        let http = FakeHttp::default().profile_uuid("at-new", "u-b");
        let rep = w.run(OS, &http, false, None).unwrap();
        assert_eq!(rep.persisted, None);
        assert_eq!(rep.quarantined.len(), 1);
        assert_eq!(rep.quarantined[0].1, Reason::ProfileMismatch);
        assert_eq!(
            w.q.get(&rep.quarantined[0].0).unwrap().unwrap().expose(),
            fresh
        );
        assert_eq!(
            w.stash_creds("id-a", OS),
            creds_json("at-id-a", "rt-a", 1000)
        );
        assert_eq!(w.q.list()[0].profile_account_uuid.as_deref(), Some("u-b"));
    }

    #[test]
    fn no_http_answer_aborts_with_nothing_written() {
        let w = World::new(OS);
        w.runtime(&creds_json("at-new", "rt-a2", 2000), Some(alice_runtime()));
        let err = w.run(OS, &FakeHttp::default(), false, None).unwrap_err();
        assert!(matches!(err, OrcaError::Network(_)));
        assert!(w.q.list().is_empty());
        assert_eq!(
            w.stash_creds("id-a", OS),
            creds_json("at-id-a", "rt-a", 1000)
        );
        // A 500 is treated the same way.
        let http = FakeHttp::default().profile("at-new", FakeHttp::reply(500, "{}"));
        assert!(matches!(
            w.run(OS, &http, false, None),
            Err(OrcaError::Network(_))
        ));
    }

    #[test]
    fn unauthorized_refreshes_once_then_files_by_the_new_answer() {
        let w = World::new(OS);
        let dead = creds_json("at-dead", "rt-a2", 2000);
        w.runtime(&dead, Some(alice_runtime()));
        let http = FakeHttp::default()
            .profile("at-dead", FakeHttp::reply(401, "{}"))
            .token_reply(FakeHttp::reply(
                200,
                r#"{"access_token":"at-rot","refresh_token":"rt-a3","expires_in":3600}"#,
            ))
            .profile_uuid("at-rot", "u-a");
        let rep = w.run(OS, &http, false, None).unwrap();
        assert_eq!(rep.persisted.as_deref(), Some("id-a"));
        let stored = w.stash_creds("id-a", OS);
        assert!(stored.contains("at-rot") && stored.contains("rt-a3"));
        assert_eq!(http.token_bodies.lock().unwrap().len(), 1);
        // Attributed and stored: the quarantine is clean again.
        assert!(w.q.list().is_empty() && rep.quarantined.is_empty());
    }

    #[test]
    fn unauthorized_with_a_live_claude_quarantines_without_refreshing() {
        let w = World::new(OS);
        let dead = creds_json("at-dead", "rt-a2", 2000);
        w.runtime(&dead, Some(alice_runtime()));
        let http = FakeHttp::default().profile("at-dead", FakeHttp::reply(401, "{}"));
        let rep = w.run(OS, &http, true, None).unwrap();
        assert_eq!(rep.persisted, None);
        assert_eq!(rep.quarantined.len(), 1);
        assert_eq!(rep.quarantined[0].1, Reason::Unauthorized);
        assert!(http.token_bodies.lock().unwrap().is_empty());
    }

    #[test]
    fn unauthorized_rotation_to_another_account_stays_quarantined() {
        let w = World::new(OS);
        w.runtime(&creds_json("at-dead", "rt-a2", 2000), Some(alice_runtime()));
        let http = FakeHttp::default()
            .profile("at-dead", FakeHttp::reply(401, "{}"))
            .token_reply(FakeHttp::reply(
                200,
                r#"{"access_token":"at-rot","expires_in":60}"#,
            ))
            .profile_uuid("at-rot", "u-b");
        let rep = w.run(OS, &http, false, None).unwrap();
        assert_eq!(rep.persisted, None);
        let reasons: Vec<Reason> = rep.quarantined.iter().map(|q| q.1).collect();
        assert_eq!(
            reasons,
            vec![
                Reason::Unauthorized,
                Reason::Rotated,
                Reason::ProfileMismatch
            ]
        );
        // Same refresh token: one entry, the fresher (rotated) grant.
        let list = w.q.list();
        assert_eq!(list.len(), 1);
        assert!(
            w.q.get(&list[0].fingerprint)
                .unwrap()
                .unwrap()
                .expose()
                .contains("at-rot")
        );
    }

    #[test]
    fn unmatched_and_ambiguous_grants_are_quarantined() {
        let w = World::new(OS);
        // No identity, an unknown RT: mismatch against both -> none.
        w.runtime(&creds_json("at-x", "rt-x", 2000), None);
        let http = FakeHttp::default();
        let rep = w.run(OS, &http, false, None).unwrap();
        assert_eq!(
            rep.quarantined.iter().map(|q| q.1).collect::<Vec<_>>(),
            vec![Reason::NoMatch]
        );
        assert!(http.profile_calls.lock().unwrap().is_empty());
        // An org-scoped grant with no email: unverifiable against both.
        let w = World::new(OS);
        w.runtime(
            &json!({"claudeAiOauth":{"accessToken":"a","refreshToken":"rt-y","organizationUuid":"org-q"}}).to_string(),
            None,
        );
        let rep = w.run(OS, &FakeHttp::default(), false, None).unwrap();
        assert_eq!(
            rep.quarantined.iter().map(|q| q.1).collect::<Vec<_>>(),
            vec![Reason::Ambiguous]
        );
    }

    #[test]
    fn a_stash_that_changed_after_the_match_is_not_overwritten() {
        struct Racing<'a> {
            inner: FakeHttp,
            ud: &'a Path,
        }
        impl OauthHttp for Racing<'_> {
            fn post_token(&self, b: &str) -> Result<http::HttpReply, http::HttpError> {
                self.inner.post_token(b)
            }
            fn get_profile(&self, a: &str) -> Result<http::HttpReply, http::HttpError> {
                let s = Stash::open(self.ud, "id-a", None).unwrap();
                s.write_credentials(self.ud, OS, &creds_json("at-other", "rt-o", 5))
                    .unwrap();
                self.inner.get_profile(a)
            }
        }
        let w = World::new(OS);
        let fresh = creds_json("at-new", "rt-a2", 2000);
        w.runtime(&fresh, Some(alice_runtime()));
        let http = Racing {
            inner: FakeHttp::default().profile_uuid("at-new", "u-a"),
            ud: &w.ud,
        };
        let rep = read_back(&ReadBack {
            os: OS,
            user_data: &w.ud,
            paths: &w.paths,
            keychain_user: &w.user,
            records: &w.records,
            exclude: None,
            live_claude: false,
            http: &http,
            quarantine: &w.q,
            now_ms: 0,
            migration: false,
        })
        .unwrap();
        assert_eq!(rep.persisted, None);
        assert_eq!(rep.quarantined[0].1, Reason::StashChanged);
        assert_eq!(w.stash_creds("id-a", OS), creds_json("at-other", "rt-o", 5));
    }

    #[cfg(unix)]
    #[test]
    fn macos_reads_scoped_legacy_then_file_and_aborts_on_keychain_errors() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = World::new(HostOs::MacOs);
        let dir = w.paths.config_dir.to_str().unwrap().to_owned();
        let scoped = creds_json("at-s", "rt-a2", 3000);
        let legacy = creds_json("at-l", "rt-a3", 2500);
        keychain::write_runtime_scoped(&scoped, Some(&dir), &w.user).unwrap();
        keychain::write_runtime_scoped(&legacy, None, &w.user).unwrap();
        w.runtime(&creds_json("at-f", "rt-a4", 2000), Some(alice_runtime()));
        let rb = |http: &FakeHttp| w.run(HostOs::MacOs, http, false, None);
        let got = gather(&ReadBack {
            os: HostOs::MacOs,
            user_data: &w.ud,
            paths: &w.paths,
            keychain_user: &w.user,
            records: &w.records,
            exclude: None,
            live_claude: false,
            http: &FakeHttp::default(),
            quarantine: &w.q,
            now_ms: 0,
            migration: false,
        })
        .unwrap();
        assert_eq!(
            got.iter().map(|c| c.source).collect::<Vec<_>>(),
            vec![Source::ScopedKeychain, Source::LegacyKeychain, Source::File]
        );
        let http = FakeHttp::default().profile_uuid("at-s", "u-a");
        let rep = rb(&http).unwrap();
        assert_eq!(rep.persisted.as_deref(), Some("id-a"));
        assert_eq!(w.stash_creds("id-a", HostOs::MacOs), scoped);
        // The two losers carry refresh tokens csm has not stored.
        assert_eq!(
            rep.quarantined.iter().map(|q| q.1).collect::<Vec<_>>(),
            vec![Reason::Superseded, Reason::Superseded]
        );
        fake.fail_find(&keychain::runtime_service(Some(&dir)), true);
        assert!(matches!(
            rb(&FakeHttp::default()),
            Err(OrcaError::Keychain(_))
        ));
    }
}
