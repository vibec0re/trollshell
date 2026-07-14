//! Pure parsers for untrusted `com.canonical.dbusmenu` payloads and for the
//! `StatusNotifierItem` registration argument.
//!
//! Everything here is free of I/O: each function takes an in-memory
//! `zvariant` value (or a `&str`) fetched elsewhere and returns typed Rust
//! data, defaulting rather than panicking on malformed input. That makes the
//! whole module independently unit-testable in the hermetic (`cargo test`)
//! bucket — see the `tests` module below.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Structure, Value};

use super::types::{Menu, MenuEntry, MenuItem, ToggleType};

// ── DBusMenu layout parsing ───────────────────────────────────────────────────

/// Peel a `Value::Value` variant wrapper down to the value it carries.
///
/// `com.canonical.dbusmenu`'s `GetLayout` types each node's children as `av`
/// (array of variant). `zvariant::Array::from(Vec<Value>)` marks every
/// element whose *own* signature is `"v"` by boxing it in an extra
/// `Value::Value(inner)` layer (see `Value::new`) — which is exactly what a
/// real `av` deserializes to off the wire too. `Structure::try_from` only
/// matches `Value::Structure` directly, so without peeling this wrapper
/// first, every child structure fails to convert and is silently dropped
/// (issue #8: tray menus decode with zero children). Peel any (possibly
/// repeated) wrapping here; a value that arrives already unwrapped passes
/// through untouched.
fn unwrap_variant(value: OwnedValue) -> Result<OwnedValue> {
    let mut inner: Value<'static> = value.into();
    while let Value::Value(boxed) = inner {
        inner = *boxed;
    }
    OwnedValue::try_from(inner).context("re-owning unwrapped variant")
}

