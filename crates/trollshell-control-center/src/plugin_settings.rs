//! The Plugins tab's **Settings** group (#1410): a form built from the
//! settings a plugin declares in its manifest, over
//! `$XDG_CONFIG_HOME/trollshell/plugin-settings.toml`.
//!
//! The shell sanitises what a plugin declared and serves it as
//! `Control.ListPluginSettings` (`id → JSON`, decoded by [`decode_schemas`]);
//! the tab mounts one [`SettingsForm`] for the selected plugin when it
//! declared anything, and none otherwise. So the form needs the shell once,
//! for the declaration. After that it keeps it: a poll that fails (a timeout,
//! the shell restarting) leaves the form, and any edit in it, where it is and
//! says the shell is not answering. The file itself is read and written here
//! directly, through the same `hytte_config::plugin_settings` writer the
//! shell's launcher reads with — the Places tab's #640 shape — so a Save works
//! while the shell is unreachable; only the restart that follows it needs the
//! shell.
//!
//! # One row per kind
//!
//! | kind | row | "unset" is |
//! |---|---|---|
//! | `Text` | an entry, the plugin's default as its placeholder | an empty entry |
//! | `Path` | the same, plus a *Choose…* button (a file or, for `directory`, a folder chooser) | an empty entry |
//! | `Bool` | a switch row, plus a reset button | the reset button's state |
//! | `Int` | a spin row over `min..=max`, plus a reset button | the reset button's state |
//! | `Choice` | a combo row whose first entry is *Default* | *Default* |
//!
//! Unset matters: it is what hands the variable back to the plugin's own
//! default, and a switch or a spin button always shows *some* value. So those
//! two carry a reset button, sensitive only while the row holds a value of its
//! own, and say in their subtitle when the plugin's default applies.
//!
//! A variable the plugin's nix `env` sets is shown read-only with its nix
//! value and the option that sets it — the #1400/#1331 greying — because the
//! launcher lets nix win and a value saved here would never arrive.
//!
//! # A value the row cannot show is left alone
//!
//! The file is hand-editable, and a plugin upgrade can rename an option or
//! narrow a range, so it can hold a value a row cannot represent: a `Choice`
//! value outside the options, an `Int` outside `min..=max` or not a whole
//! number, a boolean spelled `yes`. Such a value is **kept exactly as it is**
//! unless the user changes that row (#1415 review H1):
//!
//! - a `Choice` shows it as an extra item, "`value` (not an option)", which
//!   saves as itself;
//! - a switch or spin row shows the plugin's default and says in its subtitle
//!   what the file holds;
//! - and whatever the row shows, Save writes only the rows the user touched.
//!
//! # Save and Revert
//!
//! Nothing is written until **Save**, unlike `config_form`'s per-row
//! autosave: a plugin reads its environment once, at start, so every save
//! costs a restart, and a restart per keystroke would be worse than a button.
//! Save and Revert start disabled and light up only once a row differs from
//! the file. Save writes only the rows the user changed — never a nix-set
//! one, never a key the plugin does not declare, never an untouched row —
//! refuses a value no environment can carry
//! (`hytte_config::plugin_settings::value_refusal`), then hands the id to the
//! tab, which asks the shell to restart the plugin if it is running.
//! **Revert** re-reads the file. The file is not polled: a hand edit shows on
//! the next Revert or selection.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use hytte_config::plugin_settings::{self, Values};
use hytte_config::toml_edit;
use hytte_plugin_proto::manifest::{Setting, SettingKind};

/// Decode `ListPluginSettings`' reply into each plugin's settings.
///
/// Per **setting**, not per plugin: a setting this build cannot decode — a
/// kind a newer shell knows and this control-center does not — costs its own
/// row and nothing else, and a plugin whose whole list is unreadable simply
/// has no group. Logged at `debug`: the reply arrives every poll.
pub(crate) fn decode_schemas(reply: HashMap<String, String>) -> HashMap<String, Vec<Setting>> {
    reply
        .into_iter()
        .filter_map(|(id, json)| {
            let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else {
                tracing::debug!(plugin = %id, "ListPluginSettings: not a JSON array; no settings shown");
                return None;
            };
            let list: Vec<Setting> = items
                .into_iter()
                .filter_map(|item| match serde_json::from_value::<Setting>(item) {
                    Ok(setting) => Some(setting),
                    Err(err) => {
                        tracing::debug!(plugin = %id, %err, "ListPluginSettings: a setting this build cannot read; skipped");
                        None
                    }
                })
                .collect();
            (!list.is_empty()).then_some((id, list))
        })
        .collect()
}

/// Which chooser a `Path` row's button opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chooser {
    File,
    Folder,
}

/// The chooser a setting's kind asks for, `None` for a kind without one.
fn chooser_for(kind: &SettingKind) -> Option<Chooser> {
    match kind {
        SettingKind::Path { directory: true } => Some(Chooser::Folder),
        SettingKind::Path { directory: false } => Some(Chooser::File),
        _ => None,
    }
}

/// How a row edits its value.
enum Editor {
    /// `Text` and `Path`: empty is unset.
    Entry {
        entry: gtk::Entry,
        chooser: Option<Chooser>,
    },
    /// `Bool`. `own` is whether the row holds a value of its own.
    Switch {
        row: adw::SwitchRow,
        reset: gtk::Button,
        own: Cell<bool>,
        default: bool,
    },
    /// `Int`. `own` as for [`Editor::Switch`].
    Spin {
        row: adw::SpinRow,
        reset: gtk::Button,
        own: Cell<bool>,
        default: f64,
    },
    /// `Choice`: index 0 is *Default* (unset), `options[i]` is index `i + 1`,
    /// and a value outside the options, when the file holds one, is one more
    /// item after them (`foreign`).
    Combo {
        row: adw::ComboRow,
        model: gtk::StringList,
        options: Vec<String>,
        foreign: RefCell<Option<String>>,
    },
    /// Set in nix: shown, never written.
    Nix,
}

