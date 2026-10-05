//! csm as a second client of Orca's Claude account service.
//!
//! # Model
//!
//! Orca (the desktop app) owns every piece of Claude account state: the
//! account list (`settings.claudeManagedAccounts` in the active Orca
//! profile's `orca-data.json`), one credential stash per account
//! (`<userData>/claude-accounts/<id>/auth/` plus, on macOS, a Keychain item),
//! the active id, and the ONE runtime dir `D` it materializes the active
//! account into (Orca's own `CLAUDE_CONFIG_DIR`, trimmed, else `~/.claude`).
//! csm keeps no account registry of its own. With Orca running, a state
//! change goes through Orca's RPC, which runs the GUI's own code path; with
//! Orca stopped, csm runs its offline port of Orca's functions ([`switch`],
//! [`add`] and friends) so that Orca, on its next start, finds a state
//! its own code could have produced.
//!
//! The READ side:
//! - [`userdata`]: where Orca's state lives, the WSL rule, the Orca-profile
//!   index and the data-file choice;
//! - [`store`]: `orca-data.json` loaded with key order and number text
//!   preserved, the byte-exact round-trip gate, typed views of the account
//!   keys, and the pure `patch_settings`;
//! - [`record`]: the account record, identity (e9i), and the active-id
//!   helpers (d3/f3/p3/n6i);
//! - [`stash`]: the stash dir checks (Q2i/X4) and the credential bytes;
//! - [`keychain`]: the `/usr/bin/security` seam and its pure builders;
//! - [`runtime`]: `D`, the `.claude.json` path rule, readJsonObject
//!   semantics, `D`'s identity, and `D/sessions` liveness;
//! - [`live`], [`procenv`], [`version`]: is Orca running (fail closed),
//!   Orca main's `CLAUDE_CONFIG_DIR`, and Orca's version;
//! - [`rpc`]: the NDJSON RPC client;
//! - [`snapshot`]: one read-only view over all of the above.
//!
//! The WRITE side (Orca stopped; with Orca running every change goes over
//! RPC):
//! - [`fsx`]: guarded file primitives (atomic writes with Orca's tmp names,
//!   modes, the `switch.lock`), and csm's state dir;
//! - [`jsjson`]: `JSON.stringify` byte for byte, so every file csm writes for
//!   Orca or Claude Code is one their own code could have written;
//! - [`store`]'s write protocol (L0/L1/L2 liveness checks, stamp re-check,
//!   pre-image, RPC redo), [`stash`]'s create/write/remove, [`keychain`]'s
//!   writes, and [`runtime`]'s materialize with pre-images;
//! - [`http`], [`refresh`], [`readback`], [`quarantine`], [`sysdefault`]:
//!   the token refresh, the read-back port with the profile veto, the
//!   quarantine for grants csm cannot attribute, and the system-default
//!   snapshot;
//! - [`switch`]: `plan_switch`, the RPC path and its outcome classifier, the
//!   journaled offline executor, recovery and verification;
//! - [`add`]: `accounts add`, `import` and `rm`.
//!
//! # Never
//!
//! - csm never selects a null account (Orca's "System default"): a
//!   `selectClaude` request with a null or blank id cannot be built.
//! - csm never writes Orca's store, stashes or Keychain items while Orca
//!   runs; that goes through RPC. Every offline write re-checks liveness
//!   right before it lands.
//! - csm never discards a credential it cannot attribute: it goes to the
//!   quarantine ([`quarantine`]).
//! - csm never prints, logs or persists a secret: OAuth tokens, credential
//!   JSON, Orca's RPC `authToken`, Orca's agent hook token. Types that hold
//!   one ([`SecretString`], `rpc::AuthToken`, `rpc::RequestLine`) redact in
//!   `Debug` and implement no `Display`/`Serialize`. Errors name paths and
//!   reasons, never file contents.
//!
//! # Test safety
//!
//! Under `cfg(test)` every resolver that could reach the real machine is
//! guarded: [`HostEnv::current`] refuses without a test home (and refuses
//! the real home), the Keychain runner refuses the real `/usr/bin/security`
//! unless a fake binary is installed, RPC refuses endpoints outside the temp
//! dir, the process-table source is empty, every file write refuses a path
//! outside the temp dir ([`fsx::guard`]), and the real HTTP client and the
//! real `claude`/`orca` CLIs refuse to run.

pub mod add;
pub mod context;
pub mod forward;
pub mod fsx;
pub mod http;
pub mod jsjson;
pub mod keychain;
pub mod live;
pub mod procenv;
pub mod quarantine;
pub mod readback;
pub mod record;
pub mod refresh;
pub mod rpc;
pub mod runtime;
pub mod stash;
pub mod statedb;
pub mod store;
pub mod switch;
pub mod sysdefault;
pub mod userdata;
pub mod version;

#[cfg(test)]
pub(crate) mod testsupport;

mod snapshot;
pub use snapshot::{AccountSource, OrcaView, SnapshotOptions, snapshot, snapshot_with};

