//! Crate-wide test helpers (test builds only).

/// Call first in a plain `#[test]` that reaches GIO's lazily-created types or
/// defaults (`gio::File::for_path` → the default `GVfs`, `FileIcon`,
/// `ThemedIcon`, `AppInfo::create_from_commandline`) (#1461, #1463).
///
/// In the `system-tests` build the same binary's `#[gtk::test]`s run `gtk::init`
/// on the GTK test thread, and two threads first-initialising the same `GLib`
/// type or default at once can deadlock. So under that feature this parks the
/// caller behind the GTK test thread (`gtk::test_synced`, which runs
/// `gtk::init` first) and first-creates those types there, once; on the GTK
/// thread itself it warms directly, never waiting on itself. Without the
/// feature nothing calls `gtk::init`, so it is a no-op.
///
/// The four `plugins::effects::tests` that build a `gio::Cancellable` are
/// deliberately not routed through here: `GCancellable` is a leaf type with no
/// vfs, socket or module default behind it (#1463).
pub(crate) fn gio_ready() {
    #[cfg(feature = "system-tests")]
    {
        use std::sync::Once;
        static WARM: Once = Once::new();
        static READY: Once = Once::new();
        if hytte::gtk::is_initialized_main_thread() {
            WARM.call_once(warm_gio_types);
        } else {
            READY.call_once(|| hytte::gtk::test_synced(|| WARM.call_once(warm_gio_types)));
        }
    }
}

/// Real pointer input for display tests: `XTest` events through `xdotool` into
/// the test's X display, so a press, a drag and a release go down GTK's own
/// capture/target/bubble dispatch the way a user's would (#1413).
///
/// Used by `plugins::region`'s press-tracker tests and `panels::power`'s
/// brightness slider test (#1559). One copy, so the cleanup a review settled on
/// (#1416, MEDIUM 2) cannot drift between them.
#[cfg(feature = "system-tests")]
pub(crate) mod real_pointer {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use hytte::gtk::{self, glib, prelude::*};

