//! The host's handling of the settings a plugin declares in its manifest
//! (#1410), on the #887 `version` precedent.
//!
//! [`Manifest::settings`](hytte_plugin_proto::Manifest::settings) is whatever
//! the connected process put in its `Register` frame, so it is **untrusted**.
//! `sanitize` is the one gate it passes through, applied once at
//! registration (`register`); what survives is remembered per plugin
//! **instance id** — the id the connection registered under, which is the
//! `programs.trollshell.plugins.<id>` attribute name, so the bar and sidebar
//! `stats` instances each get their own form — and served to the
//! control-center by `Control.ListPluginSettings` ([`snapshot`]).
//!
//! # Remembered across sessions, forgotten with the plugin
//!
//! The form has to exist for a plugin that is **not** connected: one that is
//! switched off, or one that cannot start *because* its setting is missing
//! (vibectl without a reachable `V1BECTL_SERVER`). So the host keeps the last
//! list each id declared (`Store`), persisted to
//! `$XDG_STATE_HOME/trollshell/plugin-settings-schema.toml`. State, not config
//! (#866 decision 3): the shell writes it, nobody edits it, and it is
//! re-sanitised when read back all the same.
//!
//! A registration replaces its id's entry, so a plugin upgraded to declare
//! different settings shows its new form the moment it registers, and one that
//! stopped declaring any shows none. An id is **forgotten** when
//! `plugins.json` no longer declares it and it has not registered this session
//! (#1415 review L3): a renamed instance or a removed plugin does not keep a
//! form, and the cache stays as small as the set of plugins actually in use.
//! Registering this session is enough on its own, so a hand-installed static
//! unit — which `plugins.json` never declares — keeps its form while it runs.
//! When `plugins.json` cannot be read, nothing is forgotten.
//!
//! Recording is the **production host's** only: `register` acts only when
//! the connection's runtime store is the one `PluginsService::start` published
//! (`PLUGIN_RUNTIME`). The per-connection tests build their own store and never
//! publish it, so driving a `Register` through a test session can neither
//! pollute this process-wide map nor write the developer's real
//! `$XDG_STATE_HOME`.
//!
//! # Where the values go
//!
//! Not here. The values a person saves live in
//! `$XDG_CONFIG_HOME/trollshell/plugin-settings.toml`
//! ([`hytte_config::plugin_settings`]), and the launcher reads that file
//! itself at every launch (`plugin_launcher`), applying the same env-name rule
//! ([`Setting::env_refusal`]) to every key because the file is hand-editable.
//! The launcher cannot know a plugin's schema at its first launch, so it does
//! not consult this module at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use hytte_plugin_proto::manifest::{Setting, SettingKind};
use serde::{Deserialize, Serialize};

use super::PluginRuntimeStore;
use super::version::sanitize_capped;

/// The most settings the host keeps for one plugin; the rest are dropped with
/// one warning. A form longer than this is not a form anyone reads.
pub(super) const MAX_SETTINGS: usize = 32;

/// The longest [`Setting::label`] kept, in `char`s — a row title.
const MAX_LABEL_CHARS: usize = 64;

/// The longest [`Setting::doc`] kept, in `char`s — one or two sentences.
const MAX_DOC_CHARS: usize = 300;

/// The longest [`Setting::default`] kept, in `char`s. Generous, because a
/// default is often a path.
const MAX_DEFAULT_CHARS: usize = 256;

/// The longest option a [`SettingKind::Choice`] may carry, in `char`s.
const MAX_OPTION_CHARS: usize = 64;

/// The most options a [`SettingKind::Choice`] keeps.
const MAX_OPTIONS: usize = 32;

/// How many things one registration's [`sanitize`] names in the journal before
/// it summarises the rest in one line (#1415 review L4). A `Register` frame
/// can be 16 MiB; filled with refused entries, one warning each would be
/// hundreds of thousands of lines on every reconnect.
const MAX_DROP_WARNINGS: usize = 8;

/// `$XDG_STATE_HOME/trollshell/<this>.toml`: the last-seen declarations.
const CACHE_SUBSYSTEM: &str = "plugin-settings-schema";

/// Every plugin's sanitised declaration, by instance id.
pub type Schemas = BTreeMap<String, Vec<Setting>>;

/// Counts what one [`sanitize`] pass dropped and names only the first
/// [`MAX_DROP_WARNINGS`] of them.
struct Drops<'a> {
    id: &'a str,
    count: usize,
}

