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
//! call; Linux the `version` of the `package.json` packed in
//! `resources/app.asar` (the Electron archive every Linux package of Orca
//! ships: `.deb`, AppImage, tarball), read from the archive's JSON header
//! without unpacking it.
//!
//! The gate is `major.minor` (binding operator decision). Orca 1.4.214
//! changed the store inside that range: a profile may keep its state in
//! SQLite, with `orca-data.json` reduced to a hash-pinned export. That
//! change is handled by what is on disk, not by the version: an offline
//! write to such a profile goes to `profile-state.db`
//! ([`super::statedb`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use super::HostEnv;
use super::userdata::HostOs;

/// The `major.minor` versions csm's e2e has run against.
pub const TESTED: &[&str] = &["1.4"];

/// Cap on `Info.plist`.
const PLIST_CAP: u64 = 1024 * 1024;

/// Cap on an `app.asar` header (Orca 1.4.218's is 1.3 MiB).
const ASAR_HEADER_CAP: u64 = 64 * 1024 * 1024;

/// Cap on the packed `package.json`.
const ASAR_PACKAGE_CAP: u64 = 1024 * 1024;

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

/// The UTF-16LE key `VS_VERSION_INFO` that heads a `VS_VERSIONINFO`
/// resource.
const VS_VERSION_INFO_KEY: &[u8] = b"V\0S\0_\0V\0E\0R\0S\0I\0O\0N\0_\0I\0N\0F\0O\0\0\0";

/// The fixed file version (`a.b.c.d`) of a `VS_VERSIONINFO` resource in
/// `bytes` (a PE image's resource section). Only a `VS_FIXEDFILEINFO` that
/// sits where the structure puts it counts: after the `VS_VERSION_INFO`
/// key, 32-bit aligned (`wLength`, `wValueLength`, `wType`, the 16-WCHAR
/// key, one padding WORD: 40 bytes from the structure's start). A bare
/// `0xFEEF04BD` is not enough: Chromium's code carries that constant, and a
/// scan for it alone read a version out of Orca.exe's `.text`. Pure.
pub fn fixed_file_version(bytes: &[u8]) -> Option<String> {
    const SIG: [u8; 4] = 0xFEEF_04BDu32.to_le_bytes();
    const STRUC: [u8; 4] = 0x0001_0000u32.to_le_bytes();
    let word = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let mut from = 0;
    while let Some(hit) = bytes[from..]
        .windows(VS_VERSION_INFO_KEY.len())
        .position(|w| w == VS_VERSION_INFO_KEY)
    {
        let key = from + hit;
        from = key + 1;
        // The structure starts 6 bytes before its key; its value at +40.
        let Some(start) = key.checked_sub(6) else {
            continue;
        };
        let at = start + 40;
        if at + 16 > bytes.len() || bytes[at..at + 4] != SIG || bytes[at + 4..at + 8] != STRUC {
            continue;
        }
        let (ms, ls) = (word(at + 8), word(at + 12));
        return Some(format!(
            "{}.{}.{}.{}",
            ms >> 16,
            ms & 0xffff,
            ls >> 16,
            ls & 0xffff
        ));
    }
    None
}

/// The file range (offset, length) of a PE image's `.rsrc` section, from
/// its headers: the DOS header's `e_lfanew`, the `PE\0\0` signature, the
/// COFF header's section count and optional-header size, then the section
/// table. `head` must hold the headers (the first 4 KiB do). Pure.
pub fn rsrc_section(head: &[u8]) -> Option<(u64, u64)> {
    let u16_at = |o: usize| -> Option<u16> {
        Some(u16::from_le_bytes(head.get(o..o + 2)?.try_into().ok()?))
    };
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(head.get(o..o + 4)?.try_into().ok()?))
    };
    if head.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if head.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = usize::from(u16_at(pe + 6)?);
    let optional = usize::from(u16_at(pe + 20)?);
    let table = pe + 24 + optional;
    (0..sections).find_map(|i| {
        let e = table + i * 40;
        let name = head.get(e..e + 8)?;
        if !name.starts_with(b".rsrc\0") {
            return None;
        }
        let size = u64::from(u32_at(e + 16)?);
        let offset = u64::from(u32_at(e + 20)?);
        (size > 0).then_some((offset, size))
    })
}

