//! Session recording, and the one-line status file an in-game overlay reads.
//!
//! Recording lives in the daemon rather than in the GUI for two reasons, and the first
//! is not negotiable: `energy_uj` is mode 0400 (the PLATYPUS mitigation, ADR 0009), so
//! an unprivileged recorder physically cannot read package power. The second is the
//! point of the feature — a recording has to survive the window being closed, because
//! "start it, play a game, come back and look" is the whole request.
//!
//! ## The HUD file
//!
//! On GNOME/Wayland no ordinary client can draw above a fullscreen window, so the
//! in-game overlay is MangoHud's job: it is loaded *into* the game by the Vulkan loader
//! and paints into the frame before presentation, which is why the compositor never
//! gets a say. MangoHud will display the output of a command (`exec=`), so fw-helper
//! supplies the things MangoHud cannot know — the power limit we are enforcing, who
//! owns the fan, the active profile, the pack temperature.
//!
//! That command runs **inside the game's frame loop**, so it must be trivial. The
//! daemon writes one short line here every tick and the config is `exec=cat` on it;
//! anything that opened a D-Bus connection per frame would be a frame-time bug.

use fw_helper_core::{session, Recorder, Row, Telemetry, Usage, PACKAGE_TEMP_LABEL};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Where recordings live. Created on demand, as the state file's directory is —
/// `postrm purge` already removes the parent, so this needs no packaging of its own.
const SESSION_DIR: &str = "/var/lib/fw-helper/sessions";

/// Runtime directory, created by systemd's `RuntimeDirectory=fw-helper` and therefore
/// cleaned up when the daemon stops. A stale HUD line outliving the daemon would show
/// a game numbers that stopped being true some time ago.
const HUD_DIR: &str = "/run/fw-helper";

/// Both of the above move under `$XDG_RUNTIME_DIR` when the daemon is running on the
/// session bus, which is the existing development mode and is never used in production
/// — the systemd unit does not set it. Without this, iterating on recording would need
/// root purely to have somewhere to write, and the whole point of the session-bus mode
/// is that development does not need root.
fn dev_mode() -> bool {
    std::env::var_os("FW_HELPERD_SESSION_BUS").is_some()
}

fn dev_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(base).join("fw-helper-dev"))
}

/// Directory recordings are written to.
pub fn session_dir() -> PathBuf {
    match dev_mode().then(dev_root).flatten() {
        Some(root) => root.join("sessions"),
        None => PathBuf::from(SESSION_DIR),
    }
}

fn hud_dir() -> PathBuf {
    match dev_mode().then(dev_root).flatten() {
        Some(root) => root,
        None => PathBuf::from(HUD_DIR),
    }
}

/// The one-line status file an in-game overlay reads.
pub fn hud_file() -> PathBuf {
    hud_dir().join("hud")
}

/// Whether any client is currently looking at telemetry.
///
/// The GPU scan walks every file descriptor on the machine to find DRM clients, so it
/// is switched off unless someone would see the result. The telemetry property getter
/// sets this; the poll loop consumes it and keeps the scan alive for a few ticks
/// afterwards so a client polling at 1 Hz does not flicker it on and off.
#[derive(Debug, Default)]
pub struct Watchers {
    touched: AtomicBool,
}

impl Watchers {
    pub fn touch(&self) {
        self.touched.store(true, Ordering::Relaxed);
    }

    /// True if telemetry was read since the last call.
    pub fn take(&self) -> bool {
        self.touched.swap(false, Ordering::Relaxed)
    }
}

/// What a client is told about the recording in progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub label: String,
    pub name: String,
    pub path: String,
    pub started_unix: u64,
    pub samples: u64,
    pub elapsed_secs: u64,
}

/// The recording in progress, if any.
///
/// Behind a mutex so the interface's write methods can take `&self` — a `&mut self`
/// method makes zbus hold the interface write lock for the whole call, which is the
/// mistake that once stalled telemetry for the length of a password prompt. The lock is
/// never held across an await; every method here is synchronous.
#[derive(Debug, Default)]
pub struct Recording {
    slot: Mutex<Option<Recorder>>,
}

impl Recording {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin recording. Returns the path written, or a message naming the fix.
    pub fn start(&self, label: &str) -> Result<String, String> {
        self.start_in(&session_dir(), label)
    }

