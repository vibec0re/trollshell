# `host.sock` wire fixtures

One `HostResponse` JSON line per file, exactly as `hive-c0re` writes it — the
drift detector spec §12 asks for, and the reason P1 is buildable, testable and
reviewable before a hive exists on the laptop.

## Provenance — read this before trusting them

Spec §12 says the fixtures are "**recorded from a live hive**, checked in
verbatim … Recording them is the one step that needs Annika's machine." **That
step has not happened yet**, because there is no hive on the laptop until #949
lands `services.hyperhive.deploy.singleHostSwarm`.

So these are **derived from hyperhive's own source** rather than recorded from
a running daemon: every field name, field _order_, `#[serde(default)]` and
`skip_serializing_if` was read off hyperhive `origin/main` and reproduced by
hand —

| shape               | source                                                            |
| ------------------- | ----------------------------------------------------------------- |
| `HostResponse`      | `hive-host-sock/src/lib.rs:521-569` (+ its `Default`, `:591-612`) |
| `HOST_SOCK_VERSION` | `hive-host-sock/src/lib.rs:526`                                   |
| `AgentStatusRow`    | `hive-sh4re/src/container.rs:34-96`                               |
| `HiveUrls`          | `hive-host-sock/src/lib.rs:503-518`                               |

That is weaker than a recording in exactly one way, and it is worth naming: a
hand-derived fixture can agree with a _misreading_ of the source, where a
recording cannot. It is stronger than the alternative the spec warns about ("a
hand-written fixture would only prove the mirror agrees with itself") because
these are written against the daemon's struct definitions, not against
`crates/hytte-plugin-agents/src/hive/wire.rs` — the mirror was written first
and the fixtures were derived independently from hyperhive.

**Re-record on Annika's machine when #949 lands.** The live-verify entry
(`docs/live-verify.md`) carries that as a checklist item. A recording that
differs from these files is a finding, not a formatting nit.

## The files

| file                             | what it is                                                       |
| -------------------------------- | ---------------------------------------------------------------- |
| `agent_status_grouped.json`      | three agents across two projects — the golden roster             |
| `agent_status_precedence.json`   | one agent per §6.2 precedence row, plus the `needs_update` badge |
| `agent_status_empty.json`        | a hive with no agents                                            |
| `agent_status_unknown_keys.json` | forward drift: keys this build has never heard of                |
| `agent_status_v0.json`           | a pre-version daemon (no `version` key at all)                   |
| `agent_status_v99.json`          | a daemon newer than this build — must be refused, not read       |
| `agent_status_bad_name.json`     | a row whose name fails the §11 whitelist                         |
| `list.json`                      | a `List` answer                                                  |
| `urls.json`                      | a `Urls` answer                                                  |
| `error.json`                     | `ok: false` with the daemon's own message                        |

`view_*.txt` are render-tree goldens, not wire fixtures — see
`tests/view_golden.rs` for how to regenerate them.
