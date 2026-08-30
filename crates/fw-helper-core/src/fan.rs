//! Manual fan control — acquired as a lease, never held as a state.
//!
//! Once `pwm1_enable=1` the EC stops managing the fan and holds whatever duty was
//! last written, **indefinitely**. The failure is asymmetric: a daemon that dies at a
//! high duty leaves the machine loud, which is harmless; one that dies at a low duty
//! leaves the fan low under load, which is not, and which is inaudible by definition.
//! ADR 0006 therefore treats manual control as something acquired deliberately and
//! released on every exit path, including the ones nobody plans for.
//!
//! This module is the *mechanism* only. The curve engine, the watchdog, the
//! firmware-floor clamp and the `temp*_crit` ceiling all sit on top of it and are
//! what make manual control safe to expose to a user. Nothing here should be driven
//! from a control loop until those exist.
//!
//! Three details are load-bearing, and two of them were corrected by hardware rather
//! than reasoned out in advance:
//!
//! - **Any failure mid-acquisition releases.** Returning an error while manual control
//!   is still held, at a duty we failed to verify, is the exact state this module
//!   exists to prevent.
//! - **The takeover window cannot be closed from here.** Baseline Q4 took control by
//!   writing `pwm1_enable=1` while `pwm1` read `0`, and the obvious fix is to write the
//!   duty first. It does not work: measured on 2026-08-21, writing `pwm1` while the EC
//!   owns the fan fails with `EOPNOTSUPP`. So there is an unavoidable window between the
//!   mode switch and the first duty write during which the EC holds whatever it was
//!   already doing. The mitigation is that the window is microseconds of a single
//!   function and the very next statement establishes the duty — not that it is absent.
//!   The pre-write is kept because it is free and harmless where it *is* supported.
//! - **Duty read-back is quantized, so verification is not equality.** The EC stores
//!   duty as a whole percent: write 180 and it reports 181 (180/255 = 70.6% → 71% →
//!   181). A strict read-back check — correct for the charge limit in M2 — rejects a
//!   write the hardware accepted perfectly well. See [`DUTY_TOLERANCE`].

use crate::{paths, Sysfs};
use std::fmt;

/// `pwm1_enable` values. The kernel's hwmon convention: 1 is manual, 2 is
/// "automatic, driven by the firmware's own curve".
pub const PWM_MANUAL: u64 = 1;
pub const PWM_AUTO: u64 = 2;

/// Lowest duty [`FanControl::take_manual`] accepts, other than zero.
///
/// This is a **mechanical** limit, not a thermal one: measured on hardware, duty 20
/// leaves the fan stopped and duty 30 turns it at 1107 rpm. Anything between is a
/// stopped fan wearing a costume, and accepting it would mean reporting a running fan
/// that is not running. Zero is allowed and means exactly what it says.
///
/// **It is not a safety floor.** Nothing here knows the temperature, so nothing here
/// can decide what is safe. That is [`crate::FirmwareFloor`]'s job, and the caller is
/// responsible for clamping before calling in. An earlier version of this module
/// enforced a flat 77/255 here, which was both too loud to allow a silent idle and no
/// guarantee at all under load.
pub const MIN_TAKEOVER_DUTY: u8 = crate::floor::STICTION_DUTY;

/// Measured duty → RPM on this fan, for showing a curve in units people recognise.
///
/// **Interpolated, never fitted.** The relationship is concave: a line through the high
/// points (120/160/181) predicts 1343 rpm at a stopped duty of 0 and puts duty 77 at
/// 2925 rpm where it actually turns about 2693. Fitting one would misdescribe the whole
/// low end, which is exactly where a quiet curve lives.
///
/// Sources: duties 30–180 from the M0 fan sweep; duty 200 from the four Q7 sustained
/// runs of 2026-08-28, which held the fan pinned there for five minutes apiece — 5803
/// rpm, the mean of 104 settled samples. Duty 20 is measured at 0: stiction sits between
/// 20 and 30, so a duty in that band is a stopped fan rather than a slow one.
/// Starts at [`MIN_TAKEOVER_DUTY`] because stiction is a **discontinuity, not a ramp**.
/// Duty 20 measures 0 rpm and duty 30 measures 1107; interpolating between them would
/// report duty 25 as a fan turning at 554 rpm, and there is no such state — the fan is
/// either stopped or it has broken free. `rpm_for_duty` returns 0 below the table.
const DUTY_RPM: &[(u8, u16)] = &[
    (30, 1107),
    (50, 1879),
    (77, 2693),
    (90, 3052),
    (120, 3840),
    (180, 5201),
    (200, 5803),
];

