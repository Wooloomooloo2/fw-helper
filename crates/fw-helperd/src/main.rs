//! `fw-helperd` — privileged daemon for fw-helper.
//!
//! Owns all hardware access; the GUI and CLI hold none and reach it over D-Bus
//! (ADR 0003). Writes arrived with M2 (charge limit) and M3 (fan).
//!
//! Manual fan control is the one thing here that can damage the machine, so the
//! shutdown paths are not boilerplate: every route out of this process must return
//! `pwm1_enable=2` (ADR 0006). Clean exit, `SIGTERM`, `SIGINT` and panic are covered
//! below; `SIGKILL` and a hung process are not, and are covered by `ExecStopPost` and
//! the watchdog respectively. The watchdog's heartbeat is the telemetry poll loop, so
//! the thing that proves this daemon is alive is the same thing that proves it is
//! doing its job.

mod ec;
mod fan;
mod iface;
mod logind;
mod polkit;
mod ppd;
mod profiles;
mod record;
mod state;
mod watchdog;
mod wire;

use fw_helper_core::{Capabilities, Monitor, Ppd, Profile, Sysfs, UsageSampler};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const BUS_NAME: &str = "org.fwhelper.Daemon1";
const OBJECT_PATH: &str = "/org/fwhelper/Daemon1";

/// How many ticks the GPU client scan stays on after the last client read telemetry.
const WATCH_GRACE_TICKS: u8 = 5;

/// One second. This is the publication rate cap from ADR 0009, not a performance
/// tuning knob — do not raise it without superseding that ADR.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Capture the runtime while we are on it: interface methods run on zbus's executor,
    // which has no tokio timer, and the polkit prompt needs one.
    polkit::init_runtime(tokio::runtime::Handle::current());

    let fs = Sysfs::default();
    let mut caps = Capabilities::probe(&fs);
    // Core probes sysfs only (ADR 0010), and sysfs is the interface that lies about
    // this particular knob. Ask the mechanism that actually governs charging instead.
    let cros_ec: Arc<dyn ec::EcTransport> = Arc::new(ec::CrosEc::default());
    caps.charge_limit = if ec::CrosEc::default().exists() {
        ec::charge_capability(&*cros_ec)
    } else {
        fw_helper_core::Cap::No(format!("no {}; is cros_ec_chardev loaded?", ec::DEVICE))
    };

    eprintln!("fw-helperd starting ({})", build_stamp());
    for (name, cap) in caps.summary() {
        eprintln!("  {name:<18} {cap}");
    }
    if !caps.package_power.is_available() {
        eprintln!("note: package power needs root; running unprivileged?");
    }

    // Before anything else: if a previous instance died holding manual fan control,
    // take it back. This runs even when fan control is unsupported, where it is a
    // no-op, because the cost of asking is one read.
    let lease = Arc::new(fan::FanLease::new(fs.clone()));
    lease.reclaim_at_startup();
    install_panic_hook(Arc::clone(&lease));

    let state = state::State::load();
    let state_power_limit = state.power_limit;
    let persisted_profile = state.profile.clone();
    if let Some(limit) = state.charge_limit {
        eprintln!("persisted charge limit: {limit}%");
    }
    if !state.floor.is_empty() {
        let n = lease.restore_floor(state.floor.clone());
        eprintln!("restored {n} firmware fan floor observations");
    }
    // Started before the bus name is claimed: from the moment a client can ask for
    // manual fan control, the thing that takes it back must already be running.
    let watchdog = watchdog::Watchdog::new(Arc::clone(&lease));
    watchdog.spawn();

    // Beat once before serving: the interface refuses fan control on a stale
    // heartbeat, and at startup the poll loop has not ticked yet.
    watchdog.beat();
    // ADR 0005: delegate the profile axis to PPD. On its own system-bus connection,
    // deliberately: PPD is always on the system bus, including when this daemon is run
    // on the session bus for development, and it must not depend on the connection we
    // are about to claim a name on.
    let axis = Arc::new(match zbus::Connection::system().await {
        Ok(c) => ppd::ProfileAxis::connect(&c, fs.clone()).await,
        Err(e) => {
            eprintln!("no system bus for PPD ({e}); profile axis falls back to sysfs");
            ppd::ProfileAxis::disconnected(fs.clone())
        }
    });
    let pending_ppd = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let applied_ppd = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let known_profiles = Arc::new(std::sync::Mutex::new(profiles::load()));
    let recording = Arc::new(record::Recording::new());
    let watchers = Arc::new(record::Watchers::default());

    let daemon = iface::Daemon::new(
        fs.clone(),
        caps,
        state,
        iface::Shared {
            fan: Arc::clone(&lease),
            watchdog: Arc::clone(&watchdog),
            axis: Arc::clone(&axis),
            applied_ppd: Arc::clone(&applied_ppd),
            profiles: Arc::clone(&known_profiles),
            ec: Arc::clone(&cros_ec),
            recording: Arc::clone(&recording),
            watchers: Arc::clone(&watchers),
        },
    );
    daemon.reapply_charge_limit();
    if let Some(watts) = state_power_limit {
        eprintln!("persisted power limit: {watts} W");
    }
    daemon.reapply_power_limit();

    // The session bus is a development affordance: claiming a name on the system bus
    // needs both root and an installed policy file, which makes iterating painful.
    // Production always uses the system bus -- the systemd unit does not set this.
    let session = std::env::var_os("FW_HELPERD_SESSION_BUS").is_some();
    let builder = if session {
        eprintln!("FW_HELPERD_SESSION_BUS set -- using session bus (development only)");
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };

    let conn = builder
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    eprintln!("listening on {BUS_NAME} at {OBJECT_PATH}");

    // Follow the desktop's slider. The watcher signals the poll loop through an atomic
    // rather than sharing the daemon, the same shape the resume hook already uses.
    {
        let pending = Arc::clone(&pending_ppd);
        axis.watch(move |p| {
            pending.store(ppd_code(p), Ordering::SeqCst);
        })
        .await;
    }

    // PPD is D-Bus-activatable, so a cold boot can have us probing before it exists and
    // a single probe would make that timing accident permanent. Adopt it whenever it
    // appears instead — installed even when the startup probe succeeded, because PPD can
    // also restart underneath us.
    axis.watch_for_ppd().await;

    let resumed = Arc::new(AtomicBool::new(false));
    logind::watch_sleep(&conn, Arc::clone(&resumed), Arc::clone(&lease)).await;

    let poll = tokio::spawn(poll_loop(Poll {
        conn: conn.clone(),
        mon: Monitor::new(fs.clone()),
        resumed,
        watchdog: Arc::clone(&watchdog),
        lease: Arc::clone(&lease),
        axis: Arc::clone(&axis),
        pending_ppd: Arc::clone(&pending_ppd),
        applied_ppd: Arc::clone(&applied_ppd),
        fs: fs.clone(),
        cros_ec: Arc::clone(&cros_ec),
        persisted_profile,
        persisted_power_limit: state_power_limit,
        known_profiles: Arc::clone(&known_profiles),
        usage: UsageSampler::new(fs.clone()),
        recording: Arc::clone(&recording),
        watchers: Arc::clone(&watchers),
    }));

    // Shut down cleanly on either signal, and hand the fan back before doing anything
    // else — the poll task is irrelevant if the fan is stuck (ADR 0006).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("SIGINT, shutting down"),
        _ = sigterm.recv()          => eprintln!("SIGTERM, shutting down"),
    }
    // Write out anything learned since the last periodic save. SIGKILL bypasses this,
    // which is why the periodic save exists as well.
    if let Some((_, observations)) = lease.floor_snapshot() {
        if !observations.is_empty() {
            if let Ok(guard) = conn
                .object_server()
                .interface::<_, iface::Daemon>(OBJECT_PATH)
                .await
            {
                guard.get().await.save_floor(observations);
            }
        }
    }
    shut_down_fan(&lease, &watchdog);
    // Nothing should read numbers from a daemon that has stopped. systemd's
    // RuntimeDirectory= covers the packaged case; this covers a daemon run by hand.
    record::clear_hud();
    poll.abort();
    Ok(())
}

