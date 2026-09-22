//! End-to-end tests against a synthetic sysfs tree.
//!
//! This is the payoff from ADR 0004's rooted-path design: capability probing and
//! telemetry are exercised with no hardware, no root, and no network — so they run
//! in CI. The fixture mirrors the real values captured in docs/hardware-baseline.md.

use fw_helper_core::fan::MIN_TAKEOVER_DUTY;
use fw_helper_core::tune::{Cluster, CoreParking, CoreSet, GpuFreq, ParkLevel, TuneError};
use fw_helper_core::{Capabilities, FanControl, FanError, FanMode, Monitor, Sysfs};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helper-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        Self { root }
    }

    fn write(&self, rel: &str, contents: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn sysfs(&self) -> Sysfs {
        Sysfs::new(&self.root)
    }

    /// A Framework 13 Pro as actually observed: EC hwmon present, RAPL present,
    /// charge control declined to bind but the override parameter exists.
    fn framework_13(tag: &str) -> Self {
        let f = Fixture::new(tag);
        // hwmon3 is the battery, hwmon7 the EC — deliberately not the real indices,
        // to prove lookup is by name and not by number.
        f.write("sys/class/hwmon/hwmon3/name", "BAT1\n");
        f.write("sys/class/hwmon/hwmon7/name", "cros_ec\n");
        f.write("sys/class/hwmon/hwmon7/pwm1_enable", "2\n");
        f.write("sys/class/hwmon/hwmon7/pwm1", "0\n");
        f.write("sys/class/hwmon/hwmon7/fan1_input", "2925\n");
        f.write("sys/class/hwmon/hwmon7/temp1_input", "36850\n");
        f.write("sys/class/hwmon/hwmon7/temp1_label", "local_f75397@4c\n");
        f.write("sys/class/hwmon/hwmon7/temp1_crit", "87850\n");
        f.write("sys/class/hwmon/hwmon7/temp5_input", "64800\n");
        f.write("sys/class/hwmon/hwmon7/temp5_label", "peci-temp\n");
        f.write("sys/class/hwmon/hwmon7/temp5_crit", "119850\n");
        // the unset threshold that would otherwise poison fan safety
        f.write("sys/class/hwmon/hwmon7/temp5_max", "-273150\n");

        let rapl = "sys/class/powercap/intel-rapl-mmio:0";
        f.write(&format!("{rapl}/name"), "package-0\n");
        f.write(&format!("{rapl}/constraint_0_power_limit_uw"), "25000000\n");
        f.write(&format!("{rapl}/constraint_0_max_power_uw"), "25000000\n");
        f.write(&format!("{rapl}/energy_uj"), "1000000\n");
        f.write(&format!("{rapl}/max_energy_range_uj"), "262143328850\n");

        // coretemp is a separate chip from the EC, and the reference machine lists
        // `Package id 0` among sixteen per-core sensors in no useful order.
        f.write("sys/class/hwmon/hwmon9/name", "coretemp\n");
        f.write("sys/class/hwmon/hwmon9/temp1_label", "Core 0\n");
        f.write("sys/class/hwmon/hwmon9/temp1_input", "49000\n");
        f.write("sys/class/hwmon/hwmon9/temp1_crit", "100000\n");
        f.write("sys/class/hwmon/hwmon9/temp5_label", "Package id 0\n");
        f.write("sys/class/hwmon/hwmon9/temp5_input", "50000\n");
        f.write("sys/class/hwmon/hwmon9/temp5_crit", "100000\n");

        f.write("sys/firmware/acpi/platform_profile", "balanced\n");
        f.write("sys/class/power_supply/BAT1/capacity", "100\n");
        // Named ACAD on this board, so resolution must be by type, not by name.
        f.write("sys/class/power_supply/ACAD/type", "Mains\n");
        f.write("sys/class/power_supply/ACAD/online", "1\n");
        f.write("sys/class/power_supply/BAT1/status", "Not charging\n");
        // Charge family, as this board reports: no power_now, no energy_now.
        f.write("sys/class/power_supply/BAT1/current_now", "540000\n");
        f.write("sys/class/power_supply/BAT1/voltage_now", "16436000\n");
        f.write("sys/class/power_supply/BAT1/charge_now", "3464000\n");
        f.write(
            "sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control",
            "N\n",
        );
        f
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn probes_a_framework_13_correctly() {
    let f = Fixture::framework_13("probe");
    let caps = Capabilities::probe(&f.sysfs());

    assert!(caps.fan_control.is_available());
    assert!(caps.power_limit.is_available());
    assert!(caps.platform_profile.is_available());
    assert!(caps.package_power.is_available());

    // Charge control must be reported unavailable *with the actionable reason*,
    // not merely absent — this is the ADR 0008 case.
    assert!(!caps.charge_limit.is_available());
    assert!(
        format!("{}", caps.charge_limit).contains("probe_with_fwk_charge_control"),
        "reason should tell the user how to fix it, got: {}",
        caps.charge_limit
    );
}

#[test]
fn finds_hwmon_by_name_not_index() {
    let f = Fixture::framework_13("hwmon");
    let caps = Capabilities::probe(&f.sysfs());
    assert_eq!(caps.ec_hwmon.as_deref(), Some("sys/class/hwmon/hwmon7"));
}

#[test]
fn charge_limit_available_when_the_driver_bound_on_its_own() {
    // Threshold present *and* the override parameter still N, i.e. the kernel decided
    // the standard command is safe here. That is the only case we may call available.
    let f = Fixture::framework_13("charge-ok");
    f.write(
        "sys/class/power_supply/BAT1/charge_control_end_threshold",
        "80\n",
    );
    assert!(Capabilities::probe(&f.sysfs()).charge_limit.is_available());
}

#[test]
fn charge_limit_unavailable_when_the_binding_was_forced() {
    // The target machine's real state, and the one that shipped a lie: the attribute
    // exists, accepts writes and reads them back — while the EC charges past the limit
    // to full. Existence of the node is not evidence the limit works, so a forced
    // binding must report unavailable rather than Cap::Yes.
    let f = Fixture::framework_13("charge-forced");
    f.write(
        "sys/class/power_supply/BAT1/charge_control_end_threshold",
        "80\n",
    );
    f.write(
        "sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control",
        "Y\n",
    );
    let caps = Capabilities::probe(&f.sysfs());
    assert!(!caps.charge_limit.is_available());
    let why = format!("{}", caps.charge_limit);
    // The reason must give the user something to do, per the capability rule.
    assert!(why.contains("UEFI setup"), "got: {why}");
}

#[test]
fn degrades_gracefully_with_no_hardware() {
    let f = Fixture::new("empty");
    f.write("sys/placeholder", "\n"); // root exists but is otherwise bare
    let caps = Capabilities::probe(&f.sysfs());

    // Nothing panics, and every knob explains itself.
    for (name, cap) in caps.summary() {
        assert!(!cap.is_available(), "{name} should be unavailable");
        assert!(!format!("{cap}").is_empty(), "{name} must carry a reason");
    }
}

#[test]
fn samples_telemetry_and_picks_the_control_sensor() {
    let f = Fixture::framework_13("telemetry");
    let mut mon = Monitor::new(f.sysfs());
    let t = mon.sample();

    assert_eq!(t.fan_rpm, Some(2925));
    assert_eq!(
        t.on_ac,
        Some(true),
        "mains must be found by type, not by name"
    );
    assert_eq!(t.battery_percent, Some(100));
    assert_eq!(t.platform_profile.as_deref(), Some("balanced"));
    assert_eq!(t.temps.len(), 3);

    let ctrl = t.control_temp().expect("a control sensor");
    assert_eq!(ctrl.label, "peci-temp");
    assert_eq!(ctrl.celsius, 64.8);
    assert_eq!(ctrl.critical, Some(119.85));

    // The CPU package, found by label among the per-core sensors. Its critical point is
    // Tjmax, 100 C - the usable one. peci-temp above declares 119.85 C, which is above
    // Tjmax and therefore cannot be drawn as a limit.
    let package = t
        .temps
        .iter()
        .find(|r| r.label == fw_helper_core::PACKAGE_TEMP_LABEL)
        .expect("the coretemp package sensor");
    assert_eq!(package.celsius, 50.0);
    assert_eq!(package.critical, Some(100.0));
    // And it must not have displaced the fan curve's input.
    assert_ne!(ctrl.label, fw_helper_core::PACKAGE_TEMP_LABEL);

    // First sample has no prior reference, so power is not yet derivable.
    assert_eq!(t.package_watts, None);
}

/// Read a fixture file back as a trimmed string — asserts on what actually reached
/// "hardware", rather than on what the API claims it did.
fn raw(f: &Fixture, rel: &str) -> String {
    fs::read_to_string(f.root.join(rel))
        .unwrap()
        .trim()
        .to_string()
}

const EC: &str = "sys/class/hwmon/hwmon7";

#[test]
fn takes_manual_control_and_hands_it_back() {
    let f = Fixture::framework_13("fan-lease");
    let fs = f.sysfs();
    let fan = FanControl::probe(&fs).expect("fixture has a cros_ec hwmon");

    assert_eq!(fan.mode().unwrap(), FanMode::Auto);

    fan.take_manual(200).unwrap();
    assert_eq!(fan.mode().unwrap(), FanMode::Manual);
    assert_eq!(fan.duty().unwrap(), 200);
    // The mode switch and the duty both landed in sysfs, not just in our own state.
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "1");
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "200");

    fan.set_duty(120).unwrap();
    assert_eq!(fan.duty().unwrap(), 120);

    fan.release().unwrap();
    assert_eq!(fan.mode().unwrap(), FanMode::Auto);
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "2");
}

