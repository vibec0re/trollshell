//! Crate-wide test helpers (test builds only).

/// Call first in a plain `#[test]` that reaches GIO's lazily-created types or
/// defaults (`gio::File::for_path` → the default `GVfs`, `FileIcon`,
/// `ThemedIcon`, `AppInfo::create_from_commandline`) (#1461, #1463).
///
/// In the `system-tests` build the same binary's `#[gtk::test]`s run `gtk::init`
/// on the GTK test thread, and two threads first-initialising the same GLib
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
