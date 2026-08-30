//! D-Bus representations of core types.
//!
//! These live here, not in `fw-helper-core` — the hardware layer takes no external
//! dependencies, so serde and zvariant stop at this boundary. See ADR 0010.

use fw_helper_core::{Capabilities, Telemetry};
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

/// `a{s(bs)}` — knob name to (available, reason).
///
/// The reason is empty when available. A client renders the reason next to a
/// disabled control, so it must be actionable, not merely descriptive (ADR 0003).
pub fn capabilities_dict(caps: &Capabilities) -> HashMap<String, (bool, String)> {
    caps.summary()
        .into_iter()
        .map(|(name, cap)| {
            let entry = match cap {
                fw_helper_core::Cap::Yes => (true, String::new()),
                fw_helper_core::Cap::No(reason) => (false, reason.clone()),
            };
            (name.to_string(), entry)
        })
        .collect()
}

/// `a{sv}` — absent keys mean "not available", which maps `Option::None` naturally
/// and lets sensors be added later without an interface version bump (ADR 0003).
pub fn telemetry_dict(t: &Telemetry) -> HashMap<String, OwnedValue> {
    let mut d: HashMap<String, OwnedValue> = HashMap::new();

    let mut put = |key: &str, value: Value<'_>| {
        if let Ok(v) = OwnedValue::try_from(value) {
            d.insert(key.to_string(), v);
        }
    };

    if let Some(w) = t.package_watts {
        put("package_watts", Value::F64(w));
    }
    if let Some(w) = t.system_watts {
        put("system_watts", Value::F64(w));
    }
    if let Some(wh) = t.battery_wh {
        put("battery_wh", Value::F64(wh));
    }
    if let Some(wh) = t.battery_wh_full {
        put("battery_wh_full", Value::F64(wh));
    }
    if let Some(m) = t.battery_minutes {
        put("battery_minutes", Value::U64(m));
    }
    if let Some(ac) = t.on_ac {
        put("on_ac", Value::Bool(ac));
    }
    if let Some(r) = t.fan_rpm {
        put("fan_rpm", Value::U64(r));
    }
    if let Some(p) = t.battery_percent {
        put("battery_percent", Value::U64(p));
    }
    if let Some(s) = &t.battery_status {
        put("battery_status", Value::Str(s.as_str().into()));
    }
    if let Some(p) = &t.platform_profile {
        put("platform_profile", Value::Str(p.as_str().into()));
    }
    if let Some(c) = t.control_temp() {
        put("control_sensor", Value::Str(c.label.as_str().into()));
    }

    let temps: HashMap<String, f64> = t
        .temps
        .iter()
        .map(|r| (r.label.clone(), r.celsius))
        .collect();
    if !temps.is_empty() {
        if let Ok(v) = OwnedValue::try_from(Value::from(temps)) {
            d.insert("temps".to_string(), v);
        }
    }

    d
}

/// `a{sd}` — sensor label to critical threshold. Published separately from telemetry
/// because it is effectively static, and because only validated values appear here:
/// `temp*_max` reads -273150 on the reference board and is filtered out upstream.
pub fn critical_temps(t: &Telemetry) -> HashMap<String, f64> {
    t.temps
        .iter()
        .filter_map(|r| r.critical.map(|c| (r.label.clone(), c)))
        .collect()
}