/// Approximate RPM for a duty, by interpolating [`DUTY_RPM`].
///
/// For display only — the fan is commanded in duty, and the actual RPM at a duty varies
/// with temperature and airflow. Measured under load, duty 84 produced 2808 rpm where
/// the table predicts about 2886, so treat this as a label rather than a promise.
///
/// **Flat above duty 200, deliberately.** Nothing above 200 has been measured, and the
/// last segment climbs at roughly 30 rpm per duty count — extrapolating that to 255
/// would claim about 7450 rpm from a fan whose highest observed reading is 5886. Holding
/// the last measured value understates the top of the range instead of inventing it.
pub fn rpm_for_duty(duty: u8) -> u16 {
    // Below stiction the fan is stopped, whatever the duty says. Interpolating down to
    // zero would draw a gentle ramp across a band the fan cannot occupy.
    if duty < MIN_TAKEOVER_DUTY {
        return 0;
    }
    let pairs = DUTY_RPM;
    let last = pairs[pairs.len() - 1];
    if duty >= last.0 {
        return last.1;
    }
    for pair in pairs.windows(2) {
        let ((d0, r0), (d1, r1)) = (pair[0], pair[1]);
        if duty <= d1 {
            let span = f64::from(d1 - d0);
            let along = f64::from(duty - d0) / span;
            return (f64::from(r0) + along * (f64::from(r1) - f64::from(r0))).round() as u16;
        }
    }
    last.1
}

/// How far the EC's reported duty may sit from what we asked for before it counts
/// as a rejected write.
///
/// The EC round-trips duty through whole percent, so an 8-bit count comes back up to
/// ~1.3 counts away (255/100 ≈ 2.55 per step). Measured on hardware: 180 → 181. Three
/// counts is that bound with a margin, and is still ~1% of full scale — far too tight
/// to hide a write the EC actually ignored, which is the failure this guards against.
pub const DUTY_TOLERANCE: u8 = 3;

const PWM_ENABLE: &str = "pwm1_enable";
const PWM: &str = "pwm1";
/// Baseline Q4: `fan1_target` stayed `0` throughout manual control. Only
/// `fan1_input` reports real RPM.
const FAN_INPUT: &str = "fan1_input";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    /// EC owns the fan and runs its own curve. The safe resting state.
    Auto,
    /// We own the fan. The EC will not intervene.
    Manual,
    /// Something else entirely — do not guess what it means.
    Other(u64),
}

impl FanMode {
    pub fn from_raw(v: u64) -> Self {
        match v {
            PWM_MANUAL => Self::Manual,
            PWM_AUTO => Self::Auto,
            other => Self::Other(other),
        }
    }

    pub fn is_manual(self) -> bool {
        matches!(self, Self::Manual)
    }
}

impl fmt::Display for FanMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "EC automatic"),
            Self::Manual => write!(f, "manual"),
            Self::Other(v) => write!(f, "unknown mode {v}"),
        }
    }
}

#[derive(Debug)]
pub enum FanError {
    /// No `cros_ec` hwmon node, or one without `pwm1_enable`.
    Unsupported,
    /// A duty that cannot turn the fan, and is not zero.
    DutyCannotTurnFan(u8),
    /// Asked to set a duty while the EC still owns the fan. Writing `pwm1` here
    /// would be silently ignored, which would read as a working control that does
    /// nothing.
    NotUnderManualControl(FanMode),
    Io(std::io::Error),
    /// Mode written, but reading back gave something else. Manual control has been
    /// released before this is returned.
    ModeNotApplied {
        requested: FanMode,
        observed: FanMode,
    },
    /// Duty written, but reading back gave something else. Manual control has been
    /// released before this is returned.
    DutyNotApplied {
        requested: u8,
        observed: u8,
    },
    /// The release write itself failed to verify. This is the one error in this
    /// module with no clean recovery: the fan may be held at a duty of our choosing
    /// with nothing refreshing it.
    NotReleased {
        observed: FanMode,
    },
}