/// Where the archive's header JSON lies and where file data starts, from
/// the archive's first 16 bytes (Chromium pickles: `u32 4`, `u32 header
/// pickle size`, `u32 payload size`, `u32 string length`). Pure.
fn asar_layout(head: &[u8; 16]) -> Option<(u64, u64)> {
    let word = |o: usize| {
        u64::from(u32::from_le_bytes([
            head[o],
            head[o + 1],
            head[o + 2],
            head[o + 3],
        ]))
    };
    if word(0) != 4 {
        return None;
    }
    let header_size = word(4);
    let json_len = word(12);
    if json_len == 0 || json_len + 8 > header_size {
        return None;
    }
    // (json length, data base offset)
    Some((json_len, 8 + header_size))
}

/// The offset and size of `package.json` at the archive root, from the
/// header JSON. Pure.
fn asar_package_entry(header: &[u8]) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_slice(header).ok()?;
    let e = v.get("files")?.get("package.json")?;
    let size = e.get("size")?.as_u64()?;
    let offset = match e.get("offset")? {
        serde_json::Value::String(s) => s.parse().ok()?,
        n => n.as_u64()?,
    };
    Some((offset, size))
}

/// The `version` of a `package.json`. Pure.
fn package_version(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let ver = v.get("version")?.as_str()?.trim();
    (!ver.is_empty()).then(|| ver.to_owned())
}

/// Read the version packed in an Electron `app.asar`.
pub fn read_asar_version(asar: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(asar).ok()?;
    let mut head = [0u8; 16];
    f.read_exact(&mut head).ok()?;
    let (json_len, base) = asar_layout(&head)?;
    if json_len > ASAR_HEADER_CAP {
        return None;
    }
    let mut header = vec![0u8; usize::try_from(json_len).ok()?];
    f.read_exact(&mut header).ok()?;
    let (offset, size) = asar_package_entry(&header)?;
    if size > ASAR_PACKAGE_CAP {
        return None;
    }
    f.seek(SeekFrom::Start(base.checked_add(offset)?)).ok()?;
    let mut pkg = vec![0u8; usize::try_from(size).ok()?];
    f.read_exact(&mut pkg).ok()?;
    package_version(&pkg)
}

/// `resources/app.asar` beside a Linux Orca executable. Pure.
fn asar_beside(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    // The `.deb`'s `/usr/bin/orca-ide` resolves to `<app>/resources/bin/orca-ide`.
    if dir.file_name().is_some_and(|n| n == "bin")
        && let Some(res) = dir.parent()
        && res.file_name().is_some_and(|n| n == "resources")
    {
        return Some(res.join("app.asar"));
    }
    Some(dir.join("resources").join("app.asar"))
}

/// Read the version from a macOS `.app` bundle.
pub fn read_bundle_version(bundle: &Path) -> Option<String> {
    let plist = bundle.join("Contents").join("Info.plist");
    let text = super::read_capped(&plist, PLIST_CAP).ok()??;
    plist_short_version(&text)
}

/// Cap on the `.rsrc` section csm reads (Orca's is a few hundred KiB:
/// icons and the version resource).
const RSRC_CAP: u64 = 64 * 1024 * 1024;

