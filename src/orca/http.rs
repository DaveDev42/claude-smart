//! The HTTP seam for the two OAuth calls the offline switch makes: the token
//! refresh (Orca's Y2i) and the profile veto (Claude Code's
//! `/api/oauth/profile`). Tests inject answers through [`OauthHttp`]; the
//! real client ([`SystemHttp`]) refuses to run under `cfg(test)`.
//!
//! Requests, exactly as their sources send them:
//! - refresh (M:240427-240441): `POST https://platform.claude.com/v1/oauth/token`,
//!   `Content-Type: application/x-www-form-urlencoded`, body
//!   `grant_type=refresh_token&refresh_token=<trimmed>&client_id=<Claude Code's>`
//!   in `URLSearchParams` encoding, 10 s;
//! - profile (Claude Code 2.1.283): `GET <api base>/api/oauth/profile` with
//!   `Authorization: Bearer <access token>`, `Content-Type:
//!   application/json`, `Cache-Control: no-cache`, 10 s.
//!
//! "No answer" (DNS, connect, timeout, a broken body) is its own error,
//! [`HttpError::NoAnswer`]: the profile veto aborts on it instead of reading
//! it as a mismatch. Reply bodies carry tokens: they are [`SecretBytes`] and
//! never printed.

use std::time::Duration;

use serde_json::Value;

use super::SecretBytes;

/// Orca's (and Claude Code's) token endpoint.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's OAuth client id.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Both calls' timeout (Orca's W2i, Claude Code's profile fetch).
#[cfg_attr(test, allow(dead_code, reason = "the test build has no real client"))]
pub const TIMEOUT: Duration = Duration::from_secs(10);

// ─── seam ─────────────────────────────────────────────────────────────────────

/// Why a call produced no HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// DNS, connect, TLS, timeout or a body that could not be read.
    #[error("no answer from {0}")]
    NoAnswer(String),
    /// The call was refused before anything was sent.
    #[error("refused: {0}")]
    Refused(String),
}

/// An HTTP answer. The body may hold tokens.
#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    pub body: SecretBytes,
}

impl HttpReply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON, when it is JSON.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(self.body.expose()).ok()
    }
}

/// The two OAuth calls.
pub trait OauthHttp {
    /// POST the form-encoded `body` to the token endpoint.
    fn post_token(&self, body: &str) -> Result<HttpReply, HttpError>;
    /// GET the profile for `access_token`.
    fn get_profile(&self, access_token: &str) -> Result<HttpReply, HttpError>;
}

// ─── pure helpers ─────────────────────────────────────────────────────────────

/// `application/x-www-form-urlencoded` as `URLSearchParams` serializes it.
/// Pure.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Orca's refresh body for `refresh_token` (already trimmed). Pure.
pub fn refresh_body(refresh_token: &str) -> String {
    form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ])
}

/// What the profile endpoint said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAnswer {
    /// 200 with the account and organization uuids (either may be absent).
    Ok {
        account_uuid: Option<String>,
        organization_uuid: Option<String>,
    },
    /// 401: the access token is dead (expired or revoked).
    Unauthorized,
    /// Any other status, or a 200 whose body is not JSON.
    Other(u16),
}

