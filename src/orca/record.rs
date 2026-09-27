//! Orca's Claude account record and the pure helpers over it.
//!
//! Ported from Orca 1.4.209:
//! - the record `persist()` appends (key order matters for a byte-faithful
//!   store): `id, email, managedAuthPath, managedAuthRuntime, wslDistro,
//!   wslLinuxAuthPath, authMethod, organizationUuid, organizationName,
//!   createdAt, updatedAt, lastAuthenticatedAt` ([`new_record`]);
//! - identity (e9i): lower-trimmed email, trimmed organization uuid or null,
//!   and `host` / `wsl:<distro or __default__>`; the uuid `id` is never
//!   identity ([`IdentityKey`]);
//! - d3: the effective active ids, `ByRuntime.host ?? activeId ?? null` plus
//!   a copy of the WSL map ([`d3`]);
//! - f3: the active id for one runtime target ([`f3`]);
//! - p3: the active ids after selecting an id for one target ([`p3`]);
//! - t6i: the active ids after removing an account ([`t6i`]);
//! - n6i: the normalization `accounts.list` applies, dropping active ids
//!   that name no record of the matching runtime ([`n6i`]).
//!
//! Values of an unexpected JSON type (a number where Orca writes a string or
//! null) are an error here, not coerced: a later write must refuse rather
//! than guess.

use serde_json::{Map, Value};

// ─── record ───────────────────────────────────────────────────────────────────

/// Where an account's stash lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRuntime {
    Host,
    Wsl,
}

impl AuthRuntime {
    /// Orca's reading: exactly `"wsl"` is WSL; anything else is the host.
    pub fn from_value(v: Option<&Value>) -> Self {
        match v.and_then(Value::as_str) {
            Some("wsl") => AuthRuntime::Wsl,
            _ => AuthRuntime::Host,
        }
    }
}

/// A typed view of one `claudeManagedAccounts` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRecord {
    pub id: String,
    pub email: Option<String>,
    pub managed_auth_path: Option<String>,
    pub runtime: AuthRuntime,
    pub wsl_distro: Option<String>,
    pub wsl_linux_auth_path: Option<String>,
    pub auth_method: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub last_authenticated_at: Option<i64>,
}

fn opt_str(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("account field {key} has an unexpected type")),
    }
}

fn opt_ms(obj: &Map<String, Value>, key: &str) -> Option<i64> {
    let v = obj.get(key)?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

impl AccountRecord {
    /// Parse one record. Errors never quote values.
    pub fn from_value(v: &Value) -> Result<AccountRecord, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| "an account entry is not an object".to_owned())?;
        let id = match obj.get("id") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err("an account entry has no string id".to_owned()),
        };
        Ok(AccountRecord {
            email: opt_str(obj, "email")?,
            managed_auth_path: opt_str(obj, "managedAuthPath")?,
            runtime: AuthRuntime::from_value(obj.get("managedAuthRuntime")),
            wsl_distro: opt_str(obj, "wslDistro")?,
            wsl_linux_auth_path: opt_str(obj, "wslLinuxAuthPath")?,
            auth_method: opt_str(obj, "authMethod")?,
            organization_uuid: opt_str(obj, "organizationUuid")?,
            organization_name: opt_str(obj, "organizationName")?,
            created_at: opt_ms(obj, "createdAt"),
            updated_at: opt_ms(obj, "updatedAt"),
            last_authenticated_at: opt_ms(obj, "lastAuthenticatedAt"),
            id,
        })
    }

    pub fn is_host(&self) -> bool {
        self.runtime == AuthRuntime::Host
    }

    /// This record's identity key (e9i), `None` without an email.
    pub fn identity(&self) -> Option<IdentityKey> {
        IdentityKey::new(
            self.email.as_deref(),
            self.organization_uuid.as_deref(),
            self.runtime,
            self.wsl_distro.as_deref(),
        )
    }
}

/// Parse a whole `claudeManagedAccounts` array.
pub fn parse_records(v: Option<&Value>) -> Result<Vec<AccountRecord>, String> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a.iter().map(AccountRecord::from_value).collect(),
        Some(_) => Err("claudeManagedAccounts is not an array".to_owned()),
    }
}

/// Inputs to a new host record (Orca's `persist()`).
#[derive(Debug, Clone)]
pub struct NewRecord<'a> {
    pub id: &'a str,
    pub email: &'a str,
    pub managed_auth_path: &'a str,
    pub organization_uuid: Option<&'a str>,
    pub organization_name: Option<&'a str>,
}

