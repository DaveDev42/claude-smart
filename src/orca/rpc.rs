//! Orca's local RPC client: one newline-delimited JSON request per
//! connection over the transport Orca advertises in `orca-runtime.json`.
//!
//! Wire contract (Orca 1.4.209, M:95070-95093, M:314212-314222):
//! - `<userData>/orca-runtime.json` = `{runtimeId, pid, transports:[{kind,
//!   endpoint}], authToken, startedAt}`. The token changes on every Orca
//!   start, so the file is re-read on EVERY call ([`call`]).
//! - Transports: a unix socket `<userData>/o-<pid>-<rt>.sock` (mode 0600) on
//!   POSIX, a named pipe `\\.\pipe\orca-<pid>-<rt>` on Windows. The
//!   websocket transport is never used.
//! - Write ONE line `{"id","authToken","method","params"}\n`; read
//!   newline-delimited frames, skipping `{"_keepalive":true}`; the first
//!   frame whose `id` matches is final: `{id, ok:true, result, _meta}` or
//!   `{id, ok:false, error:{code,message}, _meta}`. A success frame whose
//!   `_meta.runtimeId` differs from the file's came from another Orca
//!   instance and is rejected.
//!
//! Methods ([`Method`]): `accounts.list` with `refreshUsage` always spelled
//! out (Orca defaults it to `true`, which starts network fetches),
//! `accounts.selectClaude` with a NON-blank id only (a null id is Orca's
//! "System default" restore; csm never sends it), `accounts.addClaudeFromConfigDir`,
//! and `accounts.removeClaude`.
//!
//! Every call runs on a worker thread and the caller waits with
//! `recv_timeout`: neither `UnixStream::connect` nor a pipe open has a
//! timeout, so a wedged Orca can never hang csm. On timeout the worker is
//! abandoned; its own socket timeouts end it soon after.
//!
//! Secrets: the `authToken` lives only in memory ([`AuthToken`]: redacting
//! `Debug`, no `Display`/`Serialize`). The encoded [`RequestLine`] embeds it,
//! redacts too, and is zeroed right after the write. Remote error messages
//! are passed through with the token stripped.
//!
//! Test guard: under `cfg(test)` a call refuses any endpoint outside the
//! system temp dir (and every named pipe not named `csm-test-*`), so no test
//! can reach the live Orca socket.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::record::{AccountRecord, ActiveIds, d3};

/// `<userData>/orca-runtime.json`.
pub use super::userdata::RUNTIME_FILE;

/// Hard cap on one RPC response (all frames together).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Cap on `orca-runtime.json` (a real one is a few hundred bytes).
const RUNTIME_FILE_CAP: u64 = 1024 * 1024;

/// Timeout for `accounts.list {refreshUsage:false}` (a memory read).
pub const LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for `accounts.selectClaude` (design section 3).
pub const SELECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for `accounts.addClaudeFromConfigDir` (Orca's own `claude auth
/// status` takes up to 20 s) and `accounts.removeClaude`.
pub const ADD_TIMEOUT: Duration = Duration::from_secs(60);

// ─── secrets ──────────────────────────────────────────────────────────────────

/// Orca's per-start RPC `authToken`. In memory only.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    pub(crate) fn new(token: String) -> Self {
        AuthToken(token)
    }

    fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the token replaced.
    fn strip_from(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_owned();
        }
        text.replace(&self.0, "<redacted>")
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl Drop for AuthToken {
    fn drop(&mut self) {
        // SAFETY: only ever overwritten with 0x00, which keeps valid UTF-8.
        super::zero(unsafe { self.0.as_bytes_mut() });
    }
}

/// An encoded request line. It embeds the token: `Debug` redacts, `Drop`
/// zeroes.
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
        super::zero(&mut self.0);
    }
}

// ─── runtime metadata ─────────────────────────────────────────────────────────

/// One transport Orca advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// `"unix"`, `"named-pipe"`, or `"websocket"`.
    pub kind: String,
    pub endpoint: String,
}

/// `orca-runtime.json`. The token redacts in `Debug`; nothing here is
/// `Serialize`.
#[derive(Debug, Clone)]
pub struct RuntimeMetadata {
    pub runtime_id: String,
    pub pid: u32,
    pub transports: Vec<Transport>,
    pub auth_token: AuthToken,
    pub started_at: Option<i64>,
}

