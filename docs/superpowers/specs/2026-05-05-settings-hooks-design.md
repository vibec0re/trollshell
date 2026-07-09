# Settings hooks

**Status:** design approved 2026-05-05
**Scope:** new module `crates/hytte-services/src/hooks.rs`; one new line in `crates/hytte-services/src/theme.rs::set`.

## Motivation

`hytte::services::theme::set` already fans out a theme change to GTK4/libadwaita, legacy GTK, and Qt[56]ct. Tools that don't honor any of those — `starship`, `alacritty`, terminal emulators with their own config — are left out. Rather than teaching trollshell about every such tool, expose a hook: when the desktop theme flips, run a user-authored script. The script repaints starship, rewrites alacritty colors, or whatever else the user needs.

## Design

### Layout & contract

- Hook scripts live at `$HOME/.config/trollshell/hooks/<event>` — one file per event, no `.d/` directory, no extension required.
- The file must be a regular file with the executable bit set (`mode & 0o111 != 0`). It is invoked directly (no `sh -c` wrapping); the user's shebang decides the interpreter.
- v1 shipped one event, `theme-changed`; more slot into the same directory under different names as callers are added (the API itself is unchanged). Fired events so far:
  - `theme-changed` (`theme::set`) — the desktop theme flipped.
  - `place-changed` (`places::resolve_loop`) — the resolved current place transitioned (Wi-Fi fingerprint / GeoClue presence). Deduped on the place **name**, and the first resolution after startup is silent, so login stays quiet and GeoClue jitter within one place doesn't re-fire (#235).
- Inputs are passed as environment variables, never as positional args:
  - `TROLLSHELL_EVENT=<event-name>` — always present
  - Event-specific vars set by the caller:
    - `theme-changed`: `TROLLSHELL_THEME=light` or `TROLLSHELL_THEME=dark`.
    - `place-changed`: `TROLLSHELL_PLACE=<place name>` and `TROLLSHELL_PLACE_STATION=<station id>` (empty when the place has no configured station, e.g. "away").
- `$HOME` resolution mirrors `theme.rs::config_subdir`: `$HOME/.config/...` directly, no `$XDG_CONFIG_HOME`. If both files later need XDG support, both get upgraded together.

### `hooks::run` API

```rust
pub fn run(event: &str, env: &[(&str, &str)]);
```

Synchronously: resolves the path, spawns a detached `tokio::task` on the `hytte-reactive` runtime, and returns. Caller never blocks, never gets an error. All outcomes go to `tracing`.

### Execution flow inside the spawned task

1. Resolve `$HOME/.config/trollshell/hooks/<event>`. Missing `$HOME` → `tracing::warn!`, return.
2. `std::fs::metadata(path)`:
   - `NotFound` → `tracing::debug!(event, "hooks: no script configured")`, return. No-hook is the common case and must not be noisy.
   - Other I/O error → `tracing::warn!`, return.
   - Not a regular file (dir, symlink-to-missing) → `tracing::warn!`, return.
   - Regular file, mode `& 0o111 == 0` → `tracing::warn!(path, "hooks: script not executable")`, return.
3. Build `tokio::process::Command`:
   - program = the hook path
   - `stdin(Stdio::null())`, `stdout(Stdio::piped())`, `stderr(Stdio::piped())`
   - inherit env, then `.env("TROLLSHELL_EVENT", event)` and each `(k, v)` in the caller-supplied `env` slice
4. `cmd.spawn()` to get an owned `Child` (so we can kill it on timeout — `wait_with_output` consumes the child and forecloses that). Take `child.stdout` and `child.stderr` handles, then `tokio::select!` between (a) `child.wait()` paired with reading both pipes to end-of-stream, and (b) `tokio::time::sleep(HOOK_TIMEOUT)`, where `HOOK_TIMEOUT: Duration = Duration::from_secs(10)` is a module-level const.
5. Branch on the select outcome:
   - Wait completes first, status is success → `tracing::info!(event, "hooks: ran")`. If captured stdout non-empty → `tracing::debug!` it. Same for stderr.
   - Wait completes first, status non-zero → `tracing::warn!(event, status = ?status, stdout = %String::from_utf8_lossy(&stdout), stderr = %String::from_utf8_lossy(&stderr), "hooks: script failed")`.
   - Wait completes with an I/O error (spawn-side errors land at step 4 already; wait-side errors are rare but possible) → `tracing::warn!(event, error = %io_err, "hooks: wait failed")`.
   - Sleep wins → `child.start_kill()` (best-effort), `tracing::warn!(event, "hooks: script timed out after 10s")`. Pipe readers and the child handle drop together; partial output is not surfaced.
