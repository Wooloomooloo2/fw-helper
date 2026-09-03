//! Daemon-side ownership of manual fan control.
//!
//! `fw-helper-core` provides the mechanism; this decides *when* the daemon holds the
//! lease and guarantees it lets go. ADR 0006 point 1: `pwm1_enable=2` on clean
//! shutdown, on `SIGTERM`/`SIGINT`, and on panic.
//!
//! It also enforces ADR 0006 point 4: the fan is never run slower than firmware would
//! run it, re-checked on every poll tick rather than only when a duty is requested.
//!
//! Three design choices are about the failure paths rather than the happy one:
//!
//! - **The release path takes no locks.** The panic hook calls `release_now`, and a
//!   panic can happen while another thread holds a lock — a hook that blocked on one
//!   would hang the process with the fan still held. There *is* a mutex here, around
//!   the learned floor, and the rule is that nothing on a shutdown path may touch it.
//!   Everything `release_now` needs is an atomic or a question for the hardware.
//! - **Releases are unconditional.** `release_now` writes `2` whether or not we think
//!   we hold the lease. The flag can be wrong in exactly the situation that matters —
//!   a previous instance killed while holding it — and the write is idempotent and
//!   cheap, so believing our own bookkeeping buys nothing and risks everything.
//! - **Enforcement compares decisions, not observations.** See [`FanLease::applied`]:
//!   judging "are we below the floor" against the hardware's quantized read-back let a
//!   real deficit hide inside the tolerance, which hardware caught and unit tests did
//!   not.

use fw_helper_core::fan::DUTY_TOLERANCE;
use fw_helper_core::{
    BatteryGuard, Ceiling, Curve, CurveEngine, FanControl, FanError, FanMode, FirmwareFloor,
    FloorHold, Sysfs,
};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Debug)]
pub enum LeaseError {
    /// No usable temperature. ADR 0006 point 6: no sensor, no manual fan.
    NoTemperature,
    /// Too hot for user configuration to have a say. ADR 0006 point 5.
    AboveCeiling {
        celsius: f64,
        ceiling: Ceiling,
    },
    /// The battery is near its own limit. Unlike the CPU it cannot throttle to protect
    /// itself, so this is not the user's call (ADR 0011).
    BatteryTooHot {
        celsius: f64,
        guard: BatteryGuard,
    },
    Fan(FanError),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTemperature => write!(
                f,
                "no readable temperature sensor, so the firmware floor cannot be \
                 computed and manual fan control would be unbounded; leaving the fan \
                 to the EC (ADR 0006 point 6)"
            ),
            Self::AboveCeiling { celsius, ceiling } => write!(
                f,
                "{celsius:.1} C is at or above the {ceiling} ceiling, where the fan \
                 belongs to firmware and configuration does not get a vote \
                 (ADR 0006 point 5). Let the machine cool, then ask again"
            ),
            Self::BatteryTooHot { celsius, guard } => write!(
                f,
                "the battery is at {celsius:.1} C, close to its {guard}. Unlike the CPU \
                 it cannot throttle to protect itself, so the fan is not configurable \
                 until it cools (ADR 0011)"
            ),
            Self::Fan(e) => write!(f, "{e}"),
        }
    }
}

/// Everything a fan decision needs from the thermal sensors.
///
/// Grouped because the decision genuinely depends on all of it, and because passing
/// four loose arguments through three call sites is how one of them ends up forgotten.
#[derive(Debug, Clone, Copy)]
pub struct Thermal {
    /// The sensor a fan curve follows. `None` when nothing readable.
    pub celsius: Option<f64>,
    pub ceiling: Ceiling,
    /// The battery's temperature, and its limit. `None` when the board has no battery
    /// sensor, which is a normal state and not a reason to run the fan hard.
    pub battery_celsius: Option<f64>,
    pub battery: BatteryGuard,
}

impl Thermal {
    /// Read the decision inputs out of a telemetry sample.
    pub fn from_telemetry(t: &fw_helper_core::Telemetry) -> Self {
        let control = t.control_temp();
        let battery = t.battery_temp();
        Self {
            celsius: control.map(|c| c.celsius),
            ceiling: ceiling_for(control.and_then(|c| c.critical)),
            battery_celsius: battery.map(|b| b.celsius),
            battery: BatteryGuard::from_crit(battery.and_then(|b| b.critical)),
        }
    }

    /// The battery's own demand on the fan, independent of the CPU: the CPU may be
    /// idle while the battery is warm from charging or from a hot room.
    /// Thermal inputs for a machine at `celsius` with nothing else of concern. Test
    /// support, kept beside the type so the fields stay in one place.
    #[cfg(test)]
    pub fn default_for_test(celsius: f64) -> Self {
        Self {
            celsius: Some(celsius),
            ceiling: Ceiling::from_crit(Some(119.85)),
            battery_celsius: Some(30.0),
            battery: BatteryGuard::from_crit(Some(49.9)),
        }
    }

    /// Public view of [`Self::battery_floor`], for reporting the effective floor.
    pub fn battery_floor_public(&self) -> u8 {
        self.battery_floor()
    }

    fn battery_floor(&self) -> u8 {
        self.battery_celsius
            .map(|c| self.battery.floor_duty(c))
            .unwrap_or(0)
    }

    fn battery_exceeded(&self) -> bool {
        self.battery_celsius
            .is_some_and(|c| self.battery.exceeded_by(c))
    }
}

/// Build the ceiling from a sensor's critical point.
///
/// `FW_HELPERD_DEBUG_CEILING_C` overrides it, and exists for the same reason the
/// watchdog's wedge injection does: the real ceiling sits near 100 °C, which this
/// machine does not reach under any load it is safe to apply deliberately. Without an
/// override the release path could only ever be argued for, not demonstrated. Never
/// set in production; it can only ever *lower* the ceiling, so a stray value makes the
/// daemon more cautious rather than less.
pub fn ceiling_for(crit: Option<f64>) -> Ceiling {
    let real = Ceiling::from_crit(crit);
    let Some(raw) = std::env::var_os("FW_HELPERD_DEBUG_CEILING_C") else {
        return real;
    };
    match raw.to_str().and_then(|s| s.parse::<f64>().ok()) {
        Some(c) if c < real.celsius() => {
            Ceiling::from_crit(Some(c + fw_helper_core::ceiling::CEILING_MARGIN_C))
        }
        _ => real,
    }
}

