//! Orca's local RPC: one newline-delimited JSON request per connection over
//! the unix socket Orca advertises in `orca-runtime.json`.
//!
//! Wire contract (Orca 1.4.209, the same framing Orca's own CLI client uses):
//! - write ONE line `{"id":"<uuid>","authToken":"…","method":"…","params":{…}}\n`;
//! - read newline-delimited frames, skipping `{"_keepalive":true}`;
//! - the first frame whose `id` matches is final — `{id, ok:true, result,
//!   _meta:{runtimeId}}` or `{id, ok:false, error:{code,message,data?}, _meta}`;
//! - a frame whose `_meta.runtimeId` differs from the metadata file's
//!   `runtimeId` came from another Orca instance and is rejected;
//! - the response is capped at [`MAX_RESPONSE_BYTES`].
//!
//! Only two methods can be encoded at all ([`Method`]): `accounts.list` with
//! `"refreshUsage":false` spelled out (the server default is `true`, which
//! triggers an Orca sync plus network fetches), and `accounts.selectClaude`
//! with a NON-empty account id — a `null` id (Orca's "System default"
//! restore) is unrepresentable. The websocket and Windows named-pipe
//! transports are never used; without a unix socket every call is
//! [`RpcError::Unsupported`].
//!
//! Every call runs on a worker thread and the caller waits with
//! `recv_timeout` (the `statusline.rs` `read_stdin_capped` pattern):
//! `UnixStream::connect` has no timeout, so a wedged socket must never be
//! able to hang a launch. On timeout the thread is abandoned; its own socket
//! timeouts end it shortly after.
//!
//! Secrets: the `authToken` lives only in memory ([`AuthToken`], redacting
//! `Debug`, no `Display`/`Serialize`). The encoded request line embeds it, so
//! [`RequestLine`] redacts too and is zeroed and dropped right after the
//! write. Nothing here logs, prints, or persists either.
//!
//! Negative cache: after a launch-path read times out, `<smart-dir>/orca-rpc-
//! down` = `{runtimeId, until}` makes later launch-path reads skip the RPC for
//! [`NEGATIVE_CACHE_SECS`] while Orca's `runtimeId` is unchanged.

// Off unix there is no transport, so the framing core is only exercised by
// the tests (the homeguard.rs precedent).
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::RuntimeMetadata;

/// Hard cap on one RPC response (all frames together).
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How long a timed-out launch-path read suppresses further reads.
pub const NEGATIVE_CACHE_SECS: i64 = 60;

// ─── secrets ────────────────────────────────────────────────────────────────

/// Orca's per-start RPC `authToken`. In memory only: `Debug` redacts, and
/// there is deliberately no `Display` or `Serialize`.
#[derive(Clone)]
pub struct AuthToken(String);

