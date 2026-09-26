//! The Plugins tab's **Settings** group (#1410): a form built from the
//! settings a plugin declares in its manifest, over
//! `$XDG_CONFIG_HOME/trollshell/plugin-settings.toml`.
//!
//! The shell sanitises what a plugin declared and serves it as
//! `Control.ListPluginSettings` (`id → JSON`, decoded by [`decode_schemas`]);
//! the tab mounts one [`SettingsForm`] for the selected plugin when it
//! declared anything, and none otherwise. The file itself is read and written
//! here directly, through the same `hytte_config::plugin_settings` writer the
//! shell's launcher reads with — the Places tab's #640 shape — so the form
//! works while the shell is down and a save never goes through the bus.
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
//! # Save and Revert
//!
//! Nothing is written until **Save**, unlike `config_form`'s per-row
//! autosave: a plugin reads its environment once, at start, so every save
//! costs a restart, and a restart per keystroke would be worse than a button.
//! Save writes only the rows it owns (never a nix-set one, never a key the
//! plugin does not declare), then hands the id to the tab, which restarts the
//! plugin if it is running. **Revert** re-reads the file. The file is not
//! polled: a hand edit shows on the next Revert or selection.

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
        own: Rc<Cell<bool>>,
        default: bool,
    },
    /// `Int`. `own` as for [`Editor::Switch`].
    Spin {
        row: adw::SpinRow,
        reset: gtk::Button,
        own: Rc<Cell<bool>>,
        default: f64,
    },
    /// `Choice`: index 0 is *Default* (unset), `options[i]` is index `i + 1`.
    Combo {
        row: adw::ComboRow,
        options: Vec<String>,
    },
    /// Set in nix: shown, never written.
    Nix,
}

/// One setting's row.
struct Row {
    setting: Setting,
    /// The row as the group holds it.
    widget: adw::PreferencesRow,
    editor: Editor,
}