/// What a switch or spin row's subtitle says about its value.
#[derive(Clone, Copy)]
enum Held<'a> {
    /// The row holds a value of its own.
    Own,
    /// Unset: the plugin's default applies.
    Unset,
    /// The file holds this, which the row cannot show.
    Foreign(&'a str),
}

/// One setting's row.
struct Row {
    setting: Setting,
    /// The row as the group holds it.
    widget: adw::PreferencesRow,
    editor: Editor,
    /// Whether the user changed this row since the last load. Only a touched
    /// row is saved, and only a touched row can make the form dirty — so a
    /// value the row cannot show is never rewritten by a Save of another row.
    touched: Cell<bool>,
}

/// A row as a test sees it — which kind of row it is and what it offers.
#[cfg(all(test, feature = "system-tests"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowView {
    Entry {
        placeholder: Option<String>,
        chooser: Option<Chooser>,
    },
    Switch {
        subtitle: String,
    },
    Spin {
        min: i64,
        max: i64,
        subtitle: String,
    },
    Combo {
        items: Vec<String>,
    },
    Nix {
        subtitle: String,
        sensitive: bool,
    },
}

impl Row {
    /// The value this row would save: `None` for unset, and always `None` for
    /// a nix-set row, which [`SettingsForm::save`] never writes anyway.
    fn draft(&self) -> Option<toml_edit::Value> {
        match &self.editor {
            Editor::Entry { entry, .. } => {
                let text = entry.text();
                (!text.is_empty()).then(|| toml_edit::Value::from(text.as_str()))
            }
            Editor::Switch { row, own, .. } => own.get().then(|| row.is_active().into()),
            Editor::Spin { row, own, .. } => {
                // Whole numbers only: the adjustment's step is 1 and its
                // digits 0, so the value is integral and within `min..=max`.
                #[allow(clippy::cast_possible_truncation)]
                let value = row.value().round() as i64;
                own.get().then(|| value.into())
            }
            Editor::Combo {
                row,
                options,
                foreign,
                ..
            } => {
                let index = usize::try_from(row.selected()).ok()?.checked_sub(1)?;
                options
                    .get(index)
                    .cloned()
                    .or_else(|| (index == options.len()).then(|| foreign.borrow().clone())?)
                    .map(toml_edit::Value::from)
            }
            Editor::Nix => None,
        }
    }

    /// Whether this row would save what the file holds, `loaded`, compared
    /// the way the row reads it: a switch as a boolean it can show, a spin
    /// row as a number, anything else as the text the plugin receives.
    fn matches(&self, loaded: Option<&str>) -> bool {
        match (self.draft(), loaded) {
            (None, None) => true,
            (Some(draft), Some(loaded)) => match &self.editor {
                Editor::Switch { .. } => draft.as_bool().is_some() && draft.as_bool() == strict_bool(loaded),
                Editor::Spin { .. } => draft.as_integer() == loaded.trim().parse::<i64>().ok(),
                _ => edit_text(&draft) == loaded,
            },
            _ => false,
        }
    }

    /// Show `value` (the file's text for this variable, or `None`). A value
    /// the row cannot represent is shown as such (see the module doc) rather
    /// than coerced into one it can.
    fn show(&self, value: Option<&str>) {
        match &self.editor {
            Editor::Entry { entry, .. } => entry.set_text(value.unwrap_or_default()),
            Editor::Switch {
                row,
                reset,
                own,
                default,
            } => {
                let parsed = value.and_then(strict_bool);
                own.set(parsed.is_some());
                row.set_active(parsed.unwrap_or(*default));
                reset.set_sensitive(value.is_some());
                row.set_subtitle(&held_subtitle(&self.setting, held(parsed.is_some(), value)));
            }
            Editor::Spin {
                row,
                reset,
                own,
                default,
            } => {
                let adj = row.adjustment();
                #[allow(clippy::cast_precision_loss)]
                let parsed = value
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .filter(|v| (adj.lower()..=adj.upper()).contains(&(*v as f64)));
                own.set(parsed.is_some());
                #[allow(clippy::cast_precision_loss)]
                let shown = parsed.map_or(*default, |v| v as f64);
                row.set_value(shown);
                reset.set_sensitive(value.is_some());
                row.set_subtitle(&held_subtitle(&self.setting, held(parsed.is_some(), value)));
            }
            Editor::Combo {
                row,
                model,
                options,
                foreign,
            } => {
                let base = u32::try_from(options.len() + 1).unwrap_or(u32::MAX);
                if model.n_items() > base {
                    model.splice(base, model.n_items() - base, &[]);
                }
                let position = value.map(|v| (v, options.iter().position(|o| o == v)));
                let index = match position {
                    None => 0,
                    Some((_, Some(i))) => i + 1,
                    Some((v, None)) => {
                        model.append(&format!("{v} (not an option)"));
                        options.len() + 1
                    }
                };
                *foreign.borrow_mut() = value
                    .filter(|v| !options.iter().any(|o| o == v))
                    .map(str::to_owned);
                row.set_selected(u32::try_from(index).unwrap_or(0));
            }
            Editor::Nix => {}
        }
    }

    #[cfg(all(test, feature = "system-tests"))]
    fn view(&self) -> RowView {
        match &self.editor {
            Editor::Entry { entry, chooser } => RowView::Entry {
                placeholder: entry.placeholder_text().map(Into::into),
                chooser: *chooser,
            },
            Editor::Switch { row, .. } => RowView::Switch {
                subtitle: row.subtitle().map(Into::into).unwrap_or_default(),
            },
            Editor::Spin { row, .. } => {
                let adj = row.adjustment();
                #[allow(clippy::cast_possible_truncation)]
                let (min, max) = (adj.lower() as i64, adj.upper() as i64);
                RowView::Spin {
                    min,
                    max,
                    subtitle: row.subtitle().map(Into::into).unwrap_or_default(),
                }
            }
            Editor::Combo { model, .. } => RowView::Combo {
                items: (0..model.n_items())
                    .filter_map(|i| model.string(i).map(Into::into))
                    .collect(),
            },
            Editor::Nix => {
                let row = self
                    .widget
                    .downcast_ref::<adw::ActionRow>()
                    .expect("a nix row is an action row");
                RowView::Nix {
                    subtitle: row.subtitle().map(Into::into).unwrap_or_default(),
                    sensitive: row.is_sensitive(),
                }
            }
        }
    }
}