    /// As [`Self::start`], against an explicit directory.
    ///
    /// Exists so the recording logic is testable without the polkit gate above it or
    /// a writable `/var/lib`: polkit is a system-bus service, so no write method on
    /// this interface can be exercised in the session-bus development mode.
    pub fn start_in(&self, dir: &std::path::Path, label: &str) -> Result<String, String> {
        let Ok(mut slot) = self.slot.lock() else {
            return Err("the recorder is in an unusable state; restart fw-helperd".into());
        };
        if let Some(active) = slot.as_ref() {
            return Err(format!(
                "already recording {:?}; stop it first",
                active.label()
            ));
        }
        let recorder = Recorder::start(dir, label)
            .map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
        let path = recorder.path().display().to_string();
        eprintln!(
            "recording {:?} to {path} (stops itself after {} h)",
            recorder.label(),
            session::MAX_SESSION_SECS / 3600
        );
        *slot = Some(recorder);
        Ok(path)
    }

    /// Stop recording and return the finished file.
    pub fn stop(&self) -> Result<String, String> {
        let Ok(mut slot) = self.slot.lock() else {
            return Err("the recorder is in an unusable state; restart fw-helperd".into());
        };
        let Some(recorder) = slot.take() else {
            return Err("not recording".into());
        };
        let samples = recorder.samples();
        let path = recorder.finish();
        eprintln!(
            "recording stopped after {samples} samples: {}",
            path.display()
        );
        Ok(path.display().to_string())
    }

    pub fn is_active(&self) -> bool {
        self.slot.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    /// The name of the recording in progress, if any.
    pub fn active_name(&self) -> Option<String> {
        let slot = self.slot.lock().ok()?;
        slot.as_ref().map(|r| r.name())
    }

    pub fn status(&self) -> Option<Status> {
        let slot = self.slot.lock().ok()?;
        let r = slot.as_ref()?;
        Some(Status {
            label: r.label().to_string(),
            name: r.name(),
            path: r.path().display().to_string(),
            started_unix: r.started_unix(),
            samples: r.samples(),
            elapsed_secs: r.elapsed_secs(),
        })
    }

    /// Append one sample, and stop the recording if it has run long enough.
    ///
    /// A write failure stops the recording rather than logging once per second until
    /// somebody notices the disk filled up.
    pub fn push(&self, row: &Row) {
        let Ok(mut slot) = self.slot.lock() else {
            return;
        };
        let Some(recorder) = slot.as_mut() else {
            return;
        };
        if let Err(e) = recorder.push(row) {
            eprintln!(
                "recording stopped: cannot write {}: {e}",
                recorder.path().display()
            );
            *slot = None;
            return;
        }
        if recorder.is_full() {
            let samples = recorder.samples();
            eprintln!(
                "recording reached {} h and stopped itself after {samples} samples: {}",
                session::MAX_SESSION_SECS / 3600,
                recorder.path().display()
            );
            *slot = None;
        }
    }
}

/// Everything about the machine's control state that a row or a HUD line needs, and
/// that telemetry alone does not carry.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub pl1_watts: Option<u32>,
    pub fan_mode: Option<String>,
    pub fan_duty: Option<u8>,
    pub profile: Option<String>,
}

/// Build one recorded row from a telemetry and usage sample.
pub fn row(elapsed_secs: u64, t: &Telemetry, u: &Usage, ctx: &Context) -> Row {
    Row {
        t_s: elapsed_secs,
        unix_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        cpu_pct: u.cpu_percent,
        cpu_mhz: u.cpu_mhz,
        cpu_w: u.cpu_watts,
        gpu_w: u.gpu_watts,
        gpu_pct: u.gpu_percent,
        gpu_mhz: u.gpu_mhz,
        gpu_top: u.top_gpu_client.as_ref().map(|(comm, _)| comm.clone()),
        mem_used_mb: u.mem_used_kb.map(|kb| kb / 1024),
        mem_total_mb: u.mem_total_kb.map(|kb| kb / 1024),
        package_w: t.package_watts,
        system_w: t.system_watts,
        pl1_w: ctx.pl1_watts,
        peci_c: t.control_temp().map(|r| r.celsius),
        coretemp_c: sensor(t, PACKAGE_TEMP_LABEL),
        battery_c: t.battery_temp().map(|r| r.celsius),
        board_c: board_temp(t),
        fan_rpm: t.fan_rpm,
        fan_duty: ctx.fan_duty,
        fan_mode: ctx.fan_mode.clone(),
        throttle_events: u.cpu_throttle_events,
        throttle_ms: u.cpu_throttle_ms,
        gpu_throttle: (!u.gpu_throttle.is_empty()).then(|| u.gpu_throttle.join("+")),
        profile: ctx.profile.clone(),
        on_ac: t.on_ac,
        battery_pct: t.battery_percent,
    }
}