impl fmt::Display for FanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "fan control unavailable; no cros_ec hwmon node with pwm1_enable \
                 (is cros_ec_hwmon loaded?)"
            ),
            Self::DutyCannotTurnFan(d) => write!(
                f,
                "duty {d}/255 cannot turn the fan: measured, duty 20 gives 0 rpm and \
                 duty {MIN_TAKEOVER_DUTY} gives 1107 rpm. Use 0 for a stopped fan, or \
                 at least {MIN_TAKEOVER_DUTY} for a turning one"
            ),
            Self::NotUnderManualControl(mode) => write!(
                f,
                "cannot set fan duty: the fan is {mode}; take manual control first"
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::ModeNotApplied {
                requested,
                observed,
            } => write!(
                f,
                "asked the EC for {requested} fan control but it reports {observed}; \
                 released manual control and left the fan to firmware"
            ),
            Self::DutyNotApplied {
                requested,
                observed,
            } => write!(
                f,
                "wrote fan duty {requested}/255 but the EC reports {observed}/255, \
                 more than the {DUTY_TOLERANCE} counts of percent-quantization slack; \
                 released manual control rather than hold an unverified duty"
            ),
            Self::NotReleased { observed } => write!(
                f,
                "FAILED to return the fan to EC control — it reports {observed}. \
                 Run fw-helper-restore-fan, or write 2 to the cros_ec hwmon's \
                 pwm1_enable by hand"
            ),
        }
    }
}

impl std::error::Error for FanError {}

impl From<std::io::Error> for FanError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Drives `pwm1`/`pwm1_enable` on a resolved hwmon node.
///
/// Holds no state of its own: every question is answered by asking the hardware.
/// That matters for the recovery paths, which must work when in-process state is
/// exactly what cannot be trusted.
pub struct FanControl<'a> {
    fs: &'a Sysfs,
    hwmon: String,
}

impl<'a> FanControl<'a> {
    pub fn new(fs: &'a Sysfs, hwmon: impl Into<String>) -> Self {
        Self {
            fs,
            hwmon: hwmon.into(),
        }
    }

    /// Resolve the EC hwmon by name. Indices are not stable across boots.
    pub fn probe(fs: &'a Sysfs) -> Result<Self, FanError> {
        let hwmon = fs
            .find_hwmon(paths::EC_HWMON_NAME)
            .ok_or(FanError::Unsupported)?;
        let c = Self::new(fs, hwmon);
        if !c.is_supported() {
            return Err(FanError::Unsupported);
        }
        Ok(c)
    }

    fn enable_path(&self) -> String {
        format!("{}/{PWM_ENABLE}", self.hwmon)
    }

    fn duty_path(&self) -> String {
        format!("{}/{PWM}", self.hwmon)
    }

    pub fn is_supported(&self) -> bool {
        self.fs.exists(&self.enable_path()) && self.fs.exists(&self.duty_path())
    }

    pub fn mode(&self) -> Result<FanMode, FanError> {
        if !self.is_supported() {
            return Err(FanError::Unsupported);
        }
        Ok(FanMode::from_raw(self.fs.read_u64(&self.enable_path())?))
    }

    pub fn duty(&self) -> Result<u8, FanError> {
        if !self.is_supported() {
            return Err(FanError::Unsupported);
        }
        let v = self.fs.read_u64(&self.duty_path())?;
        Ok(v.min(u64::from(u8::MAX)) as u8)
    }

    /// Actual measured RPM, or `None` if unreadable. Never `fan1_target`.
    pub fn rpm(&self) -> Option<u64> {
        self.fs
            .read_u64(&format!("{}/{FAN_INPUT}", self.hwmon))
            .ok()
    }

    /// Take manual control, establishing `duty` in the same operation.
    ///
    /// There is deliberately no way to take control without naming a duty: the
    /// dangerous state is manual control at an unknown one. On any failure after the
    /// mode switch, control is released before returning.
    ///
    /// Returns the duty the EC actually settled on, which is not necessarily `duty` —
    /// see [`DUTY_TOLERANCE`].
    pub fn take_manual(&self, duty: u8) -> Result<u8, FanError> {
        // Range before support, so a bad duty reports as a bad duty even on a machine
        // that has no fan control at all.
        if duty > 0 && duty < MIN_TAKEOVER_DUTY {
            return Err(FanError::DutyCannotTurnFan(duty));
        }
        if !self.is_supported() {
            return Err(FanError::Unsupported);
        }

        // Attempt to pre-load the duty before handing ourselves the fan. On this
        // kernel it fails with EOPNOTSUPP — pwm1 is not writable while the EC owns the
        // fan — so the takeover window is real and is closed only by how immediately
        // the write below follows. Kept because it is one ignored write on hardware
        // where it is refused, and a genuine safety gain on hardware where it is not.
        let _ = self.fs.write_string(&self.duty_path(), &duty.to_string());

        self.fs
            .write_string(&self.enable_path(), &PWM_MANUAL.to_string())?;

        // From here on we hold the lease, so every early return must release it.
        self.guarded(|| {
            let observed = self.mode()?;
            if !observed.is_manual() {
                return Err(FanError::ModeNotApplied {
                    requested: FanMode::Manual,
                    observed,
                });
            }
            self.write_duty_verified(duty)
        })
    }

