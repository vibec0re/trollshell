//! The bridge's **wallet board** (#1347) — one card per thing this desktop
//! spends from, stacked on the one drawer page.
//!
//! # Why the wallets live in this daemon
//!
//! #1347 was filed as a *new plugin* (`hytte-plugin-openrouter`), on the
//! reasoning that the bridge owns the Claude OAuth token and nothing else
//! should ride its process. Annika's answer on the thread (2026-09-17) was one
//! sentence — *"I think we could stack the cards in the same drawer"* — and
//! that settles the shape rather than the styling, because **a drawer page
//! belongs to one plugin**: the host mounts `Page::PluginSelf` per plugin id
//! (`trollshell/src/modal.rs`), so "both cards on one page" can only mean "one
//! plugin draws both". The bridge already draws one of them.
//!
//! The reachability argument that kept the HTTP route on a same-uid socket
//! (#993) is untouched by this: that argument is about the **route** — a
//! keyless endpoint that spends the owner's subscription, where being able to
//! connect *is* the authorization — not about the poller. A second outbound
//! credential read by the same process adds no inbound surface at all.
//!
//! # The board
//!
//! Wallet one is [`crate::usage`], which predates the idea and is deliberately
//! not moved here: it is the Claude quota reading, it is what the bar chip
//! paints, and since #1262 it is also a datasource other plugins query. Wallet
//! two is [`openrouter`]. The stacking, the order (**Claude first**) and the
//! chip's hover line are [`crate::plugin`]'s, because "which surfaces exist and
//! in what order" is that module's job; what a wallet's own card looks like is
//! the wallet's, which is why [`openrouter::card`] lives here.
//!
//! A third wallet is the moment to hoist a `Wallet` trait over the two —
//! deliberately **not** done for the second (Annika: a generic wallet board
//! waits until a third wallet exists). The concrete cost today would be making
//! [`crate::usage`]'s `Report`/`Outcome` generic over their payload, and that
//! is the public surface #1262's datasource is built on.
//!
//! # Where the settings live, and why they are environment variables
//!
//! This crate declares **no `hytte_config::Subsystem`** — it has no
//! `claude-bridge.toml`, no `hytte-config` dependency, and nix renders its knobs
//! as plugin `env` entries (`nix/hm-module.nix`'s
//! `programs.trollshell.plugins.claude-bridge.env`). So #1347's `[openrouter]`
//! table is spelled the way every other knob in this daemon is spelled — as
//! variables read once at startup, next to `CLAUDE_BRIDGE_MODE`
//! (`crate::Settings::from_env`) and `CLAUDE_BRIDGE_LABEL`
//! (`crate::plugin::card_title`):
//!
//! | `[openrouter]` key | variable                       |
//! |--------------------|--------------------------------|
//! | *(the key itself)* | [`openrouter::KEY_ENV`]        |
//! | `enabled`          | [`openrouter::ENABLE_ENV`]     |
//! | `low_credit`       | [`openrouter::LOW_CREDIT_ENV`] |
//! | *(an optional label)* | [`openrouter::LABEL_ENV`]   |
//!
//! Giving this one daemon a TOML subsystem for two scalars would put its
//! settings in two places at once, which is the failure mode `hytte-config`'s
//! layering exists to prevent rather than to cause. If the bridge ever grows a
//! config file, all of its knobs move together and this table moves with them.

pub mod openrouter;
