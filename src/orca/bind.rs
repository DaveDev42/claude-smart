//! Binding Orca accounts to csm profiles — pure, computed on demand from
//! files, never cached across runs.
//!
//! Key: (`accountUuid`, `organizationUuid`) of the Orca account's
//! `oauth-account.json` against each non-slot profile's `.claude.json` →
//! `oauthAccount`. When `accountUuid` is missing on either side the key falls
//! back to (lowercased email, `organizationUuid`); when an
//! `organizationUuid` is missing too, the match is on the one uuid/email
//! alone and flagged weak. Two present-but-different uuids or orgs never
//! match (the same person in two orgs is two accounts).
//!
//! Only host-runtime Orca accounts bind. Ties (several profiles share one
//! identity) go to the `orca.bindings[<accountId>]` override, else to the
//! alphabetically first profile of the strongest match tier. The default
//! state is not an input here.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::{Account, Identity, Selection};

/// How strongly an account matched its profile (strongest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchKind {
    /// accountUuid + organizationUuid.
    Uuid,
    /// accountUuid, organizationUuid missing on a side (weak).
    UuidOnly,
    /// lowercased email + organizationUuid.
    Email,
    /// lowercased email alone (weakest).
    EmailOnly,
    /// No identity match; bound only by the `orca.bindings` override.
    Override,
}

impl MatchKind {
    /// A match worth flagging in `csm orca status`.
    pub fn is_weak(self) -> bool {
        matches!(
            self,
            MatchKind::UuidOnly | MatchKind::EmailOnly | MatchKind::Override
        )
    }
}

/// Does account identity `a` match profile identity `p`, and how strongly?
pub fn match_kind(a: &Identity, p: &Identity) -> Option<MatchKind> {
    let (ao, po) = (&a.organization_uuid, &p.organization_uuid);
    let org_ok = match (ao, po) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    };
    let both_org = ao.is_some() && po.is_some();
    if !org_ok {
        return None;
    }
    if let (Some(au), Some(pu)) = (&a.account_uuid, &p.account_uuid) {
        if au != pu {
            return None;
        }
        return Some(if both_org {
            MatchKind::Uuid
        } else {
            MatchKind::UuidOnly
        });
    }
    let (Some(ae), Some(pe)) = (a.email_key(), p.email_key()) else {
        return None;
    };
    if ae != pe {
        return None;
    }
    Some(if both_org {
        MatchKind::Email
    } else {
        MatchKind::EmailOnly
    })
}

/// One account's binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub profile: String,
    pub kind: MatchKind,
    /// The `orca.bindings` override picked this profile.
    #[serde(rename = "viaOverride")]
    pub via_override: bool,
    /// Every profile whose identity matches this account (any tier), sorted.
    pub candidates: Vec<String>,
}

/// Several profiles tied for one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tie {
    #[serde(rename = "accountId")]
    pub account_id: String,
    pub profiles: Vec<String>,
    pub chosen: String,
}

/// The result of [`bind`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Bindings {
    /// Orca account id → its binding.
    #[serde(rename = "byAccount")]
    pub by_account: BTreeMap<String, Binding>,
    pub ties: Vec<Tie>,
    /// Host accounts no profile matches.
    #[serde(rename = "unboundAccounts")]
    pub unbound_accounts: Vec<String>,
    /// Non-slot profiles no account matches.
    #[serde(rename = "unboundProfiles")]
    pub unbound_profiles: Vec<String>,
    /// Accounts skipped because they are not host-runtime.
    #[serde(rename = "nonHostAccounts")]
    pub non_host_accounts: Vec<String>,
    /// `orca.bindings` entries naming an unregistered (or slot) profile.
    #[serde(rename = "invalidOverrides")]
    pub invalid_overrides: Vec<(String, String)>,
}

impl Bindings {
    pub fn binding(&self, account_id: &str) -> Option<&Binding> {
        self.by_account.get(account_id)
    }

