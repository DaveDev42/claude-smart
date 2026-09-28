//! Where Orca keeps its state: the userData dir, the Orca-profile index, and
//! the `orca-data.json` file of the active Orca profile.
//!
//! Facts ported from Orca 1.4.209:
//! - userData is Electron's `app.getPath("userData")` for the app `orca`:
//!   macOS `~/Library/Application Support/orca`, Linux `$XDG_CONFIG_HOME/orca`
//!   or `~/.config/orca`, Windows `%APPDATA%\orca` (Linux/Windows inferred
//!   from Electron's conventions).
//! - `ORCA_USER_DATA_PATH` is Orca's OUTPUT, not an input: packaged Orca sets
//!   it on its own `process.env` (xBi), so every pane inherits it; only a dev
//!   build honours `ORCA_DEV_USER_DATA_PATH`. csm therefore treats it as a
//!   hint and uses it only when it equals the default or holds a live
//!   `orca-runtime.json` (a dev build or an e2e sandbox).
//! - Under WSL the hint names the WINDOWS Orca's userData (Orca forwards it
//!   into WSL panes). csm ignores it there, never creates or writes a store
//!   reached through it, and reads the distro's own Linux userData instead.
//! - `orca-profile-index.json` picks the Orca profile (SW/xSr/bSr): a profile
//!   counts only when it passes [`is_valid_profile`]; `activeProfileId` wins
//!   when it names a valid profile, else the first valid one, else
//!   `local-default`. A missing or invalid index falls back to its `.bak`
//!   (readProfileIndex). When neither parses while either file exists,
//!   Orca 1.4.214 refuses to start (readExistingProfileIndex) instead of
//!   using `local-default`; csm then refuses every store read and write
//!   ([`DataFileChoice::index_unreadable`]), since it cannot tell which
//!   profile's store Orca will load once the index is repaired.
//! - The data file is `<userData>/profiles/<id>/orca-data.json`. For
//!   `local-default`, Orca migrates a legacy `<userData>/orca-data.json` into
//!   the profile dir at start when the profile file is missing (ESr); csm
//!   reads that legacy file as a fallback view and never writes it.
//! - The stash root and the system-default snapshot dir are NOT under this
//!   canonical userData but under the late one Orca resolves after
//!   `app.setName('Orca')`: [`late_user_data`] (Orca 1.4.214).

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::HostEnv;

/// The id Orca gives its built-in profile.
pub const DEFAULT_PROFILE_ID: &str = "local-default";
/// `<userData>/orca-profile-index.json`.
pub const PROFILE_INDEX_FILE: &str = "orca-profile-index.json";
/// The per-profile store file name.
pub const DATA_FILE: &str = "orca-data.json";
/// `<userData>/orca-runtime.json`, Orca's RPC metadata.
pub const RUNTIME_FILE: &str = "orca-runtime.json";

/// Cap on the profile index (a real one is well under a kilobyte).
const INDEX_CAP: u64 = 1024 * 1024;

// ─── host OS ──────────────────────────────────────────────────────────────────

/// The OS family whose userData convention applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    Linux,
    Windows,
}

impl HostOs {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            HostOs::MacOs
        } else if cfg!(windows) {
            HostOs::Windows
        } else {
            HostOs::Linux
        }
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Electron's default userData for the app `orca`. Pure.
pub fn default_user_data(
    os: HostOs,
    home: &Path,
    xdg_config_home: Option<&str>,
    appdata: Option<&str>,
) -> PathBuf {
    match os {
        HostOs::MacOs => home
            .join("Library")
            .join("Application Support")
            .join("orca"),
        HostOs::Linux => match non_empty(xdg_config_home) {
            Some(x) => PathBuf::from(x).join("orca"),
            None => home.join(".config").join("orca"),
        },
        // Electron's appData on Windows is %APPDATA%; without it, the
        // roaming dir under the profile is the documented default.
        HostOs::Windows => match non_empty(appdata) {
            Some(a) => PathBuf::from(a).join("orca"),
            None => home.join("AppData").join("Roaming").join("orca"),
        },
    }
}

/// Is this a WSL distro? `WSL_DISTRO_NAME` set, or `/proc/version` naming
/// Microsoft. Pure.
pub fn is_wsl(os: HostOs, wsl_distro_name: Option<&str>, proc_version: Option<&str>) -> bool {
    if os != HostOs::Linux {
        return false;
    }
    non_empty(wsl_distro_name).is_some()
        || proc_version.is_some_and(|v| v.to_ascii_lowercase().contains("microsoft"))
}