/// Take the fan back after a resume, if it was ours before the sleep.
///
/// Runs on the first tick after waking, with telemetry sampled *after* the wake, so
/// the floor and ceiling reflect the machine as it is now rather than as it was when
/// it went down.
fn restore_fan(lease: &fan::FanLease, sample: &fw_helper_core::Telemetry) {
    match lease.restore_after_resume(fan::Thermal::from_telemetry(sample)) {
        None => {}
        Some(Ok(applied)) if applied.clamped() => eprintln!(
            "fan: restored after resume at duty {}/255, raised from the requested {} \
             by the firmware floor",
            applied.settled, applied.requested
        ),
        Some(Ok(applied)) => eprintln!(
            "fan: restored manual control after resume at duty {}/255",
            applied.settled
        ),
        // Refusing to restore is a legitimate outcome, not a failure: waking too hot,
        // or with no readable sensor, are both reasons the fan should stay firmware's.
        Some(Err(e)) => eprintln!("fan: not restoring manual control after resume: {e}"),
    }
}

/// How often the learned fan floor is written out.
///
/// Not every tick: most ticks learn nothing, and the file also holds the charge limit.
/// Not only at shutdown either, because `SIGKILL` is a supported way for this daemon to
/// end and everything learned since the last write would go with it.
const FLOOR_SAVE_INTERVAL: Duration = Duration::from_secs(60);

/// How many times we will re-assert a power limit before concluding firmware owns it.
///
/// Correcting forever would be an invisible fight burning a sysfs write every second.
/// Five rides out the asynchronous re-derive seen after a `platform_profile` switch, and
/// is few enough that a real conflict surfaces as a log line rather than as silence.
const MAX_POWER_CORRECTIONS: u32 = 5;

