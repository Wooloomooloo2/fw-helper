//! Client side of `org.fwhelper.Daemon1`, shared by the CLI and the GUI.
//!
//! Uses zbus's blocking API deliberately: neither consumer wants an async runtime.
//! The GUI drives this from a worker thread so its main loop never blocks on IPC.
//!
//! This crate exists so the proxy is defined once. It sits outside `fw-helper-core`,
//! which stays dependency-free (ADR 0010).

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

/// A recorded session on the wire: `(name, label, path, started_unix, bytes)`.
/// Decoded into [`SessionInfo`] the moment it arrives.
/// One CPU cluster: `(name, cpus, max_mhz, parkable_count)`.
///
/// `parkable_count` is below `cpus.len()` wherever `cpu0` is in the cluster — the
/// kernel exposes no `online` file for the boot CPU, so it can never be parked.
pub type ClusterTuple = (String, Vec<u32>, u32, u32);

pub type SessionTuple = (String, String, String, u64, u64);

/// The recording in progress on the wire: `(label, name, path, started_unix, samples)`.
/// An empty label means nothing is being recorded.
pub type RecordingTuple = (String, String, String, u64, u64);

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

    /// Machine load: CPU, memory and GPU.
    ///
    /// Must not be cached. Beyond the usual staleness, *reading* this is what tells the
    /// daemon somebody is watching — the GPU half of the sample is only collected while
    /// a client is asking for it, so a cached proxy would switch the scan off and then
    /// wonder why the numbers never arrive.
    #[zbus(property(emits_changed_signal = "false"))]
    fn usage(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// The recording in progress as `(label, name, path, started_unix, samples)`.
    /// An empty label means nothing is being recorded.
    #[zbus(property(emits_changed_signal = "false"))]
    fn recording_session(&self) -> zbus::Result<RecordingTuple>;

    /// Recorded sessions as `(name, label, path, started_unix, bytes)`, newest first.
    ///
    /// Metadata only. The rows live in world-readable CSV files and a client that wants
    /// to plot one opens it directly rather than pulling tens of thousands of rows
    /// across the bus.
    #[zbus(property(emits_changed_signal = "false"))]
    fn sessions(&self) -> zbus::Result<Vec<SessionTuple>>;

    /// Start recording a session. Returns the file being written.
    fn start_recording(&self, label: &str) -> zbus::Result<String>;

    /// Stop recording. Returns the finished file.
    fn stop_recording(&self) -> zbus::Result<String>;

    /// Delete a recorded session by name.
    fn delete_session(&self, name: &str) -> zbus::Result<()>;
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

    /// CPU clusters as `(name, cpus, max_mhz, parkable)`. Discovered by the daemon —
    /// never hardcode "P is 0-3", the numbering is not a promise.
    #[zbus(property(emits_changed_signal = "false"))]
    fn cpu_clusters(&self) -> zbus::Result<Vec<ClusterTuple>>;

    /// `(requested, observed)` park level. `observed` is `mixed` when cores were
    /// offlined by hand.
    #[zbus(property)]
    fn park_level(&self) -> zbus::Result<(String, String)>;

    /// Park CPU cores: `none`, `lpe`, `p-only`. May prompt via polkit.
    fn set_park_level(&self, level: &str) -> zbus::Result<()>;

    /// `(min, max, rpn, rp0)` MHz for the render GT.
    #[zbus(property)]
    fn gpu_freq_window(&self) -> zbus::Result<(u32, u32, u32, u32)>;

    /// `(requested, achieved)` MHz — `cur_freq` and `act_freq`. **Different numbers**:
    /// the first is the DVFS ask and reads a constant 2500 here, the second is what
    /// happened. `act_freq` reads 0 in RC6.
    #[zbus(property)]
    fn gpu_clocks(&self) -> zbus::Result<(u32, u32)>;

    /// Cap the GPU's maximum frequency in MHz, or 0 for the full range.
    fn set_gpu_max_freq(&self, mhz: u32) -> zbus::Result<()>;

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
    /// CPU clusters as `(name, cpus, max_mhz, parkable)`, fastest first.
    pub cpu_clusters: Vec<ClusterTuple>,
    /// `(requested, observed)` park level; `observed` is `mixed` when someone offlined
    /// cores by hand.
    pub park_level: (String, String),
    /// `(min, max, rpn, rp0)` MHz for the render GT; all zero when there is no GT.
    pub gpu_freq_window: (u32, u32, u32, u32),
    /// `(requested, achieved)` GPU MHz — `cur_freq` and `act_freq`. Showing the first
    /// as "the GPU clock" is the error four separate tools make on this board.
    pub gpu_clocks: (u32, u32),
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
    /// Machine load. Every field is `None` until the daemon has two samples to compare,
    /// and the GPU ones stay `None` on a machine whose driver we cannot read.
    pub load: Load,
    /// The recording in progress, if any.
    pub recording: Option<Recording>,
    /// Recorded sessions, newest first. Metadata only — a client that plots one opens
    /// the file itself, which is why these are world-readable.
    pub sessions: Vec<SessionInfo>,
}