/// A switch's value from the file, **only** in a spelling this form writes
/// or plainly means the same: `true`/`false`/`1`/`0`, any case. Anything else
/// — `yes`, `on` — is `None`: the plugin's own parser may read it either way
/// (the pet reads only `1`/`true`), so the row does not guess and does not
/// rewrite it.
fn strict_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// What a switch or spin row holds, from whether it could read `value`.
fn held(readable: bool, value: Option<&str>) -> Held<'_> {
    match value {
        Some(_) if readable => Held::Own,
        Some(raw) => Held::Foreign(raw),
        None => Held::Unset,
    }
}

/// A switch or spin row's subtitle: the setting's doc, plus a line saying the
/// plugin's default applies while the row holds no value, or what the file
/// holds when the row cannot show it.
fn held_subtitle(setting: &Setting, held: Held<'_>) -> String {
    let note = match held {
        Held::Own => return setting.doc.clone(),
        Held::Unset => match &setting.default {
            Some(default) => format!("Not set: the plugin's default ({default}) applies."),
            None => "Not set: the plugin's default applies.".to_owned(),
        },
        Held::Foreign(raw) => format!(
            "The file holds \u{201c}{raw}\u{201d}, which this row cannot show; \
             it is kept unless you change the row."
        ),
    };
    if setting.doc.is_empty() {
        note
    } else {
        format!("{}\n{note}", setting.doc)
    }
}

/// The subtitle of a row nix sets.
fn nix_subtitle(id: &str, env: &str) -> String {
    format!("Set in nix — programs.trollshell.plugins.{id}.env.{env}")
}

/// A flat undo button that unsets a switch or spin row.
fn reset_widget() -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Unset, so the plugin's own default applies")
        .build();
    button.add_css_class("flat");
    button
}

struct Inner {
    id: String,
    path: Option<PathBuf>,
    rows: Vec<Row>,
    /// This plugin's table as the file held it at the last load.
    loaded: RefCell<Values>,
    save: gtk::Button,
    revert: gtk::Button,
    status: gtk::Label,
    /// Shown while the shell is not answering `ListPluginSettings`.
    offline: gtk::Label,
    /// Set while [`SettingsForm::load`] drives the widgets, so their change
    /// handlers do not read that as an edit.
    loading: Cell<bool>,
}

/// One plugin's mounted Settings group. Cheap to clone; every clone is the
/// same form.
#[derive(Clone)]
pub(crate) struct SettingsForm {
    group: adw::PreferencesGroup,
    inner: Rc<Inner>,
}

/// What the tab does after a successful save — restart the plugin if it is
/// running — told which plugin, and handed the form to report back on.
pub(crate) type OnSaved = Rc<dyn Fn(&str, &SettingsForm)>;

impl SettingsForm {
    /// The group for plugin `id`'s declared `schema`, over the settings file
    /// at `path` (`None` when there is no config directory: the form shows,
    /// and Save says why it cannot). `nix` is the plugin's nix `env`; a
    /// declared variable in it is read-only. A setting of a kind this build
    /// does not know gets no row.
    pub(crate) fn build(
        id: &str,
        schema: &[Setting],
        nix: &BTreeMap<String, String>,
        path: Option<PathBuf>,
        on_saved: OnSaved,
    ) -> Self {
        let group = adw::PreferencesGroup::builder()
            .title("Settings")
            .description(
                "Declared by the plugin, and saved to plugin-settings.toml in your trollshell \
                 config directory. A running plugin restarts to pick them up.",
            )
            .build();

        let revert = gtk::Button::builder()
            .label("Revert")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        let save = gtk::Button::builder()
            .label("Save")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        save.add_css_class("suggested-action");
        if let Some(path) = &path {
            save.set_tooltip_text(Some(&format!("Writes {}", path.display())));
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.append(&revert);
        buttons.append(&save);
        group.set_header_suffix(Some(&buttons));

        let rows: Vec<Row> = schema
            .iter()
            .filter_map(|setting| build_row(id, setting, nix.get(&setting.env)))
            .collect();
        for row in &rows {
            group.add(&row.widget);
        }

        // Below the rows: `AdwPreferencesGroup` renders a child that is not a
        // list row after its list, which is where these notes belong.
        let offline = note_label();
        offline.set_text(
            "The shell is not answering. This is the last form it sent; Save still \
             writes the file, and the plugin picks it up when it next starts.",
        );
        group.add(&offline);
        let status = note_label();
        group.add(&status);

        let form = Self {
            group,
            inner: Rc::new(Inner {
                id: id.to_owned(),
                path,
                rows,
                loaded: RefCell::new(Values::new()),
                save,
                revert,
                status,
                offline,
                loading: Cell::new(false),
            }),
        };
        form.connect(on_saved);
        form.load();
        form
    }

    /// The group, for the tab to add to and remove from its page.
    pub(crate) fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Re-read this plugin's table from the file and show it; the form is
    /// clean afterwards and no row counts as touched.
    pub(crate) fn load(&self) {
        let values = self
            .inner
            .path
            .as_deref()
            .map(plugin_settings::load_at)
            .and_then(|mut all| all.remove(&self.inner.id))
            .unwrap_or_default();
        self.inner.loading.set(true);
        for row in &self.inner.rows {
            row.show(values.get(&row.setting.env).map(String::as_str));
            row.touched.set(false);
        }
        self.inner.loading.set(false);
        *self.inner.loaded.borrow_mut() = values;
        self.refresh_buttons();
    }

    /// Whether any row the user touched differs from what the file held at
    /// the last load. An untouched row never counts, whatever it shows.
    pub(crate) fn is_dirty(&self) -> bool {
        let loaded = self.inner.loaded.borrow();
        self.inner
            .rows
            .iter()
            .filter(|row| row.touched.get() && !matches!(row.editor, Editor::Nix))
            .any(|row| !row.matches(loaded.get(&row.setting.env).map(String::as_str)))
    }

    /// Write the rows the user touched to the file, then re-read it.
    ///
    /// # Errors
    /// A value no environment can carry, or the writer's refusal, rendered for
    /// the status line; nothing is written.
    pub(crate) fn save(&self) -> Result<(), String> {
        let Some(path) = self.inner.path.as_deref() else {
            return Err(
                "Neither $XDG_CONFIG_HOME nor $HOME is set, so there is nowhere to save."
                    .to_owned(),
            );
        };
        let mut changes: Vec<(String, Option<toml_edit::Value>)> = Vec::new();
        for row in &self.inner.rows {
            if !row.touched.get() || matches!(row.editor, Editor::Nix) {
                continue;
            }
            let draft = row.draft();
            if let Some(reason) = draft
                .as_ref()
                .and_then(toml_edit::Value::as_str)
                .and_then(plugin_settings::value_refusal)
            {
                return Err(format!(
                    "{} was not saved: {reason}. Nothing was written.",
                    row.setting.label
                ));
            }
            changes.push((row.setting.env.clone(), draft));
        }
        plugin_settings::save_at(path, &self.inner.id, &changes).map_err(|e| e.to_string())?;
        self.load();
        Ok(())
    }

    /// Show `text` under the rows; `error` styles it as one. Empty hides it.
    pub(crate) fn set_status(&self, text: &str, error: bool) {
        let status = &self.inner.status;
        status.set_text(text);
        status.set_visible(!text.is_empty());
        if error {
            status.remove_css_class("dim-label");
            status.add_css_class("error");
        } else {
            status.remove_css_class("error");
            status.add_css_class("dim-label");
        }
    }

    /// Say, under the rows, whether the shell answered the last
    /// `ListPluginSettings` (#1415 review M1). The form itself stays as it is
    /// either way.
    pub(crate) fn set_shell_reachable(&self, reachable: bool) {
        self.inner.offline.set_visible(!reachable);
    }

    /// Each row's view, in order, keyed by variable.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn rows(&self) -> Vec<(String, RowView)> {
        self.inner
            .rows
            .iter()
            .map(|row| (row.setting.env.clone(), row.view()))
            .collect()
    }

