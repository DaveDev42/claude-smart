//! The OAuth refresh of a stashed grant, ported from Orca 1.4.209
//! (M:240384-240449, and refreshManagedAccountTokenIfNeeded M:247224-247234).
//!
//! - G2i: `claudeAiOauth` when it is a JSON object ([`oauth_object`]);
//! - K2i: its `refreshToken`, trimmed, non-blank ([`refresh_token_of`]);
//! - q2i: a refresh is due when `expiresAt` is not a finite number or
//!   `now + 300000 >= expiresAt` ([`needs_refresh`]);
//! - J2i: merge the token reply into the grant: `accessToken` always,
//!   `expiresAt = now + expires_in * 1000` when `expires_in` is a finite
//!   number, `refreshToken` when the reply's is non-blank (kept untrimmed),
//!   `scopes = scope.split(" ")` when non-blank; everything else kept in
//!   place; the result is compact `JSON.stringify` ([`merge_token_reply`]);
//! - Y2i: POST `grant_type=refresh_token&refresh_token=<K2i>&client_id=...`
//!   form-encoded to the token endpoint, 10 s; a non-2xx answer or any
//!   failure is "no refresh" ([`refresh_grant`]).
//!
//! refreshManagedAccountTokenIfNeeded writes the refreshed grant to the
//! stash only; when that write fails Orca drops the rotated grant, which may
//! have invalidated the old refresh token. csm files it in the quarantine
//! instead ([`refresh_stash_if_needed`]).

use std::path::Path;

use serde_json::{Map, Value};

use super::http::{self, OauthHttp};
use super::jsjson;
use super::quarantine::{Quarantine, Reason};
use super::stash::{self, Stash};
use super::userdata::HostOs;
use super::{OrcaError, SecretString};

/// q2i's skew (U2i).
pub const SKEW_MS: f64 = 300_000.0;

// ─── pure core ────────────────────────────────────────────────────────────────

/// G2i. Pure.
pub fn oauth_object(json: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(json).ok()? {
        Value::Object(mut m) => match m.remove("claudeAiOauth") {
            Some(Value::Object(o)) => Some(o),
            _ => None,
        },
        _ => None,
    }
}

/// K2i. Pure.
pub fn refresh_token_of(json: &str) -> Option<String> {
    let o = oauth_object(json)?;
    let t = o.get("refreshToken")?.as_str()?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// A JSON number as the double JS would hold; `None` when it is not a
/// finite one.
fn finite(v: &Value) -> Option<f64> {
    let n = v.as_number()?;
    n.to_string().parse::<f64>().ok().filter(|f| f.is_finite())
}

/// q2i. Pure.
pub fn needs_refresh(json: &str, now_ms: i64) -> bool {
    let Some(o) = oauth_object(json) else {
        return false;
    };
    match o.get("expiresAt").and_then(finite) {
        None => true,
        Some(exp) => now_ms as f64 + SKEW_MS >= exp,
    }
}

fn js_number_value(f: f64) -> Value {
    serde_json::from_str(&jsjson::js_number(f)).unwrap_or(Value::Null)
}

/// J2i: merge a token reply into the grant `json`. `None` when the grant is
/// not JSON, has no `claudeAiOauth` object, or the reply carries no
/// non-blank `access_token`. Pure.
pub fn merge_token_reply(json: &str, reply: &Value, now_ms: i64) -> Option<SecretString> {
    let mut r: Value = serde_json::from_str(json).ok()?;
    let access = reply.get("access_token")?.as_str()?;
    if access.trim().is_empty() {
        return None;
    }
    let root = r.as_object_mut()?;
    let mut a = match root.get("claudeAiOauth") {
        Some(Value::Object(o)) => o.clone(),
        Some(Value::Null) | None => Map::new(),
        // A spread of a string or array copies indices; Orca never gets
        // here (K2i needs an object), so csm refuses.
        Some(_) => return None,
    };
    a.insert("accessToken".into(), Value::String(access.to_owned()));
    if let Some(exp_in) = reply.get("expires_in").and_then(finite) {
        let exp = now_ms as f64 + exp_in * 1000.0;
        if exp.is_finite() {
            a.insert("expiresAt".into(), js_number_value(exp));
        }
    }
    if let Some(rt) = reply.get("refresh_token").and_then(Value::as_str)
        && !rt.trim().is_empty()
    {
        a.insert("refreshToken".into(), Value::String(rt.to_owned()));
    }
    if let Some(scope) = reply.get("scope").and_then(Value::as_str)
        && !scope.trim().is_empty()
    {
        a.insert(
            "scopes".into(),
            Value::Array(
                scope
                    .split(' ')
                    .map(|s| Value::String(s.to_owned()))
                    .collect(),
            ),
        );
    }
    root.insert("claudeAiOauth".into(), Value::Object(a));
    Some(SecretString::new(jsjson::stringify(&r)))
}

// ─── the call ─────────────────────────────────────────────────────────────────

/// Why a refresh produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshFail {
    /// The grant has no refresh token.
    NoRefreshToken,
    /// No HTTP answer.
    NoAnswer,
    /// The endpoint answered with this non-2xx status.
    Status(u16),
    /// A 2xx whose body J2i could not merge, or whose result is not a valid
    /// credential.
    BadReply,
}

