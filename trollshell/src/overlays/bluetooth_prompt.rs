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
//! The action, and a click anywhere on the toast's body (the spec's `default`
//! action), opens the drawer on that page on the focused output, by the
//! route the `open-page` keybind takes (`modal::open_on_focused` with
//! `focused_output::current`). The toast names the device by its alias, with
//! what the shell drops from any text a stranger chooses taken out, and its
//! address beside it, as the page's own banner does: the alias is a name the
//! device picks for itself. The toast is [`Urgency::Critical`], so
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

/// The one button's key and label.
const OPEN_KEY: &str = "open";
const OPEN_LABEL: &str = "Open Bluetooth";

/// The freedesktop notification spec's reserved action key for a click on the
/// notification itself. The toast overlay runs it on a body click and draws
/// no button for it (`components::notif_actions`). Without it a body click
/// only dismisses the toast, and with it the prompt's one toast, leaving the
/// prompt on no screen (#1629 review, F1).
const DEFAULT_KEY: &str = "default";

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
/// summary has two lines of about 40 characters, which also carry the
/// 17-character address and what the device asks for: a long alias would push
/// those out of view.
const ALIAS_CAP: usize = 24;

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

    wire_once(|| {
        glib::MainContext::default().spawn_local(reconcile(
            bluetooth::pair_prompts(),
            post_toast,
            |toast| notifications::dismiss(toast, CLOSED_BY_CALL),
        ));
    });
}

