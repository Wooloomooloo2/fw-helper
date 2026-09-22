//! `fw-helperctl` — command-line view of what this machine exposes.
//!
//! Prefers `fw-helperd` over D-Bus, which needs no privileges. Falls back to reading
//! sysfs directly when the daemon is absent — useful for debugging and before the
//! daemon is installed, but then package power needs root, because `energy_uj` is
//! 0400 (the PLATYPUS mitigation, ADR 0009).

use fw_helper_client::{connect, DaemonProxyBlocking, SessionInfo, Snapshot, SUPPORTED_VERSION};
use fw_helper_core::tune::{CoreParking, GpuFreq, ParkLevel};
use fw_helper_core::{Monitor, Sysfs};
use std::thread::sleep;
use std::time::Duration;

const USAGE: &str = "\
fw-helperctl — Framework laptop firmware control

USAGE:
    fw-helperctl status          capabilities and one telemetry sample
    fw-helperctl watch [secs]    live telemetry, 1 Hz (default 10s)
    fw-helperctl charge-limit N  set the battery charge limit (20-100)
    fw-helperctl fan N           pin the fan at duty N (0, or 30-255)
    fw-helperctl fan auto        hand the fan back to the EC
    fw-helperctl fan curve        follow the built-in quiet curve
    fw-helperctl fan curve T:D,.. follow a custom curve, e.g. 55:0,70:65,85:120
    fw-helperctl power-limit N   set the sustained CPU power limit, in watts
    fw-helperctl profile         show profiles
    fw-helperctl profile NAME    apply a profile by name
    fw-helperctl profile save NAME    save current settings as a profile
    fw-helperctl profile delete NAME  remove a saved profile
    fw-helperctl auto-profile          show power-source switching
    fw-helperctl auto-profile AC BATT  switch profile when the cable changes
    fw-helperctl auto-profile off      stop switching automatically
    fw-helperctl record            what is being recorded, and what has been
    fw-helperctl record start [NAME]  start recording a session
    fw-helperctl record stop       stop recording
    fw-helperctl record rm NAME    delete a recorded session
    fw-helperctl hud               one status line, for an in-game overlay
    fw-helperctl tune              CPU clusters and GPU frequency, as discovered
    fw-helperctl tune park LEVEL   park cores: none | lpe | p-only
    fw-helperctl tune gpu MHZ      cap the GPU clock; 0 for the full range

