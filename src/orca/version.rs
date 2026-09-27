//! The installed Orca version and csm's tested range.
//!
//! csm ports Orca's private store format and account logic, so offline
//! writes are allowed only for Orca versions csm's e2e has run against
//! ([`TESTED`], `major.minor`). An unknown or untested version means RPC
//! only (design section 2).
//!
//! Sources: macOS `CFBundleShortVersionString` in `<bundle>/Contents/Info.plist`
//! (XML; a binary plist reads as unknown); Windows the fixed file version of
//! `Orca.exe`, read from its `VS_FIXEDFILEINFO` resource without any Win32
//! call; Linux has no reliable source yet (INFERRED package metadata), so
//! it is unknown and counts as out of range.
//!
//! The gate is `major.minor` (binding operator decision). Orca 1.4.214
//! changed the store inside that range: a profile may keep its state in
//! SQLite, with `orca-data.json` reduced to a hash-pinned export. That
//! change is gated by what is on disk, not by the version: every offline
//! store write refuses a profile whose `profile-state.db` family exists
//! ([`super::store::sqlite_gate`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use super::HostEnv;
use super::userdata::HostOs;

/// The `major.minor` versions csm's e2e has run against.
pub const TESTED: &[&str] = &["1.4"];

/// Cap on `Info.plist`.
const PLIST_CAP: u64 = 1024 * 1024;

/// What csm knows about the installed Orca.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaVersion {
    /// The version text, when read.
    pub version: Option<String>,
    /// Where it was read from.
    pub source: Option<PathBuf>,
}

impl OrcaVersion {
    pub fn unknown() -> OrcaVersion {
        OrcaVersion {
            version: None,
            source: None,
        }
    }

    /// Is the version in [`TESTED`]?
    pub fn in_tested_range(&self) -> bool {
        self.version.as_deref().is_some_and(in_tested_range)
    }
}

/// `major.minor` of a version string. Pure.
pub fn major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor_text = parts.next()?;
    let digits: String = minor_text
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, digits.parse().ok()?))
}

/// Is `v` in [`TESTED`]? Pure.
pub fn in_tested_range(v: &str) -> bool {
    let Some((maj, min)) = major_minor(v) else {
        return false;
    };
    TESTED
        .iter()
        .filter_map(|t| major_minor(t))
        .any(|t| t == (maj, min))
}

/// `CFBundleShortVersionString` from an XML plist. Pure.
pub fn plist_short_version(text: &str) -> Option<String> {
    let key = "<key>CFBundleShortVersionString</key>";
    let after = &text[text.find(key)? + key.len()..];
    let after = after.trim_start();
    let body = after.strip_prefix("<string>")?;
    let end = body.find("</string>")?;
    let v = body[..end].trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// The fixed file version (`a.b.c.d`) from a PE image's `VS_FIXEDFILEINFO`.
/// Pure over the bytes.
pub fn fixed_file_version(bytes: &[u8]) -> Option<String> {
    const SIG: [u8; 4] = 0xFEEF_04BDu32.to_le_bytes();
    const STRUC: [u8; 4] = 0x0001_0000u32.to_le_bytes();
    let mut i = 0;
    while i + 16 <= bytes.len() {
        let hit = bytes[i..].windows(4).position(|w| w == SIG)?;
        let at = i + hit;
        if at + 16 <= bytes.len() && bytes[at + 4..at + 8] == STRUC {
            let word =
                |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
            let ms = word(at + 8);
            let ls = word(at + 12);
            return Some(format!(
                "{}.{}.{}.{}",
                ms >> 16,
                ms & 0xffff,
                ls >> 16,
                ls & 0xffff
            ));
        }
        i = at + 1;
    }
    None
}

/// Read the version from a macOS `.app` bundle.
pub fn read_bundle_version(bundle: &Path) -> Option<String> {
    let plist = bundle.join("Contents").join("Info.plist");
    let text = super::read_capped(&plist, PLIST_CAP).ok()??;
    plist_short_version(&text)
}

/// Read the fixed file version of a Windows executable, streaming it.
pub fn read_exe_version(exe: &Path) -> Option<String> {
    let mut f = std::fs::File::open(exe).ok()?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        let mut window = std::mem::take(&mut carry);
        window.extend_from_slice(&buf[..n]);
        if let Some(v) = fixed_file_version(&window) {
            return Some(v);
        }
        // Keep a tail so a structure split across reads is still found.
        let keep = window.len().min(15);
        carry = window[window.len() - keep..].to_vec();
    }
}

