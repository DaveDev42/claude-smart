//! Test fixtures for the Orca module: a fake `security` binary, a fake Orca
//! RPC server, a fake process table, and stash builders. Everything lives in
//! temp dirs; nothing here touches the real Keychain, userData or socket.

// The fake server and fake `security` are unix-only; off unix the helpers
// they share are unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::keychain::{FakeRunner, hex_lower, set_fake_security, set_test_timeout};
use super::stash::{CREDENTIALS_FILE, MARKER_FILE, OAUTH_ACCOUNT_FILE, default_auth_dir};
use crate::platform::proc::ProcInfo;

// ─── fake security ────────────────────────────────────────────────────────────

/// A fake `/usr/bin/security` keeping items as files in a temp dir. Supports
/// `find-generic-password -s -a [-w]`, `add-generic-password [-U] -a -s
/// (-X <hex> | -w <value>)`, `delete-generic-password -s -a`, and `-i`
/// (commands read from stdin, one per line, double-quoted tokens).
/// Installing it sets this thread's fake; dropping it clears it.
pub(crate) struct FakeSecurity {
    dir: tempfile::TempDir,
}

/// The fake's Perl source. It runs as `/usr/bin/perl -e <this> -- <root>
/// <security args…>`: nothing is written to disk to be executed, so no test
/// creates an executable (macOS inspects every new executable on its first
/// run, and hundreds of them once stalled a whole machine).
const FAKE_SECURITY: &str = r#"
use strict;
use warnings;
my $root = shift @ARGV;
sub hexs { return unpack('H*', $_[0]); }
sub item { return "$root/items/" . hexs($_[0]) . '.' . hexs($_[1]); }
sub opts {
    my ($valued, @a) = @_;
    my %o;
    while (@a) {
        my $k = shift @a;
        if ($valued->{$k}) { $o{$k} = shift @a; } else { $o{$k} = 1; }
    }
    return \%o;
}
sub notfound {
    print STDERR "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n";
    return 44;
}
sub run {
    my ($cmd, @a) = @_;
    $cmd = '' unless defined $cmd;
    if ($cmd eq 'find-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1}, @a);
        if (-e "$root/fail-find-" . hexs($o->{'-s'})) {
            print STDERR "security: fake access failure\n";
            return 1;
        }
        my $p = item($o->{'-s'}, $o->{'-a'});
        return notfound() unless -e $p;
        # Without -w the real tool prints attributes only, never the secret.
        unless ($o->{'-w'}) {
            print "keychain: \"fake\"\nclass: \"genp\"\n";
            return 0;
        }
        open(my $f, '<:raw', $p) or return 1;
        local $/;
        my $v = <$f>;
        close $f;
        print $v, "\n";
        return 0;
    }
    if ($cmd eq 'add-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1, '-X' => 1, '-w' => 1}, @a);
        if (-e "$root/fail-add-" . hexs($o->{'-s'})) {
            print STDERR "security: fake write failure\n";
            return 1;
        }
        return 0 if -e "$root/drop-add-" . hexs($o->{'-s'});
        my $p = item($o->{'-s'}, $o->{'-a'});
        if (-e $p && !$o->{'-U'}) {
            print STDERR "security: SecKeychainItemCreateFromContent: The specified item already exists in the keychain.\n";
            return 45;
        }
        my $v = exists $o->{'-X'} ? pack('H*', $o->{'-X'}) : $o->{'-w'};
        open(my $f, '>:raw', $p) or return 1;
        print $f $v;
        close $f;
        return 0;
    }
    if ($cmd eq 'delete-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1}, @a);
        my $p = item($o->{'-s'}, $o->{'-a'});
        return notfound() unless -e $p;
        unlink $p;
        return 0;
    }
    print STDERR "fake security: unsupported command\n";
    return 2;
}
open(my $log, '>>', "$root/calls") or die;
print $log (defined $ARGV[0] ? $ARGV[0] : ''), "\n";
close $log;
open(my $alog, '>>', "$root/argv") or die;
print $alog join(' ', @ARGV), "\n";
close $alog;
if (@ARGV && $ARGV[0] eq '-i') {
    my $rc = 0;
    while (my $line = <STDIN>) {
        chomp $line;
        next if $line =~ /^\s*$/;
        my @t;
        while ($line =~ /\s*(?:"((?:[^"\\]|\\.)*)"|(\S+))/g) {
            push @t, defined $1 ? $1 : $2;
        }
        $rc = run(@t);
    }
    exit $rc;
}
exit run(@ARGV);
"#;

