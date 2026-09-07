//! The plugin's own I/O: the `AgentStatus` poll loop and the command lane.
//!
//! Spec §5.1 picks the shape deliberately. `hytte-claude-bridge` binds its
//! listener *before* handing the main thread to the SDK, because its clients
//! are other plugins; `hytte-plugin-infobroker` starts its server from
//! `sources()`, because it serves the shell. This plugin is the infobroker's
//! shape: it serves nobody but the shell, so with no shell there is nothing to
//! render and a poll loop that outlived the session would burn socket round
//! trips for no reader. The loop is created per session and dies with it.
//!
//! # Parking
//!
//! `host.sock` is poll-only — there is no subscribe verb
//! (`hive-host-sock/src/lib.rs:117-432`; the ask is hyperhive#4064) — so v1
//! polls on a cadence. Two mitigations, both free (spec §5.4): the plugin
//! mounts in the **sidebar**, so it subscribes `StateKey::SlotVisible` and
//! parks the poll while the sidebar is closed; and a poll answering
//! identically re-renders to an identical `View`, which the SDK dedups before
//! it reaches the wire, so a quiet hive costs one round trip and zero frames.
//!
//! One deliberate exception to the parking: a **single seed poll at task
//! start**, regardless of visibility. The `SlotVisible` seed the runtime
//! delivers at register is whatever the sidebar happens to be
//! (`crates/hytte-plugin/src/lib.rs:472-476`), usually closed, and a card
//! reading "connecting…" until the sidebar is first opened would be a worse
//! trade than one round trip at startup.
//!
//! **A known tension, recorded rather than silently resolved.** §8's
//! `Effect::Notify` fires on a `failed` / `needs_login` **edge**, and a parked
//! poll observes no edges — so with the sidebar closed the toast does not
//! fire. §5.4 is explicit about the parking, so P1 implements the parking; if
//! Annika wants toasts from a closed sidebar, the change is a slow hidden
//! cadence here plus one key in `agents.toml`, and nothing else moves.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hytte_plugin::CmdReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{self, AgentsConfig};
use crate::hive::wire::{AgentStatusRow, HiveUrls, Request};
use crate::hive::{HiveError, client};

/// A command from the reducer to this task — the sanctioned outbound lane
/// (#280). `update` is sync, so it cannot do the I/O itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// The mount surface showed (`true`) or hid (`false`).
    SetVisible(bool),
    /// Send one frame to the hive, then re-poll immediately so the row
    /// reconciles against reality rather than against the optimistic flip.
    Send(Request),
}

/// A message from this task back into the reducer, folded as `Input::App`.
#[derive(Clone, Debug)]
pub enum Msg {
    /// `agents.toml` was (re)loaded. Boxed because it is much larger than the
    /// other variants and `clippy::large_enum_variant` is right about it.
    Config(Box<AgentsConfig>),
    /// One poll answered.
    Status(Result<Vec<AgentStatusRow>, HiveError>),
    /// The hive's `Urls`, fetched once per session after the first successful
    /// poll — it backs the panel's agent-page link and changes about never.
    Urls(Box<HiveUrls>),
}

/// Watches the `agents.toml` layer files for edits, so a save while the
/// sidebar is open is picked up on the next poll.
///
/// The same live-reload `places` gets, arrived at the same way: stat the
/// layers rather than re-parsing them every tick. `hytte-config` resolves the
/// search path (`crates/hytte-config/src/xdg.rs:130-156`), so an edit to any
/// layer — the nix-provided base or the user's overlay — is seen.
struct ConfigWatch {
    layers: Vec<PathBuf>,
    stamps: Vec<Option<SystemTime>>,
}

impl ConfigWatch {
    fn new() -> Self {
        let layers = hytte_config::xdg::config_layers(config::NAME);
        let stamps = layers.iter().map(|p| stamp(p)).collect();
        Self { layers, stamps }
    }

    /// `true` when any layer's mtime (or existence) moved since the last call.
    fn changed(&mut self) -> bool {
        let now: Vec<Option<SystemTime>> = self.layers.iter().map(|p| stamp(p)).collect();
        let moved = now != self.stamps;
        self.stamps = now;
        moved
    }
}

