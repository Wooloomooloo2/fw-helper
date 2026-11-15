//! The battery log: what the battery did, kept as one line per change.
//!
//! The question it answers is the one a percentage cannot: *where did the charge go?*
//! Time on battery, energy used, and the average draw split three ways - screen on,
//! screen off but awake, and asleep - plus what the battery was last charged to.
//!
//! ## Cost
//!
//! The daemon already samples the battery once a second for telemetry, so this adds no
//! wake-ups and no reads beyond one DRM connector file for the screen state. A line is
//! written only when something changes: each 1 % step, a plug or unplug, the screen,
//! the charge status, a suspend or resume. On battery with the screen on there is also a
//! heartbeat every [`HEARTBEAT_SECS`], so a missed change costs at most that much
//! resolution. Order of a few hundred lines a day.
//!
//! ## The file
//!
//! Append-only CSV. **Each line's state holds until the next line**, so energy for an
//! interval is the drop in `wh` between consecutive lines. Columns are read by header
//! name and only ever appended, the same contract as recorded sessions.
//!
//! `start` and `stop` bracket every daemon lifetime. The interval after a `stop` is a
//! gap, and so is one ending at a `start` with no `stop` before it (a crash or a
//! `SIGKILL`): we do not know what happened in it, so it is never credited to any
//! state. Sleep is different - the daemon is frozen in s2idle but writes `suspend` while
//! logind's delay lock still holds the machine up, and `resume` on the first tick back,
//! so asleep time and the energy it cost are both *measured*, not inferred.
//!
//! This design follows the Windows sibling's ADR 0018, minus the parts Windows needed
//! to work around: Modern Standby hides suspend from applications there, so it had to
//! rebuild the asleep figure from the event log.

use std::fmt::Write as _;

/// Heartbeat interval on battery with the screen on.
pub const HEARTBEAT_SECS: u64 = 600;

/// An AC stretch shorter than this, between two battery stretches, is charger contact
/// bounce rather than a charge. The Windows log caught one lasting 2 s.
pub const BOUNCE_SECS: u64 = 60;

/// Where the daemon keeps the log. World-readable on purpose: the CLI reads it
/// directly, so `fw-helperctl battery` works with no daemon running, and nothing in it
/// is finer than what `/sys/class/power_supply` already shows every user.
pub const LOG_FILE: &str = "/var/lib/fw-helper/battery/events.csv";

/// [`LOG_FILE`], or its development twin under `$XDG_RUNTIME_DIR` when the daemon runs
/// on the session bus - the same switch recording uses, so development never needs root.
pub fn log_path() -> std::path::PathBuf {
    if std::env::var_os("FW_HELPERD_SESSION_BUS").is_some() {
        if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
            return std::path::PathBuf::from(base).join("fw-helper-dev/battery/events.csv");
        }
    }
    std::path::PathBuf::from(LOG_FILE)
}

/// Column header. Append new columns at the end only: readers map by name.
pub const HEADER: &str = "unix_time,event,on_ac,screen_on,status,percent,wh,wh_full,cycles";

/// What the battery and its surroundings look like at one instant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryState {
    pub on_ac: Option<bool>,
    pub screen_on: Option<bool>,
    /// The kernel's `status`: `Charging`, `Discharging`, `Not charging`, `Full`.
    /// `Not charging` on mains is the charge limit holding.
    pub status: Option<String>,
    pub percent: Option<u64>,
    pub wh: Option<f64>,
    pub wh_full: Option<f64>,
    pub cycles: Option<u64>,
}

impl BatteryState {
    /// The fields whose change is worth a line. Cycles and full capacity drift too
    /// slowly to matter and are carried along on whatever line comes next.
    fn change(&self, before: &Self) -> Option<Event> {
        if self.on_ac != before.on_ac {
            Some(Event::Ac)
        } else if self.screen_on != before.screen_on {
            Some(Event::Screen)
        } else if self.status != before.status {
            Some(Event::Status)
        } else if self.percent != before.percent {
            Some(Event::Percent)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Stop,
    Ac,
    Screen,
    Status,
    Percent,
    Suspend,
    Resume,
    Heartbeat,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Start => "start",
            Event::Stop => "stop",
            Event::Ac => "ac",
            Event::Screen => "screen",
            Event::Status => "status",
            Event::Percent => "percent",
            Event::Suspend => "suspend",
            Event::Resume => "resume",
            Event::Heartbeat => "heartbeat",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "start" => Event::Start,
            "stop" => Event::Stop,
            "ac" => Event::Ac,
            "screen" => Event::Screen,
            "status" => Event::Status,
            "percent" => Event::Percent,
            "suspend" => Event::Suspend,
            "resume" => Event::Resume,
            "heartbeat" => Event::Heartbeat,
            _ => return None,
        })
    }
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub unix_time: u64,
    pub event: Event,
    pub state: BatteryState,
}

fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|v| v.to_string()).unwrap_or_default()
}

fn flag(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "1",
        Some(false) => "0",
        None => "",
    }
}

impl Entry {
    /// The line, without a trailing newline.
    pub fn to_line(&self) -> String {
        let s = &self.state;
        let mut out = String::new();
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},{},{}",
            self.unix_time,
            self.event.as_str(),
            flag(s.on_ac),
            flag(s.screen_on),
            // A status never contains a comma, but a malformed one must not shift
            // every column after it.
            s.status.as_deref().unwrap_or("").replace(',', " "),
            opt(&s.percent),
            s.wh.map(|w| format!("{w:.2}")).unwrap_or_default(),
            s.wh_full.map(|w| format!("{w:.2}")).unwrap_or_default(),
            opt(&s.cycles),
        );
        out
    }
}

/// Parse a whole log. Lines that do not parse are skipped rather than failing the
/// file: one torn line (power lost mid-write) must not hide a month of history.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<&str> = header.split(',').collect();
    let idx = |name: &str| cols.iter().position(|c| *c == name);
    let (Some(t), Some(e)) = (idx("unix_time"), idx("event")) else {
        return Vec::new();
    };
    let ac = idx("on_ac");
    let scr = idx("screen_on");
    let st = idx("status");
    let pct = idx("percent");
    let wh = idx("wh");
    let whf = idx("wh_full");
    let cyc = idx("cycles");

    let mut out = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        let get = |i: Option<usize>| i.and_then(|i| f.get(i)).copied().filter(|s| !s.is_empty());
        let b = |i| match get(i) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        let (Some(unix_time), Some(event)) = (
            get(Some(t)).and_then(|s| s.parse().ok()),
            get(Some(e)).and_then(Event::parse),
        ) else {
            continue;
        };
        out.push(Entry {
            unix_time,
            event,
            state: BatteryState {
                on_ac: b(ac),
                screen_on: b(scr),
                status: get(st).map(str::to_string),
                percent: get(pct).and_then(|s| s.parse().ok()),
                wh: get(wh).and_then(|s| s.parse().ok()),
                wh_full: get(whf).and_then(|s| s.parse().ok()),
                cycles: get(cyc).and_then(|s| s.parse().ok()),
            },
        });
    }
    out
}

/// Decides, tick by tick, whether the state is worth a line.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<BatteryState>,
    last_written: u64,
}

impl Tracker {
    /// Observe the state at `now`. Returns the event to log, if any; the caller writes
    /// the line and the tracker assumes it was written.
    pub fn observe(&mut self, now: u64, state: &BatteryState) -> Option<Event> {
        let event = match &self.last {
            None => Some(Event::Start),
            Some(before) => state.change(before).or_else(|| {
                let heartbeat = state.on_ac == Some(false)
                    && state.screen_on != Some(false)
                    && now.saturating_sub(self.last_written) >= HEARTBEAT_SECS;
                heartbeat.then_some(Event::Heartbeat)
            }),
        };
        if event.is_some() {
            self.note(now, state);
        }
        event
    }

    /// Record that a line was written by another route (suspend, resume, stop), so the
    /// next tick compares against it rather than logging the same change twice.
    pub fn note(&mut self, now: u64, state: &BatteryState) {
        self.last = Some(state.clone());
        self.last_written = now;
    }
}

// ---- analysis -------------------------------------------------------------------

/// What the time in one interval counts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ScreenOn,
    ScreenOff,
    Asleep,
    Ac,
    /// The daemon was not running: nothing is known.
    Gap,
}

#[derive(Debug, Clone, Copy)]
struct Interval {
    from: u64,
    to: u64,
    kind: Kind,
    /// Drop in stored energy over the interval; negative means it rose.
    wh_drop: Option<f64>,
}

impl Interval {
    fn secs(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }
}