impl RuntimeMetadata {
    /// The transport this build speaks: the unix socket on POSIX, the named
    /// pipe on Windows.
    pub fn local_transport(&self) -> Option<&Transport> {
        let want = if cfg!(windows) { "named-pipe" } else { "unix" };
        self.transports
            .iter()
            .find(|t| t.kind == want && !t.endpoint.is_empty())
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse `orca-runtime.json`. Pure. Errors are generic on purpose: the text
/// holds a secret, and a serde message can quote the offending value.
pub fn parse_runtime_metadata(text: &str) -> Result<RuntimeMetadata, &'static str> {
    let v: Value = serde_json::from_str(text).map_err(|_| "orca-runtime.json is not JSON")?;
    let runtime_id = str_field(&v, "runtimeId").ok_or("orca-runtime.json has no runtimeId")?;
    let pid = v
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .ok_or("orca-runtime.json has no valid pid")?;
    let token = str_field(&v, "authToken").ok_or("orca-runtime.json has no authToken")?;
    let transports = v
        .get("transports")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(Transport {
                        kind: str_field(t, "kind")?,
                        endpoint: str_field(t, "endpoint")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RuntimeMetadata {
        runtime_id,
        pid,
        transports,
        auth_token: AuthToken::new(token),
        started_at: v.get("startedAt").and_then(Value::as_i64),
    })
}

/// `<userData>/orca-runtime.json`: `Ok(None)` when absent.
pub fn read_runtime_metadata(user_data: &Path) -> std::io::Result<Option<RuntimeMetadata>> {
    let path = user_data.join(RUNTIME_FILE);
    let Some(text) = super::read_capped(&path, RUNTIME_FILE_CAP)? else {
        return Ok(None);
    };
    parse_runtime_metadata(&text)
        .map(Some)
        .map_err(|m| std::io::Error::new(std::io::ErrorKind::InvalidData, m))
}

// ─── methods ──────────────────────────────────────────────────────────────────

/// The RPC methods csm can encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `accounts.list {"refreshUsage":<bool>}`, always explicit.
    AccountsList { refresh_usage: bool },
    /// `accounts.selectClaude {"accountId":"<id>"}`; build with
    /// [`Method::select_claude`], which refuses a blank id.
    SelectClaude { account_id: String },
    /// `accounts.addClaudeFromConfigDir {configDir, previousLegacyCredentialsSha256?}`
    /// (host runtime); build with [`Method::add_claude_from_config_dir`].
    AddClaudeFromConfigDir {
        config_dir: String,
        previous_legacy_sha256: Option<String>,
    },
    /// `accounts.removeClaude {"accountId":"<id>"}`.
    RemoveClaude { account_id: String },
}

fn non_blank(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

impl Method {
    pub fn accounts_list(refresh_usage: bool) -> Method {
        Method::AccountsList { refresh_usage }
    }

    /// `None` for a blank id: a null or empty id is never sent.
    pub fn select_claude(account_id: &str) -> Option<Method> {
        non_blank(account_id).map(|account_id| Method::SelectClaude { account_id })
    }

    /// `None` for a blank dir or a digest that is not 64 lowercase hex chars
    /// (Orca's schema rejects it).
    pub fn add_claude_from_config_dir(
        config_dir: &str,
        previous_legacy_sha256: Option<&str>,
    ) -> Option<Method> {
        let config_dir = non_blank(config_dir)?;
        let previous_legacy_sha256 = match previous_legacy_sha256 {
            None => None,
            Some(d)
                if d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Some(d.to_owned())
            }
            Some(_) => return None,
        };
        Some(Method::AddClaudeFromConfigDir {
            config_dir,
            previous_legacy_sha256,
        })
    }

    pub fn remove_claude(account_id: &str) -> Option<Method> {
        non_blank(account_id).map(|account_id| Method::RemoveClaude { account_id })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Method::AccountsList { .. } => "accounts.list",
            Method::SelectClaude { .. } => "accounts.selectClaude",
            Method::AddClaudeFromConfigDir { .. } => "accounts.addClaudeFromConfigDir",
            Method::RemoveClaude { .. } => "accounts.removeClaude",
        }
    }

    fn params(&self) -> Value {
        match self {
            Method::AccountsList { refresh_usage } => json!({ "refreshUsage": refresh_usage }),
            Method::SelectClaude { account_id } | Method::RemoveClaude { account_id } => {
                json!({ "accountId": account_id })
            }
            Method::AddClaudeFromConfigDir {
                config_dir,
                previous_legacy_sha256,
            } => {
                let mut m = Map::new();
                m.insert("configDir".into(), config_dir.clone().into());
                if let Some(d) = previous_legacy_sha256 {
                    m.insert("previousLegacyCredentialsSha256".into(), d.clone().into());
                }
                Value::Object(m)
            }
        }
    }
}

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

// ─── errors ───────────────────────────────────────────────────────────────────

/// Why an RPC produced no usable result. Never carries the token.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    #[error("Orca is not running (no orca-runtime.json)")]
    NoRuntime,
    #[error("cannot read orca-runtime.json: {0}")]
    Metadata(String),
    #[error("Orca advertises no transport this build can use")]
    Unsupported,
    #[error("refused: {0}")]
    Refused(String),
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
    /// The request was never written (connect failed or no time left): Orca
    /// cannot have acted on it.
    #[error("the request never reached Orca: {0}")]
    NotSent(String),
}

