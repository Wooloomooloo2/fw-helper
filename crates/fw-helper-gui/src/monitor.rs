//! The Monitor page: record a session, and read one back.
//!
//! Two views over the same stack of measurement cards. **Live** is a rolling window
//! fed by the telemetry the rest of the window already receives. **Session** loads a
//! recorded CSV from disk and shows the whole run.
//!
//! Reading the file directly is deliberate. A session is tens of thousands of rows and
//! the daemon writes them world-readable precisely so a client can open one rather than
//! drag it across the bus; the daemon only supplies the list and the paths.

use crate::chart::{ChartStack, Sample};
use crate::worker::Command;
use adw::prelude::*;
use fw_helper_client::Snapshot;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// How much of the recent past the live view keeps. Five minutes at 1 Hz — long enough
/// to watch a load settle past PL1's ~32 s averaging window several times over, short
/// enough to stay legible at this width.
const LIVE_SAMPLES: usize = 300;

/// What the chart is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Viewing {
    Live,
    Session(String),
}

pub struct MonitorPage {
    pub widget: gtk::Widget,
    chart: Rc<ChartStack>,
    live: RefCell<VecDeque<Sample>>,
    /// Ticks since the page started, which is the live view's time axis.
    elapsed: Cell<f64>,
    viewing: RefCell<Viewing>,

    source_row: adw::ComboRow,
    /// Session name per row of `source_row`; the first is empty and means live.
    source_names: RefCell<Vec<String>>,
    /// Name to file path, as the daemon reports them. Taken from the daemon rather than
    /// assumed, so a session recorded by a development-mode daemon - which writes under
    /// $XDG_RUNTIME_DIR, not /var/lib - still opens.
    known_paths: RefCell<Vec<(String, String)>>,
    record_row: adw::EntryRow,
    record_button: gtk::Button,
    record_status: gtk::Label,
    delete_button: gtk::Button,
    summary: gtk::Label,

    commands: std::sync::mpsc::Sender<Command>,
    /// Set while a control is being written from telemetry, so its own `changed`
    /// signal is not mistaken for the user operating it.
    settling: Cell<bool>,
    /// True when the daemon is reachable. Controls are built insensitive and only this
    /// turns them on, so a window with no daemon cannot look operable.
    connected: Cell<bool>,
}

impl MonitorPage {
    pub fn new(commands: std::sync::mpsc::Sender<Command>) -> Rc<Self> {
        let chart = ChartStack::new();

        let source_row = adw::ComboRow::builder()
            .title("Showing")
            .subtitle("live, or a recorded session")
            .model(&gtk::StringList::new(&["Live (last 5 minutes)"]))
            .sensitive(false)
            .build();

        let record_row = adw::EntryRow::builder()
            .title("Session name")
            .sensitive(false)
            .build();
        let record_button = gtk::Button::builder()
            .label("Record")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        record_button.add_css_class("suggested-action");
        record_row.add_suffix(&record_button);

        let record_status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["stat-label"])
            .label("not recording")
            .build();

        let delete_button = gtk::Button::builder()
            .label("Delete")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        delete_button.add_css_class("destructive-action");
        source_row.add_suffix(&delete_button);