// ─── userData resolution ─────────────────────────────────────────────────────

/// How the `ORCA_USER_DATA_PATH` hint was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintUse {
    /// No hint was set.
    Absent,
    /// The hint equals the default dir.
    SameAsDefault,
    /// The hint differs from the default and holds a live
    /// `orca-runtime.json` (a dev build or sandbox); csm used it.
    UsedLive,
    /// The hint differs and holds no live runtime file; csm ignored it.
    Ignored,
    /// Under WSL the hint names the Windows Orca's userData; ignored.
    IgnoredWsl,
}

/// The resolved userData dir and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserData {
    pub dir: PathBuf,
    pub hint: HintUse,
    /// Running inside a WSL distro.
    pub wsl: bool,
    /// Inside WSL with a Windows Orca's hint present: csm was started from a
    /// Windows Orca pane. Launch-only; no switch and no store access.
    pub windows_orca_pane: bool,
}

impl UserData {
    /// May csm touch Orca's store, stashes, or RPC through this userData?
    /// Not from a WSL shell a Windows Orca launched.
    pub fn store_access_allowed(&self) -> bool {
        !self.windows_orca_pane
    }
}

/// Pure userData resolution. `hint_is_live(dir)` answers whether `dir` holds
/// an `orca-runtime.json` whose pid is alive.
pub fn resolve(env: &HostEnv, hint_is_live: impl Fn(&Path) -> bool) -> UserData {
    let wsl = is_wsl(
        env.os,
        env.wsl_distro_name.as_deref(),
        env.proc_version.as_deref(),
    );
    let default = default_user_data(
        env.os,
        &env.home,
        env.xdg_config_home.as_deref(),
        env.appdata.as_deref(),
    );
    let hint = non_empty(env.orca_user_data_path.as_deref()).map(PathBuf::from);
    let (dir, hint_use) = match hint {
        None => (default, HintUse::Absent),
        Some(_) if wsl => (default, HintUse::IgnoredWsl),
        Some(h) if h == default => (default, HintUse::SameAsDefault),
        Some(h) if h.is_absolute() && hint_is_live(&h) => (h, HintUse::UsedLive),
        Some(_) => (default, HintUse::Ignored),
    };
    let windows_orca_pane = hint_use == HintUse::IgnoredWsl;
    UserData {
        dir,
        hint: hint_use,
        wsl,
        windows_orca_pane,
    }
}

// ─── profile index (bSr / xSr) ───────────────────────────────────────────────

fn is_object(v: &Value) -> bool {
    v.is_object()
}

fn valid_profile_id(id: &str) -> bool {
    // ^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && id.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Orca's bSr: does `v` describe a valid Orca profile? Pure.
pub fn is_valid_profile(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    let is_num = |k: &str| obj.get(k).is_some_and(Value::is_number);
    let id_ok = obj
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(valid_profile_id);
    let name_ok = obj
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| !n.is_empty());
    let kind_ok = matches!(
        obj.get("kind").and_then(Value::as_str),
        Some("local" | "cloud-linked")
    );
    let avatar_ok = obj.get("avatar").is_some_and(|a| {
        is_object(a)
            && a.get("kind").and_then(Value::as_str) == Some("initials")
            && a.get("initials").is_some_and(Value::is_string)
            && a.get("color").and_then(Value::as_str) == Some("neutral")
    });
    // `cloud === undefined || isObject(cloud)`: an explicit null fails.
    let cloud_ok = match obj.get("cloud") {
        None => true,
        Some(c) => is_object(c),
    };
    id_ok
        && name_ok
        && kind_ok
        && is_num("createdAt")
        && is_num("updatedAt")
        && is_num("lastOpenedAt")
        && avatar_ok
        && cloud_ok
}

/// The parts of a valid profile index csm needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileIndex {
    pub active_profile_id: String,
    pub profile_ids: Vec<String>,
}

/// Orca's xSr over parsed JSON: keep valid profiles, pick the active one.
/// `None` when no profile is valid. Pure.
pub fn parse_profile_index(v: &Value) -> Option<ProfileIndex> {
    let profiles = v.as_object()?.get("profiles")?.as_array()?;
    let ids: Vec<String> = profiles
        .iter()
        .filter(|p| is_valid_profile(p))
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let requested = v.get("activeProfileId").and_then(Value::as_str);
    let active = match requested {
        Some(r) if ids.iter().any(|i| i == r) => r.to_owned(),
        _ => ids.first()?.clone(),
    };
    Some(ProfileIndex {
        active_profile_id: active,
        profile_ids: ids,
    })
}

