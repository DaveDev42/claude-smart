//! The moved cores' tests (from the former `csm migrate plan|import|retire`)
//! and the dispatch tests that prove NONE-class invocations never probe.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::SystemTime;

use serde_json::Value;

use crate::orca::add::{self};
use crate::orca::context::Context;
#[cfg(unix)]
use crate::orca::keychain;
use crate::orca::quarantine::{self, Quarantine, Reason};
use crate::orca::record::AccountRecord;
use crate::orca::{HostOs, OrcaView, SnapshotOptions};

use super::adopt::*;
use super::carry::*;
use super::cutover::*;
use super::legacy::*;
use super::plan::*;
use super::retire::*;
use crate::orca::HostEnv;
use crate::orca::http::FakeHttp;
use crate::orca::testsupport::{
    FakeProcs, creds_json, make_stash, oauth_json, record_json, write_store,
};
use serde_json::json;

fn os(ss: &[&str]) -> Vec<OsString> {
    ss.iter().map(OsString::from).collect()
}

#[test]
fn legacy_floor_comes_from_default_then_floor_dir() {
    let pj = r#"{"work":"/Users/example/.claude.work","home":"/Users/example/.claude.home"}"#;
    let l = parse_legacy(Some(pj), Some("home\n"), None).unwrap();
    assert_eq!(l.floor.as_deref(), Some("home"));
    assert_eq!(l.profiles[0].name, "home");
    let l = parse_legacy(
        Some(pj),
        Some("gone"),
        Some("/Users/example/.claude.work/\n"),
    )
    .unwrap();
    assert_eq!(l.floor.as_deref(), Some("work"));
    assert_eq!(parse_legacy(None, None, None).unwrap(), Legacy::default());
    assert!(parse_legacy(Some("[1]"), None, None).is_err());
}

fn rec(id: &str, email: &str) -> AccountRecord {
    AccountRecord::from_value(&json!({
        "id": id, "email": email, "managedAuthPath": "/x", "authMethod": "subscription-oauth"
    }))
    .unwrap()
}

fn facts(email: Option<&str>, grant: Option<(&str, f64)>) -> ProfileFacts {
    ProfileFacts {
        name: "work".into(),
        dir: "/Users/example/.claude.work".into(),
        exists: true,
        email: email.map(str::to_owned),
        organization_uuid: None,
        grant_sources: if grant.is_some() {
            vec!["file"]
        } else {
            vec![]
        },
        grant: grant.map(|(fp, e)| GrantFacts {
            fingerprint: fp.into(),
            expires_at: Some(e),
            side: BTreeMap::new(),
        }),
    }
}

/// A1 classifies each row again after an import: a second profile of
/// the account just imported is then in Orca, and read back only when its
/// grant is fresher.
#[test]
fn a_second_profile_of_an_account_is_decided_after_the_first_import() {
    let second = facts(Some(" Alice@Example.com "), Some(("fp", 1.0)));
    // Classified again once the first import added the account, the
    // repeat is in Orca: read back only if its grant is fresher.
    let rec = AccountRecord::from_value(&record_json(
        Path::new("/Users/example/orca"),
        "acct-a",
        "alice@example.com",
        None,
    ))
    .unwrap();
    let older = |_: &str| {
        Some(GrantFacts {
            fingerprint: "fp-stash".into(),
            expires_at: Some(5.0),
            side: BTreeMap::new(),
        })
    };
    let st = classify(&second, std::slice::from_ref(&rec), &older);
    assert_eq!(
        st,
        Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false)
        }
    );
    assert_eq!(row_action(&st, false), RowAction::Skip);
}

#[test]
fn classify_each_status() {
    let records = vec![rec("acct-a", "alice@example.com")];
    let none = |_: &str| None;
    assert_eq!(
        classify(&facts(Some("alice@example.com"), None), &records, &none),
        Status::NoCredentials
    );
    assert_eq!(
        classify(
            &facts(Some("bob@example.com"), Some(("f1", 1.0))),
            &records,
            &none
        ),
        Status::ToImport
    );
    assert_eq!(
        classify(&facts(None, Some(("f1", 1.0))), &records, &none),
        Status::ToImport
    );
    assert_eq!(
        classify(
            &facts(Some("Alice@Example.com"), Some(("f1", 1.0))),
            &records,
            &none
        ),
        Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(true)
        }
    );
    let same = |_: &str| {
        Some(GrantFacts {
            fingerprint: "f1".into(),
            expires_at: Some(9.0),
            side: BTreeMap::new(),
        })
    };
    assert_eq!(
        classify(
            &facts(Some("alice@example.com"), Some(("f1", 1.0))),
            &records,
            &same
        ),
        Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false)
        }
    );
    let older = |_: &str| {
        Some(GrantFacts {
            fingerprint: "f0".into(),
            expires_at: Some(5.0),
            side: BTreeMap::new(),
        })
    };
    assert!(matches!(
        classify(
            &facts(Some("alice@example.com"), Some(("f1", 4.0))),
            &records,
            &older
        ),
        Status::InOrca {
            fresher: Some(false),
            ..
        }
    ));
    assert!(matches!(
        classify(
            &facts(Some("alice@example.com"), Some(("f1", 6.0))),
            &records,
            &older
        ),
        Status::InOrca {
            fresher: Some(true),
            ..
        }
    ));
}

/// A presence-only probe knows a grant exists but not whether it is
/// fresher: `fresher` stays unknown and the stash is never consulted.
#[test]
fn classify_a_presence_only_grant() {
    let records = vec![rec("acct-a", "alice@example.com")];
    let mut f = facts(Some("alice@example.com"), None);
    f.grant_sources = vec!["scoped-keychain"];
    let never = |_: &str| -> Option<GrantFacts> { panic!("stash read in presence mode") };
    assert_eq!(
        classify(&f, &records, &never),
        Status::InOrca {
            id: "acct-a".into(),
            fresher: None
        }
    );
    f.email = Some("bob@example.com".into());
    assert_eq!(classify(&f, &records, &never), Status::ToImport);
}

#[test]
fn carry_seeds_a_missing_file_without_the_identity() {
    let from = json!({
        "oauthAccount": {"emailAddress": "alice@example.com"},
        "hasCompletedOnboarding": true,
        "numStartups": 7,
        "theme": "dark",
        "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": true}}
    })
    .as_object()
    .unwrap()
    .clone();
    let c = carry_config(None, &from);
    assert_eq!(c.seeded, Some(4));
    assert!(c.changed());
    assert!(c.map.get("oauthAccount").is_none());
    assert_eq!(c.map["hasCompletedOnboarding"], true);
    assert_eq!(c.map["numStartups"], 7);
    assert_eq!(
        c.map["projects"]["/Users/example/src/app"]["hasTrustDialogAccepted"],
        true
    );
    // A floor file with nothing but settings still seeds.
    let bare = json!({"theme": "light", "oauthAccount": {}})
        .as_object()
        .unwrap()
        .clone();
    let c = carry_config(None, &bare);
    assert_eq!(c.seeded, Some(1));
    assert!(c.added.is_empty() && c.changed());
    // An existing file is never seeded, only merged: the trust fields
    // and the onboarding keys it lacks, nothing else.
    let existing = json!({"numStartups": 1}).as_object().unwrap().clone();
    let c = carry_config(Some(existing), &from);
    assert_eq!(c.seeded, None);
    assert_eq!(c.map["numStartups"], 1);
    assert_eq!(c.map["theme"], "dark");
    assert!(c.map.get("oauthAccount").is_none());
    assert_eq!(
        c.added,
        vec![
            "hasCompletedOnboarding",
            "theme",
            "projects[/Users/example/src/app].hasTrustDialogAccepted",
        ]
    );
}

/// B2 records the digest of the floor config it merged, and records it
/// again when the floor config changed since, even with nothing to add.
#[test]
fn b2_records_the_floor_config_it_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let ctx = Context::from_env(
        HostEnv::for_test(home, HostOs::Linux),
        &FakeProcs::default(),
    );
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    let floor = json!({"mcpServers": {"docs": {"command": "x"}}});
    std::fs::write(work.join(".claude.json"), floor.to_string()).unwrap();
    let legacy = Legacy {
        profiles: vec![LegacyProfile {
            name: "work".into(),
            dir: work.clone(),
        }],
        floor: Some("work".into()),
    };
    assert!(b2(&ctx, &legacy).unwrap().is_some());
    let digest = || std::fs::read_to_string(merge_marker(&ctx.state)).unwrap();
    assert_eq!(digest(), config_digest(floor.to_string().as_bytes()));
    // The user drops the server again, and the floor config changes.
    std::fs::write(home.join(".claude.json"), "{}").unwrap();
    let changed = json!({"mcpServers": {"docs": {"command": "x"}, "web": {"command": "y"}}});
    std::fs::write(work.join(".claude.json"), changed.to_string()).unwrap();
    assert!(b2(&ctx, &legacy).unwrap().is_some());
    assert_eq!(digest(), config_digest(changed.to_string().as_bytes()));
    // A rerun with nothing to add records the merge too.
    std::fs::remove_file(merge_marker(&ctx.state)).unwrap();
    assert!(b2(&ctx, &legacy).unwrap().is_none());
    assert_eq!(digest(), config_digest(changed.to_string().as_bytes()));
}

/// An import Orca refused or did not confirm is a failed row, never
/// `imported`: the run then exits non-zero and skips the floor switch.
#[test]
fn import_line_fails_a_refused_or_unconfirmed_redo() {
    use crate::orca::store::RedoOutcome;
    let change = |redo| add::AccountChange {
        route: add::Route::OfflineThenRpc,
        id: None,
        email: Some("carol@example.com".into()),
        redo,
        leftover: None,
    };
    assert_eq!(
        import_line(&change(None)).unwrap(),
        "imported carol@example.com"
    );
    assert!(import_line(&change(Some(RedoOutcome::Reissued(Value::Null)))).is_ok());
    let e = import_line(&change(Some(RedoOutcome::Failed("no".into())))).unwrap_err();
    assert!(e.contains("not imported") && e.contains("refused"), "{e}");
    let e = import_line(&change(Some(RedoOutcome::Uncertain("timeout".into())))).unwrap_err();
    assert!(e.contains("did not confirm") && e.contains("doctor"), "{e}");
}

/// A trust value the user changed since is upgraded only where the merge
/// upgrades a default, and existing MCP servers stay.
#[test]
fn merge_config_upgrades_only_a_default_trust_value() {
    let from = json!({"hasCompletedOnboarding": true,
        "projects": {"/w": {"hasTrustDialogAccepted": true}},
        "mcpServers": {"docs": {"command": "x"}}});
    let from = from.as_object().unwrap();
    let done = json!({"projects": {"/w": {"hasTrustDialogAccepted": false}},
        "mcpServers": {"docs": {"command": "y"}}, "hasCompletedOnboarding": false});
    let mut t = done.as_object().cloned().unwrap();
    assert_eq!(
        merge_config(&mut t, from),
        vec!["projects[/w].hasTrustDialogAccepted"]
    );
}

#[test]
fn smart_carries_only_what_csm_still_reads() {
    for n in [
        "01234567-89ab-cdef-0123-456789abcdef.json",
        "titles.tsv",
        "scan-meta-v2.-Users-example-src-app.tsv",
    ] {
        assert!(smart_carries(n), "{n}");
    }
    for n in [
        ".usage-cache.json",
        ".usage-fetch-failed",
        ".last-switch",
        "01234567-89ab-cdef-0123-456789abcdef.pid",
        "01234567-89ab-cdef-0123-456789abcdef.switched",
        "work.json",
        "scan-meta.x.tsv",
        "usage",
    ] {
        assert!(!smart_carries(n), "{n}");
    }
}

#[test]
fn apply_smart_moves_sidecars_and_keeps_collisions() {
    let tmp = tempfile::tempdir().unwrap();
    let old = legacy_smart_dir(tmp.path());
    let new = tmp.path().join(".local").join("state").join("csm");
    std::fs::create_dir_all(old.join("usage")).unwrap();
    std::fs::create_dir_all(&new).unwrap();
    let sid = "01234567-89ab-cdef-0123-456789abcdef";
    let other = "11234567-89ab-cdef-0123-456789abcdef";
    std::fs::write(old.join(format!("{sid}.json")), b"{\"cwd\":\"/x\"}").unwrap();
    std::fs::write(old.join("titles.tsv"), b"t\tsid\t1\n").unwrap();
    std::fs::write(old.join(format!("{other}.json")), b"old").unwrap();
    std::fs::write(new.join(format!("{other}.json")), b"new").unwrap();
    std::fs::write(old.join(".usage-cache.json"), b"{}").unwrap();
    std::fs::write(old.join("usage").join("work.json"), b"{}").unwrap();
    assert_eq!(smart_preview(&old), Some((3, 2)));

    let line = apply_smart(&old, &new).unwrap().unwrap();
    assert!(line.contains("moved 2"), "{line}");
    assert!(line.contains("1 already there"), "{line}");
    assert_eq!(
        std::fs::read(new.join(format!("{sid}.json"))).unwrap(),
        b"{\"cwd\":\"/x\"}"
    );
    assert!(new.join("titles.tsv").is_file());
    assert_eq!(
        std::fs::read(new.join(format!("{other}.json"))).unwrap(),
        b"new"
    );
    assert!(
        old.join(format!("{other}.json")).is_file(),
        "collision stays"
    );
    assert!(old.join(".usage-cache.json").is_file());
    assert!(!new.join(".usage-cache.json").exists());
    assert!(!new.join("usage").exists());
    assert!(
        apply_smart(&tmp.path().join("none"), &new)
            .unwrap()
            .is_none()
    );
}

/// `migrate plan` on macOS reads no secret: every Keychain call is a
/// presence probe (no `-w`), and neither a legacy item nor a stash is
/// read.
#[cfg(unix)]
#[test]
fn plan_on_macos_probes_the_keychain_for_presence_only() {
    let fake = crate::orca::testsupport::FakeSecurity::install();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::MacOs);
    let procs = FakeProcs::default();
    let ctx = Context::from_env(env.clone(), &procs);
    let ud = ctx.user_data.dir.clone();
    let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
    make_stash(&ud, "acct-a", Some(&alice), None);
    fake.put(
        keychain::STASH_SERVICE,
        "acct-a",
        creds_json("at-stash", "rt-stash", 1).as_bytes(),
    );
    write_store(
        &ud,
        &[record_json(&ud, "acct-a", "alice@example.com", None)],
        Some("acct-a"),
    );
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "alice@example.com"}}).to_string(),
    )
    .unwrap();
    fake.put(
        &keychain::runtime_service(Some(&work.to_string_lossy())),
        &ctx.keychain_user.acct,
        creds_json("at-dir", "rt-dir", 2).as_bytes(),
    );
    let cfg = legacy_dir(home);
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("profiles.json"), json!({"work": work}).to_string()).unwrap();
    let legacy = load_legacy(home).unwrap();
    let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
    let before = fake.argv().len();

    let plan = build_plan(&ctx, &view, &legacy, Probe::Presence);
    let calls: Vec<String> = fake.argv().split_off(before);
    assert!(!calls.is_empty(), "the dir's items were probed");
    for c in &calls {
        assert!(c.starts_with("find-generic-password"), "{c}");
        assert!(!c.split(' ').any(|t| t == "-w"), "a secret read: {c}");
    }
    assert_eq!(
        plan.rows[0].status,
        Status::InOrca {
            id: "acct-a".into(),
            fresher: None
        }
    );
    assert_eq!(plan.rows[0].facts.grant_sources, vec!["scoped-keychain"]);
    assert!(plan.rows[0].stash.is_none());
    let text = render_plan(&plan);
    assert!(
        text.contains("present (scoped-keychain; not read)"),
        "{text}"
    );
    for secret in ["at-dir", "rt-dir", "at-stash", "rt-stash"] {
        assert!(!text.contains(secret), "{text}");
    }
}

/// A Keychain probe or read that fails (a locked Keychain) never reads
/// as "no login": plan reports it, the registry's `remaining` check
/// counts the dir, and retire moves nothing.
#[cfg(unix)]
#[test]
fn an_unreadable_keychain_item_counts_as_a_login() {
    let fake = crate::orca::testsupport::FakeSecurity::install();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::MacOs);
    let procs = FakeProcs::default();
    let ctx = Context::from_env(env.clone(), &procs);
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "alice@example.com"}}).to_string(),
    )
    .unwrap();
    let svc = keychain::runtime_service(Some(&work.to_string_lossy()));
    fake.put(
        &svc,
        &ctx.keychain_user.acct,
        creds_json("at-dir", "rt-dir", 2).as_bytes(),
    );
    fake.fail_find(&svc, true);

    assert_eq!(dir_grant_sources(&ctx, &work), vec![KEYCHAIN_UNREADABLE]);
    let lp = LegacyProfile {
        name: "work".into(),
        dir: work.clone(),
    };
    let facts = profile_facts(&ctx, &lp, Probe::Read);
    assert!(
        facts.grant_sources.contains(&KEYCHAIN_UNREADABLE),
        "{facts:?}"
    );
    assert_ne!(classify(&facts, &[], &|_| None), Status::NoCredentials);
    let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
    let err = retire_dir(&ctx, &view, &FakeHttp::default(), &work, "acct-a", &|| {
        Ok(())
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("nothing moved"), "{err:#}");
    assert!(work.is_dir());
    assert!(!home.join(".claude.work.retired").exists());
    assert!(Quarantine::new(HostOs::MacOs, &ctx.state).list().is_empty());
}

#[test]
fn merge_keeps_existing_keys_and_reports_additions() {
    let mut target = json!({
        "numStartups": 3,
        "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": false}},
        "mcpServers": {"kept": {"command": "a"}}
    })
    .as_object()
    .unwrap()
    .clone();
    let from = json!({
        "oauthAccount": {"emailAddress": "alice@example.com"},
        "projects": {
            "/Users/example/src/app": {"hasTrustDialogAccepted": true, "allowedTools": ["x"], "history": [1]},
            "/Users/example/src/lib": {"hasTrustDialogAccepted": true},
            "/Users/example/src/none": {"history": []},
            "/Users/example/src/mcp": {"mcpServers": {"local": {"command": "l"}}}
        },
        "mcpServers": {"kept": {"command": "b"}, "new": {"command": "c"}}
    })
    .as_object()
    .unwrap()
    .clone();
    let added = merge_config(&mut target, &from);
    assert_eq!(
        added,
        vec![
            "projects[/Users/example/src/app].hasTrustDialogAccepted",
            "projects[/Users/example/src/app].allowedTools",
            "projects[/Users/example/src/lib].hasTrustDialogAccepted",
            "projects[/Users/example/src/mcp].mcpServers.local",
            "mcpServers.new"
        ]
    );
    assert_eq!(
        target["projects"]["/Users/example/src/mcp"]["mcpServers"]["local"]["command"],
        "l"
    );
    // A `false` is Claude Code's default, not a decision: the floor's
    // accepted trust replaces it.
    assert_eq!(
        target["projects"]["/Users/example/src/app"]["hasTrustDialogAccepted"],
        true
    );
    assert!(
        target["projects"]["/Users/example/src/app"]
            .get("history")
            .is_none()
    );
    assert!(target["projects"].get("/Users/example/src/none").is_none());
    assert_eq!(target["mcpServers"]["kept"]["command"], "a");
    assert!(target.get("oauthAccount").is_none());
    assert_eq!(target["numStartups"], 3);
    assert!(merge_config(&mut target, &from).is_empty(), "idempotent");
}

/// Claude Code 2.1.283 saves a project entry whole from its default
/// (`nne`), so an untrusted run under the default dir leaves explicit
/// `false` and `[]` values. Step 5 upgrades those to the floor's real
/// values, keeps a target value that is a real choice, and never lets a
/// floor default overwrite anything.
#[test]
fn merge_replaces_claude_code_defaults_with_the_floor_values() {
    let nne = json!({
        "allowedTools": [],
        "mcpContextUris": [],
        "mcpServers": {},
        "enabledMcpjsonServers": [],
        "disabledMcpjsonServers": [],
        "hasTrustDialogAccepted": false,
        "hasClaudeMdExternalIncludesApproved": false,
        "hasClaudeMdExternalIncludesWarningShown": false
    });
    let mut target = json!({
        "projects": {
            "/Users/example/src/app": nne.clone(),
            "/Users/example/src/own": {"allowedTools": ["mine"], "hasTrustDialogAccepted": true},
            "/Users/example/src/same": nne
        }
    })
    .as_object()
    .unwrap()
    .clone();
    let from = json!({
        "projects": {
            "/Users/example/src/app": {
                "hasTrustDialogAccepted": true,
                "hasClaudeMdExternalIncludesApproved": true,
                "allowedTools": ["Bash(ls)"],
                "enabledMcpjsonServers": ["db"],
                "disabledMcpjsonServers": [],
                "projectOnboardingSeenCount": 2
            },
            "/Users/example/src/own": {"allowedTools": ["floor"], "hasTrustDialogAccepted": false},
            "/Users/example/src/same": {"hasTrustDialogAccepted": false, "allowedTools": []}
        }
    })
    .as_object()
    .unwrap()
    .clone();
    let added = merge_config(&mut target, &from);
    assert_eq!(
        added,
        vec![
            "projects[/Users/example/src/app].hasTrustDialogAccepted",
            "projects[/Users/example/src/app].projectOnboardingSeenCount",
            "projects[/Users/example/src/app].hasClaudeMdExternalIncludesApproved",
            "projects[/Users/example/src/app].enabledMcpjsonServers",
            "projects[/Users/example/src/app].allowedTools",
        ]
    );
    let app = &target["projects"]["/Users/example/src/app"];
    assert_eq!(app["hasTrustDialogAccepted"], true);
    assert_eq!(app["hasClaudeMdExternalIncludesApproved"], true);
    assert_eq!(app["allowedTools"], json!(["Bash(ls)"]));
    assert_eq!(app["enabledMcpjsonServers"], json!(["db"]));
    assert_eq!(app["disabledMcpjsonServers"], json!([]));
    assert_eq!(app["hasClaudeMdExternalIncludesWarningShown"], false);
    // A real target value is the user's choice and stays.
    let own = &target["projects"]["/Users/example/src/own"];
    assert_eq!(own["allowedTools"], json!(["mine"]));
    assert_eq!(own["hasTrustDialogAccepted"], true);
    // Default over default changes nothing.
    assert_eq!(
        target["projects"]["/Users/example/src/same"]["hasTrustDialogAccepted"],
        false
    );
    assert!(merge_config(&mut target, &from).is_empty(), "idempotent");
    for v in [json!(null), json!(false), json!(0), json!([]), json!({})] {
        assert!(is_cc_project_default(&v), "{v}");
    }
    for v in [
        json!(true),
        json!(1),
        json!(["x"]),
        json!({"a": 1}),
        json!(""),
    ] {
        assert!(!is_cc_project_default(&v), "{v}");
    }
}

