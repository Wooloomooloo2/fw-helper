//! Hardware access layer for Framework laptops.
//!
//! Every path goes through [`Sysfs`], which carries a filesystem root. Production
//! uses `/`; tests point it at a fixture tree so the whole capability and telemetry
//! stack runs with no hardware attached. See `docs/adr/0004-sysfs-first-hardware-access.md`.

pub mod battery;
pub mod caps;
pub mod ceiling;
pub mod charge;
pub mod curve;
pub mod ec;
pub mod energy;
pub mod fan;
pub mod floor;
pub mod power;
pub mod profile;
pub mod session;
pub mod sysfs;
pub mod telemetry;
pub mod usage;

pub use battery::BatteryGuard;
pub use caps::{Cap, Capabilities};
pub use ceiling::Ceiling;
pub use charge::{ChargeControl, ChargeError};
pub use curve::{Curve, CurveEngine, CurveError, Point};
pub use ec::ChargeLimits;
pub use energy::EnergySampler;
pub use fan::{FanControl, FanError, FanMode};
pub use floor::{Direction, FirmwareFloor, FloorHold, STICTION_DUTY};
pub use power::{PowerError, PowerLimit};
pub use profile::{Ppd, Profile};
pub use session::{Recorder, Row, Session, SessionMeta};
pub use sysfs::Sysfs;
pub use telemetry::{Monitor, Telemetry, PACKAGE_TEMP_LABEL};
pub use usage::{Usage, UsageSampler};

/// Sysfs paths this crate knows about, relative to the [`Sysfs`] root.
pub mod paths {
    /// Authoritative RAPL zone on Framework 13 Intel — see baseline Q2.
    /// The MSR zone (`intel-rapl:0`) reports a meaningless 200 W PL1.
    pub const RAPL_MMIO: &str = "sys/class/powercap/intel-rapl-mmio:0";
    pub const PLATFORM_PROFILE: &str = "sys/firmware/acpi/platform_profile";
    pub const PLATFORM_PROFILE_CHOICES: &str = "sys/firmware/acpi/platform_profile_choices";
    pub const BATTERY: &str = "sys/class/power_supply/BAT1";
    pub const CHARGE_LIMIT_PARAM: &str =
        "sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control";
    /// hwmon node *name* — indices are not stable across boots, always resolve by name.
    pub const EC_HWMON_NAME: &str = "cros_ec";
}
