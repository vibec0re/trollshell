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
//! stopped declaring any shows none.
//!
//! # Bounded by `plugins.json`
//!
//! An id is kept while `plugins.json` **declares** it or while it is
//! **connected**, and forgotten otherwise (#1415 review L3): a renamed instance
//! or a removed plugin does not keep a form. Only the declared ids are
//! **persisted**. A hand-installed static unit — which `plugins.json` never
//! declares, and which never reads `plugin-settings.toml` either — shows its
//! form while it is connected and has none once it disconnects. So the cache
//! is as large as the set of plugins actually in use, in memory and on disk,
//! and a loop of connections registering under made-up ids costs neither past
//! each connection's own life: no entry outlives it and no write is made for
//! it. Before this, every id that registered was kept for the rest of the
//! session and rewritten into the file with all the others, so such a loop
//! grew the file without limit and paid for it with a quadratic write volume.
//!
//! "Declared" is the launcher's own reading (`plugin_launcher::declared_ids`,
//! handed in by `main` through [`read_declared_with`]), taken each time
//! registrations are applied and each time the control-center asks.
//! "Connected" is the host's runtime mirror (`PLUGIN_RUNTIME`). When
//! `plugins.json` cannot be read, the last reading that could be read stands —
//! at startup, the ids the cache file already holds — so a broken file forgets
//! nothing that was declared.
//!
//! # Applied in order
//!
//! `register` only queues what a connection declared (the latest declaration
//! per id), and **one drain at a time** applies the queue. Each registration
//! used to be its own blocking task, so two registrations of one id — a
//! plugin reconnecting with an upgraded manifest — could take the store's lock
//! in either order and leave the older form standing.
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

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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
            Err(reason) => drops.note(&setting.env, &reason),
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
fn sanitize_one(setting: &Setting) -> Result<(Setting, usize), Cow<'static, str>> {
    // First, so an unreadable entry (which may have no `env` at all) is
    // reported for what it is: a newer kind, a malformed known one, or not a
    // setting (#1415 second review L6).
    if let SettingKind::Unknown(why) = &setting.kind {
        return Err(Cow::Owned(why.describe()));
    }
    if let Some(reason) = Setting::env_refusal(&setting.env) {
        return Err(Cow::Borrowed(reason));
    }
    let mut lost_options = 0;
    let kind = match &setting.kind {
        SettingKind::Int { min, max } if min > max => {
            return Err(Cow::Borrowed("its Int range is empty (min > max)"));
        }
        SettingKind::Choice { options } => {
            let (options, lost) = sanitize_options(options);
            if options.is_empty() {
                return Err(Cow::Borrowed("its Choice has no usable options"));
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
    /// The last list each kept id declared: every declared id's, and every
    /// connected one's (see the module doc). What the control-center is
    /// served.
    cached: Schemas,
    /// The ids `plugins.json` declared at the last reading that could be read,
    /// which are the ids that persist. Seeded with the ids the cache file
    /// holds, so until the first reading, what the last session kept is kept.
    declared: BTreeSet<String>,
    /// What the cache file holds, as last read or written: a persist that
    /// would write the same thing writes nothing.
    on_disk: Schemas,
}

impl Store {
    /// A store over the cache at `path`, reading what an earlier session left
    /// there. Every list is re-sanitised on the way in: the file is the
    /// shell's own, but a hand edit or a newer shell's write must not bypass
    /// the gate. A cache that does not parse is empty (the next registration
    /// rewrites it).
    fn open(path: Option<PathBuf>) -> Self {
        let cached: Schemas = path
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
            declared: cached.keys().cloned().collect(),
            on_disk: cached.clone(),
            cached,
        }
    }

    /// Apply a batch of registrations, each replacing what its id declared
    /// before (an empty list forgets the id); then forget every id that is
    /// neither declared nor `live`, and persist the declared ones if that
    /// changed them.
    ///
    /// `declared` is the ids `plugins.json` declares now, or `None` when it
    /// cannot be read — then the last reading stands. `live` is the ids
    /// connected now. A registration from an id that is neither is dropped
    /// with the rest: it has already disconnected, and nothing would read its
    /// form.
    fn apply(
        &mut self,
        batch: Schemas,
        declared: Option<&BTreeSet<String>>,
        live: &BTreeSet<String>,
    ) {
        if let Some(declared) = declared {
            self.declared.clone_from(declared);
        }
        for (id, list) in batch {
            if list.is_empty() {
                self.cached.remove(&id);
            } else {
                self.cached.insert(id, list);
            }
        }
        self.cached
            .retain(|id, _| self.declared.contains(id) || live.contains(id));
        self.persist();
    }

    /// Write the declared ids' lists to the cache file, or delete it when there
    /// are none — and do nothing when that is what the file already holds, so
    /// the common reconnect with an unchanged manifest, and any registration
    /// from an undeclared id, writes nothing. Best-effort: a failure costs the
    /// next session its forms for plugins that have not connected yet, is
    /// logged, and is retried by the next change.
    fn persist(&mut self) {
        let persisted: Schemas = self
            .cached
            .iter()
            .filter(|(id, _)| self.declared.contains(*id))
            .map(|(id, list)| (id.clone(), list.clone()))
            .collect();
        if persisted == self.on_disk {
            return;
        }
        let Some(path) = &self.path else {
            tracing::debug!("no state directory; plugin settings are not cached across sessions");
            return;
        };
        let result = if persisted.is_empty() {
            hytte_config::state::remove_at(path)
        } else {
            hytte_config::state::store_at(
                path,
                &Cache {
                    plugins: persisted.clone(),
                },
            )
        };
        match result {
            Ok(()) => self.on_disk = persisted,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not cache plugin settings");
            }
        }
    }

    /// Every remembered declaration, after forgetting the ids neither
    /// `declared` nor `live` (see [`Store::apply`], which this is with no
    /// registrations).
    fn snapshot(
        &mut self,
        declared: Option<&BTreeSet<String>>,
        live: &BTreeSet<String>,
    ) -> Schemas {
        self.apply(Schemas::new(), declared, live);
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

/// A reading of the ids `plugins.json` declares: `None` when the file exists
/// but cannot be read or parsed, `Some` (empty, if need be) otherwise.
pub type DeclaredReader = fn() -> Pin<Box<dyn Future<Output = Option<BTreeSet<String>>> + Send>>;

/// How [`drain`] reads `plugins.json`. The reading is the launcher's
/// (`plugin_launcher::declared_ids`), and the launcher is the binary's alone,
/// while this module is compiled into the library too — so `main` hands it in
/// ([`read_declared_with`]).
static DECLARED_READER: OnceLock<DeclaredReader> = OnceLock::new();

/// Tell the production host how to read the ids `plugins.json` declares,
/// which bound what this module remembers (see the module doc). `main` calls
/// it once, before the plugin host starts; a later call is ignored. Without
/// it, every application reads `plugins.json` as unreadable, so the last
/// reading — at startup, the cache file's own ids — stands.
pub fn read_declared_with(reader: DeclaredReader) {
    let _ = DECLARED_READER.set(reader);
}

/// Registrations the production store has not applied yet.
struct Queue {
    /// The latest declaration per id. A newer registration of an id replaces
    /// a queued older one, so the older can never be applied after it.
    pending: Schemas,
    /// Whether a [`drain`] task owns the queue. Read and written only under
    /// [`QUEUE`]'s lock, which is what makes "start a drain iff none is
    /// running" and "the drain stops when nothing is left" race-free against
    /// each other (`plugin_launcher`'s `WatchState::watching` shape).
    draining: bool,
}

impl Queue {
    const fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            draining: false,
        }
    }

    /// Queue `id`'s declaration, replacing one of its own still queued.
    /// Whether the caller must start a drain: true only when none owns the
    /// queue, and the caller's drain owns it from here.
    fn push(&mut self, id: String, list: Vec<Setting>) -> bool {
        self.pending.insert(id, list);
        !std::mem::replace(&mut self.draining, true)
    }

    /// Everything queued, for the drain to apply — or `None` when nothing is,
    /// and the drain gives the queue up in the same breath.
    fn next_batch(&mut self) -> Option<Schemas> {
        if self.pending.is_empty() {
            self.draining = false;
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());

fn queue() -> std::sync::MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hands the queue back when a [`drain`] ends however it ends, a panic
/// included — or the next registration would never start another.
struct Draining {
    /// Whether this drain still owns [`Queue::draining`]. The normal exit
    /// clears the flag under the lock and unsets this, so the guard cannot then
    /// clear a successor's flag.
    owns_flag: bool,
}

impl Drop for Draining {
    fn drop(&mut self) {
        if self.owns_flag {
            queue().draining = false;
        }
    }
}

/// Apply queued registrations until the queue is empty, one batch at a time —
/// the only thing that applies them, so they apply in the order they
/// registered.
///
/// Each batch takes a fresh reading of `plugins.json` (asynchronously, through
/// the launcher) and of the connected ids, then applies under the store's lock
/// on a blocking thread, because the first use opens the cache file and a
/// change writes it. A drain's reading and a `ListPluginSettings` one can
/// still apply in either order; the older standing until the next application
/// costs at most a form for an id that was neither connected nor in it.
async fn drain() {
    let mut guard = Draining { owns_flag: true };
    loop {
        let Some(batch) = queue().next_batch() else {
            guard.owns_flag = false;
            return;
        };
        let declared = match DECLARED_READER.get() {
            Some(read) => read().await,
            None => None,
        };
        let live = live_ids();
        let applied = tokio::task::spawn_blocking(move || {
            with_store(|store| store.apply(batch, declared.as_ref(), &live));
        })
        .await;
        if let Err(e) = applied {
            tracing::warn!(error = %e, "applying plugin settings declarations failed");
        }
    }
}

/// The ids connected to the production host right now.
fn live_ids() -> BTreeSet<String> {
    super::PLUGIN_RUNTIME
        .get()
        .map(|runtime| {
            runtime
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .keys()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `runtime` is the production host's store — the one
/// `PluginsService::start` published, as opposed to a test's.
fn is_production(runtime: &PluginRuntimeStore) -> bool {
    super::PLUGIN_RUNTIME
        .get()
        .is_some_and(|published| Arc::ptr_eq(published, runtime))
}

/// Sanitise and queue what a newly-registered connection declared. Called
/// from the session handshake right after `runtime_register`, with the same
/// instance id, so the id counts as connected from before its declaration is
/// queued until its teardown; one that applies after the connection is gone
/// is dropped unless `plugins.json` declares the id.
///
/// Sanitising (and its warnings) happens here, synchronously, for every host.
/// Recording does not: the declaration joins the queue and the one [`drain`]
/// applies it, so the connection's own task never waits on the disk or on
/// `plugins.json`, and two registrations of one id cannot apply out of order.
pub(super) fn register(runtime: &PluginRuntimeStore, id: &str, declared: &[Setting]) {
    let clean = sanitize(id, declared);
    if !is_production(runtime) {
        return;
    }
    let start = queue().push(id.to_owned(), clean);
    if start {
        tokio::spawn(drain());
    }
}

/// Every plugin's declared settings — what `Control.ListPluginSettings`
/// serves — after forgetting the ids that `declared` (the ids `plugins.json`
/// declares, `None` when it cannot be read) does not cover and that are not
/// connected. Blocking (the first call reads the cache file, and a prune
/// writes it), so the D-Bus handler runs it off its own task.
#[must_use]
pub fn snapshot(declared: Option<&BTreeSet<String>>) -> Schemas {
    let live = live_ids();
    with_store(|store| store.snapshot(declared, &live))
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
        let default = &sanitize(
            "p",
            &[Setting::text("A", "a").default_value("y".repeat(1000))],
        )[0];
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
        let decoded: hytte_plugin_proto::Manifest = hytte_plugin_proto::decode(&bytes)
            .expect("a newer kind does not cost the registration");
        assert_eq!(
            decoded.settings.len(),
            3,
            "the fixture's three entries, none dropped by the decoder"
        );
        let (captured, guard) = hytte_config::test_support::capture();
        let kept = sanitize("vibectl", &decoded.settings);
        drop(guard);
        assert_eq!(ids(&kept), ["V1BECTL_SERVER"]);
        // Each dropped entry is named, for what it is (#1415 second review
        // L6): the newer kind as newer, the env-less entry as unreadable.
        let events = captured.events();
        let reasons: Vec<String> = events
            .iter()
            .filter(|e| e.message == "plugin setting dropped")
            .map(|e| format!("{:?}", e.fields))
            .collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("newer than this shell"), "{reasons:?}");
        assert!(reasons[1].contains("no `env`"), "{reasons:?}");
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
            lines
                .iter()
                .filter(|l| l.contains("more of this plugin"))
                .count(),
            1,
            "{lines:?}"
        );
    }

    // ── what is remembered, and what is forgotten ───────────────────────

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|&id| id.to_owned()).collect()
    }

    /// One registration from `id`, which is connected while it applies, with
    /// `plugins.json` declaring `declared` and nothing else connected.
    fn record(store: &mut Store, id: &str, list: Vec<Setting>, declared: &[&str]) {
        store.apply(
            Schemas::from([(id.to_owned(), list)]),
            Some(&set(declared)),
            &set(&[id]),
        );
    }

    /// A registration replaces its id's list outright, and an empty one forgets
    /// the id — so an upgraded plugin shows its new form at once and one that
    /// stopped declaring settings shows none.
    #[test]
    fn a_registration_replaces_what_the_id_declared_before() {
        let mut store = Store::open(None);
        let declared = ["a", "b"];
        record(
            &mut store,
            "a",
            vec![Setting::text("A", "first")],
            &declared,
        );
        record(&mut store, "b", vec![Setting::text("B", "b")], &declared);
        record(
            &mut store,
            "a",
            vec![Setting::bool("A2", "second")],
            &declared,
        );
        record(&mut store, "b", Vec::new(), &declared);
        let out = store.snapshot(None, &set(&[]));
        assert_eq!(out.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(out["a"][0].label, "second");
    }

    /// #1415 review L3, bounded by `plugins.json`: a declared id is kept and
    /// persisted; an id `plugins.json` no longer declares is forgotten, on
    /// disk too; an undeclared id (a hand-installed static unit) is served
    /// while it is connected, never persisted, and forgotten once it is gone;
    /// and an unreadable `plugins.json` forgets nothing that was declared.
    #[test]
    fn an_id_neither_declared_nor_connected_is_forgotten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut first = Store::open(Some(path.clone()));
        let declared = ["kept", "renamed"];
        for id in declared {
            record(&mut first, id, vec![Setting::text("A", id)], &declared);
        }
        record(
            &mut first,
            "static",
            vec![Setting::text("A", "static")],
            &declared,
        );
        assert_eq!(
            first
                .snapshot(None, &set(&["static"]))
                .keys()
                .collect::<Vec<_>>(),
            ["kept", "renamed", "static"],
            "the static unit's form is served while it is connected"
        );

        let mut next = Store::open(Some(path.clone()));
        assert_eq!(
            next.snapshot(None, &set(&[])).keys().collect::<Vec<_>>(),
            ["kept", "renamed"],
            "only the declared ids were persisted, and an unreadable plugins.json forgets neither"
        );
        record(
            &mut next,
            "static",
            vec![Setting::text("A", "static")],
            &["kept"],
        );
        assert_eq!(
            next.snapshot(Some(&set(&["kept"])), &set(&["static"]))
                .keys()
                .collect::<Vec<_>>(),
            ["kept", "static"],
            "the renamed instance is forgotten; the connected static unit is not"
        );
        assert_eq!(
            next.snapshot(Some(&set(&["kept"])), &set(&[]))
                .keys()
                .collect::<Vec<_>>(),
            ["kept"],
            "the static unit's form goes when it disconnects"
        );
        assert_eq!(
            Store::open(Some(path))
                .snapshot(None, &set(&[]))
                .keys()
                .collect::<Vec<_>>(),
            ["kept"],
            "the prune reached the disk"
        );
    }

    /// A loop of connections registering under made-up ids — each connected
    /// while its own registration applies, gone by the next — leaves nothing
    /// behind in memory and makes no write at all.
    ///
    /// Red against the per-session `seen` set this replaced: every id is kept,
    /// and every one rewrites the whole cache file.
    #[test]
    fn a_stream_of_made_up_ids_costs_no_memory_and_no_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        record(&mut store, "vibectl", every_kind(), &["vibectl"]);
        std::fs::write(&path, "sentinel = true\n").expect("overwrite");

        let big: Vec<Setting> = (0..MAX_SETTINGS)
            .map(|i| Setting::text(format!("S{i}"), "x".repeat(MAX_LABEL_CHARS)))
            .collect();
        for i in 0..1000 {
            record(&mut store, &format!("a{i}"), big.clone(), &["vibectl"]);
            assert!(
                store.cached.len() <= 2,
                "at most the declared id and the connected one"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "sentinel = true\n",
            "no registration from an undeclared id wrote the cache"
        );
        assert_eq!(
            store
                .snapshot(Some(&set(&["vibectl"])), &set(&[]))
                .keys()
                .collect::<Vec<_>>(),
            ["vibectl"]
        );
    }

    /// An undeclared id that `plugins.json` then declares is persisted from the
    /// next reading on, if it is still connected — so a plugin moved from a
    /// static unit to the launcher keeps its form across the next restart.
    #[test]
    fn a_newly_declared_id_is_persisted_from_the_next_reading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        record(
            &mut store,
            "pet",
            vec![Setting::text("PET_NAME", "Name")],
            &[],
        );
        assert!(!path.exists(), "not persisted while undeclared");
        let _ = store.snapshot(Some(&set(&["pet"])), &set(&["pet"]));
        assert_eq!(
            Store::open(Some(path))
                .snapshot(None, &set(&[]))
                .keys()
                .collect::<Vec<_>>(),
            ["pet"]
        );
    }

    /// When `plugins.json` cannot be read, the last reading that could stands:
    /// what it declared is kept, and an undeclared id is still only kept while
    /// it is connected.
    #[test]
    fn an_unreadable_plugins_json_keeps_the_last_reading() {
        let mut store = Store::open(None);
        record(&mut store, "a", vec![Setting::text("A", "a")], &["a"]);
        store.apply(
            Schemas::from([("x".to_owned(), vec![Setting::text("X", "x")])]),
            None,
            &set(&["x"]),
        );
        assert_eq!(store.declared, set(&["a"]), "the last reading stands");
        assert_eq!(
            store.snapshot(None, &set(&[])).keys().collect::<Vec<_>>(),
            ["a"]
        );
    }

    /// Two registrations of one id apply in the order they registered: the
    /// queue keeps only the newer, and only the first `push` asks for a drain,
    /// so no second drain can apply the older one after it.
    ///
    /// Red against one blocking task per registration, which took the store's
    /// lock in whichever order the pool ran them.
    #[test]
    fn two_registrations_of_one_id_apply_in_order() {
        let mut queue = Queue::new();
        assert!(
            queue.push("a".into(), vec![Setting::text("A", "old")]),
            "the first registration starts the drain"
        );
        assert!(!queue.push("b".into(), vec![Setting::text("B", "b")]));
        assert!(
            !queue.push("a".into(), vec![Setting::text("A", "new")]),
            "while one drain owns the queue, nothing starts another"
        );
        let batch = queue.next_batch().expect("a batch");
        assert_eq!(batch.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(batch["a"][0].label, "new", "the older declaration is gone");

        let mut store = Store::open(None);
        store.apply(batch, Some(&set(&["a", "b"])), &set(&[]));
        assert_eq!(store.cached["a"][0].label, "new");

        assert!(queue.next_batch().is_none(), "drained");
        assert!(
            queue.push("a".into(), vec![Setting::text("A", "later")]),
            "an empty queue was given up, so the next registration starts a drain"
        );
    }

    /// A drain that dies hands the queue back, so the next registration starts
    /// another; one that returned normally has already handed it back, and its
    /// guard leaves a successor's flag alone.
    #[test]
    fn a_drain_always_hands_the_queue_back() {
        // Driven against the process-wide queue, which no test host ever
        // reaches (`register`'s production gate), so this is its only user.
        queue().draining = true;
        drop(Draining { owns_flag: true });
        assert!(!queue().draining, "a dead drain's guard released the queue");
        queue().draining = true;
        drop(Draining { owns_flag: false });
        assert!(queue().draining, "a finished drain's guard touches nothing");
        queue().draining = false;
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
        record(&mut first, "vibectl", every_kind(), &["vibectl"]);
        assert!(path.exists(), "the declaration is cached on disk");

        // A new session: the plugin has not connected yet, and the form is
        // still there — every kind round-tripped.
        let mut second = Store::open(Some(path));
        assert_eq!(
            second.snapshot(Some(&set(&["vibectl"])), &set(&[]))["vibectl"],
            every_kind()
        );
    }

    #[test]
    fn a_plugin_that_stops_declaring_settings_loses_its_cached_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        record(&mut store, "vibectl", every_kind(), &["vibectl"]);
        record(&mut store, "vibectl", Vec::new(), &["vibectl"]);
        assert!(!store.snapshot(None, &set(&[])).contains_key("vibectl"));
        assert!(!path.exists(), "an empty cache is deleted, not left behind");
        assert!(Store::open(Some(path)).snapshot(None, &set(&[])).is_empty());
    }

    #[test]
    fn an_unchanged_declaration_does_not_rewrite_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings-schema.toml");
        let mut store = Store::open(Some(path.clone()));
        record(&mut store, "vibectl", every_kind(), &["vibectl"]);
        std::fs::write(&path, "sentinel = true\n").expect("overwrite");
        record(&mut store, "vibectl", every_kind(), &["vibectl"]);
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
        record(
            &mut store,
            "vibectl",
            vec![Setting::text("SERVER", "Server")],
            &["vibectl"],
        );
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("\"SERVER\"", "\"LD_PRELOAD\"")).expect("tamper");
        assert!(
            Store::open(Some(path)).snapshot(None, &set(&[])).is_empty(),
            "a refused name read back from the cache is dropped like one from a manifest"
        );
    }

    #[test]
    fn no_state_directory_still_serves_live_declarations() {
        let mut store = Store::open(None);
        record(&mut store, "vibectl", every_kind(), &["vibectl"]);
        assert_eq!(store.snapshot(None, &set(&[]))["vibectl"], every_kind());
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
    /// process-wide queue empty and the store unopened — which is also what
    /// keeps every session test off the developer's real `$XDG_STATE_HOME`:
    /// opening the store is the only way anything here resolves that path.
    ///
    /// Red if `register` loses its production gate: it would queue the
    /// declaration and start a drain, which opens the store over the real
    /// cache path and persists into it. Run that mutation with
    /// `XDG_STATE_HOME` pointed at a scratch dir.
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
            queue().pending.is_empty(),
            "a test host's registration was queued for the production store"
        );
        assert!(
            STORE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "a test host's registration opened the production store"
        );
    }
}