impl Drops<'_> {
    /// One thing dropped: named while under the cap, counted either way.
    fn note(&mut self, env: &str, reason: &str) {
        self.count += 1;
        if self.count <= MAX_DROP_WARNINGS {
            tracing::warn!(plugin = %self.id, env = ?env, reason, "plugin setting dropped");
        }
    }

    /// The one summary line for everything past the cap.
    fn finish(self) -> usize {
        if self.count > MAX_DROP_WARNINGS {
            tracing::warn!(
                plugin = %self.id,
                unnamed = self.count - MAX_DROP_WARNINGS,
                "more of this plugin's settings were dropped than are named above"
            );
        }
        self.count
    }
}

/// Reduce a plugin's declared settings to the ones the host will act on.
///
/// Per entry, dropped:
///
/// - an [`env`](Setting::env) that [`Setting::env_refusal`] refuses, or that an
///   earlier entry already declared (the first one wins);
/// - a kind this shell does not know ([`SettingKind::Unknown`], a newer
///   plugin's);
/// - an [`Int`](SettingKind::Int) whose `min` exceeds its `max`;
/// - a [`Choice`](SettingKind::Choice) left with no options.
///
/// And cleaned:
///
/// - `label`, `doc` and `default` are display text and go through the
///   `version` module's sanitiser (control, bidi and zero-width characters
///   stripped, ends trimmed, capped with a visible `…`). A label that comes out
///   empty falls back to the variable's name, so a row always has a title.
/// - A `Choice` option is a **value** the plugin receives, not display text,
///   so it is never altered: an option that sanitising would change at all
///   (strip, trim or cap) is dropped instead, as is a repeat. Passing the
///   plugin a string it never declared would be worse than not offering it.
///
/// At most [`MAX_SETTINGS`] survivors are kept, in declaration order. Each
/// drop is a warning naming the plugin and the variable, up to
/// [`MAX_DROP_WARNINGS`] per call and then one summary line.
pub(super) fn sanitize(id: &str, declared: &[Setting]) -> Vec<Setting> {
    let mut drops = Drops { id, count: 0 };
    let mut seen = BTreeSet::new();
    let mut kept = Vec::new();
    for setting in declared {
        match sanitize_one(setting) {
            Ok((clean, lost_options)) => {
                if lost_options > 0 {
                    drops.note(
                        &clean.env,
                        "some Choice options were not plain display text and were dropped",
                    );
                }
                if seen.insert(clean.env.clone()) {
                    kept.push(clean);
                } else {
                    drops.note(&clean.env, "declared twice; the first one is kept");
                }
            }
            Err(reason) => drops.note(&setting.env, reason),
        }
    }
    if kept.len() > MAX_SETTINGS {
        drops.note(
            &kept[MAX_SETTINGS].env,
            "past the 32 settings the host keeps; this one and the rest are dropped",
        );
        kept.truncate(MAX_SETTINGS);
    }
    drops.finish();
    kept
}

/// One entry of [`sanitize`]: the cleaned setting and how many of its
/// `Choice` options were dropped, or why the whole setting is.
fn sanitize_one(setting: &Setting) -> Result<(Setting, usize), &'static str> {
    if let Some(reason) = Setting::env_refusal(&setting.env) {
        return Err(reason);
    }
    let mut lost_options = 0;
    let kind = match &setting.kind {
        SettingKind::Unknown(_) => {
            return Err("its kind is newer than this shell");
        }
        SettingKind::Int { min, max } if min > max => {
            return Err("its Int range is empty (min > max)");
        }
        SettingKind::Choice { options } => {
            let (options, lost) = sanitize_options(options);
            if options.is_empty() {
                return Err("its Choice has no usable options");
            }
            lost_options = lost;
            SettingKind::Choice { options }
        }
        other => other.clone(),
    };
    let clean = Setting {
        env: setting.env.clone(),
        label: sanitize_capped(&setting.label, MAX_LABEL_CHARS)
            .unwrap_or_else(|| setting.env.clone()),
        doc: sanitize_capped(&setting.doc, MAX_DOC_CHARS).unwrap_or_default(),
        kind,
        default: setting
            .default
            .as_deref()
            .and_then(|d| sanitize_capped(d, MAX_DEFAULT_CHARS)),
    };
    Ok((clean, lost_options))
}