impl AuthToken {
    pub(crate) fn new(token: String) -> Self {
        AuthToken(token)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

/// An encoded request line. It embeds the auth token, so `Debug` redacts and
/// `Drop` zeroes the bytes before freeing them.
pub struct RequestLine(Vec<u8>);

impl RequestLine {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for RequestLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestLine(<redacted>)")
    }
}

impl Drop for RequestLine {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: `b` is a valid, aligned, exclusively borrowed u8; the
            // volatile write only keeps the zeroing from being optimized out.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

// ─── methods ────────────────────────────────────────────────────────────────

/// The only RPC methods csm can encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `accounts.list {"refreshUsage":false}` — read-only snapshot.
    AccountsList,
    /// `accounts.selectClaude {"accountId":"<id>"}` — the one write surface.
    /// Build it with [`Method::select_claude`], which refuses an empty id.
    SelectClaude { account_id: String },
}

impl Method {
    /// `accounts.selectClaude` for `account_id`, or `None` when the id is
    /// empty/blank — a null or empty id is never sent.
    pub fn select_claude(account_id: &str) -> Option<Method> {
        let id = account_id.trim();
        (!id.is_empty()).then(|| Method::SelectClaude {
            account_id: id.to_owned(),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Method::AccountsList => "accounts.list",
            Method::SelectClaude { .. } => "accounts.selectClaude",
        }
    }

    fn params(&self) -> Value {
        match self {
            Method::AccountsList => json!({ "refreshUsage": false }),
            Method::SelectClaude { account_id } => json!({ "accountId": account_id }),
        }
    }
}

// ─── pure core ──────────────────────────────────────────────────────────────

/// Encode one request line (with its trailing `\n`). Pure.
pub fn encode_request(id: &str, token: &AuthToken, method: &Method) -> RequestLine {
    let v = json!({
        "id": id,
        "authToken": token.expose(),
        "method": method.name(),
        "params": method.params(),
    });
    let mut bytes = serde_json::to_vec(&v).unwrap_or_default();
    bytes.push(b'\n');
    RequestLine(bytes)
}

/// Why an RPC produced no usable result. Messages never carry the token.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    #[error("Orca exposes no unix-socket transport (named pipe / websocket are not used)")]
    Unsupported,
    #[error("timed out waiting for Orca")]
    Timeout,
    #[error("Orca RPC I/O error: {0}")]
    Io(String),
    #[error("connection closed before Orca answered")]
    Closed,
    #[error("Orca response exceeded {MAX_RESPONSE_BYTES} bytes")]
    TooLarge,
    #[error("malformed Orca response: {0}")]
    Malformed(String),
    #[error("Orca response came from a different runtime instance")]
    RuntimeIdMismatch,
    #[error("Orca rejected the request ({code}): {message}")]
    Remote { code: String, message: String },
    /// The request was never written: the connect failed, or the deadline
    /// had already passed. Unlike `Timeout`/`Io` after the write, Orca
    /// cannot have acted on it.
    #[error("the request never reached Orca: {0}")]
    NotSent(String),
}

impl RpcError {
    /// Orca answers `ok:false` "Account services are not configured on this
    /// runtime" while it is still starting. That is "try later", not failure.
    pub fn is_services_starting(&self) -> bool {
        matches!(self, RpcError::Remote { message, .. }
            if message.contains("Account services are not configured"))
    }
}

/// Result of scanning the bytes received so far.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameScan {
    /// No final frame yet (only keepalives, foreign ids, or a partial line).
    Incomplete,
    /// The final frame for `want_id`: its `result`, or why it failed.
    Final(Result<Value, RpcError>),
}