/// The concrete `com.canonical.dbusmenu` layout node `(i, a{sv}, av)` as it
/// deserializes off the wire.
///
/// `GetLayout` replies with signature `(u(ia{sv}av))` — a `u32` revision plus
/// this **concrete struct** (`i32` id, `a{sv}` properties, `av` children).
/// Issue #8: the old code destructured the reply as `(u32, OwnedValue)`
/// (signature `(uv)`), but zbus 5.x refuses to coerce a concrete struct into a
/// bare `Value`/`OwnedValue`, so *every* `GetLayout` failed uniformly with a
/// `Signature mismatch: got (u(ia{sv}av)), expected (uv)` error and the tray
/// fell back to the plain `ContextMenu`. Deserializing into this typed shape
/// (its derived `Type` signature is exactly `(ia{sv}av)`) matches the wire.
///
/// Children are `av` (array of variant); each variant wraps another
/// `(ia{sv}av)` node, surfaced here as an `OwnedValue` and re-parsed
/// recursively by [`parse_menu_entry`] (which peels any variant wrapper via
/// [`unwrap_variant`] before `Structure::try_from`).
#[derive(Debug, serde::Deserialize, zbus::zvariant::Type)]
pub(super) struct LayoutNode {
    id: i32,
    props: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

/// Parse the root [`LayoutNode`] (already deserialized from `GetLayout`'s
/// `(u(ia{sv}av))` reply) into a [`Menu`].
///
/// Never fails: malformed individual children are logged and skipped rather
/// than aborting the whole tree (the root's `id`/`props`/`children` are already
/// well-typed by the time they reach here).
pub(super) fn parse_layout_node(node: LayoutNode) -> Menu {
    let LayoutNode {
        id,
        props,
        children,
    } = node;

    let visible = bool_prop(&props, "visible", true);
    if !visible {
        // Return a menu with no items for invisible root (unlikely but safe).
        return Menu { id, items: vec![] };
    }

    let item_type = str_prop(&props, "type", "standard");
    if item_type == "separator" {
        // A root node that is a separator — return empty.
        return Menu { id, items: vec![] };
    }

    // Collect children into MenuEntry list.
    let mut items = Vec::new();
    for child in children {
        match parse_menu_entry(child) {
            Ok(Some(entry)) => items.push(entry),
            Ok(None) => {} // invisible / skipped
            Err(e) => tracing::debug!(error = %e, "skipping malformed menu entry"),
        }
    }

    Menu { id, items }
}

/// Parse one child value from the `av` children list into a `MenuEntry`.
/// Returns `Ok(None)` for invisible items.
fn parse_menu_entry(val: OwnedValue) -> Result<Option<MenuEntry>> {
    let val = unwrap_variant(val)?;
    let structure = Structure::try_from(val).context("menu entry not a structure")?;
    let mut fields = structure.into_fields();
    if fields.len() < 3 {
        return Err(anyhow!("menu entry has fewer than 3 fields"));
    }

    let id = i32::try_from(fields.remove(0)).context("entry id")?;

    let props_val = fields.remove(0);
    let props: HashMap<String, OwnedValue> =
        HashMap::try_from(OwnedValue::try_from(props_val).context("entry props to owned")?)
            .context("entry props")?;

    let children_val = fields.remove(0);
    let children_arr = zbus::zvariant::Array::try_from(
        OwnedValue::try_from(children_val).context("entry children to owned")?,
    )
    .context("entry children")?;

    let visible = bool_prop(&props, "visible", true);
    if !visible {
        return Ok(None);
    }

    let item_type = str_prop(&props, "type", "standard");
    if item_type == "separator" {
        return Ok(Some(MenuEntry::Separator));
    }

    let label = strip_accel(&str_prop(&props, "label", ""));
    let enabled = bool_prop(&props, "enabled", true);
    let icon_name = str_prop(&props, "icon-name", "");
    let toggle_type_str = str_prop(&props, "toggle-type", "");
    let toggle_type = match toggle_type_str.as_str() {
        "checkmark" => ToggleType::Checkmark,
        "radio" => ToggleType::Radio,
        _ => ToggleType::None,
    };
    let toggle_state = i32_prop(&props, "toggle-state", -1);
    let children_display = str_prop(&props, "children-display", "");

    // Recurse into children when this is a submenu entry.
    let submenu = if children_display == "submenu" && !children_arr.is_empty() {
        let mut sub_items = Vec::new();
        for child_val in children_arr.iter() {
            let owned: OwnedValue = child_val
                .try_clone()
                .context("clone sub-child")?
                .try_into_owned()
                .context("sub-child to owned")?;
            match parse_menu_entry(owned) {
                Ok(Some(entry)) => sub_items.push(entry),
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "skipping malformed sub-menu entry"),
            }
        }
        Some(sub_items)
    } else {
        None
    };

    Ok(Some(MenuEntry::Item(MenuItem {
        id,
        label,
        enabled,
        icon_name,
        toggle_type,
        toggle_state,
        submenu,
    })))
}

// ── DBusMenu property helpers ─────────────────────────────────────────────────

fn str_prop(props: &HashMap<String, OwnedValue>, key: &str, default: &str) -> String {
    props
        .get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_else(|| default.to_string())
}

fn bool_prop(props: &HashMap<String, OwnedValue>, key: &str, default: bool) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
        .unwrap_or(default)
}

fn i32_prop(props: &HashMap<String, OwnedValue>, key: &str, default: i32) -> i32 {
    props
        .get(key)
        .and_then(|v| i32::try_from(v.try_clone().ok()?).ok())
        .unwrap_or(default)
}

