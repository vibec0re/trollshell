//! A toast for a Bluetooth prompt nobody can see (#1608).
//!
//! The Bluetooth agent's prompts — an incoming pairing request, and since
//! #1603 an untrusted device asking to use a service — are drawn only on the
//! Bluetooth drawer page (`panels::bluetooth`'s banner). With that page
//! closed, bluetoothd's agent timeout (60 s, `REQUEST_TIMEOUT` in its
//! `src/agent.c`) runs out with nothing on screen, and the request is refused
//! without the user ever having seen it.
//!
//! So while a prompt is pending and no output's drawer shows the Bluetooth
//! page, this module posts one shell toast with an **Open Bluetooth** action.
//! The action opens the drawer on that page on the focused output, by the
//! route the `open-page` keybind takes (`modal::open_on_focused` with
//! `focused_output::current`). The toast is [`Urgency::Critical`], so
//! Do-Not-Disturb and a per-app mute let it through, and it goes through
//! [`notifications::post_local_until_closed`], so nothing closes it on a
//! timer: this module does, with reason 3 (closed by a call),
//!
//! - when the prompt ends: answered, cancelled, or timed out (the prompt slot
//!   goes back to `None`);
//! - when another prompt replaces it (a different [`PairPrompt::id`]), which
//!   gets a toast of its own if the page is still closed;
//! - when the Bluetooth page comes on screen, which then shows the prompt.
//!
//! **One toast per prompt.** A prompt whose toast has closed, because the page
//! opened or because the user dismissed it, is not toasted again, and nothing
//! is posted while the page is on screen. A prompt that arrived while the page
//! was showing and is still waiting when the page closes gets its one toast
//! then: by then it is a prompt nobody can see. `decide` is the whole rule.
//!
//! # Shape
//!
//! The OSD's (`overlays::osd`): [`install`] runs per monitor and subscribes
//! that monitor's `modal::drawer_page_signal`, keeping "this drawer shows the
//! Bluetooth page" per monitor key; [`close_all`] aborts those subscriptions
//! on hot-plug, before the re-install. The OR over every monitor is one
//! `Mutable<bool>`, and the first [`install`] spawns the one reconciler that
//! reads it beside `bluetooth::pair_prompts()`. That reconciler is a
//! process-lifetime singleton, so what it has posted survives a hot-plug.
//!
//! No widget lives here, so there is no `bind`: the toast itself is drawn by
//! `overlays::notifications`, like every other notification.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::glib;
use hytte::prelude::*;
use hytte::services::bluetooth::{self, PairPrompt, PromptKind};
use hytte::services::notifications::{self, LocalAction, Urgency};

use crate::components::focused_output;
use crate::components::monitor_key::monitor_key;
use crate::modal::{self, Page};

/// The toast's app name: the Bluetooth page's own failure toasts use it too,
/// so the two read as one source in the history.
const APP_NAME: &str = "Bluetooth";

/// The one action's key and label.
const OPEN_KEY: &str = "open";
const OPEN_LABEL: &str = "Open Bluetooth";

/// The toast's body: one line, the same for every kind of prompt. The summary
/// says what is being asked.
const BODY: &str = "Answer on the Bluetooth page before it times out.";

/// The freedesktop notification spec's `NotificationClosed` reason 3, "the
/// notification was closed by a call to `CloseNotification`": the toast went
/// because what it reported ended, not because the user dismissed it (2) or
/// it expired (1).
const CLOSED_BY_CALL: u32 = 3;

/// How many characters of the device's alias the summary shows. The alias is
/// a name the device picks for itself, up to 248 bytes, and the toast's
/// summary has two lines: a long one would push what the device is asking for
/// out of view.
const ALIAS_CAP: usize = 40;