fn intervals(entries: &[Entry]) -> Vec<Interval> {
    let mut out = Vec::new();
    for pair in entries.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let crashed = b.event == Event::Start && a.event != Event::Stop;
        let kind = if a.event == Event::Stop || crashed {
            Kind::Gap
        } else if a.event == Event::Suspend {
            Kind::Asleep
        } else if a.state.on_ac == Some(true) {
            Kind::Ac
        } else if a.state.screen_on == Some(false) {
            Kind::ScreenOff
        } else {
            Kind::ScreenOn
        };
        let wh_drop = match (a.state.wh, b.state.wh) {
            (Some(x), Some(y)) => Some(x - y),
            _ => None,
        };
        out.push(Interval {
            from: a.unix_time,
            to: b.unix_time,
            kind,
            wh_drop,
        });
    }
    out
}

/// Time and energy in one state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Share {
    pub secs: u64,
    pub wh: f64,
}

impl Share {
    fn add(&mut self, iv: &Interval) {
        self.secs += iv.secs();
        // A rise on battery is gauge noise or recalibration, not negative use.
        self.wh += iv.wh_drop.unwrap_or(0.0).max(0.0);
    }

    /// Average draw in watts, when there is enough to say.
    pub fn watts(&self) -> Option<f64> {
        (self.secs >= 60).then(|| self.wh * 3600.0 / self.secs as f64)
    }
}

/// One stretch on battery, from unplug to plug-in (or to now).
#[derive(Debug, Clone, PartialEq)]
pub struct Discharge {
    pub from: u64,
    pub to: u64,
    /// Still on battery at the end of the log.
    pub ongoing: bool,
    pub start_percent: Option<u64>,
    pub end_percent: Option<u64>,
    pub screen_on: Share,
    pub screen_off: Share,
    pub asleep: Share,
    /// Time the daemon was not running. Counted in neither time nor energy.
    pub unknown_secs: u64,
}

impl Discharge {
    pub fn total(&self) -> Share {
        Share {
            secs: self.screen_on.secs + self.screen_off.secs + self.asleep.secs,
            wh: self.screen_on.wh + self.screen_off.wh + self.asleep.wh,
        }
    }
}

/// Is this interval part of a discharge? Gaps are, unless the charge went up across
/// them: then the machine was plugged in while we were not watching, and what follows
/// is a new discharge.
fn on_battery(iv: &Interval) -> bool {
    match iv.kind {
        Kind::Ac => false,
        Kind::Gap => iv.wh_drop.is_none_or(|d| d > -0.5),
        _ => true,
    }
}

/// The most recent discharge, ongoing or finished.
///
/// `now` closes the last line's interval: pass the live state if it can be read, so an
/// ongoing discharge counts up to this moment rather than to the last 1 % step.
pub fn last_discharge(entries: &[Entry], now: Option<&Entry>) -> Option<Discharge> {
    let mut all: Vec<Entry> = entries.to_vec();
    if let Some(n) = now {
        if all.last().is_none_or(|l| n.unix_time >= l.unix_time) {
            all.push(n.clone());
        }
    }
    let ivs = intervals(&all);

    // Charger bounce: a short AC interval with battery either side is not a charge.
    let battery: Vec<bool> = (0..ivs.len())
        .map(|i| {
            on_battery(&ivs[i])
                || (ivs[i].secs() < BOUNCE_SECS
                    && i > 0
                    && i + 1 < ivs.len()
                    && on_battery(&ivs[i - 1])
                    && on_battery(&ivs[i + 1]))
        })
        .collect();

    let end = battery.iter().rposition(|b| *b)?;
    let mut start = end;
    while start > 0 && battery[start - 1] {
        start -= 1;
    }
    // Leading and trailing gaps say nothing about the discharge.
    let (mut s, mut e) = (start, end);
    while s <= e && ivs[s].kind == Kind::Gap {
        s += 1;
    }
    while e >= s && ivs[e].kind == Kind::Gap {
        if e == 0 {
            return None;
        }
        e -= 1;
    }
    if s > e {
        return None;
    }

    let mut d = Discharge {
        from: ivs[s].from,
        to: ivs[e].to,
        ongoing: e + 1 == ivs.len() && all.last().and_then(|l| l.state.on_ac) == Some(false),
        start_percent: all[s].state.percent,
        end_percent: all[e + 1].state.percent,
        screen_on: Share::default(),
        screen_off: Share::default(),
        asleep: Share::default(),
        unknown_secs: 0,
    };
    for iv in &ivs[s..=e] {
        match iv.kind {
            Kind::ScreenOn => d.screen_on.add(iv),
            Kind::ScreenOff => d.screen_off.add(iv),
            Kind::Asleep => d.asleep.add(iv),
            // A bounce counted as battery: its time is too short to matter.
            Kind::Ac => {}
            Kind::Gap => d.unknown_secs += iv.secs(),
        }
    }
    Some(d)
}