/// Strip GTK/Qt accelerator markers from a menu label.
#[must_use]
///
/// Rules:
/// - `__` → `_` (escaped underscore)
/// - `_X` → `X` (accelerator shortcut, drop the `_`)
pub fn strip_accel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            match chars.peek() {
                Some('_') => {
                    out.push('_');
                    chars.next();
                }
                Some(_) => {
                    // Drop the underscore; next char is the accelerator letter.
                    // It will be pushed naturally in the next iteration.
                }
                None => {
                    // Trailing underscore — keep it.
                    out.push('_');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── StatusNotifierItem registration argument ──────────────────────────────────

/// Split the `RegisterStatusNotifierItem` argument into `(bus_name, object_path)`.
///
/// Per the SNI spec the argument may be either an object path (starts with
/// `/`) — in which case the bus name is the message sender — or a bus name,
/// in which case the item lives at the well-known `/StatusNotifierItem` path.
pub(super) fn parse_service(service: &str, sender: &str) -> (String, String) {
    if service.starts_with('/') {
        (sender.to_string(), service.to_string())
    } else {
        (service.to_string(), "/StatusNotifierItem".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    // ── Fixture builders ──────────────────────────────────────────────────────
    //
    // A DBusMenu node is `(i, a{sv}, av)`. `Value::from((i32, HashMap<String,
    // Value>, Vec<Value>))` produces exactly that shape: the map serialises to
    // `a{sv}` (string→variant) and the `Vec<Value>` to `av` (array of
    // variant-wrapped children) — the same representation zbus hands back from
    // `GetLayout`, so these hermetic fixtures exercise the real parse path.

    fn node_value(
        id: i32,
        props: Vec<(&str, Value<'static>)>,
        children: Vec<Value<'static>>,
    ) -> Value<'static> {
        let map: HashMap<String, Value<'static>> =
            props.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        Value::from((id, map, children))
    }

    fn node_owned(
        id: i32,
        props: Vec<(&str, Value<'static>)>,
        children: Vec<Value<'static>>,
    ) -> OwnedValue {
        node_value(id, props, children)
            .try_to_owned()
            .expect("node fixture must serialise")
    }

    /// Build a root [`LayoutNode`] directly — the deserialized shape
    /// `parse_layout_node` now takes. `props` become an `a{sv}` map; each child
    /// is an `OwnedValue` wrapping a `(ia{sv}av)` node (as `node_owned`
    /// produces), matching the `av` element shape real deserialization yields.
    fn layout_node(
        id: i32,
        props: Vec<(&str, Value<'static>)>,
        children: Vec<OwnedValue>,
    ) -> LayoutNode {
        let props: HashMap<String, OwnedValue> = props
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.try_to_owned().expect("prop fixture serialises")))
            .collect();
        LayoutNode {
            id,
            props,
            children,
        }
    }

    // ── strip_accel ───────────────────────────────────────────────────────────

    #[test]
    fn strip_accel_drops_accelerator_and_unescapes() {
        assert_eq!(strip_accel("_File"), "File");
        assert_eq!(strip_accel("Save _As"), "Save As");
        assert_eq!(strip_accel("a __ b"), "a _ b");
        assert_eq!(strip_accel("plain"), "plain");
        // A trailing underscore is preserved, not dropped.
        assert_eq!(strip_accel("trailing_"), "trailing_");
    }

    // ── property extractors: missing / wrong-typed values default gracefully ──

    #[test]
    fn str_prop_missing_and_wrong_type_default() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        // Garbage: a bool where a string is expected.
        props.insert(
            "label".to_string(),
            Value::from(true).try_to_owned().unwrap(),
        );
        assert_eq!(str_prop(&props, "label", "fallback"), "fallback");
        assert_eq!(str_prop(&props, "absent", "fallback"), "fallback");

        props.insert(
            "type".to_string(),
            Value::from("separator").try_to_owned().unwrap(),
        );
        assert_eq!(str_prop(&props, "type", "standard"), "separator");
    }

    #[test]
    fn bool_prop_missing_and_wrong_type_default() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        // Garbage: a string where a bool is expected → default, no panic.
        props.insert(
            "enabled".to_string(),
            Value::from("yes").try_to_owned().unwrap(),
        );
        assert!(bool_prop(&props, "enabled", true));
        assert!(!bool_prop(&props, "absent", false));

        props.insert(
            "visible".to_string(),
            Value::from(false).try_to_owned().unwrap(),
        );
        assert!(!bool_prop(&props, "visible", true));
    }

    #[test]
    fn i32_prop_missing_and_wrong_type_default() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "toggle-state".to_string(),
            Value::from("nope").try_to_owned().unwrap(),
        );
        assert_eq!(i32_prop(&props, "toggle-state", -1), -1);
        assert_eq!(i32_prop(&props, "absent", 7), 7);

        props.insert(
            "toggle-state".to_string(),
            Value::from(1i32).try_to_owned().unwrap(),
        );
        assert_eq!(i32_prop(&props, "toggle-state", -1), 1);
    }

    // ── parse_menu_entry: structural guards (untrusted children) ──────────────
    //
    // The root node is now a typed `LayoutNode` (always 3 fields), so the
    // malformed-shape guards live at the child level — where the untrusted
    // `av` elements land.

    #[test]
    fn menu_entry_rejects_fewer_than_three_fields() {
        // A one-field structure `(i)` must Err, not panic.
        let short = Value::from((5i32,)).try_to_owned().unwrap();
        assert!(parse_menu_entry(short).is_err());
    }

    #[test]
    fn menu_entry_non_structure_errors() {
        let not_a_struct = Value::from(42i32).try_to_owned().unwrap();
        assert!(parse_menu_entry(not_a_struct).is_err());
    }

    // ── parse_layout_node: root-node handling ─────────────────────────────────

    #[test]
    fn layout_node_invisible_root_yields_empty_items() {
        // Even with a child present, an invisible root short-circuits to empty.
        let child = node_owned(2, vec![("label", Value::from("Child"))], vec![]);
        let root = layout_node(1, vec![("visible", Value::from(false))], vec![child]);
        let menu = parse_layout_node(root);
        assert_eq!(menu.id, 1);
        assert!(menu.items.is_empty());
    }

    #[test]
    fn layout_node_separator_root_yields_empty_items() {
        let root = layout_node(1, vec![("type", Value::from("separator"))], vec![]);
        let menu = parse_layout_node(root);
        assert!(menu.items.is_empty());
    }

    // ── parse_menu_entry: visibility, separators, toggles ─────────────────────

    #[test]
    fn menu_entry_invisible_is_skipped() {
        let entry = node_owned(3, vec![("visible", Value::from(false))], vec![]);
        assert!(parse_menu_entry(entry).expect("parses").is_none());
    }

    #[test]
    fn menu_entry_separator() {
        let entry = node_owned(4, vec![("type", Value::from("separator"))], vec![]);
        match parse_menu_entry(entry).expect("parses") {
            Some(MenuEntry::Separator) => {}
            other => panic!("expected separator, got {other:?}"),
        }
    }

    #[test]
    fn menu_entry_toggle_type_strings_map() {
        for (raw, expected) in [
            ("checkmark", ToggleType::Checkmark),
            ("radio", ToggleType::Radio),
            ("", ToggleType::None),
            ("garbage", ToggleType::None),
        ] {
            let entry = node_owned(
                9,
                vec![
                    ("label", Value::from("Item")),
                    ("toggle-type", Value::from(raw)),
                    ("toggle-state", Value::from(1i32)),
                ],
                vec![],
            );
            match parse_menu_entry(entry).expect("parses") {
                Some(MenuEntry::Item(item)) => {
                    assert_eq!(item.toggle_type, expected, "toggle-type {raw:?}");
                    assert_eq!(item.toggle_state, 1);
                }
                other => panic!("expected item, got {other:?}"),
            }
        }
    }

    #[test]
    fn menu_entry_defaults_when_props_absent() {
        // No props at all: enabled defaults true, label empty, toggle None,
        // toggle-state -1, no submenu — and crucially, no panic.
        let entry = node_owned(10, vec![], vec![]);
        match parse_menu_entry(entry).expect("parses") {
            Some(MenuEntry::Item(item)) => {
                assert_eq!(item.id, 10);
                assert_eq!(item.label, "");
                assert!(item.enabled);
                assert_eq!(item.icon_name, "");
                assert_eq!(item.toggle_type, ToggleType::None);
                assert_eq!(item.toggle_state, -1);
                assert!(item.submenu.is_none());
            }
            other => panic!("expected item, got {other:?}"),
        }
    }

    #[test]
    fn menu_entry_label_accelerator_is_stripped() {
        let entry = node_owned(11, vec![("label", Value::from("_Quit"))], vec![]);
        match parse_menu_entry(entry).expect("parses") {
            Some(MenuEntry::Item(item)) => assert_eq!(item.label, "Quit"),
            other => panic!("expected item, got {other:?}"),
        }
    }

    // ── parse_menu_entry: deep submenu nesting ────────────────────────────────

    /// Deep `children-display == submenu` nesting must terminate without
    /// panicking, and — since the `unwrap_variant` fix (#8) — must actually
    /// retain the nested tree. `av` children arrive as `Value::Value`-wrapped
    /// structures, both off the wire and in these fixtures (see the fixture
    /// builder doc comment above); `parse_menu_entry` now peels that wrapper
    /// before `Structure::try_from`, so the grandchild survives two levels of
    /// submenu nesting instead of being silently dropped.
    #[test]
    fn menu_entry_deep_submenu_nesting_preserves_children() {
        let grandchild = node_value(30, vec![("label", Value::from("Grandchild"))], vec![]);
        let child = node_value(
            20,
            vec![
                ("label", Value::from("Child")),
                ("children-display", Value::from("submenu")),
            ],
            vec![grandchild],
        );
        let parent = node_owned(
            10,
            vec![
                ("label", Value::from("Parent")),
                ("children-display", Value::from("submenu")),
            ],
            vec![child],
        );

        let entry = parse_menu_entry(parent).expect("deep nesting parses without panic");
        match entry {
            Some(MenuEntry::Item(item)) => {
                assert_eq!(item.label, "Parent");
                let submenu = item.submenu.expect("parent has a submenu");
                assert_eq!(submenu.len(), 1, "child was dropped: {submenu:?}");
                match &submenu[0] {
                    MenuEntry::Item(child_item) => {
                        assert_eq!(child_item.label, "Child");
                        let grandchildren =
                            child_item.submenu.as_ref().expect("child has a submenu");
                        assert_eq!(
                            grandchildren.len(),
                            1,
                            "grandchild was dropped: {grandchildren:?}"
                        );
                        match &grandchildren[0] {
                            MenuEntry::Item(grandchild_item) => {
                                assert_eq!(grandchild_item.label, "Grandchild");
                            }
                            other @ MenuEntry::Separator => {
                                panic!("expected grandchild item, got {other:?}")
                            }
                        }
                    }
                    other @ MenuEntry::Separator => panic!("expected child item, got {other:?}"),
                }
            }
            other => panic!("expected item, got {other:?}"),
        }
    }

    /// Companion to the nesting test at the root level: a root node with a
    /// visible standard child retains that child.
    #[test]
    fn layout_node_children_preserves_children() {
        let child = node_owned(2, vec![("label", Value::from("Child"))], vec![]);
        let root = layout_node(1, vec![("label", Value::from("Root"))], vec![child]);
        let menu = parse_layout_node(root);
        assert_eq!(menu.id, 1);
        assert_eq!(menu.items.len(), 1);
        match &menu.items[0] {
            MenuEntry::Item(item) => assert_eq!(item.label, "Child"),
            other @ MenuEntry::Separator => panic!("expected item, got {other:?}"),
        }
    }

    /// A menu with N visible children must parse exactly N entries in order
    /// — the general case behind the single-child regression tests above.
    #[test]
    fn layout_node_parses_all_n_children() {
        let children: Vec<OwnedValue> = (0..5)
            .map(|i| {
                node_owned(
                    100 + i,
                    vec![("label", Value::from(format!("Item {i}")))],
                    vec![],
                )
            })
            .collect();
        let root = layout_node(1, vec![("label", Value::from("Root"))], children);
        let menu = parse_layout_node(root);
        assert_eq!(menu.items.len(), 5);
        for (i, entry) in menu.items.iter().enumerate() {
            match entry {
                MenuEntry::Item(item) => {
                    assert_eq!(item.id, 100 + i32::try_from(i).unwrap());
                    assert_eq!(item.label, format!("Item {i}"));
                }
                other @ MenuEntry::Separator => {
                    panic!("expected item at index {i}, got {other:?}")
                }
            }
        }
    }

    // ── GetLayout wire round-trip (the #8 regression lock) ────────────────────

    /// Serialize a real `(u(ia{sv}av))` `GetLayout` reply, deserialize it into
    /// `(u32, LayoutNode)`, and parse the tree — the end-to-end path the old
    /// `(u32, OwnedValue)` destructure broke.
    ///
    /// `WireNode` derives the concrete `(ia{sv}av)` signature, so a
    /// `(u32, WireNode)` reply encodes to exactly the wire signature the bug
    /// report captured (`got (u(ia{sv}av)), expected (uv)`). If the
    /// `LayoutNode` shape ever drifts from the wire again, this deserialize
    /// fails and the test catches it — the earlier per-node fixtures build
    /// `LayoutNode` directly and would *not*. Covers a nested submenu so the
    /// recursion runs against genuinely round-tripped `av` children.
    #[test]
    fn getlayout_reply_deserializes_and_parses() {
        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{Endian, to_bytes};

        /// The concrete root node, mirroring `LayoutNode` on the serialize side
        /// (children as `Vec<Value>` so each is a variant-wrapped `(ia{sv}av)`).
        #[derive(serde::Serialize, zbus::zvariant::Type)]
        struct WireNode {
            id: i32,
            props: HashMap<String, Value<'static>>,
            children: Vec<Value<'static>>,
        }

        // root(1) → child(10, submenu) → grandchild(20)
        let grandchild = node_value(20, vec![("label", Value::from("Grandchild"))], vec![]);
        let child = node_value(
            10,
            vec![
                ("label", Value::from("Child")),
                ("children-display", Value::from("submenu")),
            ],
            vec![grandchild],
        );
        let root = WireNode {
            id: 1,
            props: [("label".to_string(), Value::from("Root"))]
                .into_iter()
                .collect(),
            children: vec![child],
        };

        let ctxt = Context::new_dbus(Endian::Little, 0);
        let encoded = to_bytes(ctxt, &(0u32, root)).expect("serialise GetLayout reply");
        let (revision, node): (u32, LayoutNode) =
            encoded.deserialize().expect("deserialise (u(ia{sv}av))").0;
        assert_eq!(revision, 0);

        let menu = parse_layout_node(node);
        assert_eq!(menu.id, 1);
        assert_eq!(menu.items.len(), 1, "root child dropped: {menu:?}");

        let MenuEntry::Item(child_item) = &menu.items[0] else {
            panic!("expected child item, got {:?}", menu.items[0]);
        };
        assert_eq!(child_item.label, "Child");
        let grandchildren = child_item.submenu.as_ref().expect("child has a submenu");
        assert_eq!(grandchildren.len(), 1, "grandchild dropped: {grandchildren:?}");
        match &grandchildren[0] {
            MenuEntry::Item(g) => assert_eq!(g.label, "Grandchild"),
            other @ MenuEntry::Separator => panic!("expected grandchild item, got {other:?}"),
        }
    }

    // ── parse_service ─────────────────────────────────────────────────────────

    #[test]
    fn parse_service_object_path_uses_sender_bus_name() {
        let (bus, path) = parse_service("/org/ayatana/NotificationItem/foo", ":1.42");
        assert_eq!(bus, ":1.42");
        assert_eq!(path, "/org/ayatana/NotificationItem/foo");
    }

    #[test]
    fn parse_service_bus_name_uses_default_path() {
        let (bus, path) = parse_service("org.example.Item", ":1.42");
        assert_eq!(bus, "org.example.Item");
        assert_eq!(path, "/StatusNotifierItem");
    }
}