#[test]
fn appended_history_puts_the_shared_lines_first_once() {
    assert_eq!(
        appended(b"{\"a\":1}\n", b"{\"b\":2}\n").unwrap(),
        b"{\"a\":1}\n{\"b\":2}\n"
    );
    // A missing final newline does not glue two lines together.
    assert_eq!(appended(b"x", b"y\n").unwrap(), b"x\ny\n");
    // An empty shared history adds nothing: the local file stays.
    assert_eq!(appended(b"", b"y\n"), None);
    // Already appended (a rerun): nothing to write.
    assert_eq!(appended(b"x\n", b"x\ny\n"), None);
}

/// A line a live session appends to either history file while B1 merges
/// them is never lost: the local one is re-read when it grew, and the
/// shared one's late lines follow the merged history.
#[test]
fn merge_history_keeps_lines_appended_during_the_merge() {
    use std::io::Write as _;
    let tmp = tempfile::tempdir().unwrap();
    let (s, l) = (
        tmp.path().join("shared.jsonl"),
        tmp.path().join("local.jsonl"),
    );
    let append = |p: &Path, b: &[u8]| {
        let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(b).unwrap();
    };
    // A legacy session appends to the shared file after the read.
    std::fs::write(&s, b"s1\n").unwrap();
    std::fs::write(&l, b"l1\n").unwrap();
    let mut once = true;
    merge_history(&s, &l, &mut || {
        if std::mem::take(&mut once) {
            append(&s, b"s2\n");
        }
    })
    .unwrap();
    assert_eq!(std::fs::read(&l).unwrap(), b"s1\nl1\ns2\n");
    assert!(!s.exists());
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "no aside file left"
    );

    // An implicit session appends to the local file after the read: read
    // again, nothing dropped.
    std::fs::write(&s, b"s1\n").unwrap();
    std::fs::write(&l, b"l1\n").unwrap();
    let mut once = true;
    merge_history(&s, &l, &mut || {
        if std::mem::take(&mut once) {
            append(&l, b"l2\n");
        }
    })
    .unwrap();
    assert_eq!(std::fs::read(&l).unwrap(), b"s1\nl1\nl2\n");

    // A local file that never rests: the step waits, the shared file stays.
    std::fs::write(&s, b"s1\n").unwrap();
    std::fs::write(&l, b"l1\n").unwrap();
    let e = merge_history(&s, &l, &mut || append(&l, b"lx\n")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(std::fs::read(&s).unwrap(), b"s1\n");

    // A shared file rewritten (not appended to) under the merge is kept.
    std::fs::write(&s, b"s1\n").unwrap();
    std::fs::write(&l, b"l1\n").unwrap();
    let mut once = true;
    let e = merge_history(&s, &l, &mut || {
        if std::mem::take(&mut once) {
            std::fs::write(&s, b"other\n").unwrap();
        }
    })
    .unwrap_err();
    assert!(e.to_string().contains("kept at"), "{e}");
    let kept: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".csm-merged."))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
}

/// The plugin registries record absolute paths through the profile
/// dir's `plugins` link. After step 6 moved the dir, they point at
/// `~/.claude/plugins`, but only where that path exists.
#[test]
fn plugin_paths_move_to_the_new_plugins_dir() {
    let home = Path::new("/Users/example");
    let dirs = vec![home.join(".claude.work"), home.join(".claude.home")];
    let prefixes = old_plugin_prefixes(home, &dirs);
    let mut v = json!({
        "version": 2,
        "plugins": {
            "slack@official": [{
                "scope": "user",
                "installPath": "/Users/example/.claude.work/plugins/cache/official/slack/1.0",
                "version": "1.0"
            }],
            "lint@other": [{
                "installPath": "/Users/example/.claude.home/plugins/cache/other/lint/2.0"
            }],
            "gone@other": [{
                "installPath": "/Users/example/.claude.shared/plugins/cache/other/gone/1.0"
            }],
            "elsewhere@x": [{"installPath": "/opt/plugins/x"}]
        }
    });
    let r = rewrite_plugin_paths(&mut v, &prefixes, "/Users/example/.claude/plugins", &|p| {
        !p.contains("/gone/")
    });
    assert_eq!(r.rewritten.len(), 2, "{r:?}");
    assert_eq!(
        r.dangling,
        vec!["/Users/example/.claude.shared/plugins/cache/other/gone/1.0"]
    );
    assert_eq!(
        v["plugins"]["slack@official"][0]["installPath"],
        "/Users/example/.claude/plugins/cache/official/slack/1.0"
    );
    assert_eq!(
        v["plugins"]["lint@other"][0]["installPath"],
        "/Users/example/.claude/plugins/cache/other/lint/2.0"
    );
    assert_eq!(
        v["plugins"]["elsewhere@x"][0]["installPath"],
        "/opt/plugins/x"
    );
    // Key order and other fields are kept.
    assert_eq!(
        serde_json::to_string(&v["plugins"]["slack@official"][0]).unwrap(),
        r#"{"scope":"user","installPath":"/Users/example/.claude/plugins/cache/official/slack/1.0","version":"1.0"}"#
    );
    let mut m = json!({"official": {"installLocation": "/Users/example/.claude.work/plugins/marketplaces/official"}});
    let r = rewrite_plugin_paths(&mut m, &prefixes, "/Users/example/.claude/plugins", &|_| {
        true
    });
    assert_eq!(r.rewritten.len(), 1);
    assert_eq!(
        m["official"]["installLocation"],
        "/Users/example/.claude/plugins/marketplaces/official"
    );
}

#[cfg(unix)]
#[test]
fn b1_carries_history_and_linked_dirs_and_b5_rewrites_plugin_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let d = home.join(".claude");
    let sh = shared_root(home);
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    for n in [
        "todos",
        "shell-snapshots",
        "plugins/cache/official/slack/1.0",
    ] {
        std::fs::create_dir_all(sh.join(n)).unwrap();
    }
    std::fs::write(sh.join("todos").join("t.json"), b"[]").unwrap();
    std::fs::write(sh.join("history.jsonl"), b"{\"old\":1}\n").unwrap();
    std::fs::write(d.join("history.jsonl"), b"{\"new\":2}\n").unwrap();
    std::os::unix::fs::symlink(sh.join("todos"), d.join("todos")).unwrap();
    let old = work.join("plugins/cache/official/slack/1.0");
    std::fs::write(
        sh.join("plugins").join("installed_plugins.json"),
        serde_json::to_vec_pretty(&json!({"version": 2, "plugins": {"slack@official": [
            {"installPath": old.to_string_lossy()}
        ]}}))
        .unwrap(),
    )
    .unwrap();
    // The profile's own content stays behind and is listed.
    std::fs::create_dir_all(work.join("skills")).unwrap();
    std::fs::create_dir_all(work.join("file-history")).unwrap();
    std::os::unix::fs::symlink(sh.join("todos"), work.join("todos")).unwrap();
    std::fs::write(work.join("settings.json"), b"{}").unwrap();
    std::fs::create_dir_all(work.join("hooks")).unwrap();
    // claude's own per-dir caches are listed as well.
    std::fs::create_dir_all(work.join("paste-cache")).unwrap();
    std::fs::create_dir_all(work.join("chrome")).unwrap();
    // A real projects dir: B1 does not merge it (retire moves it), and a
    // real session-env dir is never merged; both are reported.
    std::fs::create_dir_all(work.join("projects")).unwrap();
    std::fs::create_dir_all(work.join("session-env")).unwrap();
    assert_eq!(
        left_behind(home, &work),
        vec![
            "settings.json",
            "hooks",
            "skills",
            "file-history",
            "paste-cache",
            "chrome",
            "projects (its own, not the shared one: retire moves it into ~/.claude)",
            "session-env (its own, not the shared one: not merged)"
        ]
    );

    let plan = shared_plan(home);
    let get = |n: &str| plan.iter().find(|(k, _)| *k == n).unwrap().1.clone();
    assert_eq!(get("todos"), SharedStep::Start);
    assert_eq!(get("shell-snapshots"), SharedStep::Move);
    assert_eq!(get("history.jsonl"), SharedStep::Append);
    assert_eq!(get("session-env"), SharedStep::Done);
    assert_eq!(plugin_paths_preview(home, std::slice::from_ref(&work)), 1);

    b1_all(home);
    assert_eq!(
        std::fs::read(d.join("history.jsonl")).unwrap(),
        b"{\"old\":1}\n{\"new\":2}\n"
    );
    assert_eq!(
        std::fs::read_link(sh.join("history.jsonl")).unwrap(),
        d.join("history.jsonl")
    );
    assert!(d.join("shell-snapshots").is_dir());
    let tm = std::fs::symlink_metadata(d.join("todos")).unwrap();
    assert!(tm.is_dir() && !tm.file_type().is_symlink());

    let state = home.join("state");
    let lines = apply_plugin_paths(home, std::slice::from_ref(&work), &state, true).unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let v: Value =
        serde_json::from_slice(&std::fs::read(d.join("plugins/installed_plugins.json")).unwrap())
            .unwrap();
    assert_eq!(
        v["plugins"]["slack@official"][0]["installPath"],
        json!(d.join("plugins/cache/official/slack/1.0").to_string_lossy())
    );
    assert_eq!(std::fs::read_dir(state.join("migrate")).unwrap().count(), 1);
    // A rerun changes nothing.
    assert!(
        apply_plugin_paths(home, std::slice::from_ref(&work), &state, true)
            .unwrap()
            .is_empty()
    );
    // A legacy session records a new path through its dir after B5 ran:
    // the next pass rewrites it too (B5 runs on every carry). The
    // profile's plugins link resolves through the compat link.
    std::os::unix::fs::symlink(sh.join("plugins"), work.join("plugins")).unwrap();
    let reg = d.join("plugins/installed_plugins.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&reg).unwrap()).unwrap();
    std::fs::create_dir_all(d.join("plugins/cache/official/lint/2.0")).unwrap();
    let late = work.join("plugins/cache/official/lint/2.0");
    v["plugins"]["lint@official"] = json!([{"installPath": late.to_string_lossy()}]);
    std::fs::write(&reg, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    assert_eq!(plugin_refs_into(home, &work, false).len(), 1);
    assert!(plugin_refs_into(home, &work, true).is_empty());
    let lines = apply_plugin_paths(home, std::slice::from_ref(&work), &state, false).unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let v: Value = serde_json::from_slice(&std::fs::read(&reg).unwrap()).unwrap();
    assert_eq!(
        v["plugins"]["lint@official"][0]["installPath"],
        json!(d.join("plugins/cache/official/lint/2.0").to_string_lossy())
    );
    assert!(plugin_refs_into(home, &work, false).is_empty());
}

/// Stage C's plugin check: a recorded path under the dir's own plugins
/// that resolves only there holds the retire back; one that resolves
/// nowhere (broken already) or points elsewhere does not. A dangling path
/// is reported on B5's first pass only.
#[cfg(unix)]
#[test]
fn plugin_paths_only_the_dir_holds_keep_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let work = home.join(".claude.work");
    let d = home.join(".claude");
    std::fs::create_dir_all(d.join("plugins")).unwrap();
    std::fs::create_dir_all(work.join("plugins/cache/own/1.0")).unwrap();
    let own = work.join("plugins/cache/own/1.0");
    let gone = work.join("plugins/cache/gone/1.0");
    let elsewhere = home.join("elsewhere/plugin");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let reg = json!({"plugins": {
        "own@x": [{"installPath": own.to_string_lossy()}],
        "gone@x": [{"installPath": gone.to_string_lossy()}],
        "other@x": [{"installPath": elsewhere.to_string_lossy()}],
    }});
    std::fs::write(
        d.join("plugins/installed_plugins.json"),
        serde_json::to_vec(&reg).unwrap(),
    )
    .unwrap();
    let under = plugin_paths_under(&reg, &plugin_prefixes_of(std::slice::from_ref(&work)));
    assert_eq!(under.len(), 2, "{under:?}");
    for after_b5 in [false, true] {
        assert_eq!(
            plugin_refs_into(home, &work, after_b5),
            vec![own.to_string_lossy().into_owned()]
        );
    }
    let line = plugin_refs_line(&work, &plugin_refs_into(home, &work, false));
    assert!(line.starts_with("1 recorded plugin path"), "{line}");
    let state = home.join("state");
    let first = apply_plugin_paths(home, std::slice::from_ref(&work), &state, true).unwrap();
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        apply_plugin_paths(home, std::slice::from_ref(&work), &state, false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_profile_link_names_the_shared_entry_by_text() {
    let home = Path::new("/Users/example");
    let work = home.join(".claude.work");
    let shared = home.join(".claude.shared").join("projects");
    assert!(names_shared(&work, &shared, &shared));
    assert!(names_shared(
        &work,
        Path::new("../.claude.shared/projects"),
        &shared
    ));
    assert!(names_shared(
        &work,
        Path::new("/Users/example/./.claude.shared/projects"),
        &shared
    ));
    assert!(!names_shared(
        &work,
        Path::new("/Users/example/.claude/projects"),
        &shared
    ));
    assert!(!names_shared(&work, Path::new("projects"), &shared));
}

#[test]
fn session_floor_is_inert_under_test() {
    assert_eq!(session_floor().unwrap(), None);
}

/// A SQLite-backed Orca profile used to deadlock `migrate import`: the
/// write gate needs Orca stopped and the SQLite gate needed it running.
/// Now only the store-patching steps (the import, the floor switch) are
/// deferred to a running Orca; read-backs still run offline.
#[test]
fn a_sqlite_profile_defers_only_the_store_writes() {
    let fresher = Status::InOrca {
        id: "acct-a".into(),
        fresher: Some(true),
    };
    let current = Status::InOrca {
        id: "acct-a".into(),
        fresher: Some(false),
    };
    assert_eq!(row_action(&Status::ToImport, false), RowAction::Import);
    assert_eq!(row_action(&Status::ToImport, true), RowAction::Defer);
    for sqlite in [false, true] {
        assert_eq!(
            row_action(&fresher, sqlite),
            RowAction::ReadBack("acct-a".into())
        );
        assert_eq!(row_action(&current, sqlite), RowAction::Skip);
        assert_eq!(row_action(&Status::NoCredentials, sqlite), RowAction::Skip);
    }
}

/// Round 8: an agent that runs `launchctl setenv CLAUDE_CONFIG_DIR` at
/// every login is found (the cutover names it as the floor's writer),
/// whether the plist or the script it runs says it; an agent that only
/// sets the variable for its own job is not.
#[test]
fn a_launch_agent_that_sets_the_floor_again_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let agents = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    assert_eq!(floor_agents(home), Vec::<PathBuf>::new());
    let script = home.join("bin-setenv");
    std::fs::write(
        &script,
        "#!/bin/sh\nlaunchctl setenv CLAUDE_CONFIG_DIR \"$HOME/.claude.work\"\n",
    )
    .unwrap();
    let plist = |args: &[&str]| {
        let a: String = args
            .iter()
            .map(|x| format!("<string>{x}</string>"))
            .collect();
        format!(
            "<?xml version=\"1.0\"?><plist><dict><key>Label</key><string>com.example.a</string>\
             <key>ProgramArguments</key><array>{a}</array><key>RunAtLoad</key><true/></dict></plist>"
        )
    };
    std::fs::write(
        agents.join("a.plist"),
        plist(&["/bin/sh", script.to_str().unwrap()]),
    )
    .unwrap();
    std::fs::write(
        agents.join("b.plist"),
        plist(&[
            "/bin/launchctl",
            "setenv",
            "CLAUDE_CONFIG_DIR",
            "/Users/example/.claude.work",
        ]),
    )
    .unwrap();
    std::fs::write(
        agents.join("c.plist"),
        "<plist><dict><key>EnvironmentVariables</key><dict><key>CLAUDE_CONFIG_DIR</key>\
         <string>/Users/example/.claude</string></dict><key>ProgramArguments</key><array>\
         <string>/usr/bin/true</string></array></dict></plist>",
    )
    .unwrap();
    std::fs::write(
        agents.join("d.txt"),
        plist(&["/bin/launchctl", "setenv", "CLAUDE_CONFIG_DIR"]),
    )
    .unwrap();
    let found = floor_agents(home);
    assert_eq!(found, vec![agents.join("a.plist"), agents.join("b.plist")]);
    let w = floor_writer(HostOs::MacOs, &found);
    assert!(w.contains("a.plist"), "{w}");
}

/// Round 8: an existing small ~/.claude.json gains the floor's
/// onboarding keys it lacks (never `oauthAccount`, never a key it has),
/// so claude does not rerun its first launch in Orca panes.
#[test]
fn b2_carries_missing_keys_into_an_existing_file() {
    let from = json!({
        "oauthAccount": {"emailAddress": "alice@example.com"},
        "hasCompletedOnboarding": true,
        "lastOnboardingVersion": "2.1.0",
        "theme": "dark",
        "numStartups": 40,
    });
    let target = json!({"oauthAccount": {"emailAddress": "bob@example.com"}, "theme": "light"});
    let c = carry_config(
        Some(target.as_object().unwrap().clone()),
        from.as_object().unwrap(),
    );
    assert_eq!(c.seeded, None);
    assert_eq!(
        c.added,
        vec![
            "hasCompletedOnboarding",
            "lastOnboardingVersion",
            "numStartups"
        ]
    );
    assert_eq!(c.map["hasCompletedOnboarding"], json!(true));
    assert_eq!(c.map["theme"], json!("light"));
    assert_eq!(
        c.map["oauthAccount"]["emailAddress"],
        json!("bob@example.com")
    );
    assert_eq!(c.map["numStartups"], json!(40));
}

#[test]
fn retire_verdicts() {
    use DirGrant::{Newer, Superseded, Unreadable};
    use StashCheck::{Rejected, Unverified, Verified};
    let home = Path::new("/Users/example");
    let dir = home.join(".claude.work");
    let in_orca = Status::InOrca {
        id: "acct-a".into(),
        fresher: Some(false),
    };
    let v = |st: &Status, c, g, d: &Path, e| retire_verdict(st, None, c, g, d, home, e);
    let acct = || RetireAs::Account("acct-a".into());
    assert_eq!(
        v(&in_orca, Verified, Superseded, &dir, true).unwrap(),
        acct()
    );
    assert!(v(&in_orca, Unverified, Superseded, &dir, true).is_err());
    assert!(v(&in_orca, Verified, Superseded, &dir, false).is_err());
    assert!(v(&in_orca, Verified, Superseded, &home.join(".claude"), true).is_err());
    assert!(v(&Status::ToImport, Verified, Superseded, &dir, true).is_err());
    // A dir that holds no login and names no account Orca has retires
    // with nothing to file (design section 2, C); never the default dir
    // or a dir that is gone.
    assert_eq!(
        v(&Status::NoCredentials, Verified, Superseded, &dir, true).unwrap(),
        RetireAs::NoLogin
    );
    assert!(v(&Status::NoCredentials, Verified, Superseded, &dir, false).is_err());
    assert!(
        v(
            &Status::NoCredentials,
            Verified,
            Superseded,
            &home.join(".claude"),
            true
        )
        .is_err()
    );
    // No grant left but the account is Orca's (a retire that died
    // between the deletes and the rename): rename only. Still never the
    // default dir, and never a dir that is gone.
    let known = |d: &Path, e| {
        retire_verdict(
            &Status::NoCredentials,
            Some("acct-a"),
            Unverified,
            Superseded,
            d,
            home,
            e,
        )
    };
    assert_eq!(known(&dir, true).unwrap(), acct());
    assert!(known(&dir, false).is_err());
    assert!(known(&home.join(".claude"), true).is_err());
    // Known identity does not lift the ToImport refusal.
    assert!(
        retire_verdict(
            &Status::ToImport,
            Some("acct-a"),
            Verified,
            Superseded,
            &dir,
            home,
            true
        )
        .is_err()
    );
    // A dir grant newer than the stash no longer blocks: retire files it as
    // `Retired` under its account and settle stores it later, whatever the
    // stash check said. An unreadable one still blocks.
    for c in [Verified, Rejected, Unverified] {
        assert_eq!(v(&in_orca, c, Newer, &dir, true).unwrap(), acct());
    }
    let e = v(&in_orca, Verified, Unreadable, &dir, true).unwrap_err();
    assert!(e.contains("nothing moved"), "{e}");
    // A rejected (expired) stash token loses nothing: the stash holds the
    // same or a newer refresh token and the dir's grants are quarantined.
    assert_eq!(
        v(&in_orca, Rejected, Superseded, &dir, true).unwrap(),
        acct()
    );
    let e = v(&in_orca, Unverified, Superseded, &dir, true).unwrap_err();
    assert!(!e.contains("migrate import"), "{e}");
}

/// A dir with no login and no account Orca knows is renamed with nothing
/// to file; a grant that appeared since the verdict stops it.
#[test]
fn a_dir_without_a_login_retires_by_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (ctx, _, dir, _) = retire_world(home, "");
    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    // A login appeared: nothing moves.
    std::fs::write(dir.join(".credentials.json"), creds_json("at-n", "rt-n", 5)).unwrap();
    assert!(retire_dir_no_login(&ctx, &dir).is_err());
    assert!(dir.join(".credentials.json").is_file());
    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    let line = retire_dir_no_login(&ctx, &dir).unwrap();
    assert!(line.contains("no login"), "{line}");
    let retired = home.join(".claude.work.retired");
    assert!(!dir.exists() && retired.join("settings.json").is_file());
    assert!(Quarantine::new(HostOs::Linux, &ctx.state).list().is_empty());
    // The .retired name is taken: refused.
    std::fs::create_dir_all(&dir).unwrap();
    assert!(retire_dir_no_login(&ctx, &dir).is_err());
}