/// A `Choice`'s options, keeping only those that pass the display sanitiser
/// **unchanged** (see [`sanitize`]), without repeats, at most [`MAX_OPTIONS`];
/// and how many that are not plain display text it dropped.
fn sanitize_options(options: &[String]) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut lost = 0;
    for option in options {
        let intact = sanitize_capped(option, MAX_OPTION_CHARS).as_deref() == Some(option.as_str());
        if !intact {
            lost += 1;
        } else if !kept.contains(option) && kept.len() < MAX_OPTIONS {
            kept.push(option.clone());
        }
    }
    (kept, lost)
}

/// The on-disk shape of the cache: one array of settings per id.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    plugins: Schemas,
}

/// The remembered declarations, and where they persist.
#[derive(Debug)]
struct Store {
    /// The cache file, or `None` when neither `$XDG_STATE_HOME` nor `$HOME`
    /// is set — then nothing outlives the session.
    path: Option<PathBuf>,
    /// The last list each id declared, as persisted.
    cached: Schemas,
    /// Every id that registered this session. What the prune keeps even when
    /// `plugins.json` does not declare it — see the module doc.
    seen: BTreeSet<String>,
}

impl Store {
    /// A store over the cache at `path`, reading what an earlier session left
    /// there. Every list is re-sanitised on the way in: the file is the
    /// shell's own, but a hand edit or a newer shell's write must not bypass
    /// the gate. A cache that does not parse is empty (the next registration
    /// rewrites it).
    fn open(path: Option<PathBuf>) -> Self {
        let cached = path
            .as_deref()
            .and_then(hytte_config::state::load_at::<Cache>)
            .unwrap_or_default()
            .plugins
            .into_iter()
            .filter_map(|(id, list)| {
                let clean = sanitize(&id, &list);
                (!clean.is_empty()).then_some((id, clean))
            })
            .collect();
        Self {
            path,
            cached,
            seen: BTreeSet::new(),
        }
    }

    /// Record what `id` just declared, replacing what it declared before —
    /// persisted only when that actually changed, so the common reconnect with
    /// an unchanged manifest writes nothing. An empty list forgets the id.
    fn record(&mut self, id: &str, list: Vec<Setting>) {
        self.seen.insert(id.to_owned());
        let changed = if list.is_empty() {
            self.cached.remove(id).is_some()
        } else if self.cached.get(id) == Some(&list) {
            false
        } else {
            self.cached.insert(id.to_owned(), list);
            true
        };
        if changed {
            self.persist();
        }
    }

    /// Write the cache, or delete the file once nothing is cached.
    /// Best-effort: a failure costs the next session its forms for plugins
    /// that have not connected yet, and is logged.
    fn persist(&self) {
        let Some(path) = &self.path else {
            tracing::debug!("no state directory; plugin settings are not cached across sessions");
            return;
        };
        let result = if self.cached.is_empty() {
            hytte_config::state::remove_at(path)
        } else {
            hytte_config::state::store_at(
                path,
                &Cache {
                    plugins: self.cached.clone(),
                },
            )
        };
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "could not cache plugin settings");
        }
    }

    /// Every remembered declaration, after forgetting the ids neither
    /// `declared` in `plugins.json` nor registered this session (and
    /// persisting that, when it forgot any). `declared` is `None` when
    /// `plugins.json` could not be read: then nothing is forgotten.
    fn snapshot(&mut self, declared: Option<&BTreeSet<String>>) -> Schemas {
        if let Some(declared) = declared {
            let before = self.cached.len();
            self.cached
                .retain(|id, _| declared.contains(id) || self.seen.contains(id));
            if self.cached.len() != before {
                self.persist();
            }
        }
        self.cached.clone()
    }
}

/// The production host's store, opened lazily over the real cache path on
/// first use — by the first registration or the first `ListPluginSettings`,
/// whichever comes first.
static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(PoisonError::into_inner);
    let store =
        guard.get_or_insert_with(|| Store::open(hytte_config::state::path(CACHE_SUBSYSTEM)));
    f(store)
}

/// Whether `runtime` is the production host's store — the one
/// `PluginsService::start` published, as opposed to a test's.
fn is_production(runtime: &PluginRuntimeStore) -> bool {
    super::PLUGIN_RUNTIME
        .get()
        .is_some_and(|published| Arc::ptr_eq(published, runtime))
}