/// Machine load, as decoded from the daemon's `Usage` property.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Load {
    pub cpu_percent: Option<f64>,
    pub cpu_mhz: Option<u64>,
    /// RAPL `core` rail — the cores alone, not the package.
    pub cpu_watts: Option<f64>,
    /// What the executing cores actually ran at, busy-weighted (turbostat `Bzy_MHz`).
    pub cpu_mhz_busy: Option<u64>,
    pub gpu_percent: Option<f64>,
    pub gpu_mhz: Option<u64>,
    /// RAPL `uncore` rail — the iGPU's own draw, a subset of the package figure.
    pub gpu_watts: Option<f64>,
    /// The DVFS *request* (`cur_freq`), against `gpu_mhz` which is what happened.
    pub gpu_mhz_requested: Option<u64>,
    pub mem_used_kb: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub swap_used_kb: Option<u64>,
    /// Throttle events during the last interval, not since boot.
    pub throttle_events: u64,
    pub throttle_ms: u64,
    /// GPU throttle reasons currently asserted, `+`-joined. The CPU exposes no
    /// equivalent flag - read power against the limit instead.
    pub gpu_throttle: Option<String>,
    /// The process using the GPU most, and its share.
    pub gpu_top: Option<(String, f64)>,
    /// Per-engine GPU busy, busiest first.
    pub gpu_engines: Vec<(String, f64)>,
}

impl Load {
    pub fn mem_percent(&self) -> Option<f64> {
        let (used, total) = (self.mem_used_kb?, self.mem_total_kb?);
        (total > 0).then(|| used as f64 * 100.0 / total as f64)
    }

    pub fn throttled(&self) -> bool {
        self.throttle_events > 0 || self.gpu_throttle.is_some()
    }
}

/// A recording in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub label: String,
    pub name: String,
    pub path: String,
    pub started_unix: u64,
    pub samples: u64,
}

/// One recorded session on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub label: String,
    pub path: String,
    pub started_unix: u64,
    pub bytes: u64,
}

impl SessionInfo {
    pub fn fetch(d: &DaemonProxyBlocking<'_>) -> Vec<Self> {
        d.sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, label, path, started_unix, bytes)| Self {
                name,
                label,
                path,
                started_unix,
                bytes,
            })
            .collect()
    }
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
            cpu_clusters: d.cpu_clusters().unwrap_or_default(),
            park_level: d.park_level().unwrap_or_default(),
            gpu_freq_window: d.gpu_freq_window().unwrap_or_default(),
            gpu_clocks: d.gpu_clocks().unwrap_or_default(),
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
            load: decode_load(&d.usage().unwrap_or_default()),
            sessions: SessionInfo::fetch(d),
            recording: d.recording_session().ok().and_then(
                |(label, name, path, started_unix, samples)| {
                    // The daemon reports an empty label for "not recording"; keep that
                    // sentinel here rather than leaking it to consumers.
                    (!label.is_empty()).then_some(Recording {
                        label,
                        name,
                        path,
                        started_unix,
                        samples,
                    })
                },
            ),
        })
    }

    pub fn capability(&self, name: &str) -> Option<(bool, &str)> {
        self.capabilities
            .iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, ok, why)| (*ok, why.as_str()))
    }
}

fn decode_load(u: &HashMap<String, OwnedValue>) -> Load {
    let mut engines: Vec<(String, f64)> = u
        .get("gpu_engines")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| HashMap::<String, f64>::try_from(v).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();
    engines.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Load {
        cpu_percent: u.get("cpu_percent").and_then(as_f64),
        cpu_mhz: u.get("cpu_mhz").and_then(as_u64),
        cpu_watts: u.get("cpu_watts").and_then(as_f64),
        cpu_mhz_busy: u.get("cpu_mhz_busy").and_then(as_u64),
        gpu_percent: u.get("gpu_percent").and_then(as_f64),
        gpu_mhz: u.get("gpu_mhz").and_then(as_u64),
        gpu_watts: u.get("gpu_watts").and_then(as_f64),
        gpu_mhz_requested: u.get("gpu_mhz_requested").and_then(as_u64),
        mem_used_kb: u.get("mem_used_kb").and_then(as_u64),
        mem_total_kb: u.get("mem_total_kb").and_then(as_u64),
        swap_used_kb: u.get("swap_used_kb").and_then(as_u64),
        throttle_events: u
            .get("throttle_events")
            .and_then(as_u64)
            .unwrap_or_default(),
        throttle_ms: u.get("throttle_ms").and_then(as_u64).unwrap_or_default(),
        gpu_throttle: u.get("gpu_throttle").and_then(as_string),
        gpu_top: u.get("gpu_top").and_then(as_string).map(|comm| {
            (
                comm,
                u.get("gpu_top_percent")
                    .and_then(as_f64)
                    .unwrap_or_default(),
            )
        }),
        gpu_engines: engines,
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
