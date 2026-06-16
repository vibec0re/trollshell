//! Ephemeral evolution-data-server harness for the `system-tests` (#29/#33).
//!
//! Spins up a private session `dbus-daemon`, isolated `XDG_*` dirs, and the
//! `evolution-source-registry` + `evolution-calendar-factory` daemons (located
//! from the nix store via `EDS_LIBEXEC_DIR`, baked by `build.rs`), seeded with
//! one local-backend calendar and one task-list source. Lets the FFI be tested
//! against a real-but-throwaway EDS instead of a developer's live data —
//! Annika's ask on #29. Mirrors `hytte-bus`'s dbus-daemon-per-test harness.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// libexec dir holding the EDS daemons, baked at build time from
/// `pkg-config --variable=prefix libecal-2.0`.
const EDS_LIBEXEC_DIR: Option<&str> = option_env!("EDS_LIBEXEC_DIR");

/// Display names of the two fixture sources the harness seeds.
pub const CAL_NAME: &str = "hytte test calendar";
pub const TASKS_NAME: &str = "hytte test tasks";

/// A running ephemeral EDS. Dropping it kills every spawned daemon and wipes
/// the temp dir.
pub struct Eds {
    _tmp: TempDir,
    children: Vec<Child>,
}

impl Drop for Eds {
    fn drop(&mut self) {
        // Kill factory → registry → dbus (reverse spawn order).
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct EnvSet {
    bus: String,
    config: String,
    data: String,
    cache: String,
    home: String,
}

impl EnvSet {
    fn apply(&self, cmd: &mut Command) {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", &self.bus)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("HOME", &self.home)
            .env("GSETTINGS_BACKEND", "memory");
    }
}

/// Spawn an ephemeral EDS with a fixture calendar + task list, and point the
/// current process's env at it so the `hytte-ecal` FFI talks to it.
///
/// One instance per test process: GLib caches the session-bus connection on
/// first use, so a second `spawn()` in the same process would still talk to
/// the first bus. Keep one EDS per integration-test file and drive all
/// assertions through it.
///
/// # Panics
/// If `EDS_LIBEXEC_DIR` is unset (not built in the devShell), `dbus-daemon`
/// isn't on `PATH`, or a daemon never registers its bus name.
#[must_use]
pub fn spawn() -> Eds {
    let libexec = EDS_LIBEXEC_DIR
        .expect("EDS_LIBEXEC_DIR unset — run the system-tests inside the nix devShell");
    let registry_bin = Path::new(libexec).join("evolution-source-registry");
    let factory_bin = Path::new(libexec).join("evolution-calendar-factory");

    let tmp = TempDir::new().expect("create tempdir");
    let root = tmp.path();
    let bus_sock = root.join("bus.sock");
    let env = EnvSet {
        bus: format!("unix:path={}", bus_sock.display()),
        config: root.join("config").display().to_string(),
        data: root.join("data").display().to_string(),
        cache: root.join("cache").display().to_string(),
        home: root.display().to_string(),
    };

    let sources = root.join("config/evolution/sources");
    std::fs::create_dir_all(&sources).expect("mkdir sources");
    std::fs::create_dir_all(&env.data).expect("mkdir data");

    // 1. Private session bus.
    let conf = root.join("session.conf");
    std::fs::write(&conf, dbus_config(&env.bus)).expect("write dbus config");
    let dbus = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(&conf)
        .arg("--nofork")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbus-daemon (needs to be on PATH)");
    wait_for_path(&bus_sock, "dbus socket");

    // 2. Fixture sources — written before the registry starts so it adopts
    //    them on first scan. The file base name becomes the source UID.
    std::fs::write(
        sources.join("hytte-cal.source"),
        source_file(CAL_NAME, "Calendar"),
    )
    .expect("write calendar source");
    std::fs::write(
        sources.join("hytte-tasks.source"),
        source_file(TASKS_NAME, "Task List"),
    )
    .expect("write tasks source");

    // 3. EDS daemons against the private bus.
    let registry = spawn_daemon(&registry_bin, &[], &env);
    wait_for_bus_name(&env.bus, "org.gnome.evolution.dataserver.Sources5");
    let factory = spawn_daemon(&factory_bin, &["--keep-running"], &env);
    wait_for_bus_name(&env.bus, "org.gnome.evolution.dataserver.Calendar8");

    // 4. Point THIS process's env at the ephemeral EDS for the FFI calls.
    // SAFETY: single-threaded test setup, before any D-Bus/FFI use in-process.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &env.bus);
        std::env::set_var("XDG_CONFIG_HOME", &env.config);
        std::env::set_var("XDG_DATA_HOME", &env.data);
        std::env::set_var("XDG_CACHE_HOME", &env.cache);
        std::env::set_var("HOME", &env.home);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }

    Eds {
        _tmp: tmp,
        children: vec![dbus, registry, factory],
    }
}

fn spawn_daemon(bin: &Path, args: &[&str], env: &EnvSet) -> Child {
    let mut cmd = Command::new(bin);
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    env.apply(&mut cmd);
    cmd.spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()))
}

/// A local-backend `.source` keyfile for the given EDS extension
/// (`"Calendar"` or `"Task List"`).
fn source_file(display: &str, extension: &str) -> String {
    format!(
        "[Data Source]\nDisplayName={display}\nEnabled=true\n\n\
         [{extension}]\nBackendName=local\nColor=#1c71d8\nSelected=true\n"
    )
}

fn dbus_config(address: &str) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
    )
}

fn wait_for_path(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll the bus (via `dbus-send`) until `name` is registered, so a daemon is
/// actually up before the next step talks to it.
fn wait_for_bus_name(bus: &str, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = Command::new("dbus-send")
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .args([
                "--session",
                "--dest=org.freedesktop.DBus",
                "--type=method_call",
                "--print-reply",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus.ListNames",
            ])
            .output();
        if let Ok(o) = out
            && String::from_utf8_lossy(&o.stdout).contains(name)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "EDS bus name {name} never appeared on the ephemeral bus"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