    /// Put `text` into the entry row for `env`, as typing would.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn type_into(&self, env: &str, text: &str) {
        let row = self.row(env);
        let Editor::Entry { entry, .. } = &row.editor else {
            panic!("{env} is not an entry row");
        };
        entry.set_text(text);
    }

    /// Flip the switch row for `env`, as a click would.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn toggle(&self, env: &str) {
        let Editor::Switch { row, .. } = &self.row(env).editor else {
            panic!("{env} is not a switch row");
        };
        row.set_active(!row.is_active());
    }

    /// Select option `index` of the combo row for `env` (0 is *Default*).
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn select(&self, env: &str, index: u32) {
        let Editor::Combo { row, .. } = &self.row(env).editor else {
            panic!("{env} is not a combo row");
        };
        row.set_selected(index);
    }

    /// Press the reset button of the switch or spin row for `env`.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn reset(&self, env: &str) {
        match &self.row(env).editor {
            Editor::Switch { reset, .. } | Editor::Spin { reset, .. } => reset.emit_clicked(),
            _ => panic!("{env} has no reset button"),
        }
    }

    /// Whether Save and Revert are offered.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn buttons(&self) -> (bool, bool) {
        (
            self.inner.save.is_sensitive(),
            self.inner.revert.is_sensitive(),
        )
    }

    /// Press Save, as a click would.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn press_save(&self) {
        self.inner.save.emit_clicked();
    }

    /// The status line's text.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn status(&self) -> String {
        self.inner.status.text().into()
    }

    /// Whether the "shell is not answering" note is up.
    #[cfg(all(test, feature = "system-tests"))]
    pub(crate) fn says_offline(&self) -> bool {
        self.inner.offline.is_visible()
    }

    #[cfg(all(test, feature = "system-tests"))]
    fn row(&self, env: &str) -> &Row {
        self.inner
            .rows
            .iter()
            .find(|row| row.setting.env == env)
            .unwrap_or_else(|| panic!("no row for {env}"))
    }

    fn refresh_buttons(&self) {
        let dirty = self.is_dirty();
        self.inner.save.set_sensitive(dirty);
        self.inner.revert.set_sensitive(dirty);
    }

    /// Wire every row's change to the buttons, the two reset buttons to
    /// unset, the choosers, and Save/Revert. Handlers hold the form weakly:
    /// the rows are its own descendants.
    fn connect(&self, on_saved: OnSaved) {
        let weak = Rc::downgrade(&self.inner);
        let group = self.group.downgrade();
        let with_form = move |f: &dyn Fn(&SettingsForm)| {
            if let (Some(inner), Some(group)) = (weak.upgrade(), group.upgrade()) {
                f(&SettingsForm { group, inner });
            }
        };
        let with_form = Rc::new(with_form);

        for (index, row) in self.inner.rows.iter().enumerate() {
            let edited = {
                let with_form = with_form.clone();
                move || {
                    with_form(&|form: &SettingsForm| form.edited(index));
                }
            };
            match &row.editor {
                Editor::Entry { entry, chooser } => {
                    entry.connect_changed(move |_| edited());
                    if let Some(chooser) = *chooser {
                        connect_chooser(&row.widget, entry, chooser);
                    }
                }
                Editor::Switch {
                    row: switch, reset, ..
                } => {
                    switch.connect_active_notify(move |_| edited());
                    let with_form = with_form.clone();
                    reset.connect_clicked(move |_| {
                        with_form(&|form: &SettingsForm| form.unset(index));
                    });
                }
                Editor::Spin {
                    row: spin, reset, ..
                } => {
                    spin.connect_value_notify(move |_| edited());
                    let with_form = with_form.clone();
                    reset.connect_clicked(move |_| {
                        with_form(&|form: &SettingsForm| form.unset(index));
                    });
                }
                Editor::Combo { row: combo, .. } => {
                    combo.connect_selected_notify(move |_| edited());
                }
                Editor::Nix => {}
            }
        }

        {
            let with_form = with_form.clone();
            self.inner.revert.connect_clicked(move |_| {
                with_form(&|form: &SettingsForm| {
                    form.load();
                    form.set_status("", false);
                });
            });
        }
        self.inner.save.connect_clicked(move |_| {
            with_form(&|form: &SettingsForm| match form.save() {
                Ok(()) => {
                    form.set_status("Saved.", false);
                    on_saved(&form.inner.id, form);
                }
                Err(err) => form.set_status(&err, true),
            });
        });
    }

    /// Row `index` was edited by hand: it is touched, and a switch or spin
    /// row now holds a value of its own.
    fn edited(&self, index: usize) {
        if self.inner.loading.get() {
            return;
        }
        let row = &self.inner.rows[index];
        row.touched.set(true);
        match &row.editor {
            Editor::Switch {
                row: w, reset, own, ..
            } => {
                own.set(true);
                reset.set_sensitive(true);
                w.set_subtitle(&held_subtitle(&row.setting, Held::Own));
            }
            Editor::Spin {
                row: w, reset, own, ..
            } => {
                own.set(true);
                reset.set_sensitive(true);
                w.set_subtitle(&held_subtitle(&row.setting, Held::Own));
            }
            _ => {}
        }
        self.refresh_buttons();
    }

    /// Row `index`'s reset button: back to unset, showing the default. That
    /// is a change like any other, so the row is touched and a Save removes
    /// the key — the one way to clear a value the row cannot show.
    fn unset(&self, index: usize) {
        let row = &self.inner.rows[index];
        self.inner.loading.set(true);
        row.show(None);
        self.inner.loading.set(false);
        row.touched.set(true);
        self.refresh_buttons();
    }
}

