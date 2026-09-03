//! The firmware floor: never quieter than the EC would be.
//!
//! ADR 0006 point 4. A user-authored curve may only ever be *more* aggressive than
//! firmware, which bounds the damage from a badly drawn curve to "louder than
//! necessary" — an annoyance rather than a thermal event. This is what makes it
//! defensible to expose curve editing at all.
//!
//! **Firmware's duty can be read directly.** While `pwm1_enable=2`, `pwm1` reports the
//! duty the EC has chosen — confirmed 2026-08-21 across 60 samples, whose RPM matched
//! the measured duty→RPM table to within 2.5% on average. So the floor is *observed*,
//! not modelled: [`FirmwareFloor::observe`] records what firmware actually did at each
//! temperature, and that is what the floor returns.
//!
//! The two measured tables below remain only as the **cold start**, for temperatures
//! nothing has been seen at yet. Composing them (temperature→RPM, then RPM→duty) was
//! the original mechanism and it carries the interpolation error of both; a direct
//! observation always beats it and always wins.
//!
//! **Only observations taken while the temperature is rising or steady are recorded.**
//! The EC has enormous hysteresis — measured, at 61.9 °C it runs duty 0 on the way up
//! and duty 92 on the way down, and it does not stop the fan until below 44.9 °C. The
//! descending branch is not firmware's answer to "what does this temperature need"; it
//! is firmware avoiding oscillation while shedding heat. Recording it would ratchet the
//! floor up to a duty firmware only uses on the way down, and would make a silent idle
//! impossible for ten minutes after any load.
//!
//! Two honesty notes about the tables:
//!
//! - The duty→RPM curve is **concave**, and badly approximated by a line. A linear fit
//!   through the three high points measured first predicted duty 77 for 2925 rpm; the
//!   real answer is ~85. Extrapolation here errs quiet, which is the dangerous
//!   direction, so the table is used directly and interpolated only between measured
//!   points.
//! - The EC table has **four points and a large gap across the knee** (44.9 °C to
//!   53.9 °C, over which it goes from silent to 2020 rpm), and worse, those points come
//!   from measurements taken under sustained load — the *descending* branch. Measured
//!   later while heating, firmware runs the fan at 0 rpm at both 53.9 °C and 64.8 °C.
//!   So the table is not just sparse, it describes the wrong branch. It survives only
//!   as a cold start, and any observation supersedes it.

/// Below this the fan does not turn at all: duty 20 measured 0 rpm, duty 30 measured
/// 1107 rpm. A duty between 1 and 29 is not a slow fan, it is a stopped one, and
/// offering it would be a control that lies.
pub const STICTION_DUTY: u8 = 30;

/// Added to every non-zero floor duty.
///
/// The duty→RPM table was measured at ~39 °C while idle. Under load at 65.8 °C the
/// same duty produced measurably less air: duty 84 gave 2808 rpm where the table
/// predicts ~2886, and where the EC itself runs 2925 rpm at 64.8 °C. The table is
/// therefore slightly optimistic exactly where the floor matters most, and a floor
/// derived from an optimistic table is a floor that is too low.
///
/// Two counts is under 1% of full scale — inaudible — and buys back more than the
/// observed shortfall. It is not applied to a zero floor: silence must stay available
/// when firmware itself is silent.
const FLOOR_MARGIN_DUTY: u8 = 2;

/// Above this temperature the floor is full duty.
///
/// Beyond the hottest EC observation (76.8 °C) there is no data, and the safe
/// direction in unmeasured territory is loud.
///
/// **Deliberately below every ceiling** in [`crate::ceiling`], so there is always a
/// band where we hold maximum airflow before giving the fan back. That ordering
/// matters because of a measured fact: the EC's curve tops out near 3100 rpm while
/// full duty reaches ~5200, so releasing to firmware *reduces* airflow. Maximum
/// cooling has to be tried first; releasing is the last resort, not the escalation.
///
/// **Was 85 °C, which was wrong**, for the same reason the old fallback ceiling was: it
/// was set from a belief that this machine tops out near 76.8 °C. Measured 92.8 °C
/// under ordinary multi-core load with firmware driving, where firmware chose duty
/// 94/255. An 85 °C threshold would have pinned the fan at 255 there — roughly 5200 rpm
/// against firmware's 3321 — in a band the machine uses routinely. Being *louder* than
/// firmware is safe, but it is not free, and "never quieter than the EC" was never a
/// licence to be arbitrarily louder.
const FULL_DUTY_ABOVE_C: f64 = 95.0;

/// What the EC does, unloaded and loaded, as measured. Temperature ascending.
const EC_CURVE: [(f64, u16); 5] = [
    (43.9, 0),
    (44.9, 0),
    (53.9, 2020),
    (64.8, 2925),
    (76.8, 3100),
];

/// What a written duty actually produces, measured 2026-08-21 at ~39 °C descending
/// from 180 so the fan never had to start from rest at a low duty. Duty ascending.
const DUTY_RPM: [(u8, u16); 12] = [
    (0, 0),
    (20, 0),
    (30, 1107),
    (40, 1512),
    (50, 1879),
    (65, 2296),
    (77, 2693),
    (90, 3052),
    (100, 3355),
    (120, 3840),
    (150, 4551),
    (180, 5201),
];

/// Width of an observation bucket, in degrees.
const BUCKET_C: f64 = 2.0;
const BUCKET_BASE_C: f64 = 30.0;
const BUCKETS: usize = 40; // 30 °C to 110 °C

