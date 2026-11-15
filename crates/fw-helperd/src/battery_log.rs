//! The battery log's I/O shell: turns telemetry into [`BatteryState`], appends lines.
//!
//! All the decisions - when a line is due, what the file means - live in
//! `fw_helper_core::battery_log`. This only reads the two things telemetry does not
//! carry (the screen and the cycle count) and owns the file.
//!
//! Lives in the daemon because the daemon is the one process running from boot to
//! shutdown, through every login and lock screen, and it already reads the battery
//! every second - so the log costs no extra wake-ups at all.

use fw_helper_core::battery_log::{log_path, BatteryState, Entry, Event, Tracker, HEADER};
use fw_helper_core::{Monitor, Sysfs, Telemetry};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct BatteryLog {
    fs: Sysfs,
    path: PathBuf,
    /// `sys/class/drm/<card>-eDP-<n>/dpms`, resolved once. Connector names are stable
    /// for the life of the boot, unlike hwmon indices.
    screen: Option<String>,
    /// `sys/class/power_supply/<BAT>`, resolved by `type`, never by name.
    battery: Option<String>,
    tracker: Mutex<Tracker>,
}

impl BatteryLog {
    pub fn new(fs: Sysfs) -> Self {
        let screen = find(&fs, "sys/class/drm", |name, fs| {
            let rel = format!("sys/class/drm/{name}/dpms");
            (name.contains("-eDP-") && fs.exists(&rel)).then_some(rel)
        });
        let battery = find(&fs, "sys/class/power_supply", |name, fs| {
            let rel = format!("sys/class/power_supply/{name}");
            (fs.read_string(&format!("{rel}/type")).ok().as_deref() == Some("Battery"))
                .then_some(rel)
        });
        let path = log_path();
        eprintln!(
            "battery log: {} (screen from {}, battery {})",
            path.display(),
            screen
                .as_deref()
                .unwrap_or("nowhere - screen state unknown"),
            battery.as_deref().unwrap_or("not found")
        );
        Self {
            fs,
            path,
            screen,
            battery,
            tracker: Mutex::new(Tracker::default()),
        }
    }

    fn screen_on(&self) -> Option<bool> {
        let raw = self.fs.read_string(self.screen.as_ref()?).ok()?;
        Some(raw.eq_ignore_ascii_case("on"))
    }

    fn state(&self, t: &Telemetry) -> BatteryState {
        BatteryState {
            on_ac: t.on_ac,
            screen_on: self.screen_on(),
            status: t.battery_status.clone(),
            percent: t.battery_percent,
            wh: t.battery_wh,
            wh_full: t.battery_wh_full,
            // Read only when a line is written, in `write`.
            cycles: None,
        }
    }

    /// One poll-loop tick. Writes a line only if something worth one changed.
    pub fn tick(&self, t: &Telemetry) {
        let state = self.state(t);
        let event = self
            .tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observe(now(), &state);
        if let Some(event) = event {
            self.write(event, state);
        }
    }

    /// Log an event that did not come from the tick's own change detection, with a
    /// fresh reading - the poll loop's last sample may be a second old, and before a
    /// suspend that second can hold the screen turning off.
    pub fn event_now(&self, event: Event) {
        let t = Monitor::new(self.fs.clone()).sample();
        self.event_with(event, &t);
    }

    /// As [`Self::event_now`], with telemetry the caller already has.
    pub fn event_with(&self, event: Event, t: &Telemetry) {
        let state = self.state(t);
        self.tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note(now(), &state);
        self.write(event, state);
    }

    fn write(&self, event: Event, mut state: BatteryState) {
        state.cycles = self
            .battery
            .as_ref()
            .and_then(|b| self.fs.read_u64(&format!("{b}/cycle_count")).ok());
        let line = Entry {
            unix_time: now(),
            event,
            state,
        }
        .to_line();
        if let Err(e) = self.append(&line) {
            // Once per failure, which for a persistent fault is once per change -
            // a few hundred a day at most, and it should be noticed.
            eprintln!("battery log: cannot write {}: {e}", self.path.display());
        }
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // A header only on an empty file, so a rotated or deleted log restarts cleanly.
        let mut out = String::new();
        if f.metadata()?.len() == 0 {
            out.push_str(HEADER);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        // One write, so a power cut tears at most the last line, which the reader skips.
        f.write_all(out.as_bytes())
    }
}

fn find(fs: &Sysfs, dir: &str, pick: impl Fn(&str, &Sysfs) -> Option<String>) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(fs.path(dir))
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names.iter().find_map(|n| pick(n, fs))
}