/// A dim, wrapping line under the rows, hidden until it has something to say.
fn note_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .margin_top(6)
        .build();
    label.add_css_class("dim-label");
    label
}

/// The environment text a saved value becomes — what
/// `hytte_config::plugin_settings::scalar_text` makes of it on the way back
/// in, so a dirty check compares like with like.
fn edit_text(value: &toml_edit::Value) -> String {
    match value {
        toml_edit::Value::String(s) => s.value().clone(),
        toml_edit::Value::Integer(i) => i.value().to_string(),
        toml_edit::Value::Boolean(b) => b.value().to_string(),
        toml_edit::Value::Float(f) => f.value().to_string(),
        other => other.to_string().trim().to_owned(),
    }
}

/// Build one setting's row; `nix` is the value nix sets for it, if any.
/// `None` for a kind this build does not know, which gets no row.
fn build_row(id: &str, setting: &Setting, nix: Option<&String>) -> Option<Row> {
    let (widget, editor) = match (nix, &setting.kind) {
        (_, SettingKind::Unknown(_)) => return None,
        (Some(value), _) => (nix_row(id, setting, value), Editor::Nix),
        (None, SettingKind::Text | SettingKind::Path { .. }) => entry_row(setting),
        (None, SettingKind::Bool) => switch_row(setting),
        (None, SettingKind::Int { min, max }) => spin_row(setting, *min, *max),
        (None, SettingKind::Choice { options }) => choice_row(setting, options),
    };
    Some(Row {
        setting: setting.clone(),
        widget,
        editor,
        touched: Cell::new(false),
    })
}

/// Make `row` show its title and subtitle as plain text, **before** either is
/// set: `AdwPreferencesRow:use-markup` defaults to on, so a title set first
/// is parsed as Pango markup once (#1415 review L5) — a plugin-declared `&`
/// or `<` would log a warning each time the row is built.
fn plain_text(row: &impl IsA<adw::PreferencesRow>, title: &str) {
    row.set_use_markup(false);
    row.set_title(title);
}

/// A row nix sets: its value, greyed, and the option that sets it.
fn nix_row(id: &str, setting: &Setting, value: &str) -> adw::PreferencesRow {
    let row = adw::ActionRow::new();
    plain_text(&row, &setting.label);
    row.set_subtitle(&nix_subtitle(id, &setting.env));
    let shown = gtk::Label::builder()
        .label(value)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(28)
        .valign(gtk::Align::Center)
        .build();
    shown.add_css_class("dim-label");
    row.add_suffix(&shown);
    row.set_sensitive(false);
    row.upcast()
}

/// A `Text` or `Path` row: an entry with the default as its placeholder.
fn entry_row(setting: &Setting) -> (adw::PreferencesRow, Editor) {
    let row = adw::ActionRow::new();
    plain_text(&row, &setting.label);
    row.set_subtitle(&setting.doc);
    let entry = gtk::Entry::builder()
        .valign(gtk::Align::Center)
        .hexpand(true)
        .width_chars(18)
        .build();
    if let Some(default) = &setting.default {
        entry.set_placeholder_text(Some(default));
    }
    row.add_suffix(&entry);
    let chooser = chooser_for(&setting.kind);
    (row.upcast(), Editor::Entry { entry, chooser })
}

/// A `Bool` row: a switch, plus the reset that unsets it.
fn switch_row(setting: &Setting) -> (adw::PreferencesRow, Editor) {
    let row = adw::SwitchRow::new();
    plain_text(&row, &setting.label);
    let reset = reset_widget();
    row.add_suffix(&reset);
    let editor = Editor::Switch {
        row: row.clone(),
        reset,
        own: Cell::new(false),
        default: setting.default.as_deref().and_then(strict_bool).unwrap_or(false),
    };
    (row.upcast(), editor)
}

