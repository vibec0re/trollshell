//! The **plain-fetch** `ureq::Agent` builder (#1168) — connect + global-read
//! timeouts, nothing else.
//!
//! Five call sites (the weather plugin's open-meteo and Nominatim fetchers,
//! the usage plugin's Grafana poll, caw's ingredient gathering, the
//! departures plugin's HAFAS fetch) independently rewrote the same three
//! lines: `config_builder().timeout_connect(..).timeout_global(..).build()`
//! then `.into()`/`Agent::from(..)`. None of them set a `User-Agent` on the
//! agent itself — the one site that needs a descriptive UA (the weather
//! plugin's Nominatim reverse-geocode, whose usage policy rejects stock
//! library UAs) sets it as a **per-request header**, not an agent default,
//! so there is no shared UA string to hoist here.
//!
//! This is deliberately **not** [`crate::chat`]'s agent: that one fixes a 2s
//! connect timeout, disables `http_status_as_error` (a chat caller wants the
//! JSON error body, not a bare status `Err`), and routes through
//! [`crate::unix`] for `unix://` base URLs. A plain-fetch agent against a
//! public HTTP(S) API needs none of that — different timeouts per caller,
//! default status-as-error behaviour, TCP only.
//!
//! Takes the two [`Duration`]s directly rather than wrapping them in a new
//! type: `ureq::config::Timeouts` already owns that name (and covers nine
//! finer-grained phases this crate has no opinion about), so a
//! same-named-but-different `Timeouts` here would only invite confusion at
//! the call site.

use std::time::Duration;

/// Build a blocking plain-fetch `ureq::Agent`: `connect` bounds the TCP
/// connect + TLS handshake (`timeout_connect`), `read` bounds the whole
/// request — connect, send **and** read, not just the read half
/// (`timeout_global`). Every existing copy of this builder picked its own
/// pair (5-10s connect, 12-30s read); this hoist does **not** collapse those
/// to one shared constant, since they reflect each endpoint's own patience
/// budget, not a spelling difference worth de-duplicating.
#[must_use]
pub fn agent(connect: Duration, read: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(connect))
        .timeout_global(Some(read))
        .build();
    ureq::Agent::from(config)
}

#[cfg(test)]
mod tests {
    use super::agent;
    use std::time::Duration;

    /// Falsification target: deleting either `.timeout_*` call in [`agent`]
    /// leaves `ureq`'s default (`None`, i.e. no timeout) in the `Config`
    /// instead — this pins that both durations actually reach it.
    #[test]
    fn agent_carries_the_requested_timeouts_into_the_config() {
        let a = agent(Duration::from_secs(5), Duration::from_secs(15));
        let timeouts = a.config().timeouts();
        assert_eq!(timeouts.connect, Some(Duration::from_secs(5)));
        assert_eq!(timeouts.global, Some(Duration::from_secs(15)));
    }
}
