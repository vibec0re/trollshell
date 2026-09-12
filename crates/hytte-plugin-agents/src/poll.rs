//! The plugin's own I/O: the poll loop — `AgentStatus`, and since #947 P3 the
//! `Pending` approval queue on the same tick — plus the command lane.
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
//! Spec §5.4 was written when `host.sock` was poll-only. It is not any more:
//! **`SubscribeAgentStatus` landed** (hyperhive#4064,
//! `hive-host-sock/src/lib.rs:209-224`) and turns the connection into a push
//! feed. P1 still polls, by decision rather than by necessity — switching the
//! data path is the first follow-up after this merges, and it is a change
//! rather than a swap: the feed is best-effort by its own contract ("a slow
//! reader can miss updates rather than back-pressuring the daemon"), so a
//! client must *still* reconcile with a fresh `AgentStatus` after a gap. The
//! poll does not go away; it becomes the reconciler behind the stream. See
//! [`crate::hive::wire::Request::SubscribeAgentStatus`].
//!
//! So v1 polls on a cadence. Two mitigations, both free (spec §5.4): the plugin
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
use crate::hive::wire::{AgentStatusRow, Approval, HiveUrls, Request};
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
    /// The approval queue, filtered to what is still waiting on a human and
    /// ordered oldest first (#947 P3).
    ///
    /// Sent **only** when the `Pending` round trip succeeded. A failed one is
    /// logged and dropped rather than folded as an empty queue: the badge is
    /// derived from this, and a transient refusal that cleared every badge —
    /// then restored them on the next tick — would be a flicker that tells the
    /// operator the opposite of the truth. The roster's own
    /// [`Msg::Status`] failure already takes the card out of `Hive::Up`, which
    /// is where the badges are drawn, so nothing stale is shown either.
    Pending(Vec<Approval>),
    /// A [`Cmd::Send`] the hive did not accept.
    ///
    /// The row un-sticks on the next poll either way, but "the button flipped
    /// and snapped back" explains nothing — least of all on the two failures
    /// most likely in practice, a permission problem and
    /// `agent "…" is not managed by this hive`. The reducer turns this into
    /// exactly one `Effect::Notify`; the request rides along un-phrased so
    /// the wording stays where the plugin's other toast is.
    WriteRefused {
        /// The frame that was refused.
        request: Request,
        /// The hive's own message.
        reason: String,
    },
}

/// Where the running config comes from, and how a change to it is noticed.
///
/// The same live-reload `places` gets, arrived at the same way: stat the
/// layers rather than re-parsing them every tick. `hytte-config` resolves the
/// search path (`crates/hytte-config/src/xdg.rs:130-156`), so an edit to any
/// layer — the nix-provided base or the user's overlay — is seen.
///
/// [`ConfigSource::over`] is the test seam: the production path watches the
/// process's XDG layers, which a test cannot point at a tempdir without
/// mutating global environment every other test in the binary shares.
pub struct ConfigSource {
    layers: Vec<PathBuf>,
    stamps: Vec<Option<SystemTime>>,
}

impl ConfigSource {
    /// The real thing: `$XDG_CONFIG_DIRS` bases then the `$XDG_CONFIG_HOME`
    /// overlay, exactly the list [`config::load`] reads.
    #[must_use]
    pub fn xdg() -> Self {
        Self::over(hytte_config::xdg::config_layers(config::NAME))
    }