Talks to fw-helperd when it is running; otherwise reads sysfs directly, in which
case package power needs root.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("status") | None => status(),
        Some("watch") => watch(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10)),
        Some("charge-limit") => charge_limit(args.get(1).map(String::as_str)),
        Some("fan") => fan(args.get(1).map(String::as_str)),
        Some("power-limit") => power_limit(args.get(1).map(String::as_str)),
        Some("profile") => profile(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        Some("auto-profile") => auto_profile(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        Some("record") => record(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        Some("hud") => hud(),
        Some("tune") => tune(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        Some("-h") | Some("--help") => print!("{USAGE}"),
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn status() {
    match connect() {
        Ok((d, version)) => status_via_dbus(&d, version),
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e});\nreading sysfs directly\n");
            status_direct();
        }
    }
}

fn status_via_dbus(d: &DaemonProxyBlocking<'_>, version: u32) {
    if version != SUPPORTED_VERSION {
        eprintln!(
            "warning: daemon speaks interface v{version}, this client understands \
             v{SUPPORTED_VERSION}; output may be incomplete\n"
        );
    }
    let s = match Snapshot::fetch(d) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read telemetry: {e}");
            std::process::exit(1);
        }
    };

    println!("Capabilities                                    (via fw-helperd)");
    for (name, ok, why) in &s.capabilities {
        let mark = if *ok { "+" } else { "-" };
        let detail = if *ok {
            "available".to_string()
        } else {
            format!("unavailable — {why}")
        };
        println!("  {mark} {name:<18} {detail}");
    }

    println!("\nTelemetry");
    match s.package_watts {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable>"),
    }
    if let Some(ac) = s.on_ac {
        let (on_ac, on_batt) = &s.auto_profiles;
        let auto = if on_ac.is_empty() && on_batt.is_empty() {
            String::new()
        } else {
            format!("  (auto: AC={on_ac} battery={on_batt})")
        };
        println!(
            "  power source       {}{auto}",
            if ac { "AC" } else { "battery" }
        );
    }
    if let Some(p) = &s.profile {
        let via = match s.profile_backend.as_deref() {
            Some("platform_profile") => "  (platform_profile; GNOME slider not in sync)",
            _ => "",
        };
        println!("  profile            {p}{via}");
    }
    if let (Some(pl), Some(max)) = (s.power_limit, s.power_limit_max) {
        let note = if pl >= max { " (stock)" } else { "" };
        println!("  power limit        {pl} W of {max} W max{note}");
    }
    if let Some(r) = s.fan_rpm {
        // Say who is driving. Under manual control the EC's curve is not running, and
        // an RPM with no owner shown is indistinguishable from a stuck fan (ADR 0006).
        match s.fan_mode.as_deref() {
            Some("curve") => {
                let duty = s.fan_duty.unwrap_or(0);
                println!("  fan                {r} rpm  (CURVE, currently duty {duty}/255)");
                let spec: Vec<String> = s
                    .fan_curve
                    .iter()
                    .map(|(c, d)| format!("{c:.0}C:{d}"))
                    .collect();
                println!("                     {}", spec.join("  "));
            }
            Some("manual") => {
                let duty = s.fan_duty.unwrap_or(0);
                let percent = (f64::from(duty) * 100.0 / 255.0).round();
                println!("  fan                {r} rpm  (MANUAL, duty {duty}/255 = {percent:.0}%)");
                println!("                     EC curve is not running; 'fw-helperctl fan auto' restores it");
            }
            _ => println!("  fan                {r} rpm  (EC automatic)"),
        }
        // The floor moves with temperature, so show it: it is the reason a request
        // may be raised, and a number the user can act on.
        match s.fan_floor {
            Some(0) => println!("  fan floor          0/255 (firmware would have the fan off)"),
            Some(255) => println!("  fan floor          255/255 (no temperature readable)"),
            Some(f) => {
                println!("  fan floor          {f}/255 (firmware would be at least this fast)")
            }
            None => {}
        }
    }
    if let Some(p) = &s.platform_profile {
        println!("  platform profile   {p}");
    }
    if let Some(pct) = s.battery_percent {
        let st = s.battery_status.clone().unwrap_or_default();
        println!("  battery            {pct}% ({st})");
    }
    if let Some(limit) = s.charge_limit {
        // The value is real; whether it governs charging is a separate question, and
        // on this board the answer is no. Say so on the same line rather than leaving
        // it to be inferred from the capability block above.
        match s.capability("charge limit") {
            Some((false, _)) => println!("  charge limit       {limit}% (the EC ignores it)"),
            _ => println!("  charge limit       {limit}%"),
        }
    }

    if !s.temps.is_empty() {
        println!("\nTemperatures");
        for t in &s.temps {
            let mark = if s.control_sensor.as_deref() == Some(t.label.as_str()) {
                "*"
            } else {
                " "
            };
            let crit = t
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!("  {mark} {:<22} {:>6.1} C   ({crit})", t.label, t.celsius);
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn status_direct() {
    let mut mon = Monitor::new(Sysfs::default());
    println!("Capabilities                                    (direct sysfs)");
    for (name, cap) in mon.capabilities().summary() {
        let mark = if cap.is_available() { "+" } else { "-" };
        println!("  {mark} {name:<18} {cap}");
    }
    // The first sample only establishes the energy reference point.
    mon.sample();
    sleep(Duration::from_secs(1));
    let s = mon.sample();

    println!("\nTelemetry");
    match s.package_watts {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable — run as root?>"),
    }
    if let Some(r) = s.fan_rpm {
        println!("  fan                {r} rpm");
    }
    if let Some(p) = &s.platform_profile {
        println!("  platform profile   {p}");
    }
    if let (Some(pct), Some(st)) = (s.battery_percent, &s.battery_status) {
        println!("  battery            {pct}% ({st})");
    }
    if !s.temps.is_empty() {
        println!("\nTemperatures");
        let control = s.control_temp().map(|c| c.label.clone());
        for t in &s.temps {
            let mark = if control.as_deref() == Some(t.label.as_str()) {
                "*"
            } else {
                " "
            };
            let crit = t
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!("  {mark} {:<22} {:>6.1} C   ({crit})", t.label, t.celsius);
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

/// Setting a limit requires the daemon: this is a hardware write, and it goes
/// through polkit inside the daemon rather than being attempted directly.
fn charge_limit(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl charge-limit <20-100>");
        std::process::exit(2);
    };
    let Ok(percent) = arg.parse::<u8>() else {
        eprintln!("not a number: {arg}");
        std::process::exit(2);
    };

    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); setting a charge limit requires it");
            std::process::exit(1);
        }
    };
    match d.set_charge_limit(percent) {
        Ok(()) => println!("charge limit set to {percent}%"),
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Manual fan control. Requires the daemon for the same reason the charge limit does.
///
/// There is no curve behind this: it pins one duty with no temperature feedback at
/// all. `auto` is the way back, and is what should be used to make the fan quieter at
/// idle - the EC can run it slower than this command is allowed to.
fn fan(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl fan <77-255|auto>");
        std::process::exit(2);
    };

    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); fan control requires it");
            std::process::exit(1);
        }
    };

    if arg == "curve" {
        set_curve(&d, None);
    }
    if let Some(spec) = arg.strip_prefix("curve ") {
        set_curve(&d, Some(spec));
    }
    if arg == "auto" {
        match d.set_fan_auto() {
            Ok(()) => println!("fan returned to EC control"),
            Err(e) => {
                eprintln!("failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Ok(duty) = arg.parse::<u8>() else {
        eprintln!("not a duty or 'auto': {arg}");
        std::process::exit(2);
    };
    match d.set_fan_duty(duty) {
        // Report what the EC took, not what was asked for: it quantizes to whole
        // percent, so these differ by a count or two and the difference is real.
        Ok(settled) => {
            let percent = (f64::from(settled) * 100.0 / 255.0).round();
            println!("fan pinned at duty {settled}/255 ({percent:.0}%)");
            // Two quite different reasons the number can come back changed, and the
            // user needs to be able to tell them apart.
            if settled > duty.saturating_add(3) {
                println!("  raised from {duty}: the firmware floor at the current");
                println!("  temperature is higher, and we never run slower than the EC would");
            } else if settled != duty {
                println!("  (asked for {duty}; the EC stores whole percent)");
            }
            println!("  'fw-helperctl fan auto' returns control to the EC");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Send a curve to the daemon. `None` means the built-in one.
///
/// The daemon validates: points must ascend in temperature, must not fall in duty, and
/// must not ask for a duty that cannot turn the fan. Its message says which rule was
/// broken, so there is nothing to duplicate here.
fn set_curve(d: &DaemonProxyBlocking<'_>, spec: Option<&str>) -> ! {
    let points: Vec<(f64, u8)> = match spec {
        None => Vec::new(),
        Some(s) => match parse_curve(s) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                eprintln!("expected temperature:duty pairs, e.g. 55:0,70:65,85:120");
                std::process::exit(2);
            }
        },
    };
    // An empty list means "use the built-in", resolved here rather than in the daemon
    // so the wire stays a plain list of points.
    let points = if points.is_empty() {
        vec![
            (55.0, 0u8),
            (62.0, 40),
            (70.0, 65),
            (80.0, 92),
            (90.0, 130),
            (100.0, 255),
        ]
    } else {
        points
    };

    match d.set_fan_curve(points.clone()) {
        Ok(settled) => {
            println!("following a fan curve; currently duty {settled}/255");
            for (c, duty) in &points {
                println!("  {c:>5.1} C -> {duty:>3}/255");
            }
            println!("  the firmware floor still applies on top of this");
            println!("  'fw-helperctl fan auto' returns control to the EC");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_curve(spec: &str) -> Result<Vec<(f64, u8)>, String> {
    spec.split(',')
        .map(|pair| {
            let (t, d) = pair
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("'{pair}' is not temperature:duty"))?;
            let celsius: f64 = t
                .trim()
                .parse()
                .map_err(|_| format!("'{t}' is not a temperature"))?;
            let duty: u8 = d
                .trim()
                .parse()
                .map_err(|_| format!("'{d}' is not a duty"))?;
            Ok((celsius, duty))
        })
        .collect()
}

/// Set the sustained CPU power limit. Requires the daemon, like every hardware write.
fn power_limit(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl power-limit <watts>");
        std::process::exit(2);
    };
    let Ok(watts) = arg.parse::<u32>() else {
        eprintln!("not a number: {arg}");
        std::process::exit(2);
    };
    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); setting a power limit requires it");
            std::process::exit(1);
        }
    };
    match d.set_power_limit(watts) {
        Ok(()) => {
            println!("power limit set to {watts} W");
            // Measured, and the reason a user who checks immediately will think it did
            // not work: the limit averages over ~32 s.
            println!("  the limit averages over ~32 s, so power readings take that long to settle");
            println!("  roughly 10 W is worth 12 C of sustained CPU temperature");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Show or switch profile.
///
/// A profile is a PPD profile plus the knobs PPD does not manage (ADR 0005), so this
/// also moves the GNOME power slider — that is the point, not a side effect.
fn profile(arg: Option<&str>, extra: Option<&str>) {
    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); profiles require it");
            std::process::exit(1);
        }
    };
    if matches!(arg, Some("save") | Some("delete")) {
        let verb = arg.unwrap_or_default();
        let Some(name) = extra else {
            eprintln!("usage: fw-helperctl profile {verb} <name>");
            std::process::exit(2);
        };
        let result = if verb == "save" {
            d.save_profile(name).map(|path| {
                println!("saved as {name}");
                println!("  written to {path}");
                println!("  edit it by hand, or 'fw-helperctl profile delete {name}' to remove");
            })
        } else {
            d.delete_profile(name).map(|()| println!("deleted {name}"))
        };
        if let Err(e) = result {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    let Some(name) = arg else {
        let active = d.active_profile().unwrap_or_default();
        for p in d.profiles().unwrap_or_default() {
            let mark = if p == active { "*" } else { " " };
            println!("{mark} {p}");
        }
        match d.profile_backend().unwrap_or_default().as_str() {
            "ppd" => println!("\n  driven via power-profiles-daemon; the GNOME slider stays in sync"),
            "platform_profile" => println!(
                "\n  power-profiles-daemon is absent, so platform_profile is written directly.\n                   The GNOME power slider will NOT reflect changes made here"
            ),
            _ => println!("\n  no profile axis available on this machine"),
        }
        std::process::exit(0);
    };
    match d.set_profile(name) {
        Ok(()) => {
            println!("profile {name} applied");
            println!("  power limit and fan curve set; the GNOME power slider follows");
            println!("  the power limit takes ~32 s to show in readings");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Show or set power-source profile switching.
///
/// Off unless asked for: a machine that changes behaviour when a cable is plugged in,
/// without having been told to, is a machine behaving strangely.
fn auto_profile(ac: Option<&str>, batt: Option<&str>) {
    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e})");
            std::process::exit(1);
        }
    };
    let Some(ac) = ac else {
        match d.auto_profiles().unwrap_or_default() {
            (a, b) if a.is_empty() && b.is_empty() => {
                println!("automatic switching is off");
                println!("  set it with: fw-helperctl auto-profile <on-ac> <on-battery>");
            }
            (a, b) => {
                println!(
                    "on AC:      {}",
                    if a.is_empty() { "(unchanged)" } else { &a }
                );
                println!(
                    "on battery: {}",
                    if b.is_empty() { "(unchanged)" } else { &b }
                );
            }
        }
        std::process::exit(0);
    };

    let (ac, batt) = if ac == "off" {
        ("", "")
    } else {
        match batt {
            Some(b) => (ac, b),
            None => {
                eprintln!("usage: fw-helperctl auto-profile <on-ac> <on-battery>");
                eprintln!("   or: fw-helperctl auto-profile off");
                std::process::exit(2);
            }
        }
    };

    match d.set_auto_profiles(ac, batt) {
        Ok(()) if ac.is_empty() => println!("automatic switching is off"),
        Ok(()) => {
            println!("on AC: {ac}, on battery: {batt}");
            println!("  applied when the cable changes, not now");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

fn watch(secs: u64) {
    let (d, _version) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); watch requires the daemon");
            eprintln!("hint: `fw-helperctl status` still works by reading sysfs directly");
            std::process::exit(1);
        }
    };
    println!(
        "{:>6}  {:>9}  {:>8}  {:>9}  {:>6}  {:>6}  {:>6}  throttle",
        "t", "power", "fan", "cpu", "cpu%", "gpu%", "mem%"
    );
    for i in 1..=secs {
        sleep(Duration::from_secs(1));
        let Ok(s) = Snapshot::fetch(&d) else { continue };
        let power = s
            .package_watts
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "-".into());
        let fan = s
            .fan_rpm
            .map(|r| format!("{r} rpm"))
            .unwrap_or_else(|| "-".into());
        let cpu = s
            .control_sensor
            .as_ref()
            .and_then(|label| s.temps.iter().find(|t| &t.label == label))
            .map(|t| format!("{:.1} C", t.celsius))
            .unwrap_or_else(|| "-".into());
        // A dash, not a zero. The load sampler yields nothing until it has two samples
        // to compare, and on the first tick "0%" would be a lie rather than a reading.
        let pct = |v: Option<f64>| v.map(|x| format!("{x:.0}%")).unwrap_or_else(|| "-".into());
        let throttle = throttle_note(&s);
        println!(
            "{i:>5}s  {power:>9}  {fan:>8}  {cpu:>9}  {:>6}  {:>6}  {:>6}  {throttle}",
            pct(s.load.cpu_percent),
            pct(s.load.gpu_percent),
            pct(s.load.mem_percent()),
        );
    }
}

/// What was being held back this second, if anything.
fn throttle_note(s: &Snapshot) -> String {
    let mut parts = Vec::new();
    if s.load.throttle_events > 0 {
        parts.push(format!(
            "cpu thermal x{} ({} ms)",
            s.load.throttle_events, s.load.throttle_ms
        ));
    }
    if let Some(reasons) = &s.load.gpu_throttle {
        parts.push(format!("gpu {reasons}"));
    }
    // Power limiting on the package has no flag to read: draw sitting on the setpoint
    // is the only signal, so say so rather than leaving the column blank.
    if let (Some(w), Some(limit)) = (s.package_watts, s.power_limit) {
        if w >= f64::from(limit) * 0.97 {
            parts.push(format!("at PL1 {limit} W"));
        }
    }
    parts.join(", ")
}

fn record(action: Option<&str>, name: Option<&str>) {
    let (d, _version) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); recording happens in the daemon");
            eprintln!("hint: it must keep running after this command exits, and package");
            eprintln!("      power needs root, so there is no direct-sysfs fallback here");
            std::process::exit(1);
        }
    };

    match action {
        None | Some("list") => record_list(&d),
        Some("start") => {
            // A session with no name is still a session; date it and move on rather
            // than refusing over a label.
            let label = name.unwrap_or("session");
            match d.start_recording(label) {
                Ok(path) => println!("recording to {path}\nstop it with: fw-helperctl record stop"),
                Err(e) => {
                    eprintln!("could not start recording: {}", describe(e));
                    std::process::exit(1);
                }
            }
        }
        Some("stop") => match d.stop_recording() {
            Ok(path) => println!("recorded {path}"),
            Err(e) => {
                eprintln!("could not stop recording: {}", describe(e));
                std::process::exit(1);
            }
        },
        Some("rm") | Some("delete") => {
            let Some(name) = name else {
                eprintln!("which session? `fw-helperctl record` lists them");
                std::process::exit(2);
            };
            match d.delete_session(name) {
                Ok(()) => println!("deleted {name}"),
                Err(e) => {
                    eprintln!("could not delete {name}: {}", describe(e));
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("unknown record action: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn record_list(d: &DaemonProxyBlocking<'_>) {
    match d.recording_session() {
        Ok((label, _, path, _, samples)) if !label.is_empty() => {
            println!("recording {label:?}: {samples} samples so far");
            println!("  {path}\n");
        }
        _ => println!("not recording\n"),
    }

    let sessions = SessionInfo::fetch(d);
    if sessions.is_empty() {
        println!("no recorded sessions yet");
        println!("  start one with: fw-helperctl record start <name>");
        return;
    }
    println!("{:<34}  {:>8}  label", "session", "size");
    for s in &sessions {
        println!("{:<34}  {:>8}  {}", s.name, human_bytes(s.bytes), s.label);
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// The one-line status an in-game overlay shows.
///
/// Reads the file the daemon writes rather than asking over D-Bus: this is meant to be
/// wired into MangoHud's `exec=`, which runs inside the game's frame loop, and opening
/// a bus connection per frame would be a frame-time bug. The D-Bus path is only the
/// fallback for when the file is missing.
fn hud() {
    const HUD_FILE: &str = "/run/fw-helper/hud";
    if let Ok(line) = std::fs::read_to_string(HUD_FILE) {
        print!("{line}");
        return;
    }
    match connect().and_then(|(d, _)| Snapshot::fetch(&d)) {
        Ok(s) => {
            let mut parts = Vec::new();
            if let Some(w) = s.power_limit {
                parts.push(format!("PL1 {w} W"));
            }
            if let Some(rpm) = s.fan_rpm {
                parts.push(format!("fan {rpm} rpm"));
            }
            if let Some(p) = &s.profile {
                parts.push(p.clone());
            }
            println!("{}", parts.join(" | "));
        }
        Err(_) => {
            eprintln!("no {HUD_FILE} and no daemon");
            std::process::exit(1);
        }
    }
}

/// D-Bus errors arrive wrapped; the daemon's own message is the useful part, and it is
/// written to be read by a person. Generic over `Display` so this crate need not depend
/// on zbus directly.
fn describe(e: impl std::fmt::Display) -> String {
    let text = e.to_string();
    match text.split_once(": ") {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ => text,
    }
}

/// Read-only view of the M9 levers, straight from sysfs.
///
/// Deliberately not routed through the daemon: its job is to show what
/// [`CoreSet::probe`] actually found on *this* machine, which is the one thing the
/// fixtures cannot prove. Needs no privileges.
fn tune(sub: Option<&str>, arg: Option<&str>) {
    match (sub, arg) {
        (None, _) => tune_show(),
        (Some("park"), Some(level)) => tune_write("park level", |d| {
            d.set_park_level(level).map_err(Into::into)
        }),
        (Some("gpu"), Some(mhz)) => match mhz.parse::<u32>() {
            Ok(v) => tune_write("gpu cap", |d| d.set_gpu_max_freq(v).map_err(Into::into)),
            Err(_) => {
                eprintln!("expected a frequency in MHz, or 0 for the full range");
                std::process::exit(2);
            }
        },
        (Some("park"), None) => {
            eprintln!("usage: fw-helperctl tune park none|lpe|p-only");
            std::process::exit(2);
        }
        (Some("gpu"), None) => {
            eprintln!("usage: fw-helperctl tune gpu MHZ   (0 for the full range)");
            std::process::exit(2);
        }
        (Some(other), _) => {
            eprintln!("unknown tune subcommand: {other}");
            std::process::exit(2);
        }
    }
}

/// Both writes need the daemon: parking a core and moving a GT window are root-only,
/// and going behind the daemon would leave it holding a level it no longer has.
fn tune_write<F>(what: &str, apply: F)
where
    F: FnOnce(&DaemonProxyBlocking<'_>) -> Result<(), Box<dyn std::error::Error>>,
{
    let Ok((d, _)) = connect() else {
        eprintln!("fw-helperd is not running; {what} needs it (these writes are root-only)");
        std::process::exit(1);
    };
    if let Err(e) = apply(&d) {
        eprintln!("{what}: {e}");
        std::process::exit(1);
    }
    tune_show();
}

fn tune_show() {
    let fs = Sysfs::new("/");

    // Prefer the daemon's topology. It probes once, while the machine is whole, and
    // caches it — whereas probing here reads a machine that may already be parked, and
    // an offline core has no `cpufreq/` or `topology/` left to classify it by. Measured
    // 2026-09-22: parking 12-15 made a fresh probe report one Atom tier of twelve, so
    // `lpe` displayed as "parks 0" while it was the level actually in effect.
    let daemon_clusters = connect()
        .ok()
        .and_then(|(d, _)| d.cpu_clusters().ok())
        .filter(|c| !c.is_empty());

    let park = CoreParking::new(&fs);
    let set = park.core_set();
    println!("CPU topology                                     (discovered, not assumed)");
    if set.cpus().is_empty() {
        println!("  none found under /sys/devices/system/cpu");
    }
    // (name, cpus, max_mhz) from whichever source we trust more.
    let rows: Vec<(String, Vec<u32>, u32)> = match &daemon_clusters {
        Some(cl) => cl
            .iter()
            .map(|(name, cpus, mhz, _)| (name.clone(), cpus.clone(), *mhz))
            .collect(),
        None => set
            .clusters()
            .into_iter()
            .map(|c| {
                let cpus = set.in_cluster(c);
                (
                    c.as_str().to_string(),
                    cpus.iter().map(|x| x.num).collect(),
                    // The cluster max, not the first core's: cpu1 boosts to 4800 where
                    // 0/2/3 reach 4700.
                    cpus.iter().map(|x| x.max_khz / 1000).max().unwrap_or(0),
                )
            })
            .collect(),
    };
    let unparkable: Vec<u32> = set
        .cpus()
        .iter()
        .filter(|c| !c.parkable)
        .map(|c| c.num)
        .collect();
    for (name, cpus, mhz) in &rows {
        let nums: Vec<String> = cpus
            .iter()
            .map(|n| {
                let mut s = n.to_string();
                if unparkable.contains(n) {
                    s.push('*');
                }
                if !park.is_online(*n) {
                    s.push_str(" (parked)");
                }
                s
            })
            .collect();
        println!(
            "  {:<5} x{:<2} {:>5} MHz   {}",
            name,
            cpus.len(),
            mhz,
            nums.join(" ")
        );
    }
    if daemon_clusters.is_none() && !set.is_complete() {
        println!(
            "  note: cores are parked and fw-helperd is not answering, so the clusters\n\
             \x20       above are a guess — an offline CPU exposes no topology to read"
        );
    }
    if set.cpus().iter().any(|c| !c.parkable) {
        println!("  * cannot be parked — the kernel exposes no `online` file for it");
    }

    println!("\nPark levels");
    for level in [ParkLevel::None, ParkLevel::Lpe, ParkLevel::PCoresOnly] {
        let parked = set.to_park(level);
        let left = set.cpus().len() - parked.len();
        println!(
            "  {:<8} parks {:<2} leaves {:>2} online",
            level.as_str(),
            parked.len(),
            left
        );
    }
    match park.read() {
        Some(l) => println!("  current  {l}"),
        None => println!("  current  mixed — some cores were offlined by hand"),
    }

    println!("\nGPU frequency");
    let gpu = GpuFreq::new(&fs);
    match gpu.path() {
        None => println!("  no xe GT found under /sys/class/drm"),
        Some(p) => {
            println!("  node     /{p}");
            if let Some((rpn, rp0)) = gpu.range() {
                println!("  range    {rpn}-{rp0} MHz  (rpn_freq..rp0_freq)");
            }
            println!(
                "  window   min {} max {} MHz",
                fmt_mhz(gpu.min()),
                fmt_mhz(gpu.max())
            );
            // The distinction that settled the whole "2.5 GHz" question.
            println!(
                "  clock    requested {} / achieved {}",
                fmt_mhz(gpu.requested()),
                fmt_mhz(gpu.achieved())
            );
            println!("           requested is cur_freq, the DVFS ask — it reads a constant");
            println!("           2500 here. achieved is act_freq, and reads 0 in RC6.");
        }
    }
}

fn fmt_mhz(v: Option<u32>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
}