/// A store-less floor retire keeps the login that should live on in
/// `~/.claude`: the floor's moves there when `~/.claude` has none, replaces
/// an older copy of the same account's (which goes to the quarantine), and
/// leaves another account's login or a fresher copy alone.
#[test]
fn store_less_home_keeps_the_live_login() {
    let old = creds_json("at-1", "rt-1", 10);
    let new = creds_json("at-2", "rt-2", 20);
    let same_line_newer = creds_json("at-3", "rt-1", 30);
    assert_eq!(store_less_home(&new, None, None), HomeLogin::Move);
    assert_eq!(
        store_less_home(&new, Some(&new), Some(true)),
        HomeLogin::Keep
    );
    // The floor refreshed after the cutover's copy: rotated, fresher.
    assert_eq!(
        store_less_home(&new, Some(&old), Some(true)),
        HomeLogin::Replace
    );
    // ~/.claude refreshed instead: it keeps its copy.
    assert_eq!(
        store_less_home(&old, Some(&new), Some(true)),
        HomeLogin::Keep
    );
    // One refresh-token line, identities unknown: freshness decides.
    assert_eq!(
        store_less_home(&same_line_newer, Some(&old), None),
        HomeLogin::Replace
    );
    assert_eq!(
        store_less_home(&old, Some(&same_line_newer), None),
        HomeLogin::Keep
    );
    // Another account's login, or one csm cannot place: kept.
    assert_eq!(
        store_less_home(&new, Some(&old), Some(false)),
        HomeLogin::Keep
    );
    assert_eq!(store_less_home(&new, Some(&old), None), HomeLogin::Keep);
    // ~/.claude's copy holds MCP logins the floor's lacks: replacing it
    // would drop them from the live file, so it is kept.
    let mut h: Value = serde_json::from_str(&old).unwrap();
    h["mcpOAuth"] = json!({"srv-h": {"accessToken": "mcp-h"}});
    let home_more = h.to_string();
    assert_eq!(
        store_less_home(&new, Some(&home_more), Some(true)),
        HomeLogin::Keep
    );
    assert_eq!(
        store_less_home(&same_line_newer, Some(&home_more), None),
        HomeLogin::Keep
    );
    // The floor's holds them too: it replaces as before.
    let mut f: Value = serde_json::from_str(&new).unwrap();
    f["mcpOAuth"] = json!({"srv-h": {"accessToken": "mcp-h"}});
    assert_eq!(
        store_less_home(&f.to_string(), Some(&home_more), Some(true)),
        HomeLogin::Replace
    );
}

/// On Windows, where settle can never write a stash, a dir whose grant is
/// newer than its stash keeps it (the pending line says how to resolve
/// it); elsewhere it retires and settle carries the grant later.
#[test]
fn a_newer_grant_retires_only_where_settle_can_run() {
    assert!(newer_grant_gate(HostOs::MacOs, DirGrant::Newer, "acct-a").is_ok());
    assert!(newer_grant_gate(HostOs::Linux, DirGrant::Newer, "acct-a").is_ok());
    let e = newer_grant_gate(HostOs::Windows, DirGrant::Newer, "acct-a").unwrap_err();
    assert!(e.contains("acct-a") && e.contains("Orca"), "{e}");
    assert!(newer_grant_gate(HostOs::Windows, DirGrant::Superseded, "acct-a").is_ok());
}

#[test]
fn store_less_retire_carries_the_fresher_floor_login_home() {
    let ident = |uuid: &str| json!({"oauthAccount": oauth_json(uuid, "alice@example.com", None)});
    let old = creds_json("at-1", "rt-1", 10);
    let new = creds_json("at-2", "rt-2", 20);
    let world = |home: &Path, home_creds: Option<&str>, home_uuid: Option<&str>| {
        let (ctx, _, dir, _) = retire_world(home, &new);
        std::fs::write(dir.join(".claude.json"), ident("u-a").to_string()).unwrap();
        let d = home.join(".claude");
        std::fs::create_dir_all(&d).unwrap();
        if let Some(c) = home_creds {
            std::fs::write(d.join(".credentials.json"), c).unwrap();
        }
        if let Some(u) = home_uuid {
            std::fs::write(home.join(".claude.json"), ident(u).to_string()).unwrap();
        }
        (ctx, dir)
    };
    let read =
        |home: &Path| std::fs::read_to_string(home.join(".claude").join(".credentials.json"));

    // The same account, the floor's copy fresher: it replaces ~/.claude's.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (ctx, dir) = world(home, Some(&old), Some("u-a"));
    let line = retire_dir_store_less(&ctx, &dir, &|| Ok(())).unwrap();
    assert!(line.contains("replaced"), "{line}");
    assert_eq!(read(home).unwrap(), new);
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let by_fp = |g: &str| {
        q.list()
            .into_iter()
            .find(|m| m.fingerprint == quarantine::fingerprint(g))
            .map(|m| m.reason)
    };
    assert_eq!(by_fp(&old), Some(Reason::Superseded));
    assert_eq!(by_fp(&new), Some(Reason::Retired));
    assert!(home.join(".claude.work.retired").is_dir() && !dir.exists());
    for secret in ["at-1", "rt-1", "at-2", "rt-2"] {
        assert!(!line.contains(secret), "leaked {secret}: {line}");
    }

    // Another account's login in ~/.claude stays.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (ctx, dir) = world(home, Some(&old), Some("u-b"));
    retire_dir_store_less(&ctx, &dir, &|| Ok(())).unwrap();
    assert_eq!(read(home).unwrap(), old);
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    assert_eq!(q.list().len(), 1);

    // ~/.claude has none: the floor's moves there, with its identity.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (ctx, dir) = world(home, None, None);
    let line = retire_dir_store_less(&ctx, &dir, &|| Ok(())).unwrap();
    assert!(line.contains("moved"), "{line}");
    assert_eq!(read(home).unwrap(), new);
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(cfg["oauthAccount"]["accountUuid"], "u-a");
}

/// The phase ends only when nothing waits for settle and this run filed
/// no grant after its settle ran.
#[test]
fn retire_is_done_only_after_a_settle_saw_every_filing() {
    assert!(retire_done(0, false));
    assert!(!retire_done(1, false));
    assert!(!retire_done(0, true));
}

/// A grant the last retire filed after this run's settle gets a second
/// settle in the same run, so the phase can end there (the e2e expects
/// `done` after the run that retires the floor). Nothing waiting and
/// nothing filed: no second pass; something already waiting: none either
/// (the phase stays open anyway).
#[test]
fn retire_settles_again_after_filing_a_grant() {
    assert!(resettle_due(0, true));
    assert!(!resettle_due(0, false));
    assert!(!resettle_due(2, true));
}

/// Settle files a fresher grant that retire filed as `extra-logins` (the
/// dir also held MCP logins the stash lacked) into the stash, as it does
/// a plain retired one. MCP logins only the stash held go to the
/// quarantine first, so the whole-blob write loses none.
#[test]
fn settle_files_a_fresher_extra_logins_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut ctx, view, dir, _) = retire_world(tmp.path(), "");
    ctx.version_ok = true;
    std::fs::remove_dir_all(&dir).unwrap();
    let mut s: Value = serde_json::from_str(&creds_json("at-a", "rt-a", 9)).unwrap();
    s["mcpOAuth"] = json!({"srv-s": {"accessToken": "mcp-s"}});
    let stash_blob = s.to_string();
    let path = view
        .store
        .as_ref()
        .and_then(|st| st.account("acct-a"))
        .and_then(|r| r.managed_auth_path.clone());
    crate::orca::stash::Stash::open_for_write(&ctx.user_data.dir, "acct-a", path.as_deref())
        .unwrap()
        .write_credentials(&ctx.user_data.dir, HostOs::Linux, &stash_blob)
        .unwrap();
    let mut e: Value = serde_json::from_str(&creds_json("at-d", "rt-d", 50)).unwrap();
    e["mcpOAuth"] = json!({"srv-x": {"accessToken": "mcp-x"}});
    let entry = e.to_string();
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    q.file(&entry, Reason::ExtraLogins, "file", Some("acct-a"), None, 1)
        .unwrap();
    let http = FakeHttp::default().profile_uuid("at-d", "u-a");
    let procs = FakeProcs::default();
    let run = || {
        let mut st = super::state::MigrationState::default();
        let mut report = super::Report::default();
        let waiting = settle(
            &ctx,
            &view,
            &http,
            &procs,
            settle_opts(&[]),
            &mut st,
            &mut report,
        );
        (waiting, report)
    };
    let (waiting, report) = run();
    assert_eq!(waiting, 0, "{:?}", report.pending);
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), entry);
    let old = quarantine::fingerprint(&stash_blob);
    let m = q.list().into_iter().find(|m| m.fingerprint == old).unwrap();
    assert_eq!(m.reason, Reason::ExtraLogins);
    assert_eq!(m.matched_account.as_deref(), Some("acct-a"));
    assert_eq!(q.get(&old).unwrap().unwrap().expose(), stash_blob);
    for l in report.changed.iter().chain(&report.pending) {
        for secret in ["at-d", "rt-d", "mcp-x", "mcp-s"] {
            assert!(!l.contains(secret), "leaked {secret}: {l}");
        }
    }
    // A rerun finds nothing left to settle.
    let (waiting, report) = run();
    assert_eq!(waiting, 0);
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), entry);
    assert!(stash_keeps_more(&stash_blob, &entry));
    assert!(!stash_keeps_more(&entry, &entry));
}

/// A dir grant counts as superseded when the stash holds it or a newer
/// one, or when a read-back already filed it in the quarantine; a newer
/// unfiled grant or an unreadable Keychain item blocks.
#[test]
fn dir_grant_state_compares_with_the_stash() {
    let never = |_: &str| false;
    let with = facts(Some("alice@example.com"), Some(("fp-dir", 9.0)));
    let st = |fresher| Status::InOrca {
        id: "acct-a".into(),
        fresher,
    };
    assert_eq!(
        dir_grant_state(&with, &st(Some(false)), &never),
        DirGrant::Superseded
    );
    assert_eq!(
        dir_grant_state(&with, &st(Some(true)), &never),
        DirGrant::Newer
    );
    let filed = |fp: &str| fp == "fp-dir";
    assert_eq!(
        dir_grant_state(&with, &st(Some(true)), &filed),
        DirGrant::Superseded
    );
    assert_eq!(
        dir_grant_state(&with, &st(None), &never),
        DirGrant::Unreadable
    );
    let mut locked = with.clone();
    locked.grant_sources.push(KEYCHAIN_UNREADABLE);
    assert_eq!(
        dir_grant_state(&locked, &st(Some(false)), &never),
        DirGrant::Unreadable
    );
}

#[cfg(unix)]
#[test]
fn retire_refuses_a_link_to_the_default_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let link = home.join(".claude.work");
    std::os::unix::fs::symlink(home.join(".claude"), &link).unwrap();
    let in_orca = Status::InOrca {
        id: "acct-a".into(),
        fresher: Some(false),
    };
    assert!(
        retire_verdict(
            &in_orca,
            None,
            StashCheck::Verified,
            DirGrant::Superseded,
            &link,
            home,
            true
        )
        .is_err()
    );
}

/// A drain reports what collided and, with no collision left, removes
/// the source dir.
#[test]
fn drain_reports_collisions_and_removes_an_empty_source() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("s"), tmp.path().join("d"));
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::create_dir_all(dst.join("sub")).unwrap();
    std::fs::write(src.join("sub").join("x"), b"1").unwrap();
    std::fs::write(dst.join("sub").join("x"), b"2").unwrap();
    assert_eq!(drain(&src, &dst).unwrap(), vec![src.join("sub").join("x")]);
    std::fs::remove_file(src.join("sub").join("x")).unwrap();
    assert!(drain(&src, &dst).unwrap().is_empty());
    assert!(!src.exists());
    assert_eq!(collided_list(&[]), "");
    let many: Vec<PathBuf> = (0..12).map(|i| PathBuf::from(format!("p{i}"))).collect();
    assert!(collided_list(&many).ends_with("and 2 more"));
}

/// The plan over a Linux temp home: one profile in Orca with a fresher
/// dir grant, one to import, one without credentials. No secret in the
/// rendered text.
#[test]
fn plan_over_a_linux_home_without_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::Linux);
    let procs = FakeProcs::default();
    let ctx = Context::from_env(env.clone(), &procs);
    let ud = ctx.user_data.dir.clone();
    let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
    make_stash(
        &ud,
        "acct-a",
        Some(&alice),
        Some(creds_json("at-old", "rt-old", 1000).as_bytes()),
    );
    write_store(
        &ud,
        &[record_json(&ud, "acct-a", "alice@example.com", None)],
        Some("acct-a"),
    );

    let mk = |name: &str, email: Option<&str>, creds: Option<String>| {
        let dir = home.join(format!(".claude.{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(e) = email {
            std::fs::write(
                dir.join(".claude.json"),
                json!({"oauthAccount": {"emailAddress": e, "accountUuid": "u"},
                       "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": true}}})
                .to_string(),
            )
            .unwrap();
        }
        if let Some(c) = creds {
            std::fs::write(dir.join(".credentials.json"), c).unwrap();
        }
        dir
    };
    let work = mk(
        "work",
        Some("alice@example.com"),
        Some(creds_json("at-new", "rt-new", 2000)),
    );
    let home_dir = mk(
        "home",
        Some("bob@example.com"),
        Some(creds_json("at-b", "rt-b", 5)),
    );
    let spare = mk("spare", None, None);
    let cfg = legacy_dir(home);
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("profiles.json"),
        json!({"work": work, "home": home_dir, "spare": spare}).to_string(),
    )
    .unwrap();
    std::fs::write(cfg.join("default"), "work\n").unwrap();

    let legacy = load_legacy(home).unwrap();
    let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
    let plan = build_plan(&ctx, &view, &legacy, Probe::Read);
    let by = |n: &str| {
        plan.rows
            .iter()
            .find(|r| r.facts.name == n)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(
        by("work"),
        Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(true)
        }
    );
    assert_eq!(by("home"), Status::ToImport);
    assert_eq!(by("spare"), Status::NoCredentials);
    // No ~/.claude.json yet: the floor profile's file (minus
    // oauthAccount) seeds it, trust fields included, so nothing is left
    // to merge on top.
    assert_eq!(plan.seeded, Some(1));
    assert!(plan.merge.is_empty(), "{:?}", plan.merge);
    assert_eq!(plan.target_d, home.join(".claude"));
    let text = render_plan(&plan);
    assert!(text.contains("work [floor]"), "{text}");
    for secret in ["at-new", "rt-new", "at-old", "rt-old", "at-b", "rt-b"] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}

/// A Linux world for retire: account `acct-a` (uuid `u-a`) with a
/// stashed grant, and a legacy dir `~/.claude.work` holding `dir_grant`.
fn retire_world(home: &Path, dir_grant: &str) -> (Context, OrcaView, PathBuf, String) {
    let env = HostEnv::for_test(home, HostOs::Linux);
    let procs = FakeProcs::default();
    let ctx = Context::from_env(env.clone(), &procs);
    let ud = ctx.user_data.dir.clone();
    let stash_grant = creds_json("at-a", "rt-a", 9);
    make_stash(
        &ud,
        "acct-a",
        Some(
            oauth_json("u-a", "alice@example.com", None)
                .to_string()
                .as_bytes(),
        ),
        Some(stash_grant.as_bytes()),
    );
    write_store(
        &ud,
        &[record_json(&ud, "acct-a", "alice@example.com", None)],
        Some("acct-a"),
    );
    let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
    let dir = home.join(".claude.work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".credentials.json"), dir_grant).unwrap();
    (ctx, view, dir, stash_grant)
}

/// Retiring a Linux profile files its grant in the quarantine before the
/// file goes, then renames the dir. The stash's own grant needs no
/// profile call.
#[test]
fn retire_dir_quarantines_then_renames() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let grant = creds_json("at-a", "rt-a", 9);
    let (ctx, view, dir, _) = retire_world(home, &grant);
    let http = FakeHttp::default();
    let line = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(()))
        .unwrap()
        .line;
    assert!(line.contains("1 grant(s)"), "{line}");
    assert!(http.profile_calls.lock().unwrap().is_empty());
    assert!(!dir.exists());
    let retired = home.join(".claude.work.retired");
    assert!(retired.is_dir() && !retired.join(".credentials.json").exists());
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let list = q.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, Reason::Retired);
    assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));
    assert_eq!(list[0].fingerprint, quarantine::fingerprint(&grant));
    assert_eq!(
        q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
        grant
    );
    // A second retire refuses: the .retired name is taken.
    std::fs::create_dir_all(&dir).unwrap();
    assert!(retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).is_err());
}

/// Round 8: a dir copy of the stash's grant (same refresh token) that
/// also holds MCP logins the stash lacks is filed as `extra-logins`,
/// never as a plain retired copy `doctor --fix` would purge as
/// superseded, and the output names the logins but no token.
#[test]
fn retire_dir_keeps_mcp_logins_the_stash_lacks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let mut v: Value = serde_json::from_str(&creds_json("at-a", "rt-a", 9)).unwrap();
    v["mcpOAuth"] = json!({"srv-a": {"accessToken": "mcp-tok"}});
    let grant = v.to_string();
    let (ctx, view, dir, _) = retire_world(home, &grant);
    let http = FakeHttp::default();
    let line = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(()))
        .unwrap()
        .line;
    assert!(http.profile_calls.lock().unwrap().is_empty());
    assert!(line.contains("mcpOAuth/srv-a"), "{line}");
    assert!(!line.contains("mcp-tok"), "{line}");
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let list = q.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, Reason::ExtraLogins);
    assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));
    assert_eq!(
        q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
        grant
    );
}

/// A dir grant that is not the stash's goes through the profile veto:
/// one of the stash's account is retired under it, another account's is
/// filed unattributed (never as `acct-a`'s), and no answer moves
/// nothing.
#[test]
fn retire_dir_attributes_every_grant() {
    // Another account's grant.
    let tmp = tempfile::tempdir().unwrap();
    let foreign = creds_json("at-b", "rt-b", 50);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &foreign);
    let http = FakeHttp::default().profile_uuid("at-b", "u-b");
    let line = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(()))
        .unwrap()
        .line;
    assert!(line.contains("1 not provably"), "{line}");
    let list = Quarantine::new(HostOs::Linux, &ctx.state).list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, Reason::ProfileMismatch);
    assert_eq!(list[0].matched_account, None);
    assert!(!dir.exists());

    // An older grant of the same account: retired under it.
    let tmp = tempfile::tempdir().unwrap();
    let older = creds_json("at-a0", "rt-a0", 1);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &older);
    let http = FakeHttp::default().profile_uuid("at-a0", "u-a");
    retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).unwrap();
    let list = Quarantine::new(HostOs::Linux, &ctx.state).list();
    assert_eq!(list[0].reason, Reason::Retired);
    assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));

    // No answer: nothing filed, nothing deleted, no rename.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, dir, _) = retire_world(tmp.path(), &foreign);
    let err = retire_dir(
        &ctx,
        &view,
        &FakeHttp::default(),
        &dir,
        "acct-a",
        &|| Ok(()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("nothing moved"), "{err}");
    assert!(dir.join(".credentials.json").is_file());
    assert!(Quarantine::new(HostOs::Linux, &ctx.state).list().is_empty());
}

#[test]
fn retire_filing_maps_the_veto() {
    use crate::orca::readback::Veto;
    let never = || -> Result<Veto, crate::orca::OrcaError> { panic!("no profile call") };
    assert_eq!(retire_filing(true, never), Ok(RetireFiling::Own));
    assert_eq!(
        retire_filing(false, || Ok(Veto::Owner)),
        Ok(RetireFiling::Own)
    );
    // A 401 is usually an access token that expired while the dir waited
    // to retire: filed under the account, for settle to refresh.
    assert_eq!(
        retire_filing(false, || Ok(Veto::Unauthorized)),
        Ok(RetireFiling::Expired)
    );
    assert_eq!(
        retire_filing(false, || Ok(Veto::Quarantine(
            Reason::ProfileMismatch,
            Some((200, Some("u-b".into())))
        ))),
        Ok(RetireFiling::Unattributed(
            Reason::ProfileMismatch,
            Some((200, Some("u-b".into())))
        ))
    );
    assert!(
        retire_filing(false, || Err(crate::orca::OrcaError::Network(
            "down".into()
        )))
        .is_err()
    );
}