/// What actually happened when a duty was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// What the caller asked for.
    pub requested: u8,
    /// What we wrote after clamping to the firmware floor.
    pub target: u8,
    /// What the EC settled on. Differs from `target` by a count or two.
    pub settled: u8,
    /// The floor at the temperature used. Surfaced so a clamp can explain itself.
    pub floor: u8,
}

impl Applied {
    pub fn clamped(&self) -> bool {
        self.target > self.requested
    }
}

impl std::error::Error for LeaseError {}

impl From<FanError> for LeaseError {
    fn from(e: FanError) -> Self {
        Self::Fan(e)
    }
}

pub struct FanLease {
    fs: Sysfs,
    /// Whether *this process* believes it holds manual control. Used for logging and
    /// for deciding whether a shutdown is worth announcing — never as a precondition
    /// for releasing.
    held: AtomicBool,
    /// The duty the user actually asked for, before clamping. Kept because the floor
    /// moves with temperature: when the machine cools, the fan must be allowed back
    /// down to what was requested rather than staying at whatever the hottest moment
    /// demanded.
    requested: AtomicU8,
    /// The duty we last decided to write. Compared exactly against the newly computed
    /// target, so a floor deficit of one or two counts is still a deficit.
    ///
    /// The obvious alternative — compare the *observed* duty against the target and
    /// allow `DUTY_TOLERANCE` of slack — was measured on hardware doing the wrong
    /// thing: that tolerance exists to absorb the EC's percent quantization when
    /// verifying a write, and reusing it here silently permitted running up to three
    /// counts below firmware. On 2026-08-21 that left the fan at duty 84 while the
    /// floor was 87, about 4% slower than the EC would have run, for 35 seconds.
    applied: AtomicU8,
    /// Whether a duty is waiting to be restored after a resume, and which one.
    ///
    /// Two fields rather than a sentinel because **0 is a legitimate duty** — it is
    /// what the fan runs at below ~45 °C, and the case the whole floor design exists
    /// to permit. Using 0 to mean "nothing pending" would silently drop exactly the
    /// setting a quiet-machine user cares about.
    restore_pending: AtomicBool,
    restore_duty: AtomicU8,
    /// The active curve, when the fan is following one rather than pinned.
    ///
    /// Behind the same rule as the floor: **no release path may touch this lock.**
    curve: Mutex<Option<CurveEngine>>,
    /// The firmware floor, learned as it goes.
    ///
    /// **The release paths never lock this.** `release_now` runs from the panic hook,
    /// and a lock there could be held by the panicking thread. Everything on the
    /// shutdown path stays lock-free; only the deliberate control paths take this.
    floor: Mutex<FirmwareFloor>,
    /// Hysteresis on the enforced floor, so a sensor sitting on a bucket edge does not
    /// pulse the fan. Per-lease rather than part of the table: it is a fact about what
    /// we are currently driving, not about the hardware.
    ///
    /// **Under the same rule as `floor` and `curve`: no release path locks this.**
    hold: Mutex<FloorHold>,
    /// The same hysteresis for the battery guard's ramp, kept separate because it is
    /// keyed on a **different sensor**. One hold cannot serve both: a floor the battery
    /// justified at 42.9 °C has to be released by the battery cooling, not by the CPU
    /// happening to drop 2 °C while the pack stays hot.
    battery_hold: Mutex<FloorHold>,
}

impl FanLease {
    pub fn new(fs: Sysfs) -> Self {
        Self {
            fs,
            held: AtomicBool::new(false),
            requested: AtomicU8::new(0),
            applied: AtomicU8::new(0),
            restore_pending: AtomicBool::new(false),
            restore_duty: AtomicU8::new(0),
            curve: Mutex::new(None),
            floor: Mutex::new(FirmwareFloor::new()),
            hold: Mutex::new(FloorHold::new()),
            battery_hold: Mutex::new(FloorHold::new()),
        }
    }

    /// Record the duty firmware chose at this temperature.
    ///
    /// Call only while the EC owns the fan: under manual control `pwm1` is our own
    /// duty, and feeding it back would ratchet the floor up to whatever we last chose.
    /// `rising` must be false when the machine is cooling — firmware's descending
    /// branch is hysteresis, not a requirement.
    pub fn observe(&self, from_celsius: f64, to_celsius: f64, ec_duty: u8, rising: bool) {
        if let Ok(mut floor) = self.floor.lock() {
            floor.observe_span(from_celsius, to_celsius, ec_duty, rising);
        }
    }

    /// Reload observations recorded by a previous run.
    ///
    /// Worth persisting because the cold-start model is built from descending-branch
    /// measurements and is the loud one: a fresh daemon overrides a quiet curve until it
    /// has watched a heating cycle. Measured, a curve asking for silence at 55 °C got
    /// duty 61 straight after a restart.
    pub fn restore_floor(&self, observations: Vec<(f64, u8)>) -> usize {
        let n = observations.len();
        if let Ok(mut floor) = self.floor.lock() {
            floor.restore(observations);
        }
        n
    }

    /// Observations worth writing out, with the revision they correspond to.
    ///
    /// The revision lets the caller skip writing when nothing has been learned, which
    /// is most ticks.
    pub fn floor_snapshot(&self) -> Option<(u64, Vec<(f64, u8)>)> {
        let floor = self.floor.lock().ok()?;
        Some((floor.revision(), floor.observations()))
    }

    /// The lowest duty permitted at this temperature.
    pub fn floor_duty(&self, celsius: f64) -> u8 {
        self.floor
            .lock()
            .map(|f| f.floor_duty(celsius))
            // A poisoned lock means a thread panicked holding it. Refusing to guess
            // quietly, the answer is the loud one.
            .unwrap_or(u8::MAX)
    }

