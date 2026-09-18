# The "lean heavy on nix" counterpart to the Rust ephemeral-EDS
# harness (#49): boot a real NixOS VM with evolution-data-server
# configured declaratively, seed a fixture task list + calendar, and
# run the hytte-ecal probe against it end-to-end. The probe also
# creates FREQ=DAILY;COUNT=5 VEVENTs (one with an EXDATE) and expands
# them, so this gates the RRULE-expansion fix for #29 and its
# EXDATE/RDATE follow-up, plus a TZID=Europe/Berlin event whose
# absolute instant guards the zoned-time fix (#522). Verified to run
# under TCG (no KVM needed);
# GitHub's Linux runners have /dev/kvm for speed.
#
# Split out of flake.nix (#1102) into its own `callPackage`-able file,
# mirroring how `packages` already lives under `nix/*.nix`. Takes the
# module fixtures the block closed over there: `probe` (nix/probe.nix's
# slice of `workspace`) and the seeded `taskSource` plus the two calendars —
# `calSource` (with a `[Calendar] Color=`) and `calSourceNoColor` (without;
# #1223 item 1 review, MED-5) — all still built in flake.nix's own `let`,
# since they're plain `pkgs.writeText` fixtures with no other consumer.
{
  pkgs,
  probe,
  taskSource,
  calSource,
  calSourceNoColor,
}:
pkgs.testers.runNixOSTest {
  name = "eds-nixos-test";
  nodes.machine =
    { ... }:
    {
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
      # Three-line EDS module: installs the package, wires its D-Bus
      # session activation service files, and its systemd user units.
      services.gnome.evolution-data-server.enable = true;
      programs.dconf.enable = true; # EDS GSettings backend
      services.gnome.gnome-keyring.enable = true; # EDS credential store
      environment.systemPackages = [ probe ];
      virtualisation.graphics = false;
    };
  testScript = ''
    machine.wait_for_unit("multi-user.target")

    # Seed the fixture task-list + calendar sources into alice's home.
    machine.succeed("mkdir -p /home/alice/.config/evolution/sources")
    machine.copy_from_host(
        "${taskSource}",
        "/home/alice/.config/evolution/sources/test-tasks.source",
    )
    machine.copy_from_host(
        "${calSource}",
        "/home/alice/.config/evolution/sources/test-calendar.source",
    )
    machine.copy_from_host(
        "${calSourceNoColor}",
        "/home/alice/.config/evolution/sources/test-calendar-no-color.source",
    )
    machine.succeed("chown -R alice:users /home/alice/.config")
    # Store-copied files land read-only (0444); EDS's source
    # registry rewrites .source files on first open to add runtime
    # keys, which fails ("Permission denied") on a read-only file —
    # benign for the auto-provisioned lists but it left the seeded
    # *calendar* unwritable, so creating an event on it failed. Make
    # the fixtures writable.
    machine.succeed("chmod -R u+w /home/alice/.config/evolution")

    # Bring up alice's user session (creates /run/user/1000/bus).
    machine.succeed("loginctl enable-linger alice")
    machine.wait_for_unit("user@1000.service")
    machine.wait_for_file("/run/user/1000/bus")

    # Run the probe as alice; EDS D-Bus-activates on first connect.
    schemas = "${pkgs.evolution-data-server}/share/gsettings-schemas"
    output = machine.wait_until_succeeds(
        "su -s /bin/sh alice -c '"
        + "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; "
        + "export HOME=/home/alice XDG_RUNTIME_DIR=/run/user/1000; "
        + "export GSETTINGS_SCHEMA_DIR=$(ls -d " + schemas + "/*/glib-2.0/schemas | head -1); "
        + "probe'",
        timeout=180,
    )
    # EDS auto-provisions a default "Personal" list, so the count
    # isn't 1 — assert our seeded fixture is enumerated and the FFI
    # create/remove roundtrip actually worked.
    assert "Test Tasks" in output, output
    assert "created uid: hytte-ecal-probe-1" in output, output
    assert "removed hytte-ecal-probe-1" in output, output

    # Live view push (#33): the probe opens a CalClientView over the
    # task list, then from a *second* client connection (standing in
    # for Endeavour) creates + modifies a task. EDS must push the
    # objects-added/-modified notifications to the view — exercising
    # get_view_sync → view_start → the GObject signal trampoline →
    # the boxed Rust callback, pumped via a private GMainContext. A
    # missing push would bail the probe (so wait_until_succeeds would
    # fail); the explicit count line is the positive signal. The probe
    # watches whichever task list EDS lists first (ordering isn't
    # guaranteed — could be the auto-provisioned "Personal" or our
    # "Test Tasks"), so don't pin the name here.
    assert "watching '" in output, output
    assert "editor created uid: hytte-ecal-live-1" in output, output
    assert "editor modified uid: hytte-ecal-live-1" in output, output
    assert "live view push count:" in output, output
    # At least the create + modify pushes landed (initial population +
    # 2). Parse the final count and assert it advanced past the
    # initial-population baseline. (The match can't be None here — the
    # assert above already required the line — but guard it so the
    # test driver's type checker is satisfied.)
    import re
    m = re.search(r"live view push count: (\d+)", output)
    assert m is not None, output
    push_count = int(m.group(1))
    assert push_count >= 2, f"expected >=2 view pushes, got {push_count}: {output}"

    # Recurrence expansion (#29): the probe seeds a
    # FREQ=DAILY;COUNT=5 VEVENT and expands it over a one-month
    # window. All 5 occurrences must materialise — the whole point
    # of the fix (the old master-only path would surface just 1).
    assert "Test Calendar" in output, output

    # Per-source colour (#1223 item 1): the seeded fixtures are a pair —
    # "Test Calendar" carries `[Calendar] Color=#ff8800`, "Uncoloured
    # Calendar" carries the `[Calendar]` group with no `Color=` at all —
    # and `Source::color` reads each off its own `ESourceSelectable`
    # extension. This is the only place that FFI read runs against a real
    # source registry; the hermetic tests cover only the decisions it
    # applies to the string.
    #
    # BOTH lines are asserted, because either alone is satisfiable by a
    # constant: measured on the review branch, a `Source::color` that
    # discards the FFI result and returns `Some("#ff8800")` keeps all 44
    # hermetic `hytte-ecal` tests green AND still matches the `#ff8800`
    # regex. Only the `(none)` line makes the pair discriminating — no
    # single constant can answer both.
    #
    # The `(none)` line carries a SECOND duty, and that one has already
    # earned its keep. There is no "unset" colour at the FFI layer:
    # `ESourceSelectable`'s `color` is a *construct* property with
    # `default-value="#62a0ea"`, so an uncoloured calendar answers GNOME
    # blue, not NULL. The first version of this assertion found that the
    # hard way — run 35331529534 printed
    #     - Uncoloured Calendar (test-calendar-no-color) color=#62a0ea
    # which is why `hytte_ecal::EDS_DEFAULT_CALENDAR_COLOR` exists and why
    # `Source::color` reports that exact value as `None`. So this line is
    # also a deliberate **version tripwire**: an EDS release that changes
    # the default reddens it here, which is the alarm we want — the
    # alternative is the shell's whole hash-palette fallback silently
    # dying and every unconfigured calendar on screen turning one colour.
    # Read a failure here as "EDS moved, update the constant", never as a
    # flake to retry.
    #
    # Keyed off the distinct display names rather than off position, so
    # neither the registry's ordering nor which calendar the recurrence
    # probe below happens to write into can flip them. ("Uncoloured
    # Calendar" does not match the first regex either: after `- Test
    # Calendar ` that line has an `N`, not the literal `(` the pattern
    # requires.)
    #
    # EDS's own sources appear in this output too — the auto-provisioned
    # `Personal` (#62a0ea, i.e. the default) and `Birthdays &
    # Anniversaries` (#ffbe6f). Deliberately NOT asserted on: their
    # colours are EDS's business, so pinning them would fail this test for
    # reasons that say nothing about our read.
    assert re.search(r"- Test Calendar \(.*\) color=#ff8800", output), output
    assert re.search(r"- Uncoloured Calendar \(.*\) color=\(none\)", output), output

    assert "created recurring uid:" in output, output
    assert "recurring instance count: 5" in output, output
    assert "removed recurring" in output, output

    # EXDATE exclusion (#29 follow-up): the probe seeds a second
    # FREQ=DAILY;COUNT=5 series with an EXDATE cancelling Jun 3.
    # Correct expansion drops that one occurrence (4, not 5) and the
    # cancelled instant must be absent — exactly the user-visible bug
    # this fix closes (a cancelled standup still showing up).
    assert "created exdate uid:" in output, output
    assert "exdate instance count: 4" in output, output
    assert "exdate cancelled occurrence present: false" in output, output
    assert "removed exdate" in output, output

    # Zoned time (#522): a `DTSTART;TZID=Europe/Berlin:…123000` event
    # (12:30 CEST) round-tripped through EDS must expand to the
    # *absolute* instant 10:30 UTC = start_unix 1784889000 — never
    # 1784896200 (12:30 UTC), the +2h double-shift that surfaced a
    # 12:30 event as 14:30 in the Upcoming list. This is the honest
    # end-to-end guard against the pre-fix bug and against #388
    # regressing in reverse; it exercises the real backend store, not
    # just the hermetic string parser.
    assert "created tzid uid:" in output, output
    assert "tzid instance count: 1" in output, output
    assert "tzid instance start_unix: 1784889000" in output, (
        "TZID=Europe/Berlin 12:30 must resolve to 10:30 UTC "
        "(1784889000), not 12:30 UTC (1784896200): " + output
    )
    assert "tzid instance start_unix: 1784896200" not in output, output
    assert "removed tzid" in output, output
  '';
}
