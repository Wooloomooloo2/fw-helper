//! Profiles: the thing a user actually picks.
//!
//! A profile is a PPD profile *plus* the knobs PPD does not manage (ADR 0005). The
//! daemon delegates the PPD axis over D-Bus rather than writing `platform_profile` or
//! EPP itself, because GNOME's power slider is wired to PPD and last-writer-wins
//! against it is the worst bug class in this project.
//!
//! **The layers compose rather than duplicate.** A profile picks a power budget and a
//! curve; the firmware floor, the ceiling and the battery guard still apply on top of
//! that curve exactly as they do to a hand-set duty. Nothing here can make the machine
//! unsafe, which is why the curve values below can be chosen for how they sound.
//!
//! The three defaults are grounded in measurement, not taste: 10 W of PL1 is worth
//! about 12 °C, so the power budget does most of the thermal work and the curve only
//! has to cover what is left. That is also why the quiet curve can afford to be silent
//! to 55 °C — at 15 W the machine sits around 62 °C under sustained load.

use crate::curve::{Curve, Point};
use crate::tune::ParkLevel;

/// The PPD axis. These are PPD's own three profiles and the names it uses on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ppd {
    PowerSaver,
    Balanced,
    Performance,
}

impl Ppd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "power-saver" => Some(Self::PowerSaver),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }
}

/// Why a profile was rejected. Every variant names something the author can fix.
#[derive(Debug, PartialEq)]
pub enum ProfileError {
    EmptyName,
    NameNotSimple(String),
    UnknownPpd(String),
    PowerOutOfRange(u32),
    ChargeOutOfRange(u8),
    GpuFreqOutOfRange(u32),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "a profile needs a name"),
            Self::NameNotSimple(n) => write!(
                f,
                "profile name {n:?} must be lowercase letters, digits and dashes: it is \
                 what a user types and what appears on the D-Bus interface"
            ),
            Self::UnknownPpd(p) => write!(
                f,
                "{p:?} is not a power-profiles-daemon profile; expected power-saver, \
                 balanced or performance"
            ),
            Self::PowerOutOfRange(w) => write!(
                f,
                "{w} W is outside the range a power limit can sensibly take ({}-{} W)",
                crate::power::MIN_WATTS,
                crate::power::MAX_WATTS
            ),
            Self::ChargeOutOfRange(v) => write!(f, "{v}% is not a usable charge limit"),
            Self::GpuFreqOutOfRange(v) => {
                write!(f, "{v} MHz is not a plausible GPU clock")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Our name for it, as a user types it.
    pub name: String,
    /// What PPD is asked to switch to.
    pub ppd: Ppd,
    /// Sustained CPU power budget.
    pub pl1_watts: u32,
    /// The fan curve this profile runs.
    pub curve: Curve,
    /// How much of the CPU this profile wants running.
    ///
    /// **This is a placement lever, not a power one** — see [`crate::tune`]. Default
    /// [`ParkLevel::None`] so every existing profile and config file means exactly what
    /// it meant before this field existed.
    pub park_cores: ParkLevel,
    /// Cap the GPU's maximum clock, in MHz.
    ///
    /// `None` leaves the full range, which is what every shipped profile does. **The
    /// cap is unproven on this hardware** — lowering `max_freq` is at least the right
    /// direction, unlike the `min_freq` peg already known to be inert, but whether it
    /// binds is M9 Phase 0 question C. Available to a user profile; not set by a
    /// built-in until it is measured.
    pub gpu_max_mhz: Option<u32>,
    /// Charge limit, if this profile should set one.
    ///
    /// **`None` in all three built-ins, deliberately.** ADR 0005's sketch included a
    /// charge limit in the profile, but battery longevity is a standing preference, not
    /// a performance choice — someone who caps at 80% to preserve the pack does not
    /// want that undone by asking for more speed for an hour. The field exists so a
    /// user-defined profile can opt in; the defaults leave the setting alone.
    pub charge_limit: Option<u8>,
}