/// The record Orca's `persist()` appends for a host account, keys in its
/// order, timestamps all `now_ms`. Pure.
pub fn new_record(r: &NewRecord<'_>, now_ms: i64) -> Value {
    let opt = |s: Option<&str>| s.map_or(Value::Null, |s| Value::String(s.to_owned()));
    let mut m = Map::new();
    m.insert("id".into(), r.id.into());
    m.insert("email".into(), r.email.into());
    m.insert("managedAuthPath".into(), r.managed_auth_path.into());
    m.insert("managedAuthRuntime".into(), "host".into());
    m.insert("wslDistro".into(), Value::Null);
    m.insert("wslLinuxAuthPath".into(), Value::Null);
    m.insert("authMethod".into(), "subscription-oauth".into());
    m.insert("organizationUuid".into(), opt(r.organization_uuid));
    m.insert("organizationName".into(), opt(r.organization_name));
    m.insert("createdAt".into(), now_ms.into());
    m.insert("updatedAt".into(), now_ms.into());
    m.insert("lastAuthenticatedAt".into(), now_ms.into());
    Value::Object(m)
}

// ─── identity (e9i) ───────────────────────────────────────────────────────────

/// Orca's identity triple. Two records with equal keys are the same account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdentityKey {
    /// `email.trim().toLowerCase()`.
    pub email: String,
    /// `organizationUuid.trim() || null`.
    pub organization_uuid: Option<String>,
    /// `host` or `wsl:<distro trimmed, or __default__>`.
    pub runtime: String,
}

fn trimmed(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// m3: a WSL distro key, `__default__` when blank.
pub fn wsl_key(distro: Option<&str>) -> String {
    trimmed(distro).unwrap_or_else(|| "__default__".to_owned())
}

impl IdentityKey {
    /// `None` when the email is blank (e9i finds nothing then).
    pub fn new(
        email: Option<&str>,
        organization_uuid: Option<&str>,
        runtime: AuthRuntime,
        wsl_distro: Option<&str>,
    ) -> Option<IdentityKey> {
        let email = trimmed(email)?.to_lowercase();
        let runtime = match runtime {
            AuthRuntime::Host => "host".to_owned(),
            AuthRuntime::Wsl => format!("wsl:{}", wsl_key(wsl_distro)),
        };
        Some(IdentityKey {
            email,
            organization_uuid: trimmed(organization_uuid),
            runtime,
        })
    }
}

/// e9i: the first record with `key`'s identity.
pub fn find_by_identity<'a>(
    records: &'a [AccountRecord],
    key: &IdentityKey,
) -> Option<&'a AccountRecord> {
    records.iter().find(|r| r.identity().as_ref() == Some(key))
}

// ─── active ids (d3 / f3 / p3 / n6i) ─────────────────────────────────────────

/// The effective active ids: the host's and one per WSL distro key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActiveIds {
    pub host: Option<String>,
    /// Distro key → id, in the store's key order.
    pub wsl: Vec<(String, Option<String>)>,
}

impl ActiveIds {
    /// `{"host": …, "wsl": {…}}`, keys in the order `JSON.stringify` writes
    /// them for Orca's object literal.
    pub fn to_value(&self) -> Value {
        let opt = |s: &Option<String>| s.clone().map_or(Value::Null, Value::String);
        let mut wsl = Map::new();
        for (k, v) in &self.wsl {
            wsl.insert(k.clone(), opt(v));
        }
        let mut m = Map::new();
        m.insert("host".into(), opt(&self.host));
        m.insert("wsl".into(), Value::Object(wsl));
        Value::Object(m)
    }

    #[allow(
        dead_code,
        reason = "port of Orca's WSL selection; csm selects on the host only, and the tests keep the port whole"
    )]
    fn wsl_get(&self, key: &str) -> Option<&Option<String>> {
        self.wsl.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

fn id_value(v: Option<&Value>, what: &str) -> Result<Option<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{what} has an unexpected type")),
    }
}

/// d3 over the `settings` object. Pure.
pub fn d3(settings: &Map<String, Value>) -> Result<ActiveIds, String> {
    let by_runtime = match settings.get("activeClaudeManagedAccountIdsByRuntime") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => Some(o),
        Some(_) => {
            return Err("activeClaudeManagedAccountIdsByRuntime is not an object".to_owned());
        }
    };
    let active = id_value(
        settings.get("activeClaudeManagedAccountId"),
        "activeClaudeManagedAccountId",
    )?;
    let by_host = id_value(
        by_runtime.and_then(|o| o.get("host")),
        "activeClaudeManagedAccountIdsByRuntime.host",
    )?;
    let wsl = match by_runtime.and_then(|o| o.get("wsl")) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(w)) => w
            .iter()
            .map(|(k, v)| Ok((k.clone(), id_value(Some(v), "a WSL active id")?)))
            .collect::<Result<_, String>>()?,
        Some(_) => {
            return Err("activeClaudeManagedAccountIdsByRuntime.wsl is not an object".to_owned());
        }
    };
    Ok(ActiveIds {
        host: by_host.or(active),
        wsl,
    })
}

