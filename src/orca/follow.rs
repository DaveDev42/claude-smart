//! The effective current profile for a launch — one pure decision that every
//! `csm run` fallback path (`--no-pick`, `Ok(None)`, all-saturated, stale
//! fallback, the `-i` picker default) routes through, so none of them can
//! land in the Orca slot by accident.
//!
//! Outside Orca mode it reproduces the legacy `current_profile_dir` /
//! `derive_current_profile_name` pair exactly: `$CLAUDE_CONFIG_DIR` when set
//! (reverse-looked-up, else named after its `.claude.<name>` leaf), else the
//! default profile.
//!
//! In Orca mode (a slot exists), with no explicit `--profile` pin:
//! - env is a registered non-slot profile whose dir is not the default dir →
//!   a deliberate per-shell pin: that profile, no RPC;
//! - env is some other, unregistered dir → also a deliberate pin, launched
//!   as-is (legacy behaviour; it is not the slot);
//! - env ∈ {unset, slot, default dir} → follow, in order: (1) a valid pending
//!   select's profile, (2) the profile bound to Orca's LIVE active account,
//!   (3) csm's default state, (4) the env dir's profile, (5) the
//!   alphabetically first non-slot profile. The slot is returned only when it
//!   is the ONLY registered profile.
//!
//! (1) and (2) set `prefer_current` (keep the current profile while it is
//! viable) and, when the profile differs from the default state,
//! `mirror_default` (the caller rewrites csm's default to it).

use std::path::Path;

use super::Selection;
use super::bind::Bindings;
use super::pending::{self, PendingSelect, Verdict};
use super::slot::Slot;
use crate::account::ProfileMap;
use crate::cas::platform::dirs_equal;

/// Everything the decision reads. `live` is fetched lazily by the caller —
/// only when [`needs_live`] says so.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveInput<'a> {
    /// `--profile <name>` and its resolved dir.
    pub explicit_pin: Option<(&'a str, &'a str)>,
    /// Inherited `CLAUDE_CONFIG_DIR` (`None` when unset or empty).
    pub env_dir: Option<&'a str>,
    /// csm's default profile name (`ProfileMap::default_name`).
    pub default_state: &'a str,
    /// Its dir (`ProfileMap::default_dir`).
    pub default_dir: &'a str,
    /// The Orca slot while Orca mode is ON.
    pub slot: Option<&'a Slot>,
    pub registry: &'a ProfileMap,
    pub pending: Option<&'a PendingSelect>,
    /// Orca's live selection (`None` = not running / unknown / not fetched).
    pub live: Option<&'a Selection>,
    pub bindings: Option<&'a Bindings>,
    /// Epoch seconds (pending-select age).
    pub now: i64,
}

/// Why the decision landed where it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--profile`.
    ExplicitPin,
    /// A registered non-slot `CLAUDE_CONFIG_DIR` ≠ the default dir.
    EnvPin,
    /// An unregistered `CLAUDE_CONFIG_DIR` (launched as-is).
    EnvUnregistered,
    /// A valid pending select.
    Pending,
    /// The profile bound to Orca's live active account.
    OrcaLive,
    /// csm's default state.
    DefaultState,
    /// The env dir's registered profile.
    EnvDir,
    /// Alphabetically first non-slot profile.
    FirstProfile,
    /// The slot, because it is the only registered profile.
    SlotOnly,
    /// Non-Orca mode: legacy env/default resolution.
    Legacy,
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDecision {
    pub name: String,
    pub dir: String,
    pub source: Source,
    /// Keep this profile while it is viable (the proactive pick returns
    /// `Ok(None)`); true only for [`Source::Pending`] / [`Source::OrcaLive`].
    pub prefer_current: bool,
    /// Rewrite csm's default state to this profile.
    pub mirror_default: Option<String>,
}

impl EffectiveDecision {
    fn plain(name: &str, dir: &str, source: Source) -> Self {
        EffectiveDecision {
            name: name.to_owned(),
            dir: dir.to_owned(),
            source,
            prefer_current: false,
            mirror_default: None,
        }
    }
}

/// How the inherited env dir relates to the registry and the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvClass<'a> {
    /// Unset, the slot dir, or the default profile's dir: follow Orca.
    Follow,
    /// A registered non-slot profile's dir other than the default dir.
    RegisteredPin(&'a str),
    /// A dir no profile claims.
    Unregistered(&'a str),
}

fn lookup_dir<'a>(registry: &'a ProfileMap, dir: &str) -> Option<(&'a str, &'a str)> {
    let mut names = registry.names_sorted();
    names.retain(|n| registry.get(n).is_some_and(|d| dirs_equal(d, dir)));
    names
        .first()
        .map(|n| (*n, registry.get(n).unwrap_or_default()))
}

