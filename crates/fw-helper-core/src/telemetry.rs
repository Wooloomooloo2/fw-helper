use crate::{paths, Cap, Capabilities, EnergySampler, Sysfs};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct TempReading {
    pub label: String,
    pub celsius: f64,
    /// From `temp*_crit`. `None` when the board reports an implausible value —
    /// on the reference machine every `temp*_max` reads -273150 (unset), so any
    /// threshold must be sanity-checked before it is trusted (ADR 0006).
    pub critical: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Telemetry {
    pub temps: Vec<TempReading>,
    pub fan_rpm: Option<u64>,
    /// Quantized to 0.1 W and updated at most 1 Hz — ADR 0009.
    pub package_watts: Option<f64>,
    pub battery_percent: Option<u64>,
    pub battery_status: Option<String>,
    pub platform_profile: Option<String>,
    /// What the whole machine is drawing, in watts.
    ///
    /// Only measurable **on battery**, where it is the battery's own discharge rate and
    /// therefore genuinely everything: CPU, screen, USB, the lot. On mains nothing
    /// reports total system draw, so this is `None` rather than a guess — the RAPL
    /// figure is the CPU package alone and is typically a fraction of it.
    pub system_watts: Option<f64>,
    /// Minutes until the battery is empty at the current rate, when discharging.
    pub battery_minutes: Option<u64>,
    /// Energy left in the pack and what a full charge holds, both in watt-hours.
    ///
    /// A percentage says how full the battery is; watt-hours say how much work is left
    /// in it, which is the figure that can be compared against a draw in watts. Both
    /// are published because the remaining figure means little without the capacity it
    /// is a fraction of.
    ///
    /// Available whatever the battery's state, unlike [`Self::system_watts`] — this is
    /// a level rather than a rate, so charging does not make it meaningless.
    pub battery_wh: Option<f64>,
    pub battery_wh_full: Option<f64>,
    /// True on mains, false on battery, `None` when no mains supply can be found.
    ///
    /// Resolved by the supply's `type` being `Mains`, never by its name: this board
    /// calls it `ACAD`, others call it `AC` or `ADP1`, and the same reasoning that
    /// applies to hwmon indices applies here.
    pub on_ac: Option<bool>,
}

impl Telemetry {
    /// The battery's own temperature sensor, if this board exposes one.
    ///
    /// Worth having separately from [`Self::control_temp`] because the battery is the
    /// one component here with a low limit and **no protection of its own**: on the
    /// reference board `battery_temp@b` reports crit at 49.9 °C, against 100 °C for the
    /// CPU, and the CPU throttles while the battery simply degrades (ADR 0011).
    ///
    /// Resolved by label, never by index — sensor ordering is no more stable than
    /// hwmon numbering.
    pub fn battery_temp(&self) -> Option<&TempReading> {
        self.temps.iter().find(|t| t.label.starts_with("battery"))
    }

    /// The sensor a fan curve should follow. `peci-temp` is the CPU package on the
    /// reference board; fall back to anything CPU-ish, then to the hottest reading.
    pub fn control_temp(&self) -> Option<&TempReading> {
        self.temps
            .iter()
            .find(|t| t.label == "peci-temp")
            .or_else(|| self.temps.iter().find(|t| t.label.contains("cpu")))
            .or_else(|| {
                self.temps
                    .iter()
                    .max_by(|a, b| a.celsius.total_cmp(&b.celsius))
            })
    }
}

/// Polls hardware and produces [`Telemetry`].
pub struct Monitor {
    fs: Sysfs,
    caps: Capabilities,
    energy: Option<EnergySampler>,
}

impl Monitor {
    pub fn new(fs: Sysfs) -> Self {
        let caps = Capabilities::probe(&fs);
        let energy = if caps.package_power.is_available() {
            fs.read_u64(&format!("{}/max_energy_range_uj", paths::RAPL_MMIO))
                .ok()
                .map(EnergySampler::new)
        } else {
            None
        };
        Self { fs, caps, energy }
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Call on resume from suspend: the energy counter may have reset while
    /// wall-clock advanced, so the reference point is no longer meaningful.
    pub fn on_resume(&mut self) {
        if let Some(e) = self.energy.as_mut() {
            e.invalidate();
        }
    }

    pub fn sample(&mut self) -> Telemetry {
        let mut t = Telemetry {
            platform_profile: self.fs.read_string(paths::PLATFORM_PROFILE).ok(),
            battery_percent: self
                .fs
                .read_u64(&format!("{}/capacity", paths::BATTERY))
                .ok(),
            battery_status: self
                .fs
                .read_string(&format!("{}/status", paths::BATTERY))
                .ok(),
            ..Default::default()
        };

        t.on_ac = self.read_on_ac();
        let (watts, minutes) = self.read_battery_rate();
        t.system_watts = watts;
        t.battery_minutes = minutes;
        let (wh, wh_full) = self.read_battery_energy();
        t.battery_wh = wh;
        t.battery_wh_full = wh_full;

        if let Some(hwmon) = self.caps.ec_hwmon.clone() {
            // fan1_target stayed 0 under manual control during hardware validation
            // (baseline Q4) — fan1_input is the only trustworthy RPM source.
            t.fan_rpm = self.fs.read_u64(&format!("{hwmon}/fan1_input")).ok();
            t.temps = self.read_temps(&hwmon);
        }

        if let Some(sampler) = self.energy.as_mut() {
            let path = format!("{}/energy_uj", paths::RAPL_MMIO);
            if let Ok(uj) = self.fs.read_u64(&path) {
                t.package_watts = sampler
                    .sample(uj, Instant::now())
                    .map(EnergySampler::quantize);
            }
        }

        t
    }

    /// Discharge rate and time remaining, in whichever unit family the board uses.
    ///
    /// Two conventions exist and boards pick one: **energy** (`power_now` in µW,
    /// `energy_now` in µWh) or **charge** (`current_now` in µA, `voltage_now` in µV,
    /// `charge_now` in µAh). The reference machine reports the charge family and has no
    /// `power_now` at all, so reading only the energy one would silently show nothing.
    ///
    /// Returns `(None, None)` while charging or on mains: the current then describes
    /// what is going *into* the battery, which is not what the machine is using and
    /// gives no time-to-empty at all.
    fn read_battery_rate(&self) -> (Option<f64>, Option<u64>) {
        let bat = paths::BATTERY;
        if self
            .fs
            .read_string(&format!("{bat}/status"))
            .ok()
            .as_deref()
            != Some("Discharging")
        {
            return (None, None);
        }

        // Energy family first: when present it is already in watts and needs no
        // multiplication.
        if let Ok(uw) = self.fs.read_u64(&format!("{bat}/power_now")) {
            let watts = uw as f64 / 1_000_000.0;
            let minutes = self
                .fs
                .read_u64(&format!("{bat}/energy_now"))
                .ok()
                .filter(|_| uw > 0)
                .map(|uwh| (uwh as f64 * 60.0 / uw as f64) as u64);
            return (Some(watts), minutes);
        }

        // Charge family: watts is V x I, and hours is charge over current.
        let (Ok(ua), Ok(uv)) = (
            self.fs.read_u64(&format!("{bat}/current_now")),
            self.fs.read_u64(&format!("{bat}/voltage_now")),
        ) else {
            return (None, None);
        };
        if ua == 0 {
            return (None, None);
        }
        let watts = (ua as f64 / 1_000_000.0) * (uv as f64 / 1_000_000.0);
        let minutes = self
            .fs
            .read_u64(&format!("{bat}/charge_now"))
            .ok()
            .map(|uah| (uah as f64 * 60.0 / ua as f64) as u64);
        (Some(watts), minutes)
    }

    /// How much energy is in the pack, and how much it holds full, in watt-hours.
    ///
    /// The same two-family problem as [`Self::read_battery_rate`], and the same answer:
    /// try **energy** (`energy_now`/`energy_full`, already µWh) and fall back to
    /// **charge** (`charge_now`/`charge_full` in µAh, scaled by voltage). The reference
    /// machine reports only the charge family.
    ///
    /// Scaled by `voltage_min_design`, the pack's *nominal* voltage, not `voltage_now`.
    /// Nominal is the convention every other tool uses, and it is also the only stable
    /// choice: `voltage_now` rises with state of charge and sags under load, so using it
    /// would make a resting figure drift by several percent while nothing was happening.
    fn read_battery_energy(&self) -> (Option<f64>, Option<f64>) {
        let bat = paths::BATTERY;
        let uwh = |name: &str| self.fs.read_u64(&format!("{bat}/{name}")).ok();

        if let Some(now) = uwh("energy_now") {
            let full = uwh("energy_full").map(|v| v as f64 / 1_000_000.0);
            return (Some(now as f64 / 1_000_000.0), full);
        }

        let Some(uv) = uwh("voltage_min_design") else {
            return (None, None);
        };
        // µAh x µV is 1e-12 Wh, so one divisor covers both conversions.
        let wh = |uah: u64| (uah as f64) * (uv as f64) / 1e12;
        (uwh("charge_now").map(wh), uwh("charge_full").map(wh))
    }

    /// Find the mains supply by type and read whether it is online.
    fn read_on_ac(&self) -> Option<bool> {
        let dir = self.fs.path("sys/class/power_supply");
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let base = entry.file_name().to_str()?.to_string();
            let rel = format!("sys/class/power_supply/{base}");
            if self.fs.read_string(&format!("{rel}/type")).ok().as_deref() != Some("Mains") {
                continue;
            }
            if let Ok(online) = self.fs.read_u64(&format!("{rel}/online")) {
                return Some(online == 1);
            }
        }
        None
    }

    fn read_temps(&self, hwmon: &str) -> Vec<TempReading> {
        let mut out = Vec::new();
        for i in 1..=16 {
            let Ok(milli) = self.fs.read_i64(&format!("{hwmon}/temp{i}_input")) else {
                continue;
            };
            let label = self
                .fs
                .read_string(&format!("{hwmon}/temp{i}_label"))
                .unwrap_or_else(|_| format!("temp{i}"));
            out.push(TempReading {
                label,
                celsius: milli as f64 / 1000.0,
                critical: self
                    .fs
                    .read_i64(&format!("{hwmon}/temp{i}_crit"))
                    .ok()
                    .map(|c| c as f64 / 1000.0)
                    .filter(|c| Self::plausible_threshold(*c)),
            });
        }
        out
    }

    /// Guards against the unset-sensor case: `temp*_max` reads -273150 on the
    /// reference board, and trusting it would put the fan-safety ceiling at
    /// absolute zero, disabling manual control permanently (ADR 0006).
    fn plausible_threshold(celsius: f64) -> bool {
        (0.0..=150.0).contains(&celsius)
    }
}

/// Human-readable one-liner used by `fw-helperctl status` and, later, logs.
pub fn describe(cap: &Cap) -> String {
    cap.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unset_temperature_thresholds() {
        // the -273150 millidegree "unset" value seen on real hardware
        assert!(!Monitor::plausible_threshold(-273.15));
        assert!(Monitor::plausible_threshold(87.85));
        assert!(!Monitor::plausible_threshold(1000.0));
    }

    #[test]
    fn finds_the_battery_sensor_by_label() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "peci-temp".into(),
                    celsius: 70.0,
                    critical: Some(119.85),
                },
                TempReading {
                    label: "battery_temp@b".into(),
                    celsius: 38.0,
                    critical: Some(49.9),
                },
            ],
            ..Default::default()
        };
        let b = t.battery_temp().expect("battery sensor");
        assert_eq!(b.celsius, 38.0);
        assert_eq!(b.critical, Some(49.9));
        // And it must not be mistaken for the fan curve's input.
        assert_eq!(t.control_temp().unwrap().label, "peci-temp");
    }

    #[test]
    fn no_battery_sensor_is_not_an_error() {
        let t = Telemetry {
            temps: vec![TempReading {
                label: "peci-temp".into(),
                celsius: 70.0,
                critical: None,
            }],
            ..Default::default()
        };
        assert!(t.battery_temp().is_none());
    }

    #[test]
    fn control_temp_prefers_peci() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "ddr_f75303@4d".into(),
                    celsius: 90.0,
                    critical: None,
                },
                TempReading {
                    label: "peci-temp".into(),
                    celsius: 46.8,
                    critical: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.control_temp().unwrap().label, "peci-temp");
    }

    #[test]
    fn control_temp_falls_back_to_hottest() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "battery_temp@b".into(),
                    celsius: 30.0,
                    critical: None,
                },
                TempReading {
                    label: "local_f75397@4c".into(),
                    celsius: 55.0,
                    critical: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.control_temp().unwrap().celsius, 55.0);
    }
}