    /// The lowest duty to actually enforce right now: the firmware floor and the
    /// battery guard, each with hysteresis, whichever demands more.
    ///
    /// Advances both holds, so this is the accessor for the control paths and
    /// `floor_duty` stays the pure one for display and validation. A rise passes
    /// through untouched; only a drop is made to wait. See [`FloorHold`].
    ///
    /// **Hysteresis has to be applied before the two are combined, not after.** Taking
    /// `max` of two raw step functions and holding the result is not the same thing: the
    /// composed value would be released by whichever sensor moved first, including one
    /// that was not the reason the floor was raised. Measured 2026-09-01, with the hold
    /// on the firmware floor alone: the pack sat exactly on the guard's ramp start
    /// (41.9 °C, `crit - 8`) while charging, dithered by its own 1 °C quantization, and
    /// the fan alternated between duty 0 and duty 43 every few seconds with the CPU
    /// steady at 52.9 °C throughout.
    fn enforced_floor(&self, thermal: &Thermal, celsius: f64) -> (u8, u8) {
        let firmware = self
            .hold
            .lock()
            .map(|mut h| h.update(celsius, self.floor_duty(celsius)))
            // Poisoned: same answer as everywhere else on this path, the loud one.
            .unwrap_or(u8::MAX);
        // Keyed on the battery's temperature, and `NAN` when there is no battery
        // sensor — which `FloorHold` passes straight through rather than holding.
        let battery_celsius = thermal.battery_celsius.unwrap_or(f64::NAN);
        let battery = self
            .battery_hold
            .lock()
            .map(|mut h| h.update(battery_celsius, thermal.battery_floor()))
            .unwrap_or(u8::MAX);
        (firmware, battery)
    }

    /// Drop any held floor. Called where the fan changes hands, because the previous
    /// controller's hysteresis says nothing about the next one's.
    fn reset_hold(&self) {
        if let Ok(mut h) = self.hold.lock() {
            h.reset();
        }
        if let Ok(mut h) = self.battery_hold.lock() {
            h.reset();
        }
    }

