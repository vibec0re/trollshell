//! Property-map parsing helpers for `BlueZ` D-Bus objects.
//!
//! These are pure functions with no side effects — they take a
//! `HashMap<String, OwnedValue>` from a `GetManagedObjects` or
//! `PropertiesChanged` payload and return typed Rust values.

use super::types::{Adapter, Device};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

pub(super) fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

pub(super) fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    // Try direct String conversion first; fall back to zvariant::Str.
    if let Some(s) = property::<String>(props, key) {
        return s;
    }
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zbus::zvariant::Str::try_from(v).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

pub(super) fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    property::<bool>(props, key).unwrap_or(false)
}

pub(super) fn parse_adapter_props(path: &str, props: &HashMap<String, OwnedValue>) -> Adapter {
    Adapter {
        path: path.to_string(),
        address: prop_str(props, "Address"),
        name: prop_str(props, "Name"),
        powered: prop_bool(props, "Powered"),
        discoverable: prop_bool(props, "Discoverable"),
        discovering: prop_bool(props, "Discovering"),
    }
}

pub(super) fn parse_device_props(path: &str, props: &HashMap<String, OwnedValue>) -> Device {
    Device {
        path: path.to_string(),
        address: prop_str(props, "Address"),
        alias: prop_str(props, "Alias"),
        icon: prop_str(props, "Icon"),
        paired: prop_bool(props, "Paired"),
        connected: prop_bool(props, "Connected"),
        trusted: prop_bool(props, "Trusted"),
        battery: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_owned_value<T: Into<zbus::zvariant::Value<'static>>>(v: T) -> OwnedValue {
        v.into()
            .try_to_owned()
            .expect("test value must be serialisable")
    }

    #[test]
    fn prop_str_returns_string_value() {
        let mut props = HashMap::new();
        props.insert("Name".to_string(), make_owned_value("TestAdapter"));
        assert_eq!(prop_str(&props, "Name"), "TestAdapter");
    }

    #[test]
    fn prop_str_missing_key_returns_empty() {
        let props: HashMap<String, OwnedValue> = HashMap::new();
        assert_eq!(prop_str(&props, "Name"), "");
    }

    #[test]
    fn prop_bool_returns_true() {
        let mut props = HashMap::new();
        props.insert("Powered".to_string(), make_owned_value(true));
        assert!(prop_bool(&props, "Powered"));
    }

    #[test]
    fn prop_bool_missing_key_returns_false() {
        let props: HashMap<String, OwnedValue> = HashMap::new();
        assert!(!prop_bool(&props, "Powered"));
    }

    #[test]
    fn parse_adapter_props_fills_all_fields() {
        let mut props = HashMap::new();
        props.insert("Address".to_string(), make_owned_value("AA:BB:CC:DD:EE:FF"));
        props.insert("Name".to_string(), make_owned_value("hci0"));
        props.insert("Powered".to_string(), make_owned_value(true));
        props.insert("Discoverable".to_string(), make_owned_value(false));
        props.insert("Discovering".to_string(), make_owned_value(true));

        let adapter = parse_adapter_props("/org/bluez/hci0", &props);
        assert_eq!(adapter.path, "/org/bluez/hci0");
        assert_eq!(adapter.address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(adapter.name, "hci0");
        assert!(adapter.powered);
        assert!(!adapter.discoverable);
        assert!(adapter.discovering);
    }

    #[test]
    fn parse_device_props_fills_all_fields() {
        let mut props = HashMap::new();
        props.insert("Address".to_string(), make_owned_value("11:22:33:44:55:66"));
        props.insert("Alias".to_string(), make_owned_value("My Headphones"));
        props.insert("Icon".to_string(), make_owned_value("audio-headphones"));
        props.insert("Paired".to_string(), make_owned_value(true));
        props.insert("Connected".to_string(), make_owned_value(false));
        props.insert("Trusted".to_string(), make_owned_value(true));

        let device = parse_device_props("/org/bluez/hci0/dev_11_22_33_44_55_66", &props);
        assert_eq!(device.path, "/org/bluez/hci0/dev_11_22_33_44_55_66");
        assert_eq!(device.address, "11:22:33:44:55:66");
        assert_eq!(device.alias, "My Headphones");
        assert_eq!(device.icon, "audio-headphones");
        assert!(device.paired);
        assert!(!device.connected);
        assert!(device.trusted);
        assert!(device.battery.is_none());
    }
}