use std::io;
use std::path::{Path, PathBuf};

pub use userdata::HostOs;

// ─── errors ───────────────────────────────────────────────────────────────────

/// Why an Orca read failed. Messages name paths and reasons only, never the
/// content of a file (which may hold a secret).
#[derive(Debug, thiserror::Error)]
pub enum OrcaError {
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A file or response has an unexpected shape.
    #[error("{0}")]
    Invalid(String),
    /// A safety check refused the operation.
    #[error("refused: {0}")]
    Refused(String),
    /// A network call csm needs got no answer (DNS, connect, timeout), or an
    /// answer csm cannot act on. Nothing was changed.
    #[error("network unavailable: {0}")]
    Network(String),
    #[error(transparent)]
    Rpc(#[from] rpc::RpcError),
    #[error(transparent)]
    Keychain(#[from] keychain::KeychainError),
}

impl OrcaError {
    pub(crate) fn io(what: &'static str, path: &Path, source: io::Error) -> Self {
        OrcaError::Io {
            what,
            path: path.to_path_buf(),
            source,
        }
    }
}

// ─── secrets ──────────────────────────────────────────────────────────────────

/// A secret string (credential JSON, a token). `Debug` redacts, there is no
/// `Display`/`Serialize`, and the bytes are zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        SecretString(s)
    }

    /// The secret itself. Callers must not print or log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(<redacted {} bytes>)", self.0.len())
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        zero(unsafe_bytes(&mut self.0));
    }
}

/// Secret bytes (a credential file that may not be UTF-8). `Debug` redacts
/// and the bytes are zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(b: Vec<u8>) -> Self {
        SecretBytes(b)
    }

    /// The secret itself. Callers must not print or log it.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBytes(<redacted {} bytes>)", self.0.len())
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// The bytes of `s` for zeroing. Zero bytes are valid UTF-8, so the string
/// stays valid.
fn unsafe_bytes(s: &mut str) -> &mut [u8] {
    // SAFETY: only ever overwritten with 0x00, which keeps valid UTF-8.
    unsafe { s.as_bytes_mut() }
}

/// The bytes of a `String` for zeroing (see [`zero`]).
pub(crate) fn unsafe_bytes_of(s: &mut String) -> &mut [u8] {
    unsafe_bytes(s.as_mut_str())
}

/// Overwrite `bytes` with zeros in a way the optimizer keeps.
pub(crate) fn zero(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusively borrowed u8; the
        // volatile write only keeps the zeroing from being optimized out.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

// ─── host environment ─────────────────────────────────────────────────────────

/// The environment inputs every Orca resolver reads, captured once. Tests
/// build one by hand; [`HostEnv::current`] reads the real process.
#[derive(Debug, Clone)]
pub struct HostEnv {
    pub os: HostOs,
    pub home: PathBuf,
    pub xdg_config_home: Option<String>,
    pub appdata: Option<String>,
    pub localappdata: Option<String>,
    /// `XDG_STATE_HOME` (csm's state dir on unix).
    pub xdg_state_home: Option<String>,
    /// `ORCA_USER_DATA_PATH` (a hint, see [`userdata`]).
    pub orca_user_data_path: Option<String>,
    /// csm's own `CLAUDE_CONFIG_DIR` (getRuntimePaths input for csm's `D`).
    pub claude_config_dir: Option<String>,
    pub wsl_distro_name: Option<String>,
    /// `/proc/version` (Linux only).
    pub proc_version: Option<String>,
    pub user: Option<String>,
    pub username: Option<String>,
    /// The OS user name (`os.userInfo().username`), when `USER`/`USERNAME`
    /// are unset.
    pub os_user: Option<String>,
}

fn env_var(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

impl HostEnv {
    /// The real process environment.
    #[cfg(not(test))]
    pub fn current() -> Result<HostEnv, OrcaError> {
        let home = crate::paths::home_dir()
            .ok_or_else(|| OrcaError::Invalid("cannot resolve the home directory".into()))?;
        let os = HostOs::current();
        let proc_version = if os == HostOs::Linux {
            std::fs::read_to_string("/proc/version").ok()
        } else {
            None
        };
        Ok(HostEnv {
            os,
            home,
            xdg_config_home: env_var("XDG_CONFIG_HOME"),
            appdata: env_var("APPDATA"),
            localappdata: env_var("LOCALAPPDATA"),
            xdg_state_home: env_var("XDG_STATE_HOME"),
            orca_user_data_path: env_var("ORCA_USER_DATA_PATH"),
            claude_config_dir: env_var("CLAUDE_CONFIG_DIR"),
            wsl_distro_name: env_var("WSL_DISTRO_NAME"),
            proc_version,
            user: env_var("USER"),
            username: env_var("USERNAME"),
            os_user: os_user_name(),
        })
    }

    /// Test build: the thread's test home and NO real environment. Refuses
    /// without a test home, and refuses a test home that is the real home,
    /// so no test can resolve the real Orca userData, `~/.claude` or csm's
    /// state dir.
    #[cfg(test)]
    pub fn current() -> Result<HostEnv, OrcaError> {
        let home = crate::testenv::test_home().ok_or_else(|| {
            OrcaError::Refused("cfg(test): no test home set (testenv::with_test_home)".into())
        })?;
        if dirs::home_dir().is_some_and(|real| real == home) {
            return Err(OrcaError::Refused(
                "cfg(test): the test home is the real home".into(),
            ));
        }
        let _ = env_var; // the real environment is never read under test
        Ok(HostEnv::for_test(&home, HostOs::current()))
    }

    /// A blank environment rooted at `home` (tests and pure callers).
    #[cfg(test)]
    pub fn for_test(home: &Path, os: HostOs) -> HostEnv {
        HostEnv {
            os,
            home: home.to_path_buf(),
            xdg_config_home: None,
            appdata: None,
            localappdata: None,
            xdg_state_home: None,
            orca_user_data_path: None,
            claude_config_dir: None,
            wsl_distro_name: None,
            proc_version: None,
            user: None,
            username: None,
            os_user: None,
        }
    }
}

/// The OS user name, the way Node's `os.userInfo().username` reads it.
#[cfg(all(unix, not(test)))]
fn os_user_name() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
}

#[cfg(all(not(unix), not(test)))]
fn os_user_name() -> Option<String> {
    None
}

// ─── small I/O helpers ────────────────────────────────────────────────────────

/// Read `path`, refusing anything that is not a regular file (a FIFO would
/// block forever) or that exceeds `cap` bytes. `Ok(None)` when absent.
/// Error messages name the path and the reason, never the content.
pub(crate) fn read_capped_bytes(path: &Path, cap: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let not_file = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        )
    };
    // Refuse a FIFO or device before `open`, which would block on a FIFO
    // with no writer. The open itself is non-blocking on unix too, so a
    // path swapped for a FIFO between the two checks cannot hang either;
    // O_NONBLOCK has no effect on a regular file's reads.
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => return Err(not_file()),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(nix::libc::O_NONBLOCK);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    let too_big = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds {cap} bytes", path.display()),
        )
    };
    if !meta.is_file() {
        return Err(not_file());
    }
    if meta.len() > cap {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    file.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(too_big());
    }
    Ok(Some(buf))
}