#[test]
fn duty_is_attempted_before_the_mode_switch() {
    // Real hardware refuses this pre-write with EOPNOTSUPP (measured 2026-08-21), so
    // on the reference machine it changes nothing and the takeover window stays open.
    // The fixture is writable in either mode, so what this pins down is that we still
    // *try* — the ordering is a genuine safety gain on any EC that permits it, and a
    // later refactor must not quietly drop it.
    let f = Fixture::framework_13("fan-order");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "0");
    fan.take_manual(180).unwrap();
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "180");
}

#[test]
fn refuses_to_set_duty_while_the_ec_owns_the_fan() {
    let f = Fixture::framework_13("fan-auto");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    // Would be silently ignored by real hardware, so it must be an error here.
    assert!(matches!(
        fan.set_duty(200),
        Err(FanError::NotUnderManualControl(FanMode::Auto))
    ));
    assert_eq!(
        raw(&f, &format!("{EC}/pwm1")),
        "0",
        "nothing should be written"
    );
}

#[test]
fn release_is_idempotent() {
    // Every exit path calls this, including ones that run after another already did.
    let f = Fixture::framework_13("fan-idempotent");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    fan.release().unwrap();
    fan.release().unwrap();
    assert!(fan.release_best_effort());
    assert_eq!(fan.mode().unwrap(), FanMode::Auto);
}