fn read_index_file(path: &Path) -> Option<ProfileIndex> {
    let bytes = super::read_capped_bytes(path, INDEX_CAP).ok()??;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    parse_profile_index(&v)
}

fn index_bak(path: &Path) -> PathBuf {
    let mut bak = path.to_path_buf().into_os_string();
    bak.push(".bak");
    PathBuf::from(bak)
}

/// Orca's readProfileIndex (profile-index-store.ts, 1.4.212 and 1.4.214):
/// the index file, or its `.bak` when the file is missing or invalid.
/// `None` means no index parses (see [`index_present`]).
pub fn read_profile_index(user_data: &Path) -> Option<ProfileIndex> {
    let path = user_data.join(PROFILE_INDEX_FILE);
    read_index_file(&path).or_else(|| read_index_file(&index_bak(&path)))
}

/// Does the index or its `.bak` exist (readExistingProfileIndex's
/// `existsSync` check)? A path that cannot be checked counts as present
/// (fail closed).
pub fn index_present(user_data: &Path) -> bool {
    let path = user_data.join(PROFILE_INDEX_FILE);
    let exists = |p: &Path| !matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    exists(&path) || exists(&index_bak(&path))
}

// ─── data file ───────────────────────────────────────────────────────────────

/// Which `orca-data.json` Orca would load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFileChoice {
    pub profile_id: String,
    /// `<userData>/profiles/<id>/orca-data.json`.
    pub path: PathBuf,
    /// The index named the profile (false: Orca's built-in default).
    pub from_index: bool,
    /// For `local-default` only: the legacy root file Orca migrates at start
    /// when `path` is missing. Read-only for csm.
    pub legacy_root: Option<PathBuf>,
    /// The profile index (or its `.bak`) exists but neither parses: Orca
    /// 1.4.214 refuses to start, and csm cannot tell which profile's store
    /// is the real one, so every store read and write refuses.
    pub index_unreadable: bool,
}

/// Orca 1.4.214's SQLite store of record, beside the profile's
/// `orca-data.json` (getOrcaProfileStateDatabaseFile).
pub const STATE_DB: &str = "profile-state.db";

impl DataFileChoice {
    /// The SQLite file family of this profile (profileStateDatabaseFiles):
    /// `profile-state.db` and its `-wal`, `-shm` and `-journal` files.
    pub fn state_db_files(&self) -> Vec<PathBuf> {
        let dir = self.path.parent().unwrap_or(Path::new(""));
        ["", "-wal", "-shm", "-journal"]
            .iter()
            .map(|sfx| dir.join(format!("{STATE_DB}{sfx}")))
            .collect()
    }

    /// Does any file of the SQLite family exist (hasProfileStateDatabaseFiles)?
    /// From Orca 1.4.214 on, such a profile's store of record is SQLite and
    /// `orca-data.json` is a compatibility export pinned by a hash acceptance
    /// marker: any byte csm changes there makes Orca refuse to start
    /// ("both JSON and SQLite storage without a matching acceptance marker"),
    /// and a JSON file csm creates beside the database does the same. csm
    /// therefore never writes such a profile offline, and its JSON may lag
    /// the database. An unreadable check counts as present (fail closed).
    pub fn has_state_db(&self) -> bool {
        self.state_db_files()
            .iter()
            .any(|p| !matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound))
    }
}

/// How many rotating `.bak.N` backups Orca keeps of a data file
/// (PROFILE_STATE_LEGACY_BACKUP_COUNT).
pub const LEGACY_BACKUP_COUNT: usize = 5;

/// `name` is one of Orca's retained recovery artifacts beside a data file
/// named `data` and a database named [`STATE_DB`]: a SQLite migration export
/// `<data>.sqlite-export.<n>.json` (profileStateJsonExportPaths) or a
/// database backup `profile-state.db.backup.<id>.db`
/// (profileStateDatabaseBackups). The backup id is matched loosely (any
/// non-empty id): a false positive only refuses. Pure.
pub fn is_retained_artifact(name: &str, data: &str) -> bool {
    let export = format!("{data}.sqlite-export.");
    if let Some(rest) = name.strip_prefix(&export)
        && let Some(n) = rest.strip_suffix(".json")
    {
        return !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0');
    }
    let backup = format!("{STATE_DB}.backup.");
    name.strip_prefix(&backup)
        .and_then(|r| r.strip_suffix(".db"))
        .is_some_and(|id| !id.is_empty())
}