    /// Whether this test can drive **real** pointer input: `xdotool` on `PATH`
    /// and GTK on an X11 display (`xdotool` speaks `XTest`, so a Wayland display
    /// — a `cargo test` run inside a live session without `xvfb-run` — is out of
    /// its reach).
    ///
    /// Honours `TROLLSHELL_REQUIRE_XDOTOOL` the way `plugins::tests`'
    /// `systemd_run_on_path_or_skip` honours `TROLLSHELL_REQUIRE_SYSTEMD_RUN`
    /// (itself on `hytte-ui`'s `TROLLSHELL_REQUIRE_GL` precedent): a skip is
    /// indistinguishable from a pass in captured output, so the build that means
    /// these to run — CI's `system-tests` check, which carries `pkgs.xdotool`
    /// and exports the variable — **fails** naming the reason, while a bare
    /// local run without `xdotool` skips.
    pub(crate) fn available_or_skip(test_name: &str) -> bool {
        use std::process::{Command, Stdio};

        let xdotool = Command::new("xdotool")
            .arg("version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let on_x11 = gtk::gdk::Display::default()
            .is_some_and(|display| display.type_().name() == "GdkX11Display");
        if xdotool && on_x11 {
            return true;
        }
        let reason = if xdotool {
            "GTK is not on an X11 display, and xdotool drives X11 only"
        } else {
            "xdotool is not on PATH"
        };
        let required =
            std::env::var_os("TROLLSHELL_REQUIRE_XDOTOOL").is_some_and(|want| want == "1");
        assert!(
            !required,
            "TROLLSHELL_REQUIRE_XDOTOOL=1, but {reason} for {test_name}",
        );
        eprintln!("SKIPPED {test_name}: {reason}");
        false
    }

    /// How long a real-pointer test waits for something — input it sent, a
    /// layout, a value — before it fails. A bound on a failure, never a delay
    /// on a pass: every wait returns the moment what it waits for has happened
    /// (#1416 review, MEDIUM 2).
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// Iterate the main context until `done`, then [`pump`] once more so the
    /// idles that dispatch queued have run too. Panics naming `what` and
    /// `report()` past [`DEADLINE`].
    ///
    /// This, and not a fixed sleep, is what makes the real-pointer tests
    /// independent of how loaded the machine is: `region.rs`'s first version
    /// pumped a fixed 250 ms after each command, and under `stress-ng` a
    /// release could still be in flight at the end of it — #1416's review
    /// measured 1 red in 85 runs that way.
    pub(crate) fn wait_for(what: &str, done: impl Fn() -> bool, report: impl Fn() -> String) {
        let deadline = Instant::now() + DEADLINE;
        let context = glib::MainContext::default();
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what} did not happen within {DEADLINE:?}: {}",
                report(),
            );
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        pump();
    }

    /// Run one `xdotool` command line to completion and check its status. What
    /// it sends is not waited for here — see [`RealPointer::send`].
    fn xdotool(args: &[&str]) {
        let status = std::process::Command::new("xdotool")
            .args(args)
            .status()
            .expect("xdotool runs");
        assert!(status.success(), "xdotool {args:?} failed: {status}");
    }

    /// Run `xdotool` for cleanup: no output, no status check, no panic.
    fn xdotool_quiet(args: &[&str]) {
        let _ = std::process::Command::new("xdotool")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Pointer button events the window has seen, counted in the capture
    /// phase — i.e. by GTK's own dispatch, before any widget can claim them.
    #[derive(Default)]
    struct SeenButtons {
        presses: Cell<u32>,
        releases: Cell<u32>,
    }

    /// A window on screen under real pointer control, and its X window as
    /// `xdotool` names it.
    ///
    /// Dropping it leaves the display the way the next test expects to find
    /// it (see the `Drop` impl).
    pub(crate) struct RealPointer {
        window: gtk::Window,
        /// The window's X window, as `xdotool` names it.
        xid: String,
        /// The button presses and releases the window has seen so far.
        seen: Rc<SeenButtons>,
    }

    impl RealPointer {
        /// Put `window` under real pointer control: title it `name` (made
        /// unique to this process), count every press and release GTK
        /// dispatches to it, present it, wait until it is mapped, find its X
        /// window by that title and raise it.
        ///
        /// What the test aims at must be in `window` already; waiting for its
        /// own layout is the caller's, since only the caller knows what "laid
        /// out" means for it.
        pub(crate) fn attach(window: &gtk::Window, name: &str) -> Self {
            let title = format!("trollshell-real-pointer-{}-{name}", std::process::id());
            window.set_title(Some(&title));

            // Count every press and release GTK dispatches to this window, on
            // the way down and whatever claims it later, so a test can wait for
            // the input it sent rather than for a clock.
            let seen: Rc<SeenButtons> = Rc::default();
            let counter = gtk::EventControllerLegacy::new();
            counter.set_propagation_phase(gtk::PropagationPhase::Capture);
            let counts = Rc::clone(&seen);
            counter.connect_event(move |_, event| {
                match event.event_type() {
                    gtk::gdk::EventType::ButtonPress => {
                        counts.presses.set(counts.presses.get() + 1);
                    }
                    gtk::gdk::EventType::ButtonRelease => {
                        counts.releases.set(counts.releases.get() + 1);
                    }
                    _ => {}
                }
                glib::Propagation::Proceed
            });
            window.add_controller(counter);
            window.present();
            wait_for("the window to map", || window.is_mapped(), String::new);

            let pattern = format!("^{title}$");
            let search = || {
                let found = std::process::Command::new("xdotool")
                    .args(["search", "--onlyvisible", "--name", &pattern])
                    .output()
                    .expect("xdotool runs");
                String::from_utf8_lossy(&found.stdout)
                    .lines()
                    .next()
                    .map(|line| line.trim().to_owned())
                    .filter(|line| !line.is_empty())
            };
            let xid = std::cell::RefCell::new(None);
            wait_for(
                "the window's X window",
                || {
                    *xid.borrow_mut() = search();
                    xid.borrow().is_some()
                },
                || format!("no mapped window titled {title}"),
            );
            let xid = xid.into_inner().expect("found above");
            // Nothing else should be over it, but a test that left a window up
            // must not be able to take these clicks.
            xdotool(&["windowraise", &xid]);
            pump();
            Self {
                window: window.clone(),
                xid,
                seen,
            }
        }

        /// Run one `xdotool` command line, then wait until the window has seen
        /// `presses` more button presses and `releases` more releases than
        /// before it — i.e. until GTK has dispatched them, along with whatever
        /// runs inside that dispatch (a button's `clicked`, a drag tracker's
        /// release).
        pub(crate) fn send(&self, args: &[&str], presses: u32, releases: u32) {
            let (want_presses, want_releases) = (
                self.seen.presses.get() + presses,
                self.seen.releases.get() + releases,
            );
            xdotool(args);
            wait_for(
                &format!("the input `xdotool {}` sent", args.join(" ")),
                || {
                    self.seen.presses.get() >= want_presses
                        && self.seen.releases.get() >= want_releases
                },
                || {
                    format!(
                        "{} of {want_presses} presses and {} of {want_releases} releases",
                        self.seen.presses.get(),
                        self.seen.releases.get(),
                    )
                },
            );
        }

        /// The point `(fx, fy)` of the way across `target` (`(0.5, 0.5)` is its
        /// centre), in its X window's pixels: the window's own coordinates,
        /// offset by wherever GTK put the window widget inside its surface and
        /// scaled to device pixels.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "a widget's size and position are small pixel counts"
        )]
        fn point_in(&self, target: &impl IsA<gtk::Widget>, (fx, fy): (f32, f32)) -> [String; 2] {
            let (w, h) = (target.width(), target.height());
            assert!(w > 0 && h > 0, "the target must be laid out (got {w}×{h})");
            let point = gtk::graphene::Point::new(w as f32 * fx, h as f32 * fy);
            let at = target
                .compute_point(&self.window, &point)
                .expect("the target is inside its window");
            let (dx, dy) = self.window.surface_transform();
            let scale = f64::from(self.window.scale_factor());
            [
                (((f64::from(at.x()) + dx) * scale).round() as i32).to_string(),
                (((f64::from(at.y()) + dy) * scale).round() as i32).to_string(),
            ]
        }

        /// Move the pointer to the point `(fx, fy)` of the way across `target`,
        /// then run `then` there, and wait for the `presses` and `releases`
        /// that sends.
        pub(crate) fn at(
            &self,
            target: &impl IsA<gtk::Widget>,
            fraction: (f32, f32),
            then: &[&str],
            presses: u32,
            releases: u32,
        ) {
            let [x, y] = self.point_in(target, fraction);
            let mut args = vec!["mousemove", "--window", self.xid.as_str(), &x, &y];
            args.extend_from_slice(then);
            self.send(&args, presses, releases);
        }
    }

    /// Leave the X display the way the next test expects to find it, pass or
    /// fail (#1416 review, MEDIUM 2): no button held — a panic between a
    /// `mousedown` and its `mouseup` would otherwise leave button 1 down for
    /// every later test in the binary — and the pointer parked in the far
    /// corner, not over the top-left one where every WM-less window maps, where
    /// it would hover-light whatever the next test put there. Nothing here may
    /// panic: this runs while a failed test unwinds.
    impl Drop for RealPointer {
        fn drop(&mut self) {
            xdotool_quiet(&["mouseup", "1"]);
            self.window.destroy();
            let corner = std::process::Command::new("xdotool")
                .arg("getdisplaygeometry")
                .output()
                .ok()
                .and_then(|out| {
                    let text = String::from_utf8_lossy(&out.stdout).into_owned();
                    let mut size = text.split_whitespace().map(str::parse::<u32>);
                    match (size.next(), size.next()) {
                        (Some(Ok(w)), Some(Ok(h))) if w > 0 && h > 0 => Some((w - 1, h - 1)),
                        _ => None,
                    }
                });
            if let Some((x, y)) = corner {
                xdotool_quiet(&["mousemove", &x.to_string(), &y.to_string()]);
            }
            pump();
        }
    }
}

/// One pass, on one thread, over what `app_meta`'s icon rule and
/// `companion`'s `fake_app_info` touch.
#[cfg(feature = "system-tests")]
fn warm_gio_types() {
    use hytte::gtk::gio;
    drop(gio::FileIcon::new(&gio::File::for_path("/")));
    drop(gio::ThemedIcon::new("warm"));
    drop(gio::AppInfo::create_from_commandline(
        "true",
        Some("warm"),
        gio::AppInfoCreateFlags::NONE,
    ));
}
