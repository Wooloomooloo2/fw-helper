//! How fan speed is written down.
//!
//! The fan is *commanded* in duty, 0–255, and that is what a curve stores, what the
//! spinners edit and what the firmware floor is expressed in. But almost nobody thinks
//! in duty counts, and the number people can act on — compare against a spec, or against
//! how loud the machine sounds — is RPM. So every duty the window writes down carries
//! its speed beside it. No mode, no toggle: both are always visible, because the two
//! answer different questions and neither replaces the other.
//!
//! The conversion is [`fw_helper_core::fan::rpm_for_duty`], an interpolation of the
//! measured table. It is approximate by construction and the labels say so with a `~`:
//! the same duty turns the fan at different speeds depending on temperature and airflow.

/// A duty and the speed it produces, for a sentence like "manual · {this}".
pub fn describe_duty(duty: u8) -> String {
    match fw_helper_core::fan::rpm_for_duty(duty) {
        // Zero is a state worth naming. "~0 rpm" reads like a measurement that happens
        // to be small, when the fan is simply not turning.
        0 => format!("duty {duty}/255 (fan off)"),
        rpm => format!("duty {duty}/255 (~{rpm} rpm)"),
    }
}

/// Just the speed, for putting next to a control that already shows the duty.
pub fn rpm_hint(duty: u8) -> String {
    match fw_helper_core::fan::rpm_for_duty(duty) {
        0 => "fan off".to_string(),
        rpm => format!("~{rpm} rpm"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_fan_is_named_not_numbered() {
        assert_eq!(rpm_hint(0), "fan off");
        assert!(describe_duty(0).contains("fan off"));
    }

    #[test]
    fn the_stiction_band_reads_as_stopped_rather_than_slow() {
        // Duty 1-29 cannot turn this fan. A speed hint that ramped gently up from zero
        // would describe a state the hardware has no way to be in.
        for duty in 1..30u8 {
            assert_eq!(rpm_hint(duty), "fan off", "duty {duty}");
        }
        assert!(rpm_hint(30).starts_with("~1107"));
    }

    #[test]
    fn a_duty_always_carries_its_speed() {
        // The whole point of dropping the toggle: neither unit is ever hidden.
        let s = describe_duty(120);
        assert!(s.contains("duty 120/255"), "got: {s}");
        assert!(s.contains("rpm"), "got: {s}");
    }
}
