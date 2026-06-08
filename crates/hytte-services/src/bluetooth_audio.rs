//! Auto-switch the default pipewire sink to a Bluetooth audio device when one
//! connects, and restore the previous sink on disconnect.
//!
//! Reactive consumer of `bluetooth::devices()` and `pipewire::sinks()` —
//! does not run its own I/O loop. Spawned on the GTK main loop because both
//! source signals are stored in a thread-local registry that only the main
//! thread can read.
//!
//! # State machine
//!
//! Two strings tracked across emissions:
//!   * `last_non_bt_default` — name of the most recent non-BT default sink.
//!     Updated whenever the default is currently a non-BT sink. Used to
//!     restore the user's previous default after BT disconnect.
//!   * `last_observed_bt_default` — name of the BT sink currently set as
//!     default. Tracking it lets us detect the "BT used to be default,
//!     it's gone now" transition without confusing it with the "user picked
//!     the BT sink themselves" steady state.
//!
//! Edge transitions that fire a switch:
//!   * **BT appears** — `last_observed_bt_default` is None and a connected
//!     BT audio device with a matching pipewire sink shows up → switch.
//!     The bookkeeping above already captured the prior default's name, so
//!     a later BT-disappear edge knows what to restore to. The
//!     `last_observed_bt_default.is_none()` guard is what makes this a
//!     one-shot per BT episode: subsequent emissions where the user has
//!     manually picked another sink see `Some(...)` and skip.
//!   * **BT disappears** — `last_observed_bt_default` is Some, but no
//!     connected BT audio device matches a pipewire sink anymore → restore
//!     `last_non_bt_default` if any.
//!
//! Steady-state emissions (BT already-default, or no BT at all) only update
//! the bookkeeping fields; they do **not** call `set_default_sink`. That's
//! how clicking a different sink in the Audio drawer doesn't get clobbered:
//! the user's manual choice becomes the new `last_non_bt_default`, and on
//! a future BT disconnect we'll restore to that.
//!
//! # Heuristics
//!
//! * "Is this device BT audio?" — the `BlueZ` `Icon` field starts with one of
//!   `audio-headphones`, `audio-headset`, `audio-speakers`, `audio-card`.
//!   Devices without a recognized icon are ignored entirely — no auto-switch.
//! * "Does this sink belong to that device?" — pipewire's bluez sink names
//!   look like `bluez_output.AC_C5_8B_XX_XX_XX.1`. We match by replacing
//!   `:` with `_` in the device's MAC and looking for it as a substring of
//!   the sink name.
//!
//! # Persistence
//!
//! User toggle persisted to `~/.config/trollshell/bluetooth-audio.toml` as a
//! single-line `enabled = true|false` flag. Default ON. The file is parsed
//! permissively — any value other than `false` keeps the feature ON. Writes
//! are best-effort; failure is logged and the in-memory state is the source
//! of truth for the running process.

use futures_signals::map_ref;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, runtime};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bluetooth::{self, Device};
use crate::pipewire::{self, Sink};

// ── Persistence ──────────────────────────────────────────────────────────────

const CONFIG_REL_PATH: &str = ".config/trollshell/bluetooth-audio.toml";

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_REL_PATH))
}

fn load_enabled_from_disk() -> bool {
    let Some(path) = config_path() else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return true;
    };
    // Permissive: look for `enabled = false` anywhere; otherwise default ON.
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("enabled") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            if rhs.eq_ignore_ascii_case("false") {
                return false;
            }
            if rhs.eq_ignore_ascii_case("true") {
                return true;
            }
        }
    }
    true
}

fn save_enabled_to_disk(enabled: bool) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "bluetooth-audio: mkdir failed");
        return;
    }
    let body = format!("enabled = {enabled}\n");
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(error = %e, path = %path.display(), "bluetooth-audio: write failed");
    }
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct BluetoothAudioHandles {
    pub(crate) enabled: Mutable<bool>,
}

impl Default for BluetoothAudioHandles {
    fn default() -> Self {
        Self {
            enabled: Mutable::new(load_enabled_from_disk()),
        }
    }
}

/// The bluetooth-audio auto-switch service marker. Pass to `App::with`
/// AFTER both `bluetooth::service()` and `pipewire::service()` so the
/// reactor task can subscribe to their signals.
pub struct BluetoothAudioService;

