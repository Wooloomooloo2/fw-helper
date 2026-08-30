//! Client side of `org.fwhelper.Daemon1`, shared by the CLI and the GUI.
//!
//! Uses zbus's blocking API deliberately: neither consumer wants an async runtime.
//! The GUI drives this from a worker thread so its main loop never blocks on IPC.
//!
//! This crate exists so the proxy is defined once. It sits outside `fw-helper-core`,
//! which stays dependency-free (ADR 0010).

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

#[zbus::proxy(
    interface = "org.fwhelper.Daemon1",
    default_service = "org.fwhelper.Daemon1",
    default_path = "/org/fwhelper/Daemon1"
)]
// Every property here except `telemetry` and `critical_temperatures` is marked as not
// emitting a change signal, which makes zbus fetch it each time instead of caching it.
//
// The daemon emits changes for those two and nothing else, so a cached proxy froze the
// rest at their startup values: the profile list never gained a newly saved profile,
// and the power limit row never moved. The CLI could not show this - it builds a fresh
// proxy per invocation - so it only appeared once the GUI held one open.
//
// Refetching costs a round trip per property per second on a local bus, against
// silently stale readings. The daemon already caps its own publication rate (ADR 0009).
pub trait Daemon {
    #[zbus(property(emits_changed_signal = "false"))]
    fn capabilities(&self) -> zbus::Result<HashMap<String, (bool, String)>>;
    #[zbus(property)]
    fn telemetry(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn critical_temperatures(&self) -> zbus::Result<HashMap<String, f64>>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn version(&self) -> zbus::Result<u32>;

    /// Current charge limit, or 0 when unsupported.
    #[zbus(property(emits_changed_signal = "false"))]
    fn charge_limit(&self) -> zbus::Result<u8>;

    /// Set the battery charge limit. May prompt via polkit.
    fn set_charge_limit(&self, percent: u8) -> zbus::Result<()>;

    /// How the fan is driven: `auto`, `manual`, or `unavailable`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn fan_mode(&self) -> zbus::Result<String>;

    /// Duty 0-255 as the EC reports it. Meaningless unless `fan_mode` is `manual`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn fan_duty(&self) -> zbus::Result<u8>;

    /// Lowest duty permitted right now. 0 means the EC would have the fan off, so
    /// silence is allowed; 255 means no temperature could be read.
    #[zbus(property(emits_changed_signal = "false"))]
    fn fan_floor(&self) -> zbus::Result<u8>;

    /// Pin the fan at `duty` (0-255), returning what the EC settled on after being
    /// clamped up to the firmware floor. May prompt via polkit.
    fn set_fan_duty(&self, duty: u8) -> zbus::Result<u8>;

    /// Hand the fan back to the EC. May prompt via polkit.
    fn set_fan_auto(&self) -> zbus::Result<()>;

    /// Profiles this daemon knows.
    #[zbus(property(emits_changed_signal = "false"))]
    fn profiles(&self) -> zbus::Result<Vec<String>>;

    /// Profiles backed by a file, and so deletable.
    #[zbus(property(emits_changed_signal = "false"))]
    fn saved_profiles(&self) -> zbus::Result<Vec<String>>;

    /// The profile matching PPD's active profile, empty when unknown.
    #[zbus(property(emits_changed_signal = "false"))]
    fn active_profile(&self) -> zbus::Result<String>;

    /// How the profile axis is driven: `ppd`, `platform_profile`, or `none`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn profile_backend(&self) -> zbus::Result<String>;

    /// Switch profile. May prompt via polkit.
    fn set_profile(&self, name: &str) -> zbus::Result<()>;

    /// Save the current settings as a profile. Returns the file written.
    fn save_profile(&self, name: &str) -> zbus::Result<String>;

    /// Delete a saved profile. Built-ins have no file and cannot be removed.
    fn delete_profile(&self, name: &str) -> zbus::Result<()>;

    /// Profiles applied on each power source: `(on_ac, on_battery)`. Empty means off.
    #[zbus(property(emits_changed_signal = "false"))]
    fn auto_profiles(&self) -> zbus::Result<(String, String)>;

    /// Set them. Empty strings turn a side off. May prompt via polkit.
    fn set_auto_profiles(&self, on_ac: &str, on_battery: &str) -> zbus::Result<()>;

    /// Sustained CPU power limit in watts, 0 when unsupported.
    #[zbus(property(emits_changed_signal = "false"))]
    fn power_limit(&self) -> zbus::Result<u32>;

    /// Highest power limit this machine admits to. Bound sliders to this, never to the
    /// MSR zone's fictional 200 W.
    #[zbus(property(emits_changed_signal = "false"))]
    fn power_limit_max(&self) -> zbus::Result<u32>;

    /// Set the sustained CPU power limit. May prompt via polkit.
    fn set_power_limit(&self, watts: u32) -> zbus::Result<()>;

    /// The active curve as (temperature, duty) pairs; empty when none is running.
    #[zbus(property(emits_changed_signal = "false"))]
    fn fan_curve(&self) -> zbus::Result<Vec<(f64, u8)>>;

    /// The learned firmware floor across the temperature range, ascending.
    ///
    /// Grows as the machine is used, so it must not be cached for the life of the
    /// proxy - hence `emits_changed_signal = "false"` like everything else here.
    #[zbus(property(emits_changed_signal = "false"))]
    fn fan_floor_curve(&self) -> zbus::Result<Vec<(f64, u8)>>;

    /// Follow a temperature → duty curve. May prompt via polkit.
    fn set_fan_curve(&self, points: Vec<(f64, u8)>) -> zbus::Result<u8>;
}

/// Highest interface version this client understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// Connect and confirm the daemon is actually answering.
///
/// Constructing a proxy does **not** contact the service, so it succeeds even when
/// nothing owns the name. Without a forced round-trip a caller cannot distinguish
/// "daemon absent" from "daemon present", and every later property read fails
/// separately instead of falling back cleanly.
pub fn connect() -> zbus::Result<(DaemonProxyBlocking<'static>, u32)> {
    let conn = if std::env::var_os("FW_HELPERD_SESSION_BUS").is_some() {
        zbus::blocking::Connection::session()?
    } else {
        zbus::blocking::Connection::system()?
    };
    let proxy = DaemonProxyBlocking::new(&conn)?;
    let version = proxy.version()?;
    Ok((proxy, version))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub label: String,
    pub celsius: f64,
    pub critical: Option<f64>,
}

/// One decoded reading of everything the daemon publishes.
///
/// Decoding happens here so consumers never touch `zvariant` types directly, and so
/// an interface change lands in one place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub package_watts: Option<f64>,
    pub fan_rpm: Option<u64>,
    pub battery_percent: Option<u64>,
    pub battery_status: Option<String>,
    pub platform_profile: Option<String>,
    pub control_sensor: Option<String>,
    pub temps: Vec<Sensor>,
    /// `None` when charge control is unsupported on this machine.
    pub charge_limit: Option<u8>,
    /// Who is driving the fan: `auto`, `manual`, or `unavailable`.
    ///
    /// A consumer that shows fan speed must show this too. Under manual control the
    /// fan ignores the EC's curve entirely, and a user who cannot see that has no way
    /// to tell deliberate control from a stuck fan (ADR 0006).
    pub fan_mode: Option<String>,
    /// Duty 0-255. Only meaningful when `fan_mode` is `manual`.
    pub fan_duty: Option<u8>,
    /// Lowest duty permitted at the current temperature, so a client can explain a
    /// slider that will not go lower instead of appearing to ignore the user.
    pub fan_floor: Option<u8>,
    /// The active curve, empty when the fan is pinned or firmware owns it.
    pub fan_curve: Vec<(f64, u8)>,
    /// The firmware floor across the range, so an editor can draw what a curve is
    /// competing with rather than only what it asks for. Empty until observed.
    pub fan_floor_curve: Vec<(f64, u8)>,
    /// Sustained CPU power limit in watts, and the ceiling for it.
    pub power_limit: Option<u32>,
    pub power_limit_max: Option<u32>,
    /// Active profile name, and how the axis is driven.
    pub profile: Option<String>,
    pub profile_backend: Option<String>,
    /// Every profile the daemon knows, built-in and user-defined.
    pub profiles: Vec<String>,
    /// Those backed by a file, and so deletable.
    pub saved_profiles: Vec<String>,
    /// Profiles applied on each power source, empty when off.
    pub auto_profiles: (String, String),
    /// True on mains, false on battery, `None` when unknown.
    pub on_ac: Option<bool>,
    /// Whole-machine draw in watts, measurable only while on battery.
    pub system_watts: Option<f64>,
    /// Minutes until empty at the current rate.
    pub battery_minutes: Option<u64>,
    /// Energy in the pack, and what it holds full, in watt-hours. A percentage says how
    /// full it is; these say how much work is left, which is what compares against a
    /// draw in watts. Unlike `system_watts` these are levels, so charging does not make
    /// them meaningless.
    pub battery_wh: Option<f64>,
    pub battery_wh_full: Option<f64>,
    /// (knob, available, reason-if-not)
    pub capabilities: Vec<(String, bool, String)>,
}

