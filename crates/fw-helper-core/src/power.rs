//! RAPL power limits.
//!
//! The single most effective thermal control on this machine, and the reason ADR 0007
//! can drop undervolting entirely: measured, **10 W of PL1 buys about 12 °C**. Dropping
//! from 25 W to 15 W took the sustained package temperature from 76.8 °C to 64.8 °C,
//! and PL1 regulates to within ~2% of setpoint, so it is a real control rather than a
//! hint.
//!
//! Three things here are not obvious and each cost time to find:
//!
//! - **The authoritative zone is `intel-rapl-mmio:0`, not `intel-rapl:0`.** The MSR zone
//!   reports a PL1 of 200 W while its own `max_power_uw` says 25 W. Baseline Q2.
//! - **`constraint_1_max_power_uw` reads `0`.** That is "unset", not "no power allowed" —
//!   the same shape of trap as `temp*_max` reporting -273150. Any ceiling derived from a
//!   `max_power_uw` must check it is plausible first, exactly as thermal thresholds do.
//! - **Any verification must outlast the averaging window.** `constraint_0_time_window_us`
//!   is ~32 s, so a power *measurement* taken sooner reads turbo as steady state. The
//!   sysfs value itself reads back immediately; it is the effect that takes half a minute.
//!
//! PL2 (`constraint_1`, ~60 W over ~1 ms) is deliberately left alone. The profiles this
//! feeds only vary PL1, and a short-term ceiling governs burst responsiveness rather than
//! sustained thermals.

use crate::{paths, Sysfs};
use std::fmt;

/// Below this the machine is not usably slow, it is broken: single-core work stalls and
/// the desktop stops feeling responsive. Refused rather than accepted.
pub const MIN_WATTS: u32 = 8;

/// The highest PL1 this board honours, **measured** — not what the zone declares.
///
/// `constraint_0_max_power_uw` reads 25 W and does not bind: measured 2026-08-28 with
/// `scripts/sustained-perf-test.sh`, setpoints of 30 W and 35 W were both honoured and
/// held to within 0.2% for 26 consecutive intervals, worth +8.9% and +15.9% throughput
/// over 25 W. Above 35 W the board stops listening — a 40 W setpoint stayed in the
/// register for the whole run and still settled at 35.07 W, with the chassis cold
/// (board 45.9 °C) and `package_throttle_count` at zero. That is a firmware power
/// budget, not a thermal limit, and 35 W is where it sits.
///
/// Doubles as the fallback when `max_power_uw` is missing or implausible, which it is
/// exactly the kind of field to be — `constraint_1`'s reads 0.
pub const MAX_WATTS: u32 = 35;

const PL1_LIMIT: &str = "constraint_0_power_limit_uw";
const PL1_MAX: &str = "constraint_0_max_power_uw";
const PL1_WINDOW: &str = "constraint_0_time_window_us";

/// A `max_power_uw` outside this range is not believable.
const PLAUSIBLE_MAX_WATTS: std::ops::RangeInclusive<u32> = 5..=200;

#[derive(Debug)]
pub enum PowerError {
    /// No RAPL zone, or one without a long-term constraint.
    Unsupported,
    OutOfRange {
        watts: u32,
        max: u32,
    },
    Io(std::io::Error),
    /// Written, but reading back gave something else. On this hardware that means
    /// firmware overrode us.
    NotApplied {
        requested: u32,
        observed: u32,
    },
}

impl fmt::Display for PowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "no usable RAPL power limit; expected {} with a long_term constraint",
                paths::RAPL_MMIO
            ),
            Self::OutOfRange { watts, max } => write!(
                f,
                "{watts} W is outside the usable range {MIN_WATTS}-{max} W. Below \
                 {MIN_WATTS} W the machine is not quiet, it is unusable"
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::NotApplied {
                requested,
                observed,
            } => write!(
                f,
                "wrote {requested} W but the zone reports {observed} W; firmware \
                 overrode it. Check for a vendor power policy or a BIOS setting"
            ),
        }
    }
}

impl std::error::Error for PowerError {}

impl From<std::io::Error> for PowerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct PowerLimit<'a> {
    fs: &'a Sysfs,
}

impl<'a> PowerLimit<'a> {
    pub fn new(fs: &'a Sysfs) -> Self {
        Self { fs }
    }

    fn attr(&self, name: &str) -> String {
        format!("{}/{name}", paths::RAPL_MMIO)
    }

    pub fn is_supported(&self) -> bool {
        self.fs.exists(&self.attr(PL1_LIMIT))
    }

    /// The highest limit we will set, in watts.
    ///
    /// **The zone's declared maximum is a floor on this answer, not a ceiling.** On this
    /// board `max_power_uw` understates what firmware honours by 10 W (see
    /// [`MAX_WATTS`]), and clamping to it cost ~16% of available throughput. Hardware
    /// that declares *more* than we measured is believed, because that is a claim about
    /// a machine we have not characterised and refusing it would be the same mistake in
    /// the other direction.
    ///
    /// Validated: `max_power_uw` is a field that reports 0 when unset, and a ceiling of
    /// 0 W would make every value out of range and the control permanently useless.
    pub fn max_watts(&self) -> u32 {
        let declared = self
            .fs
            .read_u64(&self.attr(PL1_MAX))
            .ok()
            .map(|uw| (uw / 1_000_000) as u32)
            .filter(|w| PLAUSIBLE_MAX_WATTS.contains(w));
        declared.unwrap_or(0).max(MAX_WATTS)
    }

