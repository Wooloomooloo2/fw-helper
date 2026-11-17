//! D-Bus representations of core types.
//!
//! These live here, not in `fw-helper-core` — the hardware layer takes no external
//! dependencies, so serde and zvariant stop at this boundary. See ADR 0010.

use fw_helper_core::{Capabilities, Telemetry, Usage};
use std::collections::HashMap;
use zbus::{OwnedVariant, Variant};

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
pub fn telemetry_dict(t: &Telemetry) -> HashMap<String, OwnedVariant> {
    let mut d: HashMap<String, OwnedVariant> = HashMap::new();

    let mut put = |key: &str, value: Variant<'_>| {
        if let Ok(v) = OwnedVariant::try_from(value) {
            d.insert(key.to_string(), v);
        }
    };

    if let Some(w) = t.package_watts {
        put("package_watts", Variant::F64(w));
    }
    if let Some(w) = t.system_watts {
        put("system_watts", Variant::F64(w));
    }
    if let Some(wh) = t.battery_wh {
        put("battery_wh", Variant::F64(wh));
    }
    if let Some(wh) = t.battery_wh_full {
        put("battery_wh_full", Variant::F64(wh));
    }
    if let Some(m) = t.battery_minutes {
        put("battery_minutes", Variant::U64(m));
    }
    if let Some(ac) = t.on_ac {
        put("on_ac", Variant::Bool(ac));
    }
    if let Some(r) = t.fan_rpm {
        put("fan_rpm", Variant::U64(r));
    }
    if let Some(p) = t.battery_percent {
        put("battery_percent", Variant::U64(p));
    }
    if let Some(s) = &t.battery_status {
        put("battery_status", Variant::Str(s.as_str().into()));
    }
    if let Some(p) = &t.platform_profile {
        put("platform_profile", Variant::Str(p.as_str().into()));
    }
    if let Some(c) = t.control_temp() {
        put("control_sensor", Variant::Str(c.label.as_str().into()));
    }

    let temps: HashMap<String, f64> = t
        .temps
        .iter()
        .map(|r| (r.label.clone(), r.celsius))
        .collect();
    if !temps.is_empty() {
        if let Ok(v) = OwnedVariant::try_from(Variant::from(temps)) {
            d.insert("temps".to_string(), v);
        }
    }

    d
}

/// `a{sv}` — machine load, same absent-key-means-unavailable convention as
/// [`telemetry_dict`].
///
/// Published separately from telemetry rather than merged into it because it is
/// sampled differently: the GPU half is only collected while somebody is watching, so
/// its keys come and go for reasons that have nothing to do with the hardware.
pub fn usage_dict(u: &Usage) -> HashMap<String, OwnedVariant> {
    let mut d: HashMap<String, OwnedVariant> = HashMap::new();

    let mut put = |key: &str, value: Variant<'_>| {
        if let Ok(v) = OwnedVariant::try_from(value) {
            d.insert(key.to_string(), v);
        }
    };

    if let Some(v) = u.cpu_percent {
        put("cpu_percent", Variant::F64(v));
    }
    if let Some(v) = u.cpu_mhz {
        put("cpu_mhz", Variant::U64(v));
    }
    if let Some(v) = u.cpu_watts {
        put("cpu_watts", Variant::F64(v));
    }
    if let Some(v) = u.cpu_mhz_busy {
        put("cpu_mhz_busy", Variant::U64(v));
    }
    if let Some(v) = u.gpu_percent {
        put("gpu_percent", Variant::F64(v));
    }
    if let Some(v) = u.gpu_mhz {
        put("gpu_mhz", Variant::U64(v));
    }
    if let Some(v) = u.gpu_watts {
        put("gpu_watts", Variant::F64(v));
    }
    if let Some(v) = u.gpu_mhz_requested {
        put("gpu_mhz_requested", Variant::U64(v));
    }
    if let Some(v) = u.mem_used_kb {
        put("mem_used_kb", Variant::U64(v));
    }
    if let Some(v) = u.mem_total_kb {
        put("mem_total_kb", Variant::U64(v));
    }
    if let Some(v) = u.swap_used_kb {
        put("swap_used_kb", Variant::U64(v));
    }
    // Deltas for the interval just ended, not since-boot totals: a client showing
    // "throttled" must be able to stop showing it again.
    put("throttle_events", Variant::U64(u.cpu_throttle_events));
    put("throttle_ms", Variant::U64(u.cpu_throttle_ms));
    if !u.gpu_throttle.is_empty() {
        put(
            "gpu_throttle",
            Variant::Str(u.gpu_throttle.join("+").as_str().into()),
        );
    }
    if let Some((comm, pct)) = &u.top_gpu_client {
        put("gpu_top", Variant::Str(comm.as_str().into()));
        put("gpu_top_percent", Variant::F64(*pct));
    }

    let engines: HashMap<String, f64> = u.gpu_engines.iter().cloned().collect();
    if !engines.is_empty() {
        if let Ok(v) = OwnedVariant::try_from(Variant::from(engines)) {
            d.insert("gpu_engines".to_string(), v);
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