    /// The profile `account_id` is bound to.
    pub fn profile_for(&self, account_id: &str) -> Option<&str> {
        self.binding(account_id).map(|b| b.profile.as_str())
    }

    /// The profile to follow when `account_id` is Orca's active account:
    /// the override when one applies, else `prefer` (csm's current default)
    /// when it is one of the tied candidates — so a tie never flips the
    /// default between two dirs of the same account — else the binding.
    pub fn profile_for_active(&self, account_id: &str, prefer: Option<&str>) -> Option<&str> {
        let b = self.binding(account_id)?;
        if !b.via_override
            && let Some(p) = prefer
            && let Some(c) = b.candidates.iter().find(|c| c.as_str() == p)
        {
            return Some(c.as_str());
        }
        Some(b.profile.as_str())
    }

    /// The account bound to `profile` (a direct binding first, else any
    /// account listing it as a candidate). Sorted-id order is deterministic.
    pub fn account_for_profile(&self, profile: &str) -> Option<&str> {
        self.by_account
            .iter()
            .find(|(_, b)| b.profile == profile)
            .or_else(|| {
                self.by_account
                    .iter()
                    .find(|(_, b)| b.candidates.iter().any(|c| c == profile))
            })
            .map(|(id, _)| id.as_str())
    }

    /// Every profile that is (or shares the identity of) `account_id`'s
    /// binding — the set a limit switch away from that account must skip.
    pub fn profiles_sharing_identity(&self, account_id: &str) -> Vec<String> {
        let Some(b) = self.binding(account_id) else {
            return Vec::new();
        };
        let mut out = b.candidates.clone();
        if !out.contains(&b.profile) {
            out.push(b.profile.clone());
        }
        out.sort();
        out
    }
}

/// Bind `accounts` (with their identities) to `profiles` (non-slot
/// registered profiles with their identities; a `slot` entry is skipped
/// defensively). Pure.
pub fn bind(
    accounts: &[(Account, Identity)],
    profiles: &[(String, Option<Identity>)],
    overrides: &BTreeMap<String, String>,
    slot: Option<&str>,
) -> Bindings {
    let profiles: Vec<&(String, Option<Identity>)> = profiles
        .iter()
        .filter(|(n, _)| Some(n.as_str()) != slot)
        .collect();
    let is_profile = |n: &str| profiles.iter().any(|(p, _)| p == n);
    let mut out = Bindings::default();
    let mut matched_profiles: Vec<String> = Vec::new();

    for (id, prof) in overrides {
        if !is_profile(prof) {
            out.invalid_overrides.push((id.clone(), prof.clone()));
        }
    }

    for (acct, ident) in accounts {
        if !acct.is_host() {
            out.non_host_accounts.push(acct.id.clone());
            continue;
        }
        let mut matches: Vec<(MatchKind, &str)> = profiles
            .iter()
            .filter_map(|(name, pid)| {
                let k = match_kind(ident, pid.as_ref()?)?;
                Some((k, name.as_str()))
            })
            .collect();
        matches.sort();
        let candidates: Vec<String> = {
            let mut c: Vec<String> = matches.iter().map(|(_, n)| (*n).to_owned()).collect();
            c.sort();
            c
        };
        matched_profiles.extend(candidates.iter().cloned());

        let override_target = overrides.get(&acct.id).filter(|p| is_profile(p)).cloned();
        let best_tier: Vec<&str> = match matches.first() {
            Some((k, _)) => matches
                .iter()
                .filter(|(k2, _)| k2 == k)
                .map(|(_, n)| *n)
                .collect(),
            None => Vec::new(),
        };

        let binding = if let Some(p) = override_target {
            let kind = matches
                .iter()
                .find(|(_, n)| *n == p)
                .map(|(k, _)| *k)
                .unwrap_or(MatchKind::Override);
            matched_profiles.push(p.clone());
            Binding {
                profile: p,
                kind,
                via_override: true,
                candidates,
            }
        } else if let Some(first) = best_tier.first() {
            Binding {
                profile: (*first).to_owned(),
                kind: matches[0].0,
                via_override: false,
                candidates,
            }
        } else {
            out.unbound_accounts.push(acct.id.clone());
            continue;
        };
        if best_tier.len() > 1 {
            out.ties.push(Tie {
                account_id: acct.id.clone(),
                profiles: best_tier.iter().map(|s| (*s).to_owned()).collect(),
                chosen: binding.profile.clone(),
            });
        }
        out.by_account.insert(acct.id.clone(), binding);
    }

    let mut unbound: Vec<String> = profiles
        .iter()
        .map(|(n, _)| n.clone())
        .filter(|n| !matched_profiles.contains(n))
        .collect();
    unbound.sort();
    out.unbound_profiles = unbound;
    out
}