    /// The averaging window, in seconds. Any measurement of the *effect* of a limit
    /// must span longer than this.
    pub fn window_secs(&self) -> Option<f64> {
        self.fs
            .read_u64(&self.attr(PL1_WINDOW))
            .ok()
            .map(|us| us as f64 / 1_000_000.0)
    }

    pub fn read(&self) -> Result<u32, PowerError> {
        if !self.is_supported() {
            return Err(PowerError::Unsupported);
        }
        let uw = self.fs.read_u64(&self.attr(PL1_LIMIT))?;
        Ok((uw / 1_000_000) as u32)
    }

    /// Set PL1 and confirm the zone took it.
    ///
    /// The read-back is immediate and only proves the *value* stuck. Whether firmware
    /// later reverts it is a separate question, which is why the daemon re-reads and
    /// re-applies rather than trusting one success.
    pub fn set(&self, watts: u32) -> Result<(), PowerError> {
        let max = self.max_watts();
        // Range before support, so a typo reports as a typo even where RAPL is absent.
        if watts < MIN_WATTS || watts > max {
            return Err(PowerError::OutOfRange { watts, max });
        }
        if !self.is_supported() {
            return Err(PowerError::Unsupported);
        }
        let uw = u64::from(watts) * 1_000_000;
        self.fs
            .write_string(&self.attr(PL1_LIMIT), &uw.to_string())?;

        let observed = self.read()?;
        if observed != watts {
            return Err(PowerError::NotApplied {
                requested: watts,
                observed,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_values_that_would_make_the_machine_unusable() {
        let fs = Sysfs::new("/nonexistent");
        let p = PowerLimit::new(&fs);
        assert!(matches!(p.set(0), Err(PowerError::OutOfRange { .. })));
        assert!(matches!(
            p.set(MIN_WATTS - 1),
            Err(PowerError::OutOfRange { .. })
        ));
        // Above the envelope is equally a mistake, and the message names the ceiling.
        assert!(matches!(p.set(200), Err(PowerError::OutOfRange { .. })));
    }

    #[test]
    fn out_of_range_explains_why_there_is_a_floor() {
        let msg = PowerError::OutOfRange { watts: 3, max: 25 }.to_string();
        assert!(msg.contains("unusable"), "got: {msg}");
    }

    #[test]
    fn reports_unsupported_when_the_zone_is_absent() {
        let fs = Sysfs::new("/nonexistent");
        let p = PowerLimit::new(&fs);
        assert!(matches!(p.read(), Err(PowerError::Unsupported)));
        assert!(matches!(p.set(15), Err(PowerError::Unsupported)));
    }

    #[test]
    fn an_absent_zone_still_reports_a_usable_ceiling() {
        // max_watts must never return 0: every value would be out of range and the
        // control would be permanently dead with no explanation.
        let fs = Sysfs::new("/nonexistent");
        assert_eq!(PowerLimit::new(&fs).max_watts(), MAX_WATTS);
    }

    /// A zone rooted in a throwaway directory, declaring `max_power_uw` of `declared` W.
    fn zone_declaring(tag: &str, declared: u32) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("fw-helper-power-{}-{tag}", std::process::id()));
        let dir = root.join(crate::paths::RAPL_MMIO);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("constraint_0_power_limit_uw"), "25000000\n").unwrap();
        std::fs::write(
            dir.join("constraint_0_max_power_uw"),
            format!("{}\n", u64::from(declared) * 1_000_000),
        )
        .unwrap();
        root
    }

    #[test]
    fn a_declared_maximum_below_the_measured_one_does_not_bind() {
        // The regression this exists to catch: this board declares 25 W and honours 35 W,
        // so trusting the declaration cost ~16% of the machine's throughput.
        let root = zone_declaring("understated", 25);
        let fs = Sysfs::new(&root);
        assert_eq!(PowerLimit::new(&fs).max_watts(), MAX_WATTS);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_declared_maximum_above_the_measured_one_is_believed() {
        // Hardware we have not characterised gets the benefit of its own claim; refusing
        // it would be the same mistake as trusting an understated one.
        let root = zone_declaring("generous", 64);
        let fs = Sysfs::new(&root);
        assert_eq!(PowerLimit::new(&fs).max_watts(), 64);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_applied_blames_firmware_and_says_where_to_look() {
        let msg = PowerError::NotApplied {
            requested: 15,
            observed: 25,
        }
        .to_string();
        assert!(msg.contains("firmware"), "got: {msg}");
        assert!(msg.contains("BIOS"), "got: {msg}");
    }
}
