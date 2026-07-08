//! Shared partition logic for `Vec<Action>` on a notification or history
//! entry.
//!
//! The freedesktop notification spec reserves the action key `"default"`
//! for click-to-activate — the server is free not to render it as a
//! button. Both the toast overlay (`overlays::notifications::build_card`)
//! and the drawer history panel
//! (`panels::notifications::build_history_action_row`) render a
//! `.take(3)`-capped row of buttons from the same flat `Vec<Action>`
//! shape; without this filter a `default` action (often carrying an
//! empty label, since apps expect it to be invoked rather than shown)
//! burns a slot as a literal or blank "ghost" button. The toast overlay
//! additionally wires the `default` action to the card's body-click
//! gesture via [`default_action`].

use hytte::services::notifications::Action;

/// The reserved action key (freedesktop notification spec) that activates
/// on click rather than rendering as a button.
const DEFAULT_ACTION_KEY: &str = "default";

/// The `default` action, if `actions` carries one.
pub(crate) fn default_action(actions: &[Action]) -> Option<&Action> {
    actions.iter().find(|a| a.key == DEFAULT_ACTION_KEY)
}

/// Actions to render as buttons — everything except the reserved
/// `default` key, in original order. Callers still cap the count
/// themselves (e.g. `.take(3)`).
pub(crate) fn visible_actions(actions: &[Action]) -> impl Iterator<Item = &Action> {
    actions.iter().filter(|a| a.key != DEFAULT_ACTION_KEY)
}