/// Read a profile reply. Pure.
pub fn parse_profile(reply: &HttpReply) -> ProfileAnswer {
    if reply.status == 401 {
        return ProfileAnswer::Unauthorized;
    }
    if !reply.ok() {
        return ProfileAnswer::Other(reply.status);
    }
    let Some(v) = reply.json() else {
        return ProfileAnswer::Other(reply.status);
    };
    let s = |a: &str, b: &str| {
        v.get(a)
            .and_then(|o| o.get(b))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    ProfileAnswer::Ok {
        account_uuid: s("account", "uuid"),
        organization_uuid: s("organization", "uuid"),
    }
}

// ─── the real client ──────────────────────────────────────────────────────────

/// reqwest behind the seam.
#[derive(Debug, Clone)]
pub struct SystemHttp {
    /// The API base for the profile call (`CSM_USAGE_API_BASE` or
    /// `https://api.anthropic.com`).
    pub api_base: String,
    /// The token endpoint (`CSM_OAUTH_TOKEN_URL` or [`TOKEN_URL`]).
    pub token_url: String,
}

impl SystemHttp {
    /// The endpoints from the environment. Overrides must be https or
    /// loopback, the rule the usage collector already applies.
    pub fn from_env() -> SystemHttp {
        let token_url = std::env::var("CSM_OAUTH_TOKEN_URL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| TOKEN_URL.to_owned());
        SystemHttp {
            api_base: crate::usage::local::api::resolve_base(),
            token_url,
        }
    }

    #[cfg(not(test))]
    fn client(&self) -> Result<reqwest::blocking::Client, HttpError> {
        crate::usage::local::api::http_client(TIMEOUT)
            .map_err(|e| HttpError::Refused(e.to_string()))
    }

    /// Test build: the real network is never used.
    #[cfg(test)]
    fn client(&self) -> Result<reqwest::blocking::Client, HttpError> {
        Err(HttpError::Refused(
            "cfg(test): the real HTTP client is disabled".into(),
        ))
    }

    fn send(url: &str, req: reqwest::blocking::RequestBuilder) -> Result<HttpReply, HttpError> {
        let resp = req
            .send()
            .map_err(|e| HttpError::NoAnswer(format!("{url} ({})", e.without_url())))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .map_err(|_| HttpError::NoAnswer(format!("{url} (body)")))?;
        Ok(HttpReply {
            status,
            body: SecretBytes::new(body.to_vec()),
        })
    }
}

impl OauthHttp for SystemHttp {
    fn post_token(&self, body: &str) -> Result<HttpReply, HttpError> {
        // Decision 8's probe: the hook's tests assert it is never reached.
        crate::usage::reach::note("oauth-token");
        crate::usage::local::api::validate_base(&self.token_url)
            .map_err(|_| HttpError::Refused("unsafe token endpoint".into()))?;
        let client = self.client()?;
        let req = client
            .post(&self.token_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body.to_owned());
        Self::send(&self.token_url, req)
    }

    fn get_profile(&self, access_token: &str) -> Result<HttpReply, HttpError> {
        crate::usage::reach::note("oauth-profile");
        crate::usage::local::api::validate_base(&self.api_base)
            .map_err(|_| HttpError::Refused("unsafe API base".into()))?;
        let client = self.client()?;
        let url = format!("{}/api/oauth/profile", self.api_base);
        let req = client
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {access_token}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::CACHE_CONTROL, "no-cache");
        Self::send(&url, req)
    }
}

// ─── test fake ────────────────────────────────────────────────────────────────

/// Canned answers, recorded requests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeHttp {
    /// Token replies in order; `Err` = no answer. Empty = no answer.
    pub token: std::sync::Mutex<std::collections::VecDeque<Result<HttpReply, HttpError>>>,
    /// Profile answers by access token; absent = no answer.
    pub profiles: std::sync::Mutex<std::collections::HashMap<String, Result<HttpReply, HttpError>>>,
    pub token_bodies: std::sync::Mutex<Vec<String>>,
    pub profile_calls: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeHttp {
    pub(crate) fn reply(status: u16, body: &str) -> Result<HttpReply, HttpError> {
        Ok(HttpReply {
            status,
            body: SecretBytes::new(body.as_bytes().to_vec()),
        })
    }

    pub(crate) fn token_reply(self, r: Result<HttpReply, HttpError>) -> Self {
        self.token.lock().unwrap().push_back(r);
        self
    }

    pub(crate) fn profile(self, access: &str, r: Result<HttpReply, HttpError>) -> Self {
        self.profiles.lock().unwrap().insert(access.to_owned(), r);
        self
    }

    /// A 200 profile naming `uuid`.
    pub(crate) fn profile_uuid(self, access: &str, uuid: &str) -> Self {
        let body = serde_json::json!({"account": {"uuid": uuid}, "organization": {"uuid": null}});
        self.profile(access, Self::reply(200, &body.to_string()))
    }
}

#[cfg(test)]
impl OauthHttp for FakeHttp {
    fn post_token(&self, body: &str) -> Result<HttpReply, HttpError> {
        self.token_bodies.lock().unwrap().push(body.to_owned());
        self.token
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(HttpError::NoAnswer("fake token endpoint".into())))
    }

    fn get_profile(&self, access_token: &str) -> Result<HttpReply, HttpError> {
        self.profile_calls
            .lock()
            .unwrap()
            .push(access_token.to_owned());
        self.profiles
            .lock()
            .unwrap()
            .get(access_token)
            .cloned()
            .unwrap_or_else(|| Err(HttpError::NoAnswer("fake profile endpoint".into())))
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Both OAuth calls note their decision-8 probe step before anything
    /// else, so the hook's "reached nothing" tests cover them too. The test
    /// build's client refuses, so nothing leaves the machine.
    #[test]
    fn oauth_calls_note_their_probe_step() {
        let http = SystemHttp {
            api_base: "https://api.anthropic.com".into(),
            token_url: TOKEN_URL.into(),
        };
        crate::usage::reach::take();
        assert!(http.post_token("grant_type=refresh_token").is_err());
        assert!(http.get_profile("at-test").is_err());
        assert_eq!(
            crate::usage::reach::take(),
            vec!["oauth-token", "oauth-profile"]
        );
    }

    #[test]
    fn form_encoding_matches_urlsearchparams() {
        assert_eq!(
            refresh_body("rt-abc_1.2"),
            format!("grant_type=refresh_token&refresh_token=rt-abc_1.2&client_id={CLIENT_ID}")
        );
        assert_eq!(form_encode(&[("a b", "x/y+z~é")]), "a+b=x%2Fy%2Bz%7E%C3%A9");
    }

    #[test]
    fn profile_answers() {
        let r = FakeHttp::reply(
            200,
            r#"{"account":{"uuid":"u-1","email":"alice@example.com"},"organization":{"uuid":"o-1"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_profile(&r),
            ProfileAnswer::Ok {
                account_uuid: Some("u-1".into()),
                organization_uuid: Some("o-1".into())
            }
        );
        assert_eq!(
            parse_profile(&FakeHttp::reply(401, "{}").unwrap()),
            ProfileAnswer::Unauthorized
        );
        assert_eq!(
            parse_profile(&FakeHttp::reply(500, "").unwrap()),
            ProfileAnswer::Other(500)
        );
        assert_eq!(
            parse_profile(&FakeHttp::reply(200, "<html>").unwrap()),
            ProfileAnswer::Other(200)
        );
        assert_eq!(
            parse_profile(&FakeHttp::reply(200, "{}").unwrap()),
            ProfileAnswer::Ok {
                account_uuid: None,
                organization_uuid: None
            }
        );
    }

    #[test]
    fn the_real_client_refuses_under_test() {
        let h = SystemHttp {
            api_base: "https://api.anthropic.com".into(),
            token_url: TOKEN_URL.into(),
        };
        assert!(matches!(
            h.get_profile("at-secret"),
            Err(HttpError::Refused(_))
        ));
        assert!(matches!(
            h.post_token(&refresh_body("rt-secret")),
            Err(HttpError::Refused(_))
        ));
        let unsafe_base = SystemHttp {
            api_base: "http://example.com".into(),
            token_url: "http://example.com/token".into(),
        };
        assert!(matches!(
            unsafe_base.get_profile("x"),
            Err(HttpError::Refused(_))
        ));
    }

    #[test]
    fn replies_never_print_their_body() {
        let r = FakeHttp::reply(200, r#"{"access_token":"sk-secret"}"#).unwrap();
        assert!(!format!("{r:?}").contains("sk-secret"));
    }
}