thread_local! {
    /// Per monitor key ([`monitor_key`]): whether that output's drawer shows
    /// the Bluetooth page. Written by each monitor's [`track`] subscription,
    /// cleared by [`close_all`].
    static SHOWS: RefCell<BTreeMap<String, bool>> = const { RefCell::new(BTreeMap::new()) };

    /// Each monitor's [`track`] subscription, so [`close_all`] can abort it.
    /// Per install, so it MUST be aborted on teardown, as the OSD's
    /// `drawer_sub` is, or one accumulates per hot-plug.
    static SUBS: RefCell<BTreeMap<String, glib::JoinHandle<()>>> =
        const { RefCell::new(BTreeMap::new()) };

    /// The OR of [`SHOWS`]: the Bluetooth page is on screen on some output.
    static PAGE_VISIBLE: Mutable<bool> = Mutable::new(false);

    /// Set by the first [`install`], which spawns [`reconcile`] once for
    /// the process.
    static WIRED: Cell<bool> = const { Cell::new(false) };
}

/// Track whether `monitor`'s drawer shows the Bluetooth page, and on the
/// first call, start the reconciler that posts and closes the toast. Called
/// per monitor from the `monitors_changed` loop, after [`close_all`].
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);
    let sub = glib::MainContext::default()
        .spawn_local(track(key.clone(), modal::drawer_page_signal(monitor)));
    // The displaced handle leaves the `SUBS` borrow before it is aborted.
    if let Some(old) = SUBS.with(|subs| subs.borrow_mut().insert(key, sub)) {
        old.abort();
    }

    if !WIRED.with(Cell::get) {
        WIRED.with(|wired| wired.set(true));
        glib::MainContext::default().spawn_local(reconcile(
            bluetooth::pair_prompts(),
            post_toast,
            |toast| notifications::dismiss(toast, CLOSED_BY_CALL),
        ));
    }
}

/// Abort every monitor's subscription and forget what each drawer showed.
/// Called on hot-plug before the re-install, after `modal::close_all` has
/// closed every drawer, so the page is on screen nowhere until a fresh
/// [`install`]'s subscription says otherwise. The reconciler keeps running.
pub fn close_all() {
    let subs = SUBS.with(|subs| std::mem::take(&mut *subs.borrow_mut()));
    for sub in subs.into_values() {
        sub.abort();
    }
    SHOWS.with(|shows| shows.borrow_mut().clear());
    PAGE_VISIBLE.with(|visible| visible.set_neq(false));
}

/// Follow one drawer's page into [`SHOWS`] under `key`, and the OR over every
/// drawer into [`PAGE_VISIBLE`]. A future rather than a spawn, so a test can
/// poll it with a plain signal for `pages`.
fn track(
    key: String,
    pages: impl Signal<Item = Option<Page>> + 'static,
) -> impl Future<Output = ()> + 'static {
    pages.for_each(move |page| {
        set_shows(&key, page == Some(Page::Bluetooth));
        std::future::ready(())
    })
}

/// Record whether the drawer under `key` shows the Bluetooth page, then
/// publish whether any drawer does. The `SHOWS` borrow ends before the
/// publish wakes anyone.
fn set_shows(key: &str, shows: bool) {
    let any = SHOWS.with(|map| {
        let mut map = map.borrow_mut();
        map.insert(key.to_owned(), shows);
        map.values().any(|&shows| shows)
    });
    PAGE_VISIBLE.with(|visible| visible.set_neq(any));
}

/// Whether the Bluetooth page is on screen on any output.
fn page_visible() -> impl Signal<Item = bool> + 'static {
    PAGE_VISIBLE.with(|visible| visible.signal())
}