/// How many times we will re-assert the power limit before concluding firmware has won.
///
/// Bounded because a daemon that fights firmware forever is worse than one that stops
/// and says so. **Reset whenever the setpoint changes**, because a new setpoint is a new
/// intention rather than a continuation of a losing fight — which is exactly what the
/// "giving up until it is set again" message has always promised.
///
/// That promise was not kept. The reset lived only on the PPD-follow path, and that path
/// is deliberately skipped for our own writes (`applied_ppd` marks them so a profile is
/// not applied twice). So `fw-helperctl profile balanced` inherited an exhausted budget:
/// measured 2026-08-29, the budget ran out at 18:11 the previous evening, `balanced` was
/// applied at 13:42 and wrote 20 W, firmware re-derived 25 W moments later, and nothing
/// ever corrected it. The write verified, the profile reported success, and the machine
/// ran at a budget no profile had asked for.
#[derive(Debug)]
struct PowerCorrections {
    used: u32,
    desired: Option<u32>,
}

impl PowerCorrections {
    fn new(desired: Option<u32>) -> Self {
        Self { used: 0, desired }
    }

    /// Note the setpoint as it stands this tick, resetting the budget if it moved.
    fn observe(&mut self, desired: Option<u32>) {
        if desired != self.desired {
            self.desired = desired;
            self.used = 0;
        }
    }

    fn may_correct(&self) -> bool {
        self.used < MAX_POWER_CORRECTIONS
    }

    /// Record a correction; returns how many have now been used.
    fn record(&mut self) -> u32 {
        self.used += 1;
        self.used
    }

    fn just_exhausted(&self) -> bool {
        self.used == MAX_POWER_CORRECTIONS
    }
}

/// PPD profiles as a number, so the watcher can hand one to the poll loop through an
/// atomic. 0 means "nothing pending".
pub fn ppd_code(p: Ppd) -> u8 {
    match p {
        Ppd::PowerSaver => 1,
        Ppd::Balanced => 2,
        Ppd::Performance => 3,
    }
}

fn ppd_from_code(c: u8) -> Option<Ppd> {
    match c {
        1 => Some(Ppd::PowerSaver),
        2 => Some(Ppd::Balanced),
        3 => Some(Ppd::Performance),
        _ => None,
    }
}

/// Find a profile by name in the shared, mutable set.
fn lookup_profile(profiles: &std::sync::Mutex<Vec<Profile>>, name: &str) -> Option<Profile> {
    profiles
        .lock()
        .ok()?
        .iter()
        .find(|p| p.name == name)
        .cloned()
}

/// When the running binary was built, as far as its own file can say.
///
/// There is otherwise no way to tell a running daemon's vintage from outside, and on
/// 2026-08-22 a six-hour-old process served three rounds of testing while the fixed
/// binary sat unused on disk — because `systemctl enable --now` does not restart an
/// already-running unit. The mtime of our own executable is not a version, but it
/// answers the only question that actually gets asked: is this the thing I just built?
fn build_stamp() -> String {
    let Ok(meta) = std::fs::metadata("/proc/self/exe") else {
        return "build time unknown".into();
    };
    let Ok(modified) = meta.modified() else {
        return "build time unknown".into();
    };
    match modified.elapsed() {
        Ok(age) => format!("binary {} minutes old", age.as_secs() / 60),
        Err(_) => "binary newer than the clock".into(),
    }
}

/// Did the power source just change, as opposed to being seen for the first time?
///
/// Split out and pure because the obvious inline version is wrong in a way that
/// compiles: `last.replace(now)` sets the value before you can ask whether there was
/// one, so a "have we seen a source before" guard written after it is always true and
/// the daemon applies a profile every time it starts.
fn is_power_source_change(previous: Option<bool>, now: bool) -> bool {
    match previous {
        // First sample establishes a baseline. Starting the daemon is not a transition,
        // and treating it as one would override whatever the user last chose by hand.
        None => false,
        Some(before) => before != now,
    }
}

/// Tell the daemon which profile is now active, so the power-limit enforcement
/// re-asserts the right budget rather than the previous one.
async fn record_profile(conn: &zbus::Connection, name: &str, watts: u32) {
    if let Ok(guard) = conn
        .object_server()
        .interface::<_, iface::Daemon>(OBJECT_PATH)
        .await
    {
        guard.get().await.record_profile(name, watts);
    }
}