/// How many hotter buckets must contradict an observation before it is treated as an
/// outlier rather than a measurement.
///
/// Firmware's **ascending branch is monotone in temperature** — it cannot need less air
/// at 46 °C than it needed at 40 °C — so a bucket whose duty exceeds every one of its
/// hotter neighbours is not a curiosity, it is proof that one of the two disagrees with
/// reality. Weight of evidence decides which: several mutually consistent buckets beat a
/// lone contradicting one.
///
/// Three, so a contradiction spans 6 °C. One quiet hotter bucket is not enough and must
/// not be, because of the asymmetry [`FirmwareFloor::record`] rests on: a quiet sample
/// proves nothing on its own — firmware may simply not have spun up yet when a 4 °C/s
/// ramp flew past — whereas a loud one is proof it will.
///
/// Repetition is deliberately **not** the test. Whatever produced the `40:51` seen on
/// 2026-09-01 held for seconds at 1 Hz, so it would have corroborated itself many times
/// over. Only the table's own internal inconsistency separates it from a measurement.
const CONTRADICTING_BUCKETS: usize = 3;

/// How far above its hotter neighbours an observation must sit before the contradiction
/// counts as an outlier rather than noise.
///
/// The EC's own duty wanders a little, and the real table shows it: the plateau above
/// 78 °C reads 153, 158, 166, 156 across neighbouring buckets, a spread of 13 counts
/// that is plainly one behaviour rather than four. Suppressing on any inconsistency at
/// all would trim those to the lowest of the group for no benefit and a real loss of
/// airflow in the band where airflow matters most.
///
/// Both anomalies actually seen are an order away from that: `40:51` against zeros is 51
/// counts, `62:184` against 79 is 105. Twenty-four sits above the measured dither and far
/// below either — and, from the duty→RPM table, is worth roughly 800 rpm, which is not
/// something a sensor mistakes.
const CONTRADICTION_DUTY: u8 = 24;

/// How far the temperature must fall below where a floor was last justified before that
/// floor is allowed to drop.
///
/// The floor is a step function over [`BUCKET_C`]-wide buckets and `peci-temp` dithers
/// by ~1 °C, so a temperature parked on a boundary alternates between two buckets'
/// duties every tick. Measured 2026-09-01 at idle: `peci-temp` crossing 39.9 ↔ 40.9 °C
/// produced `moved 0 -> 54` and `moved 54 -> 0` every couple of minutes, indefinitely —
/// an audible pulse with no thermal event behind it.
///
/// Same reasoning and same width as [`DIRECTION_HYSTERESIS_C`]: a move of one whole
/// bucket is more than the sensor's quantization can account for, so it means something.
/// The asymmetry is the safe one — a floor **rises the instant** the temperature says
/// so, and only its release is delayed.
const FLOOR_RELEASE_C: f64 = 2.0;

/// Sustained change, in degrees, before the direction is allowed to flip.
///
/// `peci-temp` is quantized to ~1 °C and dithers across a boundary. Measured on
/// hardware over a monotonic cooldown from 49.9 °C to 38.9 °C: 17 genuine falls, 104
/// steady samples — and **7 upward 1 °C blips**, none of them real heating. Flipping
/// on a single blip set `rising`, and because the direction then persists through
/// plateaus it stayed set for the rest of the descent. Carrying direction through
/// plateaus was necessary but not sufficient: the entry condition needed evidence too.
///
/// One bucket width, so a flip always means the reading moved further than the
/// sensor's own quantization can account for.
const DIRECTION_HYSTERESIS_C: f64 = 2.0;

/// Which way the temperature is going, with enough hysteresis to survive a quantized
/// sensor.
///
/// A Schmitt trigger rather than a comparison of consecutive samples. `rising` becomes
/// true only once the reading is [`DIRECTION_HYSTERESIS_C`] above the lowest seen since
/// it last fell, and false only once it is that far below the highest seen since it
/// last rose. Inside the band the previous answer stands, so neither a 1 °C blip nor a
/// plateau of any length moves it — a plateau under sustained load still reads as
/// heating, which is the single most valuable observation there is.
///
/// Asymmetric in consequence rather than in threshold: failing to record costs some
/// quiet until the bucket is relearned, whereas recording the descending branch
/// ratchets the floor to a duty firmware only uses on the way down and cannot be
/// undone, because floors only ever rise.
#[derive(Debug, Clone, Default)]
pub struct Direction {
    rising: bool,
    /// The extremes since the last flip: the trough while falling, the peak while
    /// rising. Only one is ever live.
    trough: Option<f64>,
    peak: Option<f64>,
}

impl Direction {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the machine heating?
    pub fn rising(&self) -> bool {
        self.rising
    }

    /// Feed one reading; returns the direction after it.
    ///
    /// A cold start reads as **not** rising. Assuming heating with no history was the
    /// other route descending data got in: a daemon restarted partway through a
    /// cooldown recorded its first samples as ascending, and those are sticky. Under
    /// real load the die climbs ~4 °C/s, so the threshold is cleared within a second
    /// and nothing is lost by waiting for evidence.
    ///
    /// An unreadable temperature leaves the direction untouched rather than resetting
    /// it: a dropped sample is not a change of direction.
    pub fn update(&mut self, celsius: Option<f64>) -> bool {
        let Some(c) = celsius.filter(|c| c.is_finite()) else {
            return self.rising;
        };
        let trough = self.trough.map_or(c, |t| t.min(c));
        let peak = self.peak.map_or(c, |p| p.max(c));
        if !self.rising && c >= trough + DIRECTION_HYSTERESIS_C {
            self.rising = true;
            self.peak = Some(c);
            self.trough = None;
        } else if self.rising && c <= peak - DIRECTION_HYSTERESIS_C {
            self.rising = false;
            self.trough = Some(c);
            self.peak = None;
        } else if self.rising {
            self.peak = Some(peak);
        } else {
            self.trough = Some(trough);
        }
        self.rising
    }
}