    /// Set the duty while under manual control.
    ///
    /// Refuses if the EC owns the fan: `pwm1` writes are rejected outright in that
    /// state (`EOPNOTSUPP`, measured), and reporting that as success would be a control
    /// that appears to work and does nothing.
    ///
    /// Returns the duty the EC actually settled on — see [`DUTY_TOLERANCE`].
    pub fn set_duty(&self, duty: u8) -> Result<u8, FanError> {
        let mode = self.mode()?;
        if !mode.is_manual() {
            return Err(FanError::NotUnderManualControl(mode));
        }
        self.guarded(|| self.write_duty_verified(duty))
    }

    /// Hand the fan back to the EC, and confirm it took it.
    ///
    /// Idempotent — calling it when already in automatic is a no-op write and a
    /// successful verify, which is what makes it safe on every exit path.
    pub fn release(&self) -> Result<(), FanError> {
        if !self.is_supported() {
            return Err(FanError::Unsupported);
        }
        self.fs
            .write_string(&self.enable_path(), &PWM_AUTO.to_string())?;
        match self.mode()? {
            FanMode::Auto => Ok(()),
            observed => Err(FanError::NotReleased { observed }),
        }
    }

    /// Release without the ability to fail.
    ///
    /// For panic hooks, signal handlers and `Drop`, where there is no caller left to
    /// hand an error to. Returns whether the fan is now demonstrably in EC control,
    /// so the caller can log the difference between "released" and "tried to".
    pub fn release_best_effort(&self) -> bool {
        let _ = self
            .fs
            .write_string(&self.enable_path(), &PWM_AUTO.to_string());
        matches!(self.mode(), Ok(FanMode::Auto))
    }

    /// Run `f`; if it fails, release manual control before propagating.
    fn guarded<T>(&self, f: impl FnOnce() -> Result<T, FanError>) -> Result<T, FanError> {
        match f() {
            Ok(v) => Ok(v),
            Err(e) => {
                if !self.release_best_effort() {
                    // The original error is the more useful one — it says what went
                    // wrong — but a failed release is the more urgent, so it wins.
                    return Err(FanError::NotReleased {
                        observed: self.mode().unwrap_or(FanMode::Other(0)),
                    });
                }
                Err(e)
            }
        }
    }