/// A dir grant whose access token got a 401 (it expired while the dir
/// waited to retire) is filed as `Retired` under the dir's account with
/// the 401 recorded, never as no one's, so settle can refresh it.
#[test]
fn retire_files_an_expired_grant_under_its_account() {
    let tmp = tempfile::tempdir().unwrap();
    let newer = creds_json("at-d", "rt-d", 50);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &newer);
    let http = FakeHttp::default().profile("at-d", FakeHttp::reply(401, "{}"));
    retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).unwrap();
    let list = Quarantine::new(HostOs::Linux, &ctx.state).list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, Reason::Retired);
    assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));
    assert_eq!(list[0].profile_status, Some(401));
    assert!(!dir.exists());
}

fn settle_opts(legacy_dirs: &[PathBuf]) -> SettleOpts<'_> {
    SettleOpts {
        dry_run: false,
        network_due: true,
        now: 1,
        legacy_dirs,
        refresh: true,
    }
}

fn stash_creds(ctx: &Context, view: &OrcaView, id: &str) -> String {
    let path = view
        .store
        .as_ref()
        .and_then(|s| s.account(id))
        .and_then(|r| r.managed_auth_path.clone());
    crate::orca::stash::Stash::open(&ctx.user_data.dir, id, path.as_deref())
        .unwrap()
        .credentials(ctx.os())
        .unwrap()
        .unwrap()
        .expose()
        .to_owned()
}

/// Settle on a retired grant whose access token got a 401: once nothing
/// else holds its refresh token it refreshes it, files the rotated grant
/// first, stores it after its profile names the account and drops both
/// entries. A refused refresh refiles it as unauthorized; a refresh token
/// still held by a legacy dir is not refreshed at all.
#[test]
fn settle_refreshes_an_expired_retired_grant() {
    let reply = r#"{"access_token":"at-rot","refresh_token":"rt-rot","expires_in":3600}"#;
    let newer = creds_json("at-d", "rt-d", 50);
    let world = |home: &Path| {
        let (mut ctx, view, dir, stash) = retire_world(home, "");
        ctx.version_ok = true;
        std::fs::remove_dir_all(&dir).unwrap();
        let q = Quarantine::new(HostOs::Linux, &ctx.state);
        let fp = q
            .file(
                &newer,
                Reason::Retired,
                "file",
                Some("acct-a"),
                Some((401, None)),
                1,
            )
            .unwrap()
            .fingerprint()
            .to_owned();
        (ctx, view, dir, stash, q, fp)
    };
    let procs = FakeProcs::default();

    // Refreshed and stored.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, dir, _, q, _) = world(tmp.path());
    let http = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(200, reply))
        .profile_uuid("at-rot", "u-a");
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(std::slice::from_ref(&dir)),
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 0, "{:?}", report.pending);
    assert!(stash_creds(&ctx, &view, "acct-a").contains("at-rot"));
    assert!(q.list().is_empty(), "{:?}", q.list());
    assert_eq!(http.token_bodies.lock().unwrap().len(), 1);
    assert!(report.changed.iter().any(|l| l.contains("refreshed")));
    for l in report.changed.iter().chain(&report.pending) {
        for secret in ["at-rot", "rt-rot", "at-d", "rt-d"] {
            assert!(!l.contains(secret), "leaked {secret}: {l}");
        }
    }

    // The refresh is refused: refiled as unauthorized, the stash kept.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, _, stash, q, fp) = world(tmp.path());
    let http = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(400, r#"{"error":"invalid_grant"}"#));
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(&[]),
        &mut st,
        &mut report,
    );
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    let list = q.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].fingerprint, fp);
    assert_eq!(list[0].reason, Reason::Unauthorized);

    // A legacy dir still holds the refresh token: no refresh.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, dir, stash, q, fp) = world(tmp.path());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".credentials.json"), &newer).unwrap();
    let http = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(200, reply));
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(std::slice::from_ref(&dir)),
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 1);
    assert!(http.token_bodies.lock().unwrap().is_empty());
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    assert_eq!(q.list()[0].fingerprint, fp);
    assert_eq!(q.list()[0].reason, Reason::Retired);
    assert!(
        report.pending.iter().any(|l| l.contains("still held")),
        "{:?}",
        report.pending
    );

    // The refreshed grant profiles as another account: the rotated copy
    // and the entry are both refiled with no account and the answer, so
    // nothing reads them as acct-a's; the stash is kept.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, _, stash, q, fp) = world(tmp.path());
    let http = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(200, reply))
        .profile_uuid("at-rot", "u-other");
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(&[]),
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 0, "{:?}", report.pending);
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    let list = q.list();
    assert_eq!(list.len(), 2, "{list:?}");
    for m in &list {
        assert_eq!(m.matched_account, None, "{m:?}");
        assert_eq!(m.reason, Reason::ProfileMismatch, "{m:?}");
    }
    let rotated = list.iter().find(|m| m.fingerprint != fp).unwrap();
    assert_eq!(rotated.profile_account_uuid.as_deref(), Some("u-other"));
    assert_eq!(rotated.profile_status, Some(200));
    // Unattributed, they no longer wait for settle.
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(&[]),
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 0);
    assert!(report.changed.is_empty(), "{:?}", report.changed);
}

/// Settle's profile veto on a filed grant (no refresh): the entry is
/// refiled with no account and the profile answer.
#[test]
fn settle_unattributes_a_grant_of_another_account() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut ctx, view, dir, stash) = retire_world(tmp.path(), "");
    ctx.version_ok = true;
    std::fs::remove_dir_all(&dir).unwrap();
    let newer = creds_json("at-d", "rt-d", 50);
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let fp = q
        .file(&newer, Reason::Retired, "file", Some("acct-a"), None, 1)
        .unwrap()
        .fingerprint()
        .to_owned();
    let http = FakeHttp::default().profile_uuid("at-d", "u-other");
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &FakeProcs::default(),
        settle_opts(&[]),
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 0);
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    let m = q.list().into_iter().find(|m| m.fingerprint == fp).unwrap();
    assert_eq!(m.matched_account, None);
    assert_eq!(m.reason, Reason::ProfileMismatch);
    assert_eq!(m.profile_account_uuid.as_deref(), Some("u-other"));
    assert_eq!(q.get(&fp).unwrap().unwrap().expose(), newer);
}

#[test]
fn settle_refresh_needs_a_refresh_token_no_one_else_holds() {
    let g = creds_json("at", "rt-1", 1);
    assert!(settle_refresh_allowed(&g, &[]).is_ok());
    assert!(settle_refresh_allowed(&g, &["rt-2".into()]).is_ok());
    assert!(settle_refresh_allowed(&g, &["rt-1".into()]).is_err());
    assert!(settle_refresh_allowed(r#"{"claudeAiOauth":{"accessToken":"at"}}"#, &[]).is_err());
    assert!(settle_reason(Reason::Retired));
    assert!(settle_reason(Reason::Rotated));
    assert!(settle_reason(Reason::Cutover));
    // A fresher grant a retire filed with MCP logins the stash lacked.
    assert!(settle_reason(Reason::ExtraLogins));
    assert!(!settle_reason(Reason::Unauthorized));
    assert!(!settle_reason(Reason::ProfileMismatch));
}

/// A retire that filed and deleted the dir's grants but died before the
/// rename: the rerun sees no grant, finds the account in Orca through
/// the dir's identity, and renames.
#[test]
fn a_retire_that_died_before_the_rename_completes_on_a_rerun() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (ctx, view, dir, _) = retire_world(home, "");
    std::fs::remove_file(dir.join(".credentials.json")).unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"oauthAccount": oauth_json("u-a", "alice@example.com", None)}).to_string(),
    )
    .unwrap();
    let p = LegacyProfile {
        name: "work".into(),
        dir: dir.clone(),
    };
    let facts = profile_facts(&ctx, &p, Probe::Read);
    let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    let status = classify(&facts, &host, &|_| None);
    assert_eq!(status, Status::NoCredentials);
    let known = identity_match(&facts, &host).map(|r| r.id.clone());
    let id = retire_verdict(
        &status,
        known.as_deref(),
        StashCheck::Unverified,
        dir_grant_state(&facts, &status, &|_| false),
        &dir,
        home,
        facts.exists,
    )
    .unwrap();
    assert_eq!(id, RetireAs::Account("acct-a".into()));
    let line = retire_dir(
        &ctx,
        &view,
        &FakeHttp::default(),
        &dir,
        "acct-a",
        &|| Ok(()),
    )
    .unwrap()
    .line;
    assert!(line.contains("0 grant(s)"), "{line}");
    assert!(home.join(".claude.work.retired").is_dir() && !dir.exists());
}

#[test]
fn unset_floor_env_is_inert_under_test() {
    assert!(unset_floor_env().is_ok());
}

#[test]
fn remove_legacy_files_only_touches_the_three_files() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = legacy_dir(tmp.path());
    std::fs::create_dir_all(&cfg).unwrap();
    for f in ["profiles.json", "default", "other"] {
        std::fs::write(cfg.join(f), b"x").unwrap();
    }
    let removed = remove_legacy_files(tmp.path()).unwrap();
    assert_eq!(removed.len(), 2);
    assert!(cfg.join("other").exists());
}

// ─── round 7: fail-closed steps and retire's references ─────────────────

/// A floor `.claude.json` that cannot be read or is not an object is
/// an error, never "nothing to merge"; only a missing file is.
#[test]
fn an_unreadable_floor_config_is_not_nothing_to_merge() {
    assert_eq!(floor_config_from(Ok(None)), Ok(None));
    let (m, b) = floor_config_from(Ok(Some(b"{\"a\":1}".to_vec())))
        .unwrap()
        .unwrap();
    assert_eq!(m["a"], 1);
    assert_eq!(b, b"{\"a\":1}");
    let bad = floor_config_from(Ok(Some(b"{bad".to_vec()))).unwrap_err();
    assert!(bad.contains("not a JSON object"), "{bad}");
    let err = floor_config_from(Err(io::Error::from(io::ErrorKind::PermissionDenied))).unwrap_err();
    assert!(err.contains("cannot be read"), "{err}");
}

/// B2 fails on an unparseable floor config (even with a merge recorded
/// for an earlier version of the file) and the plan says so. Stage C
/// runs a last B2 before it renames the floor dir and holds the dir on
/// that failure, so trust and MCP settings are never left behind
/// unreported.
#[test]
fn an_unparseable_floor_config_fails_b2() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let ctx = Context::from_env(
        HostEnv::for_test(home, HostOs::Linux),
        &FakeProcs::default(),
    );
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    let good = json!({"mcpServers": {"docs": {"command": "x"}}}).to_string();
    std::fs::write(work.join(".claude.json"), &good).unwrap();
    let legacy = Legacy {
        profiles: vec![LegacyProfile {
            name: "work".into(),
            dir: work.clone(),
        }],
        floor: Some("work".into()),
    };
    assert!(b2(&ctx, &legacy).unwrap().is_some());
    // The floor config is truncated since.
    std::fs::write(work.join(".claude.json"), &good[..good.len() / 2]).unwrap();
    let e = b2(&ctx, &legacy).unwrap_err().to_string();
    assert!(e.contains("not a JSON object"), "{e}");
    let plan = Plan {
        rows: Vec::new(),
        orca_running: false,
        orca_d: None,
        target_d: home.join(".claude"),
        target_config: home.join(".claude.json"),
        merge: Vec::new(),
        seeded: None,
        merge_blocked: read_floor_config(&work).err(),
        differs: Vec::new(),
        shared: Vec::new(),
        plugin_paths: 0,
        left_behind: Vec::new(),
        state_dir: ctx.state.clone(),
        smart: None,
    };
    let out = render_plan(&plan);
    assert!(out.contains("cannot merge"), "{out}");
    assert!(!out.contains("(nothing)"), "{out}");
    // No floor config at all: nothing to merge, nothing held back.
    std::fs::remove_file(work.join(".claude.json")).unwrap();
    assert!(b2(&ctx, &legacy).unwrap().is_none());
}

/// A row whose Keychain could not be read fails in import instead of
/// being skipped as "not fresher": the partial read decides nothing.
#[test]
fn an_unreadable_keychain_fails_the_import_row() {
    let mut f = facts(Some("alice@example.com"), Some(("fp-file", 10.0)));
    assert_eq!(unreadable_row(&f), None);
    f.grant_sources.push(KEYCHAIN_UNREADABLE);
    let why = unreadable_row(&f).unwrap();
    assert!(why.contains("Keychain"), "{why}");
    // The same row classified alone would have been a silent skip.
    let rec = [rec("id-a", "alice@example.com")];
    let st = classify(&f, &rec, &|_| {
        Some(GrantFacts {
            fingerprint: "fp-stash".into(),
            expires_at: Some(20.0),
            side: BTreeMap::new(),
        })
    });
    assert_eq!(row_action(&st, false), RowAction::Skip);
}

/// A claude an earlier csm started counts while it runs with the
/// recorded start; a reused pid or a dead one does not.
#[test]
fn a_live_child_of_an_earlier_csm_is_found_by_its_pidfile() {
    let files = [(10, 1000), (20, 1000), (30, 1000)];
    let st = |pid: u32| match pid {
        10 => None,       // gone
        20 => Some(5000), // pid reused later
        30 => Some(1002), // the supervised claude
        _ => None,
    };
    assert_eq!(legacy_supervised_child(&files, st), Some(30));
    assert_eq!(legacy_supervised_child(&files[..2], st), None);
    assert_eq!(legacy_supervised_child(&[], st), None);
}

/// Another csm process is found, but never this one or an ancestor.
#[test]
fn another_csm_process_is_found_but_not_this_one() {
    use crate::orca::testsupport::proc_info;
    let mut me = proc_info(100, "csm", Some("/usr/local/bin/csm"), &["migrate"]);
    me.ppid = Some(90);
    let mut parent = proc_info(90, "csm", Some("/usr/local/bin/csm"), &["claude"]);
    parent.ppid = Some(1);
    let shell = proc_info(80, "zsh", Some("/bin/zsh"), &[]);
    let table = vec![me.clone(), parent.clone(), shell.clone()];
    assert_eq!(other_csm(&table, 100, None), None);
    let sup = proc_info(200, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
    let mut table = table;
    table.push(sup);
    assert_eq!(other_csm(&table, 100, None), Some(200));
    // With a start bound, only a csm started before it counts: the old
    // binary, not a new csm launched after the migration was recorded.
    let mut older = proc_info(210, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
    older.start_time = 500;
    let mut newer = proc_info(220, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
    newer.start_time = 2000;
    let bounded = vec![me.clone(), newer.clone()];
    assert_eq!(other_csm(&bounded, 100, Some(1000)), None);
    assert_eq!(other_csm(&bounded, 100, None), Some(220));
    let bounded = vec![me.clone(), newer, older];
    assert_eq!(other_csm(&bounded, 100, Some(1000)), Some(210));
    let win = proc_info(300, "csm.exe", None, &["run"]);
    assert_eq!(other_csm(&[me, win], 100, None), Some(300));
    let claude = proc_info(400, "claude", Some("/Users/example/.local/bin/claude"), &[]);
    assert_eq!(other_csm(&[claude, shell], 100, None), None);
}

#[test]
fn names_path_needs_a_path_boundary() {
    let n = "/Users/example/.claude.work";
    assert!(names_path(
        "bash /Users/example/.claude.work/statusline.sh",
        n
    ));
    assert!(names_path(
        "csm hook --owner '/Users/example/.claude.work'",
        n
    ));
    assert!(names_path("/Users/example/.claude.work", n));
    assert!(!names_path("/Users/example/.claude.work2/x", n));
    assert!(!names_path("/Users/example/.claude.work.retired/x", n));
    assert!(!names_path("/Users/example/.claude/x", n));
    assert!(!names_path("anything", ""));
}

#[test]
fn dir_needles_cover_the_home_shorthands() {
    let home = Path::new("/Users/example");
    let n = dir_needles(
        Path::new("/Users/example/.claude.work/"),
        Some(Path::new("/Volumes/Data/example/.claude.work")),
        home,
    );
    assert_eq!(
        n,
        vec![
            "/Users/example/.claude.work",
            "~/.claude.work",
            "$HOME/.claude.work",
            "${HOME}/.claude.work",
            "/Volumes/Data/example/.claude.work",
        ]
    );
}

/// Retire refuses while ~/.claude's settings or an MCP server in
/// ~/.claude.json name the dir, and a project key alone never counts.
#[test]
fn retire_refuses_while_settings_or_servers_name_the_dir() {
    let needles = dir_needles(
        Path::new("/Users/example/.claude.work"),
        None,
        Path::new("/Users/example"),
    );
    let settings = json!({
        "statusLine": {"type": "command", "command": "bash ~/.claude.work/statusline-command.sh"},
        "hooks": {"PreToolUse": [{"hooks": [{"command": "/Users/example/.claude.work/hooks/guard.sh"}]}]},
        "env": {"X": "/Users/example/.claude.work2"}
    })
    .to_string();
    let cfg = json!({
        "mcpServers": {"docs": {"command": "$HOME/.claude.work/mcp/docs"}},
        "projects": {
            "/Users/example/.claude.work": {"hasTrustDialogAccepted": true},
            "/Users/example/src": {"mcpServers": {"web": {"args": ["/Users/example/.claude.work/web.js"]}}}
        }
    })
    .to_string();
    let refs = dir_references(
        &[
            ("~/.claude/settings.json", settings.as_str()),
            (
                "~/.claude/settings.local.json",
                "not json: ~/.claude.work/x",
            ),
        ],
        Some(&cfg),
        &needles,
    );
    assert_eq!(
        refs,
        vec![
            "~/.claude/settings.json (statusLine.command)",
            "~/.claude/settings.json (hooks.PreToolUse[0].hooks[0].command)",
            "~/.claude/settings.local.json",
            "~/.claude.json (mcpServers.docs.command)",
            "~/.claude.json (projects[/Users/example/src].mcpServers.web.args[0])",
        ]
    );
    let why = references_verdict(&refs).unwrap_err();
    assert!(why.contains("statusLine.command"), "{why}");
    assert!(why.contains('…'), "{why}");
    assert_eq!(references_verdict(&[]), Ok(()));
    // Nothing names it: nothing to refuse.
    let clean = dir_references(
        &[(
            "~/.claude/settings.json",
            "{\"statusLine\":{\"command\":\"csm statusline\"}}",
        )],
        Some("{\"projects\":{\"/Users/example/.claude.work\":{}}}"),
        &needles,
    );
    assert!(clean.is_empty(), "{clean:?}");
}

/// The machine shell reads ~/.claude's files under the test home.
#[test]
fn references_to_reads_the_files_under_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    assert!(references_to(home, &work).is_empty());
    std::fs::write(
        home.join(".claude").join("settings.json"),
        json!({"statusLine": {"command": format!("bash {}/s.sh", work.display())}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        references_to(home, &work),
        vec!["~/.claude/settings.json (statusLine.command)"]
    );
}

// ─── dispatch: NONE never probes ──────────────────────────────────────────────

/// A test home with a legacy registry, so any probe would find work.
fn legacy_home() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let reg = tmp.path().join(".config").join("claude-as");
    std::fs::create_dir_all(&reg).unwrap();
    let work = tmp.path().join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        reg.join("profiles.json"),
        serde_json::to_string(&json!({ "work": work })).unwrap(),
    )
    .unwrap();
    std::fs::write(reg.join("default"), "work\n").unwrap();
    tmp
}

/// Every NONE-class invocation returns before the probe: no marker read,
/// no stat, no marker written. A NOTE word is the positive control.
#[test]
fn none_class_invocations_never_probe() {
    use crate::usage::reach;
    let home = legacy_home();
    crate::testenv::with_test_home(home.path(), || {
        let _ = reach::take();
        let none: &[(&str, &[&str])] = &[
            ("hook", &["stop"]),
            ("statusline", &[]),
            ("usage", &["capture"]),
            ("usage", &["--help"]),
            ("claude", &["-p", "hi"]),
            ("cas", &["--eval"]),
            ("scan", &[]),
            ("sidecar", &[]),
            ("reap", &[]),
            ("completions", &["zsh"]),
            ("newuuid", &[]),
            ("config", &["show"]),
            ("--version", &[]),
            ("--help", &[]),
            // Launches (Print included) classify themselves in `run`, and
            // `migrate` runs itself.
            ("run", &["-p", "hi"]),
            ("migrate", &[]),
        ];
        for (word, rest) in none {
            super::at_dispatch(word, &os(rest));
            let seen = reach::take();
            assert!(
                !seen.contains(&"migrate-probe"),
                "{word} {rest:?}: {seen:?}"
            );
        }
        let state = crate::paths::smart_dir_no_create();
        assert!(!state.join(super::state::MARKER).exists());
        // NOTE probes (and notes once a day).
        super::at_dispatch("usage", &[]);
        assert!(reach::take().contains(&"migrate-probe"));
        // Its day is kept beside, never in, a marker: a new marker would
        // read as a migration under way.
        assert!(!state.join(super::state::MARKER).exists());
        let shown = super::state::load_note_file(&state).expect("the note's day");
        assert!(!super::state::note_due(
            &BTreeMap::from([("note:pending".to_owned(), shown)]),
            "note:pending",
            shown + 60
        ));
        super::at_dispatch("usage", &[]);
        assert!(!state.join(super::state::MARKER).exists());
        assert_eq!(super::state::load_note_file(&state), Some(shown));
    });
}

/// With nothing legacy, the first run writes a `done` marker and the next
/// finds it.
#[test]
fn a_fresh_machine_gets_a_done_marker() {
    let tmp = tempfile::tempdir().unwrap();
    crate::testenv::with_test_home(tmp.path(), || {
        let r = super::run(false);
        assert!(!r.legacy);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let state = crate::paths::smart_dir_no_create();
        let m = super::state::load(&state).expect("marker");
        assert_eq!(m.phase, super::state::Phase::Done);
        let r = super::run(false);
        assert!(!r.legacy);
        assert_eq!(r.phase, Some(super::state::Phase::Done));
        assert!(!super::render(&r).is_empty());
    });
}

/// Only an inherited `~/.claude.<x>` pin brought the probe here: no
/// registry, no recorded profile, no `~/.claude.shared`. Nothing to adopt,
/// so no stage runs: `~/.claude`'s own login stays as it is and the
/// marker says done. A recorded profile, the shared dir or a cutover keep
/// the migration going.
#[test]
fn an_inherited_pin_alone_is_nothing_to_migrate() {
    use super::state::{Cutover, MigrationState, Phase, SnapProfile, Snapshot};
    let empty = Snapshot::default();
    assert!(super::nothing_recorded(&empty, false, false));
    assert!(!super::nothing_recorded(&empty, true, false));
    assert!(!super::nothing_recorded(&empty, false, true));
    let one = Snapshot {
        profiles: vec![SnapProfile {
            name: "work".into(),
            dir: PathBuf::from("/Users/example/.claude.work"),
        }],
        floor: None,
        seen_at: None,
    };
    assert!(!super::nothing_recorded(&one, false, false));

    // The marker an earlier run left for such a pin: under way, empty.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let d = home.join(".claude");
    std::fs::create_dir_all(&d).unwrap();
    let creds = r#"{"claudeAiOauth":{"refreshToken":"rt-own","expiresAt":1}}"#;
    std::fs::write(d.join(".credentials.json"), creds).unwrap();
    let cfg = r#"{"oauthAccount":{"accountUuid":"u-own"}}"#;
    std::fs::write(home.join(".claude.json"), cfg).unwrap();
    std::fs::create_dir_all(home.join(".claude.personal")).unwrap();
    crate::testenv::with_test_home(home, || {
        let state = crate::paths::smart_dir_no_create();
        std::fs::create_dir_all(&state).unwrap();
        let m = MigrationState {
            phase: Phase::Carry,
            legacy: Some(Snapshot::default()),
            ..MigrationState::default()
        };
        super::state::save(&state, &m).unwrap();
        let r = super::run(false);
        assert!(!r.legacy, "{r:?}");
        assert!(r.errors.is_empty() && r.changed.is_empty(), "{r:?}");
        assert_eq!(super::state::load(&state).unwrap().phase, Phase::Done);
        // ~/.claude's login is untouched.
        assert_eq!(
            std::fs::read_to_string(d.join(".credentials.json")).unwrap(),
            creds
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".claude.json")).unwrap(),
            cfg
        );
        assert!(home.join(".claude.personal").is_dir());
        // A recorded cutover is not undone by this rule.
        let m = MigrationState {
            phase: Phase::Retire,
            legacy: Some(Snapshot::default()),
            cutover: Some(Cutover {
                at: 1,
                boot_id: None,
            }),
            ..MigrationState::default()
        };
        assert!(!super::nothing_recorded(
            m.legacy.as_ref().unwrap(),
            false,
            m.cutover.is_some()
        ));
    });
}