#[test]
fn a_duty_that_cannot_turn_the_fan_never_reaches_hardware() {
    let f = Fixture::framework_13("fan-unsafe");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    assert!(matches!(
        fan.take_manual(MIN_TAKEOVER_DUTY - 1),
        Err(FanError::DutyCannotTurnFan(_))
    ));
    // The refusal must happen before any write: the fan is still the EC's.
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "2");
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "0");
}

#[test]
fn reads_the_charge_family_when_discharging() {
    // The reference board reports current/voltage/charge and has no power_now at all,
    // so a reader that only understands the energy family shows nothing.
    let f = Fixture::framework_13("battery-rate");
    f.write("sys/class/power_supply/BAT1/status", "Discharging\n");
    let mut mon = Monitor::new(f.sysfs());
    let t = mon.sample();

    // 0.54 A x 16.436 V
    let watts = t.system_watts.expect("a discharge rate");
    assert!((watts - 8.875).abs() < 0.01, "got {watts} W");
    // 3464 mAh at 540 mA is about 6 h 25 m.
    assert_eq!(t.battery_minutes, Some(384));
}

#[test]
fn prefers_the_energy_family_where_the_board_reports_it() {
    let f = Fixture::framework_13("battery-energy");
    f.write("sys/class/power_supply/BAT1/status", "Discharging\n");
    f.write("sys/class/power_supply/BAT1/power_now", "9000000\n");
    f.write("sys/class/power_supply/BAT1/energy_now", "45000000\n");
    let mut mon = Monitor::new(f.sysfs());
    let t = mon.sample();

    assert_eq!(t.system_watts, Some(9.0));
    assert_eq!(t.battery_minutes, Some(300), "45 Wh at 9 W is five hours");
}

