//! One rule for a container that takes room only while something in it does:
//! "visible while a child's own `visible` flag is set". The bar's `group` and
//! the tray's box both use it (#1616), so the two cannot drift apart.

use hytte::gtk::{self, prelude::*};

/// Show `widget` exactly while one of its children is marked visible.
///
/// Reads each child's own flag (`get_visible`), not `is_visible`, which is
/// also false while an ancestor is hidden. `widget` itself is hidden exactly
/// when this has to bring it back, so `is_visible` would keep it hidden for
/// good.
pub(crate) fn show_while_a_child_is(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    let any = std::iter::successors(widget.first_child(), WidgetExt::next_sibling)
        .any(|child| child.get_visible());
    widget.set_visible(any);
}