/// Y2i: refresh `json` once. The result is J2i's compact merge.
pub fn refresh_grant(
    json: &str,
    http: &dyn OauthHttp,
    now_ms: i64,
) -> Result<SecretString, RefreshFail> {
    let rt = refresh_token_of(json).ok_or(RefreshFail::NoRefreshToken)?;
    let reply = http
        .post_token(&http::refresh_body(&rt))
        .map_err(|_| RefreshFail::NoAnswer)?;
    if !reply.ok() {
        return Err(RefreshFail::Status(reply.status));
    }
    let body = reply.json().ok_or(RefreshFail::BadReply)?;
    let merged = merge_token_reply(json, &body, now_ms).ok_or(RefreshFail::BadReply)?;
    if !stash::credentials_are_valid(merged.expose()) {
        return Err(RefreshFail::BadReply);
    }
    Ok(merged)
}

/// What [`refresh_stash_if_needed`] did.
#[derive(Debug)]
pub enum StashRefresh {
    /// q2i said the grant is fresh enough.
    NotDue,
    /// The refresh produced nothing; the stash is unchanged.
    Failed(RefreshFail),
    /// The refreshed grant is in the stash.
    Refreshed(SecretString),
    /// The refreshed grant could not be written to the stash; it is in the
    /// quarantine under this fingerprint and the old grant is still used.
    Quarantined(String),
}