/// Read the fixed file version of a Windows executable from its `.rsrc`
/// section (the PE headers say where it lies; nothing else is read).
pub fn read_exe_version(exe: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(exe).ok()?;
    let mut head = Vec::with_capacity(4096);
    (&mut f).take(4096).read_to_end(&mut head).ok()?;
    let (offset, size) = rsrc_section(&head)?;
    if size > RSRC_CAP {
        return None;
    }
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut rsrc = Vec::with_capacity(size as usize);
    f.take(size).read_to_end(&mut rsrc).ok()?;
    fixed_file_version(&rsrc)
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
        HostOs::Linux => {
            out.extend(main_exe.and_then(asar_beside));
            out.extend(system_linux_installs().iter().filter_map(|e| {
                let resolved = std::fs::canonicalize(e).unwrap_or_else(|_| e.clone());
                asar_beside(&resolved)
            }));
        }
    }
    out
}

/// Conventional Linux install locations (the `.deb` puts Orca in
/// `/opt/Orca` and links `/usr/bin/orca-ide`). None under test or e2e, so
/// neither reads the machine's real install.
#[cfg(not(test))]
fn system_linux_installs() -> Vec<PathBuf> {
    if crate::e2e::ENABLED {
        return Vec::new();
    }
    vec![
        PathBuf::from("/opt/Orca/orca-ide"),
        PathBuf::from("/usr/bin/orca-ide"),
    ]
}

