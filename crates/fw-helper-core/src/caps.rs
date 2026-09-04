use crate::{paths, Sysfs};
use std::fmt;

/// Whether a given knob is actually usable on this machine.
///
/// The GUI greys out anything that is not [`Cap::Yes`] and shows the reason, rather
/// than offering a control that silently does nothing. See ADR 0003.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cap {
    Yes,
    No(String),
}

impl Cap {
    pub fn is_available(&self) -> bool {
        matches!(self, Cap::Yes)
    }

    fn no(reason: impl Into<String>) -> Self {
        Cap::No(reason.into())
    }
}

impl fmt::Display for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cap::Yes => write!(f, "available"),
            Cap::No(reason) => write!(f, "unavailable — {reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub fan_control: Cap,
    pub power_limit: Cap,
    pub charge_limit: Cap,
    pub platform_profile: Cap,
    pub package_power: Cap,
    /// Whether per-engine GPU load can be measured on this machine.
    pub gpu_usage: Cap,
    /// Resolved hwmon path for the EC, if found.
    pub ec_hwmon: Option<String>,
}

impl Capabilities {
    /// Probe what this machine can actually do. Never panics; every knob resolves to
    /// either [`Cap::Yes`] or a reason it cannot be driven.
    pub fn probe(fs: &Sysfs) -> Self {
        let ec_hwmon = fs.find_hwmon(paths::EC_HWMON_NAME);

        let fan_control = match &ec_hwmon {
            None => Cap::no("no cros_ec hwmon node; is cros_ec_hwmon loaded?"),
            Some(h) if !fs.exists(&format!("{h}/pwm1_enable")) => {
                Cap::no("cros_ec hwmon present but exposes no pwm1_enable")
            }
            Some(_) => Cap::Yes,
        };

        let power_limit = if !fs.exists(paths::RAPL_MMIO) {
            Cap::no("no intel-rapl-mmio:0 zone")
        } else if !fs.exists(&format!("{}/constraint_0_power_limit_uw", paths::RAPL_MMIO)) {
            Cap::no("rapl zone exposes no long_term constraint")
        } else {
            Cap::Yes
        };

        // The kernel driver refuses to bind on Framework hardware unless the module
        // parameter is set — see ADR 0008. Distinguish "needs opting in" from
        // "genuinely absent", because the fix differs.
        //
        // The presence of the attribute is NOT evidence that a limit works. When the
        // binding was forced with `probe_with_fwk_charge_control=1` we are driving the
        // standard CrOS EC command on a board whose firmware also runs Framework's
        // custom one, and upstream's own words are that the custom command "can get
        // overridden" — which is the exact case the driver declines to enter. Measured
        // on 2026-08-26: threshold 80, battery charged through it to 100%. So a forced
        // binding reports unavailable, because that is what it is.
        let charge_path = format!("{}/charge_control_end_threshold", paths::BATTERY);
        let forced = matches!(
            fs.read_string(paths::CHARGE_LIMIT_PARAM).as_deref(),
            Ok("Y") | Ok("y") | Ok("1")
        );
        let charge_limit = if fs.exists(&charge_path) && forced {
            Cap::no(
                "the EC ignores this threshold: Framework's custom charge command \
                 overrides the standard one, and the battery charges past the limit \
                 to full. Use the battery limit in UEFI setup instead",
            )
        } else if fs.exists(&charge_path) {
            Cap::Yes
        } else if fs.exists(paths::CHARGE_LIMIT_PARAM) {
            Cap::no(
                "cros_charge-control declined to load; set \
                 probe_with_fwk_charge_control=1 (see ADR 0008)",
            )
        } else {
            Cap::no("no charge control interface and no module parameter to enable one")
        };

        let platform_profile = if fs.exists(paths::PLATFORM_PROFILE) {
            Cap::Yes
        } else {
            Cap::no("no ACPI platform_profile")
        };

        // energy_uj is 0400 (PLATYPUS mitigation) — existence is not enough, we must
        // be able to read it. Probe with an actual read.
        let energy_path = format!("{}/energy_uj", paths::RAPL_MMIO);
        let package_power = if !fs.exists(&energy_path) {
            Cap::no("rapl zone exposes no energy_uj")
        } else if fs.read_u64(&energy_path).is_err() {
            Cap::no("energy_uj unreadable; daemon must run as root")
        } else {
            Cap::Yes
        };

        // Load is read from `/proc/*/fdinfo`, which only the `xe` driver's counters
        // have been verified against here. Anything else names its driver rather than
        // reporting a number nobody has checked — i915 publishes the same statistics in
        // a different unit and would be a small change, on a machine we could test it.
        let gpu_usage = match crate::usage::drm_driver(fs).as_deref() {
            Some("xe") => Cap::Yes,
            Some(other) => Cap::no(format!(
                "GPU load is only implemented for the xe driver; this machine uses {other}"
            )),
            None => Cap::no("no DRM device found under /sys/class/drm"),
        };

        Self {
            fan_control,
            power_limit,
            charge_limit,
            platform_profile,
            package_power,
            gpu_usage,
            ec_hwmon,
        }
    }

    pub fn summary(&self) -> Vec<(&'static str, &Cap)> {
        vec![
            ("fan control", &self.fan_control),
            ("power limit", &self.power_limit),
            ("charge limit", &self.charge_limit),
            ("platform profile", &self.platform_profile),
            ("package power", &self.package_power),
            ("gpu usage", &self.gpu_usage),
        ]
    }
}