impl DataFileChoice {
    /// Evidence that this profile's primary store was lost, which Orca
    /// restores from or refuses to start over (hasStateBackup,
    /// assertProfileStateCanInitialize, assertNoRetainedProfileStateExports):
    /// a `.bak.0`-`.bak.4` of the data file (or of the legacy root file), a
    /// retained SQLite export, or a database backup. `None` means none.
    /// A directory that cannot be listed counts as evidence (fail closed).
    /// csm never creates a store over such evidence: a fresh minimal file
    /// would be read as a valid primary and hide the user's state.
    pub fn recovery_evidence(&self) -> Option<String> {
        let exists = |p: &Path| !matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let baks = |file: &Path| {
            (0..LEGACY_BACKUP_COUNT).find_map(|i| {
                let mut b = file.as_os_str().to_owned();
                b.push(format!(".bak.{i}"));
                let b = PathBuf::from(b);
                exists(&b).then(|| b.display().to_string())
            })
        };
        if let Some(b) = baks(&self.path) {
            return Some(b);
        }
        if let Some(b) = self.legacy_root.as_deref().and_then(baks) {
            return Some(b);
        }
        let dir = self.path.parent().unwrap_or(Path::new(""));
        let data = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(DATA_FILE);
        match std::fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(format!("{} (cannot be listed)", dir.display())),
            Ok(rd) => {
                for e in rd {
                    let Ok(e) = e else {
                        return Some(format!("{} (cannot be listed)", dir.display()));
                    };
                    if e.file_name()
                        .to_str()
                        .is_some_and(|n| is_retained_artifact(n, data))
                    {
                        return Some(e.path().display().to_string());
                    }
                }
                None
            }
        }
    }
}

/// The data file of the active Orca profile. Pure given the index.
pub fn data_file_for(user_data: &Path, index: Option<&ProfileIndex>) -> DataFileChoice {
    let (profile_id, from_index) = match index {
        Some(i) => (i.active_profile_id.clone(), true),
        None => (DEFAULT_PROFILE_ID.to_owned(), false),
    };
    let path = user_data.join("profiles").join(&profile_id).join(DATA_FILE);
    let legacy_root = (profile_id == DEFAULT_PROFILE_ID).then(|| user_data.join(DATA_FILE));
    DataFileChoice {
        profile_id,
        path,
        from_index,
        legacy_root,
        index_unreadable: false,
    }
}

/// The data file of the active Orca profile under `user_data`.
pub fn data_file(user_data: &Path) -> DataFileChoice {
    let index = read_profile_index(user_data);
    let mut choice = data_file_for(user_data, index.as_ref());
    choice.index_unreadable = index.is_none() && index_present(user_data);
    choice
}

// ─── the late userData ───────────────────────────────────────────────────────
//
// Orca pins `orca-data.json`, `orca-runtime.json` and the profile index to
// the userData it captures at start (`initDataPath`,
// `getCanonicalUserDataPath`). Two account roots are NOT pinned: the stash
// root (`getClaudeManagedAccountsRoot`, managed-auth-path.ts) and the
// system-default snapshot dir (`claude-runtime-auth`,
// runtime-auth-file-storage.ts) call `app.getPath('userData')` on every use,
// which runs after the post-ready `app.setName('Orca')`. Orca's own comment
// (user-data-path.ts) says that call "flips path case", which would put the
// account roots under `<appData>/Orca`, a second dir on a case-sensitive
// filesystem.
//
// In practice the flip does not happen: Electron resolves userData once
// (Chromium's PathService caches it, and Orca reads it before `setName`),
// and a live macOS Orca 1.4.212 writes lowercase `.../orca/claude-accounts/`
// record paths and `.../orca/daemon/` socket paths long after `setName`.
// And if the flip were real, Orca's first start would create
// `<appData>/Orca/daemon` (`getDaemonRuntimeDir` → `ensurePrivateDir`), so
// "the canonical dir exists and the late one does not" can only mean the
// canonical dir is where the stashes are. csm therefore uses the late
// sibling only when the disk shows Orca put its account roots there: the
// canonical dir is missing, or both exist and only the late one holds
// `claude-accounts`, or both hold it (a flip Orca actually made).