#[cfg(test)]
fn system_linux_installs() -> Vec<PathBuf> {
    Vec::new()
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
            HostOs::Linux => read_asar_version(&c),
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

    /// A `VS_VERSIONINFO` structure (header, key, padding, fixed info).
    fn version_info(ms: u32, ls: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(0x0100u16.to_le_bytes()); // wLength (not checked)
        b.extend(52u16.to_le_bytes()); // wValueLength
        b.extend(0u16.to_le_bytes()); // wType
        b.extend(VS_VERSION_INFO_KEY);
        b.extend([0u8; 2]); // padding to 32 bits
        b.extend(0xFEEF_04BDu32.to_le_bytes());
        b.extend(0x0001_0000u32.to_le_bytes());
        b.extend(ms.to_le_bytes());
        b.extend(ls.to_le_bytes());
        b.extend([0u8; 36]);
        b
    }

    /// A PE image: headers, a `.text` section carrying a decoy fixed-info
    /// signature (as Chromium's code does), and a `.rsrc` section at
    /// `rsrc_at` holding the version resource.
    fn pe_image(rsrc_at: usize, ms: u32, ls: u32) -> Vec<u8> {
        let mut rsrc = vec![0u8; 64];
        rsrc.extend(version_info(ms, ls));
        let mut b = vec![0u8; rsrc_at + rsrc.len()];
        b[0..2].copy_from_slice(b"MZ");
        let pe = 0x80usize;
        b[0x3c..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
        b[pe..pe + 4].copy_from_slice(b"PE\0\0");
        b[pe + 6..pe + 8].copy_from_slice(&2u16.to_le_bytes());
        let optional = 0xF0usize;
        b[pe + 20..pe + 22].copy_from_slice(&(optional as u16).to_le_bytes());
        let table = pe + 24 + optional;
        for (i, (name, offset, size)) in [
            (&b".text"[..], 0x400usize, 0x100usize),
            (&b".rsrc"[..], rsrc_at, rsrc.len()),
        ]
        .into_iter()
        .enumerate()
        {
            let e = table + i * 40;
            b[e..e + name.len()].copy_from_slice(name);
            b[e + 16..e + 20].copy_from_slice(&(size as u32).to_le_bytes());
            b[e + 20..e + 24].copy_from_slice(&(offset as u32).to_le_bytes());
        }
        // The decoy: a bare signature and struct version in `.text`.
        let d = 0x410;
        b[d..d + 4].copy_from_slice(&0xFEEF_04BDu32.to_le_bytes());
        b[d + 4..d + 8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        b[d + 8..d + 12].copy_from_slice(&0x247C_8D48u32.to_le_bytes());
        b[rsrc_at..].copy_from_slice(&rsrc);
        b
    }

    #[test]
    fn fixed_file_version_needs_the_version_info_key() {
        let mut b = vec![0u8; 100];
        b.extend(version_info((1 << 16) | 4, 209 << 16));
        assert_eq!(fixed_file_version(&b).as_deref(), Some("1.4.209.0"));
        // A bare signature (Chromium's code carries the constant) is not a
        // version resource.
        let mut decoy = vec![0u8; 16];
        decoy.extend(0xFEEF_04BDu32.to_le_bytes());
        decoy.extend(0x0001_0000u32.to_le_bytes());
        decoy.extend(0x247C_8D48u32.to_le_bytes());
        decoy.extend([0u8; 32]);
        assert_eq!(fixed_file_version(&decoy), None);
        assert_eq!(fixed_file_version(&[0u8; 64]), None);
        // The key at the very start (no room for the header) is skipped.
        assert_eq!(fixed_file_version(VS_VERSION_INFO_KEY), None);
    }

    #[test]
    fn exe_version_comes_from_the_rsrc_section_only() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("Orca.exe");
        std::fs::write(&exe, pe_image(0x2000, (1 << 16) | 4, 220 << 16)).unwrap();
        assert_eq!(read_exe_version(&exe).as_deref(), Some("1.4.220.0"));
        assert_eq!(
            rsrc_section(&pe_image(0x2000, 0, 0)).map(|(o, _)| o),
            Some(0x2000)
        );
        // Not a PE image: nothing.
        std::fs::write(&exe, version_info((1 << 16) | 4, 220 << 16)).unwrap();
        assert_eq!(read_exe_version(&exe), None);
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

    /// An `app.asar` laid out the way Electron's packer writes it.
    fn asar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut entries = serde_json::Map::new();
        let mut data = Vec::new();
        for (name, body) in files {
            entries.insert(
                (*name).to_owned(),
                serde_json::json!({"size": body.len(), "offset": data.len().to_string()}),
            );
            data.extend_from_slice(body);
        }
        let json = serde_json::to_vec(&serde_json::json!({"files": entries})).unwrap();
        let padded = json.len().div_ceil(4) * 4;
        let mut out = Vec::new();
        out.extend(4u32.to_le_bytes());
        out.extend(((padded + 8) as u32).to_le_bytes());
        out.extend(((padded + 4) as u32).to_le_bytes());
        out.extend((json.len() as u32).to_le_bytes());
        out.extend(&json);
        out.resize(16 + padded, 0);
        out.extend(data);
        out
    }

    #[test]
    fn linux_reads_the_version_packed_in_app_asar() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("opt/Orca");
        std::fs::create_dir_all(app.join("resources/bin")).unwrap();
        std::fs::write(
            app.join("resources/app.asar"),
            asar(&[
                ("index.js", b"console.log(1)"),
                ("package.json", br#"{"name":"orca","version":"1.4.218"}"#),
            ]),
        )
        .unwrap();
        let env = HostEnv::for_test(dir.path(), HostOs::Linux);
        // The running main's executable, and the `/usr/bin` link's target.
        for exe in [app.join("orca-ide"), app.join("resources/bin/orca-ide")] {
            let v = detect(&env, Some(&exe));
            assert_eq!(v.version.as_deref(), Some("1.4.218"), "{}", exe.display());
            assert_eq!(
                v.source.as_deref(),
                Some(app.join("resources/app.asar").as_path())
            );
            assert!(v.in_tested_range());
        }
        // No candidate when Orca is stopped under test (system paths are off).
        assert_eq!(detect(&env, None), OrcaVersion::unknown());
    }

    #[test]
    fn a_malformed_asar_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.asar");
        for bytes in [
            b"".to_vec(),
            b"not an asar archive at all".to_vec(),
            asar(&[("index.js", b"x")]),
            asar(&[("package.json", b"{\"name\":\"orca\"}")]),
            asar(&[("package.json", b"{not json")]),
        ] {
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_asar_version(&p), None);
        }
        assert_eq!(read_asar_version(&dir.path().join("missing")), None);
    }
}