/// A dry run on a fresh machine writes nothing.
#[test]
fn a_dry_run_writes_no_marker() {
    let tmp = tempfile::tempdir().unwrap();
    crate::testenv::with_test_home(tmp.path(), || {
        let r = super::run(true);
        assert!(!r.legacy);
        let state = crate::paths::smart_dir_no_create();
        assert!(super::state::load(&state).is_none());
    });
}

#[test]
fn unregistered_dirs_are_listed_not_touched() {
    let home = Path::new("/Users/example");
    let names: Vec<String> = [
        ".claude.work",
        ".claude.old",
        ".claude.json",
        ".claude.shared",
        ".claude.home.retired",
        ".claude",
        ".config",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let known = vec![home.join(".claude.work")];
    assert_eq!(
        super::unregistered(&names, home, &known),
        vec![home.join(".claude.old")]
    );
}

#[test]
fn the_snapshot_keeps_dirs_the_registry_dropped() {
    let snap = super::state::Snapshot {
        profiles: vec![super::state::SnapProfile {
            name: "work".into(),
            dir: PathBuf::from("/Users/example/.claude.work"),
        }],
        floor: Some("work".into()),
        seen_at: Some(1_700_000_000),
    };
    let now = Legacy {
        profiles: vec![LegacyProfile {
            name: "home".into(),
            dir: PathBuf::from("/Users/example/.claude.home"),
        }],
        floor: None,
    };
    let m = super::merge_snapshot(Some(&snap), &now, || 1_800_000_000);
    assert_eq!(
        m.dirs(),
        vec![
            PathBuf::from("/Users/example/.claude.home"),
            PathBuf::from("/Users/example/.claude.work")
        ]
    );
    assert_eq!(m.floor.as_deref(), Some("work"));
    // The first detection's time stays; a first snapshot records `at`,
    // an older marker without one stays without.
    assert_eq!(m.seen_at, Some(1_700_000_000));
    assert_eq!(
        super::merge_snapshot(None, &now, || 1_800_000_000).seen_at,
        Some(1_800_000_000)
    );
    let old = super::state::Snapshot {
        seen_at: None,
        ..snap
    };
    assert_eq!(
        super::merge_snapshot(Some(&old), &now, || 1_800_000_000).seen_at,
        None
    );
}

#[test]
fn a_terminal_run_prints_one_pending_line_a_day() {
    let r = super::Report {
        legacy: true,
        changed: vec!["work: imported alice@example.com".into()],
        pending: vec!["reason a".into(), "reason b".into()],
        ..super::Report::default()
    };
    let mut notes = BTreeMap::new();
    let (lines, keys) = super::terminal_lines(&r, &notes, 1_000);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("imported alice@example.com"));
    assert!(lines[1].contains("reason a"));
    notes.insert(keys[0].clone(), 1_000);
    let (lines, _) = super::terminal_lines(&r, &notes, 1_001);
    assert!(lines[1].contains("reason b"), "{lines:?}");
}

/// A line naming a pid or a count keeps one note key across runs, so it
/// shows once a day like any other; old line keys are pruned.
#[test]
fn a_pending_line_with_numbers_notes_once_a_day() {
    let at = |pid: u32, n: u32| super::Report {
        legacy: true,
        pending: vec![format!(
            "pid {pid} (claude) runs with CLAUDE_CONFIG_DIR=/Users/example/.claude.work; \
             {n} entries wait"
        )],
        ..super::Report::default()
    };
    let (_, keys) = super::terminal_lines(&at(4242, 3), &BTreeMap::new(), 1_000);
    assert_eq!(keys.len(), 1);
    assert_eq!(
        keys[0],
        "line:pid # (claude) runs with CLAUDE_CONFIG_DIR=/Users/example/.claude.work; # entries wait"
    );
    let notes = BTreeMap::from([(keys[0].clone(), 1_000)]);
    let (lines, _) = super::terminal_lines(&at(5151, 12), &notes, 1_060);
    assert!(lines.is_empty(), "{lines:?}");

    let day = super::state::NOTE_EVERY_SECS;
    let mut notes = BTreeMap::from([
        ("line:old".to_owned(), 0),
        ("line:recent".to_owned(), 7 * day - 1),
        ("note:pending".to_owned(), 0),
    ]);
    super::state::prune_notes(&mut notes, 7 * day);
    assert_eq!(
        notes.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["line:recent", "note:pending"]
    );
    const { assert!(super::state::NOTE_KEEP_SECS > super::state::NOTE_EVERY_SECS) };
}

// ─── ending a launch ──────────────────────────────────────────────────────────

/// Only a launch-bound run stops once claude has exited; `csm migrate` and
/// a terminal word always run on.
#[test]
fn only_launch_bound_runs_stop_after_the_child_exits() {
    use super::Trigger;
    let post = Trigger::PostSpawn {
        after_prespawn: true,
    };
    assert!(super::may_continue(post, false));
    assert!(!super::may_continue(post, true));
    assert!(!super::may_continue(Trigger::PreSpawn, true));
    assert!(super::may_continue(Trigger::Terminal, true));
    assert!(super::may_continue(
        Trigger::Explicit { dry_run: false },
        true
    ));
}

/// The exit waits for a run at most the grace, and past it only while a
/// critical section holds.
#[test]
fn the_exit_waits_a_bounded_grace_unless_a_step_is_critical() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;
    let no = || false;
    assert!(super::wait_bounded(&|| true, Duration::ZERO, &no));
    let t = Instant::now();
    assert!(!super::wait_bounded(
        &|| false,
        Duration::from_millis(50),
        &no
    ));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // A critical step holds the exit until it ends.
    let crit = Arc::new(AtomicBool::new(true));
    let c2 = Arc::clone(&crit);
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        c2.store(false, Ordering::SeqCst);
    });
    let t = Instant::now();
    assert!(!super::wait_bounded(
        &|| false,
        Duration::from_millis(10),
        &|| crit.load(Ordering::SeqCst)
    ));
    assert!(
        t.elapsed() >= Duration::from_millis(140),
        "{:?}",
        t.elapsed()
    );
    h.join().unwrap();
}

/// A launch that never spawned (the picker cancelled) gets the pre-spawn
/// worker's report when it ends within the grace, and gives up on one
/// still going.
#[test]
fn an_unspawned_launch_reports_or_leaves_its_prespawn_worker() {
    let report = super::Report {
        legacy: true,
        changed: vec!["work: imported alice@example.com".into()],
        ..super::Report::default()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let r2 = report.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let _ = tx.send(r2);
    });
    let got = super::settle_unspawned((h, rx), Duration::from_secs(5), &|| false);
    assert_eq!(got, Some(report));

    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (tx, rx) = std::sync::mpsc::channel::<super::Report>();
    let h = std::thread::spawn(move || {
        let _ = go_rx.recv();
        drop(tx);
    });
    let t = std::time::Instant::now();
    // Kept aside so the test can end the thread after the give-up.
    let (h2, rx2) = (h, rx);
    let finished = super::wait_bounded(&|| h2.is_finished(), Duration::from_millis(30), &|| false);
    assert!(!finished);
    assert!(t.elapsed() < Duration::from_secs(2));
    go_tx.send(()).unwrap();
    assert_eq!(
        super::settle_unspawned((h2, rx2), Duration::from_secs(5), &|| false),
        None,
        "a worker that sent nothing has no report"
    );
}

/// The run after the spawn goes to a thread of its own: starting it
/// returns at once, so the supervisor's join of the recovery never waits
/// for the migration.
#[test]
fn the_post_spawn_run_starts_without_blocking() {
    fn slow(_: super::Armed) {
        std::thread::sleep(Duration::from_millis(300));
    }
    let armed = super::Armed {
        worker: None,
        after_prespawn: false,
        child: super::LaunchChild::Unknown,
    };
    let t = std::time::Instant::now();
    let h = super::spawn_post_spawn(armed, slow).unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "{:?}",
        t.elapsed()
    );
    assert!(!super::wait_bounded(
        &|| h.is_finished(),
        Duration::from_millis(10),
        &|| false
    ));
    h.join().unwrap();
}

// ─── stage B ──────────────────────────────────────────────────────────────────

/// B2 as the carry stage runs it, I3 on.
fn b2(ctx: &Context, legacy: &Legacy) -> anyhow::Result<Option<String>> {
    config_step(
        &ctx.env.home,
        &ctx.state,
        legacy,
        ConfigOpts {
            dry_run: false,
            wait: Duration::from_secs(1),
            switch_wait: Duration::from_secs(1),
            i3: true,
        },
    )
}

/// Nothing live, and nothing a test just wrote counts as recent.
#[cfg(unix)]
fn cold() -> Hot {
    Hot::at(SystemTime::now() + Duration::from_secs(3600))
}

/// B1 over every name with [`cold`].
#[cfg(unix)]
fn b1_all(home: &Path) -> Vec<(&'static str, B1End)> {
    SHARED_NAMES
        .iter()
        .map(|n| {
            let aside = home.join("state").join("collided").join(n);
            (*n, b1_one(home, n, &cold(), false, &aside).unwrap())
        })
        .collect()
}

fn facts_of(local: LocalKind, shared: SharedSide) -> SharedFacts {
    SharedFacts {
        root: true,
        local,
        shared,
        same_fs: true,
        live: false,
        recent: false,
    }
}

/// B1's table: the four crash states (link and real shared: start; both
/// real: drain; real local and no shared: link; real local and compat
/// link: done), the states between them, and EXDEV.
#[test]
fn shared_step_table() {
    use LocalKind as L;
    use SharedSide as S;
    let step = |l, s| shared_step(&facts_of(l, s));
    // The four crash states.
    assert_eq!(step(L::LinkToShared, S::Dir), SharedStep::Start);
    assert_eq!(step(L::LinkToShared, S::File), SharedStep::Start);
    assert_eq!(step(L::RealDir, S::Dir), SharedStep::Drain);
    assert_eq!(step(L::RealFile, S::File), SharedStep::Append);
    assert_eq!(step(L::RealDir, S::Absent), SharedStep::Link);
    assert_eq!(step(L::RealFile, S::Absent), SharedStep::Link);
    assert_eq!(step(L::RealDir, S::Compat), SharedStep::Done);
    assert_eq!(step(L::RealFile, S::Compat), SharedStep::Done);
    // Windows' compat link for the history file is a hard link.
    assert_eq!(step(L::RealFile, S::SameFile), SharedStep::Done);
    // Between them: unlinked but not moved, a dangling link, nothing.
    assert_eq!(step(L::Absent, S::Dir), SharedStep::Move);
    assert_eq!(step(L::Absent, S::File), SharedStep::Move);
    assert_eq!(step(L::LinkToShared, S::Absent), SharedStep::DropLink);
    assert_eq!(step(L::Absent, S::Absent), SharedStep::Done);
    assert_eq!(step(L::Absent, S::Compat), SharedStep::Done);
    // Without ~/.claude.shared there is nothing to link.
    let mut f = facts_of(L::RealDir, S::Absent);
    f.root = false;
    assert_eq!(shared_step(&f), SharedStep::Done);
    // Never on its own.
    for (l, s) in [
        (L::OtherLink, S::Dir),
        (L::Other, S::Dir),
        (L::RealDir, S::OtherLink),
        (L::RealDir, S::Other),
        (L::RealDir, S::File),
        (L::RealFile, S::Dir),
        (L::LinkToShared, S::Compat),
    ] {
        assert!(
            matches!(step(l.clone(), s), SharedStep::Skip(_)),
            "{l:?} {s:?}"
        );
    }
    // A history file still being written waits.
    let mut f = facts_of(L::RealFile, S::File);
    f.recent = true;
    assert_eq!(shared_step(&f), SharedStep::Wait(RECENT_WAIT));
    // EXDEV: a move that would copy waits while a claude may run; with
    // none it copies. A link or an append never moves across.
    for (l, s, then) in [
        (L::LinkToShared, S::Dir, SharedStep::Start),
        (L::Absent, S::Dir, SharedStep::Move),
        (L::RealDir, S::Dir, SharedStep::Drain),
    ] {
        let mut f = facts_of(l, s);
        f.same_fs = false;
        f.live = true;
        assert_eq!(shared_step(&f), SharedStep::Wait(EXDEV_WAIT));
        f.live = false;
        assert_eq!(shared_step(&f), then);
    }
    let mut f = facts_of(L::RealDir, S::Absent);
    f.same_fs = false;
    f.live = true;
    assert_eq!(shared_step(&f), SharedStep::Link);
    // Every step has a line.
    assert!(step_line(&SharedStep::Wait(EXDEV_WAIT)).contains("filesystem"));
}

/// B1 on disk from each crash state: every name ends as the real entry in
/// `~/.claude` with a compat link in `~/.claude.shared`, so a profile
/// dir's link resolves in two hops, and a rerun does nothing.
#[cfg(unix)]
#[test]
fn b1_finishes_from_every_crash_state() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let d = home.join(".claude");
    let sh = shared_root(home);
    let work = home.join(".claude.work");
    for p in [&d, &sh, &work] {
        std::fs::create_dir_all(p).unwrap();
    }
    // Link and real shared (the start).
    std::fs::create_dir_all(sh.join("projects").join("-app")).unwrap();
    std::fs::write(sh.join("projects").join("-app").join("a.jsonl"), b"t").unwrap();
    symlink(sh.join("projects"), d.join("projects")).unwrap();
    symlink("../.claude.shared/projects", work.join("projects")).unwrap();
    // Unlinked, not moved yet.
    std::fs::create_dir_all(sh.join("shell-snapshots")).unwrap();
    std::fs::write(sh.join("shell-snapshots").join("s.sh"), b"s").unwrap();
    // Both real.
    std::fs::create_dir_all(d.join("todos")).unwrap();
    std::fs::create_dir_all(sh.join("todos")).unwrap();
    std::fs::write(d.join("todos").join("x.json"), b"[]").unwrap();
    std::fs::write(sh.join("todos").join("y.json"), b"[1]").unwrap();
    // Real local, no shared.
    std::fs::create_dir_all(d.join("session-env")).unwrap();
    // Real local and compat link (done).
    std::fs::create_dir_all(d.join("sessions")).unwrap();
    symlink(d.join("sessions"), sh.join("sessions")).unwrap();
    symlink(sh.join("sessions"), work.join("sessions")).unwrap();
    // A dangling link.
    symlink(sh.join("plugins"), d.join("plugins")).unwrap();

    let ends = b1_all(home);
    for (n, end) in &ends {
        assert!(matches!(end, B1End::Done(_)), "{n}: {end:?}");
    }
    let did = |n: &str| match &ends.iter().find(|(k, _)| *k == n).unwrap().1 {
        B1End::Done(v) => v.clone(),
        _ => unreachable!(),
    };
    assert!(did("sessions").is_empty(), "already done");
    assert!(!did("projects").is_empty());
    for n in [
        "projects",
        "shell-snapshots",
        "todos",
        "session-env",
        "sessions",
    ] {
        let m = std::fs::symlink_metadata(d.join(n)).unwrap();
        assert!(m.is_dir() && !m.file_type().is_symlink(), "{n}");
        assert_eq!(
            std::fs::read_link(sh.join(n)).unwrap(),
            d.join(n),
            "{n}: compat link"
        );
    }
    assert!(std::fs::symlink_metadata(d.join("plugins")).is_err());
    assert!(std::fs::symlink_metadata(sh.join("plugins")).is_err());
    assert!(d.join("todos").join("x.json").is_file());
    assert!(d.join("todos").join("y.json").is_file());
    // Two hops: the profile's link, then the compat link.
    assert_eq!(
        std::fs::read(work.join("projects").join("-app").join("a.jsonl")).unwrap(),
        b"t"
    );
    std::fs::write(work.join("sessions").join("9.json"), b"{}").unwrap();
    assert!(d.join("sessions").join("9.json").is_file());
    // I4: nothing was linked at a legacy dir path.
    assert_eq!(
        std::fs::read_link(work.join("projects")).unwrap(),
        Path::new("../.claude.shared/projects")
    );
    // A rerun does nothing.
    for (n, end) in b1_all(home) {
        assert_eq!(end, B1End::Done(Vec::new()), "{n}");
    }
    assert!(
        shared_plan(home)
            .iter()
            .all(|(_, s)| *s == SharedStep::Done)
    );
}