/// The interpreter the fake runs under. Present on macOS and on the Linux
/// CI images; an existing system binary, never one a test writes.
pub(crate) const FAKE_INTERPRETER: &str = "/usr/bin/perl";

/// How long a fake `security` call may take in tests. Perl's start-up on a
/// loaded host is the only slow part; production keeps Orca's 3000 ms.
const FAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl FakeSecurity {
    pub(crate) fn install() -> FakeSecurity {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("items")).unwrap();
        set_fake_security(Some(FakeRunner {
            program: PathBuf::from(FAKE_INTERPRETER),
            lead: vec![
                "-e".into(),
                FAKE_SECURITY.into(),
                "--".into(),
                dir.path().as_os_str().to_owned(),
            ],
        }));
        set_test_timeout(Some(FAKE_TIMEOUT));
        FakeSecurity { dir }
    }

    /// The temp dir holding the fake's items and call log.
    pub(crate) fn root(&self) -> &Path {
        self.dir.path()
    }

    fn item(&self, svc: &str, acct: &str) -> PathBuf {
        self.dir.path().join("items").join(format!(
            "{}.{}",
            hex_lower(svc.as_bytes()),
            hex_lower(acct.as_bytes())
        ))
    }

    pub(crate) fn put(&self, svc: &str, acct: &str, bytes: &[u8]) {
        std::fs::write(self.item(svc, acct), bytes).unwrap();
    }

    pub(crate) fn get(&self, svc: &str, acct: &str) -> Option<Vec<u8>> {
        std::fs::read(self.item(svc, acct)).ok()
    }

    fn marker(&self, kind: &str, svc: &str) -> PathBuf {
        self.dir
            .path()
            .join(format!("{kind}-{}", hex_lower(svc.as_bytes())))
    }

    /// Make every add to `svc` fail (exit 1), or stop failing.
    pub(crate) fn fail_add(&self, svc: &str, on: bool) {
        self.set_marker("fail-add", svc, on);
    }

    /// Make every add to `svc` exit 0 without storing (a silent failure).
    pub(crate) fn drop_add(&self, svc: &str, on: bool) {
        self.set_marker("drop-add", svc, on);
    }

    /// Make every find on `svc` fail with an access error.
    pub(crate) fn fail_find(&self, svc: &str, on: bool) {
        self.set_marker("fail-find", svc, on);
    }

    fn set_marker(&self, kind: &str, svc: &str, on: bool) {
        let m = self.marker(kind, svc);
        if on {
            std::fs::write(m, b"").unwrap();
        } else {
            let _ = std::fs::remove_file(m);
        }
    }

    /// Every stored item as (service, account).
    pub(crate) fn items(&self) -> Vec<(String, String)> {
        let unhex = |h: &str| -> String {
            let bytes: Vec<u8> = (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect();
            String::from_utf8(bytes).unwrap()
        };
        let mut out: Vec<(String, String)> = std::fs::read_dir(self.dir.path().join("items"))
            .unwrap()
            .filter_map(|e| {
                let n = e.ok()?.file_name().into_string().ok()?;
                let (s, a) = n.split_once('.')?;
                Some((unhex(s), unhex(a)))
            })
            .collect();
        out.sort();
        out
    }

    /// The first argument of every invocation, in order.
    pub(crate) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every call's full argument list, one space-joined line per call
    /// (`-i` batches log only `-i`).
    pub(crate) fn argv(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("argv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for FakeSecurity {
    fn drop(&mut self) {
        set_fake_security(None);
        set_test_timeout(None);
    }
}

// ─── stash builder ────────────────────────────────────────────────────────────

/// Create `id`'s stash the way Orca's `create()` does, with optional
/// `oauth-account.json` and `.credentials.json` bytes.
pub(crate) fn make_stash(user_data: &Path, id: &str, oauth: Option<&[u8]>, creds: Option<&[u8]>) {
    let auth = default_auth_dir(user_data, id);
    std::fs::create_dir_all(&auth).unwrap();
    std::fs::write(auth.join(MARKER_FILE), format!("{id}\n")).unwrap();
    if let Some(b) = oauth {
        std::fs::write(auth.join(OAUTH_ACCOUNT_FILE), b).unwrap();
    }
    if let Some(b) = creds {
        std::fs::write(auth.join(CREDENTIALS_FILE), b).unwrap();
    }
}

// ─── store builders ───────────────────────────────────────────────────────────

/// A host account record as Orca's `persist()` writes it, pointing at the
/// default stash path under `user_data`.
pub(crate) fn record_json(user_data: &Path, id: &str, email: &str, org: Option<&str>) -> Value {
    let auth = default_auth_dir(user_data, id);
    super::record::new_record(
        &super::record::NewRecord {
            id,
            email,
            managed_auth_path: auth.to_str().unwrap(),
            organization_uuid: org,
            organization_name: None,
        },
        1_700_000_000_000,
    )
}

/// Write the active profile's `orca-data.json` with `accounts` and the
/// host active id, in Orca's compact form, and return its choice.
pub(crate) fn write_store(
    user_data: &Path,
    accounts: &[Value],
    active: Option<&str>,
) -> super::userdata::DataFileChoice {
    let choice = super::userdata::data_file(user_data);
    let v = json!({
        "schemaVersion": 1,
        "repos": [{"id": "r1", "path": "/Users/example/src/app"}],
        "settings": {
            "theme": "dark",
            "claudeManagedAccounts": accounts,
            "activeClaudeManagedAccountId": active,
            "activeClaudeManagedAccountIdsByRuntime": {"host": active, "wsl": {}},
            "zoom": 1.25
        },
        "ui": {"sidebar": true}
    });
    std::fs::create_dir_all(choice.path.parent().unwrap()).unwrap();
    std::fs::write(&choice.path, serde_json::to_vec(&v).unwrap()).unwrap();
    choice
}

/// Credential JSON as Claude Code writes it (compact).
pub(crate) fn creds_json(access: &str, refresh: &str, expires_at: i64) -> String {
    json!({"claudeAiOauth": {
        "accessToken": access,
        "refreshToken": refresh,
        "expiresAt": expires_at,
        "scopes": ["user:inference"],
        "subscriptionType": "max"
    }})
    .to_string()
}

/// An `oauthAccount` object.
pub(crate) fn oauth_json(uuid: &str, email: &str, org: Option<&str>) -> Value {
    json!({
        "accountUuid": uuid,
        "emailAddress": email,
        "organizationUuid": org,
        "organizationName": org.map(|_| "Acme")
    })
}

// ─── fake process table ───────────────────────────────────────────────────────

/// A `ProcInfo` for tests.
pub(crate) fn proc_info(pid: u32, name: &str, exe: Option<&str>, args: &[&str]) -> ProcInfo {
    let mut cmd = vec![std::ffi::OsString::from(exe.unwrap_or(name))];
    cmd.extend(args.iter().map(std::ffi::OsString::from));
    ProcInfo {
        pid,
        ppid: Some(1),
        name: name.to_owned(),
        exe: exe.map(PathBuf::from),
        cmd,
        start_time: 0,
    }
}

/// A process table under the test's control.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeProcs {
    listed: Vec<ProcInfo>,
    hidden: HashMap<u32, ProcInfo>,
    alive_only: Vec<u32>,
    unreadable: bool,
    environs: HashMap<u32, Vec<std::ffi::OsString>>,
}

impl FakeProcs {
    /// A process visible everywhere (table, probe, alive).
    pub(crate) fn with(mut self, p: ProcInfo) -> Self {
        self.listed.push(p);
        self
    }

    /// A process probe-able and alive but absent from the table sweep.
    pub(crate) fn with_hidden(mut self, p: ProcInfo) -> Self {
        self.hidden.insert(p.pid, p);
        self
    }

    /// A pid that is alive but cannot be probed.
    pub(crate) fn alive(mut self, pid: u32) -> Self {
        self.alive_only.push(pid);
        self
    }

    pub(crate) fn unreadable_table(mut self) -> Self {
        self.unreadable = true;
        self
    }

    /// Give `pid` a readable environment of `KEY=value` entries. A listed
    /// process without one reads as unreadable.
    pub(crate) fn with_env(mut self, pid: u32, vars: &[&str]) -> Self {
        self.environs
            .insert(pid, vars.iter().map(std::ffi::OsString::from).collect());
        self
    }
}

impl super::live::ProcFacts for FakeProcs {
    fn alive(&self, pid: u32) -> bool {
        self.alive_only.contains(&pid) || self.probe(pid).is_some()
    }

    fn probe(&self, pid: u32) -> Option<ProcInfo> {
        self.listed
            .iter()
            .find(|p| p.pid == pid)
            .cloned()
            .or_else(|| self.hidden.get(&pid).cloned())
    }

    fn table(&self) -> Option<Vec<ProcInfo>> {
        (!self.unreadable).then(|| self.listed.clone())
    }

    fn environ(&self, pid: u32) -> Option<Vec<std::ffi::OsString>> {
        self.environs.get(&pid).cloned()
    }
}

// ─── scripted liveness ────────────────────────────────────────────────────────

/// Liveness marks from a script: each `mark()` pops the next one; the last
/// one repeats. `on_check(n, f)` runs `f` right before check `n` (0-based)
/// returns, to change the world at L1 or L2.
pub(crate) struct ScriptedLiveness {
    marks: Mutex<std::collections::VecDeque<super::live::LiveMark>>,
    count: Mutex<usize>,
    hooks: Mutex<Vec<CheckHook>>,
}

/// A hook `on_check` runs right before check `n` returns.
type CheckHook = (usize, Box<dyn FnOnce() + Send>);

impl ScriptedLiveness {
    pub(crate) fn new(marks: Vec<super::live::LiveMark>) -> ScriptedLiveness {
        ScriptedLiveness {
            marks: Mutex::new(marks.into()),
            count: Mutex::new(0),
            hooks: Mutex::new(Vec::new()),
        }
    }

    /// Orca stays stopped.
    pub(crate) fn stopped() -> ScriptedLiveness {
        ScriptedLiveness::new(vec![super::live::LiveMark::stopped()])
    }

    /// Stopped for `n` checks, running from then on.
    pub(crate) fn appears_at(n: usize) -> ScriptedLiveness {
        let mut v = vec![super::live::LiveMark::stopped(); n];
        v.push(running_mark());
        ScriptedLiveness::new(v)
    }

    pub(crate) fn on_check(self, n: usize, f: impl FnOnce() + Send + 'static) -> Self {
        self.hooks.lock().unwrap().push((n, Box::new(f)));
        self
    }

    pub(crate) fn checks(&self) -> usize {
        *self.count.lock().unwrap()
    }
}

/// A mark for a running Orca.
pub(crate) fn running_mark() -> super::live::LiveMark {
    super::live::LiveMark {
        running: true,
        reasons: vec!["scripted".into()],
        singleton: Some("host-1".into()),
        runtime: Some(("rt-scripted".into(), 1, Some(1))),
    }
}

impl super::live::Liveness for ScriptedLiveness {
    fn mark(&self) -> super::live::LiveMark {
        let n = {
            let mut c = self.count.lock().unwrap();
            let n = *c;
            *c += 1;
            n
        };
        let hooks: Vec<_> = {
            let mut h = self.hooks.lock().unwrap();
            let (now, later): (Vec<_>, Vec<_>) = h.drain(..).partition(|(at, _)| *at == n);
            *h = later;
            now
        };
        for (_, f) in hooks {
            f();
        }
        let mut m = self.marks.lock().unwrap();
        if m.len() > 1 {
            m.pop_front().unwrap()
        } else {
            m.front().cloned().unwrap_or_default()
        }
    }
}

// ─── fake Orca RPC server ─────────────────────────────────────────────────────

type Handler = dyn Fn(&Value) -> Vec<String> + Send + Sync;

struct FakeState {
    runtime_id: String,
    token: String,
    requests: Vec<Value>,
}

/// A fake Orca: a temp userData with `orca-runtime.json` and an NDJSON unix
/// socket server. The handler returns the frames to send for a request;
/// [`FakeOrca::ok`] and [`FakeOrca::err`] build final frames that the server
/// completes with the request id and the current runtime id.
pub(crate) struct FakeOrca {
    dir: tempfile::TempDir,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    silent: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeOrca {
    const SOCKET: &'static str = "o-1-test.sock";

    #[cfg(unix)]
    pub(crate) fn start(
        handler: impl Fn(&Value) -> Vec<String> + Send + Sync + 'static,
    ) -> FakeOrca {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeState {
            runtime_id: String::new(),
            token: String::new(),
            requests: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let silent = Arc::new(AtomicBool::new(false));
        let sock = dir.path().join(Self::SOCKET);
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let (state, stop, silent) = (state.clone(), stop.clone(), silent.clone());
            std::thread::spawn(move || serve(listener, handler, state, stop, silent))
        };
        let orca = FakeOrca {
            dir,
            state,
            stop,
            silent,
            thread: Some(thread),
        };
        orca.restart();
        orca
    }

    /// Accept connections but never answer.
    pub(crate) fn silent(self) -> FakeOrca {
        self.silent.store(true, Ordering::SeqCst);
        self
    }

    /// Simulate an Orca restart: a new runtime id and token in the file.
    pub(crate) fn restart(&self) {
        let rid = format!("rt-{}", uuid::Uuid::new_v4().simple());
        let token = format!("fake-token-{}", uuid::Uuid::new_v4().simple());
        {
            let mut s = self.state.lock().unwrap();
            s.runtime_id = rid.clone();
            s.token = token.clone();
        }
        let meta = json!({
            "runtimeId": rid,
            "pid": std::process::id(),
            "transports": [
                {"kind": "unix", "endpoint": self.dir.path().join(Self::SOCKET)},
                {"kind": "websocket", "endpoint": "ws://127.0.0.1:1"}
            ],
            "authToken": token,
            "startedAt": 1
        });
        std::fs::write(self.dir.path().join("orca-runtime.json"), meta.to_string()).unwrap();
    }

    /// Stop serving and remove the socket (a crashed Orca).
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(self.dir.path().join(Self::SOCKET));
    }

    pub(crate) fn user_data(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn token(&self) -> String {
        self.state.lock().unwrap().token.clone()
    }

    pub(crate) fn requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().requests.clone()
    }

    /// A success frame for whatever request it answers.
    pub(crate) fn ok(result: Value) -> String {
        json!({"__fake_ok__": result}).to_string()
    }

    /// An error frame.
    pub(crate) fn err(code: &str, message: &str) -> String {
        json!({"__fake_err__": {"code": code, "message": message}}).to_string()
    }
}

impl Drop for FakeOrca {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Orca's Claude account state behind a [`FakeOrca`].
#[derive(Debug, Clone, Default)]
pub(crate) struct OrcaModel {
    pub accounts: Vec<Value>,
    pub active: Option<String>,
    /// The record `addClaudeFromConfigDir` appends.
    pub add_result: Option<Value>,
    /// Answer every mutation with this error (code, message).
    pub fail: Option<(String, String)>,
}

impl OrcaModel {
    fn claude(&self) -> Value {
        json!({
            "accounts": self.accounts,
            "activeAccountId": self.active,
            "activeAccountIdsByRuntime": {"host": self.active, "wsl": {}}
        })
    }
}

/// A handler serving `accounts.*` from a shared [`OrcaModel`].
pub(crate) fn model_handler(
    model: Arc<Mutex<OrcaModel>>,
) -> impl Fn(&Value) -> Vec<String> + Send + Sync + 'static {
    move |req: &Value| {
        let mut m = model.lock().unwrap();
        let method = req["method"].as_str().unwrap_or("");
        if method != "accounts.list"
            && let Some((code, msg)) = &m.fail
        {
            return vec![FakeOrca::err(code, msg)];
        }
        match method {
            "accounts.list" => vec![FakeOrca::ok(json!({
                "claude": m.claude(),
                "rateLimits": {"claude": null, "inactiveClaudeAccounts": []}
            }))],
            "accounts.selectClaude" => {
                m.active = req["params"]["accountId"].as_str().map(str::to_owned);
                vec![FakeOrca::ok(m.claude())]
            }
            "accounts.removeClaude" => {
                let id = req["params"]["accountId"].as_str().unwrap_or("").to_owned();
                m.accounts.retain(|a| a["id"] != id.as_str());
                if m.active.as_deref() == Some(id.as_str()) {
                    m.active = None;
                }
                vec![FakeOrca::ok(m.claude())]
            }
            "accounts.addClaudeFromConfigDir" => {
                if let Some(r) = m.add_result.clone() {
                    m.accounts.push(r);
                }
                vec![FakeOrca::ok(m.claude())]
            }
            _ => vec![FakeOrca::err("method_not_found", "unknown method")],
        }
    }
}

/// Complete a handler frame with the request id and runtime id.
fn complete_frame(frame: &str, id: &Value, rid: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(frame) else {
        return frame.to_owned();
    };
    if let Some(r) = v.get("__fake_ok__") {
        return json!({"id": id, "ok": true, "result": r, "_meta": {"runtimeId": rid}}).to_string();
    }
    if let Some(e) = v.get("__fake_err__") {
        return json!({"id": id, "ok": false, "error": e, "_meta": {"runtimeId": rid}}).to_string();
    }
    frame.to_owned()
}

#[cfg(unix)]
fn serve(
    listener: std::os::unix::net::UnixListener,
    handler: Arc<Handler>,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    silent: Arc<AtomicBool>,
) {
    use std::io::{BufRead, BufReader, Write};

    while !stop.load(Ordering::SeqCst) {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Err(_) => return,
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        });
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let rid = {
            let mut s = state.lock().unwrap();
            s.requests.push(req.clone());
            s.runtime_id.clone()
        };
        if silent.load(Ordering::SeqCst) {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            return;
        }
        let mut w = stream;
        for frame in handler(&req) {
            let out = complete_frame(&frame, &req["id"], &rid);
            let _ = w.write_all(out.as_bytes());
            let _ = w.write_all(b"\n");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::orca::keychain::{STASH_SERVICE, delete_stash, find_stash, write_stash};

    /// Every regular file under `dir` that carries an execute bit.
    fn executables_under(dir: &Path) -> Vec<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let m = std::fs::symlink_metadata(e.path()).unwrap();
                if m.is_dir() {
                    todo.push(e.path());
                } else if m.permissions().mode() & 0o111 != 0 {
                    out.push(e.path());
                }
            }
        }
        out
    }

    /// Regression guard for the machine freeze: the fake `security` runs
    /// under an existing interpreter and never writes a runnable file.
    #[test]
    fn the_fake_security_never_writes_an_executable() {
        let fake = FakeSecurity::install();
        write_stash("id-1", "{\"claudeAiOauth\":{}}").unwrap();
        assert!(find_stash("id-1").unwrap().is_some());
        assert!(delete_stash("id-1").unwrap());
        assert_eq!(fake.items(), Vec::<(String, String)>::new());
        assert!(fake.calls().len() >= 3, "{:?}", fake.calls());
        assert_eq!(executables_under(fake.root()), Vec::<PathBuf>::new());
        // The file `put` writes is data, not a program either.
        fake.put(STASH_SERVICE, "id-2", b"x");
        assert_eq!(executables_under(fake.root()), Vec::<PathBuf>::new());
    }
}
