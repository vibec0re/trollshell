# Future ideas

Tracked-but-unscheduled work. When something here gets a spec, move it
to `docs/superpowers/specs/` and remove the entry.

## Networking

- **eBPF per-PID byte counts.** Augment `hytte-services::netconn` with
  cgroup-attached eBPF (aya crate) to deliver real per-process rx/tx.
  Requires CAP_BPF on the trollshell binary; kernel ≥ 5.13. Distinct
  enough to warrant its own spec when scheduled.

## Network drawer

- **Connections search/filter UI.** Defer until top-N sorted-by-program
  proves insufficient.
- **Port-name resolution** via `/etc/services`.

## Wi-Fi

- **Hidden-network entry, signal-strength graph, roaming history.**