/// refreshManagedAccountTokenIfNeeded for account `stash` holding `json`.
pub fn refresh_stash_if_needed(
    user_data: &Path,
    os: HostOs,
    stash: &Stash,
    json: &str,
    http: &dyn OauthHttp,
    quarantine: &Quarantine,
    now_ms: i64,
) -> Result<StashRefresh, OrcaError> {
    if !needs_refresh(json, now_ms) {
        return Ok(StashRefresh::NotDue);
    }
    let fresh = match refresh_grant(json, http, now_ms) {
        Ok(f) => f,
        Err(e) => return Ok(StashRefresh::Failed(e)),
    };
    match stash.write_credentials(user_data, os, fresh.expose()) {
        Ok(()) => Ok(StashRefresh::Refreshed(fresh)),
        Err(_) => {
            let filed = quarantine.file(
                fresh.expose(),
                Reason::PersistFailed,
                "refresh",
                Some(&stash.id),
                None,
                now_ms,
            )?;
            Ok(StashRefresh::Quarantined(filed.fingerprint().to_owned()))
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::http::FakeHttp;
    use serde_json::json;

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn q2i_and_k2i() {
        let fresh = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":" r ","expiresAt":{}}}}}"#,
            NOW + 300_001
        );
        assert!(!needs_refresh(&fresh, NOW));
        let due = fresh.replace(&(NOW + 300_001).to_string(), &(NOW + 300_000).to_string());
        assert!(needs_refresh(&due, NOW));
        assert!(needs_refresh(r#"{"claudeAiOauth":{"expiresAt":"1"}}"#, NOW));
        assert!(needs_refresh(
            r#"{"claudeAiOauth":{"expiresAt":1e400}}"#,
            NOW
        ));
        assert!(needs_refresh(r#"{"claudeAiOauth":{}}"#, NOW));
        assert!(!needs_refresh(r#"{"claudeAiOauth":[]}"#, NOW));
        assert!(!needs_refresh("null", NOW));
        assert!(!needs_refresh("not json", NOW));
        assert_eq!(refresh_token_of(&fresh).as_deref(), Some("r"));
        assert_eq!(
            refresh_token_of(r#"{"claudeAiOauth":{"refreshToken":"  "}}"#),
            None
        );
        assert_eq!(
            refresh_token_of(r#"{"claudeAiOauth":{"refreshToken":1}}"#),
            None
        );
    }

    #[test]
    fn j2i_merges_in_place_and_prints_compact() {
        let src = r#"{"x":1.0,"claudeAiOauth":{"accessToken":"old","refreshToken":"r0","expiresAt":5,"scopes":["a"],"subscriptionType":"max"},"2":true}"#;
        let reply = json!({"access_token":"new","expires_in":3600,"refresh_token":" r1 ","scope":"user:inference  user:profile"});
        let out = merge_token_reply(src, &reply, NOW).unwrap();
        assert_eq!(
            out.expose(),
            format!(
                r#"{{"2":true,"x":1,"claudeAiOauth":{{"accessToken":"new","refreshToken":" r1 ","expiresAt":{},"scopes":["user:inference","","user:profile"],"subscriptionType":"max"}}}}"#,
                NOW + 3_600_000
            )
        );
        // Only the access token: everything else kept.
        let out = merge_token_reply(
            src,
            &json!({"access_token":"n2","refresh_token":"","expires_in":"9"}),
            NOW,
        )
        .unwrap();
        assert!(
            out.expose()
                .contains(r#""refreshToken":"r0","expiresAt":5,"scopes":["a"]"#)
        );
        assert!(merge_token_reply(src, &json!({"access_token":" "}), NOW).is_none());
        assert!(merge_token_reply(src, &json!(null), NOW).is_none());
        assert!(merge_token_reply("nope", &json!({"access_token":"a"}), NOW).is_none());
        // A missing claudeAiOauth is created, as the JS spread of undefined.
        let out = merge_token_reply("{}", &json!({"access_token":"a"}), NOW).unwrap();
        assert_eq!(out.expose(), r#"{"claudeAiOauth":{"accessToken":"a"}}"#);
    }

    #[test]
    fn y2i_posts_the_form_and_maps_failures() {
        let grant = creds(" rt-1 ");
        let http = FakeHttp::default().token_reply(FakeHttp::reply(
            200,
            r#"{"access_token":"at-2","expires_in":60}"#,
        ));
        let out = refresh_grant(&grant, &http, NOW).unwrap();
        assert!(out.expose().contains(r#""accessToken":"at-2""#));
        assert_eq!(
            http.token_bodies.lock().unwrap()[0],
            format!(
                "grant_type=refresh_token&refresh_token=rt-1&client_id={}",
                http::CLIENT_ID
            )
        );
        let http = FakeHttp::default().token_reply(FakeHttp::reply(400, "{}"));
        assert_eq!(
            refresh_grant(&grant, &http, NOW).unwrap_err(),
            RefreshFail::Status(400)
        );
        let http = FakeHttp::default();
        assert_eq!(
            refresh_grant(&grant, &http, NOW).unwrap_err(),
            RefreshFail::NoAnswer
        );
        let http = FakeHttp::default().token_reply(FakeHttp::reply(200, "[]"));
        assert_eq!(
            refresh_grant(&grant, &http, NOW).unwrap_err(),
            RefreshFail::BadReply
        );
        assert_eq!(
            refresh_grant(r#"{"claudeAiOauth":{}}"#, &FakeHttp::default(), NOW).unwrap_err(),
            RefreshFail::NoRefreshToken
        );
    }

    fn creds(rt: &str) -> String {
        json!({"claudeAiOauth":{"accessToken":"at-1","refreshToken":rt,"expiresAt":1}}).to_string()
    }

    #[test]
    fn refreshing_a_stash_writes_it_or_quarantines_the_rotated_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path().join("ud");
        let state = tmp.path().join("state");
        let q = Quarantine::new(HostOs::Linux, &state);
        let s = stash::create(&ud, "id-a").unwrap();
        let grant = creds("rt-1");
        s.write_credentials(&ud, HostOs::Linux, &grant).unwrap();
        let ok = || {
            FakeHttp::default().token_reply(FakeHttp::reply(
                200,
                r#"{"access_token":"at-2","refresh_token":"rt-2"}"#,
            ))
        };
        match refresh_stash_if_needed(&ud, HostOs::Linux, &s, &grant, &ok(), &q, NOW).unwrap() {
            StashRefresh::Refreshed(f) => assert_eq!(
                s.credentials(HostOs::Linux).unwrap().unwrap().expose(),
                f.expose()
            ),
            other => panic!("{other:?}"),
        }
        // Not due: no call.
        let far =
            json!({"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":NOW * 2}})
                .to_string();
        assert!(matches!(
            refresh_stash_if_needed(&ud, HostOs::Linux, &s, &far, &FakeHttp::default(), &q, NOW)
                .unwrap(),
            StashRefresh::NotDue
        ));
        // The stash write fails (marker gone): the rotated grant is filed.
        std::fs::remove_file(s.auth_dir.join(stash::MARKER_FILE)).unwrap();
        match refresh_stash_if_needed(&ud, HostOs::Linux, &s, &grant, &ok(), &q, NOW).unwrap() {
            StashRefresh::Quarantined(fp) => {
                assert!(q.get(&fp).unwrap().unwrap().expose().contains("at-2"));
            }
            other => panic!("{other:?}"),
        }
    }
}