/// Apply a profile: PPD axis, power budget, fan curve.
///
/// `set_ppd` is false when we are *reacting* to PPD rather than driving it. Asking PPD
/// for the profile it just told us about would be harmless but would echo: PPD emits a
/// change, we set it back, PPD emits again. The guard is here rather than in the watcher
/// because this is the function that knows the difference.
///
/// Order matters. The power budget goes first because it does most of the thermal work —
/// 10 W is worth about 12 °C — so the curve is applied to a machine already heading for
/// the right temperature rather than chasing one that is not.
async fn apply_profile(
    profile: &Profile,
    set_ppd: bool,
    axis: &ppd::ProfileAxis,
    lease: &fan::FanLease,
    fs: &Sysfs,
    ec: &dyn ec::EcTransport,
    thermal: fan::Thermal,
) -> Result<(), String> {
    if set_ppd {
        axis.set(profile.ppd).await?;
    }
    fw_helper_core::PowerLimit::new(fs)
        .set(profile.pl1_watts)
        .map_err(|e| format!("power limit: {e}"))?;

    // The curve is a request; the firmware floor, the ceiling and the battery guard are
    // applied on top of it every tick, exactly as for a hand-set duty. A profile cannot
    // reach past them.
    lease
        .set_curve(profile.curve.clone(), thermal)
        .map_err(|e| format!("fan curve: {e}"))?;

    if let Some(limit) = profile.charge_limit {
        if let Err(e) = crate::ec::set_charge_limit(fs, ec, limit) {
            eprintln!(
                "profile {}: charge limit {limit}% failed: {e}",
                profile.name
            );
        }
    }
    eprintln!(
        "profile {} applied: ppd={} pl1={} W",
        profile.name,
        profile.ppd.as_str(),
        profile.pl1_watts
    );
    Ok(())
}

#[cfg(test)]
mod power_correction_tests {
    use super::*;

    #[test]
    fn a_bounded_budget_stops_fighting_firmware() {
        let mut b = PowerCorrections::new(Some(25));
        for _ in 0..MAX_POWER_CORRECTIONS {
            assert!(b.may_correct());
            b.record();
        }
        assert!(!b.may_correct(), "budget must be bounded");
        assert!(b.just_exhausted(), "the give-up message fires exactly once");
    }

    #[test]
    fn a_new_setpoint_gets_a_fresh_budget() {
        // The defect this exists to prevent, measured 2026-08-29: the budget was spent
        // on a 25 W setpoint the previous evening, and `profile balanced` then wrote
        // 20 W that firmware immediately re-derived to 25 W with nothing left to correct
        // it. A new setpoint is a new intention.
        let mut b = PowerCorrections::new(Some(25));
        for _ in 0..MAX_POWER_CORRECTIONS {
            b.record();
        }
        assert!(!b.may_correct());

        b.observe(Some(20));
        assert!(
            b.may_correct(),
            "a changed setpoint must restore the budget"
        );
    }

    #[test]
    fn the_same_setpoint_keeps_spending_the_same_budget() {
        // Otherwise the bound means nothing: observing an unchanged value every tick
        // would reset it every tick and we would fight firmware forever.
        let mut b = PowerCorrections::new(Some(25));
        for _ in 0..MAX_POWER_CORRECTIONS {
            b.observe(Some(25));
            b.record();
        }
        b.observe(Some(25));
        assert!(
            !b.may_correct(),
            "an unchanged setpoint must not refill the budget"
        );
    }

    #[test]
    fn losing_the_setpoint_entirely_is_also_a_change() {
        let mut b = PowerCorrections::new(Some(25));
        b.record();
        b.observe(None);
        assert!(b.may_correct());
        assert_eq!(b.used, 0);
    }
}

/// What `govern_fan` has to remember between ticks.
#[derive(Default)]
struct Govern {
    celsius: Option<f64>,
    duty: Option<u8>,
    /// Which way the temperature was last seen moving. Hysteretic, so neither a
    /// plateau nor a 1 C blip disturbs it: see [`fw_helper_core::Direction`].
    direction: fw_helper_core::Direction,
}