impl Service for BluetoothAudioService {
    type Handles = BluetoothAudioHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // The Service protocol gives us a tokio handle, but the reactor needs
        // to read from `bluetooth::devices()` and `pipewire::sinks()` which
        // both go through the thread-local registry. The reactor spawns from
        // `init()` once the GTK main context is alive; here we just publish
        // the persisted toggle state.
        BluetoothAudioHandles::default()
    }
}

#[must_use]
pub fn service() -> BluetoothAudioService {
    BluetoothAudioService
}

// ── Public toggle API ────────────────────────────────────────────────────────

/// Signal of the user-facing auto-switch toggle. Bound to the Switch row
/// in the bluetooth drawer page.
pub fn auto_switch_enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<BluetoothAudioHandles>()
            .expect(
                "bluetooth_audio::init() must be called after \
                 bluetooth::service() and pipewire::service()",
            )
            .enabled
            .signal_cloned()
    })
}

/// Update the toggle and persist it to disk. Idempotent — no-op when the
/// value already matches.
pub fn set_auto_switch_enabled(on: bool) {
    let prev = registry::with(|r| {
        r.get::<BluetoothAudioHandles>().map(|h| {
            let cur = h.enabled.get();
            if cur != on {
                h.enabled.set(on);
            }
            cur
        })
    });
    if prev != Some(on) {
        // File I/O off the GTK main thread.
        runtime::handle().spawn_blocking(move || save_enabled_to_disk(on));
    }
}

// ── Heuristics ───────────────────────────────────────────────────────────────

/// True for `BlueZ` icon names that identify an audio device. Only these
/// devices are considered for auto-switch; everything else (mice, keyboards,
/// phones, etc.) is ignored.
fn is_bt_audio_icon(icon: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "audio-headphones",
        "audio-headset",
        "audio-speakers",
        "audio-card",
    ];
    PREFIXES.iter().any(|p| icon.starts_with(p))
}

/// Convert a `BlueZ` MAC like `"AC:C5:8B:11:22:33"` into the underscore form
/// pipewire embeds in `bluez_output.AC_C5_8B_11_22_33.1`.
fn mac_to_pw_token(addr: &str) -> String {
    addr.replace(':', "_")
}

/// True if `sink_name` looks like the bluez sink for `device`.
fn sink_belongs_to_device(sink_name: &str, device: &Device) -> bool {
    if device.address.is_empty() {
        return false;
    }
    // Pipewire sometimes lowercases MAC tokens in node names
    // (`bluez_input.ac_c5_…`) while BlueZ paths use uppercase. Match
    // case-insensitively to cover both shapes.
    let token = mac_to_pw_token(&device.address).to_ascii_uppercase();
    sink_name.to_ascii_uppercase().contains(&token)
}

/// Structural classifier for pipewire bluez sinks: matches by canonical name
/// prefix (`bluez_output.` / `bluez_source.`) regardless of whether a matching
/// connected device is present.
///
/// This exists so steady-state bookkeeping in `react()` doesn't depend on the
/// `BlueZ` device list and the pipewire sink list being in lock-step. During
/// a disconnect, `BlueZ` emits `Connected=false` immediately, but pipewire
/// takes a beat to drop the bluez sink. Without a structural check, the
/// still-default bluez sink would be misclassified as non-BT and pollute
/// `last_non_bt_default`.
fn is_bluez_sink_name(name: &str) -> bool {
    name.starts_with("bluez_output.") || name.starts_with("bluez_source.")
}

/// True when this sink is owned by *some* connected BT audio device in the
/// current device list.
///
/// Among connected BT audio devices, return the first one whose pipewire
/// sink we can find. None means no candidate to switch to right now.
fn find_bt_target<'a>(devices: &[Device], sinks: &'a [Sink]) -> Option<&'a Sink> {
    for dev in devices
        .iter()
        .filter(|d| d.connected && is_bt_audio_icon(&d.icon))
    {
        if let Some(sink) = sinks.iter().find(|s| sink_belongs_to_device(&s.name, dev)) {
            return Some(sink);
        }
    }
    None
}

// ── Reactor ──────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ReactorState {
    /// Most recent default sink name we observed that *wasn't* a BT sink.
    /// Used as the restore target when BT goes away. Updated continuously
    /// while a non-BT sink is default — that means a user manually switching
    /// to another non-BT sink seamlessly updates this slot.
    last_non_bt_default: Option<String>,
    /// Set when the current default is a BT sink. Cleared when no
    /// matching BT sink is present anymore. The presence of this value
    /// drives the "restore" branch.
    last_observed_bt_default: Option<String>,
}