    fn control(&self) -> Result<FanControl<'_>, FanError> {
        FanControl::probe(&self.fs)
    }

    pub fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    pub fn mode(&self) -> Option<FanMode> {
        self.control().ok()?.mode().ok()
    }

    pub fn duty(&self) -> Option<u8> {
        self.control().ok()?.duty().ok()
    }

    /// Reclaim the fan at startup if it was left under manual control.
    ///
    /// The only way to arrive here is a previous instance dying without releasing and
    /// `ExecStopPost` not covering it either. Like M2's charge re-apply, this reads
    /// before writing so the log distinguishes "nothing to do" from "something had
    /// gone wrong", which makes every start a free check on whether the restore paths
    /// are actually working.
    pub fn reclaim_at_startup(&self) {
        match self.mode() {
            Some(FanMode::Auto) | None => {}
            Some(other) => {
                eprintln!("fan was left under {other} control by a previous instance; reclaiming");
                if self.release_now() {
                    eprintln!("fan returned to EC control");
                } else {
                    eprintln!("WARNING: could not return the fan to EC control");
                }
            }
        }
    }

    /// Set the fan duty, taking manual control first if we do not already hold it.
    ///
    /// Returns the duty the EC actually settled on, which is not necessarily what was
    /// asked for — the EC quantizes to whole percent.
    pub fn set_duty(&self, duty: u8, thermal: Thermal) -> Result<Applied, LeaseError> {
        let Some(celsius) = thermal.celsius.filter(|c| c.is_finite()) else {
            return Err(LeaseError::NoTemperature);
        };
        // Taking the fan starts a fresh hysteresis: a floor held last time we drove it
        // was justified at a temperature from another era. Before the floor is computed
        // below, not after the write — the write is what establishes the new hold.
        if !self.held() {
            self.reset_hold();
        }
        // Checked before anything else touches hardware: above the ceiling the answer
        // is no, whatever the duty and whoever is asking.
        if thermal.ceiling.exceeded_by(celsius) {
            return Err(LeaseError::AboveCeiling {
                celsius,
                ceiling: thermal.ceiling,
            });
        }
        if thermal.battery_exceeded() {
            return Err(LeaseError::BatteryTooHot {
                celsius: thermal.battery_celsius.unwrap_or(f64::NAN),
                guard: thermal.battery,
            });
        }
        // Whichever demands more air. They are independent: the CPU can be idle while
        // the battery is warm from charging or a hot room.
        let (firmware_floor, battery_floor) = self.enforced_floor(&thermal, celsius);
        let floor = firmware_floor.max(battery_floor);
        let target = duty.max(floor);

        let settled = self.write(target)?;
        // Store the *unclamped* request: it is what the user wants, and it is what the
        // fan should return to once the machine cools.
        self.requested.store(duty, Ordering::SeqCst);
        Ok(Applied {
            requested: duty,
            target,
            settled,
            floor,
        })
    }

    /// Re-evaluate the floor and correct the fan if it has drifted from where it
    /// should be. Called every poll tick while we hold the lease.
    ///
    /// **This is what makes the clamp mean anything.** Clamping only at the moment a
    /// duty is requested protects nothing: a duty chosen at idle is perfectly safe
    /// when it is chosen and becomes stuck-low the moment the machine is put under
    /// load. The floor has to be enforced continuously or not at all.
    ///
    /// Corrects downward too. When the machine cools the floor drops, and the fan is
    /// allowed back to what was actually asked for.
    pub fn enforce_floor(&self, thermal: Thermal) -> Option<Enforced> {
        if !self.held() {
            return None;
        }
        let celsius = thermal.celsius;
        let ceiling = thermal.ceiling;
        // Losing the sensor while holding the fan is not a reason to keep holding it
        // blind. Firmware has its own sensors; give it back.
        let Some(celsius) = celsius.filter(|c| c.is_finite()) else {
            let released = self.release_now();
            return Some(Enforced::ReleasedNoSensor { released });
        };

        // ADR 0006 point 5. Note this is reached only after the floor has already been
        // demanding full duty for several degrees: releasing hands the fan to a curve
        // that tops out slower than we can drive it, so it is the last resort rather
        // than the next step up.
        if ceiling.exceeded_by(celsius) {
            let released = self.release_now();
            return Some(Enforced::ReleasedTooHot {
                celsius,
                ceiling,
                released,
            });
        }

        if thermal.battery_exceeded() {
            let released = self.release_now();
            return Some(Enforced::ReleasedBatteryHot {
                celsius: thermal.battery_celsius.unwrap_or(f64::NAN),
                guard: thermal.battery,
                released,
            });
        }

        // A curve moves the request every tick; a pinned duty keeps the one it was
        // given. Either way the floor is applied afterwards, so the curve cannot make
        // the machine unsafe and the floor is never smoothed.
        let requested = match self.curve_tick(celsius) {
            Some(duty) => {
                self.requested.store(duty, Ordering::SeqCst);
                duty
            }
            None => self.requested.load(Ordering::SeqCst),
        };
        let (firmware_floor, battery_floor) = self.enforced_floor(&thermal, celsius);
        let floor = firmware_floor.max(battery_floor);
        let target = requested.max(floor);

        let current = self.duty()?;
        // Two separate questions, and conflating them is what caused the defect
        // described on `applied`:
        //
        // 1. Has the *decision* changed? Compared exactly — one count below the floor
        //    is still below the floor.
        // 2. Has something moved the fan out from under us? That comparison is against
        //    what the hardware reports, and only there does quantization slack belong.
        let decision_changed = target != self.applied.load(Ordering::SeqCst);
        let drifted = current.abs_diff(target) > DUTY_TOLERANCE;
        if !decision_changed && !drifted {
            return None;
        }
        // Ties go to the firmware floor: when both ask for the same duty, the EC's own
        // curve is the plainer explanation and the one the reader expects.
        let source = if battery_floor > firmware_floor {
            FloorSource::Battery {
                celsius: thermal.battery_celsius.unwrap_or(f64::NAN),
                firmware: firmware_floor,
            }
        } else {
            FloorSource::Firmware
        };
        match self.write(target) {
            Ok(settled) => Some(Enforced::Corrected {
                from: current,
                to: settled,
                floor,
                celsius,
                source,
            }),
            Err(e) => Some(Enforced::Failed(e)),
        }
    }

    /// Take the fan if we do not have it, then write `duty`.
    fn write(&self, duty: u8) -> Result<u8, LeaseError> {
        let fan = self.control()?;
        // Trust the hardware over our own flag: if something else handed the fan back
        // to the EC, take it again rather than issue a write that would be rejected.
        let settled = if fan.mode()?.is_manual() {
            fan.set_duty(duty)?
        } else {
            fan.take_manual(duty)?
        };
        // Only after a verified write. take_manual releases on failure, so recording
        // the lease before this point could leave the flag set with the fan in EC
        // control — harmless, but it would make the shutdown log lie.
        self.held.store(true, Ordering::SeqCst);
        self.applied.store(duty, Ordering::SeqCst);
        Ok(settled)
    }

    /// Follow `curve` from now on, re-evaluated every tick.
    ///
    /// The curve produces a *request*; the firmware floor and the battery guard are
    /// applied to it afterwards, so a badly drawn curve is bounded by the same limits a
    /// pinned duty is. Returns what the first tick settled on.
    pub fn set_curve(&self, curve: Curve, thermal: Thermal) -> Result<Applied, LeaseError> {
        let Some(celsius) = thermal.celsius.filter(|c| c.is_finite()) else {
            return Err(LeaseError::NoTemperature);
        };
        let mut engine = CurveEngine::new(curve);
        let wanted = engine.tick(celsius);
        let applied = self.set_duty(wanted, thermal)?;
        // Only after the write is accepted: a stored curve with no lease behind it
        // would drive the next tick from a state that never happened.
        if let Ok(mut slot) = self.curve.lock() {
            *slot = Some(engine);
        }
        Ok(applied)
    }

    pub fn curve_active(&self) -> bool {
        self.curve.lock().map(|c| c.is_some()).unwrap_or(false)
    }

    /// A description of the active curve, for reporting.
    pub fn curve_points(&self) -> Option<Vec<(f64, u8)>> {
        let slot = self.curve.lock().ok()?;
        let engine = slot.as_ref()?;
        Some(
            engine
                .curve()
                .points()
                .iter()
                .map(|p| (p.celsius, p.duty))
                .collect(),
        )
    }

    fn clear_curve(&self) {
        if let Ok(mut slot) = self.curve.lock() {
            *slot = None;
        }
    }

    /// Advance the curve one tick and return the duty it asks for, if one is active.
    fn curve_tick(&self, celsius: f64) -> Option<u8> {
        let mut slot = self.curve.lock().ok()?;
        slot.as_mut().map(|e| e.tick(celsius))
    }

    /// Hand the fan back and confirm the EC took it.
    pub fn release(&self) -> Result<(), LeaseError> {
        self.clear_curve();
        self.reset_hold();
        self.requested.store(0, Ordering::SeqCst);
        self.applied.store(0, Ordering::SeqCst);
        // Asking for EC control means asking for it after the next resume too.
        self.restore_pending.store(false, Ordering::SeqCst);
        let r = self.control()?.release();
        self.held.store(false, Ordering::SeqCst);
        Ok(r?)
    }

    /// Hand the fan back before the machine suspends, remembering what to restore.
    ///
    /// A suspended process is not minding anything — the watchdog thread is frozen
    /// alongside everything else — so for the whole sleep there would be nothing
    /// between the fan and whatever duty it was left holding (ADR 0006 point 2).
    ///
    /// Returns the duty that was held, or `None` if the EC already owned the fan.
    pub fn release_for_sleep(&self) -> Option<u8> {
        if !self.held() {
            return None;
        }
        let duty = self.requested.load(Ordering::SeqCst);
        self.restore_duty.store(duty, Ordering::SeqCst);
        self.restore_pending.store(true, Ordering::SeqCst);
        self.release_now();
        Some(duty)
    }

    /// Take the fan back after a resume, if it was ours before the sleep.
    ///
    /// Deliberately re-runs the full clamp rather than restoring the raw duty: the
    /// machine may have woken warmer than it slept, and the floor is computed from
    /// telemetry read *after* the wake. Returns `None` when there is nothing pending.
    pub fn restore_after_resume(&self, thermal: Thermal) -> Option<Result<Applied, LeaseError>> {
        if !self.restore_pending.swap(false, Ordering::SeqCst) {
            return None;
        }
        // The ramp state from before the sleep says nothing about now: the machine may
        // have woken at a different temperature entirely, and easing from a duty it held
        // ten minutes ago would be smoothing across a discontinuity.
        if let Ok(mut slot) = self.curve.lock() {
            if let Some(engine) = slot.as_mut() {
                engine.reset();
            }
        }
        let duty = self.restore_duty.load(Ordering::SeqCst);
        Some(self.set_duty(duty, thermal))
    }

    /// Release without the ability to fail, for panic hooks and signal handlers.
    ///
    /// Allocates nothing, locks nothing, and does not care what we believed about the
    /// lease. Returns whether the fan is now demonstrably in EC control.
    pub fn release_now(&self) -> bool {
        let ok = match self.control() {
            Ok(fan) => fan.release_best_effort(),
            // No fan control on this machine: nothing to restore, so nothing is wrong.
            Err(_) => true,
        };
        self.held.store(false, Ordering::SeqCst);
        ok
    }
}