/// What the battery was last charged to: the percentage at the most recent unplug.
pub fn last_charged_to(entries: &[Entry]) -> Option<(u64, u64)> {
    entries.windows(2).rev().find_map(|w| {
        (w[0].state.on_ac == Some(true) && w[1].state.on_ac == Some(false))
            .then_some(())
            .and_then(|_| w[1].state.percent.or(w[0].state.percent))
            .map(|p| (w[1].unix_time, p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(on_ac: bool, screen: bool, pct: u64, wh: f64) -> BatteryState {
        BatteryState {
            on_ac: Some(on_ac),
            screen_on: Some(screen),
            status: Some(if on_ac { "Charging" } else { "Discharging" }.into()),
            percent: Some(pct),
            wh: Some(wh),
            wh_full: Some(70.0),
            cycles: Some(41),
        }
    }

    fn e(t: u64, ev: Event, s: BatteryState) -> Entry {
        Entry {
            unix_time: t,
            event: ev,
            state: s,
        }
    }

    #[test]
    fn a_line_round_trips() {
        let entry = e(1_700_000_000, Event::Percent, st(false, true, 61, 42.734));
        let text = format!("{HEADER}\n{}\n", entry.to_line());
        let back = parse(&text);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].state.wh, Some(42.73));
        assert_eq!(back[0].state.percent, Some(61));
        assert_eq!(back[0].event, Event::Percent);
    }

    #[test]
    fn columns_are_read_by_name_and_a_torn_line_is_skipped() {
        // A future version appends a column and reorders nothing; an old reader and a
        // half-written last line must both cope.
        let text = "unix_time,event,on_ac,screen_on,status,percent,wh,wh_full,cycles,extra\n\
                    100,start,0,1,Discharging,80,50.00,70.00,41,x\n\
                    160,perc";
        let back = parse(text);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].state.on_ac, Some(false));
    }

    #[test]
    fn the_tracker_writes_on_change_and_not_otherwise() {
        let mut t = Tracker::default();
        let s = st(false, true, 80, 50.0);
        assert_eq!(t.observe(0, &s), Some(Event::Start));
        assert_eq!(t.observe(1, &s), None);
        // Energy moving within a percent is not a line: only the step is.
        let mut s2 = s.clone();
        s2.wh = Some(49.9);
        assert_eq!(t.observe(2, &s2), None);
        s2.percent = Some(79);
        assert_eq!(t.observe(3, &s2), Some(Event::Percent));
        s2.screen_on = Some(false);
        assert_eq!(t.observe(4, &s2), Some(Event::Screen));
        s2.on_ac = Some(true);
        assert_eq!(t.observe(5, &s2), Some(Event::Ac));
    }

    #[test]
    fn heartbeat_only_on_battery_with_the_screen_on() {
        let mut t = Tracker::default();
        let on_batt = st(false, true, 80, 50.0);
        t.observe(0, &on_batt);
        assert_eq!(t.observe(HEARTBEAT_SECS - 1, &on_batt), None);
        assert_eq!(t.observe(HEARTBEAT_SECS, &on_batt), Some(Event::Heartbeat));

        let mut t = Tracker::default();
        let dark = st(false, false, 80, 50.0);
        t.observe(0, &dark);
        assert_eq!(t.observe(10 * HEARTBEAT_SECS, &dark), None);

        let mut t = Tracker::default();
        let mains = st(true, true, 80, 50.0);
        t.observe(0, &mains);
        assert_eq!(t.observe(10 * HEARTBEAT_SECS, &mains), None);
    }

    #[test]
    fn a_discharge_splits_screen_on_off_and_asleep() {
        let log = vec![
            e(0, Event::Start, st(true, true, 80, 56.0)),
            e(100, Event::Ac, st(false, true, 80, 56.0)),
            // 1 h screen on, 7 Wh -> 7 W
            e(3700, Event::Screen, st(false, false, 70, 49.0)),
            // 30 min screen off, 1 Wh -> 2 W
            e(5500, Event::Suspend, st(false, false, 69, 48.0)),
            // 2 h asleep, 0.6 Wh -> 0.3 W
            e(12700, Event::Resume, st(false, true, 68, 47.4)),
            e(12760, Event::Ac, st(true, true, 68, 47.4)),
        ];
        let d = last_discharge(&log, None).unwrap();
        assert!(!d.ongoing);
        assert_eq!((d.from, d.to), (100, 12760));
        assert_eq!(d.start_percent, Some(80));
        assert_eq!(d.end_percent, Some(68));
        assert_eq!(d.screen_on.secs, 3600 + 60);
        assert!((d.screen_on.watts().unwrap() - 7.0 * 3600.0 / 3660.0).abs() < 1e-9);
        assert!((d.screen_off.watts().unwrap() - 2.0).abs() < 1e-9);
        assert!((d.asleep.watts().unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(last_charged_to(&log), Some((100, 80)));
    }

    #[test]
    fn an_ongoing_discharge_runs_to_now() {
        let log = vec![
            e(0, Event::Start, st(true, true, 80, 56.0)),
            e(100, Event::Ac, st(false, true, 80, 56.0)),
            e(1900, Event::Percent, st(false, true, 79, 55.3)),
        ];
        let now = e(3700, Event::Heartbeat, st(false, true, 78, 54.5));
        let d = last_discharge(&log, Some(&now)).unwrap();
        assert!(d.ongoing);
        assert_eq!(d.to, 3700);
        assert_eq!(d.screen_on.secs, 3600);
        assert!((d.screen_on.wh - 1.5).abs() < 1e-9);
    }

    #[test]
    fn charger_bounce_does_not_end_a_discharge() {
        let log = vec![
            e(0, Event::Ac, st(false, true, 80, 56.0)),
            e(1000, Event::Ac, st(true, true, 78, 54.6)),
            e(1002, Event::Ac, st(false, true, 78, 54.6)),
            e(2000, Event::Percent, st(false, true, 77, 53.9)),
        ];
        let d = last_discharge(&log, None).unwrap();
        assert_eq!(d.from, 0);
        assert_eq!(d.start_percent, Some(80));
    }

    #[test]
    fn a_real_charge_ends_a_discharge() {
        let log = vec![
            e(0, Event::Ac, st(false, true, 80, 56.0)),
            e(1000, Event::Ac, st(true, true, 78, 54.6)),
            e(5000, Event::Ac, st(false, true, 90, 63.0)),
            e(6000, Event::Percent, st(false, true, 89, 62.3)),
        ];
        let d = last_discharge(&log, None).unwrap();
        assert_eq!(d.from, 5000);
        assert_eq!(last_charged_to(&log), Some((5000, 90)));
    }

    #[test]
    fn a_crash_is_a_gap_not_screen_on_time() {
        // No stop before the start: the daemon died. The time between is unknown and
        // its energy must not be charged to whatever state the last line showed.
        let log = vec![
            e(0, Event::Ac, st(false, true, 80, 56.0)),
            e(600, Event::Percent, st(false, true, 79, 55.3)),
            e(4200, Event::Start, st(false, true, 70, 49.0)),
            e(4800, Event::Percent, st(false, true, 69, 48.3)),
        ];
        let d = last_discharge(&log, None).unwrap();
        assert_eq!(d.unknown_secs, 3600);
        assert_eq!(d.screen_on.secs, 1200);
        assert!((d.screen_on.wh - 1.4).abs() < 1e-9);
    }

    #[test]
    fn charging_while_we_were_not_watching_starts_a_new_discharge() {
        let log = vec![
            e(0, Event::Ac, st(false, true, 50, 35.0)),
            e(600, Event::Stop, st(false, true, 49, 34.3)),
            e(9000, Event::Start, st(false, true, 80, 56.0)),
            e(9600, Event::Percent, st(false, true, 79, 55.3)),
        ];
        let d = last_discharge(&log, None).unwrap();
        assert_eq!(d.from, 9000);
        assert_eq!(d.start_percent, Some(80));
    }

    #[test]
    fn nothing_on_battery_is_none() {
        let log = vec![
            e(0, Event::Start, st(true, true, 80, 56.0)),
            e(100, Event::Percent, st(true, true, 81, 56.7)),
        ];
        assert_eq!(last_discharge(&log, None), None);
        assert_eq!(last_discharge(&[], None), None);
    }
}
