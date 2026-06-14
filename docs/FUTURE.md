# Future ideas

Tracked-but-unscheduled work. When something here gets a spec, move it
to `docs/superpowers/specs/` and remove the entry.

## Networking

- **eBPF per-PID byte counts.** Augment `hytte-services::netconn` with
  cgroup-attached eBPF (aya crate) to deliver real per-process rx/tx.
  Requires CAP_BPF on the trollshell binary; kernel ≥ 5.13. Distinct
  enough to warrant its own spec when scheduled.

## VPN

- **VPN connect/disconnect actions.** Read-only panel only for now;
  toggling tunnels is vendor- and config-specific.

## Network drawer

- **Connections search/filter UI.** Defer until top-N sorted-by-program
  proves insufficient.
- **Port-name resolution** via `/etc/services`.

## Wi-Fi

- **Hidden-network entry, signal-strength graph, roaming history.**

## CI / testing

- **GitHub CI running `nix flake check`** on the main flake (build + clippy
  gate via the package, treefmt formatting check, NixOS module eval).
- **A separate home-manager test flake** that evaluates the home-manager
  module (it needs `home-manager` as an input, which we deliberately keep out
  of the main flake so consumers don't inherit it). First attempt pulled the
  whole NixOS system + crane closure (~20 GB) just to eval; needs an
  eval-only approach that doesn't realise the world before it's worth adding.
  CI would run `nix flake check` on this flake too.