fn classify_env<'a>(i: &EffectiveInput<'a>, slot: &Slot) -> EnvClass<'a> {
    let Some(env) = i.env_dir else {
        return EnvClass::Follow;
    };
    if slot.is_dir(env) || dirs_equal(env, i.default_dir) {
        return EnvClass::Follow;
    }
    match lookup_dir(i.registry, env) {
        Some((name, _)) if !slot.is_profile(name) => EnvClass::RegisteredPin(name),
        Some(_) => EnvClass::Follow,
        None => EnvClass::Unregistered(env),
    }
}

/// Does the decision need Orca's live selection? (Only in Orca mode, with no
/// pin and an env dir that follows Orca.) Lets the caller skip the RPC.
pub fn needs_live(i: &EffectiveInput<'_>) -> bool {
    match (i.slot, i.explicit_pin) {
        (Some(slot), None) => classify_env(i, slot) == EnvClass::Follow,
        _ => false,
    }
}

/// Legacy (non-Orca) resolution: env dir (reverse lookup, else its
/// `.claude.<name>` leaf), else the default profile.
fn legacy(i: &EffectiveInput<'_>) -> EffectiveDecision {
    match i.env_dir {
        Some(env) => {
            let name = i
                .registry
                .iter()
                .find(|(_, d)| *d == env)
                .map(|(n, _)| n.to_owned())
                .or_else(|| {
                    Path::new(env)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.strip_prefix(".claude.").unwrap_or(n).to_owned())
                })
                .unwrap_or_else(|| i.default_state.to_owned());
            EffectiveDecision::plain(&name, env, Source::Legacy)
        }
        None => EffectiveDecision::plain(i.default_state, i.default_dir, Source::Legacy),
    }
}