/// One-shot guard so a future "reload services" feature or a confused test
/// harness doesn't end up with two reactor tasks racing each other on the
/// same `set_default_sink` calls.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Spawn the reactor on the GTK main loop. Call once from `main.rs` after
/// the App services are registered. Subsequent calls are no-ops (guarded by
/// `INITIALIZED` — see comment above).
///
/// # Panics
///
/// `bluetooth_audio::init()` must be called after `bluetooth::service()` and
/// `pipewire::service()` have been registered with `App::with`; otherwise
/// `auto_switch_enabled()` and the upstream signal lookups will panic with a
/// registration-order diagnostic.
pub fn init() {
    if INITIALIZED.swap(true, Ordering::Relaxed) {
        tracing::debug!("bluetooth_audio::init called twice — ignoring");
        return;
    }

    let state = Arc::new(Mutex::new(ReactorState::default()));

    let combined = map_ref! {
        let devices = bluetooth::devices(),
        let sinks = pipewire::sinks(),
        let enabled = auto_switch_enabled() => {
            (devices.clone(), sinks.clone(), *enabled)
        }
    };

    gtk::glib::MainContext::default().spawn_local(combined.for_each(
        move |(devices, sinks, enabled)| {
            react(&state, &devices, &sinks, enabled);
            std::future::ready(())
        },
    ));
}