impl Snapshot {
    pub fn fetch(d: &DaemonProxyBlocking<'_>) -> zbus::Result<Self> {
        let t = d.telemetry()?;
        let crit = d.critical_temperatures().unwrap_or_default();

        let mut caps: Vec<(String, bool, String)> = d
            .capabilities()
            .unwrap_or_default()
            .into_iter()
            .map(|(k, (ok, why))| (k, ok, why))
            .collect();
        caps.sort_by(|a, b| a.0.cmp(&b.0));

        let mut temps: Vec<Sensor> = as_temp_map(t.get("temps"))
            .into_iter()
            .map(|(label, celsius)| {
                let critical = crit.get(&label).copied();
                Sensor {
                    label,
                    celsius,
                    critical,
                }
            })
            .collect();
        // Hottest first: that is the one a fan curve cares about and the one a
        // reader scans for.
        temps.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));

        Ok(Self {
            // The daemon reports 0 for "unsupported"; keep that distinction here
            // rather than leaking a sentinel value to consumers.
            charge_limit: d.charge_limit().ok().filter(|v| *v > 0),
            fan_mode: d.fan_mode().ok(),
            fan_duty: d.fan_duty().ok(),
            fan_floor: d.fan_floor().ok(),
            fan_curve: d.fan_curve().unwrap_or_default(),
            fan_floor_curve: d.fan_floor_curve().unwrap_or_default(),
            power_limit: d.power_limit().ok().filter(|v| *v > 0),
            power_limit_max: d.power_limit_max().ok().filter(|v| *v > 0),
            profile: d.active_profile().ok().filter(|v| !v.is_empty()),
            profile_backend: d.profile_backend().ok(),
            profiles: d.profiles().unwrap_or_default(),
            saved_profiles: d.saved_profiles().unwrap_or_default(),
            auto_profiles: d.auto_profiles().unwrap_or_default(),
            on_ac: t.get("on_ac").and_then(as_bool),
            system_watts: t.get("system_watts").and_then(as_f64),
            battery_wh: t.get("battery_wh").and_then(as_f64),
            battery_wh_full: t.get("battery_wh_full").and_then(as_f64),
            battery_minutes: t.get("battery_minutes").and_then(as_u64),
            package_watts: t.get("package_watts").and_then(as_f64),
            fan_rpm: t.get("fan_rpm").and_then(as_u64),
            battery_percent: t.get("battery_percent").and_then(as_u64),
            battery_status: t.get("battery_status").and_then(as_string),
            platform_profile: t.get("platform_profile").and_then(as_string),
            control_sensor: t.get("control_sensor").and_then(as_string),
            temps,
            capabilities: caps,
        })
    }

    pub fn capability(&self, name: &str) -> Option<(bool, &str)> {
        self.capabilities
            .iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, ok, why)| (*ok, why.as_str()))
    }
}

fn as_bool(v: &OwnedValue) -> Option<bool> {
    bool::try_from(v).ok()
}

fn as_f64(v: &OwnedValue) -> Option<f64> {
    f64::try_from(v)
        .ok()
        .or_else(|| u64::try_from(v).ok().map(|n| n as f64))
}

fn as_u64(v: &OwnedValue) -> Option<u64> {
    u64::try_from(v).ok()
}

fn as_string(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

fn as_temp_map(v: Option<&OwnedValue>) -> HashMap<String, f64> {
    v.and_then(|v| v.try_clone().ok())
        .and_then(|v| HashMap::<String, f64>::try_from(v).ok())
        .unwrap_or_default()
}