/// The effective current profile. Pure. See the module doc for the order.
pub fn effective_current(i: &EffectiveInput<'_>) -> EffectiveDecision {
    if let Some((name, dir)) = i.explicit_pin {
        return EffectiveDecision::plain(name, dir, Source::ExplicitPin);
    }
    let Some(slot) = i.slot else {
        return legacy(i);
    };
    match classify_env(i, slot) {
        EnvClass::RegisteredPin(name) => {
            let dir = i.registry.get(name).unwrap_or_default();
            return EffectiveDecision::plain(name, dir, Source::EnvPin);
        }
        EnvClass::Unregistered(env) => {
            let mut d = legacy(i);
            d.dir = env.to_owned();
            d.source = Source::EnvUnregistered;
            return d;
        }
        EnvClass::Follow => {}
    }

    // A registered non-slot profile name → its (name, dir).
    let usable = |name: &str| -> Option<(String, String)> {
        if slot.is_profile(name) {
            return None;
        }
        let dir = i.registry.get(name)?;
        if slot.is_dir(dir) {
            return None;
        }
        Some((name.to_owned(), dir.to_owned()))
    };
    let followed = |name: String, dir: String, source| {
        let mirror = (name != i.default_state).then(|| name.clone());
        EffectiveDecision {
            name,
            dir,
            source,
            prefer_current: true,
            mirror_default: mirror,
        }
    };
    let live_active = i.live.and_then(Selection::effective_active_id);

    // (1) a valid pending select.
    if let (Some(p), Some(_)) = (i.pending, i.live)
        && pending::verdict(p, live_active, i.now) == Verdict::Valid
        && let Some((n, d)) = usable(&p.profile)
    {
        return followed(n, d, Source::Pending);
    }
    // (2) the profile bound to Orca's live active account.
    if let (Some(id), Some(b)) = (live_active, i.bindings)
        && let Some(p) = b.profile_for_active(id, Some(i.default_state))
        && let Some((n, d)) = usable(p)
    {
        return followed(n, d, Source::OrcaLive);
    }
    // (3) csm's default state.
    if let Some((n, d)) = usable(i.default_state) {
        return EffectiveDecision::plain(&n, &d, Source::DefaultState);
    }
    // (4) the env dir's registered profile.
    if let Some(env) = i.env_dir
        && let Some((name, _)) = lookup_dir(i.registry, env)
        && let Some((n, d)) = usable(name)
    {
        return EffectiveDecision::plain(&n, &d, Source::EnvDir);
    }
    // (5) alphabetically first non-slot profile.
    if let Some((n, d)) = i.registry.names_sorted().into_iter().find_map(usable) {
        return EffectiveDecision::plain(&n, &d, Source::FirstProfile);
    }
    // Only the slot exists.
    EffectiveDecision::plain(&slot.name, &slot.dir, Source::SlotOnly)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::bind::Binding;
    use crate::orca::bind::MatchKind;
    use std::collections::HashMap;

    const SLOT: &str = "/Users/example/.claude.orca";
    const WORK: &str = "/Users/example/.claude.work";
    const HOME: &str = "/Users/example/.claude.home";

    fn registry(entries: &[(&str, &str)]) -> ProfileMap {
        ProfileMap(
            entries
                .iter()
                .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
                .collect::<HashMap<_, _>>(),
        )
    }

    fn full() -> ProfileMap {
        registry(&[("orca", SLOT), ("work", WORK), ("home", HOME)])
    }

    fn slot() -> Slot {
        Slot {
            name: "orca".into(),
            dir: SLOT.into(),
        }
    }

    fn live(active: Option<&str>) -> Selection {
        Selection {
            accounts: vec![],
            active_id: active.map(Into::into),
            host_active_id: None,
            rate_limits: None,
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> Bindings {
        let mut b = Bindings::default();
        for (acct, prof) in pairs {
            b.by_account.insert(
                (*acct).to_owned(),
                Binding {
                    profile: (*prof).to_owned(),
                    kind: MatchKind::Uuid,
                    via_override: false,
                    candidates: vec![(*prof).to_owned()],
                },
            );
        }
        b
    }

    fn input<'a>(
        reg: &'a ProfileMap,
        slot: Option<&'a Slot>,
        env: Option<&'a str>,
        default_state: &'a str,
        default_dir: &'a str,
    ) -> EffectiveInput<'a> {
        EffectiveInput {
            explicit_pin: None,
            env_dir: env,
            default_state,
            default_dir,
            slot,
            registry: reg,
            pending: None,
            live: None,
            bindings: None,
            now: 1_000_000,
        }
    }

    #[test]
    fn non_orca_mode_is_legacy() {
        let reg = full();
        let i = input(&reg, None, None, "work", WORK);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.dir.as_str()), ("work", WORK));
        assert!(!needs_live(&i));

        let i = input(&reg, None, Some(HOME), "work", WORK);
        assert_eq!(effective_current(&i).name, "home");

        let i = input(
            &reg,
            None,
            Some("/Users/example/.claude.extra"),
            "work",
            WORK,
        );
        let d = effective_current(&i);
        assert_eq!(
            d.name, "extra",
            "leaf-derived name, like derive_current_profile_name"
        );
        assert_eq!(d.source, Source::Legacy);

        // Legacy mode may return the slot-named profile: nothing is a slot.
        let i = input(&reg, None, Some(SLOT), "work", WORK);
        assert_eq!(effective_current(&i).name, "orca");
    }

    #[test]
    fn explicit_pin_wins_even_for_the_slot() {
        let reg = full();
        let s = slot();
        let mut i = input(&reg, Some(&s), Some(WORK), "work", WORK);
        i.explicit_pin = Some(("orca", SLOT));
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("orca", Source::ExplicitPin));
        assert!(!needs_live(&i));
    }

    #[test]
    fn env_pin_to_a_non_default_profile_skips_orca() {
        let reg = full();
        let s = slot();
        let l = live(Some("acct-w"));
        let b = bindings(&[("acct-w", "work")]);
        let mut i = input(&reg, Some(&s), Some(HOME), "work", WORK);
        i.live = Some(&l);
        i.bindings = Some(&b);
        assert!(!needs_live(&i));
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("home", Source::EnvPin));
        assert!(!d.prefer_current);
    }

    #[test]
    fn unregistered_env_dir_launches_as_is() {
        let reg = full();
        let s = slot();
        let i = input(
            &reg,
            Some(&s),
            Some("/Users/example/.claude.extra"),
            "work",
            WORK,
        );
        assert!(!needs_live(&i));
        let d = effective_current(&i);
        assert_eq!(d.source, Source::EnvUnregistered);
        assert_eq!(d.dir, "/Users/example/.claude.extra");
    }

    #[test]
    fn follows_orcas_live_active_account_and_mirrors_default() {
        let reg = full();
        let s = slot();
        let l = live(Some("acct-h"));
        let b = bindings(&[("acct-h", "home"), ("acct-w", "work")]);
        for env in [
            None,
            Some(SLOT),
            Some(WORK),
            Some("/Users/example/.claude.orca/"),
        ] {
            let mut i = input(&reg, Some(&s), env, "work", WORK);
            assert!(needs_live(&i), "env {env:?} follows Orca");
            i.live = Some(&l);
            i.bindings = Some(&b);
            let d = effective_current(&i);
            assert_eq!((d.name.as_str(), d.source), ("home", Source::OrcaLive));
            assert!(d.prefer_current);
            assert_eq!(d.mirror_default.as_deref(), Some("home"));
        }
        // Already the default → no mirror.
        let mut i = input(&reg, Some(&s), None, "home", HOME);
        i.live = Some(&l);
        i.bindings = Some(&b);
        assert_eq!(effective_current(&i).mirror_default, None);
    }

    #[test]
    fn valid_pending_select_beats_live() {
        let reg = full();
        let s = slot();
        let l = live(Some("acct-h"));
        let b = bindings(&[("acct-h", "home"), ("acct-w", "work")]);
        let p = PendingSelect::new("acct-w", "work", 1_000_000 - 10, Some("acct-h"));
        let mut i = input(&reg, Some(&s), None, "home", HOME);
        i.live = Some(&l);
        i.bindings = Some(&b);
        i.pending = Some(&p);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("work", Source::Pending));
        assert!(d.prefer_current);

        // Superseded (Orca moved on) → ignored, live wins.
        let stale = PendingSelect::new("acct-w", "work", 1_000_000 - 10, Some("acct-x"));
        i.pending = Some(&stale);
        assert_eq!(effective_current(&i).source, Source::OrcaLive);

        // Orca not running → pending cannot be validated; default state.
        i.live = None;
        i.pending = Some(&p);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("home", Source::DefaultState));
        assert!(!d.prefer_current);
    }

    #[test]
    fn unbound_live_account_falls_back_to_default_state() {
        let reg = full();
        let s = slot();
        let l = live(Some("acct-unknown"));
        let b = bindings(&[("acct-h", "home")]);
        let mut i = input(&reg, Some(&s), None, "work", WORK);
        i.live = Some(&l);
        i.bindings = Some(&b);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("work", Source::DefaultState));
        assert_eq!(d.mirror_default, None);
    }

    #[test]
    fn fallbacks_never_return_the_slot() {
        let reg = full();
        let s = slot();
        // Default state IS the slot; env is the slot; no live.
        let i = input(&reg, Some(&s), Some(SLOT), "orca", SLOT);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("home", Source::FirstProfile));

        // A binding pointing at the slot (misconfigured override) is ignored.
        let l = live(Some("acct-o"));
        let b = bindings(&[("acct-o", "orca")]);
        let mut i = input(&reg, Some(&s), None, "orca", SLOT);
        i.live = Some(&l);
        i.bindings = Some(&b);
        assert_ne!(effective_current(&i).name, "orca");

        // A non-slot profile registered AT the slot dir is not usable either.
        let reg2 = registry(&[("orca", SLOT), ("alias", SLOT), ("work", WORK)]);
        let i = input(&reg2, Some(&s), None, "alias", SLOT);
        assert_eq!(effective_current(&i).name, "work");
    }

    #[test]
    fn env_dir_step_when_default_state_is_unusable() {
        // Default state names the slot, env points at the default dir which
        // (odd but possible) is registered under another profile.
        let reg = registry(&[("orca", SLOT), ("work", WORK), ("alpha", HOME)]);
        let s = slot();
        let i = input(&reg, Some(&s), Some(WORK), "orca", WORK);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("work", Source::EnvDir));
    }

    #[test]
    fn slot_only_registry_launches_into_the_slot() {
        let reg = registry(&[("orca", SLOT)]);
        let s = slot();
        let i = input(&reg, Some(&s), None, "orca", SLOT);
        let d = effective_current(&i);
        assert_eq!((d.name.as_str(), d.source), ("orca", Source::SlotOnly));
    }

    #[test]
    fn tie_keeps_the_current_default() {
        let reg = full();
        let s = slot();
        let l = live(Some("acct-1"));
        let mut b = Bindings::default();
        b.by_account.insert(
            "acct-1".into(),
            Binding {
                profile: "home".into(),
                kind: MatchKind::Uuid,
                via_override: false,
                candidates: vec!["home".into(), "work".into()],
            },
        );
        let mut i = input(&reg, Some(&s), None, "work", WORK);
        i.live = Some(&l);
        i.bindings = Some(&b);
        let d = effective_current(&i);
        assert_eq!(d.name, "work");
        assert_eq!(d.mirror_default, None);
    }
}