/// Scan `buf` for the final frame answering `want_id`. Pure.
///
/// Only complete (`\n`-terminated) lines are considered; trailing partial
/// data waits for the next read. Keepalives and frames for other ids are
/// skipped. A non-JSON line is malformed. Over [`MAX_RESPONSE_BYTES`] with no
/// final frame found is [`RpcError::TooLarge`].
pub fn parse_frames(buf: &[u8], want_id: &str, runtime_id: &str) -> FrameScan {
    let mut rest = buf;
    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let line = rest[..nl].trim_ascii();
        rest = &rest[nl + 1..];
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => {
                return FrameScan::Final(Err(RpcError::Malformed(
                    "response frame is not JSON".to_owned(),
                )));
            }
        };
        if v.get("_keepalive").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if v.get("id").and_then(Value::as_str) != Some(want_id) {
            continue;
        }
        let frame_rid = v
            .get("_meta")
            .and_then(|m| m.get("runtimeId"))
            .and_then(Value::as_str);
        return FrameScan::Final(match v.get("ok").and_then(Value::as_bool) {
            Some(true) => {
                if frame_rid != Some(runtime_id) {
                    Err(RpcError::RuntimeIdMismatch)
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Some(false) => {
                // Orca's error frames may omit/null `_meta.runtimeId`; only a
                // present-and-different id is a foreign instance.
                if frame_rid.is_some_and(|r| r != runtime_id) {
                    Err(RpcError::RuntimeIdMismatch)
                } else {
                    let err = v.get("error");
                    let field = |k: &str| {
                        err.and_then(|e| e.get(k))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    Err(RpcError::Remote {
                        code: field("code"),
                        message: field("message"),
                    })
                }
            }
            None => Err(RpcError::Malformed("final frame has no ok flag".to_owned())),
        });
    }
    if buf.len() > MAX_RESPONSE_BYTES {
        FrameScan::Final(Err(RpcError::TooLarge))
    } else {
        FrameScan::Incomplete
    }
}

// ─── transport ──────────────────────────────────────────────────────────────

/// Call `method` on the Orca instance described by `meta`, giving up at
/// `deadline`. Unix socket only; anything else is [`RpcError::Unsupported`].
pub fn call(meta: &RuntimeMetadata, method: &Method, deadline: Instant) -> Result<Value, RpcError> {
    let endpoint = meta.unix_endpoint().ok_or(RpcError::Unsupported)?;
    call_unix(
        Path::new(endpoint),
        &meta.auth_token,
        &meta.runtime_id,
        method,
        deadline,
    )
}

/// The worker-thread transport behind [`call`] (a seam for the fake-socket
/// tests).
#[cfg(unix)]
pub(crate) fn call_unix(
    endpoint: &Path,
    token: &AuthToken,
    runtime_id: &str,
    method: &Method,
    deadline: Instant,
) -> Result<Value, RpcError> {
    use std::sync::mpsc;

    if deadline <= Instant::now() {
        return Err(RpcError::NotSent("no time left in the budget".to_owned()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let line = encode_request(&id, token, method);
    let endpoint = endpoint.to_path_buf();
    let rid = runtime_id.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = exchange(&endpoint, line, &id, &rid, deadline);
        let _ = tx.send(r);
    });
    let wait = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(wait).unwrap_or(Err(RpcError::Timeout))
}

#[cfg(not(unix))]
pub(crate) fn call_unix(
    _endpoint: &Path,
    _token: &AuthToken,
    _runtime_id: &str,
    _method: &Method,
    _deadline: Instant,
) -> Result<Value, RpcError> {
    Err(RpcError::Unsupported)
}

/// One request/response exchange on a fresh connection (worker thread).
#[cfg(unix)]
fn exchange(
    endpoint: &Path,
    line: RequestLine,
    want_id: &str,
    runtime_id: &str,
    deadline: Instant,
) -> Result<Value, RpcError> {
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;

    let io_err = |e: std::io::Error| RpcError::Io(e.to_string());
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(RpcError::Timeout)
    };

    let not_sent = |e: RpcError| RpcError::NotSent(e.to_string());
    let mut stream = UnixStream::connect(endpoint)
        .map_err(|e| RpcError::NotSent(format!("connect failed: {e}")))?;
    // Socket timeouts are best-effort: macOS refuses the setsockopt with
    // EINVAL once the peer has already answered and closed. The caller's
    // `recv_timeout` is what bounds the wait; these only let an abandoned
    // worker end sooner.
    let _ = stream.set_write_timeout(Some(remaining().map_err(not_sent)?));
    stream.write_all(line.as_bytes()).map_err(io_err)?;
    drop(line);

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let _ = stream.set_read_timeout(Some(remaining()?));
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(RpcError::Timeout);
            }
            Err(e) => return Err(io_err(e)),
        };
        if n == 0 {
            return match parse_frames(&buf, want_id, runtime_id) {
                FrameScan::Final(r) => r,
                FrameScan::Incomplete => Err(RpcError::Closed),
            };
        }
        buf.extend_from_slice(&chunk[..n]);
        if let FrameScan::Final(r) = parse_frames(&buf, want_id, runtime_id) {
            return r;
        }
    }
}

// ─── negative cache ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct NegativeCache {
    #[serde(rename = "runtimeId")]
    runtime_id: String,
    until: i64,
}

/// Until when the negative-cache file content `text` suppresses RPCs to the
/// Orca instance `runtime_id` at `now` (epoch s). Pure. A record for a
/// different runtimeId (Orca restarted since) never blocks.
fn negative_cache_until_in(text: Option<&str>, runtime_id: &str, now: i64) -> Option<i64> {
    let rec: NegativeCache = serde_json::from_str(text?).ok()?;
    (rec.runtime_id == runtime_id && now < rec.until).then_some(rec.until)
}

fn now_epoch() -> i64 {
    crate::epoch::now_secs() as i64
}

/// Until when (epoch s) the negative cache suppresses RPCs to `runtime_id`,
/// or `None` when it does not.
pub fn negative_cache_until(runtime_id: &str) -> Option<i64> {
    let text = std::fs::read_to_string(crate::paths::orca_rpc_down()).ok();
    negative_cache_until_in(text.as_deref(), runtime_id, now_epoch())
}

/// Record a timed-out read against `runtime_id` (best-effort, atomic).
pub fn write_negative_cache(runtime_id: &str) {
    let rec = NegativeCache {
        runtime_id: runtime_id.to_owned(),
        until: now_epoch() + NEGATIVE_CACHE_SECS,
    };
    if let (Ok(json), Ok(_)) = (serde_json::to_vec(&rec), crate::paths::smart_dir()) {
        let _ = super::atomic_write(&crate::paths::orca_rpc_down(), &json);
    }
}