/// Run `spawn` on the first call only, for the process: there is one
/// reconciler, however many monitors [`install`] runs for and however many
/// hot-plugs re-install them. Without the latch, every `install` would start
/// another reconciler, each posting its own toast for the same prompt.
fn wire_once(spawn: impl FnOnce()) {
    if !WIRED.with(Cell::get) {
        WIRED.with(|wired| wired.set(true));
        spawn();
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
/// notification service registered). Everything it posts comes from
/// [`toast_args`]; this only hands it [`open_bluetooth_page`] and the post.
fn post_toast(prompt: &PairPrompt) -> Option<u32> {
    let ToastArgs {
        app_name,
        summary,
        body,
        urgency,
        actions,
    } = toast_args(prompt, open_bluetooth_page);
    notifications::post_local_until_closed(
        app_name,
        &summary,
        body,
        urgency,
        actions
            .into_iter()
            .map(|(key, label, open)| LocalAction::new(key, label, open))
            .collect(),
    )
}

/// What [`post_toast`] posts for one prompt.
struct ToastArgs<F> {
    app_name: &'static str,
    summary: String,
    body: &'static str,
    urgency: Urgency,
    /// Each action as its key, its label and the callback it runs.
    actions: Vec<(&'static str, &'static str, F)>,
}

/// The toast for `prompt`, short of posting it: what it says, its urgency,
/// and its actions, each running `open`. Pure, so a test can read the urgency
/// and run each action's callback with a recorder for `open`, where
/// [`post_toast`] passes [`open_bluetooth_page`].
///
/// `Critical`, so Do-Not-Disturb and a per-app mute let it through. Two
/// actions, both opening the page: the **Open Bluetooth** button, and
/// [`DEFAULT_KEY`], the body click, which draws no button.
fn toast_args<F: Clone>(prompt: &PairPrompt, open: F) -> ToastArgs<F> {
    ToastArgs {
        app_name: APP_NAME,
        summary: summary(prompt),
        body: BODY,
        urgency: Urgency::Critical,
        actions: vec![
            (OPEN_KEY, OPEN_LABEL, open.clone()),
            (DEFAULT_KEY, OPEN_LABEL, open),
        ],
    }
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
    let who = device_label(prompt);
    match prompt.kind {
        PromptKind::AuthorizeService => {
            let service = prompt.service.as_deref().map(one_line);
            format!(
                "{who} wants to use {}",
                service
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("a service")
            )
        }
        PromptKind::ConfirmPasskey => format!("{who} wants to pair with a code to confirm"),
        PromptKind::Authorize => format!("{who} wants to pair"),
        PromptKind::EnterPinCode => format!("{who} wants to pair with a PIN"),
        PromptKind::EnterPasskey => format!("{who} wants to pair with a passkey"),
    }
}

/// The device as the toast names it: [`device_name`] with the address beside
/// it, as `panels::bluetooth`'s banner names it. The alias is a name the
/// device picks for itself, so two devices can share one, and one can take
/// the name of a device the user knows; the address tells them apart. The
/// alias alone when there is no address, or when the alias already is it.
fn device_label(prompt: &PairPrompt) -> String {
    let name = device_name(&prompt.alias);
    let address = one_line(&prompt.address);
    if address.is_empty() || name == address {
        name
    } else {
        format!("{name} ({address})")
    }
}

/// The device's alias as [`one_line`] leaves it, capped at [`ALIAS_CAP`]
/// characters, or "A Bluetooth device" when nothing is left: an alias of
/// only invisible characters names nothing the user can read.
fn device_name(alias: &str) -> String {
    let name = one_line(alias);
    if name.is_empty() {
        return "A Bluetooth device".to_owned();
    }
    if name.chars().count() <= ALIAS_CAP {
        return name;
    }
    let mut capped: String = name.chars().take(ALIAS_CAP - 1).collect();
    capped.push('\u{2026}');
    capped
}

/// `text` as one plain line: a character the shell drops from text someone
/// else chose (the shared rule, [`is_invisible_or_control`]: controls, line
/// and paragraph separators, bidi overrides and isolates, zero-width
/// characters) is dropped, or becomes a space if it is whitespace, and runs of
/// whitespace become one space. `plugins::version`'s sanitiser applies the
/// same rule to a plugin's version string. A bidi override would otherwise
/// reverse the rest of the summary, and a newline would take one of its two
/// lines (#1629 review, F4).
///
/// [`is_invisible_or_control`]: hytte_plugin_proto::text::is_invisible_or_control
fn one_line(text: &str) -> String {
    let kept: String = text
        .chars()
        .filter_map(|c| {
            if !hytte_plugin_proto::text::is_invisible_or_control(c) {
                Some(c)
            } else if c.is_whitespace() {
                Some(' ')
            } else {
                None
            }
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{
        Action, PAGE_VISIBLE, Toasted, WIRED, apply, close_all, decide, device_name, reconcile,
        step, summary, toast_args, track, wire_once,
    };
    use crate::components::notif_actions;
    use crate::modal::Page;
    use hytte::futures_signals::signal::Mutable;
    use hytte::services::bluetooth::{PairPrompt, PromptKind};
    use hytte::services::notifications::{Action as NotificationAction, Urgency};
    use std::cell::{Cell, RefCell};
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

    /// Unplugging the output whose drawer showed the page must not leave the
    /// page "on screen" for good (#1629 review, F3): its stale entry would be
    /// counted back in by the next drawer to report, and no prompt would ever be
    /// toasted again. The reviewer's test, on this module's helpers.
    ///
    /// **Falsification:** delete `close_all`'s `SHOWS` clear and this fails
    /// at its first assertion: the page reads as on screen once the next
    /// drawer reports. Past it, the later prompt would get no toast, which
    /// the second assertion holds.
    #[test]
    fn an_unplugged_outputs_page_does_not_outlive_close_all() {
        close_all();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (post, dismiss) = recorders(&calls);
        let slot = Mutable::new(None::<PairPrompt>);
        let mut reconciler: Pin<Box<dyn Future<Output = ()>>> =
            Box::pin(reconcile(slot.signal_cloned(), post, dismiss));

        let hdmi = Mutable::new(Some(Page::Bluetooth));
        let mut old: Pin<Box<dyn Future<Output = ()>>> =
            Box::pin(track("HDMI-A-1".to_owned(), hdmi.signal()));
        poll(&mut old);
        poll(&mut reconciler);

        // HDMI-A-1 unplugged: its subscription goes, then the teardown.
        drop(old);
        close_all();
        let dp1 = Mutable::new(None::<Page>);
        let mut new: Pin<Box<dyn Future<Output = ()>>> =
            Box::pin(track("DP-1".to_owned(), dp1.signal()));
        poll(&mut new);
        poll(&mut reconciler);
        assert!(
            !PAGE_VISIBLE.with(|visible| visible.get()),
            "the unplugged output's page is on screen nowhere"
        );

        slot.set(Some(prompt(9, PromptKind::Authorize)));
        poll(&mut reconciler);
        assert_eq!(
            *calls.borrow(),
            [Call::Post(9)],
            "a later prompt gets its toast"
        );
    }

    /// However many times `install` runs (once per monitor, again on every
    /// hot-plug), the reconciler is spawned once (#1629 review, F3).
    ///
    /// **Falsification:** delete `wire_once`'s `WIRED` set and this fails:
    /// two spawns, i.e. two reconcilers, each posting a toast for the same
    /// prompt.
    #[test]
    fn the_reconciler_is_spawned_once_however_many_installs() {
        WIRED.with(|wired| wired.set(false));
        let spawned = Cell::new(0);
        wire_once(|| spawned.set(spawned.get() + 1));
        wire_once(|| spawned.set(spawned.get() + 1));
        assert_eq!(spawned.get(), 1);
    }

    /// The toast's arguments: Critical, so it gets past Do-Not-Disturb and a
    /// per-app mute (M12), and two actions that both open the page, the
    /// button and the spec's `default` key that a body click runs (#1629
    /// review, F1). Each action's callback is run, with a recorder standing in
    /// for `open_bluetooth_page`. The keys are checked against the toast
    /// overlay's own reading of them (`components::notif_actions`), so the
    /// body click finds `default` and no button is drawn for it.
    ///
    /// **Falsification:** `Urgency::Normal` in `toast_args`, drop the
    /// `default` action, or give it a callback that does nothing, and this
    /// fails.
    #[test]
    fn the_toast_is_critical_and_a_body_click_opens_the_page() {
        let opened = Rc::new(Cell::new(0));
        let open = {
            let opened = Rc::clone(&opened);
            move || opened.set(opened.get() + 1)
        };
        let pending = prompt(1, PromptKind::ConfirmPasskey);
        let args = toast_args(&pending, open);

        assert_eq!(args.app_name, "Bluetooth");
        assert_eq!(args.urgency, Urgency::Critical);
        assert_eq!(args.summary, summary(&pending));
        assert_eq!(args.body, super::BODY);

        let shown: Vec<NotificationAction> = args
            .actions
            .iter()
            .map(|(key, label, _)| NotificationAction {
                key: (*key).to_owned(),
                label: (*label).to_owned(),
            })
            .collect();
        assert_eq!(
            notif_actions::default_action(&shown).map(|a| a.key.as_str()),
            Some("default"),
            "a body click finds an action to run"
        );
        let buttons: Vec<&str> = notif_actions::visible_actions(&shown)
            .map(|a| a.label.as_str())
            .collect();
        assert_eq!(buttons, ["Open Bluetooth"], "one button");

        for (key, _, open) in &args.actions {
            let before = opened.get();
            open();
            assert_eq!(opened.get(), before + 1, "{key:?} opens the page");
        }
    }

    /// The summary names the device, with its address beside the alias, and
    /// what it asks for: the service for a service request, the kind of
    /// pairing otherwise.
    #[test]
    fn the_summary_names_the_device_and_the_ask() {
        let cases = [
            (
                PromptKind::AuthorizeService,
                "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to use audio streaming (A2DP)",
            ),
            (
                PromptKind::ConfirmPasskey,
                "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to pair with a code to confirm",
            ),
            (
                PromptKind::Authorize,
                "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to pair",
            ),
            (
                PromptKind::EnterPinCode,
                "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to pair with a PIN",
            ),
            (
                PromptKind::EnterPasskey,
                "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to pair with a passkey",
            ),
        ];
        for (kind, want) in cases {
            assert_eq!(summary(&prompt(1, kind)), want, "{kind:?}");
        }
        let mut unnamed = prompt(1, PromptKind::AuthorizeService);
        unnamed.service = None;
        assert_eq!(
            summary(&unnamed),
            "Pixel 7 (AA:BB:CC:DD:EE:FF) wants to use a service"
        );
    }

    /// The address is shown once: not at all when there is none, and alone
    /// when the alias already is the address.
    #[test]
    fn the_address_is_not_shown_twice_or_empty() {
        let mut p = prompt(1, PromptKind::Authorize);
        p.address = String::new();
        assert_eq!(summary(&p), "Pixel 7 wants to pair");
        p.alias = "AA:BB:CC:DD:EE:FF".to_owned();
        p.address = "AA:BB:CC:DD:EE:FF".to_owned();
        assert_eq!(summary(&p), "AA:BB:CC:DD:EE:FF wants to pair");
    }

    /// A name a stranger chose cannot reorder the toast or hide in invisible
    /// characters (#1629 review, F4): a right-to-left override is taken out,
    /// and an alias of only zero-width characters names "A Bluetooth device",
    /// with the address beside it either way.
    ///
    /// **Falsification:** drop the shared predicate from `one_line` and the
    /// override survives; check emptiness before filtering and the zero-width
    /// alias shows as an empty name.
    #[test]
    fn a_spoofing_alias_is_cleaned_and_the_address_still_shows() {
        let mut p = prompt(1, PromptKind::Authorize);
        p.alias = "\u{202E}sdohpdaeH s'aknnA".to_owned();
        let shown = summary(&p);
        assert!(!shown.contains('\u{202E}'), "{shown:?}");
        assert_eq!(shown, "sdohpdaeH s'aknnA (AA:BB:CC:DD:EE:FF) wants to pair");

        p.alias = "\u{200B}\u{200D}\u{2060}".to_owned();
        assert_eq!(
            summary(&p),
            "A Bluetooth device (AA:BB:CC:DD:EE:FF) wants to pair"
        );
        assert_eq!(device_name("\u{200B}"), "A Bluetooth device");
        assert_eq!(device_name("\u{202E}x"), "x");
    }

    /// The alias is put on one line and capped, and an empty one still names
    /// something.
    #[test]
    fn the_alias_is_one_capped_line() {
        assert_eq!(device_name("Pixel\n7\tPro "), "Pixel 7 Pro");
        assert_eq!(device_name("Pixel\u{2028}7"), "Pixel 7");
        assert_eq!(device_name("  "), "A Bluetooth device");
        let long = "x".repeat(100);
        let capped = device_name(&long);
        assert_eq!(capped.chars().count(), super::ALIAS_CAP);
        assert!(capped.ends_with('\u{2026}'), "{capped}");
        let exact = "y".repeat(super::ALIAS_CAP);
        assert_eq!(device_name(&exact), exact, "one that fits is left alone");
    }
}