// ---------------------------------------------------------------------------------
// M9 — core parking and GPU frequency
// ---------------------------------------------------------------------------------

/// The reference machine's topology, as measured 2026-09-22: no SMT, P 0-3 (core_id
/// 0/4/8/12 at 4.7-4.8 GHz), E 4-11 (16-23 at 3.7), LP-E 12-15 (32-35 at 3.3), and
/// **cpu0 with no `online` file at all**.
fn with_hybrid_cpus(f: &Fixture) {
    f.write("sys/devices/cpu_core/cpus", "0-3\n");
    f.write("sys/devices/cpu_atom/cpus", "4-15\n");
    let spec: [(u32, u32, u32); 16] = [
        (0, 0, 4700000),
        (1, 4, 4800000),
        (2, 8, 4700000),
        (3, 12, 4700000),
        (4, 16, 3700000),
        (5, 17, 3700000),
        (6, 18, 3700000),
        (7, 19, 3700000),
        (8, 20, 3700000),
        (9, 21, 3700000),
        (10, 22, 3700000),
        (11, 23, 3700000),
        (12, 32, 3300000),
        (13, 33, 3300000),
        (14, 34, 3300000),
        (15, 35, 3300000),
    ];
    for (n, core_id, khz) in spec {
        f.write(
            &format!("sys/devices/system/cpu/cpu{n}/topology/core_id"),
            &format!("{core_id}\n"),
        );
        f.write(
            &format!("sys/devices/system/cpu/cpu{n}/cpufreq/cpuinfo_max_freq"),
            &format!("{khz}\n"),
        );
        // cpu0 deliberately has none: the kernel pins the boot CPU.
        if n != 0 {
            f.write(&format!("sys/devices/system/cpu/cpu{n}/online"), "1\n");
        }
    }
}

/// The GPU is `card1`, not card0, and `xe` names its nodes nothing like i915.
fn with_xe_gpu(f: &Fixture) {
    f.write("sys/class/drm/card0/dev", "226:0\n"); // a display-only node, no GT
    let gt = "sys/class/drm/card1/device/tile0/gt0/freq0";
    f.write(&format!("{gt}/max_freq"), "2500\n");
    f.write(&format!("{gt}/min_freq"), "900\n");
    f.write(&format!("{gt}/cur_freq"), "2500\n");
    f.write(&format!("{gt}/act_freq"), "1850\n");
    f.write(&format!("{gt}/rp0_freq"), "2500\n");
    f.write(&format!("{gt}/rpe_freq"), "900\n");
    f.write(&format!("{gt}/rpn_freq"), "100\n");
}

#[test]
fn discovers_three_clusters_without_trusting_cpu_numbers() {
    let f = Fixture::framework_13("topology");
    with_hybrid_cpus(&f);
    let set = CoreSet::probe(&f.sysfs());

    assert_eq!(set.cpus().len(), 16);
    assert_eq!(set.clusters(), vec![Cluster::P, Cluster::E, Cluster::LpE]);
    assert_eq!(set.in_cluster(Cluster::P).len(), 4);
    assert_eq!(set.in_cluster(Cluster::E).len(), 8);
    assert_eq!(set.in_cluster(Cluster::LpE).len(), 4);
    assert_eq!(set.in_cluster(Cluster::LpE)[0].num, 12);
}

#[test]
fn cpu0_is_never_parkable() {
    let f = Fixture::framework_13("cpu0");
    with_hybrid_cpus(&f);
    let set = CoreSet::probe(&f.sysfs());

    let cpu0 = set.cpus().iter().find(|c| c.num == 0).unwrap();
    assert!(
        !cpu0.parkable,
        "cpu0 has no online file and must not be offered"
    );
    assert!(set.cpus().iter().filter(|c| c.num != 0).all(|c| c.parkable));
}

