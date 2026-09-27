//! The read-only preview of every legacy dir against Orca's accounts and
//! of what the files stages would change, rendered by `csm migrate
//! --dry-run`. Pure over the facts [`build_plan`] gathers.

use std::path::PathBuf;

use crate::orca::OrcaView;
use crate::orca::context::Context;
use crate::orca::record::AccountRecord;
use crate::orca::runtime::{read_json_object, runtime_paths};

use super::carry::*;
use super::legacy::*;

// ─── plan ─────────────────────────────────────────────────────────────────────

/// One plan row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub facts: ProfileFacts,
    pub status: Status,
    pub stash: Option<GrantFacts>,
    pub floor: bool,
}

/// The whole plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub rows: Vec<Row>,
    pub orca_running: bool,
    pub orca_d: Option<PathBuf>,
    pub target_d: PathBuf,
    pub target_config: PathBuf,
    /// B2's additions from the floor profile.
    pub merge: Vec<String>,
    /// B2 seeds a new file with this many of the floor profile's keys.
    pub seeded: Option<usize>,
    /// Why B2 cannot run: the floor profile's `.claude.json` cannot be
    /// read or is not a JSON object.
    pub merge_blocked: Option<String>,
    /// Trust and MCP keys B2 adds from each other profile (add-only).
    pub differs: Vec<(String, Vec<String>)>,
    pub shared: Vec<(&'static str, SharedStep)>,
    /// Recorded plugin paths that go through an old profile's or the
    /// shared plugins dir (B5 points them at `~/.claude/plugins`).
    pub plugin_paths: usize,
    /// Per profile, what [`left_behind`] found.
    pub left_behind: Vec<(String, Vec<String>)>,
    /// csm's new state dir, and B4's (moving, staying) count over the old
    /// one.
    pub state_dir: PathBuf,
    pub smart: Option<(usize, usize)>,
}

pub(crate) fn build_plan(ctx: &Context, view: &OrcaView, legacy: &Legacy, probe: Probe) -> Plan {
    let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    // A stash's grant is a secret too: read only when the grants are.
    let sg = |id: &str| match probe {
        Probe::Read => stash_grant(ctx, view, id),
        Probe::Presence => None,
    };
    let rows: Vec<Row> = legacy
        .profiles
        .iter()
        .map(|p| {
            let facts = profile_facts(ctx, p, probe);
            let status = classify(&facts, &host, &sg);
            let stash = match &status {
                Status::InOrca { id, .. } => sg(id),
                _ => None,
            };
            Row {
                floor: legacy.floor.as_deref() == Some(p.name.as_str()),
                facts,
                status,
                stash,
            }
        })
        .collect();
    let home = &ctx.env.home;
    let target = runtime_paths(None, home, |p| p.exists());
    // B2 writes ~/.claude.json whatever runtime_paths picks (I3).
    let target_config = home.join(".claude.json");
    let existing = target_config
        .exists()
        .then(|| read_json_object(&target_config).unwrap_or_default());
    let mut merged = existing.clone().unwrap_or_default();
    let mut merge = Vec::new();
    let mut seeded = None;
    let mut merge_blocked = None;
    if let Some(floor) = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
    {
        match read_floor_config(&floor.dir) {
            Ok(Some((from, _))) => {
                let c = carry_config(existing, &from);
                merged = c.map;
                merge = c.added;
                seeded = c.seeded;
            }
            Ok(None) => {}
            Err(why) => merge_blocked = Some(why),
        }
    }
    let differs = legacy
        .profiles
        .iter()
        .filter(|p| legacy.floor.as_deref() != Some(p.name.as_str()))
        .filter_map(|p| {
            let from = read_json_object(&p.dir.join(".claude.json"))?;
            let extra = merge_trust_mcp(&mut merged, &from, false);
            (!extra.is_empty()).then(|| (p.name.clone(), extra))
        })
        .collect();
    Plan {
        rows,
        orca_running: view.running,
        orca_d: view.orca_runtime_dir.clone(),
        target_d: target.config_dir.clone(),
        target_config,
        merge,
        seeded,
        merge_blocked,
        differs,
        shared: shared_plan(home),
        plugin_paths: plugin_paths_preview(home, &legacy_dirs(legacy)),
        left_behind: legacy
            .profiles
            .iter()
            .map(|p| (p.name.clone(), left_behind(home, &p.dir)))
            .filter(|(_, v)| !v.is_empty())
            .collect(),
        state_dir: ctx.state.clone(),
        smart: smart_preview(&legacy_smart_dir(home)),
    }
}