/// An `Int` row: a spin row over `min..=max`, plus the reset that unsets it.
/// Unset, it shows the plugin's default when that is a number, else the value
/// in range nearest zero.
fn spin_row(setting: &Setting, min: i64, max: i64) -> (adw::PreferencesRow, Editor) {
    #[allow(clippy::cast_precision_loss)]
    let (lo, hi) = (min as f64, max as f64);
    let row = adw::SpinRow::with_range(lo, hi, 1.0);
    plain_text(&row, &setting.label);
    row.set_digits(0);
    let reset = reset_widget();
    row.add_suffix(&reset);
    #[allow(clippy::cast_precision_loss)]
    let default = setting
        .default
        .as_deref()
        .and_then(|d| d.trim().parse::<i64>().ok())
        .map_or(0.0_f64.clamp(lo, hi), |d| d.clamp(min, max) as f64);
    let editor = Editor::Spin {
        row: row.clone(),
        reset,
        own: Cell::new(false),
        default,
    };
    (row.upcast(), editor)
}

/// A `Choice` row: *Default* first, then the options (and, while the file
/// holds a value outside them, that value — see [`Row::show`]).
fn choice_row(setting: &Setting, options: &[String]) -> (adw::PreferencesRow, Editor) {
    let first = match &setting.default {
        Some(default) => format!("Default ({default})"),
        None => "Default".to_owned(),
    };
    let model = gtk::StringList::new(&[first.as_str()]);
    for option in options {
        model.append(option);
    }
    let row = adw::ComboRow::new();
    plain_text(&row, &setting.label);
    row.set_subtitle(&setting.doc);
    row.set_model(Some(&model));
    let editor = Editor::Combo {
        row: row.clone(),
        model,
        options: options.to_vec(),
        foreign: RefCell::new(None),
    };
    (row.upcast(), editor)
}