fn stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// The I/O task behind [`crate::plugin::Agents::sources`].
///
/// Owns both directions: it drains the command lane and re-emits every poll as
/// a [`Msg`] on the message stream. Returns when the lane closes, which is the
/// session tearing down.
pub async fn poll_task(mut cmds: CmdReceiver<Cmd>, msg_tx: UnboundedSender<Msg>) {
    let mut cfg = config::load();
    let mut watch = ConfigWatch::new();
    if msg_tx.send(Msg::Config(Box::new(cfg.clone()))).is_err() {
        return;
    }

    let mut visible = false;
    let mut urls_done = false;
    let mut interval = tokio::time::interval(cfg.poll_interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The seed poll — see the module docs on why it ignores `visible`.
    poll_once(&cfg, &msg_tx, &mut urls_done).await;

    loop {
        tokio::select! {
            // Prefer commands over interval ticks, so a close parks the poller
            // promptly rather than firing one more round trip first.
            biased;
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    return; // lane closed → session teardown
                };
                match cmd {
                    Cmd::SetVisible(want) => {
                        let opened = want && !visible;
                        visible = want;
                        if opened {
                            // Reset first so the next scheduled tick lands a
                            // clean interval after this immediate refresh.
                            interval.reset();
                            if reload(&mut cfg, &mut watch, &msg_tx) {
                                interval = fresh_interval(&cfg);
                            }
                            poll_once(&cfg, &msg_tx, &mut urls_done).await;
                        }
                    }
                    Cmd::Send(req) => {
                        match client::request(Path::new(&cfg.socket), &req).await {
                            Ok(_) => tracing::debug!(?req, "hive accepted"),
                            Err(e) => tracing::warn!(?req, %e, "hive refused"),
                        }
                        // Re-poll regardless of the outcome: the roster is the
                        // truth, and a refused write must un-stick the row's
                        // optimistic flip just as fast as an accepted one.
                        interval.reset();
                        poll_once(&cfg, &msg_tx, &mut urls_done).await;
                    }
                }
            }
            // Disabled while hidden — the poller parks (no ticks, no sockets).
            _ = interval.tick(), if visible => {
                if reload(&mut cfg, &mut watch, &msg_tx) {
                    interval = fresh_interval(&cfg);
                }
                poll_once(&cfg, &msg_tx, &mut urls_done).await;
            }
        }
        if msg_tx.is_closed() {
            return;
        }
    }
}

fn fresh_interval(cfg: &AgentsConfig) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(cfg.poll_interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Re-read `agents.toml` when a layer moved. `true` when the config changed
/// and the caller must re-arm its interval.
fn reload(cfg: &mut AgentsConfig, watch: &mut ConfigWatch, msg_tx: &UnboundedSender<Msg>) -> bool {
    if !watch.changed() {
        return false;
    }
    let next = config::load();
    if next == *cfg {
        // An mtime moved but nothing we read did (a comment edit, a touch).
        return false;
    }
    tracing::info!(socket = %next.socket, poll_seconds = next.poll_seconds, "agents.toml reloaded");
    *cfg = next;
    let _ = msg_tx.send(Msg::Config(Box::new(cfg.clone())));
    true
}

/// One `AgentStatus` round trip, plus a one-shot `Urls` once the hive answers.
async fn poll_once(cfg: &AgentsConfig, msg_tx: &UnboundedSender<Msg>, urls_done: &mut bool) {
    let socket = Path::new(&cfg.socket);
    let status = match client::request(socket, &Request::AgentStatus).await {
        // A daemon that answers `ok` but carries no roster is answering a
        // question we did not ask; an empty roster is a legitimate answer and
        // `unwrap_or_default` renders it as "no agents" rather than an error.
        Ok(resp) => Ok(resp.agent_statuses.unwrap_or_default()),
        Err(e) => Err(e),
    };
    let ok = status.is_ok();
    if msg_tx.send(Msg::Status(status)).is_err() {
        return;
    }

    if ok
        && !*urls_done
        && let Ok(resp) = client::request(socket, &Request::Urls).await
        && let Some(urls) = resp.urls
    {
        *urls_done = true;
        let _ = msg_tx.send(Msg::Urls(Box::new(urls)));
    }
}
