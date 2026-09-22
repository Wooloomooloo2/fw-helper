//! `fw-helper-restore-fan` — undo everything the daemon can hold, and say whether it took.
//!
//! This exists for the case the daemon cannot handle itself: it was `SIGKILL`ed, it
//! deadlocked, it panicked inside its own panic hook. systemd runs it as
//! `ExecStopPost=`, so it fires on *every* stop of the unit including the crash
//! paths, and it is safe to run by hand at any time (ADR 0006 point 1).
//!
//! Writing `2` to `pwm1_enable` when the fan is already in EC control is a no-op, so
//! there is nothing to detect and no reason to be clever. Being boring is the point:
//! the machine may be hot and unattended when this runs.
//!
//! **It also re-onlines parked CPU cores and lifts a GPU frequency cap (ADR 0014).**
//! Named for the fan because that is what it started as, and renaming a binary the
//! installed unit references is not worth the breakage.
//!
//! The cores are here rather than only in the daemon's startup reclaim because those
//! are not the same guarantee. A startup reclaim needs a startup: it fixes a `SIGKILL`
//! only because `Restart=on-failure` happens to bring the daemon back, and does nothing
//! at all for `systemctl stop` of a wedged process, a masked unit, or a daemon removed
//! between the kill and the next boot. `ExecStopPost=` runs on **every** stop of the
//! unit, which is the property ADR 0006 relies on and which ADR 0014 claimed parity
//! with. Measured 2026-09-22: it did not have it.

use fw_helper_core::tune::{CoreParking, GpuFreq};
use fw_helper_core::{FanControl, FanMode, Sysfs};
use std::process::ExitCode;

fn main() -> ExitCode {
    let fs = Sysfs::default();

    // The fan first, always: it is the only one of the three that can hurt the machine.
    let fan_ok = restore_fan(&fs);
    // Then the cheap, boring ones. Neither can fail in a way worth aborting for, and
    // both are no-ops on a machine that was never tuned.
    restore_cores(&fs);
    restore_gpu(&fs);

    // Only the fan decides the exit code. As `ExecStopPost=` with `ignore_errors=no`,
    // failing here marks the unit's stop as failed, and that signal is reserved for the
    // one outcome that can damage hardware.
    if fan_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Re-online every CPU that has an `online` file.
///
/// Deliberately does no classification. An offline core loses its `cpufreq/` and
/// `topology/` directories, so a parked machine cannot be described in terms of
/// clusters at all — but `online` survives, and "write 1 to all of them" needs to know
/// nothing else. Writing 1 to an already-online CPU is a no-op.
fn restore_cores(fs: &Sysfs) {
    let parking = CoreParking::new(fs);
    let offline: Vec<u32> = parking
        .core_set()
        .cpus()
        .iter()
        .filter(|c| c.parkable && !parking.is_online(c.num))
        .map(|c| c.num)
        .collect();
    if offline.is_empty() {
        return;
    }
    let n = offline.len();
    match parking.restore_all() {
        Ok(()) => eprintln!("fw-helper-restore-fan: re-onlined {n} parked CPU core(s)"),
        Err(e) => eprintln!(
            "fw-helper-restore-fan: FAILED to re-online {n} parked CPU core(s) ({e}). \
             Nothing else on this machine will bring them back — write 1 to each \
             /sys/devices/system/cpu/cpu*/online by hand"
        ),
    }
}

/// Hand the GT its full frequency range back.
///
/// `reset` is a no-op when the window already matches the hardware's own range, so
/// there is nothing to detect here either.
fn restore_gpu(fs: &Sysfs) {
    let gpu = GpuFreq::new(fs);
    let (Some(max), Some((_, rp0))) = (gpu.max(), gpu.range()) else {
        return;
    };
    if max >= rp0 {
        return;
    }
    match gpu.reset() {
        Ok(()) => eprintln!("fw-helper-restore-fan: GPU was capped at {max} MHz, now {rp0} MHz"),
        Err(e) => eprintln!("fw-helper-restore-fan: FAILED to lift the GPU cap ({e})"),
    }
}

fn restore_fan(fs: &Sysfs) -> bool {
    let fan = match FanControl::probe(fs) {
        Ok(f) => f,
        Err(e) => {
            // No fan control on this machine means nothing to restore. Say so and
            // succeed — as ExecStopPost, failing here would mark every clean stop
            // of the unit as failed on hardware that never had a fan to take.
            eprintln!("fw-helper-restore-fan: no fan to restore ({e})");
            return true;
        }
    };

    let was = fan.mode().unwrap_or(FanMode::Other(0));

    if fan.release_best_effort() {
        match was {
            FanMode::Auto => eprintln!("fw-helper-restore-fan: fan was already EC automatic"),
            other => eprintln!("fw-helper-restore-fan: fan was {other}, now EC automatic"),
        }
        true
    } else {
        // The one genuinely bad outcome, and the reason this prints to stderr rather
        // than exiting quietly: the fan may be held at a fixed duty with nothing
        // refreshing it, and nobody is going to notice a silent failure here.
        eprintln!(
            "fw-helper-restore-fan: FAILED to restore EC fan control (fan reports {}). \
             Are you root? Write 2 to the cros_ec hwmon's pwm1_enable by hand.",
            fan.mode().unwrap_or(FanMode::Other(0))
        );
        false
    }
}