/// Where Orca is installed, most specific first: the running main's
/// executable, then the conventional install locations.
pub fn install_candidates(env: &HostEnv, main_exe: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match env.os {
        HostOs::MacOs => {
            if let Some(b) = main_exe.and_then(super::live::bundle_of) {
                out.push(b);
            }
            out.extend(system_app_dirs().into_iter().map(|d| d.join("Orca.app")));
            out.push(env.home.join("Applications").join("Orca.app"));
        }
        HostOs::Windows => {
            if let Some(e) = main_exe {
                out.push(e.to_path_buf());
            }
            if let Some(l) = env.localappdata.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push(
                    PathBuf::from(l)
                        .join("Programs")
                        .join("Orca")
                        .join("Orca.exe"),
                );
            }
        }
        HostOs::Linux => {}
    }
    out
}

/// System-wide app dirs. None under test, so a test never reads the
/// machine's real Orca install.
#[cfg(not(test))]
fn system_app_dirs() -> Vec<PathBuf> {
    // The e2e build never reads the machine's real Orca install.
    if crate::e2e::ENABLED {
        return Vec::new();
    }
    vec![PathBuf::from("/Applications")]
}

#[cfg(test)]
fn system_app_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// The installed Orca's version, from the first candidate that yields one.
pub fn detect(env: &HostEnv, main_exe: Option<&Path>) -> OrcaVersion {
    if let Some(version) = crate::e2e::orca_version() {
        return OrcaVersion {
            version: Some(version),
            source: None,
        };
    }
    for c in install_candidates(env, main_exe) {
        let v = match env.os {
            HostOs::MacOs => read_bundle_version(&c),
            HostOs::Windows => read_exe_version(&c),
            HostOs::Linux => None,
        };
        if let Some(version) = v {
            return OrcaVersion {
                version: Some(version),
                source: Some(c),
            };
        }
    }
    OrcaVersion::unknown()
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>Orca</string>
	<key>CFBundleShortVersionString</key>
	<string>1.4.209</string>
	<key>CFBundleVersion</key>
	<string>1.4.209</string>
</dict>
</plist>"#;

    #[test]
    fn plist_version() {
        assert_eq!(plist_short_version(PLIST).as_deref(), Some("1.4.209"));
        assert_eq!(plist_short_version("<plist/>"), None);
        assert_eq!(
            plist_short_version("<key>CFBundleShortVersionString</key><integer>1</integer>"),
            None
        );
    }

    #[test]
    fn tested_range_is_major_minor() {
        assert_eq!(major_minor("1.4.209"), Some((1, 4)));
        assert_eq!(major_minor("1.4"), Some((1, 4)));
        assert_eq!(major_minor("1.4-beta.2"), Some((1, 4)));
        assert_eq!(major_minor("1"), None);
        assert!(in_tested_range("1.4.209"));
        assert!(in_tested_range("1.4.0.0"));
        assert!(!in_tested_range("1.5.0"));
        assert!(!in_tested_range("1.40.0"));
        assert!(!in_tested_range(""));
        assert!(!OrcaVersion::unknown().in_tested_range());
    }

    fn pe_with_version(prefix: usize, ms: u32, ls: u32) -> Vec<u8> {
        let mut b = vec![0u8; prefix];
        // A lone signature without the struct version is skipped.
        b.extend(0xFEEF_04BDu32.to_le_bytes());
        b.extend(0x0000_0000u32.to_le_bytes());
        b.extend(0xFEEF_04BDu32.to_le_bytes());
        b.extend(0x0001_0000u32.to_le_bytes());
        b.extend(ms.to_le_bytes());
        b.extend(ls.to_le_bytes());
        b.extend([0u8; 32]);
        b
    }

    #[test]
    fn fixed_file_version_scan() {
        let b = pe_with_version(100, (1 << 16) | 4, 209 << 16);
        assert_eq!(fixed_file_version(&b).as_deref(), Some("1.4.209.0"));
        assert_eq!(fixed_file_version(&[0u8; 64]), None);
    }

    #[test]
    fn exe_version_found_across_read_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("Orca.exe");
        // Straddle the 4 MiB read boundary.
        std::fs::write(
            &exe,
            pe_with_version(4 * 1024 * 1024 - 18, (1 << 16) | 4, 7 << 16),
        )
        .unwrap();
        assert_eq!(read_exe_version(&exe).as_deref(), Some("1.4.7.0"));
    }

    #[test]
    fn detect_reads_a_bundle_under_the_test_home() {
        let dir = tempfile::tempdir().unwrap();
        let env = HostEnv::for_test(dir.path(), HostOs::MacOs);
        assert_eq!(detect(&env, None), OrcaVersion::unknown());
        let bundle = dir.path().join("Applications").join("Orca.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents").join("Info.plist"), PLIST).unwrap();
        let v = detect(&env, None);
        assert_eq!(v.version.as_deref(), Some("1.4.209"));
        assert!(v.in_tested_range());
        assert!(
            !install_candidates(&env, None)
                .iter()
                .any(|c| c.starts_with("/Applications")),
            "system dirs are never read under test"
        );
        let env = HostEnv::for_test(dir.path(), HostOs::Linux);
        assert_eq!(detect(&env, None), OrcaVersion::unknown());
    }
}