/// Drop the negative cache (after a successful call).
pub fn clear_negative_cache() {
    let _ = std::fs::remove_file(crate::paths::orca_rpc_down());
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const RID: &str = "runtime-1";
    const ID: &str = "req-1";

    fn token() -> AuthToken {
        AuthToken::new("tok-secret-value".to_owned())
    }

    #[test]
    fn encode_accounts_list_spells_out_refresh_usage_false() {
        let line = encode_request(ID, &token(), &Method::AccountsList);
        let s = String::from_utf8(line.as_bytes().to_vec()).unwrap();
        assert!(s.ends_with('\n'), "one line, newline-terminated: {s:?}");
        assert_eq!(s.matches('\n').count(), 1);
        assert!(
            s.contains(r#""params":{"refreshUsage":false}"#),
            "refreshUsage:false must be explicit: {s}"
        );
        assert!(s.contains(r#""method":"accounts.list""#));
        assert!(s.contains(r#""id":"req-1""#));
    }

    #[test]
    fn encode_select_carries_the_account_id() {
        let m = Method::select_claude("acct-1").unwrap();
        let line = encode_request(ID, &token(), &m);
        let s = String::from_utf8(line.as_bytes().to_vec()).unwrap();
        assert!(s.contains(r#""params":{"accountId":"acct-1"}"#), "{s}");
        assert!(s.contains(r#""method":"accounts.selectClaude""#));
    }

    #[test]
    fn select_refuses_an_empty_id() {
        assert_eq!(Method::select_claude(""), None);
        assert_eq!(Method::select_claude("   "), None);
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let t = token();
        assert!(!format!("{t:?}").contains("tok-secret-value"));
        let line = encode_request(ID, &t, &Method::AccountsList);
        assert!(!format!("{line:?}").contains("tok-secret-value"));
    }

    fn ok_frame(id: &str, rid: &str) -> String {
        format!(r#"{{"id":"{id}","ok":true,"result":{{"x":1}},"_meta":{{"runtimeId":"{rid}"}}}}"#)
    }

    #[test]
    fn keepalives_are_skipped_and_the_matching_id_is_final() {
        let buf = format!(
            "{{\"_keepalive\":true}}\n{{\"_keepalive\":true}}\n{}\n",
            ok_frame(ID, RID)
        );
        assert_eq!(
            parse_frames(buf.as_bytes(), ID, RID),
            FrameScan::Final(Ok(json!({"x": 1})))
        );
    }

    #[test]
    fn frames_for_other_ids_are_ignored() {
        let buf = format!("{}\n", ok_frame("someone-else", RID));
        assert_eq!(parse_frames(buf.as_bytes(), ID, RID), FrameScan::Incomplete);
    }

    #[test]
    fn runtime_id_mismatch_is_rejected() {
        let buf = format!("{}\n", ok_frame(ID, "other-runtime"));
        assert_eq!(
            parse_frames(buf.as_bytes(), ID, RID),
            FrameScan::Final(Err(RpcError::RuntimeIdMismatch))
        );
        // A missing runtimeId on a success frame is not trusted either.
        let buf = format!("{{\"id\":\"{ID}\",\"ok\":true,\"result\":1}}\n");
        assert_eq!(
            parse_frames(buf.as_bytes(), ID, RID),
            FrameScan::Final(Err(RpcError::RuntimeIdMismatch))
        );
    }

    #[test]
    fn error_frames_surface_code_and_message() {
        let buf = format!(
            "{{\"id\":\"{ID}\",\"ok\":false,\"error\":{{\"code\":\"runtime_error\",\"message\":\"Account services are not configured on this runtime\"}},\"_meta\":{{\"runtimeId\":null}}}}\n"
        );
        let FrameScan::Final(Err(e)) = parse_frames(buf.as_bytes(), ID, RID) else {
            panic!("expected an error frame");
        };
        assert!(e.is_services_starting(), "{e:?}");
        assert!(matches!(e, RpcError::Remote { ref code, .. } if code == "runtime_error"));
    }

    #[test]
    fn partial_trailing_data_waits_for_more() {
        let full = ok_frame(ID, RID);
        let (head, _) = full.split_at(full.len() / 2);
        let buf = format!("{{\"_keepalive\":true}}\n{head}");
        assert_eq!(parse_frames(buf.as_bytes(), ID, RID), FrameScan::Incomplete);
        // Complete but unterminated: still waits (the server always ends
        // frames with a newline).
        assert_eq!(
            parse_frames(full.as_bytes(), ID, RID),
            FrameScan::Incomplete
        );
    }

    #[test]
    fn oversized_response_is_capped() {
        let mut buf = b"{\"_keepalive\":true}\n".to_vec();
        buf.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
        assert_eq!(
            parse_frames(&buf, ID, RID),
            FrameScan::Final(Err(RpcError::TooLarge))
        );
    }

    #[test]
    fn non_json_frame_is_malformed() {
        assert!(matches!(
            parse_frames(b"not json\n", ID, RID),
            FrameScan::Final(Err(RpcError::Malformed(_)))
        ));
    }

    #[test]
    fn negative_cache_honours_runtime_id_and_expiry() {
        let negative_cache_blocks =
            |t: Option<&str>, rid: &str, now: i64| negative_cache_until_in(t, rid, now).is_some();
        let rec = r#"{"runtimeId":"runtime-1","until":1000}"#;
        assert!(negative_cache_blocks(Some(rec), RID, 999));
        assert!(!negative_cache_blocks(Some(rec), RID, 1000), "expired");
        assert!(
            !negative_cache_blocks(Some(rec), "runtime-2", 999),
            "Orca restarted"
        );
        assert!(!negative_cache_blocks(None, RID, 0));
        assert!(!negative_cache_blocks(Some("garbage"), RID, 0));
    }

    // ─── transport against a fake socket (never the live Orca) ─────────────

    /// Serve one connection on a temp unix socket: read the request line,
    /// hand it to `respond`, write back whatever it returns.
    #[cfg(unix)]
    fn fake_orca(
        respond: impl FnOnce(Value) -> Vec<u8> + Send + 'static,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("o.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let out = respond(req);
            let mut w = stream;
            let _ = w.write_all(&out);
        });
        (dir, sock, h)
    }

    #[cfg(unix)]
    #[test]
    fn transport_roundtrip_skips_keepalives() {
        let (_dir, sock, h) = fake_orca(|req| {
            assert_eq!(req["method"], "accounts.list");
            assert_eq!(req["params"], json!({"refreshUsage": false}));
            assert_eq!(req["authToken"], "tok-secret-value");
            let id = req["id"].as_str().unwrap().to_owned();
            format!(
                "{{\"_keepalive\":true}}\n{{\"id\":\"{id}\",\"ok\":true,\"result\":{{\"claude\":{{}}}},\"_meta\":{{\"runtimeId\":\"{RID}\"}}}}\n"
            )
            .into_bytes()
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let got = call_unix(&sock, &token(), RID, &Method::AccountsList, deadline);
        h.join().unwrap();
        assert_eq!(got, Ok(json!({"claude": {}})));
    }

    #[cfg(unix)]
    #[test]
    fn transport_times_out_on_a_silent_server() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("o.sock");
        // Accepts (via the backlog) but never answers.
        let _listener = UnixListener::bind(&sock).unwrap();
        let start = Instant::now();
        let deadline = start + std::time::Duration::from_millis(300);
        let got = call_unix(&sock, &token(), RID, &Method::AccountsList, deadline);
        assert_eq!(got, Err(RpcError::Timeout));
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn transport_reports_an_unsent_request_apart_from_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // No listener: the connect fails, so nothing was written.
        let sock = dir.path().join("dead.sock");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let got = call_unix(&sock, &token(), RID, &Method::AccountsList, deadline);
        assert!(matches!(got, Err(RpcError::NotSent(_))), "{got:?}");
        // A spent budget never writes either.
        let got = call_unix(&sock, &token(), RID, &Method::AccountsList, Instant::now());
        assert!(matches!(got, Err(RpcError::NotSent(_))), "{got:?}");
    }

    #[cfg(unix)]
    #[test]
    fn transport_reports_a_closed_connection() {
        let (_dir, sock, h) = fake_orca(|_req| b"{\"_keepalive\":true}\n".to_vec());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let got = call_unix(&sock, &token(), RID, &Method::AccountsList, deadline);
        h.join().unwrap();
        assert_eq!(got, Err(RpcError::Closed));
    }
}