6. If `cmd.spawn()` itself errored at step 4 → `tracing::warn!(event, error = %io_err, "hooks: spawn failed")` and return.

### `theme::set` integration

After all toolkit fan-out (gsettings spawns + ini writes) at the bottom of `theme::set`, append:

```rust
hooks::run(
    "theme-changed",
    &[("TROLLSHELL_THEME", match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
    })],
);
```

Order matters: hooks run _after_ the toolkit writes so a script that `gsettings get`s the current scheme sees the new value.

### Module placement

`crates/hytte-services/src/hooks.rs`, declared `pub mod hooks;` in `lib.rs`. Re-exported as `hytte::services::hooks` via the existing umbrella crate. No new dependency: `tokio::process`, `tokio::time`, and `tracing` are all already in the `hytte-services` dep graph.

## Failure-mode summary

| Condition                               | Action                                         | Log level |
| --------------------------------------- | ---------------------------------------------- | --------- |
| `$HOME` unset                           | return                                         | `warn`    |
| Script absent                           | return silently                                | `debug`   |
| `metadata()` errors with non-`NotFound` | return                                         | `warn`    |
| Path is not a regular file              | return                                         | `warn`    |
| Regular file, no exec bit               | return                                         | `warn`    |
| Spawn fails (ENOENT race, ENOMEM, …)    | return                                         | `warn`    |
| Exit 0                                  | success; stdout/stderr at `debug` if non-empty | `info`    |
| Exit non-zero                           | logged with status + captured stdout/stderr    | `warn`    |
| Wall-clock > 10s                        | child killed, output abandoned                 | `warn`    |
| No tokio runtime when `run` is called   | return                                         | `warn`    |

No panics. No errors propagated to caller. All tracing events carry structured fields: `event`, `path`, `status`, `stdout`, `stderr` as applicable.

## Testing

Unit tests live in `hooks.rs` `#[cfg(test)] mod tests`. Each test:

1. Acquires a process-wide mutex (since `$HOME` is mutated).
2. Creates a fresh tempdir, sets `$HOME` to it.
3. Writes a script (or doesn't) to `<home>/.config/trollshell/hooks/<event>` with the desired mode.
4. Captures `tracing` events through a test subscriber. Add `tracing-subscriber` to `hytte-services`'s `[dev-dependencies]` if not already there.
5. Calls `hooks::run` inside a `#[tokio::test]` body.
6. Asserts on captured events.

Cases:

1. **No script configured.** Hook file absent. Expect one `DEBUG` event with field `event = "test-event"`. No `WARN` events.
2. **Success path.** Script `#!/bin/sh\necho hi\nexit 0`, mode 0o755. Expect one `INFO` event. `stdout` field (at `DEBUG`) contains `"hi\n"`.
3. **Non-zero exit.** Script exits 1 with text on stderr. Expect one `WARN` event with `status` field reflecting code 1 and `stderr` field containing the text.
4. **Non-executable.** Script written, mode 0o644. Expect one `WARN` event matching "not executable". The script's body must contain a side-effect-detectable command (e.g. `touch sentinel`); assert sentinel does not exist.
5. **Timeout.** Script `#!/bin/sh\nsleep 30`. Expect one `WARN` event matching "timed out" within ~10–11s. Assert no orphan child by polling `/proc/<pid>` or relying on `Child::start_kill` semantics.
6. **Env vars.** Script writes `$TROLLSHELL_EVENT` and `$TROLLSHELL_THEME` to a sentinel file. Caller passes `("TROLLSHELL_THEME", "dark")`. Assert sentinel contains the expected event name and theme value.

No integration test for `theme::set` → `hooks::run`. The wire-up is a single line; a unit test on `hooks::run` plus the type system covers it.

## Out of scope (deferred)

- `.d/` directories of multiple scripts per event.
- Positional args / arg-parsing.
- OSD or settings-panel surfacing of hook failures.
- `$XDG_CONFIG_HOME` resolution (would be a one-shot upgrade across `theme.rs` and `hooks.rs` together).
- Further events: `network-up`, `power-state`, `lock`, etc. The contract is designed so they slot in by adding a new caller; `hooks::run` itself does not need to change. (`place-changed` was the first such addition — see the event list above.)
