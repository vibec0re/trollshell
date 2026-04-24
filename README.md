# trollshell + hytte

A library-first Rust toolkit (`hytte`) for composing GTK4 + libadwaita + layer-shell desktop shells, and `trollshell` — the personal shell built on it.

This repo holds the v0.1 milestone: a top-edge bar on every Niri monitor with workspaces (left) and a clock (right). See `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` for the full design.

## Build

```sh
cargo build --release -p trollshell
```

## Run (Niri only, v0.1)

```sh
cargo run --release -p trollshell
```

`trollshell` connects to `$NIRI_SOCKET` for compositor state. Make sure you're inside a Niri session.

## Repo layout

- `crates/hytte-reactive/` — `Service` trait, thread-local registry, tokio runtime accessor, `bind` helpers.
- `crates/hytte-ui/` — `App`, `Bar`, `LayerWindow` primitives + default shell stylesheet.
- `crates/hytte-services/` — `clock`, `niri` (more in v0.2+).
- `crates/hytte/` — umbrella re-export crate.
- `trollshell/` — the binary.

## Logs

```sh
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell
```