    /// Write a duty and confirm the EC took something close enough to it.
    ///
    /// Returns the duty the EC actually reports, which is the honest answer to "what
    /// is the fan doing" and is what callers should display — not what we asked for.
    fn write_duty_verified(&self, duty: u8) -> Result<u8, FanError> {
        self.fs.write_string(&self.duty_path(), &duty.to_string())?;
        let observed = self.duty()?;
        if observed.abs_diff(duty) > DUTY_TOLERANCE {
            return Err(FanError::DutyNotApplied {
                requested: duty,
                observed,
            });
        }
        Ok(observed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_duty_that_cannot_turn_the_fan() {
        let fs = Sysfs::new("/nonexistent");
        let c = FanControl::new(&fs, "sys/class/hwmon/hwmon7");
        // Reported as a bad duty, not as missing hardware: the check runs before the
        // support probe so a typo reads as a typo.
        assert!(matches!(
            c.take_manual(MIN_TAKEOVER_DUTY - 1),
            Err(FanError::DutyCannotTurnFan(_))
        ));
        // Zero is a legitimate request: it is what the EC itself does below ~45 C.
        assert!(matches!(c.take_manual(0), Err(FanError::Unsupported)));
    }

    #[test]
    fn reports_unsupported_when_the_hwmon_is_absent() {
        let fs = Sysfs::new("/nonexistent");
        let c = FanControl::new(&fs, "sys/class/hwmon/hwmon7");
        assert!(matches!(c.mode(), Err(FanError::Unsupported)));
        assert!(matches!(c.duty(), Err(FanError::Unsupported)));
        assert!(matches!(c.release(), Err(FanError::Unsupported)));
        assert!(matches!(c.take_manual(200), Err(FanError::Unsupported)));
        assert!(matches!(FanControl::probe(&fs), Err(FanError::Unsupported)));
    }

    #[test]
    fn duty_tolerance_covers_percent_quantization_but_not_a_rejected_write() {
        // The EC round-trips duty through whole percent. Model it and check the
        // tolerance spans every count in the range without being slack enough to
        // swallow a write the hardware simply ignored.
        let round_trip = |d: u8| -> u8 {
            let percent = (f64::from(d) * 100.0 / 255.0).round();
            (percent * 255.0 / 100.0).round() as u8
        };
        for d in MIN_TAKEOVER_DUTY..=u8::MAX {
            let observed = round_trip(d);
            assert!(
                observed.abs_diff(d) <= DUTY_TOLERANCE,
                "duty {d} round-trips to {observed}, beyond the tolerance"
            );
        }
        // The measured case that started this.
        assert_eq!(round_trip(180), 181);
        // An ignored write leaves the old value, which must still be caught.
        assert!(0_u8.abs_diff(180) > DUTY_TOLERANCE);
    }

    #[test]
    fn duty_not_applied_error_explains_the_quantization() {
        let msg = FanError::DutyNotApplied {
            requested: 180,
            observed: 0,
        }
        .to_string();
        assert!(msg.contains("quantization"), "got: {msg}");
    }

    #[test]
    fn mode_decodes_the_hwmon_convention() {
        assert_eq!(FanMode::from_raw(1), FanMode::Manual);
        assert_eq!(FanMode::from_raw(2), FanMode::Auto);
        assert_eq!(FanMode::from_raw(0), FanMode::Other(0));
        assert!(!FanMode::from_raw(0).is_manual());
    }

    #[test]
    fn stiction_error_names_both_valid_choices() {
        // A user told "no" must be told what "yes" looks like.
        let msg = FanError::DutyCannotTurnFan(10).to_string();
        assert!(msg.contains("Use 0"), "got: {msg}");
        assert!(msg.contains("0 rpm"), "got: {msg}");
    }

    #[test]
    fn failed_release_error_names_the_recovery_tool() {
        let msg = FanError::NotReleased {
            observed: FanMode::Manual,
        }
        .to_string();
        assert!(msg.contains("fw-helper-restore-fan"), "got: {msg}");
        assert!(msg.contains("pwm1_enable"), "got: {msg}");
    }

    #[test]
    fn not_applied_errors_say_control_was_released() {
        let mode = FanError::ModeNotApplied {
            requested: FanMode::Manual,
            observed: FanMode::Auto,
        }
        .to_string();
        assert!(mode.contains("released"), "got: {mode}");

        let duty = FanError::DutyNotApplied {
            requested: 200,
            observed: 0,
        }
        .to_string();
        assert!(duty.contains("released"), "got: {duty}");
    }

    #[test]
    fn duty_to_rpm_reproduces_every_measured_point() {
        // The table is measurement; interpolation between it must not disturb it.
        for &(duty, rpm) in DUTY_RPM {
            assert_eq!(rpm_for_duty(duty), rpm, "duty {duty}");
        }
    }

    #[test]
    fn duty_to_rpm_never_goes_backwards() {
        let mut last = 0;
        for duty in 0..=255u8 {
            let rpm = rpm_for_duty(duty);
            assert!(
                rpm >= last,
                "duty {duty} turns slower than the duty below it"
            );
            last = rpm;
        }
    }

    #[test]
    fn the_stiction_band_reads_as_a_stopped_fan() {
        // Duty 1-29 is a stopped fan, not a slow one, and the display must not imply
        // otherwise by drawing a gentle ramp up from zero.
        for duty in 0..MIN_TAKEOVER_DUTY {
            assert_eq!(rpm_for_duty(duty), 0, "duty {duty} should read as stopped");
        }
        assert!(rpm_for_duty(MIN_TAKEOVER_DUTY) > 1000);
    }

    #[test]
    fn duty_to_rpm_is_flat_above_what_was_measured() {
        // Deliberate: extrapolating the last segment to 255 would claim ~7450 rpm from
        // a fan never seen above 5886. Understating beats inventing.
        let top = rpm_for_duty(200);
        assert_eq!(rpm_for_duty(255), top);
        assert_eq!(rpm_for_duty(220), top);
    }

    #[test]
    fn duty_to_rpm_stays_concave_where_a_line_would_not() {
        // A straight line through the high points puts duty 77 near 2925 rpm. The
        // measured answer is 2693, and the table must keep saying so.
        assert!(rpm_for_duty(77) < 2800, "got {}", rpm_for_duty(77));
    }
}