/// A runtime a selection applies to (u3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTarget {
    Host,
    /// A WSL distro, `None` for "no distro named".
    #[allow(
        dead_code,
        reason = "port of Orca's WSL selection; csm selects on the host only, and the tests keep the port whole"
    )]
    Wsl(Option<String>),
}

/// f3: the active id for `target`. Pure.
#[allow(
    dead_code,
    reason = "port of Orca's WSL selection; csm selects on the host only, and the tests keep the port whole"
)]
pub fn f3(active: &ActiveIds, target: &RuntimeTarget) -> Option<String> {
    match target {
        RuntimeTarget::Host => active.host.clone(),
        RuntimeTarget::Wsl(distro) => {
            if let Some(d) = trimmed(distro.as_deref()) {
                return active.wsl_get(&wsl_key(Some(&d))).cloned().flatten();
            }
            if let Some(Some(id)) = active.wsl_get("__default__") {
                return Some(id.clone());
            }
            let mut unique: Vec<&String> = Vec::new();
            for id in active.wsl.iter().filter_map(|(_, v)| v.as_ref()) {
                if !id.is_empty() && !unique.contains(&id) {
                    unique.push(id);
                }
            }
            (unique.len() == 1).then(|| unique[0].clone())
        }
    }
}

/// p3: the active ids after selecting `id` for `target`. Pure.
pub fn p3(active: &ActiveIds, id: Option<&str>, target: &RuntimeTarget) -> ActiveIds {
    let id = id.map(str::to_owned);
    match target {
        RuntimeTarget::Host => ActiveIds {
            host: id,
            wsl: active.wsl.clone(),
        },
        RuntimeTarget::Wsl(distro) => {
            let distro = trimmed(distro.as_deref());
            if id.is_none() && distro.is_none() {
                return ActiveIds {
                    host: active.host.clone(),
                    wsl: active.wsl.iter().map(|(k, _)| (k.clone(), None)).collect(),
                };
            }
            let key = wsl_key(distro.as_deref());
            let mut wsl = active.wsl.clone();
            match wsl.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = id,
                None => wsl.push((key, id)),
            }
            ActiveIds {
                host: active.host.clone(),
                wsl,
            }
        }
    }
}

/// t6i: the active ids with `id` cleared from the host slot and from every
/// WSL slot (Orca's `remove`, M:241835). Pure.
pub fn t6i(active: &ActiveIds, id: &str) -> ActiveIds {
    let clear = |v: &Option<String>| v.clone().filter(|x| x != id);
    ActiveIds {
        host: clear(&active.host),
        wsl: active
            .wsl
            .iter()
            .map(|(k, v)| (k.clone(), clear(v)))
            .collect(),
    }
}