fn react(state: &Mutex<ReactorState>, devices: &[Device], sinks: &[Sink], enabled: bool) {
    let mut st = state.lock().expect("reactor state mutex poisoned");

    // Always track the current default's identity so we can restore on
    // disconnect even if auto-switch was toggled off and on again.
    //
    // We classify by sink-name prefix here (NOT by joining against the live
    // device list) so a transient race between BlueZ's `Connected=false` event
    // and pipewire's removal of the bluez sink can't make us misclassify the
    // still-default bluez sink as non-BT and overwrite `last_non_bt_default`
    // with its name.
    let current_default = sinks.iter().find(|s| s.is_default);
    if let Some(cur) = current_default {
        if is_bluez_sink_name(&cur.name) {
            st.last_observed_bt_default = Some(cur.name.clone());
        } else {
            // Non-BT default: this is what we'd want to restore to.
            st.last_non_bt_default = Some(cur.name.clone());
            // We're not currently on a BT sink. If we previously had one,
            // a *real* BT-disappear edge needs to clear that bookkeeping
            // when no BT sink exists at all (handled in the disappear arm).
        }
    }

    if !enabled {
        // Toggle off: keep the bookkeeping current but never fire commands.
        return;
    }

    let bt_target = find_bt_target(devices, sinks);
    let bt_was_default_previously = st.last_observed_bt_default.is_some();

    match (bt_target, current_default) {
        // BT-appears edge: BT is available, no BT was observed as default
        // before, and the current default is non-BT (or absent). Fire the
        // switch ONCE per "BT becomes available" episode. Subsequent
        // emissions while the user manually picks another sink see
        // `bt_was_default_previously == true` and skip this arm — that's
        // how the user's manual override sticks.
        (Some(bt), cur_opt) if !bt_was_default_previously => {
            // If the user already had a non-BT default, the bookkeeping
            // above captured it as last_non_bt_default — perfect restore
            // target.
            let from = cur_opt.map_or("(none)", |c| c.name.as_str()).to_string();
            tracing::info!(
                bt_sink = %bt.name,
                %from,
                "bluetooth-audio: switching default to BT sink"
            );
            st.last_observed_bt_default = Some(bt.name.clone());
            pipewire::set_default_sink(&bt.name);
        }
        // BT available and we've already done the BT-appears edge for this
        // episode. Either:
        //   - default is still that BT sink → steady state.
        //   - default is something else (user manually switched) → respect it.
        // The bookkeeping at the top already updated last_non_bt_default
        // accordingly.
        (Some(_), _) => {}
        // BT not available: if we had observed one as default previously,
        // it has now disappeared — restore the captured non-BT default.
        (None, cur_opt) => {
            if st.last_observed_bt_default.take().is_some()
                && let Some(target) = st.last_non_bt_default.clone()
            {
                // Skip if the saved target is already the current default
                // (e.g. user manually switched away from BT before it
                // disconnected, so there's nothing to restore).
                let already_current = cur_opt.is_some_and(|c| c.name == target);
                // Belt-and-suspenders: even if `last_non_bt_default` somehow
                // ended up containing a bluez_-prefixed name, never call
                // set_default_sink on it. The structural check upstream
                // should already prevent this, but the cost of the extra
                // guard here is one strncmp.
                if is_bluez_sink_name(&target) {
                    tracing::warn!(
                        target = %target,
                        "bluetooth-audio: skip restore — saved target is itself a bluez sink"
                    );
                } else if already_current {
                    tracing::debug!(
                        target = %target,
                        "bluetooth-audio: skip restore — already current default"
                    );
                } else if sinks.iter().any(|s| s.name == target) {
                    tracing::info!(target = %target, "bluetooth-audio: restoring non-BT default");
                    pipewire::set_default_sink(&target);
                } else {
                    tracing::debug!(
                        target = %target,
                        "bluetooth-audio: skip restore — saved sink no longer present"
                    );
                }
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(addr: &str, icon: &str, connected: bool) -> Device {
        Device {
            address: addr.to_string(),
            icon: icon.to_string(),
            connected,
            ..Device::default()
        }
    }

    fn sink(name: &str, is_default: bool) -> Sink {
        Sink {
            id: 0,
            name: name.to_string(),
            description: String::new(),
            volume: 0.0,
            muted: false,
            is_default,
        }
    }

    #[test]
    fn icon_match_recognizes_audio_devices() {
        assert!(is_bt_audio_icon("audio-headphones"));
        assert!(is_bt_audio_icon("audio-headset"));
        assert!(is_bt_audio_icon("audio-speakers"));
        assert!(is_bt_audio_icon("audio-card"));
        assert!(!is_bt_audio_icon("input-mouse"));
        assert!(!is_bt_audio_icon("phone"));
        assert!(!is_bt_audio_icon(""));
    }

    #[test]
    fn mac_normalization_matches_pipewire_naming() {
        let d = dev("AC:C5:8B:11:22:33", "audio-headphones", true);
        assert!(sink_belongs_to_device(
            "bluez_output.AC_C5_8B_11_22_33.1",
            &d
        ));
        assert!(!sink_belongs_to_device("alsa_output.pci-0000_00_1f.3", &d));
    }

    #[test]
    fn find_bt_target_picks_connected_audio_with_sink() {
        let devs = vec![
            dev("AA:BB:CC:DD:EE:FF", "input-mouse", true), // not audio
            dev("11:22:33:44:55:66", "audio-headphones", false), // not connected
            dev("AC:C5:8B:11:22:33", "audio-headphones", true), // good
        ];
        let sinks = vec![
            sink("alsa_output.pci-0000_00_1f.3", true),
            sink("bluez_output.AC_C5_8B_11_22_33.1", false),
        ];
        let target = find_bt_target(&devs, &sinks).expect("should find BT target");
        assert_eq!(target.name, "bluez_output.AC_C5_8B_11_22_33.1");
    }

    #[test]
    fn find_bt_target_none_when_no_audio_connected() {
        let devs = vec![dev("AC:C5:8B:11:22:33", "audio-headphones", false)];
        let sinks = vec![sink("bluez_output.AC_C5_8B_11_22_33.1", false)];
        assert!(find_bt_target(&devs, &sinks).is_none());
    }

    #[test]
    fn react_records_non_bt_default_in_steady_state() {
        let st = Mutex::new(ReactorState::default());
        let devs: Vec<Device> = Vec::new();
        let sinks = vec![sink("alsa_output.builtin", true)];
        react(&st, &devs, &sinks, true);
        assert_eq!(
            st.lock().unwrap().last_non_bt_default.as_deref(),
            Some("alsa_output.builtin")
        );
        assert!(st.lock().unwrap().last_observed_bt_default.is_none());
    }

    #[test]
    fn react_marks_bt_when_already_default() {
        let st = Mutex::new(ReactorState::default());
        let devs = vec![dev("AC:C5:8B:11:22:33", "audio-headphones", true)];
        let sinks = vec![sink("bluez_output.AC_C5_8B_11_22_33.1", true)];
        react(&st, &devs, &sinks, true);
        assert_eq!(
            st.lock().unwrap().last_observed_bt_default.as_deref(),
            Some("bluez_output.AC_C5_8B_11_22_33.1")
        );
    }

    #[test]
    fn react_does_not_override_user_manual_pick() {
        // Steady state: user has BT connected and was switched to BT by the
        // BT-appears edge in a prior emission.
        let st = Mutex::new(ReactorState {
            last_non_bt_default: Some("alsa_output.builtin".to_string()),
            last_observed_bt_default: Some("bluez_output.AC_C5_8B_11_22_33.1".to_string()),
        });
        // Now the user clicks the builtin sink in the audio drawer.
        let devs = vec![dev("AC:C5:8B:11:22:33", "audio-headphones", true)];
        let sinks = vec![
            sink("alsa_output.builtin", true), // user picked this
            sink("bluez_output.AC_C5_8B_11_22_33.1", false),
        ];
        // We must not flip back to BT — last_observed_bt_default was already
        // set, so the BT-appears edge is suppressed.
        react(&st, &devs, &sinks, true);
        let state = st.lock().unwrap();
        // The reactor should preserve last_observed_bt_default (so that a
        // later disconnect still triggers restore) and update
        // last_non_bt_default to the user's choice (which is what they want
        // to fall back to next time).
        assert_eq!(
            state.last_observed_bt_default.as_deref(),
            Some("bluez_output.AC_C5_8B_11_22_33.1")
        );
        assert_eq!(
            state.last_non_bt_default.as_deref(),
            Some("alsa_output.builtin")
        );
    }

    #[test]
    fn react_clears_bt_state_on_disconnect() {
        // After user-pick: BT still connected with sink, user picked builtin.
        // Now BT disconnects entirely.
        let st = Mutex::new(ReactorState {
            last_non_bt_default: Some("alsa_output.builtin".to_string()),
            last_observed_bt_default: Some("bluez_output.AC_C5_8B_11_22_33.1".to_string()),
        });
        let devs: Vec<Device> = vec![]; // BT gone
        let sinks = vec![sink("alsa_output.builtin", true)]; // BT sink gone too
        react(&st, &devs, &sinks, true);
        let state = st.lock().unwrap();
        // last_observed_bt_default should be cleared by the take().
        assert!(state.last_observed_bt_default.is_none());
    }

    #[test]
    fn is_bluez_sink_name_recognizes_canonical_prefixes() {
        assert!(is_bluez_sink_name("bluez_output.AC_C5_8B_11_22_33.1"));
        assert!(is_bluez_sink_name("bluez_source.AC_C5_8B_11_22_33.a2dp"));
        // Plain `bluez_*` without a dot doesn't count — the canonical
        // pipewire shape always has the trailing dot.
        assert!(!is_bluez_sink_name("bluez_output_no_dot"));
        assert!(!is_bluez_sink_name("alsa_output.pci-0000_00_1f.3"));
        assert!(!is_bluez_sink_name(""));
    }

    #[test]
    fn react_handles_bt_disconnect_race_without_pollution() {
        // Reproduces the C1/C2 race: BlueZ reports the device as no longer
        // connected, but pipewire still has the bluez_output sink as
        // is_default=true. The bookkeeping must NOT misclassify that sink as
        // non-BT and clobber `last_non_bt_default` with its name.

        // Initial: BT connected, BT sink is default. The reactor records
        // `last_observed_bt_default` and we keep `last_non_bt_default` as the
        // alsa builtin (e.g. captured during a previous emission).
        let st = Mutex::new(ReactorState {
            last_non_bt_default: Some("alsa_output.builtin".to_string()),
            last_observed_bt_default: None,
        });
        let bt_dev = dev("AC:C5:8B:FF:FF:FF", "audio-headphones", true);
        let bt_sink = sink("bluez_output.AC_C5_8B_FF_FF_FF.1", true);
        let alsa = sink("alsa_output.builtin", false);
        react(&st, &[bt_dev], &[alsa.clone(), bt_sink.clone()], true);
        assert_eq!(
            st.lock().unwrap().last_observed_bt_default.as_deref(),
            Some("bluez_output.AC_C5_8B_FF_FF_FF.1")
        );

        // Race window: BlueZ emits Connected=false (devices empty) but
        // pipewire hasn't dropped the bluez_output sink yet — it's still
        // marked is_default=true.
        let bt_sink_still_default = sink("bluez_output.AC_C5_8B_FF_FF_FF.1", true);
        react(&st, &[], &[alsa, bt_sink_still_default], true);

        // Critical: name-pattern classifier identifies the lingering bluez
        // sink as BT-shaped, so `last_non_bt_default` stays as
        // "alsa_output.builtin" rather than being polluted with the bluez
        // sink name.
        let state = st.lock().unwrap();
        assert_eq!(
            state.last_non_bt_default.as_deref(),
            Some("alsa_output.builtin"),
            "race: bookkeeping mis-classified the lingering bluez sink as non-BT"
        );
    }

    #[test]
    fn react_disappear_arm_already_current_short_circuit() {
        // BT-disappear edge where the saved restore target is *already* the
        // current default (e.g. user manually switched away before BT
        // disconnected). The disappear arm must still `take()` the BT
        // observation so a reconnect re-arms BT-appears, and must not
        // clobber the saved target.
        let st = Mutex::new(ReactorState {
            last_non_bt_default: Some("alsa_output.builtin".to_string()),
            last_observed_bt_default: Some("bluez_output.AC_C5_8B_FF_FF_FF.1".to_string()),
        });
        let sinks = vec![sink("alsa_output.builtin", true)];
        react(&st, &[], &sinks, true);
        let state = st.lock().unwrap();
        assert!(state.last_observed_bt_default.is_none());
        assert_eq!(
            state.last_non_bt_default.as_deref(),
            Some("alsa_output.builtin")
        );
    }

    #[test]
    fn react_disappear_arm_with_no_current_default() {
        // BT-disappear edge with no current default sink at all: the
        // top-of-react bookkeeping leaves `last_non_bt_default` intact, so
        // the disappear arm sees a real restore-needed scenario (this is
        // the path that would call `set_default_sink(saved_target)`). The
        // pipewire call is fire-and-forget; we assert observable state
        // instead — observation cleared, saved target survives.
        let st = Mutex::new(ReactorState {
            last_non_bt_default: Some("alsa_output.builtin".to_string()),
            last_observed_bt_default: Some("bluez_output.AC_C5_8B_FF_FF_FF.1".to_string()),
        });
        let sinks = vec![sink("alsa_output.builtin", false)];
        react(&st, &[], &sinks, true);
        let state = st.lock().unwrap();
        assert!(state.last_observed_bt_default.is_none());
        assert_eq!(
            state.last_non_bt_default.as_deref(),
            Some("alsa_output.builtin")
        );
    }

    #[test]
    fn react_no_action_when_disabled() {
        let st = Mutex::new(ReactorState::default());
        let devs = vec![dev("AC:C5:8B:11:22:33", "audio-headphones", true)];
        let sinks = vec![
            sink("alsa_output.builtin", true),
            sink("bluez_output.AC_C5_8B_11_22_33.1", false),
        ];
        // Disabled: should not panic, should still record the non-BT default.
        react(&st, &devs, &sinks, false);
        assert_eq!(
            st.lock().unwrap().last_non_bt_default.as_deref(),
            Some("alsa_output.builtin")
        );
        assert!(st.lock().unwrap().last_observed_bt_default.is_none());
    }

    #[test]
    fn sink_belongs_to_device_matches_lowercase_pw_name() {
        let dev = Device {
            path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
            address: "AC:C5:8B:11:22:33".to_string(),
            ..Device::default()
        };
        assert!(sink_belongs_to_device(
            "bluez_input.ac_c5_8b_11_22_33.headset-head-unit",
            &dev,
        ));
    }

    #[test]
    fn sink_belongs_to_device_still_matches_uppercase() {
        let dev = Device {
            path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
            address: "AC:C5:8B:11:22:33".to_string(),
            ..Device::default()
        };
        assert!(sink_belongs_to_device(
            "bluez_output.AC_C5_8B_11_22_33.1",
            &dev,
        ));
    }

    #[test]
    fn sink_belongs_to_device_rejects_other_mac() {
        let dev = Device {
            path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
            address: "AC:C5:8B:11:22:33".to_string(),
            ..Device::default()
        };
        assert!(!sink_belongs_to_device(
            "bluez_output.DE_AD_BE_EF_00_00.1",
            &dev,
        ));
    }
}