/// One tick of fan governance: learn what firmware does, and hold ourselves to it.
///
/// Two halves, and which one runs depends on who owns the fan:
///
/// - **EC owns it:** record what firmware is actually doing at this temperature. The
///   static floor table has only four points and a large gap right across the knee,
///   so this is what turns a model into a measurement.
/// - **We own it:** re-enforce the floor. This is the half that matters. Clamping only
///   when a duty is requested protects nothing — a duty chosen at idle is safe when it
///   is chosen and becomes stuck-low as soon as the machine is loaded.
fn govern_fan(lease: &fan::FanLease, sample: &fw_helper_core::Telemetry, state: &mut Govern) {
    let previous_duty = state.duty;
    let thermal = fan::Thermal::from_telemetry(sample);
    let celsius = thermal.celsius;

    // Rising or steady. Firmware's descending branch is hysteresis rather than a
    // requirement - measured, it runs duty 0 at 61.9 C climbing and duty 92 at the
    // same temperature falling - so only the ascending branch says anything about what
    // a temperature actually needs.
    let previous = state.celsius;
    // Direction is hysteretic rather than a comparison of consecutive samples. The
    // sensor is quantized to ~1 C and dithers across a boundary, so both plateaus and
    // single-sample blips have to leave it alone. An earlier version counted "steady"
    // as rising and recorded most of the descending branch; the version after that
    // fixed plateaus but still flipped on one 1 C blip, and a real cooldown produces
    // seven of those. See `fw_helper_core::Direction`.
    let rising = celsius.is_some() && state.direction.update(celsius);
    state.celsius = celsius;

    if !lease.held() {
        // pwm1 reports firmware's OWN duty while the EC owns the fan, so this reads
        // the real curve rather than inferring it from RPM through two tables.
        if let (Some(c), Some(duty)) = (celsius, lease.duty()) {
            // Credit the whole span since the last sample, not just this point: the die
            // sensor climbs ~4 C/s under load, so consecutive 1 Hz samples skip whole
            // buckets and endpoint-only recording learns almost nothing per event.
            //
            // Only when firmware's duty was the SAME at both ends, though. If it
            // changed, we do not know where in the interval it changed, and crediting
            // the new duty to the whole span would attribute a spun-up fan to every
            // cooler bucket the ramp passed through - erring loud, but erasing the
            // record that firmware runs the fan off down there, which is the finding
            // this whole mechanism exists to capture.
            let from = match (previous, previous_duty) {
                (Some(p), Some(pd)) if pd == duty => p,
                _ => c,
            };
            lease.observe(from, c, duty, rising);
            state.duty = Some(duty);
        } else {
            state.duty = None;
        }
        return;
    }

    match lease.enforce_floor(thermal) {
        None => {}
        // Re-asserting the same duty is a real write but not news. It happens because
        // the EC quantizes: a target of 88 settles at 89, so next tick's target of 89
        // is a genuine change of decision that moves nothing.
        Some(fan::Enforced::Corrected { from, to, .. }) if from == to => {}
        // Name the guard that actually demanded the duty. The earlier message reported
        // the CPU temperature whatever the cause, so the first time the battery guard
        // fired - 2026-09-01, charging, the pack parked on its ramp start - it read as
        // the fan oscillating at a steady 52.9 C for no reason at all.
        Some(fan::Enforced::Corrected {
            from,
            to,
            floor,
            celsius,
            source: fan::FloorSource::Firmware,
        }) => eprintln!(
            "fan: {celsius:.1} C puts the firmware floor at {floor}/255; moved {from} -> {to}"
        ),
        Some(fan::Enforced::Corrected {
            from,
            to,
            floor,
            celsius,
            source:
                fan::FloorSource::Battery {
                    celsius: battery_c,
                    firmware,
                },
        }) => eprintln!(
            "fan: battery at {battery_c:.1} C asks for {floor}/255, above the {firmware}/255 \
             the CPU at {celsius:.1} C needs; moved {from} -> {to}"
        ),
        Some(fan::Enforced::ReleasedBatteryHot {
            celsius,
            guard,
            released,
        }) => eprintln!(
            "fan: battery at {celsius:.1} C is near its {guard}; it cannot throttle to \
             protect itself, so the fan goes back to firmware. Returned: {released}"
        ),
        Some(fan::Enforced::ReleasedTooHot {
            celsius,
            ceiling,
            released,
        }) => eprintln!(
            "fan: {celsius:.1} C reached the {ceiling} ceiling; the fan belongs to \
             firmware from here (ADR 0006 point 5). Returned to EC control: {released}"
        ),
        Some(fan::Enforced::ReleasedNoSensor { released }) => eprintln!(
            "fan: temperature became unreadable while under manual control; \
             returned to EC control: {released}"
        ),
        Some(fan::Enforced::Failed(e)) => {
            eprintln!("fan: could not enforce the firmware floor: {e}")
        }
    }
}