/// Both sides real: the smaller drains into the larger. A live session's
/// entries and fresh files stay until they rest (the step waits);
/// identical files go; a collision keeps both, the source's copy under
/// the aside dir.
#[cfg(unix)]
#[test]
fn b1_drain_skips_live_sessions_and_keeps_collisions() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (d, sh) = (home.join(".claude"), shared_root(home));
    let (lp, sp) = (d.join("projects").join("p"), sh.join("projects").join("p"));
    std::fs::create_dir_all(&lp).unwrap();
    std::fs::create_dir_all(d.join("projects").join("q")).unwrap();
    std::fs::create_dir_all(sp.join("abc")).unwrap();
    for (f, b) in [
        ("1.jsonl", "mine"),
        ("2.jsonl", "same"),
        ("4.jsonl", "4"),
        ("5.jsonl", "5"),
    ] {
        std::fs::write(lp.join(f), b).unwrap();
    }
    std::fs::write(d.join("projects").join("q").join("6.jsonl"), b"6").unwrap();
    for (f, b) in [
        ("1.jsonl", "theirs"),
        ("2.jsonl", "same"),
        ("3.jsonl", "new"),
    ] {
        std::fs::write(sp.join(f), b).unwrap();
    }
    std::fs::write(sp.join("abc.jsonl"), b"live").unwrap();
    std::fs::write(sp.join("abc").join("t.json"), b"{}").unwrap();

    let mut hot = cold();
    hot.sids.push("abc".into());
    let aside = home.join("state").join("collided").join("projects");
    let end = b1_one(home, "projects", &hot, false, &aside).unwrap();
    let B1End::Wait(why, did) = end else {
        panic!("{end:?}");
    };
    assert!(
        why.contains("2 entries") && why.contains("live session"),
        "{why}"
    );
    assert!(
        did.iter().any(|l| l.contains("different content")),
        "{did:?}"
    );
    assert_eq!(std::fs::read(lp.join("1.jsonl")).unwrap(), b"mine");
    assert_eq!(std::fs::read(lp.join("3.jsonl")).unwrap(), b"new");
    assert_eq!(
        std::fs::read(aside.join("p").join("1.jsonl")).unwrap(),
        b"theirs"
    );
    assert!(!sp.join("2.jsonl").exists(), "identical copy dropped");
    assert!(sp.join("abc.jsonl").is_file() && sp.join("abc").is_dir());
    // The session ended: the rest moves and the compat link follows.
    let end = b1_one(home, "projects", &cold(), false, &aside).unwrap();
    assert!(matches!(end, B1End::Done(_)), "{end:?}");
    assert_eq!(std::fs::read(lp.join("abc.jsonl")).unwrap(), b"live");
    assert!(lp.join("abc").join("t.json").is_file());
    assert_eq!(
        std::fs::read_link(sh.join("projects")).unwrap(),
        d.join("projects")
    );

    // A file changed in the last five minutes stays too.
    let (src, dst) = (tmp.path().join("s"), tmp.path().join("t"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(src.join("fresh"), b"x").unwrap();
    let mut out = Drained::default();
    drain_hot(
        &src,
        &dst,
        &Hot::at(SystemTime::now()),
        &tmp.path().join("aside"),
        true,
        &mut out,
    )
    .unwrap();
    assert_eq!(out.hot, 1);
    assert!(src.join("fresh").is_file() && !dst.join("fresh").exists());
    // A live pid's registry file is a live session's too.
    let mut hot = cold();
    hot.pids.push(4242);
    assert!(hot.names("4242.json"));
    assert!(!hot.names("42421.json"));
}

/// Both history files real: the shared lines go first, once, then a
/// compat link; a history written in the last five minutes waits.
#[cfg(unix)]
#[test]
fn b1_history_appends_once_and_waits_while_written() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (d, sh) = (home.join(".claude"), shared_root(home));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::create_dir_all(&sh).unwrap();
    std::fs::write(sh.join("history.jsonl"), b"{\"old\":1}\n").unwrap();
    std::fs::write(d.join("history.jsonl"), b"{\"new\":2}\n").unwrap();
    let aside = home.join("aside");
    let end = b1_one(
        home,
        "history.jsonl",
        &Hot::at(SystemTime::now()),
        false,
        &aside,
    )
    .unwrap();
    assert_eq!(end, B1End::Wait(RECENT_WAIT.to_owned(), Vec::new()));
    let end = b1_one(home, "history.jsonl", &cold(), false, &aside).unwrap();
    assert!(matches!(end, B1End::Done(ref v) if v.len() == 2), "{end:?}");
    assert_eq!(
        std::fs::read(d.join("history.jsonl")).unwrap(),
        b"{\"old\":1}\n{\"new\":2}\n"
    );
    assert_eq!(
        std::fs::read(sh.join("history.jsonl")).unwrap(),
        b"{\"old\":1}\n{\"new\":2}\n",
        "the compat link reads the one file"
    );
    assert_eq!(
        b1_one(home, "history.jsonl", &cold(), false, &aside).unwrap(),
        B1End::Done(Vec::new())
    );
}

/// B2's pure core: a missing file is seeded from the floor minus the
/// identity; an existing one gains the keys it lacks, never
/// `oauthAccount`, keeping every value it has; the other profiles add
/// trust and MCP keys only where the file has none.
#[test]
fn carry_config_from_adds_keys_and_never_the_identity() {
    let obj = |v: Value| v.as_object().unwrap().clone();
    let floor = obj(json!({
        "oauthAccount": {"emailAddress": "alice@example.com"},
        "userID": "u-floor",
        "theme": "dark",
        "projects": {"/w": {"hasTrustDialogAccepted": true}},
        "mcpServers": {"docs": {"command": "x"}}
    }));
    let other = obj(json!({
        "oauthAccount": {"emailAddress": "bob@example.com"},
        "tipsHistory": {"a": 1},
        "projects": {
            "/w": {"hasTrustDialogAccepted": false, "allowedTools": ["Bash"]},
            "/h": {"hasTrustDialogAccepted": true}
        },
        "mcpServers": {"docs": {"command": "other"}, "web": {"command": "y"}}
    }));
    let target = obj(json!({
        "oauthAccount": {"emailAddress": "carol@example.com"},
        "theme": "light",
        "projects": {"/w": {"hasTrustDialogAccepted": false}}
    }));
    let c = carry_config_from(Some(target), Some(&floor), None, &[&other]);
    assert_eq!(c.seeded, None);
    assert_eq!(c.map["oauthAccount"]["emailAddress"], "carol@example.com");
    assert_eq!(c.map["theme"], "light", "an existing value stays");
    assert_eq!(c.map["userID"], "u-floor");
    // The floor's accepted trust replaces Claude Code's default.
    assert_eq!(c.map["projects"]["/w"]["hasTrustDialogAccepted"], true);
    // Another profile: only what the file lacks.
    assert_eq!(c.map["projects"]["/w"]["allowedTools"], json!(["Bash"]));
    assert_eq!(c.map["projects"]["/h"]["hasTrustDialogAccepted"], true);
    assert_eq!(c.map["mcpServers"]["docs"]["command"], "x");
    assert_eq!(c.map["mcpServers"]["web"]["command"], "y");
    assert!(
        !c.map.contains_key("tipsHistory"),
        "only trust and MCP from others"
    );
    assert!(c.added.contains(&"userID".to_owned()), "{:?}", c.added);
    assert!(!c.added.iter().any(|k| k.contains("oauthAccount")));
    // Idempotent.
    let again = carry_config_from(Some(c.map.clone()), Some(&floor), None, &[&other]);
    assert!(!again.changed(), "{:?}", again.added);

    // Seeded from the floor; a stray seeds when there is no floor; the
    // identity never comes along.
    let c = carry_config_from(None, Some(&floor), None, &[]);
    assert_eq!(c.seeded, Some(4));
    assert!(!c.map.contains_key("oauthAccount"));
    let stray = obj(json!({"oauthAccount": {"x": 1}, "numStartups": 3}));
    let c = carry_config_from(None, None, Some(&stray), &[&other]);
    assert_eq!(c.seeded, Some(1));
    assert_eq!(c.map["numStartups"], 3);
    assert!(!c.map.contains_key("oauthAccount"));
    assert_eq!(c.map["mcpServers"]["docs"]["command"], "other");
    // Nothing at all: nothing to write.
    let c = carry_config_from(None, None, None, &[]);
    assert!(!c.changed());
}

/// B2 on disk: seeded, merged under both locks, a pre-image kept, and a
/// held Claude Code config lock makes it wait (busy), not fail.
#[test]
fn b2_merges_under_the_config_lock_with_a_pre_image() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let ctx = Context::from_env(
        HostEnv::for_test(home, HostOs::Linux),
        &FakeProcs::default(),
    );
    let (work, other) = (home.join(".claude.work"), home.join(".claude.home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "alice@example.com"}, "theme": "dark"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        other.join(".claude.json"),
        json!({"mcpServers": {"web": {"command": "y"}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        home.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "bob@example.com"}}).to_string(),
    )
    .unwrap();
    let legacy = Legacy {
        profiles: vec![
            LegacyProfile {
                name: "home".into(),
                dir: other,
            },
            LegacyProfile {
                name: "work".into(),
                dir: work,
            },
        ],
        floor: Some("work".into()),
    };
    // Claude Code holds its lock: B2 waits.
    let target = std::fs::canonicalize(home.join(".claude.json")).unwrap();
    let held = crate::orca::fsx::ClaudeConfigLock::acquire(&target, Duration::ZERO).unwrap();
    let e = config_step(
        home,
        &ctx.state,
        &legacy,
        ConfigOpts {
            dry_run: false,
            wait: Duration::from_millis(50),
            switch_wait: Duration::from_millis(50),
            i3: true,
        },
    )
    .unwrap_err();
    assert_eq!(super::adopt::error_class(&e), ("busy", true), "{e}");
    drop(held);
    let line = b2(&ctx, &legacy).unwrap().expect("merged");
    assert!(line.contains("merged"), "{line}");
    let got: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(got["oauthAccount"]["emailAddress"], "bob@example.com");
    assert_eq!(got["theme"], "dark");
    assert_eq!(got["mcpServers"]["web"]["command"], "y");
    let pres: Vec<String> = std::fs::read_dir(ctx.state.join("migrate"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        pres.iter()
            .any(|n| n.starts_with("claude.json.") && n.ends_with(".pre")),
        "{pres:?}"
    );
    assert!(
        !home.join(".claude.json.lock").exists(),
        "the lock is released"
    );
    assert!(
        b2(&ctx, &legacy).unwrap().is_none(),
        "a rerun changes nothing"
    );
    // With nothing to merge no lock is taken: a held switch.lock and a
    // held config lock do not make it wait or fail.
    let marker = std::fs::read(merge_marker(&ctx.state)).unwrap();
    let switch = crate::orca::fsx::SwitchLock::acquire(&ctx.state, Duration::ZERO).unwrap();
    let held = crate::orca::fsx::ClaudeConfigLock::acquire(&target, Duration::ZERO).unwrap();
    let t = std::time::Instant::now();
    let got = config_step(
        home,
        &ctx.state,
        &legacy,
        ConfigOpts {
            dry_run: false,
            wait: Duration::from_secs(5),
            switch_wait: Duration::from_secs(5),
            i3: true,
        },
    )
    .unwrap();
    assert!(got.is_none());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(std::fs::read(merge_marker(&ctx.state)).unwrap(), marker);
    drop((held, switch));
}

/// A linked ~/.claude.json: Claude Code locks the link's own name
/// (`~/.claude.json.lock`, never resolved), so B2 waits for a lock held
/// there, and for one at the target's name too; it then writes through
/// the link and releases both.
#[cfg(unix)]
#[test]
fn b2_takes_the_config_lock_at_a_linked_configs_own_name() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let ctx = Context::from_env(
        HostEnv::for_test(home, HostOs::Linux),
        &FakeProcs::default(),
    );
    let work = home.join(".claude.work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"theme": "dark"}).to_string(),
    )
    .unwrap();
    let dots = home.join("dotfiles");
    std::fs::create_dir_all(&dots).unwrap();
    let real = dots.join("claude.json");
    std::fs::write(&real, json!({"numStartups": 3}).to_string()).unwrap();
    let link = home.join(".claude.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let legacy = Legacy {
        profiles: vec![LegacyProfile {
            name: "work".into(),
            dir: work,
        }],
        floor: Some("work".into()),
    };
    let busy = |at: &Path| {
        let held = crate::orca::fsx::ClaudeConfigLock::acquire(at, Duration::ZERO).unwrap();
        let e = config_step(
            home,
            &ctx.state,
            &legacy,
            ConfigOpts {
                dry_run: false,
                wait: Duration::from_millis(50),
                switch_wait: Duration::from_millis(50),
                i3: true,
            },
        )
        .unwrap_err();
        assert_eq!(super::adopt::error_class(&e), ("busy", true), "{e}");
        drop(held);
    };
    busy(&link);
    busy(&real);
    let line = b2(&ctx, &legacy).unwrap().expect("merged");
    assert!(line.contains("merged"), "{line}");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let got: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
    assert_eq!(got["theme"], "dark");
    assert!(!home.join(".claude.json.lock").exists());
    assert!(!dots.join("claude.json.lock").exists());
}

/// Before a pane's spawn a drain walks a bounded tree: a larger one waits
/// for the run after the spawn, which drains it.
#[cfg(unix)]
#[test]
fn a_pane_leaves_a_large_drain_to_the_run_after_the_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (d, sh) = (home.join(".claude"), shared_root(home));
    let (lp, sp) = (d.join("projects"), sh.join("projects"));
    std::fs::create_dir_all(&lp).unwrap();
    std::fs::create_dir_all(&sp).unwrap();
    std::fs::write(lp.join("mine.jsonl"), b"m").unwrap();
    for i in 0..(PANE_DRAIN_CAP + 2) {
        std::fs::write(sp.join(format!("{i}.jsonl")), b"x").unwrap();
    }
    let aside = home.join("state").join("collided").join("projects");
    let end = b1_one(home, "projects", &cold(), true, &aside).unwrap();
    assert_eq!(end, B1End::Wait(PANE_DRAIN_WAIT.to_owned(), vec![]));
    assert!(
        sp.join("0.jsonl").is_file(),
        "nothing moved before the spawn"
    );
    let end = b1_one(home, "projects", &cold(), false, &aside).unwrap();
    assert!(matches!(end, B1End::Done(_)), "{end:?}");
    assert!(lp.join("0.jsonl").is_file() && lp.join("mine.jsonl").is_file());
}

/// I3: a stray `~/.claude/.claude.json` is merged into `~/.claude.json`
/// and moved to `<state>/migrate/`, unless Orca runs with an explicit
/// `CLAUDE_CONFIG_DIR` naming `~/.claude` (then it is the live config).
#[test]
fn i3_merges_and_moves_a_stray_config() {
    use crate::launch_context::OrcaMain;
    use crate::orca::procenv::OrcaDir;
    let home = Path::new("/Users/example");
    let d = home.join(".claude");
    let dir = |dir: PathBuf, explicit| OrcaMain::Dir(OrcaDir { dir, explicit });
    assert!(i3_applies(&dir(d.clone(), false), None, home));
    assert!(!i3_applies(&dir(d.clone(), true), None, home));
    assert!(!i3_applies(&dir(home.join(".claude/"), true), None, home));
    assert!(i3_applies(
        &dir(home.join(".claude.work"), true),
        None,
        home
    ));
    assert!(!i3_applies(&OrcaMain::Unreadable, None, home));
    assert!(i3_applies(&OrcaMain::Stopped, None, home));
    assert!(i3_applies(
        &OrcaMain::Stopped,
        Some("/Users/example/.claude.work"),
        home
    ));
    assert!(!i3_applies(
        &OrcaMain::Stopped,
        Some("/Users/example/.claude"),
        home
    ));

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let ctx = Context::from_env(
        HostEnv::for_test(home, HostOs::Linux),
        &FakeProcs::default(),
    );
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let stray = stray_config(home);
    std::fs::write(
        &stray,
        json!({"oauthAccount": {"emailAddress": "alice@example.com"},
               "projects": {"/w": {"hasTrustDialogAccepted": true}}})
        .to_string(),
    )
    .unwrap();
    let legacy = Legacy::default();
    let off = ConfigOpts {
        dry_run: false,
        wait: Duration::from_secs(1),
        switch_wait: Duration::from_secs(1),
        i3: false,
    };
    assert!(
        config_step(home, &ctx.state, &legacy, off)
            .unwrap()
            .is_none()
    );
    assert!(stray.is_file(), "left alone while it is the live config");
    let line = b2(&ctx, &legacy).unwrap().expect("moved");
    assert!(line.contains("moved to"), "{line}");
    assert!(!stray.exists());
    let got: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(got["projects"]["/w"]["hasTrustDialogAccepted"], true);
    assert!(got.get("oauthAccount").is_none());
    let kept = std::fs::read_dir(ctx.state.join("migrate"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("claude-dir.claude.json.")
        });
    assert!(kept, "the stray is kept, never deleted");
}

/// B3 copies the floor profile's own files `~/.claude` lacks, rebasing a
/// settings path into the floor dir, and never replaces one it has.
#[cfg(unix)]
#[test]
fn b3_copies_only_what_claude_lacks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (d, work) = (home.join(".claude"), home.join(".claude.work"));
    std::fs::create_dir_all(work.join("skills").join("s1")).unwrap();
    std::fs::create_dir_all(work.join("plans")).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    let hook = format!("{}/hooks/pre.sh", work.display());
    std::fs::write(
        work.join("settings.json"),
        json!({"hooks": {"Stop": [{"command": format!("bash {hook}")}]}, "model": "x"}).to_string(),
    )
    .unwrap();
    std::fs::write(work.join("skills").join("s1").join("SKILL.md"), b"s").unwrap();
    std::fs::write(work.join("CLAUDE.md"), b"floor").unwrap();
    std::fs::write(d.join("CLAUDE.md"), b"mine").unwrap();
    assert!(b3_copies("settings.local.json") && !b3_copies("plans"));

    let dry = copy_missing(&work, home, true).unwrap();
    assert_eq!(dry.len(), 2, "{dry:?}");
    assert!(!d.join("skills").exists());
    let lines = copy_missing(&work, home, false).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(std::fs::read(d.join("CLAUDE.md")).unwrap(), b"mine");
    assert!(d.join("skills").join("s1").join("SKILL.md").is_file());
    assert!(!d.join("plans").exists());
    let s: Value =
        serde_json::from_slice(&std::fs::read(d.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        s["hooks"]["Stop"][0]["command"],
        format!("bash {}/hooks/pre.sh", d.display())
    );
    assert!(work.join("settings.json").is_file(), "the source stays");
    assert!(std::fs::read_dir(&d).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("csm-copy")
    }));
    assert!(copy_missing(&work, home, false).unwrap().is_empty());
}

/// The whole stage over a temp home: it settles, records its steps, and a
/// rerun changes nothing.
#[cfg(unix)]
#[test]
fn carry_settles_and_reruns_quietly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::Linux);
    let state = crate::orca::fsx::state_dir(&env);
    let (d, sh, work) = (
        home.join(".claude"),
        shared_root(home),
        home.join(".claude.work"),
    );
    std::fs::create_dir_all(sh.join("projects")).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(sh.join("projects"), d.join("projects")).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"hasCompletedOnboarding": true}).to_string(),
    )
    .unwrap();
    std::fs::write(work.join("keybindings.json"), b"[]").unwrap();
    let legacy = Legacy {
        profiles: vec![LegacyProfile {
            name: "work".into(),
            dir: work,
        }],
        floor: Some("work".into()),
    };
    let opts = CarryOpts {
        dry_run: false,
        pane: false,
        i3: true,
        now: SystemTime::now() + Duration::from_secs(3600),
        child_live: false,
        child_sid: None,
    };
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let end = carry(
        &env,
        &state,
        &legacy,
        &FakeProcs::default(),
        opts,
        &mut st,
        &mut report,
    );
    assert!(end.settled, "{report:?}");
    assert!(
        report.errors.is_empty() && report.pending.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.changed.len(), 3, "{:?}", report.changed);
    for k in ["B1:projects", "B2", "B3", "B4"] {
        assert_eq!(
            st.steps.get(k).map(|s| s.status),
            Some(super::state::StepStatus::Done),
            "{k}"
        );
    }
    assert!(d.join("keybindings.json").is_file());
    let mut report = super::Report::default();
    let end = carry(
        &env,
        &state,
        &legacy,
        &FakeProcs::default(),
        opts,
        &mut st,
        &mut report,
    );
    assert!(end.settled);
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    // The pane run never settles the stage (B3-B5 wait for the full run).
    let mut report = super::Report::default();
    let pane = CarryOpts { pane: true, ..opts };
    assert!(
        !carry(
            &env,
            &state,
            &legacy,
            &FakeProcs::default(),
            pane,
            &mut st,
            &mut report
        )
        .settled
    );
}

/// A pane launch with Orca stopped (so not on `~/.claude`) does nothing
/// before its spawn: the probe, and neither the carry stage nor a marker.
#[test]
fn pane_prespawn_waits_unless_orca_runs_in_claude() {
    use crate::usage::reach;
    let home = legacy_home();
    crate::testenv::with_test_home(home.path(), || {
        let _ = reach::take();
        super::pane_prespawn();
        let seen = reach::take();
        assert!(seen.contains(&"migrate-probe"), "{seen:?}");
        assert!(!seen.contains(&"migrate-carry"), "{seen:?}");
        let state = crate::paths::smart_dir_no_create();
        assert!(!state.join(super::state::MARKER).exists());
    });
}

/// `csm orca status` probes once: the dispatch's NOTE probe feeds the
/// status row, and a second row (no dispatch before it) probes afresh.
#[test]
fn orca_status_probes_once() {
    use crate::usage::reach;
    let home = legacy_home();
    crate::testenv::with_test_home(home.path(), || {
        let _ = reach::take();
        super::at_dispatch("orca", &os(&["status"]));
        let row = super::status_line();
        let seen = reach::take();
        let probes = seen.iter().filter(|s| **s == "migrate-probe").count();
        assert_eq!(probes, 1, "{seen:?}");
        assert!(row.starts_with("pending"), "{row}");
        assert_eq!(super::status_line(), row);
        assert!(reach::take().contains(&"migrate-probe"));
        // Another NOTE word leaves nothing for the row.
        super::at_dispatch("usage", &[]);
        let _ = reach::take();
        let _ = super::status_line();
        assert!(reach::take().contains(&"migrate-probe"));
    });
}