/// A row as a test sees it — which kind of row it is and what it offers.
#[cfg(all(test, feature = "system-tests"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowView {
    Entry {
        placeholder: Option<String>,
        chooser: Option<Chooser>,
    },
    Switch,
    Spin {
        min: i64,
        max: i64,
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
            Editor::Combo { row, options } => {
                let index = usize::try_from(row.selected()).ok()?;
                index
                    .checked_sub(1)
                    .and_then(|i| options.get(i))
                    .map(|s| toml_edit::Value::from(s.as_str()))
            }
            Editor::Nix => None,
        }
    }

    /// Whether this row would save what the file holds, `loaded`, compared
    /// the way the row reads it: a switch as a boolean (a hand-written `yes`
    /// is the same as a saved `true`), a spin row as a number, anything else
    /// as the text the plugin receives.
    fn matches(&self, loaded: Option<&str>) -> bool {
        match (self.draft(), loaded) {
            (None, None) => true,
            (Some(draft), Some(loaded)) => match &self.editor {
                Editor::Switch { .. } => draft.as_bool() == Some(parse_bool(loaded)),
                Editor::Spin { .. } => draft.as_integer() == loaded.trim().parse::<i64>().ok(),
                _ => edit_text(&draft) == loaded,
            },
            _ => false,
        }
    }

    /// Show `value` (the file's text for this variable, or `None`).
    fn show(&self, value: Option<&str>) {
        match &self.editor {
            Editor::Entry { entry, .. } => entry.set_text(value.unwrap_or_default()),
            Editor::Switch {
                row,
                reset,
                own,
                default,
            } => {
                own.set(value.is_some());
                row.set_active(value.map_or(*default, parse_bool));
                reset.set_sensitive(own.get());
                row.set_subtitle(&own_subtitle(&self.setting, own.get()));
            }
            Editor::Spin {
                row,
                reset,
                own,
                default,
            } => {
                let parsed = value.and_then(|v| v.trim().parse::<i64>().ok());
                own.set(parsed.is_some());
                #[allow(clippy::cast_precision_loss)]
                let shown = parsed.map_or(*default, |v| v as f64);
                row.set_value(shown);
                reset.set_sensitive(own.get());
                row.set_subtitle(&own_subtitle(&self.setting, own.get()));
            }
            Editor::Combo { row, options } => {
                let index = value
                    .and_then(|v| options.iter().position(|o| o == v))
                    .map_or(0, |i| i + 1);
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
            Editor::Switch { .. } => RowView::Switch,
            Editor::Spin { row, .. } => {
                let adj = row.adjustment();
                #[allow(clippy::cast_possible_truncation)]
                let (min, max) = (adj.lower() as i64, adj.upper() as i64);
                RowView::Spin { min, max }
            }
            Editor::Combo { row, .. } => RowView::Combo {
                items: row
                    .model()
                    .and_downcast::<gtk::StringList>()
                    .map(|m| {
                        (0..m.n_items())
                            .filter_map(|i| m.string(i).map(Into::into))
                            .collect()
                    })
                    .unwrap_or_default(),
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

/// `"true"`, `"1"`, `"yes"` or `"on"`, any case, is on; anything else is off.
fn parse_bool(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// A switch or spin row's subtitle: the setting's doc, plus a line saying the
/// plugin's default applies while the row holds no value of its own.
fn own_subtitle(setting: &Setting, own: bool) -> String {
    if own {
        return setting.doc.clone();
    }
    let unset = match &setting.default {
        Some(default) => format!("Not set: the plugin's default ({default}) applies."),
        None => "Not set: the plugin's default applies.".to_owned(),
    };
    if setting.doc.is_empty() {
        unset
    } else {
        format!("{}\n{unset}", setting.doc)
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
    /// declared variable in it is read-only.
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
                "Declared by the plugin, saved to ~/.config/trollshell/plugin-settings.toml. \
                 A running plugin restarts to pick them up.",
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
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.append(&revert);
        buttons.append(&save);
        group.set_header_suffix(Some(&buttons));

        let rows: Vec<Row> = schema
            .iter()
            .map(|setting| build_row(id, setting, nix.get(&setting.env)))
            .collect();
        for row in &rows {
            group.add(&row.widget);
        }

        // Below the rows: `AdwPreferencesGroup` renders a child that is not a
        // list row after its list, which is where a save's outcome belongs.
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .margin_top(6)
            .build();
        status.add_css_class("dim-label");
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
    /// clean afterwards.
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
        }
        self.inner.loading.set(false);
        *self.inner.loaded.borrow_mut() = values;
        self.refresh_buttons();
    }

    /// Whether any row's value differs from what the file held at the last
    /// load.
    pub(crate) fn is_dirty(&self) -> bool {
        let loaded = self.inner.loaded.borrow();
        self.inner
            .rows
            .iter()
            .filter(|row| !matches!(row.editor, Editor::Nix))
            .any(|row| !row.matches(loaded.get(&row.setting.env).map(String::as_str)))
    }

    /// Write every row this form owns to the file, then re-read it.
    ///
    /// # Errors
    /// The writer's refusal, rendered for the status line; nothing is written.
    pub(crate) fn save(&self) -> Result<(), String> {
        let Some(path) = self.inner.path.as_deref() else {
            return Err(
                "Neither $XDG_CONFIG_HOME nor $HOME is set, so there is nowhere to save.".to_owned(),
            );
        };
        let changes: Vec<(String, Option<toml_edit::Value>)> = self
            .inner
            .rows
            .iter()
            .filter(|row| !matches!(row.editor, Editor::Nix))
            .map(|row| (row.setting.env.clone(), row.draft()))
            .collect();
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
                Editor::Switch { row: switch, reset, .. } => {
                    switch.connect_active_notify(move |_| edited());
                    let with_form = with_form.clone();
                    reset.connect_clicked(move |_| {
                        with_form(&|form: &SettingsForm| form.unset(index));
                    });
                }
                Editor::Spin { row: spin, reset, .. } => {
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

    /// Row `index` was edited by hand: a switch or spin row now holds a value
    /// of its own.
    fn edited(&self, index: usize) {
        if self.inner.loading.get() {
            return;
        }
        let row = &self.inner.rows[index];
        match &row.editor {
            Editor::Switch { row: w, reset, own, .. } => {
                own.set(true);
                reset.set_sensitive(true);
                w.set_subtitle(&own_subtitle(&row.setting, true));
            }
            Editor::Spin { row: w, reset, own, .. } => {
                own.set(true);
                reset.set_sensitive(true);
                w.set_subtitle(&own_subtitle(&row.setting, true));
            }
            _ => {}
        }
        self.refresh_buttons();
    }

    /// Row `index`'s reset button: back to unset, showing the default.
    fn unset(&self, index: usize) {
        self.inner.loading.set(true);
        self.inner.rows[index].show(None);
        self.inner.loading.set(false);
        self.refresh_buttons();
    }
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
fn build_row(id: &str, setting: &Setting, nix: Option<&String>) -> Row {
    let (widget, editor) = match (nix, &setting.kind) {
        (Some(value), _) => (nix_row(id, setting, value), Editor::Nix),
        (None, SettingKind::Text | SettingKind::Path { .. }) => entry_row(setting),
        (None, SettingKind::Bool) => switch_row(setting),
        (None, SettingKind::Int { min, max }) => spin_row(setting, *min, *max),
        (None, SettingKind::Choice { options }) => choice_row(setting, options),
    };
    Row {
        setting: setting.clone(),
        widget,
        editor,
    }
}

/// A row nix sets: its value, greyed, and the option that sets it.
fn nix_row(id: &str, setting: &Setting, value: &str) -> adw::PreferencesRow {
    let row = adw::ActionRow::builder()
        .title(setting.label.as_str())
        .use_markup(false)
        .subtitle(nix_subtitle(id, &setting.env))
        .build();
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
    let row = adw::ActionRow::builder()
        .title(setting.label.as_str())
        .use_markup(false)
        .subtitle(setting.doc.as_str())
        .build();
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
    let row = adw::SwitchRow::builder()
        .title(setting.label.as_str())
        .use_markup(false)
        .build();
    let reset = reset_widget();
    row.add_suffix(&reset);
    let editor = Editor::Switch {
        row: row.clone(),
        reset,
        own: Rc::new(Cell::new(false)),
        default: setting.default.as_deref().is_some_and(parse_bool),
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
    row.set_title(&setting.label);
    row.set_use_markup(false);
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
        own: Rc::new(Cell::new(false)),
        default,
    };
    (row.upcast(), editor)
}

/// A `Choice` row: *Default* first, then the options.
fn choice_row(setting: &Setting, options: &[String]) -> (adw::PreferencesRow, Editor) {
    let first = match &setting.default {
        Some(default) => format!("Default ({default})"),
        None => "Default".to_owned(),
    };
    let mut items: Vec<&str> = vec![first.as_str()];
    items.extend(options.iter().map(String::as_str));
    let (row, _model) = crate::config_form::combo_row(&setting.label, &items, None);
    row.set_use_markup(false);
    row.set_subtitle(&setting.doc);
    let editor = Editor::Combo {
        row: row.clone(),
        options: options.to_vec(),
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

    #[test]
    fn booleans_read_the_way_people_write_them() {
        for on in ["true", "TRUE", "1", "yes", "On", " true "] {
            assert!(parse_bool(on), "{on}");
        }
        for off in ["false", "0", "no", "off", "", "maybe"] {
            assert!(!parse_bool(off), "{off}");
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
        let s = Setting::bool("A", "A").doc("Does a thing.").default_value("true");
        assert_eq!(own_subtitle(&s, true), "Does a thing.");
        assert_eq!(
            own_subtitle(&s, false),
            "Does a thing.\nNot set: the plugin's default (true) applies."
        );
        assert_eq!(
            own_subtitle(&Setting::int("N", "N", 0, 9), false),
            "Not set: the plugin's default applies."
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

    fn form_at(
        path: Option<PathBuf>,
        nix: &[(&str, &str)],
        on_saved: OnSaved,
    ) -> SettingsForm {
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
                        subtitle: "Set in nix — programs.trollshell.plugins.vibectl.env.V1BECTL_SERVER"
                            .to_owned(),
                        sensitive: false,
                    }
                ),
                ("V1BECTL_DEBUG".to_owned(), RowView::Switch),
                ("V1BECTL_COLUMNS".to_owned(), RowView::Spin { min: 1, max: 8 }),
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
        let form = form_at(Some(path.clone()), &[("V1BECTL_SERVER", "host:1")], on_saved);
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
        assert_eq!(form.buttons(), (false, false), "a loaded Int is not an edit");
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
        assert!(form.status().contains("does not parse"), "{}", form.status());
        assert!(!saved.get(), "no restart after a refused save");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "[vibectl\n");
    }
}