/// The reconciler: on every change of the pending prompt or of the page's
/// visibility, take the [`step`] that [`decide`] calls for. `post` and `dismiss`
/// are the two notification calls, and `prompts` the agent's prompt slot;
/// [`install`] passes the real ones, a test passes recorders and a
/// `Mutable`. The visibility is always [`page_visible`]'s, the one [`track`]
/// writes.
fn reconcile<S, P, D>(prompts: S, mut post: P, mut dismiss: D) -> impl Future<Output = ()> + 'static
where
    S: Signal<Item = Option<PairPrompt>> + 'static,
    P: FnMut(&PairPrompt) -> Option<u32> + 'static,
    D: FnMut(u32) + 'static,
{
    let mut toasted = None;
    map_ref! {
        let prompt = prompts,
        let visible = page_visible() =>
        (prompt.clone(), *visible)
    }
    .for_each(move |(prompt, visible)| {
        toasted = step(prompt.as_ref(), visible, toasted, &mut post, &mut dismiss);
        std::future::ready(())
    })
}

/// Post the toast for `prompt`, returning its notification id (`None` with no
/// notification service registered).
fn post_toast(prompt: &PairPrompt) -> Option<u32> {
    notifications::post_local_until_closed(
        APP_NAME,
        &summary(prompt),
        BODY,
        Urgency::Critical,
        vec![LocalAction::new(OPEN_KEY, OPEN_LABEL, open_bluetooth_page)],
    )
}

/// The toast action: open the drawer on the Bluetooth page on the focused
/// output, else on any output with a drawer, as the `open-page` keybind does.
/// Runs on the GTK main thread, where the toast's button handler calls it (see
/// `notifications::LocalActionCallback`).
fn open_bluetooth_page() {
    modal::open_on_focused(focused_output::current().as_deref(), Page::Bluetooth);
}

/// What has been posted for the current prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Toasted {
    /// The [`PairPrompt::id`] the toast was posted for.
    prompt: u64,
    /// The toast's notification id while it may still be up. `None` once
    /// this module has closed it, or if posting found no notification
    /// service. The user may have dismissed it in the meantime; a dismiss for
    /// an id that has closed does nothing, so that needs no tracking here.
    toast: Option<u32>,
}

/// What to do about the toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    /// Post a toast for the pending prompt.
    Post,
    /// Close this toast.
    Dismiss(u32),
    /// Close this toast, which was for an earlier prompt, and post one for
    /// the pending prompt.
    Replace(u32),
}

/// The rule. `prompt` is the pending prompt, `page_visible` whether the
/// Bluetooth page is on screen on any output, and `toasted` what has been
/// posted so far.
///
/// - No prompt: close a toast that is still up.
/// - The page is on screen: close a toast that is still up, whichever prompt
///   it was for; the page shows the pending one.
/// - The prompt has had its toast: nothing, whether or not that toast is
///   still up (a re-emission of the same prompt, or one whose toast closed).
/// - Otherwise: post one, closing an earlier prompt's toast first.
fn decide(prompt: Option<&PairPrompt>, page_visible: bool, toasted: Option<Toasted>) -> Action {
    let up = toasted.and_then(|t| t.toast);
    match prompt {
        None => up.map_or(Action::Nothing, Action::Dismiss),
        Some(_) if page_visible => up.map_or(Action::Nothing, Action::Dismiss),
        Some(p) if toasted.is_some_and(|t| t.prompt == p.id) => Action::Nothing,
        Some(_) => up.map_or(Action::Post, Action::Replace),
    }
}

/// Carry out `action` with `post` and `dismiss`, and return what has been
/// posted for `prompt` afterwards. A record for any prompt but the pending one
/// is dropped: prompt ids are never reused, so it can never match again.
fn apply(
    action: Action,
    prompt: Option<&PairPrompt>,
    toasted: Option<Toasted>,
    post: &mut impl FnMut(&PairPrompt) -> Option<u32>,
    dismiss: &mut impl FnMut(u32),
) -> Option<Toasted> {
    let current = toasted.filter(|t| prompt.is_some_and(|p| p.id == t.prompt));
    match action {
        Action::Nothing => current,
        Action::Dismiss(toast) => {
            dismiss(toast);
            current.map(|t| Toasted { toast: None, ..t })
        }
        Action::Post | Action::Replace(_) => {
            if let Action::Replace(earlier) = action {
                dismiss(earlier);
            }
            let prompt = prompt?;
            Some(Toasted {
                prompt: prompt.id,
                toast: post(prompt),
            })
        }
    }
}