#[test]
fn park_levels_select_the_right_clusters() {
    let f = Fixture::framework_13("levels");
    with_hybrid_cpus(&f);
    let set = CoreSet::probe(&f.sysfs());

    assert!(set.to_park(ParkLevel::None).is_empty());
    assert_eq!(set.to_park(ParkLevel::Lpe), vec![12, 13, 14, 15]);
    assert_eq!(
        set.to_park(ParkLevel::PCoresOnly),
        vec![4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
}

#[test]
fn parking_round_trips_and_reads_back() {
    let f = Fixture::framework_13("park");
    with_hybrid_cpus(&f);
    let fs = f.sysfs();
    let park = CoreParking::new(&fs);
    assert!(park.is_supported());
    assert_eq!(park.read(), Some(ParkLevel::None));

    park.apply(ParkLevel::PCoresOnly).unwrap();
    assert_eq!(park.read(), Some(ParkLevel::PCoresOnly));
    assert!(!park.is_online(4));
    assert!(park.is_online(0), "cpu0 must survive every level");
    assert!(park.is_online(3));

    // Transitioning between two parked levels must not dip below either.
    park.apply(ParkLevel::Lpe).unwrap();
    assert_eq!(park.read(), Some(ParkLevel::Lpe));
    assert!(park.is_online(4));
    assert!(!park.is_online(12));

    park.restore_all().unwrap();
    assert_eq!(park.read(), Some(ParkLevel::None));
}

#[test]
fn a_hand_offlined_core_reads_as_mixed_not_a_level() {
    let f = Fixture::framework_13("mixed");
    with_hybrid_cpus(&f);
    let fs = f.sysfs();
    fs.write_string("sys/devices/system/cpu/cpu7/online", "0")
        .unwrap();

    // Reported, not silently corrected — the user may have done it deliberately.
    assert_eq!(CoreParking::new(&fs).read(), None);
}

#[test]
fn gpu_freq_resolves_card1_and_not_card0() {
    let f = Fixture::framework_13("gpu");
    with_xe_gpu(&f);
    let fs = f.sysfs();
    let gpu = GpuFreq::new(&fs);

    assert!(gpu.is_supported());
    assert!(gpu.path().unwrap().contains("card1"), "{:?}", gpu.path());
    assert_eq!(gpu.range(), Some((100, 2500)));
    // The distinction four separate tools get wrong.
    assert_eq!(gpu.requested(), Some(2500));
    assert_eq!(gpu.achieved(), Some(1850));
}

#[test]
fn gpu_cap_lowers_the_floor_with_the_ceiling() {
    let f = Fixture::framework_13("gpucap");
    with_xe_gpu(&f);
    let fs = f.sysfs();
    let gpu = GpuFreq::new(&fs);

    // min_freq starts at 900; a 1200 cap leaves it alone.
    gpu.set_max(1200).unwrap();
    assert_eq!(gpu.max(), Some(1200));
    assert_eq!(gpu.min(), Some(900));

    // A cap below the floor must drag the floor down, or the kernel rejects it.
    gpu.set_max(500).unwrap();
    assert_eq!(gpu.max(), Some(500));
    assert_eq!(gpu.min(), Some(500));

    gpu.reset().unwrap();
    assert_eq!(gpu.max(), Some(2500));
    assert_eq!(gpu.min(), Some(900), "reset returns min to rpe, not rpn");
}

#[test]
fn gpu_cap_refuses_a_frequency_the_hardware_has_no_word_for() {
    let f = Fixture::framework_13("gpurange");
    with_xe_gpu(&f);
    let fs = f.sysfs();
    let err = GpuFreq::new(&fs).set_max(4000).unwrap_err();
    assert!(matches!(err, TuneError::OutOfRange { .. }), "{err}");
    assert!(err.to_string().contains("2500"), "{err}");
}

#[test]
fn no_gpu_is_a_reason_not_a_panic() {
    let f = Fixture::framework_13("nogpu");
    let fs = f.sysfs();
    let gpu = GpuFreq::new(&fs);
    assert!(!gpu.is_supported());
    assert!(gpu.range().is_none());
    assert!(matches!(
        gpu.set_max(1200).unwrap_err(),
        TuneError::Unsupported(_)
    ));
}
