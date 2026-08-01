//! Library target that exists **only** to give `trollshell/tests/*.rs`
//! (the `system-tests` integration bucket, #674) something to link against.
//!
//! `trollshell` is a binary crate — `main.rs` declares its own `mod` tree and
//! has no `[lib]` target, so an external integration test in `tests/` cannot
//! reach any of its modules (a `cargo test` crate is always a *separate*
//! compilation unit from the thing under test, and Rust's privacy model has
//! no path in from outside a binary at all). This file exists purely to give
//! that external test crate a path in, by declaring the same source files
//! `main.rs` declares as a real `[lib]` target instead.
//!
//! `main.rs` is untouched: it keeps its own separate `mod modal;` (etc.)
//! declarations and compiles them again into the binary as before — the two
//! targets are independent compilations of the same source files, so nothing
//! here changes the shipped binary's behaviour. Cargo auto-detects both
//! `src/lib.rs` and `src/main.rs` from a single package with no extra
//! `[lib]`/`[[bin]]` wiring needed.
//!
//! The module list is deliberately **narrower** than `main.rs`'s full set: it
//! covers exactly the transitive `crate::` closure `modal.rs` needs to
//! compile (verified by grepping every `crate::` reference reachable from
//! `modal`), not the whole binary. `commands`, `control`, `fullscreen`,
//! `plugin_launcher`, `revision` and `secrets` are never referenced from that
//! closure, so they're left out — adding them would only widen the surface
//! this crate has to keep clean under `--features system-tests` clippy for no
//! test benefit.
//!
//! `must_use_candidate` (`clippy::pedantic`) only fires for a `lib`-typed
//! crate — it assumes a `pub fn`'s return value might be silently discarded
//! by a caller outside your control, which isn't a meaningful warning for a
//! `bin` crate (no external caller exists). Every `pub fn widget(...)` /
//! `panel_*()` / `install(...)` etc. below is written exactly as it is in the
//! real (bin-only) `trollshell`, where this lint has never applied; adding
//! `#[must_use]` to ~40 call sites across files outside this crate's file
//! lane just to satisfy a lint this shadow lib target invented would be
//! exactly the kind of change #674 asks NOT to make. Scoped to this crate,
//! not the workspace default.
#![allow(clippy::must_use_candidate)]

pub mod assets;
pub mod components;
pub mod modal;
pub mod overlays;
pub mod panels;
pub mod plugins;
// `scale`'s `CSS_BASE_FONT_PX`/`css_base_font_px` are `pub(crate)` and used
// only by `main.rs`'s `install_scaled_base_font` (the CSS base-font-size
// injection, #135 part 2) — which isn't part of this lib target. They're
// live in the real binary; here they're just unreachable from this crate's
// own tree, which `dead_code` can't tell apart from a genuine orphan. Scoped
// to this one `mod` rather than a crate-wide allow.
#[allow(dead_code)]
pub mod scale;
pub mod widgets;