/// [`decide`], then [`apply`]: one reconciler tick.
fn step(
    prompt: Option<&PairPrompt>,
    page_visible: bool,
    toasted: Option<Toasted>,
    post: &mut impl FnMut(&PairPrompt) -> Option<u32>,
    dismiss: &mut impl FnMut(u32),
) -> Option<Toasted> {
    let action = decide(prompt, page_visible, toasted);
    apply(action, prompt, toasted, post, dismiss)
}

/// The toast's summary: the device, and what it asks for. Plain text — the
/// toast's summary label renders no markup.
fn summary(prompt: &PairPrompt) -> String {
    let who = device_name(&prompt.alias);
    match prompt.kind {
        PromptKind::AuthorizeService => format!(
            "{who} wants to use {}",
            prompt.service.as_deref().unwrap_or("a service")
        ),
        PromptKind::ConfirmPasskey => format!("{who} wants to pair with a code to confirm"),
        PromptKind::Authorize => format!("{who} wants to pair"),
        PromptKind::EnterPinCode => format!("{who} wants to pair with a PIN"),
        PromptKind::EnterPasskey => format!("{who} wants to pair with a passkey"),
    }
}

/// The device's alias on one line, capped at [`ALIAS_CAP`] characters. A
/// newline in it would take one of the summary's two lines, and a long one
/// would push the rest out of view.
fn device_name(alias: &str) -> String {
    let one_line = alias.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "A Bluetooth device".to_owned();
    }
    if one_line.chars().count() <= ALIAS_CAP {
        return one_line;
    }
    let mut capped: String = one_line.chars().take(ALIAS_CAP - 1).collect();
    capped.push('\u{2026}');
    capped
}

#[cfg(test)]
mod tests {
    use super::{
        Action, PAGE_VISIBLE, Toasted, apply, close_all, decide, device_name, reconcile, step,
        summary, track,
    };
    use crate::modal::Page;
    use hytte::futures_signals::signal::Mutable;
    use hytte::services::bluetooth::{PairPrompt, PromptKind};
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    fn prompt(id: u64, kind: PromptKind) -> PairPrompt {
        PairPrompt {
            id,
            device_path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_owned(),
            alias: "Pixel 7".to_owned(),
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            passkey: None,
            kind,
            service: (kind == PromptKind::AuthorizeService)
                .then(|| "audio streaming (A2DP)".to_owned()),
        }
    }

    /// A toast posted for `prompt`, which may still be up.
    const fn up(prompt: u64, toast: u32) -> Toasted {
        Toasted {
            prompt,
            toast: Some(toast),
        }
    }

    /// A toast posted for `prompt`, which this module has closed.
    const fn closed(prompt: u64) -> Toasted {
        Toasted {
            prompt,
            toast: None,
        }
    }

