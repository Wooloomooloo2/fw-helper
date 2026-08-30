//! How fan speed is written down.
//!
//! The fan is *commanded* in duty, 0–255, and that is what a curve stores and what the
//! daemon sends. But almost nobody thinks in duty counts, and the number people can act
//! on — compare against a spec, or against how loud the machine sounds — is RPM. So the
//! window shows RPM by default and keeps duty available for anyone editing a curve
//! against the firmware floor, which is itself expressed in duty.
//!
//! The conversion is [`fw_helper_core::fan::rpm_for_duty`], an interpolation of the
//! measured table. It is approximate by construction and the labels say so with a `~`:
//! the same duty turns the fan at different speeds depending on temperature and airflow.

use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FanUnits {
    /// What people recognise. The default.
    #[default]
    Rpm,
    /// What the hardware is actually told, and what the firmware floor is drawn in.
    Duty,
}

/// Shared between the window and the curve editor, which both render duties.
pub type Units = Rc<Cell<FanUnits>>;

impl FanUnits {
    /// A duty written the chosen way, for a sentence like "manual · {this}".
    pub fn describe(self, duty: u8) -> String {
        match self {
            Self::Duty => format!("duty {duty}/255"),
            Self::Rpm => match fw_helper_core::fan::rpm_for_duty(duty) {
                // Zero is a state worth naming. "~0 rpm" reads like a measurement that
                // happens to be small, when the fan is simply not turning.
                0 => "fan off".to_string(),
                rpm => format!("~{rpm} rpm"),
            },
        }
    }

    /// Axis gridlines for the curve plot: the duty to draw a line at, and its label.
    ///
    /// In RPM the gridlines **are the measured points**, which is the honest choice:
    /// duty→RPM is concave, so evenly spaced duties produce unevenly spaced speeds, and
    /// evenly spaced speeds would need an inverse of a table that is flat above duty 200
    /// — two gridlines would land on the same label. Using the measurements sidesteps
    /// both and puts the lines exactly where something is actually known.
    pub fn gridlines(self) -> Vec<(u8, String)> {
        match self {
            Self::Duty => [0u8, 64, 128, 192, 255]
                .iter()
                .map(|&d| (d, d.to_string()))
                .collect(),
            Self::Rpm => [0u8, 30, 77, 120, 180, 200]
                .iter()
                .map(|&d| (d, fw_helper_core::fan::rpm_for_duty(d).to_string()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gridlines_never_repeat_a_label() {
        // The failure this guards: duty→RPM is flat above 200, so evenly spaced duties
        // put 192 and 255 on the same speed and the axis grows two identical lines.
        for units in [FanUnits::Rpm, FanUnits::Duty] {
            let labels: Vec<_> = units.gridlines().into_iter().map(|(_, l)| l).collect();
            let mut unique = labels.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(labels.len(), unique.len(), "{units:?} repeats a label");
        }
    }

    #[test]
    fn a_stopped_fan_is_named_not_numbered() {
        assert_eq!(FanUnits::Rpm.describe(0), "fan off");
        assert_eq!(FanUnits::Duty.describe(0), "duty 0/255");
    }

    #[test]
    fn rpm_is_the_default() {
        // Deliberate: duty is the unit the hardware takes, not the one people read.
        assert_eq!(FanUnits::default(), FanUnits::Rpm);
    }
}