/// The pane's own pre-spawn work (B1 and B2 through `run_carry` with
/// `pane`) over a pending legacy home moves the shared dir and merges the
/// floor config, and reaches no Keychain, RPC, token, profile or usage
/// call (Invariant 6: an Orca-pane launch touches neither before claude
/// starts).
#[cfg(unix)]
#[test]
fn pane_carry_reaches_no_keychain_rpc_or_network() {
    use crate::usage::reach;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::MacOs);
    let state = crate::orca::fsx::state_dir(&env);
    let (d, sh, work) = (
        home.join(".claude"),
        shared_root(home),
        home.join(".claude.work"),
    );
    std::fs::create_dir_all(sh.join("projects").join("-w")).unwrap();
    std::fs::write(sh.join("projects").join("-w").join("s.jsonl"), b"{}\n").unwrap();
    std::fs::create_dir_all(&d).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::os::unix::fs::symlink(sh.join("projects"), d.join("projects")).unwrap();
    std::fs::write(
        work.join(".claude.json"),
        json!({"hasCompletedOnboarding": true,
            "oauthAccount": {"emailAddress": "alice@example.com"}})
        .to_string(),
    )
    .unwrap();
    let legacy = Legacy {
        profiles: vec![LegacyProfile {
            name: "work".into(),
            dir: work,
        }],
        floor: Some("work".into()),
    };
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let _ = reach::take();
    super::run_carry(
        &env,
        &state,
        &legacy,
        false,
        true,
        &super::LaunchChild::None,
        &mut st,
        &mut report,
    );
    let seen = reach::take();
    assert!(seen.contains(&"migrate-carry"), "{seen:?}");
    for step in [
        "keychain",
        "orca-rpc",
        "oauth-token",
        "oauth-profile",
        "usage-api",
        "usage-cmd",
    ] {
        assert!(!seen.contains(&step), "{step} reached: {seen:?}");
    }
    assert!(report.errors.is_empty(), "{report:?}");
    // The positive half: B1 moved the transcripts, B2 seeded the config
    // without the login.
    let projects = d.join("projects");
    assert!(
        !std::fs::symlink_metadata(&projects)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(projects.join("-w").join("s.jsonl").is_file());
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(cfg["hasCompletedOnboarding"], json!(true));
    assert!(cfg.get("oauthAccount").is_none(), "{cfg}");
}

// ─── stage 3: the cutover, the floor, stage C ────────────────────────────────

fn cut_facts<'a>(
    home: &'a Path,
    dirs: &'a [PathBuf],
    orca: &'a OrcaAt,
    store: &'a StoreKind,
) -> CutoverFacts<'a> {
    CutoverFacts {
        carried: true,
        floor: Ok(None),
        legacy_dirs: dirs,
        home,
        orca,
        listed: true,
        active: Some("acct-a"),
        store,
        neutralise_needed: false,
        live_implicit: false,
    }
}

fn is_wait(p: &CutoverPlan, needle: &str) -> bool {
    matches!(p, CutoverPlan::Wait(w) if w.contains(needle))
}

#[test]
fn cutover_gate_checks_each_precondition() {
    let home = Path::new("/Users/example");
    let dirs = vec![home.join(".claude.work"), home.join(".claude.home")];
    let stopped = OrcaAt::Stopped;
    let json = StoreKind::Json(Ok(()));
    let base = cut_facts(home, &dirs, &stopped, &json);
    // All preconditions hold, Orca stopped with a JSON store: a=i switch.
    assert_eq!(
        cutover_gate(&base),
        CutoverPlan::Go {
            neutralise: false,
            materialise: Materialise::Switch("acct-a".into()),
            clear_floor: false,
        }
    );
    // Not carried yet.
    let f = CutoverFacts {
        carried: false,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "not carried"));
    // The floor cannot be read.
    let f = CutoverFacts {
        floor: Err("denied"),
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "cannot read"));
    // A floor naming no recorded dir is left alone.
    let f = CutoverFacts {
        floor: Ok(Some("/Users/example/elsewhere")),
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "no recorded profile dir"));
    // A floor naming a legacy dir, or ~/.claude, is cleared.
    for v in ["/Users/example/.claude.work", "/Users/example/.claude"] {
        let f = CutoverFacts {
            floor: Ok(Some(v)),
            ..base
        };
        assert!(
            matches!(
                cutover_gate(&f),
                CutoverPlan::Go {
                    clear_floor: true,
                    ..
                }
            ),
            "{v}"
        );
    }
    // Orca runs but its D cannot be read.
    let un = OrcaAt::Unreadable;
    let f = CutoverFacts { orca: &un, ..base };
    assert!(is_wait(&cutover_gate(&f), "cannot be read"));
    // Orca runs: it must have listed and must name an active account;
    // then nothing to materialise (Orca does it at its start).
    let running = OrcaAt::Dir(home.join(".claude.work"));
    let f = CutoverFacts {
        orca: &running,
        listed: false,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "did not list"));
    let f = CutoverFacts {
        orca: &running,
        active: None,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "no active account"));
    let f = CutoverFacts {
        orca: &running,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            materialise: Materialise::Nothing,
            ..
        }
    ));
    // Orca stopped: no store waits, a SQLite export materialises, a store
    // without an active account waits, a refused offline write waits, a
    // store-less host does the files half only.
    let missing = StoreKind::Missing;
    let f = CutoverFacts {
        store: &missing,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "no store yet"));
    let sqlite = StoreKind::Sqlite(Ok(()));
    let f = CutoverFacts {
        store: &sqlite,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            materialise: Materialise::Export(ref id),
            ..
        } if id == "acct-a"
    ));
    let f = CutoverFacts {
        store: &sqlite,
        active: None,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "SQLite"));
    let f = CutoverFacts {
        active: None,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "no active account"));
    let refused = StoreKind::Json(Err("csm may not write here".into()));
    let f = CutoverFacts {
        store: &refused,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "csm may not write here"));
    let sl = StoreKind::StoreLess;
    let f = CutoverFacts {
        store: &sl,
        active: None,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            materialise: Materialise::StoreLess,
            ..
        }
    ));
    // Orca stopped: step 2 reads ~/.claude back by its identity before it
    // writes, so step 1 must not strip that identity first; a live
    // implicit claude does not hold the switch back.
    let f = CutoverFacts {
        neutralise_needed: true,
        live_implicit: true,
        ..base
    };
    assert_eq!(
        cutover_gate(&f),
        CutoverPlan::Go {
            neutralise: false,
            materialise: Materialise::Switch("acct-a".into()),
            clear_floor: false,
        }
    );
    let f = CutoverFacts {
        store: &sqlite,
        neutralise_needed: true,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            neutralise: false,
            ..
        }
    ));
    // Orca runs elsewhere: ~/.claude still holds another identity, so it
    // is neutralised, but not under a live implicit claude, and not when
    // Orca runs in ~/.claude itself.
    let f = CutoverFacts {
        orca: &running,
        neutralise_needed: true,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            neutralise: true,
            materialise: Materialise::Nothing,
            ..
        }
    ));
    let f = CutoverFacts {
        orca: &running,
        neutralise_needed: true,
        live_implicit: true,
        ..base
    };
    assert!(is_wait(&cutover_gate(&f), "without CLAUDE_CONFIG_DIR"));
    let in_home = OrcaAt::Dir(home.join(".claude"));
    let f = CutoverFacts {
        orca: &in_home,
        neutralise_needed: true,
        live_implicit: true,
        ..base
    };
    assert!(matches!(
        cutover_gate(&f),
        CutoverPlan::Go {
            neutralise: false,
            ..
        }
    ));
}

/// Before the Orca-stopped step 2 copies the active stash into
/// `~/.claude`: a legacy dir holding the active account's login must be
/// idle (the launch's own child counts) and no fresher than the stash; a
/// fresher one is read back first. Other accounts' dirs do not matter.
#[test]
fn holder_gate_waits_for_a_live_or_fresher_active_login() {
    let st = |id: &str, fresher| Status::InOrca {
        id: id.into(),
        fresher,
    };
    assert_eq!(
        holder_gate("w", &st("acct-a", Some(false)), "acct-a", None, false),
        HolderGate::Clear
    );
    assert_eq!(
        holder_gate("w", &st("acct-a", Some(true)), "acct-a", None, false),
        HolderGate::ReadBack
    );
    assert!(matches!(
        holder_gate("w", &st("acct-a", None), "acct-a", None, false),
        HolderGate::Wait(w) if w.contains("compared")
    ));
    assert!(matches!(
        holder_gate("w", &st("acct-a", Some(false)), "acct-a", None, true),
        HolderGate::Wait(w) if w.contains("could not be read")
    ));
    assert!(matches!(
        holder_gate(
            "w",
            &st("acct-a", Some(false)),
            "acct-a",
            Some("this launch's claude"),
            false
        ),
        HolderGate::Wait(w) if w.contains("this launch's claude")
    ));
    // Another account's login, or none: nothing to settle first.
    assert_eq!(
        holder_gate("w", &st("acct-b", Some(true)), "acct-a", Some("x"), false),
        HolderGate::Clear
    );
    assert_eq!(
        holder_gate("w", &Status::ToImport, "acct-a", Some("x"), false),
        HolderGate::Clear
    );
    assert_eq!(
        holder_gate("w", &Status::NoCredentials, "acct-a", None, true),
        HolderGate::Clear
    );
}

/// The launch's own claude, noted before it registers, counts as a user
/// of its dir (any dir when its `D` is unknown); no child counts nowhere.
#[test]
fn launch_child_uses_only_its_own_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join(".claude.work");
    let other = tmp.path().join(".claude");
    let c = super::LaunchChild::In {
        dir: d.clone(),
        sid: Some("s-1".into()),
    };
    assert!(c.runs() && c.uses(&d) && !c.uses(&other));
    let mut trailing = d.clone().into_os_string();
    trailing.push("/");
    assert!(c.uses(Path::new(&trailing)));
    assert_eq!(c.sid(), Some("s-1"));
    assert!(super::LaunchChild::Unknown.uses(&other));
    assert!(super::LaunchChild::Unknown.runs());
    assert!(!super::LaunchChild::None.uses(&d));
    assert!(!super::LaunchChild::None.runs());
}

/// B1 counts the launch's child as live even before it registers: a move
/// across filesystems then waits, and its session's entries stay.
#[test]
fn the_launch_child_is_hot_before_it_registers() {
    let mut hot = Hot::at(std::time::SystemTime::now());
    child_is_hot(&mut hot, false, Some("s-1"));
    assert!(!hot.live && hot.sids.is_empty());
    child_is_hot(&mut hot, true, Some("s-1"));
    assert!(hot.live);
    assert_eq!(hot.sids, vec!["s-1".to_owned()]);
    child_is_hot(&mut hot, true, Some("s-1"));
    assert_eq!(hot.sids.len(), 1);
    let mut hot = Hot::at(std::time::SystemTime::now());
    child_is_hot(&mut hot, true, None);
    assert!(hot.live && hot.sids.is_empty());
}

/// A pre-spawn worker that outlived its budget holds the launch's own
/// switch.lock users back until it is done.
#[test]
fn a_prespawn_worker_counts_as_running_until_it_ends() {
    let (tx, rx) = std::sync::mpsc::channel::<super::Report>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let h = std::thread::spawn(move || {
        let _ = go_rx.recv();
        drop(tx);
    });
    let armed = super::Armed {
        worker: Some((h, rx)),
        after_prespawn: true,
        child: super::LaunchChild::Unknown,
    };
    assert!(!super::worker_running(None));
    assert!(super::worker_running(Some(&armed)));
    go_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while super::worker_running(Some(&armed)) {
        assert!(std::time::Instant::now() < deadline, "worker never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    let done = super::Armed {
        worker: None,
        after_prespawn: true,
        child: super::LaunchChild::None,
    };
    assert!(!super::worker_running(Some(&done)));
    if let Some((h, _)) = armed.worker {
        h.join().unwrap();
    }
}

#[test]
fn offline_d_gate_refuses_windows_untested_and_foreign() {
    assert!(offline_d_gate(HostOs::MacOs, true, true).is_ok());
    assert!(offline_d_gate(HostOs::Linux, true, true).is_ok());
    assert!(offline_d_gate(HostOs::Windows, true, true).is_err());
    assert!(offline_d_gate(HostOs::MacOs, false, true).is_err());
    assert!(offline_d_gate(HostOs::MacOs, true, false).is_err());
}

#[test]
fn orca_at_counts_a_running_orca_without_a_runtime_as_unreadable() {
    use crate::launch_context::OrcaMain;
    assert_eq!(OrcaAt::of(&OrcaMain::Stopped, false), OrcaAt::Stopped);
    assert_eq!(OrcaAt::of(&OrcaMain::Stopped, true), OrcaAt::Unreadable);
    assert_eq!(OrcaAt::of(&OrcaMain::Unreadable, false), OrcaAt::Unreadable);
}

#[test]
fn floor_gate_waits_for_a_later_boot_without_the_floor() {
    use super::state::Cutover;
    let c = Cutover {
        at: 1_000,
        boot_id: Some("boot-1".into()),
    };
    assert_eq!(
        floor_gate(None, Some("boot-2"), Ok(None)),
        FloorGate::NoCutover
    );
    assert_eq!(
        floor_gate(Some(&c), Some("boot-1"), Ok(None)),
        FloorGate::SameBoot
    );
    assert_eq!(
        floor_gate(Some(&c), Some("boot-2"), Ok(None)),
        FloorGate::Pass
    );
    assert_eq!(floor_gate(Some(&c), None, Ok(None)), FloorGate::BootUnknown);
    assert_eq!(
        floor_gate(
            Some(&Cutover {
                at: 1_000,
                boot_id: None
            }),
            Some("boot-2"),
            Ok(None)
        ),
        FloorGate::BootUnknown
    );
    assert_eq!(
        floor_gate(
            Some(&c),
            Some("boot-2"),
            Ok(Some("/Users/example/.claude.work"))
        ),
        FloorGate::FloorSet("/Users/example/.claude.work".into())
    );
    // A blank value is no floor.
    assert_eq!(
        floor_gate(Some(&c), Some("boot-2"), Ok(Some(""))),
        FloorGate::Pass
    );
    assert_eq!(
        floor_gate(Some(&c), Some("boot-2"), Err(())),
        FloorGate::FloorUnreadable
    );
    // Windows boot times: within the slack is the same boot.
    let w = Cutover {
        at: 1_000,
        boot_id: Some("boot-time:5000".into()),
    };
    assert_eq!(
        floor_gate(Some(&w), Some("boot-time:5030"), Ok(None)),
        FloorGate::SameBoot
    );
    assert_eq!(
        floor_gate(Some(&w), Some("boot-time:90000"), Ok(None)),
        FloorGate::Pass
    );
    // Every refusal has a line; a pass has none.
    assert_eq!(floor_gate_line(&FloorGate::Pass), None);
    for g in [
        FloorGate::NoCutover,
        FloorGate::SameBoot,
        FloorGate::BootUnknown,
        FloorGate::FloorSet("x".into()),
        FloorGate::FloorUnreadable,
    ] {
        assert!(floor_gate_line(&g).is_some(), "{g:?}");
    }
}

fn gate_facts<'a>(users: &'a crate::orca::live::DirUsers, floor: &'a FloorGate) -> RetireFacts<'a> {
    RetireFacts {
        users,
        is_floor: false,
        orca_d_is_dir: Some(false),
        floor,
        store_less: false,
        grant: DirGrant::Superseded,
        shims: &[],
        supervisor: None,
        home_empty: false,
    }
}

/// A legacy csm supervisor holds every dir, floor or not: between hops
/// it has no live claude and its own environment may name another dir.
#[test]
fn a_legacy_csm_supervisor_holds_every_dir() {
    use crate::orca::live::DirUsers;
    let free = DirUsers::Free;
    let pass = FloorGate::Pass;
    let why = "csm pid 210, started before the migration, still runs";
    for is_floor in [false, true] {
        let f = RetireFacts {
            is_floor,
            supervisor: Some(why),
            ..gate_facts(&free, &pass)
        };
        assert_eq!(
            retire_gate(&f),
            RetireGate::Wait(why.to_owned()),
            "{is_floor}"
        );
    }
}

/// `supervision` over a fake process table: a csm started before the
/// migration was recorded holds, a later one (the new binary) does not,
/// and a marker without `seen_at` counts every other csm.
#[test]
fn supervision_counts_only_a_csm_older_than_the_migration() {
    use crate::orca::testsupport::{FakeProcs, proc_info};
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let mut old = proc_info(210, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
    old.start_time = 500;
    let mut new = proc_info(220, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
    new.start_time = 2000;
    let procs = FakeProcs::default().with(new.clone());
    assert_eq!(supervision(home, &procs, Some(1000)), None);
    assert!(supervision(home, &procs, None).is_some_and(|w| w.contains("pid 220")));
    let procs = FakeProcs::default().with(new).with(old);
    assert!(supervision(home, &procs, Some(1000)).is_some_and(|w| w.contains("pid 210")));
}

#[test]
fn retire_gate_refuses_live_unknown_and_unreadable_but_not_newer() {
    use crate::orca::live::DirUsers;
    let free = DirUsers::Free;
    let pass = FloorGate::Pass;
    let base = gate_facts(&free, &pass);
    assert_eq!(retire_gate(&base), RetireGate::Go);
    // A live user.
    let live = DirUsers::Live("pid 7 (claude) runs with CLAUDE_CONFIG_DIR=x".into());
    let f = RetireFacts {
        users: &live,
        ..base
    };
    assert!(matches!(retire_gate(&f), RetireGate::Wait(w) if w.contains("in use")));
    // Users unknown count as live.
    let unknown = DirUsers::Unknown("the process table cannot be read".into());
    let f = RetireFacts {
        users: &unknown,
        ..base
    };
    assert!(matches!(retire_gate(&f), RetireGate::Wait(w) if w.contains("counted as in use")));
    // Orca's live D, or an unknown one.
    for d in [Some(true), None] {
        let f = RetireFacts {
            orca_d_is_dir: d,
            ..base
        };
        assert!(matches!(retire_gate(&f), RetireGate::Wait(_)), "{d:?}");
    }
    // An unreadable grant.
    let f = RetireFacts {
        grant: DirGrant::Unreadable,
        ..base
    };
    assert!(matches!(retire_gate(&f), RetireGate::Wait(w) if w.contains("could not be read")));
    // A grant newer than the stash passes: it is quarantined as Retired.
    let f = RetireFacts {
        grant: DirGrant::Newer,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
    // The floor dir waits for I2's floor half; other dirs ignore it.
    let same = FloorGate::SameBoot;
    let f = RetireFacts {
        floor: &same,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
    let f = RetireFacts {
        floor: &same,
        is_floor: true,
        ..base
    };
    assert!(matches!(retire_gate(&f), RetireGate::Wait(_)));
    let f = RetireFacts {
        is_floor: true,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
    // A store-less host keeps every dir but the floor dir.
    let f = RetireFacts {
        store_less: true,
        ..base
    };
    assert!(matches!(retire_gate(&f), RetireGate::Stay(_)));
    let f = RetireFacts {
        store_less: true,
        is_floor: true,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
}

/// A shell startup file that still runs the old `cas` guard holds the
/// floor dir (its fallback would export the renamed dir in every new
/// shell), not the other dirs; comments and other commands do not count.
#[test]
fn an_old_shell_guard_holds_the_floor_dir() {
    use crate::orca::live::DirUsers;
    let free = DirUsers::Free;
    let pass = FloorGate::Pass;
    let shims = vec![PathBuf::from("/Users/example/.zshenv")];
    let base = RetireFacts {
        shims: &shims,
        ..gate_facts(&free, &pass)
    };
    assert_eq!(retire_gate(&base), RetireGate::Go);
    let f = RetireFacts {
        is_floor: true,
        ..base
    };
    assert!(
        matches!(retire_gate(&f), RetireGate::Wait(w) if w.contains(".zshenv") && w.contains("--print-default-dir"))
    );
    assert!(shim_line(&[]).is_none());

    let guard = "_d=$(csm cas --print-default-dir 2>/dev/null)\n[ -n \"$_d\" ] || _d=~/.claude.x\n";
    assert!(runs_print_default_dir(guard));
    assert!(runs_print_default_dir(
        "  $d = & csm.exe cas --print-default-dir\n"
    ));
    assert!(!runs_print_default_dir("# csm cas --print-default-dir\n"));
    assert!(!runs_print_default_dir("csm accounts list\n"));
    assert!(!runs_print_default_dir("broadcast --print-default-dir\n"));

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    assert!(cas_shims(home).is_empty());
    std::fs::write(home.join(".zshenv"), guard).unwrap();
    std::fs::write(home.join(".bashrc"), "export PATH=/usr/bin\n").unwrap();
    assert_eq!(cas_shims(home), vec![home.join(".zshenv")]);
}

#[test]
fn store_less_needs_a_non_macos_host_without_its_own_store() {
    assert!(store_less(HostOs::Linux, false, false, true));
    assert!(store_less(HostOs::Linux, false, true, false));
    assert!(store_less(HostOs::Windows, false, false, true));
    assert!(!store_less(HostOs::Linux, true, false, true));
    assert!(!store_less(HostOs::Linux, false, true, true));
    assert!(!store_less(HostOs::MacOs, false, false, true));
}

fn cred(expires: u64, rt: &str) -> String {
    json!({"claudeAiOauth": {"accessToken": "at", "refreshToken": rt, "expiresAt": expires}})
        .to_string()
}

#[test]
fn settle_wants_only_a_fresher_grant_and_a_stopped_tested_orca() {
    let old = cred(1_000, "rt-1");
    let new = cred(2_000, "rt-2");
    assert!(settle_wanted(&new, None));
    assert!(settle_wanted(&new, Some(&old)));
    assert!(!settle_wanted(&old, Some(&new)));
    assert!(!settle_wanted(&new, Some(&new)));
    assert!(settle_gate(false, HostOs::MacOs, true, true).is_ok());
    assert!(settle_gate(false, HostOs::Linux, true, true).is_ok());
    assert!(settle_gate(true, HostOs::MacOs, true, true).is_err());
    assert!(settle_gate(false, HostOs::Windows, true, true).is_err());
    assert!(settle_gate(false, HostOs::MacOs, false, true).is_err());
    assert!(settle_gate(false, HostOs::MacOs, true, false).is_err());
}

#[test]
fn drain_own_moves_history_and_plans_keeping_collisions() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dir = home.join(".claude.work");
    std::fs::create_dir_all(dir.join("file-history").join("s1")).unwrap();
    std::fs::write(dir.join("file-history").join("s1").join("a"), b"a").unwrap();
    std::fs::create_dir_all(dir.join("plans")).unwrap();
    std::fs::write(dir.join("plans").join("p.md"), b"mine").unwrap();
    std::fs::create_dir_all(home.join(".claude").join("plans")).unwrap();
    std::fs::write(home.join(".claude").join("plans").join("p.md"), b"theirs").unwrap();
    let lines = drain_own(&dir, home).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        home.join(".claude/file-history/s1/a").is_file(),
        "file-history moved"
    );
    assert_eq!(
        std::fs::read(home.join(".claude/plans/p.md")).unwrap(),
        b"theirs"
    );
    assert!(dir.join("plans/p.md").is_file(), "the collision stays");
    // Idempotent: a rerun moves nothing new and fails on nothing.
    drain_own(&dir, home).unwrap();
}

/// A profile dir that kept its own projects, todos and history (no link
/// into `~/.claude.shared`): retire moves the transcripts into `~/.claude`,
/// where `claude --resume` and the picker find them, and puts the history
/// lines in front of `~/.claude`'s, never overwriting what is there.
#[test]
fn drain_own_moves_a_dirs_own_transcripts_and_history() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dir = home.join(".claude.work");
    let d = home.join(".claude");
    let proj = |root: &Path| root.join("projects").join("-tmp-cwd");
    std::fs::create_dir_all(proj(&dir)).unwrap();
    std::fs::write(proj(&dir).join("s-own.jsonl"), b"own\n").unwrap();
    std::fs::write(proj(&dir).join("s-both.jsonl"), b"old copy\n").unwrap();
    std::fs::create_dir_all(dir.join("todos")).unwrap();
    std::fs::write(dir.join("todos").join("t.json"), b"[]").unwrap();
    std::fs::write(dir.join("history.jsonl"), b"{\"old\":1}\n").unwrap();
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(dir.join("sessions").join("1.json"), b"{}").unwrap();
    std::fs::create_dir_all(proj(&d)).unwrap();
    std::fs::write(proj(&d).join("s-both.jsonl"), b"new copy\n").unwrap();
    std::fs::write(d.join("history.jsonl"), b"{\"new\":2}\n").unwrap();

    let lines = drain_own(&dir, home).unwrap();
    assert!(lines.iter().any(|l| l.contains("projects")), "{lines:?}");
    assert_eq!(
        std::fs::read(proj(&d).join("s-own.jsonl")).unwrap(),
        b"own\n"
    );
    assert_eq!(
        std::fs::read(proj(&d).join("s-both.jsonl")).unwrap(),
        b"new copy\n",
        "~/.claude's copy stays"
    );
    assert!(
        proj(&dir).join("s-both.jsonl").is_file(),
        "the collision stays"
    );
    assert!(d.join("todos").join("t.json").is_file());
    assert_eq!(
        std::fs::read_to_string(d.join("history.jsonl")).unwrap(),
        "{\"old\":1}\n{\"new\":2}\n"
    );
    assert!(!dir.join("history.jsonl").exists());
    assert!(!d.join("sessions").exists(), "runtime state stays behind");
    // Idempotent: a rerun moves nothing new and fails on nothing.
    drain_own(&dir, home).unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("history.jsonl")).unwrap(),
        "{\"old\":1}\n{\"new\":2}\n"
    );

    // No history in ~/.claude yet: the dir's file moves there whole.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dir = home.join(".claude.home");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("history.jsonl"), b"{\"only\":1}\n").unwrap();
    drain_own(&dir, home).unwrap();
    assert_eq!(
        std::fs::read(home.join(".claude").join("history.jsonl")).unwrap(),
        b"{\"only\":1}\n"
    );
}

#[cfg(unix)]
#[test]
fn links_into_shared_finds_only_links_that_point_into_it() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let d = home.join(".claude");
    let sh = shared_root(home);
    let other = home.join(".claude.other");
    for p in [&d, &sh, &other] {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::create_dir_all(d.join("projects")).unwrap();
    // The compat link inside the shared dir points out of it: not counted.
    symlink(d.join("projects"), sh.join("projects")).unwrap();
    assert!(links_into_shared(home, std::slice::from_ref(&other)).is_empty());
    // An unregistered dir that still links into it holds it back.
    std::fs::create_dir_all(sh.join("todos")).unwrap();
    symlink("../.claude.shared/todos", other.join("todos")).unwrap();
    assert_eq!(
        links_into_shared(home, std::slice::from_ref(&other)),
        vec![other.join("todos")]
    );
}

// ─── the report ───────────────────────────────────────────────────────────────

/// `csm migrate`'s stdout: one heading per legacy dir with its stage rows
/// under it, then the changed, pending and error lists and the
/// unregistered dirs; a busy or legacy-free run is one line.
#[test]
fn the_report_groups_rows_per_dir_and_lists_the_rest() {
    use super::state::Phase;
    use super::{Report, ReportRow, render};
    let busy = Report {
        busy: true,
        legacy: true,
        ..Report::default()
    };
    assert_eq!(
        render(&busy),
        "another csm is migrating this machine; nothing done\n"
    );
    assert!(render(&Report::default()).starts_with("nothing to migrate"));

    let home = Path::new("/Users/example");
    let row = |name: &str, stage: &'static str, line: &str| ReportRow {
        name: name.into(),
        dir: home.join(format!(".claude.{name}")),
        stage,
        line: line.into(),
    };
    let r = Report {
        dry_run: true,
        legacy: true,
        phase: Some(Phase::Carry),
        rows: vec![
            row("home", "adopt", "in Orca (acct-home)"),
            row("home", "retire", "waits: a claude runs in it"),
            row("work", "adopt", "in Orca (acct-work)"),
        ],
        changed: vec!["work: imported carol@example.com".into()],
        pending: vec!["home: a claude runs in it".into()],
        errors: vec!["cannot read profiles.json".into()],
        unregistered: vec![home.join(".claude.scratch")],
        ..Report::default()
    };
    let out = render(&r);
    assert!(
        out.starts_with("phase: carry (dry run: nothing written)\n"),
        "{out}"
    );
    // One heading per dir, its rows under it.
    assert_eq!(
        out.matches("home  /Users/example/.claude.home\n").count(),
        1
    );
    let h = out.find("home  ").unwrap();
    let w = out.find("work  ").unwrap();
    assert!(h < out.find("waits: a claude runs in it").unwrap());
    assert!(out.find("waits: a claude runs in it").unwrap() < w);
    for (title, item) in [
        ("changed:", "  work: imported carol@example.com"),
        ("pending:", "  home: a claude runs in it"),
        ("errors:", "  cannot read profiles.json"),
        (
            "not registered, left alone:",
            "  /Users/example/.claude.scratch",
        ),
    ] {
        let t = out.find(&format!("{title}\n")).expect(title);
        assert!(out[t..].contains(item), "{title}: {out}");
    }
    // Empty lists print no heading.
    let quiet = Report {
        legacy: true,
        phase: Some(Phase::Retire),
        ..Report::default()
    };
    assert_eq!(render(&quiet), "phase: retire\n");
}

// ─── round 2: retire/settle/cutover regressions ──────────────────────────────

/// Only a filing settle would act on holds the phase open: a dir copy
/// equal to the stash, or an older one, does not; a fresher one (here an
/// expired access token, which settle refreshes) does. With every dir
/// retired, a run that filed only such copies may end the phase.
#[test]
fn retire_dir_says_when_settle_is_due() {
    // Equal to the stash.
    let tmp = tempfile::tempdir().unwrap();
    let grant = creds_json("at-a", "rt-a", 9);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &grant);
    let r = retire_dir(
        &ctx,
        &view,
        &FakeHttp::default(),
        &dir,
        "acct-a",
        &|| Ok(()),
    )
    .unwrap();
    assert!(!r.settle_due, "{}", r.line);
    assert!(retire_done(0, r.settle_due));

    // Older than the stash, same account.
    let tmp = tempfile::tempdir().unwrap();
    let older = creds_json("at-a0", "rt-a0", 1);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &older);
    let http = FakeHttp::default().profile_uuid("at-a0", "u-a");
    let r = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).unwrap();
    assert!(!r.settle_due);

    // Fresher, its access token refused: settle's to refresh.
    let tmp = tempfile::tempdir().unwrap();
    let newer = creds_json("at-d", "rt-d", 50);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &newer);
    let http = FakeHttp::default().profile("at-d", FakeHttp::reply(401, "{}"));
    let r = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).unwrap();
    assert!(r.settle_due);
    assert!(!retire_done(0, r.settle_due));

    // Another account's grant is filed unattributed: settle skips it.
    let tmp = tempfile::tempdir().unwrap();
    let foreign = creds_json("at-b", "rt-b", 50);
    let (ctx, view, dir, _) = retire_world(tmp.path(), &foreign);
    let http = FakeHttp::default().profile_uuid("at-b", "u-b");
    let r = retire_dir(&ctx, &view, &http, &dir, "acct-a", &|| Ok(())).unwrap();
    assert!(!r.settle_due);
}

