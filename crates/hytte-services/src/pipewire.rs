//! Default audio sink volume + mute state, polled via `wpctl`.
//!
//! v0.2.0 uses a 250 ms shell-out poll for simplicity. v0.3+ should
//! switch to a proper `pipewire-rs` registry subscription so updates
//! arrive event-driven.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::process::Command;
use std::time::Duration;

pub struct PipewireService;

#[derive(Clone, Copy, Debug, Default)]
pub struct Volume {
    /// Linear volume, `0.0..=1.0` (may exceed 1.0 if user boosts above
    /// 100%). Untouched on parse failure.
    pub linear: f64,
    pub muted: bool,
}

#[doc(hidden)]
pub struct PipewireHandles {
    pub(crate) sink: Mutable<Volume>,
}

impl Default for PipewireHandles {
    fn default() -> Self {
        Self {
            sink: Mutable::new(Volume::default()),
        }
    }
}

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();
        let writer = handles.sink.clone();

        rt.spawn(async move {
            let mut last = Volume::default();
            loop {
                if let Some(v) = poll() {
                    #[allow(clippy::float_cmp)]
                    if v.linear != last.linear || v.muted != last.muted {
                        writer.set(v);
                        last = v;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });

        handles
    }
}

fn poll() -> Option<Volume> {
    let out = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?;
    parse(s)
}

fn parse(s: &str) -> Option<Volume> {
    // Expected: "Volume: 0.65 [MUTED]\n" or "Volume: 0.65\n"
    let trimmed = s.trim();
    let rest = trimmed.strip_prefix("Volume:")?.trim();
    let mut parts = rest.split_whitespace();
    let linear: f64 = parts.next()?.parse().ok()?;
    let muted = rest.contains("[MUTED]");
    Some(Volume { linear, muted })
}

#[must_use]
pub fn service() -> PipewireService {
    PipewireService
}

pub fn default_sink() -> impl Signal<Item = Volume> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sink
            .signal_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parse_unmuted() {
        let v = parse("Volume: 0.65\n").unwrap();
        assert!((v.linear - 0.65).abs() < 1e-9);
        assert!(!v.muted);
    }

    #[test]
    fn parse_muted() {
        let v = parse("Volume: 0.20 [MUTED]\n").unwrap();
        assert!((v.linear - 0.20).abs() < 1e-9);
        assert!(v.muted);
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse("not wpctl output").is_none());
        assert!(parse("Volume: foo").is_none());
    }
}