/// The EC's floor, as a duty, for the current temperature.
#[derive(Debug, Clone)]
pub struct FirmwareFloor {
    /// Highest **duty** the EC has been seen running at, per temperature bucket, while
    /// the temperature was rising or steady.
    ///
    /// Only ever raised within a bucket: a quiet sample proves nothing on its own,
    /// because firmware may not have spun up yet, whereas a loud one is proof it will.
    observed: [u8; BUCKETS],
    /// Whether anything has been recorded for a bucket. Distinguishes "firmware ran
    /// duty 0 here" — a real observation, and what firmware does below ~65 °C — from
    /// "nothing seen yet", which must fall back to the model.
    seen: [bool; BUCKETS],
    /// Bumped whenever an observation changes anything, so a caller can tell whether
    /// there is something new worth persisting without diffing the whole table.
    revision: u64,
}

impl Default for FirmwareFloor {
    fn default() -> Self {
        Self::new()
    }
}

impl FirmwareFloor {
    pub fn new() -> Self {
        Self {
            observed: [0; BUCKETS],
            seen: [false; BUCKETS],
            revision: 0,
        }
    }

    fn bucket(celsius: f64) -> Option<usize> {
        if !celsius.is_finite() || celsius < BUCKET_BASE_C {
            return None;
        }
        let idx = ((celsius - BUCKET_BASE_C) / BUCKET_C) as usize;
        (idx < BUCKETS).then_some(idx)
    }

    /// Record the duty firmware chose at this temperature.
    ///
    /// Call **only while the EC owns the fan** (`pwm1_enable=2`): under manual control
    /// `pwm1` is our own duty, and feeding it back would ratchet the floor up to
    /// whatever we last chose, forever.
    ///
    /// `rising` must be false when the machine is cooling — see the module note.
    pub fn observe(&mut self, celsius: f64, ec_duty: u8, rising: bool) {
        self.observe_span(celsius, celsius, ec_duty, rising);
    }

    /// Record firmware's duty across the whole temperature span covered since the last
    /// sample, not just the endpoint.
    ///
    /// Sampling at 1 Hz against a die sensor that climbs **4 °C per second** under load
    /// — measured: 42.9 → 64.8 °C in five seconds — means consecutive samples routinely
    /// skip whole buckets, and a floor that only ever learns the buckets it happened to
    /// land in fills in glacially. Firmware held `ec_duty` throughout the interval, so
    /// attributing it to the interval is a record of what happened rather than an
    /// interpolation of it.
    ///
    /// Conservative in the one direction that matters: if firmware raised its duty
    /// during the span, the endpoint value is the higher one, so the cooler buckets are
    /// credited with *more* airflow than firmware may have been giving them, which
    /// errs loud.
    pub fn observe_span(&mut self, from_celsius: f64, to_celsius: f64, ec_duty: u8, rising: bool) {
        if !rising {
            return;
        }
        let (lo, hi) = if from_celsius <= to_celsius {
            (from_celsius, to_celsius)
        } else {
            (to_celsius, from_celsius)
        };
        let (Some(a), Some(b)) = (Self::bucket(lo), Self::bucket(hi)) else {
            // One end outside the tracked range: still record the end that is inside.
            for c in [lo, hi] {
                if let Some(i) = Self::bucket(c) {
                    self.record(i, ec_duty);
                }
            }
            return;
        };
        for i in a..=b {
            self.record(i, ec_duty);
        }
    }

    fn record(&mut self, i: usize, ec_duty: u8) {
        if !self.seen[i] || ec_duty > self.observed[i] {
            self.observed[i] = ec_duty;
            self.seen[i] = true;
            self.revision += 1;
        }
    }

    /// Changes so far. Compare against a previously held value to know whether there
    /// is anything new to persist.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Every observation, as (temperature, duty) pairs at the low edge of each bucket.
    ///
    /// Exported by temperature rather than by bucket index so the file stays readable
    /// and survives a change to the bucket width: [`Self::restore`] re-buckets whatever
    /// it is given.
    pub fn observations(&self) -> Vec<(f64, u8)> {
        (0..BUCKETS)
            .filter(|&i| self.seen[i])
            .map(|i| (BUCKET_BASE_C + (i as f64) * BUCKET_C, self.observed[i]))
            .collect()
    }

    /// Reload observations recorded earlier.
    ///
    /// These have already passed the ascending-branch filter, so they are recorded
    /// directly. Anything outside the tracked range is dropped rather than clamped —
    /// silently folding a 200 °C entry into the top bucket would turn a corrupt file
    /// into a fan at full duty.
    pub fn restore(&mut self, observations: impl IntoIterator<Item = (f64, u8)>) {
        for (celsius, duty) in observations {
            if let Some(i) = Self::bucket(celsius) {
                self.record(i, duty);
            }
        }
    }

    /// What firmware has been observed doing here, if anything.
    pub fn observed_duty(&self, celsius: f64) -> Option<u8> {
        let i = Self::bucket(celsius)?;
        self.seen[i].then(|| self.observed[i])
    }