/// Fault injection for the watchdog, off unless `FW_HELPERD_DEBUG_WEDGE_AFTER` is set
/// to a number of seconds.
///
/// This exists because the watchdog's entire claim is about a failure that cannot be
/// produced on demand: a daemon that is alive, holding the fan, and no longer doing
/// any work. Unit tests with a 50 ms timeout show the logic is right; they cannot show
/// that a real wedged runtime on real hardware ends with the fan back under EC
/// control. Blocking a runtime worker thread forever reproduces exactly that, and
/// `SIGSTOP` cannot substitute — it freezes the watchdog thread too, which is a
/// failure nothing in this process could cover anyway.
///
/// Never set in production. The variable is read every tick rather than cached so the
/// cost when unset is one `getenv`, and so it cannot be armed after start.
fn maybe_wedge(started: &std::time::Instant) {
    let Some(after) = std::env::var_os("FW_HELPERD_DEBUG_WEDGE_AFTER") else {
        return;
    };
    let Some(secs) = after.to_str().and_then(|s| s.parse::<u64>().ok()) else {
        return;
    };
    if started.elapsed() < Duration::from_secs(secs) {
        return;
    }
    // Block *every* worker, not just this one. Blocking a single thread of a
    // multi-threaded runtime leaves D-Bus answering normally, which is a much weaker
    // fault than the one the watchdog claims to cover — measured on 2026-08-21, where
    // the first version of this injection left the daemon perfectly responsive.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    eprintln!(
        "DEBUG: wedging the runtime deliberately (FW_HELPERD_DEBUG_WEDGE_AFTER={secs}), \
         blocking {} worker slots. The watchdog should take the fan back.",
        workers * 2
    );
    for _ in 0..workers * 2 {
        tokio::spawn(async {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
    // And this one last, so the queued blockers get picked up first.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Release manual fan control on the way out, saying which case it was.
///
/// Reads before writing for the same reason M2's charge re-apply does: "we were not
/// holding it" and "we were, and gave it back" are different facts, and a log line
/// that cannot tell them apart cannot show that the restore paths work.
fn shut_down_fan(lease: &fan::FanLease, watchdog: &watchdog::Watchdog) {
    // A trip means this daemon stopped working while holding the fan. The machine was
    // protected, but that is a bug and it should not vanish quietly at shutdown.
    if watchdog.tripped() {
        eprintln!("note: the fan watchdog intervened at least once during this run");
    }
    let held = lease.held();
    if lease.release_now() {
        if held {
            eprintln!("released manual fan control; fan is back under EC control");
        }
    } else {
        eprintln!(
            "WARNING: could not return the fan to EC control. Run \
             fw-helper-restore-fan as root"
        );
    }
}

/// Release the fan on panic, then let the normal hook print the backtrace.
///
/// The hook must not lock anything: a panic can occur while another thread holds a
/// lock, and blocking here would leave the process alive with the fan held — the
/// exact failure ADR 0006 is written against. [`fan::FanLease`] is lock-free for this
/// reason.
fn install_panic_hook(lease: Arc<fan::FanLease>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let ok = lease.release_now();
        eprintln!(
            "fw-helperd panicked; fan {}",
            if ok {
                "returned to EC control"
            } else {
                "NOT returned to EC control - run fw-helper-restore-fan as root"
            }
        );
        previous(info);
    }));
}

/// Everything the poll loop needs. A struct rather than nine arguments, because the
/// order of nine `Arc`s of similar shape is exactly the kind of thing that gets
/// transposed silently.
struct Poll {
    conn: zbus::Connection,
    mon: Monitor,
    resumed: Arc<AtomicBool>,
    watchdog: Arc<watchdog::Watchdog>,
    lease: Arc<fan::FanLease>,
    axis: Arc<ppd::ProfileAxis>,
    pending_ppd: Arc<std::sync::atomic::AtomicU8>,
    /// The PPD profile we set ourselves, so its echo can be told from a real slider move.
    applied_ppd: Arc<std::sync::atomic::AtomicU8>,
    fs: Sysfs,
    cros_ec: Arc<dyn ec::EcTransport>,
    persisted_profile: Option<String>,
    persisted_power_limit: Option<u32>,
    known_profiles: Arc<std::sync::Mutex<Vec<Profile>>>,
    usage: UsageSampler,
    recording: Arc<record::Recording>,
    watchers: Arc<record::Watchers>,
}