impl RpcError {
    /// Orca's global switch lock is held (M:39351): retry with backoff.
    pub fn is_switch_in_progress(&self) -> bool {
        matches!(self, RpcError::Remote { message, .. }
            if message.contains("A Claude account switch is already in progress"))
    }

    /// Could Orca have acted on the request?
    pub fn maybe_delivered(&self) -> bool {
        matches!(
            self,
            RpcError::Timeout | RpcError::Io(_) | RpcError::Closed | RpcError::TooLarge
        )
    }
}

// ─── framing ──────────────────────────────────────────────────────────────────

/// Result of scanning the bytes received so far.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameScan {
    /// No final frame yet (only keepalives, foreign ids, or a partial line).
    Incomplete,
    /// The final frame for the request.
    Final(Result<Value, RpcError>),
}

/// Scan `buf` for the final frame answering `want_id`. Pure. Only complete
/// (`\n`-terminated) lines count. `token` is stripped from remote messages.
pub fn parse_frames(buf: &[u8], want_id: &str, runtime_id: &str, token: &AuthToken) -> FrameScan {
    let mut rest = buf;
    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let line = rest[..nl].trim_ascii();
        rest = &rest[nl + 1..];
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            return FrameScan::Final(Err(RpcError::Malformed("a frame is not JSON".into())));
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
            Some(true) if frame_rid == Some(runtime_id) => {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
            Some(true) => Err(RpcError::RuntimeIdMismatch),
            // Error frames may carry a null runtimeId; only a different one
            // is a foreign instance.
            Some(false) if frame_rid.is_some_and(|r| r != runtime_id) => {
                Err(RpcError::RuntimeIdMismatch)
            }
            Some(false) => {
                let field = |k: &str| {
                    v.get("error")
                        .and_then(|e| e.get(k))
                        .and_then(Value::as_str)
                        .map(|s| token.strip_from(s))
                        .unwrap_or_default()
                };
                Err(RpcError::Remote {
                    code: field("code"),
                    message: field("message"),
                })
            }
            None => Err(RpcError::Malformed("the final frame has no ok flag".into())),
        });
    }
    if buf.len() > MAX_RESPONSE_BYTES {
        FrameScan::Final(Err(RpcError::TooLarge))
    } else {
        FrameScan::Incomplete
    }
}

// ─── typed results ────────────────────────────────────────────────────────────

/// One usage window (`G4`): percent used, window length, reset time.
#[derive(Debug, Clone, PartialEq)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Epoch ms, when Orca knows it.
    pub resets_at: Option<i64>,
}

/// One provider's usage (`K4`'s result shape).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderLimits {
    pub session: Option<RateWindow>,
    pub weekly: Option<RateWindow>,
    pub fable_weekly: Option<RateWindow>,
    pub updated_at: Option<i64>,
    pub status: Option<String>,
}

/// One entry of `rateLimits.inactiveClaudeAccounts`.
#[derive(Debug, Clone, PartialEq)]
pub struct InactiveUsage {
    pub account_id: String,
    pub limits: Option<ProviderLimits>,
    pub updated_at: Option<i64>,
    pub is_fetching: bool,
}

/// The `claude` part of `accounts.list` (also `selectClaude`'s result).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSnapshot {
    /// Orca's i9i view (no `managedAuthPath`), newest `updatedAt` first.
    pub accounts: Vec<AccountRecord>,
    pub active_account_id: Option<String>,
    pub active_by_runtime: ActiveIds,
}

/// `accounts.list`'s result.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountsSnapshot {
    pub claude: ClaudeSnapshot,
    /// `rateLimits.claude`, the active account's usage.
    pub active_usage: Option<ProviderLimits>,
    pub inactive_usage: Vec<InactiveUsage>,
}