    /// The observation at `celsius`, with a lone contradicted outlier discounted.
    ///
    /// Floors only ever rise within a bucket, so one bad sample sticks forever — and
    /// two have now been seen: `62:184` against neighbours of 79 (~5200 rpm), and
    /// `40:51` sitting between `38:0` and six consecutive zeros running to `52:0`, which
    /// pulsed the fan at idle for as long as it stood.
    ///
    /// The check is [`CONTRADICTING_BUCKETS`] hotter *seen* buckets, every one of them
    /// at least [`CONTRADICTION_DUTY`] below this one. When they agree, the highest of
    /// them stands in — firmware demonstrably runs no harder than that anywhere hotter,
    /// so it cannot need more here. The margin keeps the EC's own few counts of dither
    /// from reading as a contradiction.
    ///
    /// **Non-destructive on purpose.** The observation is kept, not overwritten, so a
    /// hotter bucket relearning a higher duty restores the cooler one on the next read
    /// rather than requiring the table to be cleared by hand.
    pub fn trusted_duty(&self, celsius: f64) -> Option<u8> {
        let i = Self::bucket(celsius)?;
        if !self.seen[i] {
            return None;
        }
        let duty = self.observed[i];
        // Nothing below zero to fall back to, and no reason to look.
        if duty == 0 {
            return Some(0);
        }
        let hotter: Vec<u8> = (i + 1..BUCKETS)
            .filter(|&j| self.seen[j])
            .take(CONTRADICTING_BUCKETS)
            .map(|j| self.observed[j])
            .collect();
        // Too few hotter observations to overrule anything: the table simply has not
        // seen enough yet, and the loud answer is the right one meanwhile.
        if hotter.len() < CONTRADICTING_BUCKETS {
            return Some(duty);
        }
        match hotter.iter().copied().max() {
            // `saturating_sub` on the observation, not `+` on the neighbour: the
            // comparison must not wrap when a bucket is near full duty.
            Some(highest) if highest < duty.saturating_sub(CONTRADICTION_DUTY) => Some(highest),
            _ => Some(duty),
        }
    }

    /// The RPM the model predicts firmware would run at this temperature.
    ///
    /// Cold start only. [`Self::floor_duty`] prefers a direct observation whenever one
    /// exists, because measuring firmware beats interpolating two tables about it.
    pub fn modelled_rpm(&self, celsius: f64) -> u16 {
        interpolate_rpm(celsius)
    }

    /// The lowest duty we may hold at this temperature.
    ///
    /// Zero when firmware would have the fan off — the point of the clamp is to track
    /// the EC, and the EC runs the fan at 0 rpm below ~45 °C. A silent idle is
    /// therefore allowed, which is most of why anyone wants this feature.
    pub fn floor_duty(&self, celsius: f64) -> u8 {
        // Catch an unreadable temperature before the bucket lookup silently drops it.
        if !celsius.is_finite() {
            return u8::MAX;
        }
        // A direct observation of firmware wins outright, including an observation of
        // duty 0. Firmware running the fan off at 60 °C while heating is not a gap in
        // our knowledge — it is the answer. `trusted_duty` rather than `observed_duty`,
        // so a single sample the rest of the table contradicts cannot be that answer.
        if let Some(duty) = self.trusted_duty(celsius) {
            if duty == 0 {
                return 0;
            }
            return duty.saturating_add(FLOOR_MARGIN_DUTY).max(STICTION_DUTY);
        }
        let rpm = self.modelled_rpm(celsius);
        if rpm == 0 {
            return 0;
        }
        duty_for_rpm(rpm)
            .saturating_add(FLOOR_MARGIN_DUTY)
            .max(STICTION_DUTY)
    }

    /// Raise `duty` to the floor if it sits below it.
    ///
    /// Returns the duty to write and whether the clamp bit. Callers must surface the
    /// second half: ADR 0006 warns that a control silently ignoring the user reads as
    /// a bug.
    pub fn clamp(&self, duty: u8, celsius: f64) -> (u8, bool) {
        let floor = self.floor_duty(celsius);
        if duty < floor {
            (floor, true)
        } else {
            (duty, false)
        }
    }
}

/// Hysteresis on the floor itself, so a sensor dithering across a bucket edge cannot
/// chatter the fan.
///
/// [`FirmwareFloor::floor_duty`] is a step function — `54:0` next to `56:66` is a jump of
/// 68 counts across one boundary — and `peci-temp` is quantized to ~1 °C and wanders
/// either side of wherever it sits. Read literally every tick, that produces a fan
/// switching between two duties at 1 Hz with nothing thermal happening at all. Measured
/// at idle on 2026-09-01, before this existed: `moved 0 -> 54` then `moved 54 -> 0` three
/// seconds later, repeating for as long as the machine was left alone.
///
/// **Raise immediately, lower lazily.** The unsafe direction is being slow to speed the
/// fan up, so a rise is never delayed by even one tick; only the drop waits, until the
/// temperature has fallen [`FLOOR_RELEASE_C`] below where the held floor was last
/// justified. The cost of the lag is some quiet, which is the direction this whole module
/// is allowed to err in.
///
/// Kept separate from [`FirmwareFloor`] because it is per-*controller* state rather than
/// knowledge about the hardware: it belongs to whoever is driving the fan right now, and
/// resets when the lease changes hands.
#[derive(Debug, Clone, Default)]
pub struct FloorHold {
    /// The floor being held, and the temperature at which it was last justified.
    held: Option<(u8, f64)>,
}

impl FloorHold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed this tick's temperature and the floor the table gives for it; returns the
    /// floor to actually enforce.
    pub fn update(&mut self, celsius: f64, floor_now: u8) -> u8 {
        // An unreadable sensor is not a boundary crossing, and it is emphatically not a
        // reason to hold a stale floor: report what was asked and remember nothing.
        if !celsius.is_finite() {
            return floor_now;
        }
        match self.held {
            // Falling, and not yet clear of where the current floor was justified.
            Some((duty, at)) if floor_now < duty && celsius > at - FLOOR_RELEASE_C => duty,
            // Rising, equal, or far enough below to let go. Both the duty and the
            // temperature that justifies it are refreshed together, so a climb re-arms
            // the hysteresis at the new height rather than leaving it anchored low.
            _ => {
                self.held = Some((floor_now, celsius));
                floor_now
            }
        }
    }

    /// The floor currently being enforced, if any tick has run.
    pub fn held(&self) -> Option<u8> {
        self.held.map(|(duty, _)| duty)
    }

    /// Forget the held floor. For a change of fan ownership, where the previous
    /// controller's hysteresis means nothing.
    pub fn reset(&mut self) {
        self.held = None;
    }
}

