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
| `HostResponse`      | `hive-host-sock/src/lib.rs:546-604` (+ its `Default`, `:612-629`) |
| `HOST_SOCK_VERSION` | `hive-host-sock/src/lib.rs:543`                                   |
| `AgentStatusRow`    | `hive-sh4re/src/container.rs:34-96`                               |
| `HiveUrls`          | `hive-host-sock/src/lib.rs:519-534`                               |

### The gap that leaves was measured, and it is closed

The obvious weakness of a hand-derived fixture is that it can agree with a
_misreading_ of the source, where a recording cannot. That was checked, and it
did not happen. The PR's adversarial review built a throwaway crate holding
**verbatim copies** of hyperhive `origin/main` (`ded23b71`)'s own
`hive-types`, `hive-sh4re::container` and `hive-host-sock` — four documented
substitutions, all in cross-crate `use`s plus one field the plugin never reads
— and put this directory through them:

- all 10 fixtures **decode** through hyperhive's real `HostResponse`;
- 8 of them **decode → re-serialize with hyperhive's own `Serialize` →
  compare → byte-identical**: `agent_status_grouped`,
  `agent_status_precedence`, `agent_status_empty`, `agent_status_bad_name`,
  `agent_status_v99`, `list`, `urls`, `error`;
- all 8 pinned request lines from `wire.rs` deserialize into the **correct**
  `HostRequest` variant, and the plugin's `Start` / `Stop` scope reads `false`
  through hyperhive's own `LifecycleScope::is_everything()` — while `{}` and
  `{"agent_names":[]}` read `true`, so the §11 footgun is real and the mirror
  dodges it.

A byte-identical round trip through the real writer pins every field **name**,
every field **order**, and every `skip_serializing_if`. What it cannot pin is a
field `hive-c0re` _populates_ differently from what its struct implies — a much
smaller surface than "these were written by hand".

**Re-record on Annika's machine anyway when #949 lands.** The live-verify entry
(`docs/live-verify.md`) carries that as a checklist item; it is now
belt-and-braces rather than the only evidence. A recording that differs from
these files is still a finding, not a formatting nit.

## The files

| file                             | what it is                                                                             |
| -------------------------------- | -------------------------------------------------------------------------------------- |
| `agent_status_grouped.json`      | three agents across two projects — the golden roster                                   |
| `agent_status_precedence.json`   | one agent per §6.2 precedence row, plus the `needs_update` badge                       |
| `agent_status_empty.json`        | a hive with no agents                                                                  |
| `agent_status_unknown_keys.json` | forward drift: keys this build has never heard of                                      |
| `agent_status_v0.json`           | a pre-version daemon (no `version` key at all)                                         |
| `agent_status_v99.json`          | a daemon newer than this build — must be refused, not read                             |
| `agent_status_bad_name.json`     | a row whose name fails the §11 whitelist                                               |
| `list.json`                      | a `List` answer                                                                        |
| `urls.json`                      | a `Urls` answer                                                                        |
| `error.json`                     | `ok: false` with the daemon's own message                                              |
| `pending.json`                   | a `Pending` answer: two queued approvals plus one already resolved                     |
| `pending_empty.json`             | a hive with an empty approval queue                                                    |
| `pending_unknown.json`           | forward drift: an `ApprovalKind` and an `ApprovalStatus` this build has never heard of |

### The three `pending_*.json` (#947 P3)

Same provenance and the same caveat, read off
`hive-sh4re/src/approvals.rs:14-38` (`Approval`, with its field **order** and
its two `skip_serializing_if`s) and `hive-host-sock/src/lib.rs:228-233`
(`Pending` / `Approve` / `Deny`). They are **not** part of the verbatim-copy
round trip described above, which predates them, so re-recording them against a
live hive is the same open item — and the one field worth watching is
`commit_ref`, which is overloaded per kind (a sha, a PR number, an inputs
array, or empty) and which this mirror deliberately does not read.

`pending.json` carries an `approved` row on purpose: `Pending`'s answer is
whatever the daemon's store returns, so the filter that keeps a resolved
approval from raising a prompt has to be exercised against a fixture that
contains one.

`view_*.txt` are render-tree goldens, not wire fixtures — see
`tests/view_golden.rs` for how to regenerate them.
