//! Public data types for the Bluetooth service.

// ── Public data shapes ────────────────────────────────────────────────────────

/// Snapshot of the Bluetooth adapter state.
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// D-Bus object path, e.g. `"/org/bluez/hci0"`.
    pub path: String,
    pub address: String,
    pub name: String,
    pub powered: bool,
    pub discoverable: bool,
    pub discovering: bool,
}

/// What sort of pairing prompt the `BlueZ` agent is asking us to handle.
/// The UI uses this to choose copy/buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// "Confirm pairing with X — code 123456" style. The user matches the
    /// number against the one shown on the remote device. Most modern path.
    ConfirmPasskey,
    /// Bare "allow this device to pair?" without a code. Older or
    /// no-input devices.
    Authorize,
    /// Legacy: the device wants the user to type a free-form PIN string
    /// (length up to 16 chars, ASCII). Used by older pre-SSP devices.
    EnterPinCode,
    /// Legacy: the device wants the user to type a 0..=999999 numeric
    /// passkey. Pre-SSP path; rare on modern hardware.
    EnterPasskey,
}

/// A pending Bluetooth pairing prompt the user must accept or reject.
/// The agent suspends pairing on the `BlueZ` side until
/// `respond_to_prompt(...)` is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairPrompt {
    pub device_path: String,
    /// Resolved alias from the `devices()` snapshot at prompt time.
    /// Falls back to the bare path if the device isn't yet in our cache.
    pub alias: String,
    pub passkey: Option<u32>,
    pub kind: PromptKind,
}

/// Snapshot of a single paired/nearby Bluetooth device.
#[derive(Clone, Debug, Default)]
pub struct Device {
    /// D-Bus object path, e.g. `"/org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX"`.
    pub path: String,
    pub address: String,
    /// User-friendly alias (falls back to Name then Address).
    pub alias: String,
    /// Freedesktop icon name from `BlueZ`, e.g. `"audio-headphones"`.  Empty
    /// when `BlueZ` doesn't report one.
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    /// Battery percentage 0..=100, when the device exposes the
    /// `org.bluez.Battery1` interface (mostly headphones, mice, keyboards).
    /// `None` when `BlueZ` doesn't report one — either the device doesn't
    /// support it or it's currently disconnected.
    pub battery: Option<u8>,
}

/// Internal reply variants from the UI back to the agent's awaiting
/// method handler. Each variant maps to a specific Agent1 method's
/// expected return shape.
#[derive(Debug)]
pub(crate) enum AgentReply {
    /// User clicked Confirm on a yes/no prompt (`RequestConfirmation`,
    /// `RequestAuthorization`).
    Confirm,
    /// User explicitly rejected — agent throws `org.bluez.Error.Rejected`.
    Reject,
    /// User submitted a PIN code for `RequestPinCode`.
    Pin(String),
    /// User submitted a numeric passkey for `RequestPasskey`.
    Passkey(u32),
}
