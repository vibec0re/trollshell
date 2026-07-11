//! Shell effects and the pages/actions they drive.
//!
//! An [`Effect`] is something a plugin *requests* on its render frame; the host
//! matches each to a real `do_thing` command and gates it on the plugin's
//! granted [`Capability`](crate::manifest::Capability) set. The plugin never
//! touches D-Bus / niri directly. These are wire-side mirrors of the host's
//! command surfaces; the host maps them (PR 2) — this crate stays GTK-free and
//! host-free.

use serde::{Deserialize, Serialize};

/// A drawer page the host can open. Wire-side mirror of the host's
/// `modal::Page`; the host maps `wire::Page -> modal::Page` (PR 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Media,
    Network,
    Vpn,
    Connections,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}

/// A niri compositor action (maps to `hytte_services::niri::*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NiriAction {
    FocusWorkspace { id: u64 },
    FocusWindow { id: u64 },
}

/// A media-player transport action (maps to `hytte_services::mpris::*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
}

/// An audio-sink action (maps to the `pipewire` default-sink setters).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AudioAction {
    /// Set the default sink volume, `0.0..=1.0`.
    SetVolume(f64),
    ToggleMute,
}

/// A shell effect a plugin returns on its [`Render`](crate::msg::PluginMsg::Render)
/// frame (bundled with the tree so a frame is atomic). Each maps to a real host
/// command, gated on the matching capability.
///
/// All variants are fire-and-forget **except** [`Effect::RunCommand`], whose
/// outcome comes back as a [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult)
/// keyed by its `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Open a drawer page (cap: [`OpenPage`](crate::manifest::Capability::OpenPage)).
    OpenPage(Page),
    /// A niri action (cap: [`Niri`](crate::manifest::Capability::Niri)).
    Niri(NiriAction),
    /// A media transport action (cap: [`Media`](crate::manifest::Capability::Media)).
    Media(MediaAction),
    /// An audio action (cap: [`Audio`](crate::manifest::Capability::Audio)).
    Audio(AudioAction),
    /// Spawn a command (cap: [`RunCommand`](crate::manifest::Capability::RunCommand),
    /// a separately granted, higher-trust capability). `id` correlates the
    /// resulting [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult).
    RunCommand { id: u64, argv: Vec<String> },
}

/// The outcome of a brokered [`Effect::RunCommand`], returned to the plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Whether the command exited successfully.
    pub ok: bool,
    /// Captured stdout (host may truncate), if any.
    pub output: Option<String>,
}