/// [`read_capped_bytes`] decoded as UTF-8.
pub(crate) fn read_capped(path: &Path, cap: u64) -> io::Result<Option<String>> {
    match read_capped_bytes(path, cap)? {
        None => Ok(None),
        Some(b) => String::from_utf8(b).map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not UTF-8", path.display()),
            )
        }),
    }
}

/// Current wall clock in epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_env_refuses_without_a_test_home() {
        crate::testenv::set_test_home(None);
        let err = HostEnv::current().unwrap_err();
        assert!(matches!(err, OrcaError::Refused(_)), "{err:?}");
    }

    #[test]
    fn host_env_refuses_the_real_home() {
        let Some(real) = dirs::home_dir() else {
            return;
        };
        let err = crate::testenv::with_test_home(&real, HostEnv::current).unwrap_err();
        assert!(matches!(err, OrcaError::Refused(_)), "{err:?}");
    }

    #[test]
    fn host_env_under_test_reads_no_real_environment() {
        let dir = tempfile::tempdir().unwrap();
        let env = crate::testenv::with_test_home(dir.path(), HostEnv::current).unwrap();
        assert_eq!(env.home, dir.path());
        assert!(env.orca_user_data_path.is_none() && env.claude_config_dir.is_none());
        assert!(env.xdg_config_home.is_none() && env.appdata.is_none());
        assert!(env.user.is_none() && env.os_user.is_none());
    }

    #[test]
    fn secret_string_redacts_and_reports_length_only() {
        let s = SecretString::new("{\"accessToken\":\"sk-secret\"}".into());
        let d = format!("{s:?}");
        assert!(!d.contains("sk-secret"), "{d}");
        assert!(d.contains("redacted"));
    }

    #[test]
    fn read_capped_refuses_oversize_and_non_files() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"0123456789").unwrap();
        assert_eq!(read_capped_bytes(&f, 10).unwrap().unwrap().len(), 10);
        let e = read_capped_bytes(&f, 9).unwrap_err();
        assert!(!e.to_string().contains("0123"), "{e}");
        assert!(read_capped_bytes(dir.path(), 10).is_err());
        assert!(
            read_capped_bytes(&dir.path().join("absent"), 10)
                .unwrap()
                .is_none()
        );
        std::fs::write(&f, [0xff, 0xfe]).unwrap();
        assert!(read_capped(&f, 10).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_capped_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("creds");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        // A blocking open of a FIFO with no writer never returns: run the
        // read on a thread and fail the test instead of hanging the suite.
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_capped_bytes(&p, 10).map(|o| o.is_some()));
        });
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read_capped_bytes blocked on a FIFO");
        let e = r.unwrap_err();
        assert!(e.to_string().contains("not a regular file"), "{e}");
    }
}