/// Add the *Choose…* button to a `Path` row: a file chooser, or a folder
/// chooser when the setting asked for a directory, starting from whatever
/// the entry already holds. The pick goes into the entry, so it is an
/// ordinary edit that Save writes.
fn connect_chooser(widget: &adw::PreferencesRow, entry: &gtk::Entry, chooser: Chooser) {
    let (icon, tooltip) = match chooser {
        Chooser::File => ("document-open-symbolic", "Choose file…"),
        Chooser::Folder => ("folder-open-symbolic", "Choose folder…"),
    };
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    button.add_css_class("flat");
    if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
        row.add_suffix(&button);
    }
    let entry = entry.downgrade();
    button.connect_clicked(move |button| {
        let Some(entry) = entry.upgrade() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title(tooltip.trim_end_matches('…'))
            .modal(true)
            .build();
        let current = entry.text();
        if !current.is_empty() {
            let file = gio::File::for_path(current.as_str());
            match chooser {
                Chooser::File => dialog.set_initial_file(Some(&file)),
                Chooser::Folder => dialog.set_initial_folder(Some(&file)),
            }
        }
        let parent = button.root().and_downcast::<gtk::Window>();
        let target = entry.downgrade();
        let on_pick = move |result: Result<gio::File, glib::Error>| match result {
            Ok(file) => match (file.path(), target.upgrade()) {
                (Some(path), Some(entry)) => entry.set_text(&path.to_string_lossy()),
                (None, _) => tracing::warn!("the chosen file has no local path"),
                (_, None) => {}
            },
            Err(err) => tracing::debug!(%err, "chooser dismissed"),
        };
        match chooser {
            Chooser::File => dialog.open(parent.as_ref(), gio::Cancellable::NONE, on_pick),
            Chooser::Folder => {
                dialog.select_folder(parent.as_ref(), gio::Cancellable::NONE, on_pick);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_decode_per_setting() {
        let reply = HashMap::from([
            (
                "vibectl".to_owned(),
                r#"[{"env":"A","label":"A","doc":"","kind":{"Path":{"directory":true}}},
                    {"env":"B","label":"B","doc":"","kind":"FromTheFuture"},
                    {"env":"C","label":"C","doc":"","kind":"Text","default":"c"}]"#
                    .to_owned(),
            ),
            ("broken".to_owned(), "{not json".to_owned()),
            (
                "unreadable".to_owned(),
                r#"[{"env":"X","label":"X","doc":"","kind":"Nope"}]"#.to_owned(),
            ),
        ]);
        let schemas = decode_schemas(reply);
        assert_eq!(schemas.keys().collect::<Vec<_>>(), ["vibectl"]);
        assert_eq!(
            schemas["vibectl"],
            [
                Setting::directory("A", "A"),
                Setting::text("C", "C").default_value("c"),
            ]
        );
    }

    #[test]
    fn the_path_kind_picks_its_chooser() {
        assert_eq!(
            chooser_for(&SettingKind::Path { directory: true }),
            Some(Chooser::Folder)
        );
        assert_eq!(
            chooser_for(&SettingKind::Path { directory: false }),
            Some(Chooser::File)
        );
        assert_eq!(chooser_for(&SettingKind::Text), None);
    }

    /// #1415 review H1: a switch reads only the spellings it would write back
    /// meaning the same thing. `yes`/`on` are not guessed at — the pet reads
    /// only `1`/`true`, so showing `yes` as on and saving `true` would change
    /// what the plugin does.
    #[test]
    fn a_switch_reads_only_what_it_would_write() {
        for on in ["true", "TRUE", "1", " true "] {
            assert_eq!(strict_bool(on), Some(true), "{on}");
        }
        for off in ["false", "False", "0"] {
            assert_eq!(strict_bool(off), Some(false), "{off}");
        }
        for foreign in ["yes", "On", "no", "off", "", "maybe"] {
            assert_eq!(strict_bool(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn a_draft_compares_as_the_text_the_plugin_receives() {
        assert_eq!(edit_text(&toml_edit::Value::from("a b")), "a b");
        assert_eq!(edit_text(&toml_edit::Value::from(true)), "true");
        assert_eq!(edit_text(&toml_edit::Value::from(-4_i64)), "-4");
    }

    #[test]
    fn the_unset_subtitle_names_the_default() {
        let s = Setting::bool("A", "A")
            .doc("Does a thing.")
            .default_value("true");
        assert_eq!(held_subtitle(&s, Held::Own), "Does a thing.");
        assert_eq!(
            held_subtitle(&s, Held::Unset),
            "Does a thing.\nNot set: the plugin's default (true) applies."
        );
        assert_eq!(
            held_subtitle(&Setting::int("N", "N", 0, 9), Held::Unset),
            "Not set: the plugin's default applies."
        );
        assert_eq!(
            held_subtitle(&Setting::int("N", "N", 0, 9), Held::Foreign("12")),
            "The file holds \u{201c}12\u{201d}, which this row cannot show; it is kept unless \
             you change the row."
        );
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::*;

    fn every_kind() -> Vec<Setting> {
        vec![
            Setting::path("V1BECTL_SCREENS", "Screens layout file")
                .doc("A screens.kdl.")
                .default_value("~/.config/v1bectl/screens.kdl"),
            Setting::directory("V1BECTL_CACHE", "Cache folder"),
            Setting::text("V1BECTL_SERVER", "Server address"),
            Setting::bool("V1BECTL_DEBUG", "Debug overlay"),
            Setting::int("V1BECTL_COLUMNS", "Columns", 1, 8).default_value("3"),
            Setting::choice("V1BECTL_THEME", "Theme", ["dark", "light"]).default_value("dark"),
        ]
    }

    fn no_op() -> OnSaved {
        Rc::new(|_: &str, _: &SettingsForm| {})
    }

    fn form_at(path: Option<PathBuf>, nix: &[(&str, &str)], on_saved: OnSaved) -> SettingsForm {
        let nix: BTreeMap<String, String> = nix
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        SettingsForm::build("vibectl", &every_kind(), &nix, path, on_saved)
    }

    #[gtk::test]
    fn every_kind_gets_its_row_and_nix_set_ones_are_read_only() {
        adw::init().expect("libadwaita init");
        let form = form_at(None, &[("V1BECTL_SERVER", "host:1")], no_op());
        assert_eq!(
            form.rows(),
            vec![
                (
                    "V1BECTL_SCREENS".to_owned(),
                    RowView::Entry {
                        placeholder: Some("~/.config/v1bectl/screens.kdl".to_owned()),
                        chooser: Some(Chooser::File),
                    }
                ),
                (
                    "V1BECTL_CACHE".to_owned(),
                    RowView::Entry {
                        placeholder: None,
                        chooser: Some(Chooser::Folder),
                    }
                ),
                (
                    "V1BECTL_SERVER".to_owned(),
                    RowView::Nix {
                        subtitle:
                            "Set in nix — programs.trollshell.plugins.vibectl.env.V1BECTL_SERVER"
                                .to_owned(),
                        sensitive: false,
                    }
                ),
                (
                    "V1BECTL_DEBUG".to_owned(),
                    RowView::Switch {
                        subtitle: "Not set: the plugin's default applies.".to_owned()
                    }
                ),
                (
                    "V1BECTL_COLUMNS".to_owned(),
                    RowView::Spin {
                        min: 1,
                        max: 8,
                        subtitle: "Not set: the plugin's default (3) applies.".to_owned()
                    }
                ),
                (
                    "V1BECTL_THEME".to_owned(),
                    RowView::Combo {
                        items: vec!["Default (dark)".into(), "dark".into(), "light".into()],
                    }
                ),
            ]
        );
        assert_eq!(form.buttons(), (false, false), "a fresh form is clean");
    }

    #[gtk::test]
    fn save_writes_only_the_forms_own_rows_and_keeps_the_rest_of_the_file() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trollshell").join("plugin-settings.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let before = "\
# kept
[pet]
PET_NAME = 'nisse'

[vibectl]
HAND_ADDED = \"x\" # not declared, not touched
V1BECTL_SERVER = \"stale\"
";
        std::fs::write(&path, before).expect("seed");

        let saved = Rc::new(RefCell::new(Vec::<String>::new()));
        let on_saved: OnSaved = {
            let saved = saved.clone();
            Rc::new(move |id: &str, _: &SettingsForm| saved.borrow_mut().push(id.to_owned()))
        };
        let form = form_at(
            Some(path.clone()),
            &[("V1BECTL_SERVER", "host:1")],
            on_saved,
        );
        assert_eq!(form.buttons(), (false, false));

        form.type_into("V1BECTL_SCREENS", "/home/u/screens.kdl");
        form.toggle("V1BECTL_DEBUG");
        form.select("V1BECTL_THEME", 2);
        assert_eq!(form.buttons(), (true, true), "edits make it dirty");
        form.press_save();

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "\
# kept
[pet]
PET_NAME = 'nisse'

[vibectl]
HAND_ADDED = \"x\" # not declared, not touched
V1BECTL_SERVER = \"stale\"
V1BECTL_SCREENS = \"/home/u/screens.kdl\"
V1BECTL_DEBUG = true
V1BECTL_THEME = \"light\"
",
            "the nix-set row and the undeclared key are untouched; unset rows write nothing"
        );
        assert_eq!(*saved.borrow(), ["vibectl"], "the tab is told, once");
        assert_eq!(form.buttons(), (false, false), "clean after the save");
        assert_eq!(form.status(), "Saved.");

        // Clearing an entry and choosing Default unset them again.
        form.type_into("V1BECTL_SCREENS", "");
        form.select("V1BECTL_THEME", 0);
        form.press_save();
        let after = std::fs::read_to_string(&path).expect("read");
        assert!(!after.contains("V1BECTL_SCREENS"), "{after}");
        assert!(!after.contains("V1BECTL_THEME"), "{after}");
        assert!(after.contains("V1BECTL_DEBUG = true"), "{after}");
    }

    #[gtk::test]
    fn revert_rereads_the_file() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(&path, "[vibectl]\nV1BECTL_COLUMNS = 5\n").expect("seed");
        let form = form_at(Some(path.clone()), &[], no_op());
        assert_eq!(
            form.buttons(),
            (false, false),
            "a loaded Int is not an edit"
        );
        form.type_into("V1BECTL_SERVER", "typed, never saved");
        assert!(form.is_dirty());
        form.load();
        assert!(!form.is_dirty());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "[vibectl]\nV1BECTL_COLUMNS = 5\n"
        );
    }

    #[gtk::test]
    fn a_broken_file_is_reported_and_left_alone() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(&path, "[vibectl\n").expect("seed");
        let saved = Rc::new(Cell::new(false));
        let on_saved: OnSaved = {
            let saved = saved.clone();
            Rc::new(move |_: &str, _: &SettingsForm| saved.set(true))
        };
        let form = form_at(Some(path.clone()), &[], on_saved);
        form.type_into("V1BECTL_SERVER", "x");
        form.press_save();
        assert!(
            form.status().contains("does not parse"),
            "{}",
            form.status()
        );
        assert!(!saved.get(), "no restart after a refused save");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "[vibectl\n");
    }

    /// #1415 review H1 (the reviewer's killing test, adapted): values a row
    /// cannot show — a `Choice` value outside the options, an `Int` outside
    /// `min..=max`, a boolean spelled `yes` — open the form **clean** and
    /// survive a Save of an unrelated row byte for byte.
    ///
    /// Red before the fix: the form opened dirty, and the save deleted the
    /// theme, clamped the columns to 8 and rewrote `yes` as `true`.
    #[gtk::test]
    fn unrepresentable_values_survive_an_unrelated_save() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        let before = "[vibectl]\nV1BECTL_THEME = \"solarized\"\nV1BECTL_COLUMNS = 12\nV1BECTL_DEBUG = \"yes\"\n";
        std::fs::write(&path, before).expect("seed");
        let form = form_at(Some(path.clone()), &[], no_op());
        assert_eq!(form.buttons(), (false, false), "the form opens dirty");
        form.type_into("V1BECTL_SERVER", "h:1");
        assert_eq!(form.buttons(), (true, true));
        form.press_save();
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{before}V1BECTL_SERVER = \"h:1\"\n"),
            "only the touched row was written"
        );
    }

    /// The same values are **shown** for what they are, not coerced: the
    /// combo carries the foreign value as its own item and has it selected,
    /// and the switch and spin rows say what the file holds.
    #[gtk::test]
    fn a_value_the_row_cannot_show_is_shown_as_such() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(
            &path,
            "[vibectl]\nV1BECTL_THEME = \"solarized\"\nV1BECTL_COLUMNS = 3.5\nV1BECTL_DEBUG = \"yes\"\n",
        )
        .expect("seed");
        let form = form_at(Some(path), &[], no_op());
        let rows: HashMap<String, RowView> = form.rows().into_iter().collect();
        assert_eq!(
            rows["V1BECTL_THEME"],
            RowView::Combo {
                items: vec![
                    "Default (dark)".into(),
                    "dark".into(),
                    "light".into(),
                    "solarized (not an option)".into()
                ]
            }
        );
        let RowView::Spin { subtitle, .. } = &rows["V1BECTL_COLUMNS"] else {
            panic!("a spin row");
        };
        assert!(subtitle.contains("\u{201c}3.5\u{201d}"), "{subtitle}");
        let RowView::Switch { subtitle } = &rows["V1BECTL_DEBUG"] else {
            panic!("a switch row");
        };
        assert!(subtitle.contains("\u{201c}yes\u{201d}"), "{subtitle}");

        // Picking another option and then the foreign one again is no
        // change at all.
        form.select("V1BECTL_THEME", 1);
        assert!(form.is_dirty());
        form.select("V1BECTL_THEME", 3);
        assert!(!form.is_dirty(), "the foreign item saves as itself");
    }

    /// The reset button is how a value the row cannot show is cleared: it
    /// touches the row, and Save removes the key.
    #[gtk::test]
    fn reset_removes_a_value_the_row_cannot_show() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(&path, "[vibectl]\nV1BECTL_COLUMNS = 12\nOTHER = \"x\"\n").expect("seed");
        let form = form_at(Some(path.clone()), &[], no_op());
        form.reset("V1BECTL_COLUMNS");
        assert_eq!(form.buttons(), (true, true));
        form.press_save();
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "[vibectl]\nOTHER = \"x\"\n"
        );
    }

    /// #1415 review M3, on this side: a value no environment can carry is
    /// refused before anything is written, naming the row.
    #[gtk::test]
    fn a_value_no_environment_can_carry_is_refused_before_writing() {
        adw::init().expect("libadwaita init");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        let saved = Rc::new(Cell::new(false));
        let on_saved: OnSaved = {
            let saved = saved.clone();
            Rc::new(move |_: &str, _: &SettingsForm| saved.set(true))
        };
        let form = form_at(Some(path.clone()), &[], on_saved);
        form.type_into("V1BECTL_SCREENS", "/ok");
        form.type_into(
            "V1BECTL_SERVER",
            &"x".repeat(plugin_settings::MAX_VALUE_BYTES + 1),
        );
        form.press_save();
        assert!(
            form.status().starts_with("Server address was not saved"),
            "{}",
            form.status()
        );
        assert!(!saved.get(), "no restart after a refused save");
        assert!(!path.exists(), "nothing was written, not even the good row");
    }

    /// #1415 review M1, the form's half: the "shell is not answering" note
    /// comes and goes without touching the rows.
    #[gtk::test]
    fn the_offline_note_comes_and_goes() {
        adw::init().expect("libadwaita init");
        let form = form_at(None, &[], no_op());
        assert!(!form.says_offline());
        form.type_into("V1BECTL_SERVER", "typed");
        form.set_shell_reachable(false);
        assert!(form.says_offline());
        assert!(form.is_dirty(), "the edit is untouched");
        form.set_shell_reachable(true);
        assert!(!form.says_offline());
    }
}