fn curve(points: &[(f64, u8)]) -> Curve {
    Curve::new(
        points
            .iter()
            .map(|&(celsius, duty)| Point { celsius, duty })
            .collect(),
    )
    .expect("built-in curves are valid")
}

impl Profile {
    /// Silent as long as possible. At 15 W the machine settles around 62 °C under
    /// sustained load, so a curve that starts at 55 °C rarely has to do anything.
    pub fn quiet() -> Self {
        Self {
            name: "quiet".into(),
            ppd: Ppd::PowerSaver,
            pl1_watts: 15,
            curve: curve(&[
                (55.0, 0),
                (62.0, 40),
                (70.0, 65),
                (80.0, 92),
                (90.0, 130),
                (100.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// The default. 20 W lands about 6 °C above quiet, and the curve starts earlier to
    /// absorb it.
    pub fn balanced() -> Self {
        Self {
            name: "balanced".into(),
            ppd: Ppd::Balanced,
            pl1_watts: 20,
            curve: curve(&[
                (50.0, 0),
                (60.0, 45),
                (68.0, 72),
                (78.0, 100),
                (88.0, 145),
                (100.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// Stock power, and a curve that trades noise for headroom. Measured, the machine
    /// reaches 92.8 °C at 25 W under firmware's own curve; this one is working well
    /// before that.
    pub fn performance() -> Self {
        Self {
            name: "performance".into(),
            ppd: Ppd::Performance,
            pl1_watts: 25,
            curve: curve(&[
                (45.0, 0),
                (55.0, 50),
                (65.0, 85),
                (75.0, 120),
                (85.0, 170),
                (95.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// 30 W. The first profile that spends the headroom `max_power_uw` was hiding:
    /// measured, 30 W is worth +8.9% throughput over 25 W for 20% more power, with the
    /// cores at 76 °C and no throttling at all.
    ///
    /// The curve has to start earlier and climb harder than performance's, because the
    /// power budget is no longer doing most of the thermal work — above 25 W the curve
    /// is what keeps the machine in range.
    pub fn turbo() -> Self {
        Self {
            name: "turbo".into(),
            ppd: Ppd::Performance,
            pl1_watts: 30,
            curve: curve(&[
                (40.0, 0),
                (50.0, 60),
                (60.0, 100),
                (70.0, 140),
                (80.0, 190),
                (90.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// 35 W, everything this board will give. Above it firmware simply stops listening:
    /// a 40 W setpoint held the register and still settled at 35.07 W (see
    /// [`crate::power::MAX_WATTS`]), so there is no profile beyond this one to write.
    ///
    /// Worth +15.9% over 25 W, and it costs noise: measured at 35 W the cores reach
    /// 84 °C and peci 91.8 °C even with the fan at ~5850 rpm. The curve is accordingly
    /// the loudest thing here, and is meant to be chosen deliberately.
    pub fn max() -> Self {
        Self {
            name: "max".into(),
            ppd: Ppd::Performance,
            pl1_watts: 35,
            curve: curve(&[
                (40.0, 0),
                (48.0, 70),
                (58.0, 115),
                (68.0, 160),
                (78.0, 210),
                (88.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// For a GPU-bound title. **Not an unlock — a budget that stops being wasted.**
    ///
    /// There is very little withheld GPU performance on this machine, and saying so is
    /// part of the design. Measured: Cyberpunk 2077 at PL1 **25 W scores 48.01 fps and
    /// at 35 W scores 48.16**, with the GPU already at its clock ceiling and 96-97%
    /// occupied. So this profile takes the 25 W — identical frames for 10 W less heat
    /// and about 156 rpm less fan — and spends the difference on a curve that starts
    /// early, where our curves are measured to beat firmware by 13-36 duty counts.
    ///
    /// Parks nothing: a title waiting on the GPU is not helped by having fewer cores.
    pub fn game() -> Self {
        Self {
            name: "game".into(),
            ppd: Ppd::Performance,
            pl1_watts: 25,
            curve: curve(&[
                (45.0, 0),
                (55.0, 75),
                (65.0, 120),
                (75.0, 165),
                (85.0, 210),
                (95.0, 255),
            ]),
            park_cores: ParkLevel::None,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// For older titles and emulation — one hot thread that wants the fastest core.
    ///
    /// The measured case for this is a game reporting itself CPU-bound while nothing
    /// looks busy: Horizon Zero Dawn gives **CPU FPS 34 against GPU FPS 45 while no
    /// thread exceeds 50% and the busiest core sits at 47%**. That is a latency-bound
    /// critical thread, and on a hybrid part the usual cause is the scheduler placing
    /// it on an E core at 3.7 GHz or an LP-E core at 3.3 instead of a P core at 4.8.
    ///
    /// Defaults to [`ParkLevel::Lpe`], not `PCoresOnly`. Removing only the slowest
    /// cluster is where the placement argument is strongest and the downside smallest —
    /// 12 cores still remain. `p-only` is a real choice and is one command away, but it
    /// is wrong for anything that threads well: RPCS3 emulates SPUs across many
    /// threads and would likely lose. Which of the two wins is a question for a
    /// benchmark, not for this file.
    ///
    /// Sets no GPU cap. Freeing package budget by throttling the GT is plausible and
    /// unmeasured, and a shipped default is the wrong place to find out.
    pub fn retro() -> Self {
        Self {
            name: "retro".into(),
            ppd: Ppd::Performance,
            pl1_watts: 35,
            curve: curve(&[
                (45.0, 0),
                (55.0, 70),
                (65.0, 115),
                (75.0, 160),
                (85.0, 205),
                (95.0, 255),
            ]),
            park_cores: ParkLevel::Lpe,
            gpu_max_mhz: None,
            charge_limit: None,
        }
    }

    /// The power ladder: strictly ascending in watts, and never quieter as it climbs.
    ///
    /// This is the axis the GNOME slider moves along, and the ordering is a real
    /// invariant rather than a presentation choice — a "faster" profile that asked for
    /// less air than the one below it would be a worse machine by every measure.
    /// Enforced by a test over the whole list, so a new rung cannot quietly break it.
    ///
    /// `turbo` and `max` share the `performance` PPD position with `performance` itself.
    /// That is fine and is why [`Self::canonical_name_for`] exists: the GNOME slider
    /// still lands on `performance`, and the extra two are reached by name.
    pub fn ladder() -> Vec<Self> {
        vec![
            Self::quiet(),
            Self::balanced(),
            Self::performance(),
            Self::turbo(),
            Self::max(),
        ]
    }

    /// Workload presets, which are **not** rungs on the ladder.
    ///
    /// `game` draws less than `max` and `retro` draws the same, so neither belongs in a
    /// monotone ordering: they are not "more performance", they are performance shaped
    /// differently. Kept separate so the ladder's invariant stays meaningful, and
    /// listed after it, which is also the order a user reads them in.
    pub fn workload_presets() -> Vec<Self> {
        vec![Self::game(), Self::retro()]
    }

    /// Everything shipped: the ladder, then the workload presets.
    pub fn built_ins() -> Vec<Self> {
        let mut all = Self::ladder();
        all.extend(Self::workload_presets());
        all
    }

    /// Validate a profile assembled from somewhere less trustworthy than this file.
    ///
    /// Range-checking the power budget here is a courtesy, not the guarantee:
    /// [`crate::PowerLimit::set`] clamps against the zone's real maximum at apply time,
    /// and it is the one that matters. This exists so a typo in a config file is
    /// reported when the file is read rather than when the profile is first used.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ProfileError::NameNotSimple(self.name.clone()));
        }
        if self.pl1_watts < crate::power::MIN_WATTS || self.pl1_watts > crate::power::MAX_WATTS {
            return Err(ProfileError::PowerOutOfRange(self.pl1_watts));
        }
        if let Some(mhz) = self.gpu_max_mhz {
            // A real bound needs the hardware, which this function does not have. This
            // only catches a value that cannot be a frequency at all; `GpuFreq::set_max`
            // clamps against rpn_freq..rp0_freq at apply time and is the one that binds.
            if !(100..=4000).contains(&mhz) {
                return Err(ProfileError::GpuFreqOutOfRange(mhz));
            }
        }
        if let Some(limit) = self.charge_limit {
            if !(crate::charge::MIN_LIMIT..=crate::charge::MAX_LIMIT).contains(&limit) {
                return Err(ProfileError::ChargeOutOfRange(limit));
            }
        }
        Ok(())
    }

    /// The canonical name a PPD profile maps to.
    ///
    /// **User profiles never take part in this mapping**, even one that names the same
    /// PPD profile. When the GNOME slider moves, the machine must land somewhere
    /// predictable; picking between several user profiles that all claim `power-saver`
    /// would be a coin toss the user cannot see. A user profile that *replaces* a
    /// built-in by name is used here, because that is an explicit choice.
    pub fn canonical_name_for(ppd: Ppd) -> &'static str {
        match ppd {
            Ppd::PowerSaver => "quiet",
            Ppd::Balanced => "balanced",
            Ppd::Performance => "performance",
        }
    }

    /// The built-in matching a PPD profile. Callers holding a merged set should prefer
    /// looking up [`Self::canonical_name_for`] in that set.
    pub fn for_ppd(ppd: Ppd) -> Self {
        let name = Self::canonical_name_for(ppd);
        Self::built_ins()
            .into_iter()
            .find(|p| p.name == name)
            .expect("every PPD profile has a built-in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ppd_profile_maps_to_one_of_ours() {
        // If the GNOME slider can reach a state we have no profile for, the desktop and
        // this daemon disagree about what the machine is doing - the exact failure
        // ADR 0005 exists to prevent.
        for ppd in [Ppd::PowerSaver, Ppd::Balanced, Ppd::Performance] {
            assert_eq!(Profile::for_ppd(ppd).ppd, ppd);
            assert_eq!(Profile::for_ppd(ppd).name, Profile::canonical_name_for(ppd));
        }
    }

    #[test]
    fn ppd_names_round_trip_on_the_wire() {
        for ppd in [Ppd::PowerSaver, Ppd::Balanced, Ppd::Performance] {
            assert_eq!(Ppd::parse(ppd.as_str()), Some(ppd));
        }
        assert_eq!(Ppd::parse("turbo"), None);
    }

    #[test]
    fn rejects_profiles_a_config_file_could_get_wrong() {
        let base = Profile::quiet();
        let with = |f: fn(&mut Profile)| {
            let mut p = base.clone();
            f(&mut p);
            p.validate().unwrap_err()
        };
        assert_eq!(with(|p| p.name.clear()), ProfileError::EmptyName);
        assert!(matches!(
            with(|p| p.name = "My Profile".into()),
            ProfileError::NameNotSimple(_)
        ));
        assert!(matches!(
            with(|p| p.pl1_watts = 2),
            ProfileError::PowerOutOfRange(2)
        ));
        assert!(matches!(
            with(|p| p.charge_limit = Some(5)),
            ProfileError::ChargeOutOfRange(5)
        ));
    }

    #[test]
    fn profiles_are_ordered_by_power_and_by_noise() {
        // built_ins() is declared in ascending power order, and each step up must ask
        // for at least as much air as the one below it at every temperature. Written
        // over the whole list so adding a profile cannot quietly break the ordering.
        let all = Profile::ladder();
        for pair in all.windows(2) {
            let (lo, hi) = (&pair[0], &pair[1]);
            assert!(
                lo.pl1_watts < hi.pl1_watts,
                "{} does not draw less than {}",
                lo.name,
                hi.name
            );
            for t in [50.0, 60.0, 70.0, 80.0, 90.0] {
                assert!(
                    lo.curve.duty_at(t) <= hi.curve.duty_at(t),
                    "{} is louder than {} at {t} C",
                    lo.name,
                    hi.name
                );
            }
        }
    }

    #[test]
    fn the_top_profile_matches_what_the_board_will_actually_give() {
        // 35 W is not a taste decision: above it a setpoint is accepted and ignored, so
        // a profile asking for more would advertise throughput the machine cannot make.
        assert_eq!(Profile::max().pl1_watts, crate::power::MAX_WATTS);
    }

    #[test]
    fn sharing_a_ppd_position_does_not_disturb_the_slider() {
        // `performance`, `turbo`, `max`, `game` and `retro` all claim the same PPD
        // position. However many there are, the slider must land somewhere predictable
        // — a coin toss between them is the ADR 0005 failure in miniature. Asserted as
        // a property rather than a count, so adding a profile cannot break it.
        for ppd in [Ppd::PowerSaver, Ppd::Balanced, Ppd::Performance] {
            let claimants: Vec<String> = Profile::built_ins()
                .into_iter()
                .filter(|p| p.ppd == ppd)
                .map(|p| p.name)
                .collect();
            let canonical = Profile::canonical_name_for(ppd);
            assert!(
                claimants.iter().any(|n| n == canonical),
                "{ppd:?} resolves to {canonical}, which is not among {claimants:?}"
            );
            assert_eq!(Profile::for_ppd(ppd).name, canonical);
        }
    }

    #[test]
    fn workload_presets_stay_off_the_ladder() {
        // The whole reason they are a separate list. `game` draws 25 W where `max`
        // draws 35, so folding them in would either break the ordering or require
        // pretending `game` is a step down from `performance`, which it is not.
        let ladder: Vec<String> = Profile::ladder().into_iter().map(|p| p.name).collect();
        for preset in Profile::workload_presets() {
            assert!(
                !ladder.contains(&preset.name),
                "{} must not be a rung on the ladder",
                preset.name
            );
        }
        assert_eq!(Profile::built_ins().len(), ladder.len() + 2);
    }

    #[test]
    fn the_shipped_profiles_set_no_unproven_gpu_cap() {
        // Lowering the GT ceiling is plausible and unmeasured on this board (M9 Phase 0
        // question C). A shipped default is the wrong place to find out, so the control
        // exists and nothing built-in uses it.
        for p in Profile::built_ins() {
            assert_eq!(p.gpu_max_mhz, None, "{} ships a GPU cap", p.name);
        }
    }

    #[test]
    fn retro_parks_the_slowest_cluster_and_no_more() {
        // Not PCoresOnly: removing only the 3.3 GHz cluster is where the placement
        // argument is strongest and the downside smallest, and 12 cores still remain.
        // `p-only` is a real choice but wrong for anything that threads well.
        assert_eq!(Profile::retro().park_cores, ParkLevel::Lpe);
        assert_eq!(Profile::game().park_cores, ParkLevel::None);
    }

    #[test]
    fn game_takes_the_measured_optimum_not_the_maximum() {
        // CP2077: 25 W scores 48.01 fps, 35 W scores 48.16. The extra 10 W buys 0.3%,
        // so `game` spends it on being cooler and quieter instead. If this ever changes
        // to 35, something measured has to have changed first.
        assert_eq!(Profile::game().pl1_watts, 25);
        assert!(Profile::game().pl1_watts < Profile::max().pl1_watts);
    }

    #[test]
    fn the_quiet_profile_is_actually_silent_where_it_matters() {
        // At 15 W the machine sits around 62 C under sustained load. If the quiet curve
        // is already working there, the profile is not quiet.
        let q = Profile::quiet();
        assert_eq!(q.curve.duty_at(50.0), 0);
        assert_eq!(q.curve.duty_at(55.0), 0);
        assert!(q.curve.duty_at(62.0) <= 45);
    }

    #[test]
    fn no_built_in_profile_touches_the_charge_limit() {
        // Battery longevity is a standing preference, not a performance choice.
        for p in Profile::built_ins() {
            assert_eq!(p.charge_limit, None, "{} sets a charge limit", p.name);
        }
    }

    #[test]
    fn power_budgets_stay_inside_the_measured_envelope() {
        for p in Profile::built_ins() {
            assert!(
                p.validate().is_ok(),
                "built-in {} does not validate",
                p.name
            );
        }
    }
}
