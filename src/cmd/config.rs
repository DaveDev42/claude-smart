//! `csm config` — csm's own global config (`~/.config/claude-smart/
//! config.json`), distinct from the `~/.config/claude-as/` profile contract.

use std::ffi::OsString;

use anyhow::Context as _;

use crate::config;

/// A key `csm config get|set|unset` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    LaunchCommand,
    /// `orca.followSwitch` (bool).
    OrcaFollowSwitch,
    /// `orca.userDataDir` (path).
    OrcaUserDataDir,
}

const KEYS_HINT: &str = "launch-command | orca.follow-switch | orca.user-data-dir";

/// Map a CLI key to a [`Key`]. `orca.slot-profile` is refused on every verb:
/// only `csm orca init` / `csm orca disable` may change it (they also move
/// the floor and provision the slot, which a bare config write would skip).
fn parse_key(key: Option<&str>, verb: &str) -> anyhow::Result<Key> {
    match key {
        Some("launch-command") => Ok(Key::LaunchCommand),
        Some("orca.follow-switch") => Ok(Key::OrcaFollowSwitch),
        Some("orca.user-data-dir") => Ok(Key::OrcaUserDataDir),
        Some("orca.slot-profile") => anyhow::bail!(
            "csm config {verb}: orca.slot-profile is managed by `csm orca init` / \
             `csm orca disable` (see `csm orca status`)"
        ),
        Some(other) => {
            anyhow::bail!("csm config {verb}: unknown key '{other}' (expected {KEYS_HINT})")
        }
        None => anyhow::bail!("csm config {verb}: missing key (expected {KEYS_HINT})"),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Apply `set <key> <rest…>` to `cfg`. Pure.
fn apply_set(cfg: &mut config::Config, key: Key, rest: &[String]) -> anyhow::Result<()> {
    match key {
        Key::LaunchCommand => {
            if rest.is_empty() {
                anyhow::bail!(
                    "csm config set launch-command: missing command \
                     (e.g. `csm config set launch-command happy`)"
                );
            }
            cfg.launch_command = rest.to_vec();
        }
        Key::OrcaFollowSwitch => {
            let [v] = rest else {
                anyhow::bail!("csm config set orca.follow-switch: expected one value (true|false)");
            };
            cfg.orca.follow_switch = parse_bool(v).with_context(|| {
                format!("csm config set orca.follow-switch: '{v}' is not true|false")
            })?;
        }
        Key::OrcaUserDataDir => {
            let [v] = rest else {
                anyhow::bail!("csm config set orca.user-data-dir: expected one path");
            };
            let v = v.trim();
            if !(std::path::Path::new(v).is_absolute() || v.starts_with("~/")) {
                anyhow::bail!(
                    "csm config set orca.user-data-dir: '{v}' must be an absolute path (or ~/…)"
                );
            }
            cfg.orca.user_data_dir = Some(v.to_owned());
        }
    }
    Ok(())
}

/// Apply `unset <key>` to `cfg`. Pure.
fn apply_unset(cfg: &mut config::Config, key: Key) {
    match key {
        Key::LaunchCommand => cfg.launch_command.clear(),
        Key::OrcaFollowSwitch => cfg.orca.follow_switch = false,
        Key::OrcaUserDataDir => cfg.orca.user_data_dir = None,
    }
}

/// Load the config for a rewrite. Absent → defaults; a file that exists but
/// fails to parse is REFUSED — overwriting it would silently drop whatever
/// it held (the Orca slot included).
fn load_for_write(verb: &str) -> anyhow::Result<config::Config> {
    config::Config::load().with_context(|| {
        format!(
            "csm config {verb}: config.json exists but could not be read; refusing to \
             overwrite it (fix or remove {})",
            crate::paths::config_json().display()
        )
    })
}

/// `csm config <verb> ...`
///
/// Verbs (noun-verb, mirroring `cmd_profiles`):
///   show                         print the config JSON (bare `csm config` ≡ show)
///   get   <key>                  print the effective value
///   set   <key> <value>...       write a value
///   unset <key>                  clear a value (back to the default)
///
/// Keys: `launch-command` (argv tokens), `orca.follow-switch` (bool),
/// `orca.user-data-dir` (path). `orca.slot-profile` is refused — `csm orca
/// init`/`disable` own it.
pub(crate) fn cmd_config(args: &[OsString]) -> anyhow::Result<()> {
    let verb = args.first().map(|a| a.to_string_lossy().into_owned());
    let key = args.get(1).map(|a| a.to_string_lossy().into_owned());
    // Remaining tokens (after `<verb> <key>`) are the value.
    let rest: Vec<String> = args
        .iter()
        .skip(2)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    match verb.as_deref() {
        None | Some("show") => {
            let cfg = config::Config::load().context("csm config: failed to load config.json")?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        Some("get") => match parse_key(key.as_deref(), "get")? {
            Key::LaunchCommand => {
                // Print the EFFECTIVE launch command (env > config > "claude"),
                // space-joined, so users see what `csm run` will actually spawn.
                let tokens = config::resolve_launch_command();
                let joined: Vec<String> = tokens
                    .iter()
                    .map(|t| t.to_string_lossy().into_owned())
                    .collect();
                println!("{}", joined.join(" "));
            }
            Key::OrcaFollowSwitch => {
                let cfg =
                    config::Config::load().context("csm config: failed to load config.json")?;
                println!("{}", cfg.orca().follow_switch);
            }
            Key::OrcaUserDataDir => {
                // The EFFECTIVE userData dir (config > $ORCA_USER_DATA_PATH >
                // platform default).
                let cfg =
                    config::Config::load().context("csm config: failed to load config.json")?;
                if let Some(dir) = crate::orca::user_data_dir_for(cfg.orca()) {
                    println!("{}", dir.display());
                }
            }
        },
        Some("set") => {
            let k = parse_key(key.as_deref(), "set")?;
            let mut cfg = load_for_write("set")?;
            apply_set(&mut cfg, k, &rest)?;
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some("unset") => {
            let k = parse_key(key.as_deref(), "unset")?;
            let mut cfg = load_for_write("unset")?;
            apply_unset(&mut cfg, k);
            cfg.save()
                .context("csm config: failed to write config.json")?;
        }
        Some(other) => {
            anyhow::bail!("csm config: unknown verb '{other}' (expected show|get|set|unset)");
        }
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn slot_profile_is_refused_on_every_verb() {
        for verb in ["get", "set", "unset"] {
            let e = parse_key(Some("orca.slot-profile"), verb).unwrap_err();
            assert!(e.to_string().contains("csm orca init"), "{e}");
        }
        assert!(parse_key(Some("nope"), "get").is_err());
        assert!(parse_key(None, "get").is_err());
    }

    #[test]
    fn orca_keys_set_and_unset() {
        let mut cfg = config::Config::default();
        apply_set(&mut cfg, Key::OrcaFollowSwitch, &s(&["on"])).unwrap();
        assert!(cfg.orca.follow_switch);
        assert!(apply_set(&mut cfg, Key::OrcaFollowSwitch, &s(&["maybe"])).is_err());
        assert!(apply_set(&mut cfg, Key::OrcaFollowSwitch, &s(&[])).is_err());
        apply_unset(&mut cfg, Key::OrcaFollowSwitch);
        assert!(!cfg.orca.follow_switch);

        // `Path::is_absolute` needs a drive letter on Windows.
        let abs = if cfg!(windows) {
            r"C:\Users\example\od"
        } else {
            "/Users/example/od"
        };
        apply_set(&mut cfg, Key::OrcaUserDataDir, &s(&[abs])).unwrap();
        assert_eq!(cfg.orca.user_data_dir.as_deref(), Some(abs));
        apply_set(&mut cfg, Key::OrcaUserDataDir, &s(&["~/od"])).unwrap();
        assert!(apply_set(&mut cfg, Key::OrcaUserDataDir, &s(&["relative"])).is_err());
        apply_unset(&mut cfg, Key::OrcaUserDataDir);
        assert_eq!(cfg.orca.user_data_dir, None);
    }

    #[test]
    fn orca_writes_never_touch_the_slot() {
        let mut cfg = config::Config::default();
        cfg.orca.slot_profile = Some("orca".into());
        apply_set(&mut cfg, Key::OrcaFollowSwitch, &s(&["true"])).unwrap();
        apply_unset(&mut cfg, Key::OrcaUserDataDir);
        apply_set(&mut cfg, Key::LaunchCommand, &s(&["happy"])).unwrap();
        assert_eq!(cfg.orca.slot_profile.as_deref(), Some("orca"));
    }

    #[test]
    fn a_corrupt_config_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let path = crate::paths::config_json();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{ not json").unwrap();
            let args: Vec<OsString> = ["set", "orca.follow-switch", "true"]
                .iter()
                .map(OsString::from)
                .collect();
            assert!(cmd_config(&args).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        });
    }
}