/// Sanitise and record what a newly-registered connection declared. Called
/// from the session handshake beside `runtime_register`, with the same
/// instance id.
///
/// The record — and the cache write it may cause — runs on a blocking thread
/// under the store's lock, so two plugins registering at once serialise their
/// read-modify-write rather than racing it, and the connection's own task
/// never waits on the disk. Sanitising (and its warnings) happens here,
/// synchronously, for every host.
pub(super) fn register(runtime: &PluginRuntimeStore, id: &str, declared: &[Setting]) {
    let clean = sanitize(id, declared);
    if !is_production(runtime) {
        return;
    }
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || with_store(|store| store.record(&id, clean)));
}

/// Every plugin's declared settings — what `Control.ListPluginSettings`
/// serves — after forgetting the ids `declared` (the ids `plugins.json`
/// declares, `None` when it cannot be read) no longer covers and that have
/// not registered this session. Blocking (the first call reads the cache
/// file, and a prune writes it), so the D-Bus handler runs it off its own
/// task.
#[must_use]
pub fn snapshot(declared: Option<&BTreeSet<String>>) -> Schemas {
    with_store(|store| store.snapshot(declared))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[Setting]) -> Vec<&str> {
        list.iter().map(|s| s.env.as_str()).collect()
    }

    // ── the sanitiser's truth table ─────────────────────────────────────

    #[test]
    fn refused_names_are_dropped_and_the_rest_kept_in_order() {
        let declared = [
            Setting::text("GOOD_ONE", "Good"),
            Setting::text("LD_PRELOAD", "Loader"),
            Setting::text("HYTTE_PLUGIN_ID", "Runtime"),
            Setting::text("XDG_CONFIG_HOME", "Session"),
            Setting::text("PATH", "Path"),
            Setting::text("HOME", "Home"),
            Setting::text("OPENROUTER_API_KEY", "Key"),
            Setting::text("lower_case", "Malformed"),
            Setting::text("1DIGIT", "Malformed"),
            Setting::text("", "Empty"),
            Setting::path("GOOD_TWO", "Also good"),
        ];
        assert_eq!(ids(&sanitize("p", &declared)), ["GOOD_ONE", "GOOD_TWO"]);
    }

    #[test]
    fn a_repeated_variable_keeps_its_first_entry() {
        let declared = [Setting::text("A", "First"), Setting::bool("A", "Second")];
        let kept = sanitize("p", &declared);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].label, "First");
    }

    #[test]
    fn at_most_the_cap_is_kept() {
        let declared: Vec<Setting> = (0..MAX_SETTINGS + 5)
            .map(|i| Setting::text(format!("S{i}"), "x"))
            .collect();
        let kept = sanitize("p", &declared);
        assert_eq!(kept.len(), MAX_SETTINGS);
        assert_eq!(kept[0].env, "S0", "the first ones survive");
    }

    #[test]
    fn display_text_is_stripped_and_capped() {
        let declared = [Setting::text("A", "Scre\u{202e}ens\n")
            .doc("x".repeat(MAX_DOC_CHARS * 2))
            .default_value("\u{200b}/etc/x\u{1b}")];
        let kept = &sanitize("p", &declared)[0];
        assert_eq!(kept.label, "Screens");
        assert_eq!(kept.doc.chars().count(), MAX_DOC_CHARS);
        assert!(kept.doc.ends_with('…'));
        assert_eq!(kept.default.as_deref(), Some("/etc/x"));
    }

    #[test]
    fn an_empty_label_falls_back_to_the_variable() {
        let kept = sanitize("p", &[Setting::text("V1BECTL_SERVER", "\u{200b} \n")]);
        assert_eq!(kept[0].label, "V1BECTL_SERVER");
        let blank_default = sanitize("p", &[Setting::text("A", "a").default_value("  ")]);
        assert_eq!(blank_default[0].default, None);
    }

    #[test]
    fn an_empty_int_range_is_dropped() {
        let declared = [
            Setting::int("BAD", "Bad", 5, 1),
            Setting::int("POINT", "Point", 3, 3),
        ];
        assert_eq!(ids(&sanitize("p", &declared)), ["POINT"]);
    }

    #[test]
    fn choice_options_are_values_and_are_never_altered() {
        let declared = [Setting::choice(
            "THEME",
            "Theme",
            ["dark", " padded ", "zero\u{200b}width", "dark", "light", ""],
        )];
        let kept = &sanitize("p", &declared)[0];
        assert_eq!(
            kept.kind,
            SettingKind::Choice {
                options: vec!["dark".into(), "light".into()]
            },
            "an option sanitising would change is dropped, not rewritten; repeats go too"
        );
        let hopeless = [Setting::choice("MODE", "Mode", ["\n", ""])];
        assert!(sanitize("p", &hopeless).is_empty(), "no usable option left");
        let many: Vec<String> = (0..MAX_OPTIONS + 3).map(|i| format!("o{i}")).collect();
        let SettingKind::Choice { options } =
            &sanitize("p", &[Setting::choice("M", "M", many)])[0].kind
        else {
            panic!("still a choice");
        };
        assert_eq!(options.len(), MAX_OPTIONS);
    }

    #[test]
    fn the_path_kind_keeps_its_directory_flag() {
        let kept = sanitize(
            "p",
            &[Setting::directory("D", "Dir"), Setting::path("F", "File")],
        );
        assert_eq!(kept[0].kind, SettingKind::Path { directory: true });
        assert_eq!(kept[1].kind, SettingKind::Path { directory: false });
    }

    /// #1415 review TS5: the label cap is 64 chars, a row title — not merely
    /// "some cap". Red if `MAX_LABEL_CHARS` is raised or the label stops going
    /// through the capping sanitiser.
    #[test]
    fn a_label_is_capped_at_a_row_title() {
        let kept = &sanitize("p", &[Setting::text("A", "x".repeat(1000))])[0];
        assert_eq!(kept.label.chars().count(), 64);
        assert!(kept.label.ends_with('…'));
        let default = &sanitize("p", &[Setting::text("A", "a").default_value("y".repeat(1000))])[0];
        assert_eq!(
            default.default.as_ref().map(|d| d.chars().count()),
            Some(256)
        );
    }

    /// #1415 review M2: a setting of a kind this shell does not know is
    /// dropped, and the plugin's other settings are kept.
    #[test]
    fn a_setting_of_a_newer_kind_is_dropped_and_the_rest_kept() {
        // The proto crate's committed newer-plugin frame: the only way to get
        // an `Unknown` kind, which nothing outside that crate can construct.
        let hex = include_str!(
            "../../../crates/hytte-plugin-proto/tests/fixtures/manifest_settings_future_kind_v1.hex"
        )
        .trim();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect();
        let decoded: hytte_plugin_proto::Manifest =
            hytte_plugin_proto::decode(&bytes).expect("a newer kind does not cost the registration");
        assert_eq!(decoded.settings.len(), 2, "the fixture's two readable entries");
        assert_eq!(ids(&sanitize("vibectl", &decoded.settings)), ["V1BECTL_SERVER"]);
    }

    /// #1415 review L4: a flood of refused entries is named only up to the
    /// cap, however many there are — and every one is still dropped.
    #[test]
    fn a_flood_of_refused_entries_is_counted_not_logged_one_by_one() {
        let declared: Vec<Setting> = (0..10_000)
            .map(|i| Setting::text(format!("LD_{i}"), "x"))
            .collect();
        let mut drops = Drops { id: "p", count: 0 };
        for setting in &declared {
            drops.note(&setting.env, "refused");
        }
        assert_eq!(drops.finish(), 10_000);
        assert!(sanitize("p", &declared).is_empty());
        let (captured, guard) = hytte_config::test_support::capture();
        let _ = sanitize("flood", &declared);
        drop(guard);
        let lines = captured.warnings();
        let named = lines
            .iter()
            .filter(|l| l.contains("plugin setting dropped"))
            .count();
        assert_eq!(named, MAX_DROP_WARNINGS, "{lines:?}");
        assert_eq!(
            lines.iter().filter(|l| l.contains("more of this plugin")).count(),
            1,
            "{lines:?}"
        );
    }

    // ── what is remembered, and what is forgotten ───────────────────────

    /// A registration replaces its id's list outright, and an empty one forgets
    /// the id — so an upgraded plugin shows its new form at once and one that
    /// stopped declaring settings shows none.
    #[test]
    fn a_registration_replaces_what_the_id_declared_before() {
        let mut store = Store::open(None);
        store.record("a", vec![Setting::text("A", "first")]);
        store.record("b", vec![Setting::text("B", "b")]);
        store.record("a", vec![Setting::bool("A2", "second")]);
        store.record("b", Vec::new());
        let out = store.snapshot(None);
        assert_eq!(out.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(out["a"][0].label, "second");
    }

    /// #1415 review L3: an id `plugins.json` no longer declares, and that has
    /// not registered this session, is forgotten — on disk too. One that
    /// registered this session (a hand-installed static unit) is kept, and an
    /// unreadable `plugins.json` forgets nothing.
    #[test]
    fn an_id_neither_declared_nor_seen_is_forgotten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut first = Store::open(Some(path.clone()));
        for id in ["kept", "renamed", "static"] {
            first.record(id, vec![Setting::text("A", id)]);
        }

        let mut next = Store::open(Some(path.clone()));
        next.record("static", vec![Setting::text("A", "static")]);
        assert_eq!(
            next.snapshot(None).len(),
            3,
            "no plugins.json reading, nothing forgotten"
        );
        let declared = BTreeSet::from(["kept".to_owned()]);
        let out = next.snapshot(Some(&declared));
        assert_eq!(out.keys().collect::<Vec<_>>(), ["kept", "static"]);
        assert_eq!(
            Store::open(Some(path))
                .snapshot(None)
                .keys()
                .collect::<Vec<_>>(),
            ["kept", "static"],
            "the prune reached the disk"
        );
    }

    // ── the cache, under a tempdir ──────────────────────────────────────

    fn every_kind() -> Vec<Setting> {
        vec![
            Setting::path("SCREENS", "Screens")
                .doc("d")
                .default_value("~/s.kdl"),
            Setting::directory("CACHE", "Cache"),
            Setting::text("SERVER", "Server"),
            Setting::bool("DEBUG", "Debug"),
            Setting::int("COLUMNS", "Columns", -2, 8),
            Setting::choice("THEME", "Theme", ["dark", "light"]),
        ]
    }

    #[test]
    fn a_recorded_declaration_survives_into_the_next_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("trollshell")
            .join("plugin-settings-schema.toml");

        let mut first = Store::open(Some(path.clone()));
        first.record("vibectl", every_kind());
        assert!(path.exists(), "the declaration is cached on disk");

        // A new session: nothing is live, the plugin has not connected yet,
        // and the form is still there — every kind round-tripped.
        let mut second = Store::open(Some(path));
        assert!(second.seen.is_empty());
        assert_eq!(second.snapshot(None)["vibectl"], every_kind());
    }

    #[test]
    fn a_plugin_that_stops_declaring_settings_loses_its_cached_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        store.record("vibectl", every_kind());
        store.record("vibectl", Vec::new());
        assert!(!store.snapshot(None).contains_key("vibectl"));
        assert!(!path.exists(), "an empty cache is deleted, not left behind");
        assert!(Store::open(Some(path)).snapshot(None).is_empty());
    }

    #[test]
    fn an_unchanged_declaration_does_not_rewrite_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        store.record("vibectl", every_kind());
        std::fs::write(&path, "sentinel = true\n").expect("overwrite");
        store.record("vibectl", every_kind());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "sentinel = true\n",
            "a reconnect with the same manifest wrote nothing"
        );
    }

    #[test]
    fn a_cache_edited_by_hand_goes_through_the_gate_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        store.record("vibectl", vec![Setting::text("SERVER", "Server")]);
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("\"SERVER\"", "\"LD_PRELOAD\"")).expect("tamper");
        assert!(
            Store::open(Some(path)).snapshot(None).is_empty(),
            "a refused name read back from the cache is dropped like one from a manifest"
        );
    }

    #[test]
    fn no_state_directory_still_serves_live_declarations() {
        let mut store = Store::open(None);
        store.record("vibectl", every_kind());
        assert_eq!(store.snapshot(None)["vibectl"], every_kind());
    }

    #[test]
    fn a_test_hosts_store_is_never_the_production_one() {
        // `plugins/tests.rs` drives sessions against stores like this one,
        // which `PluginsService::start` never publishes — so `register` from
        // those sessions records nothing and writes nothing.
        let store: PluginRuntimeStore = Arc::new(Mutex::new(BTreeMap::new()));
        assert!(!is_production(&store));
    }

    /// `register` from a host that is not the production one leaves the
    /// process-wide store unopened — which is also what keeps every session
    /// test off the developer's real `$XDG_STATE_HOME`: opening it is the only
    /// way anything here resolves that path.
    ///
    /// Red if `register` loses its production gate (it would spawn the record,
    /// open the store over the real cache path and persist into it). Run that
    /// mutation with `XDG_STATE_HOME` pointed at a scratch dir.
    #[test]
    fn register_from_a_test_host_leaves_the_real_store_alone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let store: PluginRuntimeStore = Arc::new(Mutex::new(BTreeMap::new()));
        runtime.block_on(async {
            register(&store, "vibectl", &[Setting::text("SERVER", "Server")]);
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        assert!(
            STORE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "a test host's registration opened the production store"
        );
    }
}