fn sensor(t: &Telemetry, label: &str) -> Option<f64> {
    t.temps.iter().find(|r| r.label == label).map(|r| r.celsius)
}

/// The hottest board sensor.
///
/// Board sensors are what has no protection of its own besides the battery — they read
/// crit around 87 °C while the CPU looks after itself at Tjmax (ADR 0011). Identified by
/// exclusion because their names are chip part numbers (`local_f75397@4c`,
/// `ddr_f75303@4d`) that mean nothing and vary by board revision.
fn board_temp(t: &Telemetry) -> Option<f64> {
    t.temps
        .iter()
        .filter(|r| {
            let l = r.label.as_str();
            l != PACKAGE_TEMP_LABEL && !l.starts_with("peci") && !l.starts_with("battery")
        })
        .map(|r| r.celsius)
        .max_by(f64::total_cmp)
}

/// One line for an in-game HUD: the things MangoHud cannot know.
///
/// MangoHud reports frame rate, CPU load and package power far better than a shelled-out
/// command could, and none of that is repeated here. Two things it cannot see:
///
/// 1. **fw-helper's own control state** — the limit being enforced, who owns the fan,
///    the active profile, the pack temperature.
/// 2. **GPU load on this machine.** Measured 2026-09-04 with MangoHud 0.6.9.1: its
///    Intel support is i915-only, and this board runs `xe`, so it logs "no
///    discrete/integrated i915 devices found" and *disables gpu_stats entirely*. It
///    then falls back to `intel_gpu_top`, which needs `perf_event_open` and hits the
///    same `perf_event_paranoid` wall that ruled the PMU out for us. So the in-game HUD
///    has no GPU number at all unless we supply one — and we can, because the fdinfo
///    counters need no permission.
pub fn hud_line(t: &Telemetry, u: &Usage, ctx: &Context, recording: Option<&Status>) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(pct) = u.gpu_percent {
        let mut gpu = format!("GPU {pct:.0}%");
        // Zero is filtered rather than printed: in a HUD line "0MHz" reads as a broken
        // sensor, where the window has room to say "parked". Both come from the same
        // RC6 reading (see `UsageSampler::read_gpu_mhz`).
        if let Some(mhz) = u.gpu_mhz.filter(|m| *m > 0) {
            gpu.push_str(&format!(" {mhz}MHz"));
        }
        if let Some(w) = u.gpu_watts {
            gpu.push_str(&format!(" {w:.1}W"));
        }
        parts.push(gpu);
    }

    if let Some(w) = ctx.pl1_watts {
        parts.push(format!("PL1 {w}W"));
    }
    if let Some(mode) = ctx.fan_mode.as_deref() {
        // Which side of ADR 0006 the fan is on. A speed means something different when
        // firmware owns the fan than when we do, and a reader cannot tell them apart.
        let owner = if mode == "manual" { "fw" } else { "ec" };
        match (t.fan_rpm, ctx.fan_duty) {
            (Some(rpm), Some(duty)) if mode == "manual" => {
                parts.push(format!("fan {rpm}rpm {}% {owner}", duty as u32 * 100 / 255));
            }
            (Some(rpm), _) => parts.push(format!("fan {rpm}rpm {owner}")),
            _ => parts.push(format!("fan {owner}")),
        }
    }
    if let Some(p) = ctx.profile.as_deref() {
        parts.push(p.to_string());
    }
    if let Some(b) = t.battery_temp() {
        // The pack, which is the one component with a low limit and no protection of
        // its own. MangoHud reports no battery temperature at all.
        parts.push(format!("pack {:.0}C", b.celsius));
    }
    if let Some(r) = recording {
        let (m, s) = (r.elapsed_secs / 60, r.elapsed_secs % 60);
        parts.push(format!("REC {m}:{s:02}"));
    }

    parts.join(" | ")
}