fn num_i64(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn parse_window(v: Option<&Value>) -> Option<RateWindow> {
    let v = v?;
    Some(RateWindow {
        used_percent: v.get("usedPercent")?.as_f64()?,
        window_minutes: num_i64(v.get("windowMinutes")),
        resets_at: num_i64(v.get("resetsAt")),
    })
}

/// Tolerant: a malformed usage block reads as `None`, never an error.
fn parse_limits(v: Option<&Value>) -> Option<ProviderLimits> {
    let v = v.filter(|v| v.is_object())?;
    Some(ProviderLimits {
        session: parse_window(v.get("session")),
        weekly: parse_window(v.get("weekly")),
        fable_weekly: parse_window(v.get("fableWeekly")),
        updated_at: num_i64(v.get("updatedAt")),
        status: v.get("status").and_then(Value::as_str).map(str::to_owned),
    })
}

/// Parse the `claude` snapshot. Pure. Account and active-id shapes are
/// strict (a later switch acts on them); nothing is quoted in errors.
pub fn parse_claude_snapshot(v: &Value) -> Result<ClaudeSnapshot, RpcError> {
    let bad = |m: String| RpcError::Malformed(m);
    let obj = v
        .as_object()
        .ok_or_else(|| bad("the claude snapshot is not an object".into()))?;
    let accounts = match obj.get("accounts") {
        Some(Value::Array(a)) => a
            .iter()
            .map(AccountRecord::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(bad)?,
        _ => return Err(bad("the claude snapshot has no accounts array".into())),
    };
    // d3 over a synthetic settings object: the same null/absent rules.
    let mut settings = Map::new();
    if let Some(id) = obj.get("activeAccountId") {
        settings.insert("activeClaudeManagedAccountId".into(), id.clone());
    }
    if let Some(by) = obj.get("activeAccountIdsByRuntime") {
        settings.insert("activeClaudeManagedAccountIdsByRuntime".into(), by.clone());
    }
    let active_by_runtime = d3(&settings).map_err(bad)?;
    let active_account_id = match obj.get("activeAccountId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(bad("activeAccountId has an unexpected type".into())),
    };
    Ok(ClaudeSnapshot {
        accounts,
        active_account_id,
        active_by_runtime,
    })
}

/// Parse `accounts.list`'s result. Pure.
pub fn parse_accounts_snapshot(v: &Value) -> Result<AccountsSnapshot, RpcError> {
    let claude = parse_claude_snapshot(
        v.get("claude")
            .ok_or_else(|| RpcError::Malformed("the result has no claude snapshot".into()))?,
    )?;
    let rl = v.get("rateLimits");
    let inactive_usage = rl
        .and_then(|r| r.get("inactiveClaudeAccounts"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(InactiveUsage {
                        account_id: str_field(e, "accountId")?,
                        limits: parse_limits(e.get("rateLimits")),
                        updated_at: num_i64(e.get("updatedAt")),
                        is_fetching: e.get("isFetching").and_then(Value::as_bool) == Some(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AccountsSnapshot {
        claude,
        active_usage: parse_limits(rl.and_then(|r| r.get("claude"))),
        inactive_usage,
    })
}

// ─── transport ────────────────────────────────────────────────────────────────

/// Call `method` on the Orca whose runtime file lives in `user_data`,
/// re-reading that file first. Gives up after `timeout`.
pub fn call(user_data: &Path, method: &Method, timeout: Duration) -> Result<Value, RpcError> {
    crate::usage::reach::note("orca-rpc");
    let deadline = Instant::now() + timeout;
    let meta = read_runtime_metadata(user_data)
        .map_err(|e| RpcError::Metadata(e.to_string()))?
        .ok_or(RpcError::NoRuntime)?;
    let transport = meta.local_transport().ok_or(RpcError::Unsupported)?;
    call_endpoint(
        transport,
        &meta.auth_token,
        &meta.runtime_id,
        method,
        deadline,
    )
}

/// `accounts.list` through [`call`], typed.
pub fn accounts_list(
    user_data: &Path,
    refresh_usage: bool,
    timeout: Duration,
) -> Result<AccountsSnapshot, RpcError> {
    let v = call(user_data, &Method::accounts_list(refresh_usage), timeout)?;
    parse_accounts_snapshot(&v)
}

/// `accounts.selectClaude` through [`call`], typed. Refuses a blank id.
pub fn select_claude(
    user_data: &Path,
    account_id: &str,
    timeout: Duration,
) -> Result<ClaudeSnapshot, RpcError> {
    let m = Method::select_claude(account_id)
        .ok_or_else(|| RpcError::Refused("a blank account id is never selected".into()))?;
    let v = call(user_data, &m, timeout)?;
    parse_claude_snapshot(&v)
}

/// `accounts.addClaudeFromConfigDir` through [`call`]. Orca runs `claude
/// auth status` (20 s) inside it, so give it [`ADD_TIMEOUT`].
pub fn add_claude_from_config_dir(
    user_data: &Path,
    config_dir: &str,
    previous_legacy_sha256: Option<&str>,
    timeout: Duration,
) -> Result<Value, RpcError> {
    let m = Method::add_claude_from_config_dir(config_dir, previous_legacy_sha256)
        .ok_or_else(|| RpcError::Refused("a blank dir or a malformed digest".into()))?;
    call(user_data, &m, timeout)
}

/// `accounts.removeClaude` through [`call`].
pub fn remove_claude(
    user_data: &Path,
    account_id: &str,
    timeout: Duration,
) -> Result<Value, RpcError> {
    let m = Method::remove_claude(account_id)
        .ok_or_else(|| RpcError::Refused("a blank account id is never removed".into()))?;
    call(user_data, &m, timeout)
}

/// Under test only endpoints in the temp dir (never the live Orca's).
#[cfg(test)]
fn guard_endpoint(t: &Transport) -> Result<(), RpcError> {
    let ok = match t.kind.as_str() {
        "unix" => {
            let p = Path::new(&t.endpoint);
            let tmp = std::env::temp_dir();
            let under_tmp =
                p.starts_with(&tmp) || std::fs::canonicalize(&tmp).is_ok_and(|c| p.starts_with(c));
            let real_orca = dirs::home_dir().map(|h| {
                super::userdata::default_user_data(super::HostOs::current(), &h, None, None)
            });
            under_tmp && !real_orca.is_some_and(|r| p.starts_with(r))
        }
        "named-pipe" => t.endpoint.starts_with(r"\\.\pipe\csm-test-"),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(RpcError::Refused(
            "cfg(test): endpoint outside the temp dir".into(),
        ))
    }
}

#[cfg(not(test))]
fn guard_endpoint(_t: &Transport) -> Result<(), RpcError> {
    Ok(())
}

/// The worker-thread transport behind [`call`].
pub(crate) fn call_endpoint(
    transport: &Transport,
    token: &AuthToken,
    runtime_id: &str,
    method: &Method,
    deadline: Instant,
) -> Result<Value, RpcError> {
    use std::sync::mpsc;

    guard_endpoint(transport)?;
    let want = if cfg!(windows) { "named-pipe" } else { "unix" };
    if transport.kind != want {
        return Err(RpcError::Unsupported);
    }
    if deadline <= Instant::now() {
        return Err(RpcError::NotSent("no time left".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let line = encode_request(&id, token, method);
    let endpoint = PathBuf::from(&transport.endpoint);
    let rid = runtime_id.to_owned();
    let token = token.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = exchange(&endpoint, line, &id, &rid, &token, deadline);
        let _ = tx.send(r);
    });
    let wait = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(wait).unwrap_or(Err(RpcError::Timeout))
}

/// Read frames from `stream` until the final one (worker thread).
fn read_final(
    stream: &mut impl std::io::Read,
    want_id: &str,
    runtime_id: &str,
    token: &AuthToken,
    mut before_read: impl FnMut() -> Result<(), RpcError>,
) -> Result<Value, RpcError> {
    use std::io::ErrorKind;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        before_read()?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(RpcError::Timeout);
            }
            Err(e) => return Err(RpcError::Io(e.kind().to_string())),
        };
        if n == 0 {
            return match parse_frames(&buf, want_id, runtime_id, token) {
                FrameScan::Final(r) => r,
                FrameScan::Incomplete => Err(RpcError::Closed),
            };
        }
        buf.extend_from_slice(&chunk[..n]);
        if let FrameScan::Final(r) = parse_frames(&buf, want_id, runtime_id, token) {
            return r;
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, RpcError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(RpcError::Timeout)
}

/// One exchange on a fresh unix-socket connection.
#[cfg(unix)]
fn exchange(
    endpoint: &Path,
    line: RequestLine,
    want_id: &str,
    runtime_id: &str,
    token: &AuthToken,
    deadline: Instant,
) -> Result<Value, RpcError> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(endpoint)
        .map_err(|e| RpcError::NotSent(format!("connect failed: {}", e.kind())))?;
    // Best effort: macOS refuses the setsockopt once the peer has closed.
    // The caller's recv_timeout is what bounds the wait.
    let _ = stream.set_write_timeout(Some(
        remaining(deadline).map_err(|_| RpcError::NotSent("no time left".into()))?,
    ));
    stream
        .write_all(line.as_bytes())
        .map_err(|e| RpcError::Io(e.kind().to_string()))?;
    drop(line);
    let reader = stream
        .try_clone()
        .map_err(|e| RpcError::Io(e.kind().to_string()))?;
    let mut reader = reader;
    read_final(&mut reader, want_id, runtime_id, token, || {
        let _ = stream.set_read_timeout(Some(remaining(deadline)?));
        Ok(())
    })
}

/// One exchange on a fresh named-pipe connection. Node's `net` server on
/// Windows speaks a byte-mode pipe, which `std::fs::File` reads and writes;
/// a blocked read is bounded by the caller's `recv_timeout`.
#[cfg(windows)]
fn exchange(
    endpoint: &Path,
    line: RequestLine,
    want_id: &str,
    runtime_id: &str,
    token: &AuthToken,
    deadline: Instant,
) -> Result<Value, RpcError> {
    use std::io::Write;

    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(endpoint)
        .map_err(|e| RpcError::NotSent(format!("pipe open failed: {}", e.kind())))?;
    remaining(deadline).map_err(|_| RpcError::NotSent("no time left".into()))?;
    pipe.write_all(line.as_bytes())
        .map_err(|e| RpcError::Io(e.kind().to_string()))?;
    drop(line);
    read_final(&mut pipe, want_id, runtime_id, token, || {
        remaining(deadline).map(|_| ())
    })
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::orca::testsupport::FakeOrca;

    const RID: &str = "runtime-1";
    const ID: &str = "req-1";

    fn token() -> AuthToken {
        AuthToken::new("tok-secret-value".to_owned())
    }

    fn line_of(m: &Method) -> String {
        String::from_utf8(encode_request(ID, &token(), m).as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn encode_accounts_list_spells_out_refresh_usage() {
        let s = line_of(&Method::accounts_list(false));
        assert!(s.ends_with('\n') && s.matches('\n').count() == 1, "{s:?}");
        assert!(s.contains(r#""params":{"refreshUsage":false}"#), "{s}");
        assert!(s.contains(r#""method":"accounts.list""#));
        assert!(line_of(&Method::accounts_list(true)).contains(r#"{"refreshUsage":true}"#));
    }

    #[test]
    fn encode_the_account_write_methods() {
        let s = line_of(&Method::select_claude(" acct-1 ").unwrap());
        assert!(
            s.contains(r#""method":"accounts.selectClaude","params":{"accountId":"acct-1"}"#),
            "{s}"
        );
        let s = line_of(&Method::remove_claude("acct-2").unwrap());
        assert!(
            s.contains(r#""method":"accounts.removeClaude","params":{"accountId":"acct-2"}"#),
            "{s}"
        );
        let sha = "a".repeat(64);
        let s =
            line_of(&Method::add_claude_from_config_dir("/Users/example/tmp", Some(&sha)).unwrap());
        assert!(s.contains(&format!(
            r#""params":{{"configDir":"/Users/example/tmp","previousLegacyCredentialsSha256":"{sha}"}}"#
        )), "{s}");
        let s = line_of(&Method::add_claude_from_config_dir("/d", None).unwrap());
        assert!(s.contains(r#""params":{"configDir":"/d"}"#), "{s}");
    }

    #[test]
    fn blank_ids_and_bad_digests_cannot_be_encoded() {
        assert_eq!(Method::select_claude(""), None);
        assert_eq!(Method::select_claude("   "), None);
        assert_eq!(Method::remove_claude(" "), None);
        assert_eq!(Method::add_claude_from_config_dir(" ", None), None);
        assert_eq!(Method::add_claude_from_config_dir("/d", Some("ABC")), None);
        assert_eq!(
            Method::add_claude_from_config_dir("/d", Some(&"A".repeat(64))),
            None
        );
    }

    #[test]
    fn secrets_are_redacted() {
        let t = token();
        assert!(!format!("{t:?}").contains("tok-secret"));
        let line = encode_request(ID, &t, &Method::accounts_list(false));
        assert!(!format!("{line:?}").contains("tok-secret"));
        let m = parse_runtime_metadata(
            r#"{"runtimeId":"r","pid":1,"transports":[],"authToken":"tok-secret-value"}"#,
        )
        .unwrap();
        assert!(!format!("{m:?}").contains("tok-secret"));
        // A remote message that echoes the token is stripped.
        let buf = format!(
            "{{\"id\":\"{ID}\",\"ok\":false,\"error\":{{\"code\":\"unauthorized\",\"message\":\"bad tok-secret-value\"}},\"_meta\":{{\"runtimeId\":\"{RID}\"}}}}\n"
        );
        let FrameScan::Final(Err(e)) = parse_frames(buf.as_bytes(), ID, RID, &t) else {
            panic!()
        };
        assert!(!e.to_string().contains("tok-secret"), "{e}");
        // Metadata errors never quote the file.
        let e =
            parse_runtime_metadata(r#"{"runtimeId":"r","pid":"tok-secret-value"}"#).unwrap_err();
        assert!(!e.contains("tok-secret"));
    }

    #[test]
    fn runtime_metadata_parsing() {
        let m = parse_runtime_metadata(
            r#"{"runtimeId":"r","pid":42,"transports":[{"kind":"websocket","endpoint":"ws://x"},{"kind":"unix","endpoint":"/tmp/o.sock"},{"kind":"named-pipe","endpoint":"\\\\.\\pipe\\orca-42-r"}],"authToken":"t","startedAt":5}"#,
        )
        .unwrap();
        assert_eq!((m.pid, m.started_at), (42, Some(5)));
        let t = m.local_transport().unwrap();
        if cfg!(windows) {
            assert_eq!(t.kind, "named-pipe");
        } else {
            assert_eq!(t.endpoint, "/tmp/o.sock");
        }
        for bad in [
            "{",
            r#"{"pid":1,"authToken":"t"}"#,
            r#"{"runtimeId":"r","pid":-1,"authToken":"t"}"#,
            r#"{"runtimeId":"r","pid":1,"authToken":""}"#,
        ] {
            assert!(parse_runtime_metadata(bad).is_err(), "{bad}");
        }
    }

    fn ok_frame(id: &str, rid: &str) -> String {
        format!(r#"{{"id":"{id}","ok":true,"result":{{"x":1}},"_meta":{{"runtimeId":"{rid}"}}}}"#)
    }

    #[test]
    fn framing() {
        let t = token();
        let buf = format!("{{\"_keepalive\":true}}\n\n{}\n", ok_frame(ID, RID));
        assert_eq!(
            parse_frames(buf.as_bytes(), ID, RID, &t),
            FrameScan::Final(Ok(json!({"x": 1})))
        );
        let other = format!("{}\n", ok_frame("someone-else", RID));
        assert_eq!(
            parse_frames(other.as_bytes(), ID, RID, &t),
            FrameScan::Incomplete
        );
        let foreign = format!("{}\n", ok_frame(ID, "other-runtime"));
        assert_eq!(
            parse_frames(foreign.as_bytes(), ID, RID, &t),
            FrameScan::Final(Err(RpcError::RuntimeIdMismatch))
        );
        let no_meta = format!("{{\"id\":\"{ID}\",\"ok\":true,\"result\":1}}\n");
        assert_eq!(
            parse_frames(no_meta.as_bytes(), ID, RID, &t),
            FrameScan::Final(Err(RpcError::RuntimeIdMismatch))
        );
        let full = ok_frame(ID, RID);
        assert_eq!(
            parse_frames(full.as_bytes(), ID, RID, &t),
            FrameScan::Incomplete
        );
        assert!(matches!(
            parse_frames(b"not json\n", ID, RID, &t),
            FrameScan::Final(Err(RpcError::Malformed(_)))
        ));
        let mut big = b"{\"_keepalive\":true}\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
        assert_eq!(
            parse_frames(&big, ID, RID, &t),
            FrameScan::Final(Err(RpcError::TooLarge))
        );
    }

    #[test]
    fn remote_errors_classify() {
        let t = token();
        let err = |msg: &str| {
            let buf = format!(
                "{{\"id\":\"{ID}\",\"ok\":false,\"error\":{{\"code\":\"runtime_error\",\"message\":\"{msg}\"}},\"_meta\":{{\"runtimeId\":null}}}}\n"
            );
            match parse_frames(buf.as_bytes(), ID, RID, &t) {
                FrameScan::Final(Err(e)) => e,
                other => panic!("{other:?}"),
            }
        };
        let e = err("A Claude account switch is already in progress.");
        assert!(e.is_switch_in_progress() && !e.maybe_delivered());
        assert!(RpcError::Timeout.maybe_delivered());
        assert!(!RpcError::NotSent("x".into()).maybe_delivered());
    }

    fn list_result() -> Value {
        json!({
            "claude": {
                "accounts": [
                    {"id": "id-b", "email": "bob@example.com", "managedAuthRuntime": "host",
                     "wslDistro": null, "authMethod": "subscription-oauth",
                     "organizationUuid": "org-acme", "organizationName": "Acme",
                     "createdAt": 2, "updatedAt": 3, "lastAuthenticatedAt": 3},
                    {"id": "id-a", "email": "alice@example.com", "managedAuthRuntime": "host",
                     "wslDistro": null, "authMethod": "subscription-oauth",
                     "organizationUuid": null, "organizationName": null,
                     "createdAt": 1, "updatedAt": 1, "lastAuthenticatedAt": 1}
                ],
                "activeAccountId": "id-a",
                "activeAccountIdsByRuntime": {"host": "id-a", "wsl": {}}
            },
            "codex": {"accounts": []},
            "rateLimits": {
                "claude": {"provider": "claude",
                           "session": {"usedPercent": 12.5, "windowMinutes": 300, "resetsAt": 1700000000000_i64, "resetDescription": "x"},
                           "weekly": null, "fableWeekly": null, "updatedAt": 5, "error": null, "status": "ok"},
                "claudeTarget": null,
                "inactiveClaudeAccounts": [
                    {"accountId": "id-b", "rateLimits": {"session": {"usedPercent": 99}, "weekly": {"usedPercent": 40, "windowMinutes": 10080}, "updatedAt": 7, "status": "ok"}, "updatedAt": 7, "isFetching": false},
                    {"accountId": "id-c", "rateLimits": null, "updatedAt": 0, "isFetching": true}
                ]
            }
        })
    }

    #[test]
    fn accounts_list_schema() {
        let s = parse_accounts_snapshot(&list_result()).unwrap();
        assert_eq!(s.claude.accounts.len(), 2);
        assert_eq!(
            s.claude.accounts[0].organization_name.as_deref(),
            Some("Acme")
        );
        assert!(s.claude.accounts[0].managed_auth_path.is_none());
        assert_eq!(s.claude.active_account_id.as_deref(), Some("id-a"));
        assert_eq!(s.claude.active_by_runtime.host.as_deref(), Some("id-a"));
        let a = s.active_usage.unwrap();
        assert_eq!(a.session.unwrap().used_percent, 12.5);
        assert!(a.weekly.is_none());
        assert_eq!(s.inactive_usage.len(), 2);
        assert_eq!(
            s.inactive_usage[0]
                .limits
                .as_ref()
                .unwrap()
                .weekly
                .as_ref()
                .unwrap()
                .window_minutes,
            Some(10080)
        );
        assert!(s.inactive_usage[1].limits.is_none() && s.inactive_usage[1].is_fetching);

        assert!(parse_accounts_snapshot(&json!({})).is_err());
        assert!(parse_claude_snapshot(&json!({"accounts": [{"email": "x"}]})).is_err());
        assert!(parse_claude_snapshot(&json!({"accounts": [], "activeAccountId": 3})).is_err());
        let min = parse_claude_snapshot(&json!({"accounts": []})).unwrap();
        assert!(min.active_account_id.is_none());
    }

    #[test]
    fn guard_refuses_endpoints_outside_the_temp_dir() {
        let t = token();
        let deadline = Instant::now() + Duration::from_secs(1);
        let m = Method::accounts_list(false);
        let real = dirs::home_dir().map(|h| {
            crate::orca::userdata::default_user_data(crate::orca::HostOs::current(), &h, None, None)
                .join("o-1-abcd.sock")
        });
        let mut endpoints = vec![
            "/Users/example/Library/Application Support/orca/o-1-abcd.sock".to_owned(),
            r"\\.\pipe\orca-1-abcd".to_owned(),
        ];
        endpoints.extend(real.map(|p| p.to_string_lossy().into_owned()));
        for ep in endpoints {
            for kind in ["unix", "named-pipe", "websocket"] {
                let tr = Transport {
                    kind: kind.into(),
                    endpoint: ep.clone(),
                };
                let r = call_endpoint(&tr, &t, RID, &m, deadline);
                assert!(matches!(r, Err(RpcError::Refused(_))), "{kind} {ep}: {r:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn call_rereads_the_runtime_file_and_returns_typed_results() {
        let result = list_result();
        let orca = FakeOrca::start(move |_req| {
            vec![
                r#"{"_keepalive":true}"#.to_owned(),
                FakeOrca::ok(result.clone()),
            ]
        });
        let s = accounts_list(orca.user_data(), false, Duration::from_secs(5)).unwrap();
        assert_eq!(s.claude.accounts.len(), 2);
        let reqs = orca.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["method"], "accounts.list");
        assert_eq!(reqs[0]["params"], json!({"refreshUsage": false}));
        assert_eq!(reqs[0]["authToken"], orca.token());

        // Orca restarts: new token and runtime id. The next call uses them.
        orca.restart();
        accounts_list(orca.user_data(), true, Duration::from_secs(5)).unwrap();
        let reqs = orca.requests();
        assert_eq!(reqs[1]["authToken"], orca.token());
        assert_ne!(reqs[0]["authToken"], reqs[1]["authToken"]);
        assert_eq!(reqs[1]["params"], json!({"refreshUsage": true}));
    }

    #[cfg(unix)]
    #[test]
    fn select_claude_round_trip_and_blank_refusal() {
        let orca = FakeOrca::start(|req| {
            let id = req["params"]["accountId"].as_str().unwrap_or("").to_owned();
            vec![FakeOrca::ok(json!({
                "accounts": [{"id": id, "email": "bob@example.com"}],
                "activeAccountId": id,
                "activeAccountIdsByRuntime": {"host": id, "wsl": {}}
            }))]
        });
        let s = select_claude(orca.user_data(), "id-b", Duration::from_secs(5)).unwrap();
        assert_eq!(s.active_account_id.as_deref(), Some("id-b"));
        let e = select_claude(orca.user_data(), " ", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(e, RpcError::Refused(_)));
        assert_eq!(
            orca.requests().len(),
            1,
            "the blank select never reached Orca"
        );
    }

    #[cfg(unix)]
    #[test]
    fn transport_failures_are_classified() {
        // No runtime file.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            call(
                dir.path(),
                &Method::accounts_list(false),
                Duration::from_secs(1)
            ),
            Err(RpcError::NoRuntime)
        );

        // A silent server times out within the budget.
        let orca = FakeOrca::start(|_| vec![]).silent();
        let start = Instant::now();
        let r = call(
            orca.user_data(),
            &Method::accounts_list(false),
            Duration::from_millis(300),
        );
        assert_eq!(r, Err(RpcError::Timeout));
        assert!(start.elapsed() < Duration::from_secs(3));

        // A server that closes after a keepalive.
        let orca = FakeOrca::start(|_| vec![r#"{"_keepalive":true}"#.to_owned()]);
        let r = call(
            orca.user_data(),
            &Method::accounts_list(false),
            Duration::from_secs(5),
        );
        assert_eq!(r, Err(RpcError::Closed));

        // A dead socket: never sent.
        let orca = FakeOrca::start(|_| vec![]);
        orca.stop();
        let r = call(
            orca.user_data(),
            &Method::accounts_list(false),
            Duration::from_secs(2),
        );
        assert!(matches!(r, Err(RpcError::NotSent(_))), "{r:?}");
    }
}