        let summary = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["stat-label"])
            .build();

        // Says plainly what this page cannot do, rather than leaving someone to
        // discover it by alt-tabbing out of a fullscreen game and finding nothing.
        let hint = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["stat-label"])
            .label(
                "A recording keeps running after this window is closed. For numbers \
                 inside a fullscreen game, see the MangoHud section of the README \u{2014} \
                 no ordinary window can draw over one on Wayland.",
            )
            .build();

        // One group, whose rows are the measurement cards. The summary sits under
        // them because it is a sentence about all five, not about any one.
        let chart_group = adw::PreferencesGroup::builder()
            .title("Machine")
            .description("each measurement in its own strip, against the limit it is read against")
            .build();
        let chart_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .build();
        chart_box.append(&chart.widget);
        chart_box.append(&summary);
        chart_group.add(&chart_box);

        let controls = adw::PreferencesGroup::builder().title("Recording").build();
        controls.add(&source_row);
        controls.add(&record_row);

        let status_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        status_box.append(&record_status);
        status_box.append(&hint);
        controls.add(&status_box);

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        column.append(&chart_group);
        column.append(&controls);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&column)
            .build();

        let page = Rc::new(Self {
            widget: scroll.upcast(),
            chart,
            live: RefCell::new(VecDeque::with_capacity(LIVE_SAMPLES)),
            elapsed: Cell::new(0.0),
            viewing: RefCell::new(Viewing::Live),
            source_row,
            source_names: RefCell::new(vec![String::new()]),
            known_paths: RefCell::new(Vec::new()),
            record_row,
            record_button,
            record_status,
            delete_button,
            summary,
            commands,
            settling: Cell::new(false),
            connected: Cell::new(false),
        });

        page.connect_signals();
        page
    }

    fn connect_signals(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.source_row.connect_selected_notify(move |row| {
            if this.settling.get() {
                return;
            }
            let index = row.selected() as usize;
            let names = this.source_names.borrow();
            let chosen = match names.get(index) {
                Some(name) if name.is_empty() => Viewing::Live,
                Some(name) => Viewing::Session(name.clone()),
                None => Viewing::Live,
            };
            drop(names);
            *this.viewing.borrow_mut() = chosen;
            this.refresh_view();
        });

        let this = Rc::clone(self);
        self.record_button.connect_clicked(move |_| {
            // The button is one control for two actions, so what it does depends on
            // what the daemon says is happening rather than on any local flag.
            if this.record_status.text().starts_with("recording") {
                let _ = this.commands.send(Command::StopRecording);
            } else {
                let label = this.record_row.text().to_string();
                let label = if label.trim().is_empty() {
                    "session".to_string()
                } else {
                    label
                };
                let _ = this.commands.send(Command::StartRecording(label));
            }
        });

        let this = Rc::clone(self);
        self.delete_button.connect_clicked(move |_| {
            let Viewing::Session(name) = this.viewing.borrow().clone() else {
                return;
            };
            let _ = this.commands.send(Command::DeleteSession(name));
            // Back to live: whatever was on screen is about to stop existing.
            *this.viewing.borrow_mut() = Viewing::Live;
            this.refresh_view();
        });
    }

    /// One tick of telemetry.
    pub fn update(self: &Rc<Self>, s: &Snapshot) {
        self.connected.set(true);

        let t = self.elapsed.get() + 1.0;
        self.elapsed.set(t);
        {
            let mut live = self.live.borrow_mut();
            live.push_back(Sample::from_snapshot(t, s));
            while live.len() > LIVE_SAMPLES {
                live.pop_front();
            }
        }

        self.sync_sessions(s);
        self.sync_recording(s);

        if *self.viewing.borrow() == Viewing::Live {
            self.push_live();
        }
    }

    /// No daemon: stop pretending the controls do anything.
    pub fn set_disconnected(&self) {
        self.connected.set(false);
        self.source_row.set_sensitive(false);
        self.record_row.set_sensitive(false);
        self.record_button.set_sensitive(false);
        self.delete_button.set_sensitive(false);
        self.record_status
            .set_label("no daemon; nothing is being recorded");
        self.chart
            .set_placeholder("no connection to fw-helperd \u{2014} nothing to plot");
    }

    fn push_live(&self) {
        let samples: Vec<Sample> = self.live.borrow().iter().cloned().collect();
        self.chart.set_samples(samples);
        self.summary.set_label("");
    }

    /// Rebuild the source list when the set of sessions has actually changed.
    ///
    /// Only on a change: replacing the model resets the selection, so doing it every
    /// tick would snap the view back to live once a second.
    fn sync_sessions(self: &Rc<Self>, s: &Snapshot) {
        *self.known_paths.borrow_mut() = s
            .sessions
            .iter()
            .map(|x| (x.name.clone(), x.path.clone()))
            .collect();

        let mut wanted: Vec<String> = vec![String::new()];
        wanted.extend(s.sessions.iter().map(|x| x.name.clone()));
        if *self.source_names.borrow() == wanted {
            self.sync_sensitivity();
            return;
        }

        let mut labels: Vec<String> = vec!["Live (last 5 minutes)".to_string()];
        for session in &s.sessions {
            labels.push(format!(
                "{}  \u{00b7}  {}",
                session.label,
                human_bytes(session.bytes)
            ));
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();

        // Hold the selection across the rebuild where the session still exists, so a
        // new recording appearing does not yank the view away from what is being read.
        let current = self.viewing.borrow().clone();
        let index = match &current {
            Viewing::Live => 0,
            Viewing::Session(name) => wanted.iter().position(|n| n == name).unwrap_or(0),
        };
        if index == 0 && current != Viewing::Live {
            // It has been deleted or pruned out from under us.
            *self.viewing.borrow_mut() = Viewing::Live;
            self.push_live();
        }

        *self.source_names.borrow_mut() = wanted;
        self.settling.set(true);
        self.source_row
            .set_model(Some(&gtk::StringList::new(&refs)));
        self.source_row.set_selected(index as u32);
        self.settling.set(false);
        self.sync_sensitivity();
    }

    fn sync_sensitivity(&self) {
        let connected = self.connected.get();
        self.source_row.set_sensitive(connected);
        self.record_row.set_sensitive(connected);
        self.record_button.set_sensitive(connected);
        self.delete_button
            .set_sensitive(connected && matches!(*self.viewing.borrow(), Viewing::Session(_)));
    }

    fn sync_recording(&self, s: &Snapshot) {
        match &s.recording {
            Some(r) => {
                let secs = r.samples;
                self.record_status.set_label(&format!(
                    "recording {:?} \u{2014} {} sample{} ({}:{:02})",
                    r.label,
                    secs,
                    if secs == 1 { "" } else { "s" },
                    secs / 60,
                    secs % 60
                ));
                self.record_button.set_label("Stop");
                self.record_button.remove_css_class("suggested-action");
                self.record_button.add_css_class("destructive-action");
                self.record_row.set_sensitive(false);
            }
            None => {
                self.record_status.set_label("not recording");
                self.record_button.set_label("Record");
                self.record_button.remove_css_class("destructive-action");
                self.record_button.add_css_class("suggested-action");
                self.record_row.set_sensitive(self.connected.get());
            }
        }
    }

    /// Load whatever the source row now points at.
    fn refresh_view(self: &Rc<Self>) {
        self.sync_sensitivity();
        let viewing = self.viewing.borrow().clone();
        match viewing {
            Viewing::Live => self.push_live(),
            Viewing::Session(name) => self.load_session(&name),
        }
    }

    fn load_session(&self, name: &str) {
        let Some(path) = self.session_path(name) else {
            self.summary.set_label("");
            self.chart.set_samples(Vec::new());
            self.chart
                .set_placeholder(&format!("the daemon no longer lists {name}"));
            return;
        };

        match fw_helper_core::Session::read(&path) {
            Ok(session) => {
                let samples: Vec<Sample> = session.rows.iter().map(Sample::from_row).collect();
                self.summary.set_label(&summarise(&session));
                self.chart.set_samples(samples);
            }
            Err(e) => {
                self.summary.set_label("");
                self.chart
                    .set_placeholder(&format!("cannot read {}: {e}", path.display()));
                self.chart.set_samples(Vec::new());
            }
        }
    }

    fn session_path(&self, name: &str) -> Option<std::path::PathBuf> {
        self.known_paths
            .borrow()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| std::path::PathBuf::from(p))
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// The numbers worth having in words under a recorded run.
///
/// Peaks rather than means for temperature and fan, because what a session is opened to
/// answer is "how bad did it get". Power carries both: the mean is the sustained figure
/// a limit governs, and it is the one that answers whether PL1 was binding.
fn summarise(session: &fw_helper_core::Session) -> String {
    let rows = &session.rows;
    if rows.is_empty() {
        return "no samples".into();
    }

    let mean = |f: fn(&fw_helper_core::Row) -> Option<f64>| {
        let values: Vec<f64> = rows.iter().filter_map(f).collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let peak = |f: fn(&fw_helper_core::Row) -> Option<f64>| {
        rows.iter().filter_map(f).max_by(f64::total_cmp)
    };

    let mut parts: Vec<String> = Vec::new();
    let secs = session.duration_secs();
    parts.push(format!("{}m{:02}s", secs / 60, secs % 60));

    if let (Some(m), Some(p)) = (mean(|r| r.package_w), peak(|r| r.package_w)) {
        parts.push(format!("power {m:.1} W mean, {p:.1} W peak"));
    }
    if let Some(limit) = rows.iter().rev().find_map(|r| r.pl1_w) {
        // The inference the whole strip exists to support, stated in words as well:
        // sustained draw sitting on the setpoint is what PL1 binding looks like.
        match mean(|r| r.package_w) {
            Some(m) if m >= f64::from(limit) * 0.95 => {
                parts.push(format!("PL1 {limit} W was the binding constraint"));
            }
            Some(_) => parts.push(format!("PL1 {limit} W, not reached")),
            None => {}
        }
    }
    if let Some(p) = peak(|r| r.coretemp_c.or(r.peci_c)) {
        parts.push(format!("cpu peak {p:.1} \u{b0}C"));
    }
    if let Some(p) = peak(|r| r.battery_c) {
        parts.push(format!("battery peak {p:.1} \u{b0}C"));
    }
    if let Some(p) = peak(|r| r.fan_rpm.map(|v| v as f64)) {
        parts.push(format!("fan peak {p:.0} rpm"));
    }

    let throttle: u64 = rows.iter().map(|r| r.throttle_events).sum();
    let gpu_throttle = rows.iter().filter(|r| r.gpu_throttle.is_some()).count();
    parts.push(match (throttle, gpu_throttle) {
        (0, 0) => "no throttling".to_string(),
        (c, 0) => format!("{c} cpu throttle events"),
        (0, g) => format!("gpu throttled for {g}s"),
        (c, g) => format!("{c} cpu throttle events, gpu throttled for {g}s"),
    });

    parts.join("   \u{00b7}   ")
}