    /// Watch an explicit layer list, lowest precedence first.
    #[must_use]
    pub fn over(layers: Vec<PathBuf>) -> Self {
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

    /// Re-read the layers, degrading to the documented default on any failure
    /// — the same contract [`config::load`] has, over the same reader.
    fn read(&self) -> AgentsConfig {
        match hytte_config::subsystem::load_from::<AgentsConfig>(&self.layers) {
            Ok(loaded) => loaded.config,
            Err(e) => {
                tracing::error!(%e, "agents.toml is unusable; keeping the built-in default");
                AgentsConfig::default()
            }
        }
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
pub async fn poll_task(cmds: CmdReceiver<Cmd>, msg_tx: UnboundedSender<Msg>) {
    poll_task_with(cmds, msg_tx, config::load(), ConfigSource::xdg()).await;
}

/// [`poll_task`] with its seed config injected — the seam the park / reload /
/// seed / `urls_done` tests drive.
///
/// It exists only because [`poll_task`] resolves its config from the **process
/// environment** (`$XDG_CONFIG_HOME`, `$HOME`), which a test cannot set without
/// serialising every other test in the binary against a global mutation. With
/// `cfg` passed in, the loop's four behaviours — parking on visibility,
/// reloading on an `agents.toml` mtime, the seed poll, and the one-shot `Urls`
/// latch — are drivable against a `FakeHive` in a tempdir with no environment
/// at all. The live reload still watches the real XDG layers, so what the
/// tests do not cover is only *which files* are watched.
pub async fn poll_task_with(
    mut cmds: CmdReceiver<Cmd>,
    msg_tx: UnboundedSender<Msg>,
    mut cfg: AgentsConfig,
    mut watch: ConfigSource,
) {
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
                            Err(e) => {
                                tracing::warn!(?req, %e, "hive refused");
                                // One message per refusal — the reducer turns
                                // it into exactly one toast.
                                let _ = msg_tx.send(Msg::WriteRefused {
                                    request: req.clone(),
                                    reason: e.to_string(),
                                });
                            }
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
fn reload(cfg: &mut AgentsConfig, watch: &mut ConfigSource, msg_tx: &UnboundedSender<Msg>) -> bool {
    if !watch.changed() {
        return false;
    }
    let next = watch.read();
    if next == *cfg {
        // An mtime moved but nothing we read did (a comment edit, a touch).
        return false;
    }
    tracing::info!(socket = %next.socket, poll_seconds = next.poll_seconds, "agents.toml reloaded");
    *cfg = next;
    let _ = msg_tx.send(Msg::Config(Box::new(cfg.clone())));
    true
}

/// One `AgentStatus` round trip, then the `Pending` queue, plus a one-shot
/// `Urls` once the hive answers.
///
/// # Why `Pending` rides this tick rather than a task of its own (#947 P3)
///
/// Spec §5.4 gives the status poll one cadence, one parking rule and one
/// config key, and the approval queue wants all three to be *the same* ones —
/// so this is the existing poll extended, not a second task:
///
/// - **The badge is on the row.** A badge count folded from a different tick
///   than the roster it decorates can describe an agent the card is no longer
///   drawing. One round of I/O per tick keeps them a single observation.
/// - **Parking is the whole energy argument.** A second task would need its own
///   copy of the `SlotVisible` gate, the `agents.toml` reload and the seed
///   poll, and any divergence between the two copies would be an approval
///   prompt firing at a closed sidebar.
/// - **It costs one round trip on a unix socket**, on a cadence measured in
///   seconds, and only after the status call already proved the socket is
///   answering.
///
/// The order matters and is not alphabetical: `AgentStatus` goes first so a
/// dead hive costs exactly one failed connect, and `Pending` is skipped
/// entirely when it failed.
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
    if !ok {
        return;
    }

    // #947 P3. A failure here is a debug line, not a `Msg` — see `Msg::Pending`
    // for why an empty queue must not stand in for an unanswered one.
    match client::request(socket, &Request::Pending).await {
        Ok(resp) => {
            if msg_tx.send(Msg::Pending(resp.pending_approvals())).is_err() {
                return;
            }
        }
        Err(e) => tracing::debug!(%e, "the approval queue did not answer; keeping the last one"),
    }

    if !*urls_done
        && let Ok(resp) = client::request(socket, &Request::Urls).await
        && let Some(urls) = resp.urls
    {
        *urls_done = true;
        let _ = msg_tx.send(Msg::Urls(Box::new(urls)));
    }
}