    /// One row of [`every_transition`]: its name, the pending prompt, whether
    /// the page is on screen, what has been posted, and the action wanted.
    type Row<'a> = (
        &'static str,
        Option<&'a PairPrompt>,
        bool,
        Option<Toasted>,
        Action,
    );

    /// Every transition of the rule, one row each.
    ///
    /// **Falsification:** each arm of `decide` has a row that fails without
    /// it — drop the `page_visible` guard (rows 2 and 5), the same-prompt
    /// arm (rows 6 and 7), the `None` arm's dismiss (row 3), or turn
    /// `Replace` into `Post` (row 4).
    #[test]
    fn every_transition() {
        let p1 = prompt(1, PromptKind::AuthorizeService);
        let p2 = prompt(2, PromptKind::ConfirmPasskey);
        let rows: [Row; 11] = [
            (
                "1. a prompt arrives with the page closed: post",
                Some(&p1),
                false,
                None,
                Action::Post,
            ),
            (
                "2. the page opens: dismiss",
                Some(&p1),
                true,
                Some(up(1, 10)),
                Action::Dismiss(10),
            ),
            (
                "3. the prompt is answered (or cancelled, or times out): dismiss",
                None,
                false,
                Some(up(1, 10)),
                Action::Dismiss(10),
            ),
            (
                "4. the prompt is replaced by a new id: replace",
                Some(&p2),
                false,
                Some(up(1, 10)),
                Action::Replace(10),
            ),
            (
                "5. the page is open when the prompt arrives: nothing",
                Some(&p1),
                true,
                None,
                Action::Nothing,
            ),
            (
                "6. the same prompt is re-emitted: nothing",
                Some(&p1),
                false,
                Some(up(1, 10)),
                Action::Nothing,
            ),
            (
                "7. the page closes again on a prompt whose toast closed: nothing",
                Some(&p1),
                false,
                Some(closed(1)),
                Action::Nothing,
            ),
            (
                "8. answered with no toast up: nothing",
                None,
                false,
                Some(closed(1)),
                Action::Nothing,
            ),
            (
                "9. replaced while the page is open: dismiss the earlier toast only",
                Some(&p2),
                true,
                Some(up(1, 10)),
                Action::Dismiss(10),
            ),
            (
                "10. a new prompt after an earlier one's toast closed: post",
                Some(&p2),
                false,
                Some(closed(1)),
                Action::Post,
            ),
            (
                "11. the page closes on a prompt that was never toasted: post",
                Some(&p2),
                false,
                None,
                Action::Post,
            ),
        ];
        for (name, pending, visible, toasted, want) in rows {
            assert_eq!(decide(pending, visible, toasted), want, "{name}");
        }
    }

    /// What one tick calls, and what it records, collected for an assertion.
    #[derive(Debug, PartialEq)]
    enum Call {
        Post(u64),
        Dismiss(u32),
    }

    /// No calls.
    const NONE: [Call; 0] = [];

    /// A post that hands out notification ids from 100 up, and a dismiss,
    /// both recording into `calls`.
    fn recorders(
        calls: &Rc<RefCell<Vec<Call>>>,
    ) -> (
        impl FnMut(&PairPrompt) -> Option<u32> + use<>,
        impl FnMut(u32) + use<>,
    ) {
        let mut next = 100;
        let (posts, dismisses) = (Rc::clone(calls), Rc::clone(calls));
        let post = move |p: &PairPrompt| {
            posts.borrow_mut().push(Call::Post(p.id));
            next += 1;
            Some(next - 1)
        };
        let dismiss = move |toast| dismisses.borrow_mut().push(Call::Dismiss(toast));
        (post, dismiss)
    }

    /// `apply` makes the calls the action names, in order (an earlier
    /// prompt's toast closes before the next one is posted), and records the
    /// id the post returned against the prompt it was posted for.
    ///
    /// **Falsification:** post before dismissing in `Replace`, record the
    /// earlier prompt's id, or keep the toast id after a `Dismiss`, and this
    /// fails.
    #[test]
    fn apply_calls_and_records() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut post, mut dismiss) = recorders(&calls);
        let p1 = prompt(1, PromptKind::Authorize);
        let p2 = prompt(2, PromptKind::Authorize);

        let after = apply(Action::Post, Some(&p1), None, &mut post, &mut dismiss);
        assert_eq!(after, Some(up(1, 100)));
        let after = apply(
            Action::Replace(100),
            Some(&p2),
            after,
            &mut post,
            &mut dismiss,
        );
        assert_eq!(after, Some(up(2, 101)));
        let after = apply(
            Action::Dismiss(101),
            Some(&p2),
            after,
            &mut post,
            &mut dismiss,
        );
        assert_eq!(
            after,
            Some(closed(2)),
            "the prompt keeps its record, the toast is gone"
        );
        let after = apply(Action::Nothing, None, after, &mut post, &mut dismiss);
        assert_eq!(after, None, "an ended prompt's record goes with it");
        assert_eq!(
            *calls.borrow(),
            [
                Call::Post(1),
                Call::Dismiss(100),
                Call::Post(2),
                Call::Dismiss(101)
            ]
        );
    }

    /// A post that found no notification service still records the prompt,
    /// so the next tick does not post for it again.
    #[test]
    fn a_post_with_no_service_is_not_retried_for_that_prompt() {
        let mut posts = 0;
        let mut post = |_: &PairPrompt| {
            posts += 1;
            None
        };
        let mut dismiss = |_| panic!("nothing to dismiss");
        let p1 = prompt(1, PromptKind::Authorize);
        let after = step(Some(&p1), false, None, &mut post, &mut dismiss);
        assert_eq!(after, Some(closed(1)));
        let after = step(Some(&p1), false, after, &mut post, &mut dismiss);
        assert_eq!(after, Some(closed(1)));
        assert_eq!(posts, 1);
    }

    /// Poll `future` once with a waker that does nothing: the test re-polls by
    /// hand after every change, so nothing needs waking.
    fn poll(future: &mut Pin<Box<dyn Future<Output = ()>>>) {
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            matches!(future.as_mut().poll(&mut cx), Poll::Pending),
            "a subscription never finishes"
        );
    }

    /// The production wiring short of the two services: [`track`] for two
    /// outputs' drawers feeding [`page_visible`], and [`reconcile`] reading
    /// it beside a prompt slot, with recorders for the notification calls.
    /// This is every transition again, through the real subscriptions.
    ///
    /// **Falsification:** hand `reconcile` a constant `false` in place of
    /// `page_visible()`, have `track` count any page as the Bluetooth page,
    /// or OR only the last drawer `set_shows` heard from, and this fails.
    #[test]
    fn the_wiring_posts_and_closes_through_the_real_subscriptions() {
        // Fresh per-thread state, whatever ran on this thread before.
        close_all();

        let dp1 = Mutable::new(None::<Page>);
        let hdmi = Mutable::new(None::<Page>);
        let slot = Mutable::new(None::<PairPrompt>);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (post, dismiss) = recorders(&calls);

        let mut tracked: Vec<Pin<Box<dyn Future<Output = ()>>>> = vec![
            Box::pin(track("DP-1".to_owned(), dp1.signal())),
            Box::pin(track("HDMI-A-1".to_owned(), hdmi.signal())),
        ];
        let mut reconciler: Pin<Box<dyn Future<Output = ()>>> =
            Box::pin(reconcile(slot.signal_cloned(), post, dismiss));
        // The drawers first, so the reconciler reads what they publish.
        let mut tick = || {
            for future in &mut tracked {
                poll(future);
            }
            poll(&mut reconciler);
        };
        let take = |calls: &Rc<RefCell<Vec<Call>>>| std::mem::take(&mut *calls.borrow_mut());

        tick();
        assert_eq!(take(&calls), NONE, "no prompt, no toast");

        slot.set(Some(prompt(1, PromptKind::AuthorizeService)));
        tick();
        assert_eq!(take(&calls), [Call::Post(1)], "page closed: post");

        slot.set(Some(prompt(1, PromptKind::AuthorizeService)));
        tick();
        assert_eq!(take(&calls), NONE, "the same prompt again: nothing");

        hdmi.set(Some(Page::Audio));
        tick();
        assert_eq!(take(&calls), NONE, "another page is not the Bluetooth page");

        hdmi.set(Some(Page::Bluetooth));
        tick();
        assert_eq!(
            take(&calls),
            [Call::Dismiss(100)],
            "the page opens: dismiss"
        );

        hdmi.set(None);
        tick();
        assert_eq!(take(&calls), NONE, "its toast has been shown: not again");

        // The page back on HDMI-A-1, then DP-1's drawer changing to another
        // page: DP-1 is the last drawer heard from, and the page still shows
        // on HDMI-A-1. Two ticks, so DP-1's change lands after HDMI-A-1's.
        hdmi.set(Some(Page::Bluetooth));
        tick();
        dp1.set(Some(Page::Media));
        tick();
        assert_eq!(take(&calls), NONE, "no toast is up to close");
        slot.set(Some(prompt(2, PromptKind::ConfirmPasskey)));
        tick();
        assert_eq!(
            take(&calls),
            NONE,
            "another output's drawer does not hide the page on this one"
        );

        hdmi.set(None);
        tick();
        assert_eq!(
            take(&calls),
            [Call::Post(2)],
            "the page closes on a prompt that was never toasted: post"
        );

        slot.set(Some(prompt(3, PromptKind::EnterPinCode)));
        tick();
        assert_eq!(
            take(&calls),
            [Call::Dismiss(101), Call::Post(3)],
            "replaced: the earlier toast closes, the new prompt gets one"
        );

        slot.set(None);
        tick();
        assert_eq!(take(&calls), [Call::Dismiss(102)], "answered: dismiss");

        dp1.set(Some(Page::Bluetooth));
        tick();
        slot.set(Some(prompt(4, PromptKind::Authorize)));
        tick();
        assert_eq!(
            take(&calls),
            NONE,
            "the page is open when it arrives: nothing"
        );

        dp1.set(None);
        tick();
        assert_eq!(take(&calls), [Call::Post(4)], "the page closes on it: post");
    }

    /// [`close_all`] forgets every drawer, so the page reads as on screen
    /// nowhere until a fresh [`track`] says otherwise.
    #[test]
    fn close_all_forgets_every_drawer() {
        close_all();
        let dp1 = Mutable::new(Some(Page::Bluetooth));
        let mut tracked: Pin<Box<dyn Future<Output = ()>>> =
            Box::pin(track("DP-1".to_owned(), dp1.signal()));
        poll(&mut tracked);
        assert!(PAGE_VISIBLE.with(|visible| visible.get()));
        close_all();
        assert!(!PAGE_VISIBLE.with(|visible| visible.get()));
    }

    /// The summary names the device and what it asks for: the service for a
    /// service request, the kind of pairing otherwise.
    #[test]
    fn the_summary_names_the_device_and_the_ask() {
        let cases = [
            (
                PromptKind::AuthorizeService,
                "Pixel 7 wants to use audio streaming (A2DP)",
            ),
            (
                PromptKind::ConfirmPasskey,
                "Pixel 7 wants to pair with a code to confirm",
            ),
            (PromptKind::Authorize, "Pixel 7 wants to pair"),
            (PromptKind::EnterPinCode, "Pixel 7 wants to pair with a PIN"),
            (
                PromptKind::EnterPasskey,
                "Pixel 7 wants to pair with a passkey",
            ),
        ];
        for (kind, want) in cases {
            assert_eq!(summary(&prompt(1, kind)), want, "{kind:?}");
        }
        let mut unnamed = prompt(1, PromptKind::AuthorizeService);
        unnamed.service = None;
        assert_eq!(summary(&unnamed), "Pixel 7 wants to use a service");
    }

    /// The alias is put on one line and capped, and an empty one still names
    /// something.
    #[test]
    fn the_alias_is_one_capped_line() {
        assert_eq!(device_name("Pixel\n7\tPro "), "Pixel 7 Pro");
        assert_eq!(device_name("  "), "A Bluetooth device");
        let long = "x".repeat(100);
        let capped = device_name(&long);
        assert_eq!(capped.chars().count(), super::ALIAS_CAP);
        assert!(capped.ends_with('\u{2026}'), "{capped}");
        let exact = "y".repeat(super::ALIAS_CAP);
        assert_eq!(device_name(&exact), exact, "one that fits is left alone");
    }
}