/// How the late sibling relates to the canonical userData on disk.
// Only the unix probe builds the on-disk answers; Windows never asks.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LateProbe {
    /// Both names open the same dir: a case-insensitive filesystem.
    SameDir,
    /// Both exist and are different dirs.
    OtherDir,
    /// The canonical dir exists and the late one does not: the filesystem
    /// tells the two names apart.
    LateMissing,
    /// Only the late dir exists.
    CanonicalMissing,
    /// Neither exists, or a probe failed: the filesystem cannot be asked.
    Unknown,
}

/// Does Orca use the late sibling `<appData>/Orca` for its account roots?
/// Pure. Windows never does: its filesystem is case-insensitive by default,
/// and csm does not compare paths there by identity. Elsewhere only when
/// the disk shows it (see the section note): `LateMissing` and `Unknown`
/// keep the canonical dir, which is safe under either reading of `setName`
/// (before Orca's first start there is no stash anywhere).
pub fn use_late_sibling(os: HostOs, probe: LateProbe) -> bool {
    match (os, probe) {
        (HostOs::Windows, _) => false,
        (_, LateProbe::OtherDir | LateProbe::CanonicalMissing) => true,
        (_, LateProbe::SameDir | LateProbe::LateMissing | LateProbe::Unknown) => false,
    }
}

/// Both dirs exist and differ: which one holds Orca's account roots? Pure.
/// The late one, unless only the canonical one has `claude-accounts` (the
/// late dir then exists for some other reason, and Orca never flipped).
pub fn other_dir_uses_late(canonical_has_accounts: bool, late_has_accounts: bool) -> bool {
    !(canonical_has_accounts && !late_has_accounts)
}

/// `<parent>/Orca` when `user_data`'s last component is exactly `orca`
/// (Electron's name before `setName`), else `None`: a dev or sandbox
/// userData is pinned with `setPath` and does not move. Pure.
pub fn late_sibling(user_data: &Path) -> Option<PathBuf> {
    (user_data.file_name()? == "orca").then(|| user_data.with_file_name("Orca"))
}