/// A claude that starts in the dir while retire attributes its grants:
/// a user found by the second check stops the retire before anything is
/// filed; a grant rotated between the read and the delete is never
/// deleted (the copy filed before stays in the quarantine, the dir keeps
/// the rotated one and is not renamed).
#[test]
fn retire_dir_never_deletes_a_grant_that_changed_or_a_dir_in_use() {
    let grant = creds_json("at-a", "rt-a", 9);

    // In use by the time the attribution is done.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, dir, _) = retire_world(tmp.path(), &grant);
    let err = retire_dir(&ctx, &view, &FakeHttp::default(), &dir, "acct-a", &|| {
        Err("pid 7 (claude) runs with CLAUDE_CONFIG_DIR=x".to_owned())
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("in use now"), "{err:#}");
    assert!(Quarantine::new(HostOs::Linux, &ctx.state).list().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
        grant
    );

    // Rotated between the read and the delete.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, dir, _) = retire_world(tmp.path(), &grant);
    let rotated = creds_json("at-r", "rt-r", 60);
    let file = dir.join(".credentials.json");
    let err = retire_dir(&ctx, &view, &FakeHttp::default(), &dir, "acct-a", &|| {
        std::fs::write(&file, &rotated).unwrap();
        Ok(())
    })
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("changed while it was retired"), "{msg}");
    for secret in ["at-a", "rt-a", "at-r", "rt-r"] {
        assert!(!msg.contains(secret), "leaked {secret}: {msg}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), rotated);
    assert!(dir.is_dir());
    assert!(!tmp.path().join(".claude.work.retired").exists());
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let list = q.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].fingerprint, quarantine::fingerprint(&grant));

    // The store-less retire keeps the same rule.
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _, dir, _) = retire_world(tmp.path(), &grant);
    let err = retire_dir_store_less(&ctx, &dir, &|| Err("busy".to_owned())).unwrap_err();
    assert!(format!("{err:#}").contains("in use now"), "{err:#}");
    assert!(dir.join(".credentials.json").is_file());
}

#[test]
fn delete_verdict_deletes_only_the_bytes_filed() {
    assert_eq!(delete_verdict("a", Some("a")), Some(true));
    assert_eq!(delete_verdict("a", None), Some(false));
    assert_eq!(delete_verdict("a", Some("b")), None);
}

/// A world for settle's refresh: `acct-a`'s stash holds `rt-a`, the
/// quarantine a fresher retired grant of it (`rt-d`) whose access token
/// got a 401.
fn expired_settle_world(home: &Path) -> (Context, OrcaView, String, Quarantine, String) {
    let (mut ctx, view, dir, stash) = retire_world(home, "");
    ctx.version_ok = true;
    std::fs::remove_dir_all(&dir).unwrap();
    let q = Quarantine::new(HostOs::Linux, &ctx.state);
    let fp = q
        .file(
            &creds_json("at-d", "rt-d", 50),
            Reason::Retired,
            "file",
            Some("acct-a"),
            Some((401, None)),
            1,
        )
        .unwrap()
        .fingerprint()
        .to_owned();
    (ctx, view, stash, q, fp)
}

/// A run bound to a launch never refreshes in settle: the process exits
/// a second after claude does, and a refresh cut between the rotation
/// and the filing of its answer would lose the only live refresh token.
#[test]
fn settle_does_not_refresh_beside_a_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, stash, q, fp) = expired_settle_world(tmp.path());
    let reply = r#"{"access_token":"at-rot","refresh_token":"rt-rot","expires_in":3600}"#;
    let http = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(200, reply))
        .profile_uuid("at-rot", "u-a");
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &FakeProcs::default(),
        SettleOpts {
            refresh: false,
            ..settle_opts(&[])
        },
        &mut st,
        &mut report,
    );
    assert_eq!(waiting, 1);
    assert!(http.token_bodies.lock().unwrap().is_empty());
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    let list = q.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].fingerprint, fp);
    assert_eq!(list[0].reason, Reason::Retired);
    assert!(
        report
            .pending
            .iter()
            .any(|l| l.contains("never beside a launch")),
        "{:?}",
        report.pending
    );
}

/// A process table that becomes unreadable (so Orca counts as running)
/// once `on` is set.
struct FlipProcs<'a> {
    on: &'a std::cell::Cell<bool>,
}

impl crate::orca::live::ProcFacts for FlipProcs<'_> {
    fn alive(&self, _: u32) -> bool {
        false
    }
    fn probe(&self, _: u32) -> Option<crate::platform::proc::ProcInfo> {
        None
    }
    fn table(&self) -> Option<Vec<crate::platform::proc::ProcInfo>> {
        (!self.on.get()).then(Vec::new)
    }
}

/// The token endpoint, where Orca starts while the refresh is in flight.
struct OrcaStartsDuringRefresh<'a> {
    inner: &'a FakeHttp,
    on: &'a std::cell::Cell<bool>,
}

impl crate::orca::http::OauthHttp for OrcaStartsDuringRefresh<'_> {
    fn post_token(
        &self,
        body: &str,
    ) -> Result<crate::orca::http::HttpReply, crate::orca::http::HttpError> {
        self.on.set(true);
        self.inner.post_token(body)
    }
    fn get_profile(
        &self,
        token: &str,
    ) -> Result<crate::orca::http::HttpReply, crate::orca::http::HttpError> {
        self.inner.get_profile(token)
    }
}

/// Orca starting during settle's refresh (its liveness was checked before
/// two network calls): the stash is not written behind it, and the
/// rotated grant, now the only live copy, stays in the quarantine with
/// the entry it came from.
#[test]
fn settle_refresh_never_writes_a_stash_behind_an_orca_that_started() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, view, stash, q, fp) = expired_settle_world(tmp.path());
    let reply = r#"{"access_token":"at-rot","refresh_token":"rt-rot","expires_in":3600}"#;
    let inner = FakeHttp::default()
        .profile("at-d", FakeHttp::reply(401, "{}"))
        .token_reply(FakeHttp::reply(200, reply))
        .profile_uuid("at-rot", "u-a");
    let on = std::cell::Cell::new(false);
    let http = OrcaStartsDuringRefresh {
        inner: &inner,
        on: &on,
    };
    let procs = FlipProcs { on: &on };
    assert!(!ctx.orca_running(&procs));
    let mut st = super::state::MigrationState::default();
    let mut report = super::Report::default();
    let waiting = settle(
        &ctx,
        &view,
        &http,
        &procs,
        settle_opts(&[]),
        &mut st,
        &mut report,
    );
    assert!(on.get(), "the refresh ran");
    assert!(ctx.orca_running(&procs));
    assert_eq!(waiting, 1, "{:?}", report.pending);
    assert_eq!(stash_creds(&ctx, &view, "acct-a"), stash);
    let reasons: Vec<(String, Reason)> = q
        .list()
        .into_iter()
        .map(|m| (m.fingerprint, m.reason))
        .collect();
    assert!(
        reasons.contains(&(fp.clone(), Reason::Retired)),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().any(|(_, r)| *r == Reason::Rotated),
        "{reasons:?}"
    );
    let rotated = q
        .list()
        .into_iter()
        .find(|m| m.reason == Reason::Rotated)
        .unwrap();
    assert!(
        q.get(&rotated.fingerprint)
            .unwrap()
            .unwrap()
            .expose()
            .contains("rt-rot")
    );
    assert!(
        report.pending.iter().any(|l| l.contains("Orca started")),
        "{:?}",
        report.pending
    );
}

/// `csm migrate --dry-run` at the cutover reads no secret (decision 6):
/// whether step 1 has work is decided without the active stash's grant,
/// so any `~/.claude/.credentials.json` counts. A real run compares it
/// with the stash (a `-w` read) and leaves the stash's own copy alone.
#[cfg(unix)]
#[test]
fn a_dry_run_cutover_reads_no_stash_secret() {
    let fake = crate::orca::testsupport::FakeSecurity::install();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env = HostEnv::for_test(home, HostOs::MacOs);
    let procs = FakeProcs::default();
    let ctx = Context::from_env(env.clone(), &procs);
    let ud = ctx.user_data.dir.clone();
    let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
    make_stash(&ud, "acct-a", Some(&alice), None);
    let stash = creds_json("at-stash", "rt-stash", 1);
    fake.put(keychain::STASH_SERVICE, "acct-a", stash.as_bytes());
    write_store(
        &ud,
        &[record_json(&ud, "acct-a", "alice@example.com", None)],
        Some("acct-a"),
    );
    std::fs::create_dir_all(&ctx.paths.config_dir).unwrap();
    std::fs::write(&ctx.paths.credentials_path, &stash).unwrap();
    let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
    let store = StoreKind::Json(Ok(()));

    let before = fake.argv().len();
    assert!(neutralise_needed(
        &ctx,
        &view,
        Some("acct-a"),
        false,
        &store,
        true
    ));
    let calls: Vec<String> = fake.argv().split_off(before);
    for c in &calls {
        assert!(!c.split(' ').any(|t| t == "-w"), "a secret read: {c}");
    }

    // For real: the file is the stash's grant, so step 1 has nothing.
    let before = fake.argv().len();
    assert!(!neutralise_needed(
        &ctx,
        &view,
        Some("acct-a"),
        false,
        &store,
        false
    ));
    let calls: Vec<String> = fake.argv().split_off(before);
    assert!(
        calls.iter().any(|c| c.split(' ').any(|t| t == "-w")),
        "{calls:?}"
    );
    // Orca in ~/.claude, or a store-less host: never.
    assert!(!neutralise_needed(
        &ctx,
        &view,
        Some("acct-a"),
        true,
        &store,
        true
    ));
    assert!(!neutralise_needed(
        &ctx,
        &view,
        Some("acct-a"),
        false,
        &StoreKind::StoreLess,
        true
    ));
}

/// Linux or Windows with a store: the floor dir stays while `~/.claude`
/// holds no login (csm launches with Orca stopped still run there); other
/// dirs do not wait on it.
#[test]
fn the_floor_waits_while_home_holds_no_login() {
    use crate::orca::live::DirUsers;
    let free = DirUsers::Free;
    let pass = FloorGate::Pass;
    let base = gate_facts(&free, &pass);
    let f = RetireFacts {
        is_floor: true,
        home_empty: true,
        ..base
    };
    assert!(
        matches!(retire_gate(&f), RetireGate::Wait(w) if w.contains("no login yet")),
        "{:?}",
        retire_gate(&f)
    );
    let f = RetireFacts {
        is_floor: false,
        home_empty: true,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
    let f = RetireFacts {
        is_floor: true,
        ..base
    };
    assert_eq!(retire_gate(&f), RetireGate::Go);
}

// ─── the pre-spawn budget ─────────────────────────────────────────────────────

/// A stalled stage A never holds the launch past its budget: `bounded`
/// returns once the budget runs out, the worker keeps running, and its
/// result still arrives for the post-spawn run to finish.
#[test]
fn a_stalled_prespawn_worker_returns_at_the_budget() {
    use std::sync::mpsc;
    use std::time::Instant;
    assert_eq!(super::PRESPAWN_BUDGET, Duration::from_secs(3));
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let budget = Duration::from_millis(200);
    let t0 = Instant::now();
    let got = super::bounded(budget, move || {
        let _ = gate_rx.recv();
        7u32
    });
    let waited = t0.elapsed();
    assert!(waited >= budget, "returned before the budget: {waited:?}");
    assert!(
        waited < Duration::from_secs(2),
        "held past the budget: {waited:?}"
    );
    let super::Bounded::Running(handle, rx) = got else {
        panic!("a stalled worker must still be running");
    };
    assert!(!handle.is_finished());
    gate_tx.send(()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(7));
    handle.join().unwrap();
}

#[test]
fn a_prompt_prespawn_worker_is_done_and_joined() {
    let got = super::bounded(Duration::from_secs(5), || "ok");
    assert!(matches!(got, super::Bounded::Done("ok")));
}

#[test]
fn a_prespawn_worker_that_dies_is_a_failure_not_a_wait() {
    let got = super::bounded(Duration::from_secs(5), || -> u32 {
        panic!("stage A died (expected in this test)")
    });
    assert!(matches!(got, super::Bounded::Failed));
}