pub(crate) fn legacy_dirs(legacy: &Legacy) -> Vec<PathBuf> {
    legacy.profiles.iter().map(|p| p.dir.clone()).collect()
}

pub(crate) fn fmt_exp(e: Option<f64>) -> String {
    match e.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64)) {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => "?".into(),
    }
}

/// Render the plan. Pure.
pub(crate) fn render_plan(p: &Plan) -> String {
    let mut o = String::new();
    o.push_str("profiles (~/.config/claude-as/profiles.json):\n");
    if p.rows.is_empty() {
        o.push_str("  (none: nothing to migrate)\n");
    }
    for r in &p.rows {
        let who = r.facts.email.as_deref().unwrap_or("-");
        let floor = if r.floor { " [floor]" } else { "" };
        let status = match &r.status {
            Status::InOrca { id, fresher } => format!(
                "in Orca ({id}){}",
                match fresher {
                    Some(true) => "; the dir's grant is fresher: import reads it back",
                    Some(false) => "",
                    None => "; import compares the dir's grant with the stash",
                }
            ),
            Status::ToImport => "to import".into(),
            Status::NoCredentials => "no credentials".into(),
        };
        o.push_str(&format!(
            "  {}{floor}  {}  {who}\n    {status}\n",
            r.facts.name,
            r.facts.dir.display()
        ));
        if let Some(g) = &r.facts.grant {
            o.push_str(&format!(
                "    dir grant   {}  expires {}\n",
                g.fingerprint,
                fmt_exp(g.expires_at)
            ));
        } else if !r.facts.grant_sources.is_empty() {
            o.push_str(&format!(
                "    dir grant   present ({}; not read)\n",
                r.facts.grant_sources.join(", ")
            ));
        }
        if let Some(g) = &r.stash {
            o.push_str(&format!(
                "    stash grant {}  expires {}\n",
                g.fingerprint,
                fmt_exp(g.expires_at)
            ));
        }
    }
    o.push_str(&format!(
        "\nOrca: {}; its D: {}\n",
        if p.orca_running { "running" } else { "stopped" },
        p.orca_d
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "unknown".into())
    ));
    o.push_str(&format!("target D: {}\n", p.target_d.display()));
    o.push_str(&format!(
        "\n{} gains from the floor profile:\n",
        p.target_config.display()
    ));
    if let Some(why) = &p.merge_blocked {
        o.push_str(&format!("  cannot merge: {why}; B2 waits until it reads\n"));
    } else if let Some(n) = p.seeded {
        o.push_str(&format!(
            "  (no file yet: seeded with the floor profile's {n} top-level key(s), oauthAccount left out)\n"
        ));
    } else if p.merge.is_empty() {
        o.push_str("  (nothing)\n");
    }
    for k in &p.merge {
        o.push_str(&format!("  + {k}\n"));
    }
    for (name, keys) in &p.differs {
        o.push_str(&format!(
            "  profile {name} adds (keys the file lacks only):\n"
        ));
        for k in keys {
            o.push_str(&format!("    {k}\n"));
        }
    }
    o.push_str("\n~/.claude shared dirs:\n");
    for (name, step) in &p.shared {
        o.push_str(&format!("  {name}: {}\n", step_line(step)));
    }
    if p.plugin_paths > 0 {
        o.push_str(&format!(
            "  plugin registries: {} recorded path(s) through an old plugins dir are pointed at ~/.claude/plugins\n",
            p.plugin_paths
        ));
    }
    if !p.left_behind.is_empty() {
        o.push_str(
            "\nper-profile content csm does not carry (it stays in <dir>.retired; move what ~/.claude should keep by hand):\n",
        );
        for (name, names) in &p.left_behind {
            o.push_str(&format!("  {name}: {}\n", names.join(", ")));
        }
        if p.left_behind
            .iter()
            .any(|(_, v)| v.iter().any(|n| n == "settings.json"))
        {
            o.push_str(
                "  note: a profile's settings.json (statusLine, hooks, enabledPlugins, permissions) is not \
                 carried; put what you need in ~/.claude/settings.json, including `csm statusline`, \
                 which the weekly-cap switch runs from\n",
            );
        }
    }
    match p.smart {
        None => o.push_str("  ~/.claude.shared/smart: none\n"),
        Some((carry, stay)) => o.push_str(&format!(
            "  ~/.claude.shared/smart: {carry} session file(s) move to {}; {stay} other entr{} (caches, markers, per-profile records) stay unread\n",
            p.state_dir.display(),
            if stay == 1 { "y" } else { "ies" }
        )),
    }
    o
}