#[cfg(unix)]
fn probe_late(canonical: &Path, late: &Path) -> LateProbe {
    use std::os::unix::fs::MetadataExt;
    let found = |p: &Path| match std::fs::metadata(p) {
        Ok(m) => Ok(Some((m.dev(), m.ino()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    match (found(canonical), found(late)) {
        (Ok(Some(a)), Ok(Some(b))) if a == b => LateProbe::SameDir,
        (Ok(Some(_)), Ok(Some(_))) => LateProbe::OtherDir,
        (Ok(Some(_)), Ok(None)) => LateProbe::LateMissing,
        (Ok(None), Ok(Some(_))) => LateProbe::CanonicalMissing,
        _ => LateProbe::Unknown,
    }
}

#[cfg(not(unix))]
fn probe_late(_canonical: &Path, _late: &Path) -> LateProbe {
    LateProbe::Unknown
}

/// The userData Orca's account roots live under (see the section note).
/// Equals `user_data` unless a case-sensitive filesystem shows Orca put its
/// account roots under the sibling `Orca`.
pub fn late_user_data(user_data: &Path) -> PathBuf {
    late_user_data_for(HostOs::current(), user_data)
}

fn late_user_data_for(os: HostOs, user_data: &Path) -> PathBuf {
    let Some(late) = late_sibling(user_data) else {
        return user_data.to_path_buf();
    };
    let probe = probe_late(user_data, &late);
    let use_late = use_late_sibling(os, probe)
        && (probe != LateProbe::OtherDir
            || other_dir_uses_late(
                user_data.join("claude-accounts").is_dir(),
                late.join("claude-accounts").is_dir(),
            ));
    if use_late {
        late
    } else {
        user_data.to_path_buf()
    }
}

/// `<late userData>/claude-accounts`, the stash root (per userData, not per
/// Orca profile). `user_data` is the canonical dir; see [`late_user_data`].
pub fn claude_accounts_root(user_data: &Path) -> PathBuf {
    late_user_data(user_data).join("claude-accounts")
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Orca's 'refuses an unreadable %s index without replacing it'
    /// (profile-index-store.test.ts): primary, backup, both.
    #[test]
    fn an_unreadable_index_is_never_read_as_local_default() {
        let good = r#"{"schemaVersion":1,"activeProfileId":"p1","profiles":[{"id":"p1","name":"Work","kind":"local","createdAt":1,"updatedAt":1,"lastOpenedAt":1,"avatar":{"kind":"initials","initials":"W","color":"neutral"}}]}"#;
        for (primary, bak) in [
            (Some("{"), None),
            (None, Some("{")),
            (Some("{"), Some("{")),
            (
                Some(r#"{"activeProfileId":"../x","profiles":[{"id":"../x"}]}"#),
                None,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let ud = tmp.path();
            if let Some(t) = primary {
                std::fs::write(ud.join(PROFILE_INDEX_FILE), t).unwrap();
            }
            if let Some(t) = bak {
                std::fs::write(index_bak(&ud.join(PROFILE_INDEX_FILE)), t).unwrap();
            }
            let c = data_file(ud);
            assert!(c.index_unreadable, "{primary:?} {bak:?}");
            assert!(matches!(
                crate::orca::store::load_choice(&c),
                Err(crate::orca::OrcaError::Refused(_))
            ));
            assert!(!ud.join("profiles").exists());
        }
        // A broken primary with a good backup, and no index at all, are fine.
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path();
        assert!(!data_file(ud).index_unreadable);
        std::fs::write(ud.join(PROFILE_INDEX_FILE), "{").unwrap();
        std::fs::write(index_bak(&ud.join(PROFILE_INDEX_FILE)), good).unwrap();
        let c = data_file(ud);
        assert!(!c.index_unreadable);
        assert_eq!(c.profile_id, "p1");
    }

    #[test]
    fn retained_artifacts_are_orcas_export_and_backup_names() {
        let d = "orca-data.json";
        assert!(is_retained_artifact(
            "orca-data.json.sqlite-export.1.json",
            d
        ));
        assert!(is_retained_artifact(
            "orca-data.json.sqlite-export.42.json",
            d
        ));
        assert!(is_retained_artifact("profile-state.db.backup.17-x.db", d));
        assert!(!is_retained_artifact(
            "orca-data.json.sqlite-export.0.json",
            d
        ));
        assert!(!is_retained_artifact(
            "orca-data.json.sqlite-export..json",
            d
        ));
        assert!(!is_retained_artifact(
            "orca-data.json.sqlite-export.1.jsonx",
            d
        ));
        assert!(!is_retained_artifact("profile-state.db", d));
        assert!(!is_retained_artifact("profile-state.db.backup..db", d));
        assert!(!is_retained_artifact("orca-data.json", d));
    }
    use serde_json::json;

    fn env(os: HostOs) -> HostEnv {
        HostEnv::for_test(Path::new("/Users/example"), os)
    }

    #[test]
    fn per_os_defaults() {
        let home = Path::new("/Users/example");
        assert_eq!(
            default_user_data(HostOs::MacOs, home, None, None),
            PathBuf::from("/Users/example/Library/Application Support/orca")
        );
        assert_eq!(
            default_user_data(HostOs::Linux, home, None, None),
            PathBuf::from("/Users/example/.config/orca")
        );
        assert_eq!(
            default_user_data(HostOs::Linux, home, Some("/xdg"), None),
            PathBuf::from("/xdg/orca")
        );
        assert_eq!(
            default_user_data(HostOs::Linux, home, Some("  "), None),
            PathBuf::from("/Users/example/.config/orca")
        );
        assert_eq!(
            default_user_data(HostOs::Windows, home, None, Some("/appdata")),
            PathBuf::from("/appdata/orca")
        );
    }

    #[test]
    fn wsl_detection() {
        assert!(is_wsl(HostOs::Linux, Some("Ubuntu"), None));
        assert!(is_wsl(
            HostOs::Linux,
            None,
            Some("Linux version 5.15.0-microsoft-standard-WSL2")
        ));
        assert!(!is_wsl(HostOs::Linux, None, Some("Linux version 6.1.0")));
        assert!(!is_wsl(HostOs::MacOs, Some("Ubuntu"), None));
    }

    #[test]
    fn hint_is_used_only_when_default_or_live() {
        let mut e = env(HostOs::MacOs);
        let default = PathBuf::from("/Users/example/Library/Application Support/orca");
        assert_eq!(resolve(&e, |_| true).hint, HintUse::Absent);

        e.orca_user_data_path = Some(default.to_string_lossy().into_owned());
        let ud = resolve(&e, |_| false);
        assert_eq!(
            (ud.dir.clone(), ud.hint),
            (default.clone(), HintUse::SameAsDefault)
        );

        // The hint must be absolute on this host to be used at all.
        let dev = crate::testenv::abs("/sandbox/orca-dev");
        e.orca_user_data_path = Some(dev.to_string_lossy().into_owned());
        let ud = resolve(&e, |_| false);
        assert_eq!((ud.dir, ud.hint), (default.clone(), HintUse::Ignored));
        let ud = resolve(&e, |p| p == dev);
        assert_eq!((ud.dir, ud.hint), (dev.clone(), HintUse::UsedLive));

        // A relative hint is never used.
        e.orca_user_data_path = Some("orca-dev".into());
        assert_eq!(resolve(&e, |_| true).hint, HintUse::Ignored);
    }

    #[test]
    fn wsl_ignores_the_windows_hint_and_blocks_store_access() {
        let mut e = env(HostOs::Linux);
        e.wsl_distro_name = Some("Ubuntu".into());
        e.orca_user_data_path = Some("/mnt/c/Users/example/AppData/Roaming/orca".into());
        let ud = resolve(&e, |_| true);
        assert_eq!(ud.dir, PathBuf::from("/Users/example/.config/orca"));
        assert_eq!(ud.hint, HintUse::IgnoredWsl);
        assert!(ud.wsl && ud.windows_orca_pane);
        assert!(!ud.store_access_allowed());

        // A distro with no Windows Orca link is a Linux host with its own
        // userData.
        e.orca_user_data_path = None;
        let ud = resolve(&e, |_| true);
        assert!(ud.wsl && !ud.windows_orca_pane && ud.store_access_allowed());
    }

    fn profile(id: &str) -> Value {
        json!({
            "id": id, "name": "Work",
            "avatar": {"kind": "initials", "initials": "W", "color": "neutral"},
            "kind": "local", "createdAt": 1, "updatedAt": 2, "lastOpenedAt": 3
        })
    }

    #[test]
    fn bsr_accepts_orcas_default_profile_shape() {
        assert!(is_valid_profile(&profile("local-default")));
        let mut p = profile("local-1");
        p["kind"] = json!("cloud-linked");
        p["cloud"] = json!({});
        assert!(is_valid_profile(&p));
    }

    #[test]
    fn bsr_rejects_each_broken_field() {
        let breakers: Vec<(&str, Value)> = vec![
            ("id", json!("-leading-dash")),
            ("id", json!("")),
            ("id", json!("a".repeat(129))),
            ("id", json!("has space")),
            ("id", json!(7)),
            ("name", json!("")),
            ("kind", json!("remote")),
            ("createdAt", json!("1")),
            ("updatedAt", Value::Null),
            ("lastOpenedAt", json!(true)),
            (
                "avatar",
                json!({"kind": "image", "initials": "W", "color": "neutral"}),
            ),
            (
                "avatar",
                json!({"kind": "initials", "initials": 1, "color": "neutral"}),
            ),
            (
                "avatar",
                json!({"kind": "initials", "initials": "W", "color": "blue"}),
            ),
            ("cloud", Value::Null),
            ("cloud", json!("x")),
        ];
        for (k, v) in breakers {
            let mut p = profile("local-1");
            p[k] = v.clone();
            assert!(!is_valid_profile(&p), "{k}={v} must fail bSr");
        }
        assert!(is_valid_profile(&profile(&"a".repeat(128))));
        assert!(!is_valid_profile(&json!([])));
    }

    #[test]
    fn index_picks_active_else_first_valid() {
        let idx = json!({
            "schemaVersion": 1, "activeProfileId": "local-2",
            "profiles": [profile("local-1"), profile("local-2")]
        });
        let got = parse_profile_index(&idx).unwrap();
        assert_eq!(got.active_profile_id, "local-2");
        assert_eq!(got.profile_ids, vec!["local-1", "local-2"]);

        // Active names an invalid profile → first valid one.
        let mut bad = profile("local-2");
        bad["kind"] = json!("nope");
        let idx = json!({"activeProfileId": "local-2", "profiles": [bad, profile("local-3")]});
        assert_eq!(
            parse_profile_index(&idx).unwrap().active_profile_id,
            "local-3"
        );

        // No valid profile → None (Orca's built-in default).
        let idx = json!({"activeProfileId": "x", "profiles": [{"id": "x"}]});
        assert_eq!(parse_profile_index(&idx), None);
        assert_eq!(parse_profile_index(&json!({"profiles": "x"})), None);
    }

    #[test]
    fn data_file_choice_and_bak_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path();
        // No index → local-default, with the legacy root as a read fallback.
        let c = data_file(ud);
        assert_eq!(c.profile_id, DEFAULT_PROFILE_ID);
        assert!(!c.from_index);
        assert_eq!(c.path, ud.join("profiles/local-default/orca-data.json"));
        assert_eq!(c.legacy_root, Some(ud.join("orca-data.json")));

        // A .bak without the index itself is read, as Orca's
        // readProfileIndex does (and ensureActiveOrcaProfile then rewrites
        // the index from it).
        let good = json!({"activeProfileId": "local-9", "profiles": [profile("local-9")]});
        std::fs::write(ud.join("orca-profile-index.json.bak"), good.to_string()).unwrap();
        assert_eq!(data_file(ud).profile_id, "local-9");

        // A corrupt index falls back to its .bak.
        std::fs::write(ud.join(PROFILE_INDEX_FILE), "{not json").unwrap();
        let c = data_file(ud);
        assert_eq!(c.profile_id, "local-9");
        assert!(c.from_index);
        assert_eq!(c.legacy_root, None);

        // A valid index wins.
        let idx = json!({"activeProfileId": "local-5", "profiles": [profile("local-5")]});
        std::fs::write(ud.join(PROFILE_INDEX_FILE), idx.to_string()).unwrap();
        assert_eq!(
            data_file(ud).path,
            ud.join("profiles/local-5/orca-data.json")
        );
    }

    #[test]
    fn the_late_userdata_rule() {
        assert_eq!(
            late_sibling(Path::new("/home/example/.config/orca")),
            Some(PathBuf::from("/home/example/.config/Orca"))
        );
        assert_eq!(late_sibling(Path::new("/tmp/sandbox-ud")), None);
        assert_eq!(late_sibling(Path::new("/home/example/.config/Orca")), None);
        for os in [HostOs::MacOs, HostOs::Linux] {
            for p in [LateProbe::OtherDir, LateProbe::CanonicalMissing] {
                assert!(use_late_sibling(os, p), "{os:?} {p:?}");
            }
            // The canonical dir exists and `Orca` does not: Orca's late
            // userData is the canonical dir (it would have created
            // `Orca/daemon` otherwise). Nothing on disk: no stash yet.
            for p in [
                LateProbe::SameDir,
                LateProbe::LateMissing,
                LateProbe::Unknown,
            ] {
                assert!(!use_late_sibling(os, p), "{os:?} {p:?}");
            }
        }
        for p in [
            LateProbe::SameDir,
            LateProbe::OtherDir,
            LateProbe::LateMissing,
            LateProbe::CanonicalMissing,
            LateProbe::Unknown,
        ] {
            assert!(!use_late_sibling(HostOs::Windows, p), "{p:?}");
        }
        assert!(other_dir_uses_late(false, true));
        assert!(other_dir_uses_late(true, true));
        assert!(other_dir_uses_late(false, false));
        assert!(!other_dir_uses_late(true, false));
        // A fresh host with neither dir keeps the canonical one.
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("fresh").join("orca");
        assert_eq!(late_user_data_for(HostOs::Linux, &ud), ud);
        assert_eq!(late_user_data_for(HostOs::Windows, &ud), ud);
    }

    /// Round 8: a Linux host where Orca keeps its stashes under the
    /// canonical `~/.config/orca` and no `~/.config/Orca` exists must look
    /// for them there, and a stray `Orca` dir without `claude-accounts`
    /// does not move the root either.
    #[test]
    fn stashes_under_the_canonical_dir_stay_there() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join(".config").join("orca");
        std::fs::create_dir_all(ud.join("claude-accounts").join("id-1").join("auth")).unwrap();
        std::fs::create_dir_all(ud.join("daemon")).unwrap();
        assert_eq!(late_user_data_for(HostOs::Linux, &ud), ud);
        assert_eq!(claude_accounts_root(&ud), ud.join("claude-accounts"));
        let late = dir.path().join(".config").join("Orca");
        std::fs::create_dir_all(late.join("logs")).unwrap();
        assert_eq!(late_user_data_for(HostOs::Linux, &ud), ud);
    }
}