/// What set the floor being enforced, for a log line that names the real cause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FloorSource {
    /// Never quieter than the EC would be at this CPU temperature (ADR 0006 point 4).
    Firmware,
    /// The battery guard's ramp is asking for more air than the CPU does. The CPU can
    /// be idle while the pack is hot from charging or a warm room, so this is not a
    /// variation on the firmware floor — it is a different sensor with its own reason.
    Battery {
        /// What the pack is reading.
        celsius: f64,
        /// What the firmware floor alone would have allowed, so the message can show
        /// how far the guard moved it.
        firmware: u8,
    },
}

/// Outcome of one floor enforcement pass.
#[derive(Debug)]
pub enum Enforced {
    /// The fan was moved to keep up with (or come back down from) the floor.
    Corrected {
        from: u8,
        to: u8,
        floor: u8,
        celsius: f64,
        /// Which guard demanded `floor`.
        ///
        /// Worth carrying rather than inferring at the log line, because getting this
        /// wrong cost real time on 2026-09-01: the message named the CPU temperature
        /// beside a floor the *battery* had asked for, so a battery guard firing for
        /// the first time in the project's life read as a fan bug at 52.9 °C.
        source: FloorSource,
    },
    /// The temperature became unreadable, so the fan went back to firmware.
    ReleasedNoSensor {
        released: bool,
    },
    /// The battery reached its guard limit, so the fan went back to firmware.
    ReleasedBatteryHot {
        celsius: f64,
        guard: BatteryGuard,
        released: bool,
    },
    /// Too hot for us to be holding the fan at all (ADR 0006 point 5).
    ReleasedTooHot {
        celsius: f64,
        ceiling: Ceiling,
        released: bool,
    },
    Failed(LeaseError),
}