/// Publish the HUD line, replacing it atomically.
///
/// Written to a temporary file and renamed, so a reader in a game's frame loop can
/// never catch a half-written line.
pub fn write_hud(line: &str) {
    let dir = hud_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("hud.tmp");
    if std::fs::write(&tmp, format!("{line}\n")).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, hud_file());
}

/// Remove the HUD file on shutdown, so nothing reads numbers from a daemon that is no
/// longer running. systemd's `RuntimeDirectory=` covers the ordinary case; this covers
/// a daemon started by hand.
pub fn clear_hud() {
    let _ = std::fs::remove_file(hud_file());
}

#[cfg(test)]
mod tests {
    use super::*;
    use fw_helper_core::telemetry::TempReading;

    fn temp(label: &str, celsius: f64) -> TempReading {
        TempReading {
            label: label.into(),
            celsius,
            critical: None,
        }
    }

    fn telemetry() -> Telemetry {
        Telemetry {
            temps: vec![
                temp("local_f75397@4c", 62.1),
                temp("peci-temp", 78.9),
                temp("battery_temp@b", 38.9),
                temp("ddr_f75303@4d", 66.4),
                temp(PACKAGE_TEMP_LABEL, 81.0),
            ],
            fan_rpm: Some(4820),
            package_watts: Some(34.82),
            battery_percent: Some(80),
            on_ac: Some(true),
            ..Default::default()
        }
    }

    fn context() -> Context {
        Context {
            pl1_watts: Some(35),
            fan_mode: Some("manual".into()),
            fan_duty: Some(180),
            profile: Some("max".into()),
        }
    }

    #[test]
    fn the_board_temperature_is_the_hottest_board_sensor() {
        // Board sensors are named after chip part numbers, so they are identified by
        // excluding the ones we do know. Getting that wrong would silently report the
        // CPU as the board and put an 81 C reading against an ~87 C limit.
        assert_eq!(board_temp(&telemetry()), Some(66.4));
    }

    #[test]
    fn each_temperature_lands_in_its_own_column() {
        let sample = row(42, &telemetry(), &Usage::default(), &context());
        assert_eq!(sample.peci_c, Some(78.9));
        assert_eq!(sample.coretemp_c, Some(81.0));
        assert_eq!(sample.battery_c, Some(38.9));
        assert_eq!(sample.board_c, Some(66.4));
    }

    #[test]
    fn the_power_limit_is_recorded_on_every_row() {
        // The setpoint is the line a power graph is read against: draw sitting on it is
        // how PL1 binding is inferred, because the package exposes no throttle flag.
        // Recorded per row rather than once per session because it can change mid-run.
        let sample = row(1, &telemetry(), &Usage::default(), &context());
        assert_eq!(sample.pl1_w, Some(35));
        assert_eq!(sample.package_w, Some(34.82));
    }

    #[test]
    fn absent_load_readings_stay_absent() {
        // Usage::default means the sampler had no reference point yet. Those must not
        // become zeroes: 0% CPU is a measurement and a graph would draw it as one.
        let sample = row(0, &telemetry(), &Usage::default(), &context());
        assert_eq!(sample.cpu_pct, None);
        assert_eq!(sample.gpu_pct, None);
        assert_eq!(sample.mem_used_mb, None);
    }

    #[test]
    fn gpu_throttle_reasons_are_joined_into_one_field() {
        let usage = Usage {
            gpu_throttle: vec!["pl1".into(), "thermal".into()],
            ..Default::default()
        };
        let sample = row(0, &telemetry(), &usage, &context());
        assert_eq!(sample.gpu_throttle.as_deref(), Some("pl1+thermal"));
        // And no throttling is absent, not the empty string.
        let quiet = row(0, &telemetry(), &Usage::default(), &context());
        assert_eq!(quiet.gpu_throttle, None);
    }