/// n6i: drop active ids that name no record of the matching runtime. Pure.
pub fn n6i(active: &ActiveIds, records: &[AccountRecord]) -> ActiveIds {
    let host = active
        .host
        .as_ref()
        .filter(|h| records.iter().any(|r| &r.id == *h && r.is_host()))
        .cloned();
    let wsl = active
        .wsl
        .iter()
        .map(|(k, v)| {
            let keep = v.as_ref().filter(|id| {
                records.iter().any(|r| {
                    &r.id == *id
                        && r.runtime == AuthRuntime::Wsl
                        && wsl_key(r.wsl_distro.as_deref()) == *k
                })
            });
            (k.clone(), keep.cloned())
        })
        .collect();
    ActiveIds { host, wsl }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(id: &str, email: &str, org: Option<&str>) -> AccountRecord {
        AccountRecord::from_value(&json!({
            "id": id, "email": email, "managedAuthPath": "/Users/example/x",
            "managedAuthRuntime": "host", "organizationUuid": org
        }))
        .unwrap()
    }

    fn wsl_rec(id: &str, distro: Option<&str>) -> AccountRecord {
        AccountRecord::from_value(&json!({
            "id": id, "email": "alice@example.com",
            "managedAuthRuntime": "wsl", "wslDistro": distro
        }))
        .unwrap()
    }

    #[test]
    fn new_record_has_persists_key_order() {
        let v = new_record(
            &NewRecord {
                id: "id-1",
                email: "alice@example.com",
                managed_auth_path: "/Users/example/Library/Application Support/orca/claude-accounts/id-1/auth",
                organization_uuid: Some("org-1"),
                organization_name: None,
            },
            1_700_000_000_123,
        );
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "email",
                "managedAuthPath",
                "managedAuthRuntime",
                "wslDistro",
                "wslLinuxAuthPath",
                "authMethod",
                "organizationUuid",
                "organizationName",
                "createdAt",
                "updatedAt",
                "lastAuthenticatedAt"
            ]
        );
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains(r#""managedAuthRuntime":"host","wslDistro":null,"wslLinuxAuthPath":null,"authMethod":"subscription-oauth","organizationUuid":"org-1","organizationName":null,"createdAt":1700000000123"#), "{s}");
    }

    #[test]
    fn record_parse_is_strict_about_types_but_orca_lenient_on_runtime() {
        assert!(AccountRecord::from_value(&json!({"email": "a"})).is_err());
        assert!(AccountRecord::from_value(&json!({"id": ""})).is_err());
        assert!(AccountRecord::from_value(&json!({"id": "x", "email": 3})).is_err());
        let r =
            AccountRecord::from_value(&json!({"id": "x", "managedAuthRuntime": "other"})).unwrap();
        assert!(r.is_host(), "anything but \"wsl\" is the host, as in Orca");
        let r = AccountRecord::from_value(&json!({"id": "x", "createdAt": 1.5e12})).unwrap();
        assert_eq!(r.created_at, Some(1_500_000_000_000));
        assert!(parse_records(Some(&json!({}))).is_err());
        assert_eq!(parse_records(None).unwrap(), vec![]);
    }

    #[test]
    fn identity_normalises_email_org_and_runtime() {
        let k = IdentityKey::new(
            Some("  Alice@Example.COM "),
            Some(" org-1 "),
            AuthRuntime::Host,
            None,
        )
        .unwrap();
        assert_eq!(k.email, "alice@example.com");
        assert_eq!(k.organization_uuid.as_deref(), Some("org-1"));
        assert_eq!(k.runtime, "host");
        assert_eq!(
            IdentityKey::new(Some("  "), None, AuthRuntime::Host, None),
            None
        );
        assert_eq!(
            IdentityKey::new(
                Some("a@example.com"),
                Some(" "),
                AuthRuntime::Wsl,
                Some(" ")
            )
            .unwrap(),
            IdentityKey {
                email: "a@example.com".into(),
                organization_uuid: None,
                runtime: "wsl:__default__".into()
            }
        );
    }

    #[test]
    fn e9i_matches_on_the_triple_never_the_id() {
        let records = vec![
            rec("id-a", "alice@example.com", None),
            rec("id-b", "alice@example.com", Some("org-1")),
            rec("id-c", "bob@example.com", None),
        ];
        let key = IdentityKey::new(
            Some("ALICE@example.com"),
            Some("org-1"),
            AuthRuntime::Host,
            None,
        )
        .unwrap();
        assert_eq!(find_by_identity(&records, &key).unwrap().id, "id-b");
        let key =
            IdentityKey::new(Some("alice@example.com"), None, AuthRuntime::Host, None).unwrap();
        assert_eq!(find_by_identity(&records, &key).unwrap().id, "id-a");
        let key =
            IdentityKey::new(Some("alice@example.com"), None, AuthRuntime::Wsl, None).unwrap();
        assert!(find_by_identity(&records, &key).is_none());
    }

    fn settings(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn d3_prefers_by_runtime_host_then_active_id() {
        let s = settings(json!({
            "activeClaudeManagedAccountId": "a",
            "activeClaudeManagedAccountIdsByRuntime": {"host": "b", "wsl": {"Ubuntu": "w"}}
        }));
        let a = d3(&s).unwrap();
        assert_eq!(a.host.as_deref(), Some("b"));
        assert_eq!(a.wsl, vec![("Ubuntu".to_owned(), Some("w".to_owned()))]);

        // `??` treats null like absent.
        let s = settings(json!({
            "activeClaudeManagedAccountId": "a",
            "activeClaudeManagedAccountIdsByRuntime": {"host": null}
        }));
        assert_eq!(d3(&s).unwrap().host.as_deref(), Some("a"));
        assert_eq!(d3(&Map::new()).unwrap(), ActiveIds::default());

        // Unexpected types are errors, not guesses.
        assert!(d3(&settings(json!({"activeClaudeManagedAccountId": 1}))).is_err());
        assert!(
            d3(&settings(
                json!({"activeClaudeManagedAccountIdsByRuntime": {"wsl": []}})
            ))
            .is_err()
        );
        assert!(
            d3(&settings(
                json!({"activeClaudeManagedAccountIdsByRuntime": "x"})
            ))
            .is_err()
        );
    }

    #[test]
    fn f3_resolves_host_named_default_and_unique_wsl() {
        let a = ActiveIds {
            host: Some("h".into()),
            wsl: vec![("Ubuntu".into(), Some("u".into())), ("Debian".into(), None)],
        };
        assert_eq!(f3(&a, &RuntimeTarget::Host).as_deref(), Some("h"));
        assert_eq!(
            f3(&a, &RuntimeTarget::Wsl(Some(" Ubuntu ".into()))).as_deref(),
            Some("u")
        );
        assert_eq!(f3(&a, &RuntimeTarget::Wsl(Some("Debian".into()))), None);
        assert_eq!(f3(&a, &RuntimeTarget::Wsl(Some("Arch".into()))), None);
        // No distro: the single distinct id.
        assert_eq!(f3(&a, &RuntimeTarget::Wsl(None)).as_deref(), Some("u"));
        let two = ActiveIds {
            host: None,
            wsl: vec![
                ("A".into(), Some("x".into())),
                ("B".into(), Some("y".into())),
            ],
        };
        assert_eq!(f3(&two, &RuntimeTarget::Wsl(None)), None);
        let with_default = ActiveIds {
            host: None,
            wsl: vec![
                ("A".into(), Some("x".into())),
                ("__default__".into(), Some("d".into())),
            ],
        };
        assert_eq!(
            f3(&with_default, &RuntimeTarget::Wsl(None)).as_deref(),
            Some("d")
        );
    }

    #[test]
    fn p3_host_select_keeps_the_wsl_map() {
        let a = ActiveIds {
            host: Some("old".into()),
            wsl: vec![("Ubuntu".into(), Some("u".into()))],
        };
        let n = p3(&a, Some("new"), &RuntimeTarget::Host);
        assert_eq!(n.host.as_deref(), Some("new"));
        assert_eq!(n.wsl, a.wsl);
        assert_eq!(
            serde_json::to_string(&n.to_value()).unwrap(),
            r#"{"host":"new","wsl":{"Ubuntu":"u"}}"#
        );
        // WSL: named distro set, unnamed null clears every distro.
        let n = p3(&a, Some("v"), &RuntimeTarget::Wsl(Some("Debian".into())));
        assert_eq!(n.host.as_deref(), Some("old"));
        assert_eq!(n.wsl.len(), 2);
        let n = p3(&a, None, &RuntimeTarget::Wsl(None));
        assert_eq!(n.wsl, vec![("Ubuntu".into(), None)]);
        let n = p3(&a, Some("z"), &RuntimeTarget::Wsl(None));
        assert_eq!(n.wsl[1], ("__default__".into(), Some("z".into())));
    }

    #[test]
    fn n6i_drops_ids_of_missing_or_mismatched_records() {
        let records = vec![
            rec("h1", "alice@example.com", None),
            wsl_rec("w1", Some("Ubuntu")),
        ];
        let a = ActiveIds {
            host: Some("w1".into()), // a WSL record is not a host selection
            wsl: vec![
                ("Ubuntu".into(), Some("w1".into())),
                ("Debian".into(), Some("w1".into())), // wrong distro
                ("Arch".into(), Some("gone".into())),
            ],
        };
        let n = n6i(&a, &records);
        assert_eq!(n.host, None);
        assert_eq!(
            n.wsl,
            vec![
                ("Ubuntu".into(), Some("w1".into())),
                ("Debian".into(), None),
                ("Arch".into(), None)
            ]
        );
        let a = ActiveIds {
            host: Some("h1".into()),
            wsl: vec![],
        };
        assert_eq!(n6i(&a, &records).host.as_deref(), Some("h1"));
    }

    #[test]
    fn t6i_clears_the_id_everywhere() {
        let a = ActiveIds {
            host: Some("x".into()),
            wsl: vec![
                ("Ubuntu".into(), Some("x".into())),
                ("Debian".into(), Some("y".into())),
            ],
        };
        let t = t6i(&a, "x");
        assert_eq!(t.host, None);
        assert_eq!(
            t.wsl,
            vec![("Ubuntu".into(), None), ("Debian".into(), Some("y".into()))]
        );
        assert_eq!(t6i(&a, "z"), a);
    }
}
