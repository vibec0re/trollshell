# The "lean heavy on nix" harness for the NetworkManager Wi-Fi
# backend (#96): boot a real NixOS VM with NetworkManager and a pair
# of virtual Wi-Fi radios (mac80211_hwsim), then drive wifi_nm
# end-to-end via the wifi_probe example — backend detection, device
# discovery, a live RequestScan, and a state read. mac80211_hwsim
# gives NM a real (simulated) wlan device so the whole D-Bus path
# exercises against a live daemon, not a mock. Mirrors
# eds-nixos-test; runs under TCG (no KVM needed).
#
# Split out of flake.nix (#1102) into its own `callPackage`-able file,
# mirroring how `packages` already lives under `nix/*.nix`. Takes the one
# module fixture the block closed over there: `wifiProbe` (nix/wifi-probe.nix's
# slice of `workspace`).
{
  pkgs,
  wifiProbe,
}:
pkgs.testers.runNixOSTest {
  name = "wifi-nm-nixos-test";
  nodes.machine =
    { ... }:
    {
      networking.networkmanager.enable = true;
      # Two virtual 802.11 radios; NM manages the resulting wlan
      # interfaces, giving the probe a real device + AP scan path.
      boot.kernelModules = [ "mac80211_hwsim" ];
      boot.extraModprobeConfig = "options mac80211_hwsim radios=2";
      environment.systemPackages = [ wifiProbe ];
      virtualisation.graphics = false;
    };
  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("NetworkManager.service")
    # Wait until NM has a Wi-Fi device registered (hwsim + NM takeover).
    machine.wait_until_succeeds(
        "nmcli -t -f DEVICE,TYPE device | grep ':wifi'", timeout=60
    )

    # Run the probe as root on the system bus — drives wifi_nm against
    # the live NetworkManager.
    output = machine.wait_until_succeeds("wifi_probe", timeout=180)
    assert "backend=NetworkManager" in output, output
    assert "device=" in output, output
    assert "scan=" in output, output
    assert "networks=" in output, output
  '';
}
