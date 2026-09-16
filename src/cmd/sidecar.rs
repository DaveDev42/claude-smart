//! `csm sidecar {read|write|merge|flags} <sid> [key=value...]`.

use std::ffi::OsString;

use anyhow::Context as _;

use crate::paths;
use crate::sidecar::{merge_sidecar, read_sidecar, write_sidecar, Sidecar};

/// `csm sidecar {read|write|merge|flags} <sid> [key=value...]`
pub(crate) fn cmd_sidecar(args: &[OsString]) -> anyhow::Result<()> {
    let op = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| {
            anyhow::anyhow!("csm sidecar: operation required (read|write|merge|flags)")
        })?;
    let sid = args
        .get(1)
        .map(|a| a.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("csm sidecar: session id required"))?;
    let path = paths::sidecar(&sid);

    match op.as_str() {
        "read" => {
            let s = read_sidecar(&path)?;
            println!("{}", serde_json::to_string(&s)?);
        }
        "write" => {
            let patch = parse_sidecar_kv_args(&args[2..])?;
            write_sidecar(&path, &patch)?;
        }
        "merge" => {
            let patch = parse_sidecar_kv_args(&args[2..])?;
            merge_sidecar(&path, &patch)?;
        }
        "flags" => {
            let s = read_sidecar(&path)?;
            let flags = s.sidecar_flags();
            // Print each flag pair on its own line for shell consumption.
            let mut i = 0;
            while i < flags.len() {
                if i + 1 < flags.len() {
                    println!(
                        "{} {}",
                        flags[i].to_string_lossy(),
                        flags[i + 1].to_string_lossy()
                    );
                    i += 2;
                } else {
                    println!("{}", flags[i].to_string_lossy());
                    i += 1;
                }
            }
        }
        other => {
            anyhow::bail!("csm sidecar: unknown operation {other:?} — use read|write|merge|flags")
        }
    }
    Ok(())
}

/// Parse `key=value` args into a `Sidecar` patch for `write` / `merge`.
///
/// Recognised keys: `session_id`, `permission_mode`, `effort`, `model`,
/// `cwd`, `profile`, `hop`.
fn parse_sidecar_kv_args(args: &[OsString]) -> anyhow::Result<Sidecar> {
    let mut patch = Sidecar::default();
    for arg in args {
        let s = arg.to_string_lossy();
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("csm sidecar: expected key=value, got {s:?}"))?;
        match key {
            "session_id" | "sessionId" => patch.session_id = Some(value.to_owned()),
            "permission_mode" | "permissionMode" => patch.permission_mode = Some(value.to_owned()),
            "effort" => patch.effort = Some(value.to_owned()),
            "model" => patch.model = Some(value.to_owned()),
            "cwd" => patch.cwd = Some(value.to_owned()),
            "profile" => patch.profile = Some(value.to_owned()),
            "hop" => {
                let n: i64 = value.parse().with_context(|| {
                    format!("csm sidecar: hop must be an integer, got {value:?}")
                })?;
                // Store as a JSON Number (the canonical Rust-binary form; the
                // legacy sidecar writer used a string, so readers tolerate both).
                patch.hop = Some(serde_json::Value::Number(serde_json::Number::from(n)));
            }
            other => anyhow::bail!("csm sidecar: unknown key {other:?}"),
        }
    }
    Ok(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn parse_sidecar_kv_permission_mode() {
        let args = argv(&["permission_mode=bypassPermissions"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.permission_mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn parse_sidecar_kv_effort() {
        let args = argv(&["effort=max"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.effort.as_deref(), Some("max"));
    }

    #[test]
    fn parse_sidecar_kv_hop() {
        let args = argv(&["hop=1"]);
        let patch = parse_sidecar_kv_args(&args).unwrap();
        assert_eq!(patch.hop_int(), 1);
    }

    #[test]
    fn parse_sidecar_kv_hop_invalid() {
        let args = argv(&["hop=notanumber"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }

    #[test]
    fn parse_sidecar_kv_unknown_key_errors() {
        let args = argv(&["unknownkey=value"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }

    #[test]
    fn parse_sidecar_kv_no_equals_errors() {
        let args = argv(&["permission_mode"]);
        assert!(parse_sidecar_kv_args(&args).is_err());
    }
}