/// Gather identities from disk and [`bind`] `selection`'s accounts to the
/// registry's non-slot profiles. Unreadable identity files count as "no
/// identity" (the profile just stays unbound).
pub fn compute(
    user_data: &Path,
    selection: &Selection,
    profiles: &crate::account::ProfileMap,
    slot: Option<&super::slot::Slot>,
    overrides: &BTreeMap<String, String>,
) -> Bindings {
    let accounts: Vec<(Account, Identity)> = selection
        .accounts
        .iter()
        .map(|a| (a.clone(), super::identity::account_identity(user_data, a)))
        .collect();
    let profs: Vec<(String, Option<Identity>)> = profiles
        .names_sorted()
        .into_iter()
        .filter(|n| slot.is_none_or(|s| !s.is_profile(n)))
        .map(|n| {
            let dir = profiles.get(n).unwrap_or_default();
            let id = super::identity::read_profile_identity(Path::new(dir))
                .ok()
                .flatten();
            (n.to_owned(), id)
        })
        .collect();
    bind(&accounts, &profs, overrides, slot.map(|s| s.name.as_str()))
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(uuid: Option<&str>, email: Option<&str>, org: Option<&str>) -> Identity {
        Identity {
            account_uuid: uuid.map(Into::into),
            email: email.map(Into::into),
            organization_uuid: org.map(Into::into),
        }
    }

    fn acct(id: &str, runtime: &str) -> Account {
        Account {
            id: id.into(),
            email: String::new(),
            organization_uuid: None,
            organization_name: None,
            runtime: runtime.into(),
        }
    }

    #[test]
    fn match_kind_tiers() {
        let full = ident(Some("u1"), Some("alice@example.com"), Some("o1"));
        assert_eq!(match_kind(&full, &full), Some(MatchKind::Uuid));
        assert_eq!(
            match_kind(&full, &ident(Some("u1"), None, None)),
            Some(MatchKind::UuidOnly)
        );
        assert_eq!(
            match_kind(
                &full,
                &ident(Some("u2"), Some("alice@example.com"), Some("o1"))
            ),
            None,
            "different uuids never fall back to email"
        );
        assert_eq!(
            match_kind(&full, &ident(Some("u1"), None, Some("o2"))),
            None,
            "same person, other org"
        );
        assert_eq!(
            match_kind(&full, &ident(None, Some("ALICE@example.com"), Some("o1"))),
            Some(MatchKind::Email)
        );
        assert_eq!(
            match_kind(&ident(None, Some("alice@example.com"), None), &full),
            Some(MatchKind::EmailOnly)
        );
        assert_eq!(match_kind(&ident(None, None, None), &full), None);
    }

    #[test]
    fn binds_by_uuid_and_reports_unbound() {
        let accounts = vec![
            (acct("a1", "host"), ident(Some("u1"), None, Some("o1"))),
            (acct("a2", "host"), ident(Some("u9"), None, Some("o1"))),
            (acct("a3", "wsl"), ident(Some("u1"), None, Some("o1"))),
        ];
        let profiles = vec![
            ("work".to_owned(), Some(ident(Some("u1"), None, Some("o1")))),
            ("home".to_owned(), Some(ident(Some("u2"), None, Some("o1")))),
            ("orca".to_owned(), Some(ident(Some("u1"), None, Some("o1")))),
        ];
        let b = bind(&accounts, &profiles, &BTreeMap::new(), Some("orca"));
        assert_eq!(b.profile_for("a1"), Some("work"), "the slot never binds");
        assert_eq!(b.binding("a1").unwrap().kind, MatchKind::Uuid);
        assert_eq!(b.unbound_accounts, vec!["a2".to_owned()]);
        assert_eq!(b.non_host_accounts, vec!["a3".to_owned()]);
        assert_eq!(b.unbound_profiles, vec!["home".to_owned()]);
        assert_eq!(b.account_for_profile("work"), Some("a1"));
        assert!(b.ties.is_empty());
    }

    #[test]
    fn ties_go_alphabetical_then_override() {
        let accounts = vec![(acct("a1", "host"), ident(Some("u1"), None, Some("o1")))];
        let same = Some(ident(Some("u1"), None, Some("o1")));
        let profiles = vec![("work".to_owned(), same.clone()), ("home".to_owned(), same)];
        let b = bind(&accounts, &profiles, &BTreeMap::new(), None);
        assert_eq!(b.profile_for("a1"), Some("home"));
        assert_eq!(b.ties.len(), 1);
        assert_eq!(
            b.ties[0].profiles,
            vec!["home".to_owned(), "work".to_owned()]
        );
        assert_eq!(b.profiles_sharing_identity("a1"), vec!["home", "work"]);
        // The current default among the candidates is kept (no flip-flop).
        assert_eq!(b.profile_for_active("a1", Some("work")), Some("work"));
        assert_eq!(b.profile_for_active("a1", Some("other")), Some("home"));

        let ov = BTreeMap::from([("a1".to_owned(), "work".to_owned())]);
        let b = bind(&accounts, &profiles, &ov, None);
        assert_eq!(b.profile_for("a1"), Some("work"));
        assert!(b.binding("a1").unwrap().via_override);
        assert_eq!(
            b.profile_for_active("a1", Some("home")),
            Some("work"),
            "an explicit override beats the current default"
        );
    }

    #[test]
    fn stronger_tier_wins_over_alphabetical() {
        let accounts = vec![(
            acct("a1", "host"),
            ident(Some("u1"), Some("alice@example.com"), Some("o1")),
        )];
        let profiles = vec![
            (
                "alpha".to_owned(),
                Some(ident(None, Some("alice@example.com"), None)),
            ),
            ("work".to_owned(), Some(ident(Some("u1"), None, Some("o1")))),
        ];
        let b = bind(&accounts, &profiles, &BTreeMap::new(), None);
        assert_eq!(b.profile_for("a1"), Some("work"));
        assert!(b.ties.is_empty());
        assert_eq!(b.binding("a1").unwrap().candidates, vec!["alpha", "work"]);
    }

    #[test]
    fn override_without_identity_match_and_invalid_overrides() {
        let accounts = vec![(acct("a1", "host"), ident(Some("u1"), None, None))];
        let profiles = vec![("work".to_owned(), None)];
        let ov = BTreeMap::from([
            ("a1".to_owned(), "work".to_owned()),
            ("a2".to_owned(), "missing".to_owned()),
        ]);
        let b = bind(&accounts, &profiles, &ov, None);
        assert_eq!(b.binding("a1").unwrap().kind, MatchKind::Override);
        assert!(b.unbound_profiles.is_empty());
        assert_eq!(
            b.invalid_overrides,
            vec![("a2".to_owned(), "missing".to_owned())]
        );
    }

    #[test]
    fn email_fallback_binds_when_uuid_missing() {
        let accounts = vec![(
            acct("a1", "host"),
            ident(None, Some("Alice@Example.com"), Some("o1")),
        )];
        let profiles = vec![(
            "work".to_owned(),
            Some(ident(Some("u1"), Some("alice@example.com"), Some("o1"))),
        )];
        let b = bind(&accounts, &profiles, &BTreeMap::new(), None);
        assert_eq!(b.binding("a1").unwrap().kind, MatchKind::Email);
    }
}