    #[test]
    fn the_hud_line_says_who_owns_the_fan() {
        // A speed means a different thing under each owner, and ADR 0006 turns on the
        // user being able to tell deliberate control from a stuck fan.
        let line = hud_line(&telemetry(), &Usage::default(), &context(), None);
        assert!(line.contains("PL1 35W"), "{line}");
        assert!(line.contains("4820rpm"), "{line}");
        assert!(line.contains("fw"), "{line}");
        assert!(line.contains("pack 39C"), "{line}");

        let ec = Context {
            fan_mode: Some("auto".into()),
            fan_duty: None,
            ..context()
        };
        let line = hud_line(&telemetry(), &Usage::default(), &ec, None);
        assert!(line.contains("fan 4820rpm ec"), "{line}");
    }

    #[test]
    fn the_hud_line_carries_gpu_load_because_mangohud_cannot() {
        // MangoHud disables gpu_stats entirely on this board - its Intel path is
        // i915-only and this is xe - so without this the in-game HUD has no GPU number.
        let usage = Usage {
            gpu_percent: Some(91.4),
            gpu_mhz: Some(1950),
            ..Default::default()
        };
        let line = hud_line(&telemetry(), &usage, &context(), None);
        assert!(line.contains("GPU 91% 1950MHz"), "{line}");
    }

    #[test]
    fn the_hud_line_is_a_single_line() {
        // It is read by `cat` inside a game's frame loop; a second line would be a
        // second HUD column appearing from nowhere.
        let status = Status {
            elapsed_secs: 872,
            ..Default::default()
        };
        let line = hud_line(&telemetry(), &Usage::default(), &context(), Some(&status));
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("REC 14:32"), "{line}");
    }

    #[test]
    fn recording_twice_is_refused_rather_than_silently_replacing_the_first() {
        let dir = temp_dir("twice");
        let r = Recording::new();
        assert!(!r.is_active());
        assert!(r.status().is_none());
        // Not started, so stopping says so instead of pretending it worked.
        assert!(r.stop().is_err());

        r.start_in(&dir, "first").expect("start");
        let second = r.start_in(&dir, "second").expect_err("must refuse");
        // And the message names the first one, so the fix is obvious.
        assert!(second.contains("first"), "{second}");
        assert!(r.is_active());
        r.stop().expect("stop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recording_round_trips_from_start_to_a_readable_session() {
        // Everything the D-Bus methods do, minus the polkit gate they sit behind.
        let dir = temp_dir("roundtrip");
        let r = Recording::new();
        r.start_in(&dir, "Stress Test").expect("start");

        let usage = Usage {
            cpu_percent: Some(97.5),
            gpu_percent: Some(41.0),
            mem_used_kb: Some(21_366_504),
            mem_total_kb: Some(32_388_396),
            cpu_throttle_events: 1,
            cpu_throttle_ms: 30,
            gpu_throttle: vec!["pl1".into()],
            ..Default::default()
        };
        for t in 0..5 {
            r.push(&row(t, &telemetry(), &usage, &context()));
        }

        let status = r.status().expect("a status while recording");
        assert_eq!(status.label, "Stress Test");
        assert_eq!(status.samples, 5);

        r.stop().expect("stop");
        assert!(!r.is_active());

        let listed = fw_helper_core::session::list(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "stress-test");

        let session = fw_helper_core::Session::read(&listed[0].path).expect("read back");
        assert_eq!(session.rows.len(), 5);
        assert_eq!(session.rows[0].cpu_pct, Some(97.5));
        assert_eq!(session.rows[0].pl1_w, Some(35));
        assert_eq!(session.rows[0].coretemp_c, Some(81.0));
        assert_eq!(session.rows[0].gpu_throttle.as_deref(), Some("pl1"));
        assert_eq!(session.rows[4].t_s, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_recording_in_progress_knows_its_own_name() {
        // Deleting a session unlinks its file. Unlinking one that is still being
        // written does NOT fail the writes - the descriptor keeps addressing the
        // now-nameless inode and the rows go nowhere visible, then vanish on close.
        // So the interface refuses that delete, and this is the name it checks.
        let dir = temp_dir("active-name");
        let r = Recording::new();
        assert_eq!(r.active_name(), None);
        r.start_in(&dir, "in progress").expect("start");
        let name = r.active_name().expect("a name while recording");
        assert!(name.starts_with("in-progress-"), "{name}");
        r.stop().expect("stop");
        assert_eq!(r.active_name(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("fw-helperd-record-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