impl Drop for FanLease {
    fn drop(&mut self) {
        // Last line of in-process defence. Ordinary shutdown releases explicitly and
        // this is then a no-op write.
        if !self.release_now() {
            eprintln!("WARNING: fan not confirmed back under EC control at shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU32;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Idle on the reference machine. The EC runs the fan at 0 rpm here, so the floor
    /// is 0 and the user may ask for silence.
    const IDLE_C: f64 = 40.0;
    /// Loaded. Measured EC behaviour is 2925 rpm, which needs duty ~85.
    const HOT_C: f64 = 64.8;
    /// The ceiling this machine actually derives, from peci-temp's 119.85 C crit.
    fn real_ceiling() -> Ceiling {
        Ceiling::from_crit(Some(119.85))
    }

    /// Thermal inputs with a cool battery: the CPU temperature is what varies in most
    /// of these tests, and a battery at 30 C never influences the result.
    fn th(celsius: f64) -> Thermal {
        Thermal {
            celsius: Some(celsius),
            ceiling: real_ceiling(),
            battery_celsius: Some(30.0),
            battery: BatteryGuard::from_crit(Some(49.9)),
        }
    }

    /// No readable CPU sensor.
    fn th_none() -> Thermal {
        Thermal {
            celsius: None,
            ..th(40.0)
        }
    }

    /// A specific battery temperature alongside an idle CPU.
    fn th_battery(battery_celsius: f64) -> Thermal {
        Thermal {
            battery_celsius: Some(battery_celsius),
            ..th(IDLE_C)
        }
    }

    fn fixture(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helperd-fan-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        let hwmon = root.join("sys/class/hwmon/hwmon7");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("name"), "cros_ec\n").unwrap();
        fs::write(hwmon.join("pwm1_enable"), "2\n").unwrap();
        fs::write(hwmon.join("pwm1"), "0\n").unwrap();
        fs::write(hwmon.join("fan1_input"), "0\n").unwrap();
        root
    }

    fn enable(root: &Path) -> String {
        fs::read_to_string(root.join("sys/class/hwmon/hwmon7/pwm1_enable"))
            .unwrap()
            .trim()
            .into()
    }

    fn lease(root: &Path) -> FanLease {
        FanLease::new(Sysfs::new(root))
    }

    #[test]
    fn refuses_manual_control_without_a_temperature() {
        // ADR 0006 point 6: no sensor, no manual fan. Without a temperature there is
        // no floor, and without a floor the duty is unbounded.
        let root = fixture("no-temp");
        let l = lease(&root);

        assert!(matches!(
            l.set_duty(200, th_none()),
            Err(LeaseError::NoTemperature)
        ));
        assert!(matches!(
            l.set_duty(200, th(f64::NAN)),
            Err(LeaseError::NoTemperature)
        ));
        assert_eq!(enable(&root), "2", "the EC must still own the fan");
        assert!(!l.held());
    }

    #[test]
    fn a_silent_fan_is_allowed_at_idle() {
        // The whole point of the feature. If this ever fails, the floor has stopped
        // tracking the EC and started inventing policy.
        let root = fixture("silent");
        let l = lease(&root);

        let applied = l.set_duty(0, th(IDLE_C)).unwrap();
        assert_eq!(applied.floor, 0);
        assert_eq!(applied.target, 0);
        assert!(!applied.clamped());
    }

    #[test]
    fn a_low_duty_is_raised_when_the_machine_is_hot() {
        let root = fixture("clamp");
        let l = lease(&root);

        let applied = l.set_duty(0, th(HOT_C)).unwrap();
        assert!(applied.clamped(), "{applied:?}");
        assert!(applied.target >= 78, "{applied:?}");
        assert_eq!(applied.requested, 0, "the request itself is remembered");
    }

    #[test]
    fn a_duty_safe_at_idle_is_corrected_when_the_machine_heats_up() {
        // The failure the clamp exists to prevent, and the reason enforcement cannot
        // happen only at request time: duty 0 is perfectly safe when it is chosen and
        // is stuck-low a minute later.
        let root = fixture("heats-up");
        let l = lease(&root);

        l.set_duty(0, th(IDLE_C)).unwrap();
        assert_eq!(l.duty().unwrap(), 0);

        let outcome = l.enforce_floor(th(HOT_C));
        match outcome {
            Some(Enforced::Corrected { from, to, .. }) => {
                assert_eq!(from, 0);
                assert!(to >= 78, "raised only to {to}");
            }
            other => panic!("expected a correction, got {other:?}"),
        }
    }

    #[test]
    fn the_fan_comes_back_down_when_the_machine_cools() {
        // Clamping upward without ever relaxing would leave the fan stuck at whatever
        // the hottest moment demanded, which is a different kind of broken.
        let root = fixture("cools");
        let l = lease(&root);

        l.set_duty(0, th(HOT_C)).unwrap();
        let hot_duty = l.duty().unwrap();
        assert!(hot_duty >= 78);

        match l.enforce_floor(th(IDLE_C)) {
            Some(Enforced::Corrected { to, .. }) => {
                assert_eq!(to, 0, "should return to the duty actually requested")
            }
            other => panic!("expected a correction, got {other:?}"),
        }
    }

    #[test]
    fn a_steady_temperature_provokes_no_writes() {
        let root = fixture("steady");
        let l = lease(&root);
        l.set_duty(120, th(IDLE_C)).unwrap();

        assert!(
            l.enforce_floor(th(IDLE_C)).is_none(),
            "nothing changed, so nothing should be written"
        );
    }

    #[test]
    fn losing_the_sensor_hands_the_fan_back() {
        // Firmware has its own sensors. Holding the fan blind is the worst option.
        let root = fixture("blind");
        let l = lease(&root);
        l.set_duty(200, th(IDLE_C)).unwrap();

        match l.enforce_floor(th_none()) {
            Some(Enforced::ReleasedNoSensor { released }) => assert!(released),
            other => panic!("expected a release, got {other:?}"),
        }
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn enforcement_does_nothing_when_the_ec_owns_the_fan() {
        let root = fixture("not-held");
        let l = lease(&root);
        assert!(l.enforce_floor(th(HOT_C)).is_none());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn watching_firmware_changes_the_floor_the_daemon_enforces() {
        let root = fixture("observe");
        let l = lease(&root);
        let before = l.floor_duty(48.0);

        l.observe(48.0, 48.0, 120, true);
        assert!(l.floor_duty(48.0) > before);
    }

    #[test]
    fn observing_firmware_idle_permits_a_silent_fan_where_the_model_would_not() {
        // Measured: firmware runs the fan off at 60 C while heating, but the model -
        // built from descending-branch data - demands airflow there. Watching
        // firmware is what unlocks the quiet.
        let root = fixture("observe-quiet");
        let l = lease(&root);
        assert!(l.floor_duty(60.0) > 0);

        l.observe(60.0, 60.0, 0, true);
        assert_eq!(l.floor_duty(60.0), 0);
        let applied = l.set_duty(0, th(60.0)).unwrap();
        assert!(!applied.clamped(), "{applied:?}");
    }

    #[test]
    fn cooling_observations_never_reach_the_floor() {
        let root = fixture("observe-cool");
        let l = lease(&root);
        let before = l.floor_duty(61.9);
        l.observe(61.9, 61.9, 92, false);
        assert_eq!(l.floor_duty(61.9), before);
    }

    #[test]
    fn suspend_releases_the_fan_and_resume_takes_it_back() {
        let root = fixture("sleep");
        let l = lease(&root);
        l.set_duty(120, th(IDLE_C)).unwrap();

        assert_eq!(l.release_for_sleep(), Some(120));
        assert_eq!(
            enable(&root),
            "2",
            "the fan must be firmware's during sleep"
        );
        assert!(!l.held());

        let restored = l.restore_after_resume(th(IDLE_C));
        assert_eq!(restored.unwrap().unwrap().requested, 120);
        assert_eq!(enable(&root), "1");
    }

    #[test]
    fn a_requested_duty_of_zero_survives_a_suspend() {
        // 0 is a real setting, not an absent one. A sentinel-based implementation
        // would quietly lose exactly the setting a quiet-machine user chose.
        let root = fixture("sleep-zero");
        let l = lease(&root);
        l.set_duty(0, th(IDLE_C)).unwrap();

        assert_eq!(l.release_for_sleep(), Some(0));
        let restored = l.restore_after_resume(th(IDLE_C));
        assert_eq!(restored.unwrap().unwrap().requested, 0);
    }

    #[test]
    fn resume_restores_nothing_if_the_ec_already_had_the_fan() {
        let root = fixture("sleep-unheld");
        let l = lease(&root);

        assert_eq!(l.release_for_sleep(), None);
        assert!(l.restore_after_resume(th(IDLE_C)).is_none());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn resume_re_clamps_rather_than_restoring_the_raw_duty() {
        // Woken warmer than it slept: the restored duty must obey the floor at the
        // temperature read *after* the wake, not the one from before the sleep.
        let root = fixture("sleep-warm");
        let l = lease(&root);
        l.set_duty(0, th(IDLE_C)).unwrap();
        l.release_for_sleep();

        let applied = l.restore_after_resume(th(HOT_C)).unwrap().unwrap();
        assert!(applied.clamped(), "{applied:?}");
        assert!(applied.target >= 78, "{applied:?}");
    }

    #[test]
    fn asking_for_ec_control_cancels_a_pending_restore() {
        let root = fixture("sleep-cancel");
        let l = lease(&root);
        l.set_duty(120, th(IDLE_C)).unwrap();
        l.release_for_sleep();
        l.release().unwrap();

        assert!(l.restore_after_resume(th(IDLE_C)).is_none());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn refuses_to_take_the_fan_above_the_ceiling() {
        // ADR 0006 point 5: user configuration does not get a vote up here.
        let root = fixture("ceiling-refuse");
        let l = lease(&root);
        let ceiling = real_ceiling();

        let too_hot = ceiling.celsius() + 1.0;
        assert!(matches!(
            l.set_duty(255, th(too_hot)),
            Err(LeaseError::AboveCeiling { .. })
        ));
        assert_eq!(enable(&root), "2", "the fan must stay with firmware");
        assert!(!l.held());
    }

    #[test]
    fn gives_the_fan_back_when_the_ceiling_is_reached() {
        let root = fixture("ceiling-release");
        let l = lease(&root);
        let ceiling = real_ceiling();
        l.set_duty(255, th(IDLE_C)).unwrap();
        assert_eq!(enable(&root), "1");

        match l.enforce_floor(th(ceiling.celsius() + 5.0)) {
            Some(Enforced::ReleasedTooHot { released, .. }) => assert!(released),
            other => panic!("expected a ceiling release, got {other:?}"),
        }
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn the_ceiling_error_tells_the_user_what_to_do() {
        let msg = LeaseError::AboveCeiling {
            celsius: 105.0,
            ceiling: real_ceiling(),
        }
        .to_string();
        assert!(msg.contains("cool"), "got: {msg}");
    }

    #[test]
    fn full_load_temperatures_do_not_trip_the_ceiling() {
        // 76.8 C is the measured steady state under sustained full load. If the
        // ceiling fires there, manual fan control is useless exactly when wanted.
        let root = fixture("ceiling-load");
        let l = lease(&root);
        assert!(l.set_duty(200, th(76.8)).is_ok());
    }

    #[test]
    fn a_hot_battery_raises_the_floor_even_with_an_idle_cpu() {
        // The battery has an independent say. Measured, it stays around 34 C under
        // full load, so this only matters in the case nobody has measured: a fan held
        // low for a long time.
        let root = fixture("batt-floor");
        let l = lease(&root);
        assert_eq!(l.set_duty(0, th_battery(30.0)).unwrap().target, 0);

        let applied = l.set_duty(0, th_battery(45.0)).unwrap();
        assert!(applied.clamped(), "{applied:?}");
    }

    #[test]
    fn a_battery_near_its_limit_takes_the_fan_away() {
        let root = fixture("batt-release");
        let l = lease(&root);
        assert!(matches!(
            l.set_duty(255, th_battery(48.5)),
            Err(LeaseError::BatteryTooHot { .. })
        ));
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn a_battery_that_heats_while_we_hold_the_fan_gets_it_back() {
        let root = fixture("batt-enforce");
        let l = lease(&root);
        l.set_duty(200, th_battery(30.0)).unwrap();

        match l.enforce_floor(th_battery(48.5)) {
            Some(Enforced::ReleasedBatteryHot { released, .. }) => assert!(released),
            other => panic!("expected a battery release, got {other:?}"),
        }
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn the_battery_error_explains_why_it_is_not_negotiable() {
        let msg = LeaseError::BatteryTooHot {
            celsius: 48.5,
            guard: BatteryGuard::from_crit(Some(49.9)),
        }
        .to_string();
        assert!(msg.contains("throttle"), "got: {msg}");
    }

    #[test]
    fn no_battery_sensor_does_not_constrain_the_fan() {
        // A normal state, and it must not read as "battery at 0 C" or force airflow.
        let root = fixture("batt-absent");
        let l = lease(&root);
        let thermal = Thermal {
            battery_celsius: None,
            ..th(IDLE_C)
        };
        assert_eq!(l.set_duty(0, thermal).unwrap().target, 0);
    }

    #[test]
    fn takes_the_lease_then_gives_it_back() {
        let root = fixture("lease");
        let l = lease(&root);

        assert_eq!(l.set_duty(200, th(IDLE_C)).unwrap().settled, 200);
        assert!(l.held());
        assert_eq!(enable(&root), "1");

        l.release().unwrap();
        assert!(!l.held());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn release_now_works_whatever_we_believed() {
        let root = fixture("unconditional");
        let l = lease(&root);
        l.set_duty(200, th(IDLE_C)).unwrap();

        // Bookkeeping that has gone wrong: the flag says we hold nothing, the
        // hardware says otherwise. The hardware wins.
        l.held.store(false, Ordering::SeqCst);
        assert!(l.release_now());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn reclaims_a_fan_left_manual_by_a_previous_instance() {
        let root = fixture("reclaim");
        fs::write(root.join("sys/class/hwmon/hwmon7/pwm1_enable"), "1\n").unwrap();

        lease(&root).reclaim_at_startup();
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn startup_reclaim_leaves_an_untouched_fan_alone() {
        let root = fixture("no-reclaim");
        let l = lease(&root);
        l.reclaim_at_startup();
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn a_dropped_lease_releases() {
        let root = fixture("drop");
        {
            let l = lease(&root);
            l.set_duty(200, th(IDLE_C)).unwrap();
            assert_eq!(enable(&root), "1");
        }
        assert_eq!(enable(&root), "2", "Drop must have handed the fan back");
    }
    /// The floor table as `/var/lib/fw-helper/state` held it on 2026-09-01, trimmed to
    /// the band the idle pulse happened in plus the 54/56 edge used below.
    const MEASURED_FLOOR: [(f64, u8); 11] = [
        (38.0, 0),
        (40.0, 51),
        (42.0, 0),
        (44.0, 0),
        (46.0, 0),
        (48.0, 0),
        (50.0, 0),
        (52.0, 0),
        (54.0, 51),
        (56.0, 66),
        (58.0, 74),
    ];

    #[test]
    fn the_idle_pulse_is_gone_end_to_end() {
        // The 2026-09-01 defect exactly as the journal recorded it: a machine sitting
        // still, `peci-temp` wandering 39.9 <-> 40.9 C, and the daemon logging
        // `moved 0 -> 54` / `moved 54 -> 0` every couple of minutes for ever.
        let root = fixture("idle-pulse");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th(39.9)).unwrap();

        for c in [40.9, 39.9, 40.9, 39.9, 40.9, 39.9] {
            assert!(
                l.enforce_floor(th(c)).is_none(),
                "{c} C moved the fan; nothing thermal happened"
            );
        }
        assert_eq!(l.duty().unwrap(), 0, "an idle machine stays silent");
    }

    #[test]
    fn a_real_bucket_edge_does_not_pulse_either() {
        // The outlier rejection alone would not have fixed this one: 54:51 and 56:66
        // are both genuine, and a sensor parked between them still has to produce one
        // duty rather than two. This is the half FloorHold is responsible for.
        let root = fixture("edge-pulse");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th(56.9)).unwrap();
        let raised = l.duty().unwrap();
        assert!(
            raised >= 66,
            "the floor should have raised it, got {raised}"
        );

        for c in [55.9, 56.9, 55.9, 56.9] {
            assert!(
                l.enforce_floor(th(c)).is_none(),
                "{c} C moved the fan across a bucket edge"
            );
        }
    }

    #[test]
    fn a_genuine_cooldown_still_gets_its_silence_back() {
        // Hysteresis, not a ratchet. The whole point of the floor is that a quiet idle
        // is allowed once firmware itself would be quiet.
        let root = fixture("edge-cooldown");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th(56.9)).unwrap();
        assert!(l.duty().unwrap() >= 66);

        match l.enforce_floor(th(44.9)) {
            Some(Enforced::Corrected { to, .. }) => assert_eq!(to, 0),
            other => panic!("expected the fan to come back down, got {other:?}"),
        }
    }

    #[test]
    fn taking_the_fan_starts_a_fresh_hysteresis() {
        // A floor justified at 56.9 C in a previous session says nothing about a fan
        // taken again at idle.
        let root = fixture("hold-reset");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th(56.9)).unwrap();
        assert!(l.duty().unwrap() >= 66);

        l.release().unwrap();
        l.set_duty(0, th(55.9)).unwrap();
        assert_eq!(
            l.duty().unwrap(),
            53,
            "the 54 C bucket's own floor, not the one held at 56.9 C"
        );
    }
    /// A thermal reading with both sensors set, for the guard that keys on the battery.
    fn th_bat(cpu: f64, battery: f64) -> Thermal {
        Thermal {
            celsius: Some(cpu),
            ceiling: real_ceiling(),
            battery_celsius: Some(battery),
            battery: BatteryGuard::from_crit(Some(49.9)),
        }
    }

    #[test]
    fn the_battery_guards_ramp_edge_does_not_pulse_either() {
        // 2026-09-01, charging: the pack sat on `crit - 8` = 41.9 C exactly and dithered
        // across it by its own 1 C quantization, so the guard's ramp alternated between
        // duty 0 and duty 43 with the CPU steady at 52.9 C the whole time. Holding only
        // the firmware floor left this untouched, which is why it needed its own hold.
        let root = fixture("battery-edge");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th_bat(52.9, 41.9)).unwrap();
        assert_eq!(
            l.duty().unwrap(),
            0,
            "below the ramp, the guard asks for nothing"
        );

        // The first step onto the ramp is a real change and must be obeyed at once.
        match l.enforce_floor(th_bat(52.9, 42.9)) {
            Some(Enforced::Corrected { to, .. }) => assert_eq!(to, 43),
            other => panic!("the guard should have raised the fan, got {other:?}"),
        }
        for bat in [41.9, 42.9, 41.9, 42.9, 41.9] {
            assert!(
                l.enforce_floor(th_bat(52.9, bat)).is_none(),
                "battery at {bat} C moved the fan; the pack is sitting still"
            );
        }
    }

    #[test]
    fn a_battery_driven_floor_names_the_battery() {
        // The message that sent this diagnosis the wrong way for an evening: a floor the
        // pack demanded, reported against the CPU's temperature.
        let root = fixture("battery-named");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th_bat(52.9, 41.9)).unwrap();

        match l.enforce_floor(th_bat(52.9, 42.9)) {
            Some(Enforced::Corrected {
                source: FloorSource::Battery { celsius, firmware },
                floor,
                ..
            }) => {
                assert_eq!(celsius, 42.9);
                assert_eq!(firmware, 0, "the CPU at 52.9 C needs nothing");
                assert_eq!(floor, 43);
            }
            other => panic!("expected a battery-sourced correction, got {other:?}"),
        }
    }

    #[test]
    fn the_battery_hold_is_released_by_the_battery_not_the_cpu() {
        // Why the two holds cannot be one. A floor the pack justified has to wait for
        // the pack to cool; the CPU dropping 2 C says nothing about it, and a composed
        // hold would have let the CPU release a guard it never triggered.
        let root = fixture("battery-release");
        let l = lease(&root);
        l.restore_floor(MEASURED_FLOOR.to_vec());
        l.set_duty(0, th_bat(52.9, 42.9)).unwrap();
        assert_eq!(l.duty().unwrap(), 43);

        assert!(
            l.enforce_floor(th_bat(50.0, 42.9)).is_none(),
            "the CPU cooled; the battery did not, and it is the battery asking"
        );
        match l.enforce_floor(th_bat(50.0, 40.5)) {
            Some(Enforced::Corrected { to, .. }) => {
                assert_eq!(
                    to, 0,
                    "a pack that has genuinely cooled gets the quiet back"
                )
            }
            other => panic!("expected the guard to stand down, got {other:?}"),
        }
    }
}