async fn poll_loop(ctx: Poll) {
    let Poll {
        conn,
        mut mon,
        resumed,
        watchdog,
        lease,
        axis,
        pending_ppd,
        applied_ppd,
        fs,
        cros_ec,
        persisted_profile,
        persisted_power_limit,
        known_profiles,
        mut usage,
        recording,
        watchers,
    } = ctx;
    let iface_ref = match conn
        .object_server()
        .interface::<_, iface::Daemon>(OBJECT_PATH)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot obtain interface reference: {e}");
            return;
        }
    };

    let started = std::time::Instant::now();
    let mut restore_fan_after_resume = false;
    let mut govern = Govern::default();
    let mut startup_profile = persisted_profile;
    // What the state file says the power limit should be. Compared against the
    // profile's own value to tell "this profile's budget" from "a limit the user set
    // afterwards", which are the same field but not the same intent.
    let power_override = persisted_power_limit;
    let mut power_corrections = PowerCorrections::new(None);
    // `None` until the first sample: the first reading is a baseline, not a transition.
    let mut last_on_ac: Option<bool> = None;
    let mut last_floor_save = std::time::Instant::now();
    let mut saved_floor_revision = 0u64;
    // Ticks of GPU scanning left after the last client read telemetry. A few, not one,
    // so a client polling at our own 1 Hz does not flicker the scan on and off and
    // lose its reference point every other tick.
    let mut watch_grace: u8 = 0;
    // Wall-clock seconds into the current recording, so rows are evenly spaced even if
    // a tick is late.
    let mut recording_started: Option<std::time::Instant> = None;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;

        // The heartbeat. Deliberately the first thing after the tick and before any
        // work that could block: it must mean "the runtime is turning", not "the last
        // iteration happened to finish".
        watchdog.beat();

        maybe_wedge(&started);

        // swap(false) so the resume is consumed exactly once.
        if resumed.swap(false, Ordering::SeqCst) {
            mon.on_resume();
            // Same reasoning, different counters: /proc/stat keeps counting across
            // s2idle while the GPU's clock does not, so every rate spanning the sleep
            // is wrong in one direction or the other.
            usage.invalidate();
            eprintln!("resumed from sleep; energy and load references invalidated");
            // The fan is restored below rather than here, on the freshly sampled
            // telemetry: the machine may have woken warmer than it slept, and the
            // floor must be computed from temperatures read after the wake.
            restore_fan_after_resume = true;
            // Firmware commonly resets the charge threshold across suspend.
            if let Ok(guard) = conn
                .object_server()
                .interface::<_, iface::Daemon>(OBJECT_PATH)
                .await
            {
                let d = guard.get().await;
                d.reapply_charge_limit();
                // Firmware commonly resets RAPL across sleep. Same read-before-write, so
                // the log says whether it actually did.
                d.reapply_power_limit();
            }
        }

        let sample = mon.sample();

        // The GPU half of the load sample walks every file descriptor on the machine
        // to find DRM clients, so it runs only while someone would see the result:
        // a client reading telemetry, or a recording in progress.
        if watchers.take() {
            watch_grace = WATCH_GRACE_TICKS;
        } else {
            watch_grace = watch_grace.saturating_sub(1);
        }
        let recording_now = recording.is_active();
        usage.set_gpu_scan(watch_grace > 0 || recording_now);
        let load = usage.sample(std::time::Instant::now());

        // Apply the persisted profile on the first tick that has real telemetry: the
        // fan curve needs a temperature, and at startup there is not one yet.
        if let Some(name) = startup_profile.take() {
            match lookup_profile(&known_profiles, &name) {
                Some(p) => {
                    eprintln!("re-applying persisted profile {name}");
                    applied_ppd.store(ppd_code(p.ppd), Ordering::SeqCst);
                    let thermal = fan::Thermal::from_telemetry(&sample);
                    if let Err(e) =
                        apply_profile(&p, true, &axis, &lease, &fs, &*cros_ec, thermal).await
                    {
                        eprintln!("could not re-apply profile {name}: {e}");
                    } else if let Some(override_watts) =
                        power_override.filter(|w| *w != p.pl1_watts)
                    {
                        // The user set a power limit by hand after choosing this
                        // profile. Applying the profile would silently discard it -
                        // measured, an 18 W setting came back as the profile's 20 W
                        // after a restart - so the profile is applied first, for its
                        // fan curve and PPD, and the override put back on top.
                        eprintln!(
                            "restoring your {override_watts} W power limit over {}'s {} W",
                            p.name, p.pl1_watts
                        );
                        match fw_helper_core::PowerLimit::new(&fs).set(override_watts) {
                            Ok(()) => record_profile(&conn, &p.name, override_watts).await,
                            Err(e) => {
                                eprintln!("could not restore {override_watts} W: {e}");
                                record_profile(&conn, &p.name, p.pl1_watts).await;
                            }
                        }
                    } else {
                        record_profile(&conn, &p.name, p.pl1_watts).await;
                    }
                }
                None => eprintln!("persisted profile {name:?} is not one we know; ignoring"),
            }
        }

        // The power source changed. Apply the profile configured for it, if any.
        //
        // Edge-triggered, not level: re-applying on every tick would fight anything the
        // user chose by hand while on that source, which is a machine arguing with its
        // owner. `None` on the first sample means we have never seen a source, so the
        // first reading establishes a baseline rather than counting as a change.
        if let Some(on_ac) = sample.on_ac {
            let previous = last_on_ac.replace(on_ac);
            if is_power_source_change(previous, on_ac) {
                let source = if on_ac { "AC" } else { "battery" };
                let wanted = match conn
                    .object_server()
                    .interface::<_, iface::Daemon>(OBJECT_PATH)
                    .await
                {
                    Ok(guard) => guard.get().await.auto_profile_for(on_ac),
                    Err(_) => None,
                };
                if let Some(name) = wanted {
                    match lookup_profile(&known_profiles, &name) {
                        Some(p) => {
                            eprintln!("switched to {source}; applying profile {name}");
                            applied_ppd.store(ppd_code(p.ppd), Ordering::SeqCst);
                            let thermal = fan::Thermal::from_telemetry(&sample);
                            if let Err(e) =
                                apply_profile(&p, true, &axis, &lease, &fs, &*cros_ec, thermal)
                                    .await
                            {
                                eprintln!("could not apply {name} on {source}: {e}");
                            } else {
                                record_profile(&conn, &p.name, p.pl1_watts).await;
                            }
                        }
                        None => {
                            eprintln!("switched to {source}, but profile {name:?} no longer exists")
                        }
                    }
                }
            }
        }

        // The GNOME power slider moved. Follow it, but do not tell PPD what it just told
        // us, and do not re-apply a profile we have only just applied ourselves: setting
        // PPD makes PPD emit a change, which arrives here as an event indistinguishable
        // from a user moving the slider.
        let code = pending_ppd.swap(0, Ordering::SeqCst);
        if let Some(target) = ppd_from_code(code) {
            if applied_ppd.swap(0, Ordering::SeqCst) != code {
                // Canonical name, looked up in the merged set: a user profile that
                // replaces a built-in is used, but one under a new name never becomes
                // the slider's destination.
                let canonical = Profile::canonical_name_for(target);
                let p = lookup_profile(&known_profiles, canonical)
                    .unwrap_or_else(|| Profile::for_ppd(target));
                eprintln!(
                    "PPD switched to {}; following with profile {}",
                    target.as_str(),
                    p.name
                );
                // No explicit budget reset here any more. `record_profile` below
                // updates the desired setpoint, and the enforcement block notices the
                // change and resets — which also covers the D-Bus paths this branch
                // never sees.
                let thermal = fan::Thermal::from_telemetry(&sample);
                if let Err(e) =
                    apply_profile(&p, false, &axis, &lease, &fs, &*cros_ec, thermal).await
                {
                    eprintln!("could not follow PPD to {}: {e}", p.name);
                } else {
                    // Before the enforcement below runs, or it would restore the
                    // previous profile's budget over the one just applied.
                    record_profile(&conn, &p.name, p.pl1_watts).await;
                }
            }
        }

        // Re-assert the power limit. A verified write is not a durable one here: see
        // Daemon::enforce_power_limit. Bounded, so we never fight firmware forever.
        if let Ok(guard) = conn
            .object_server()
            .interface::<_, iface::Daemon>(OBJECT_PATH)
            .await
        {
            let daemon = guard.get().await;
            // Before the budget check, so a freshly set limit is always enforced even
            // if the previous setpoint had used the budget up.
            power_corrections.observe(daemon.desired_power_limit());
            if power_corrections.may_correct() {
                if let Some((desired, observed)) = daemon.enforce_power_limit() {
                    let used = power_corrections.record();
                    eprintln!(
                        "power limit was {observed} W, expected {desired} W; re-applied \
                         (correction {used} of {MAX_POWER_CORRECTIONS})"
                    );
                    if power_corrections.just_exhausted() {
                        eprintln!(
                            "power limit: firmware keeps overriding us; giving up until it is \
                             set again. This machine may re-derive PL1 from platform_profile"
                        );
                    }
                }
            }
        }

        // Persist anything newly learned about the firmware floor, at most once a
        // minute and only when the revision has actually moved.
        if last_floor_save.elapsed() >= FLOOR_SAVE_INTERVAL {
            last_floor_save = std::time::Instant::now();
            if let Some((revision, observations)) = lease.floor_snapshot() {
                if revision != saved_floor_revision && !observations.is_empty() {
                    saved_floor_revision = revision;
                    if let Ok(guard) = conn
                        .object_server()
                        .interface::<_, iface::Daemon>(OBJECT_PATH)
                        .await
                    {
                        guard.get().await.save_floor(observations);
                    }
                }
            }
        }

        if std::mem::take(&mut restore_fan_after_resume) {
            restore_fan(&lease, &sample);
        }
        govern_fan(&lease, &sample, &mut govern);

        // `get()`, not `get_mut()`. The write lock would queue behind any method
        // still running, and a method awaiting a polkit prompt holds the read lock for
        // as long as the dialog is on screen. Telemetry - and therefore the watchdog's
        // heartbeat - must not depend on how quickly someone types a password.
        let guard = iface_ref.get().await;

        // Recording and the HUD line both need control state the telemetry sample does
        // not carry - the limit we are enforcing, who owns the fan, the active profile.
        let context = guard.record_context();

        if recording_now {
            let started = recording_started.get_or_insert_with(std::time::Instant::now);
            let elapsed = started.elapsed().as_secs();
            guard
                .recording()
                .push(&record::row(elapsed, &sample, &load, &context));
        } else {
            recording_started = None;
        }

        // Written every tick regardless: an overlay reading this in a game's frame loop
        // must never be the reason the daemon does extra work, and one short file is
        // cheaper than answering a D-Bus call per frame.
        record::write_hud(&record::hud_line(
            &sample,
            &load,
            &context,
            recording.status().as_ref(),
        ));

        guard.update_usage(load);
        if guard.update(sample) {
            let emitter = iface_ref.signal_emitter();
            let _ = guard.telemetry_changed(emitter).await;
            let _ = guard.critical_temperatures_changed(emitter).await;
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_first_power_reading_is_a_baseline_not_a_transition() {
        // Starting the daemon must not count as plugging in: it would override whatever
        // the user last chose by hand, every boot.
        assert!(!super::is_power_source_change(None, true));
        assert!(!super::is_power_source_change(None, false));
    }

    #[test]
    fn only_a_real_change_of_source_counts() {
        assert!(super::is_power_source_change(Some(false), true));
        assert!(super::is_power_source_change(Some(true), false));
        // Level-triggered would fight anything the user chose while on that source.
        assert!(!super::is_power_source_change(Some(true), true));
        assert!(!super::is_power_source_change(Some(false), false));
    }
}