/// The EC's RPM at `celsius`, interpolated between measured points.
fn interpolate_rpm(celsius: f64) -> u16 {
    if !celsius.is_finite() {
        // A sensor we cannot read is not permission to run the fan slowly.
        return u16::MAX;
    }
    let (last_c, last_rpm) = EC_CURVE[EC_CURVE.len() - 1];
    if celsius >= FULL_DUTY_ABOVE_C {
        return u16::MAX;
    }
    if celsius > last_c {
        // Unmeasured: ramp from the last known point to "as fast as possible" by the
        // time we reach FULL_DUTY_ABOVE_C.
        let span = FULL_DUTY_ABOVE_C - last_c;
        let t = (celsius - last_c) / span;
        let top = f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1);
        return lerp(f64::from(last_rpm), top, t) as u16;
    }
    if celsius <= EC_CURVE[0].0 {
        return EC_CURVE[0].1;
    }
    for w in EC_CURVE.windows(2) {
        let ((c0, r0), (c1, r1)) = (w[0], w[1]);
        if celsius <= c1 {
            let t = (celsius - c0) / (c1 - c0);
            return lerp(f64::from(r0), f64::from(r1), t) as u16;
        }
    }
    last_rpm
}

/// The lowest duty that reaches at least `rpm`, rounding **up**.
fn duty_for_rpm(rpm: u16) -> u8 {
    if rpm == 0 {
        return 0;
    }
    let top = DUTY_RPM[DUTY_RPM.len() - 1];
    if rpm >= top.1 {
        // Beyond anything measured: full duty. There is nothing faster to offer.
        return u8::MAX;
    }
    for w in DUTY_RPM.windows(2) {
        let ((d0, r0), (d1, r1)) = (w[0], w[1]);
        if rpm <= r1 {
            if r1 == r0 {
                return d1;
            }
            let t = (f64::from(rpm) - f64::from(r0)) / (f64::from(r1) - f64::from(r0));
            let duty = lerp(f64::from(d0), f64::from(d1), t);
            // Ceil, never round: landing one count short of firmware is the failure
            // this whole module exists to prevent.
            return duty.ceil().min(255.0) as u8;
        }
    }
    u8::MAX
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_allows_a_silent_fan() {
        // The EC runs the fan at 0 rpm below ~45 C, so we may too. If this ever
        // returns non-zero, the quiet-at-idle case the product exists for is gone.
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(30.0), 0);
        assert_eq!(f.floor_duty(43.9), 0);
        assert_eq!(f.floor_duty(44.9), 0);
    }

    #[test]
    fn floor_tracks_the_measured_ec_curve() {
        let f = FirmwareFloor::new();
        // Measured: EC at 2925 rpm at 64.8 C, and duty 90 produces 3052 rpm while
        // duty 77 produces only 2693. So the floor must be above 77.
        let d = f.floor_duty(64.8);
        assert!((78..=90).contains(&d), "floor at 64.8 C was {d}");

        // 53.9 C -> 2020 rpm, which sits between duty 50 (1879) and 65 (2296).
        let d = f.floor_duty(53.9);
        assert!((51..=65).contains(&d), "floor at 53.9 C was {d}");
    }

    #[test]
    fn the_floor_rises_with_temperature() {
        let f = FirmwareFloor::new();
        let mut previous = 0;
        for t in [40.0, 50.0, 55.0, 60.0, 65.0, 70.0, 75.0, 80.0, 85.0] {
            let d = f.floor_duty(t);
            assert!(d >= previous, "floor fell from {previous} to {d} at {t} C");
            previous = d;
        }
    }

    #[test]
    fn rounds_up_never_down() {
        // Landing a count short of firmware defeats the entire purpose.
        for rpm in [1u16, 1108, 1500, 2000, 2700, 3000, 3500] {
            let d = duty_for_rpm(rpm);
            let produced = interpolate_duty_rpm(d);
            assert!(
                produced >= f64::from(rpm) - 0.5,
                "duty {d} produces {produced} rpm, short of {rpm}"
            );
        }
    }

    /// Forward lookup, only used to check that `duty_for_rpm` did not round down.
    fn interpolate_duty_rpm(duty: u8) -> f64 {
        if duty >= DUTY_RPM[DUTY_RPM.len() - 1].0 {
            return f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1);
        }
        for w in DUTY_RPM.windows(2) {
            let ((d0, r0), (d1, r1)) = (w[0], w[1]);
            if duty <= d1 {
                let t = (f64::from(duty) - f64::from(d0)) / (f64::from(d1) - f64::from(d0));
                return lerp(f64::from(r0), f64::from(r1), t);
            }
        }
        f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1)
    }

    #[test]
    fn never_offers_a_duty_that_cannot_turn_the_fan() {
        // Anything in 1..STICTION_DUTY is a stopped fan pretending otherwise.
        let f = FirmwareFloor::new();
        for t in [46.0, 48.0, 50.0, 52.0] {
            let d = f.floor_duty(t);
            assert!(
                d == 0 || d >= STICTION_DUTY,
                "floor at {t} C was {d}, which cannot turn the fan"
            );
        }
    }

    #[test]
    fn an_unreadable_sensor_demands_full_duty() {
        // No temperature is not permission to run the fan slowly.
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(f64::NAN), 255);
    }

    #[test]
    fn very_hot_means_full_duty() {
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(FULL_DUTY_ABOVE_C), 255);
        assert_eq!(f.floor_duty(110.0), 255);
        // And the unmeasured band above the last EC point ramps rather than flattening.
        assert!(f.floor_duty(85.0) > f.floor_duty(76.8));
    }

    #[test]
    fn full_duty_is_reached_before_any_ceiling_releases_the_fan() {
        // The ordering invariant. If a ceiling were ever to sit below this, the fan
        // would be handed back to firmware - at a lower RPM than we were capable of -
        // without ever having run flat out.
        // const blocks: this ordering is a compile-time fact about the constants, so
        // getting it wrong should fail the build, not wait for someone to run tests.
        use crate::ceiling::{FALLBACK_CEILING_C, MAX_CEILING_C};
        const { assert!(FULL_DUTY_ABOVE_C < FALLBACK_CEILING_C) };
        const { assert!(FULL_DUTY_ABOVE_C < MAX_CEILING_C) };

        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(FALLBACK_CEILING_C), 255);
    }

    #[test]
    fn a_direct_observation_beats_the_model() {
        let mut f = FirmwareFloor::new();
        let modelled = f.floor_duty(48.0);

        // Firmware seen running duty 90 at 48 C is proof it does that, whatever the
        // model interpolated from two tables.
        f.observe(48.0, 90, true);
        assert!(f.floor_duty(48.0) > modelled);
        assert_eq!(f.observed_duty(48.0), Some(90));
    }

    #[test]
    fn within_a_bucket_a_quiet_sample_does_not_undo_a_loud_one() {
        // Firmware may simply not have spun up yet, so quiet is weak evidence while
        // loud is proof.
        let mut f = FirmwareFloor::new();
        f.observe(48.0, 90, true);
        let learned = f.floor_duty(48.0);
        f.observe(48.0, 0, true);
        assert_eq!(f.floor_duty(48.0), learned);
    }

    #[test]
    fn firmware_running_the_fan_off_is_an_answer_not_a_gap() {
        // The heart of the redesign. Measured while heating, firmware runs duty 0 at
        // 60 C. The model, built from descending-branch measurements, demands ~70
        // there. Observation must win, or the quiet this feature exists for is
        // unreachable.
        let mut f = FirmwareFloor::new();
        assert!(
            f.floor_duty(60.0) > 0,
            "model should demand airflow at 60 C"
        );

        f.observe(60.0, 0, true);
        assert_eq!(f.floor_duty(60.0), 0, "observed silence must be honoured");
    }

    #[test]
    fn a_fast_ramp_fills_every_bucket_it_crossed() {
        // The machine climbs ~4 C/s under load, so 1 Hz sampling skips buckets. What
        // firmware was doing across the whole jump is known, and recording only the
        // endpoint would leave the floor learning almost nothing per heating event.
        let mut f = FirmwareFloor::new();
        f.observe_span(42.9, 64.8, 0, true);

        for c in [44.0, 48.0, 52.0, 56.0, 60.0, 64.0] {
            assert_eq!(f.observed_duty(c), Some(0), "bucket at {c} C not filled");
            assert_eq!(f.floor_duty(c), 0, "floor at {c} C should permit silence");
        }
        // Outside the span is untouched.
        assert_eq!(f.observed_duty(70.0), None);
    }

    #[test]
    fn a_span_never_lowers_a_louder_observation() {
        let mut f = FirmwareFloor::new();
        f.observe(50.0, 120, true);
        f.observe_span(42.0, 60.0, 0, true);
        assert_eq!(f.observed_duty(50.0), Some(120), "quiet must not undo loud");
        assert_eq!(f.observed_duty(58.0), Some(0));
    }

    #[test]
    fn observations_survive_a_round_trip() {
        let mut a = FirmwareFloor::new();
        a.observe_span(42.0, 64.0, 0, true);
        a.observe(70.0, 90, true);
        let saved = a.observations();
        assert!(!saved.is_empty());

        let mut b = FirmwareFloor::new();
        b.restore(saved);
        for c in [44.0, 52.0, 60.0, 64.0, 70.0] {
            assert_eq!(b.floor_duty(c), a.floor_duty(c), "differs at {c} C");
        }
    }

    #[test]
    fn restoring_nonsense_does_not_produce_a_screaming_fan() {
        // A corrupt or hand-edited file must not fold out-of-range entries into the
        // hottest bucket, which would pin the floor at full duty.
        let mut f = FirmwareFloor::new();
        f.restore([(500.0, 255), (-40.0, 255), (f64::NAN, 255)]);
        assert!(f.observed_duty(108.0).is_none());
        assert_eq!(f.floor_duty(40.0), 0);
    }

    #[test]
    fn the_revision_only_moves_when_something_changes() {
        let mut f = FirmwareFloor::new();
        assert_eq!(f.revision(), 0);
        f.observe(60.0, 90, true);
        let after = f.revision();
        assert!(after > 0);

        // A quieter sample in the same bucket changes nothing, so there is nothing new
        // to write.
        f.observe(60.0, 10, true);
        assert_eq!(f.revision(), after);
        // And a cooling sample is discarded entirely.
        f.observe(60.0, 200, false);
        assert_eq!(f.revision(), after);
    }

    #[test]
    fn cooling_observations_are_discarded() {
        // At 61.9 C firmware runs duty 0 heating and 92 cooling. Recording the
        // descending branch would ratchet the floor to a duty firmware only uses while
        // shedding heat, and would cost a silent idle for ten minutes after any load.
        let mut f = FirmwareFloor::new();
        f.observe(61.9, 92, false);
        assert_eq!(f.observed_duty(61.9), None, "cooling must not be recorded");

        f.observe(61.9, 0, true);
        assert_eq!(f.floor_duty(61.9), 0);
    }

    #[test]
    fn the_floor_beats_the_ec_at_every_measured_point() {
        // The invariant, checked against firmware's own measured behaviour rather than
        // against our model of it. Hardware caught a violation here that the model
        // was happy with, so assert on the measurements directly.
        let f = FirmwareFloor::new();
        for (celsius, ec_rpm) in EC_CURVE {
            if ec_rpm == 0 {
                continue;
            }
            let duty = f.floor_duty(celsius);
            let ours = interpolate_duty_rpm(duty);
            assert!(
                ours >= f64::from(ec_rpm),
                "at {celsius} C the EC runs {ec_rpm} rpm but our floor of {duty}/255 \
                 produces only {ours:.0} rpm"
            );
        }
    }

    #[test]
    fn clamp_reports_whether_it_bit() {
        let f = FirmwareFloor::new();
        // Idle: the user's choice stands, including silence.
        assert_eq!(f.clamp(0, 40.0), (0, false));
        // Hot: raised, and the caller is told so it can explain itself.
        let (duty, clamped) = f.clamp(0, 64.8);
        assert!(clamped);
        assert!(duty >= 78, "got {duty}");
    }

    #[test]
    fn observations_outside_the_bucket_range_are_ignored_not_panicking() {
        let mut f = FirmwareFloor::new();
        f.observe(-40.0, 200, true);
        f.observe(500.0, 200, true);
        f.observe(f64::NAN, 200, true);
        assert_eq!(f.floor_duty(40.0), 0);
    }

    #[test]
    fn the_ceiling_no_longer_fires_where_the_machine_actually_operates() {
        // 92.8 C measured under ordinary multi-core load. Full duty must not be
        // demanded there: firmware itself chose 94/255.
        const { assert!(FULL_DUTY_ABOVE_C > 92.8) };
    }

    /// The exact sequence measured on hardware on 2026-08-23: a cooldown that only
    /// ever went down, carrying seven spurious 1 C upward blips. The previous
    /// implementation flipped to rising on the first of them and stayed there, which
    /// is how the 44-54 C band came to hold firmware's descending duties.
    #[test]
    fn a_quantized_cooldown_never_reads_as_heating() {
        let mut d = Direction::default();
        // Arrive at the top of the descent already heating, as a real run does.
        for t in [40.0, 44.0, 48.0, 49.9] {
            d.update(Some(t));
        }
        assert!(d.rising(), "the climb should register");

        let cooldown = [
            49.9, 48.9, 47.9, 46.9, 45.9, 44.9, 43.9, 42.9, 43.9, // blip
            42.9, 41.9, 42.9, // blip
            41.9, 41.9, 40.9, 39.9, 38.9, 39.9, // blip
            38.9, 39.9, // blip
            38.9, 38.9, 39.9, // blip
            38.9,
        ];
        let mut fell = false;
        for t in cooldown {
            let rising = d.update(Some(t));
            if !rising {
                fell = true;
            }
            // Once the descent is established, no blip may undo it.
            if fell {
                assert!(!rising, "a 1 C blip at {t} C read as heating");
            }
        }
        assert!(!d.rising(), "should be falling at the end of a cooldown");
    }

    #[test]
    fn a_plateau_under_sustained_load_still_counts_as_heating() {
        // The other half, and why the fix is hysteresis rather than "ignore steady":
        // holding at temperature under load is firmware stating what that temperature
        // needs, and it must be recorded.
        let mut d = Direction::default();
        for t in [60.0, 65.0, 70.0] {
            d.update(Some(t));
        }
        assert!(d.rising());
        for _ in 0..100 {
            assert!(
                d.update(Some(70.0)),
                "a plateau while heating is sustained load"
            );
        }
        // A single 1 C dip is sensor noise, not the end of the climb.
        assert!(d.update(Some(69.0)), "one dip must not end the climb");
    }

    #[test]
    fn a_cold_start_does_not_assume_heating() {
        // Assuming heating with no history is how a daemon restarted partway through a
        // cooldown recorded its first samples as ascending. Those are sticky, because
        // floors only ever rise.
        let mut d = Direction::default();
        assert!(!d.update(Some(40.0)));
        assert!(!d.rising());
    }

    #[test]
    fn a_sustained_climb_is_recognised_promptly() {
        // The cost of waiting for evidence has to stay small: the die climbs ~4 C/s
        // under load, so one 1 Hz sample is enough.
        let mut d = Direction::default();
        d.update(Some(38.9));
        assert!(d.update(Some(42.9)), "4 C in one sample is unambiguous");
    }

    #[test]
    fn an_unreadable_sample_does_not_change_direction() {
        let mut d = Direction::default();
        for t in [40.0, 44.0] {
            d.update(Some(t));
        }
        assert!(d.rising());
        assert!(
            d.update(None),
            "a dropped sample is not a change of direction"
        );
        assert!(d.update(Some(f64::NAN)), "NaN is not a change of direction");
    }
    /// `/var/lib/fw-helper/state` as it stood on 2026-09-01, verbatim. `40:51` is the
    /// anomaly that pulsed the fan at idle; everything else is a real observation and
    /// must survive untouched.
    const TABLE_2026_09_01: [(f64, u8); 35] = [
        (30.0, 0),
        (32.0, 0),
        (34.0, 0),
        (36.0, 0),
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
        (60.0, 79),
        (62.0, 84),
        (64.0, 89),
        (66.0, 94),
        (68.0, 153),
        (70.0, 153),
        (72.0, 153),
        (74.0, 158),
        (76.0, 158),
        (78.0, 153),
        (80.0, 153),
        (82.0, 153),
        (84.0, 158),
        (86.0, 166),
        (88.0, 156),
        (90.0, 156),
        (92.0, 156),
        (94.0, 156),
        (96.0, 153),
        (98.0, 153),
    ];

    fn measured() -> FirmwareFloor {
        let mut f = FirmwareFloor::new();
        f.restore(TABLE_2026_09_01);
        f
    }

    #[test]
    fn the_idle_pulse_outlier_is_rejected() {
        let f = measured();
        // What the daemon logged, over and over: 40.9 C demanded 53 while 39.9 C
        // demanded nothing, three seconds apart, with the machine doing nothing.
        assert_eq!(f.observed_duty(40.9), Some(51), "the record is kept");
        assert_eq!(
            f.floor_duty(40.9),
            0,
            "42 through 46 C all saw firmware silent; 40 C cannot need more air than they do"
        );
        assert_eq!(f.floor_duty(39.9), 0);
    }

    #[test]
    fn an_observation_its_hotter_neighbours_agree_with_stands() {
        // 54:51 sat under the same suspicion as 40:51 and is a different case: 56, 58
        // and 60 C all recorded MORE duty, which is exactly what a monotone curve does.
        let f = measured();
        assert_eq!(f.floor_duty(54.9), 51 + FLOOR_MARGIN_DUTY);
    }

    #[test]
    fn the_ecs_own_dither_is_not_a_contradiction() {
        // The plateau above 78 C reads 153, 158, 166, 156. Trimming 86 C to its
        // neighbours would cost airflow in the band that needs it, for noise.
        let f = measured();
        assert_eq!(f.floor_duty(86.9), 166 + FLOOR_MARGIN_DUTY);
        assert_eq!(f.floor_duty(74.9), 158 + FLOOR_MARGIN_DUTY);
    }

    #[test]
    fn one_quiet_hotter_bucket_cannot_overrule_a_loud_one() {
        // The asymmetry `record` rests on: a quiet sample may only mean firmware had
        // not spun up yet when a 4 C/s ramp flew past. One of them proves nothing.
        let mut f = FirmwareFloor::new();
        f.restore([(66.0, 94), (68.0, 0), (70.0, 153), (72.0, 153)]);
        assert_eq!(f.floor_duty(66.9), 94 + FLOOR_MARGIN_DUTY);
    }

    #[test]
    fn too_few_hotter_observations_to_overrule_anything() {
        // Two zeros are not evidence; the loud answer stands until the table has seen
        // enough to say otherwise.
        let mut f = FirmwareFloor::new();
        f.restore([(40.0, 51), (42.0, 0), (44.0, 0)]);
        assert_eq!(f.floor_duty(40.9), 51 + FLOOR_MARGIN_DUTY);
        f.restore([(46.0, 0)]);
        assert_eq!(
            f.floor_duty(40.9),
            0,
            "three agreeing neighbours is the bar"
        );
    }

    #[test]
    fn suppression_lifts_when_a_hotter_bucket_relearns() {
        // Non-destructive: the observation is discounted, not erased, so watching
        // firmware actually run the fan at 46 C brings 40 C back without hand-editing
        // the state file.
        let mut f = measured();
        assert_eq!(f.floor_duty(40.9), 0);
        f.observe(46.9, 80, true);
        assert_eq!(
            f.floor_duty(40.9),
            51 + FLOOR_MARGIN_DUTY,
            "the record was kept, so it can be restored"
        );
    }

    #[test]
    fn a_floor_rises_the_instant_the_table_says_so() {
        // The unsafe direction, and the one that is never delayed.
        let mut h = FloorHold::new();
        assert_eq!(h.update(50.0, 0), 0);
        assert_eq!(h.update(56.9, 68), 68, "no lag on the way up");
    }

    #[test]
    fn dithering_across_a_bucket_edge_does_not_pulse_the_fan() {
        // The 2026-09-01 defect in miniature, at the 54/56 boundary rather than the
        // 38/40 one: without the hold this alternates 53, 68, 53, 68 at 1 Hz.
        let f = measured();
        let mut h = FloorHold::new();
        let mut seen = Vec::new();
        for c in [56.9, 55.9, 56.9, 55.9, 56.9, 55.9] {
            seen.push(h.update(c, f.floor_duty(c)));
        }
        assert_eq!(seen, vec![68, 68, 68, 68, 68, 68], "one duty, not two");
    }

    #[test]
    fn the_floor_does_drop_once_the_temperature_is_clear() {
        // Hysteresis, not a ratchet. A machine that has genuinely cooled gets its
        // silence back.
        let f = measured();
        let mut h = FloorHold::new();
        assert_eq!(h.update(56.9, f.floor_duty(56.9)), 68);
        assert_eq!(h.update(55.9, f.floor_duty(55.9)), 68, "inside the band");
        assert_eq!(
            h.update(54.8, f.floor_duty(54.8)),
            53,
            "a full bucket below where 68 was justified"
        );
        assert_eq!(h.update(51.0, f.floor_duty(51.0)), 0);
    }

    #[test]
    fn a_climb_re_arms_the_hysteresis_at_the_new_height() {
        // Otherwise the band stays anchored to the temperature of the first crossing
        // and a much hotter machine would be allowed to drop after 2 C of cooling from
        // wherever it started.
        let f = measured();
        let mut h = FloorHold::new();
        h.update(56.9, f.floor_duty(56.9));
        assert_eq!(h.update(60.9, f.floor_duty(60.9)), 79 + FLOOR_MARGIN_DUTY);
        assert_eq!(
            h.update(59.9, f.floor_duty(59.9)),
            79 + FLOOR_MARGIN_DUTY,
            "1 C below 60.9 is inside the band"
        );
        assert_eq!(h.update(58.5, f.floor_duty(58.5)), 74 + FLOOR_MARGIN_DUTY);
    }

    #[test]
    fn an_unreadable_temperature_neither_holds_nor_is_held() {
        // A dropped sample is not a boundary crossing, and it is not licence to keep
        // enforcing a floor justified by a temperature we can no longer see.
        let mut h = FloorHold::new();
        h.update(56.9, 68);
        assert_eq!(h.update(f64::NAN, u8::MAX), u8::MAX);
        assert_eq!(h.held(), Some(68), "and it remembered nothing about NaN");
    }

    #[test]
    fn a_reset_forgets_the_held_floor() {
        let mut h = FloorHold::new();
        h.update(56.9, 68);
        h.reset();
        assert_eq!(h.held(), None);
        assert_eq!(h.update(55.9, 53), 53, "no hysteresis to inherit");
    }
}
