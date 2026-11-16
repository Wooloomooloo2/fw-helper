//! The Monitor page: watch the machine, record a run, and read runs back.
//!
//! Laid out like the Windows build's monitor window: a row of buttons along the top, a
//! status line, and the measurement cards two to a row beneath. Three things can be on
//! screen:
//!
//! - **Live** - a rolling five minutes, fed by the telemetry the rest of the window
//!   already receives, plus the frame rate read from MangoHud's log;
//! - **a saved run** - one in `~/Documents/FW Benchmark`, with its frame rate joined
//!   in, its notes, and a summary report;
//! - **an unsaved recording** - one the daemon still holds in its own directory, which
//!   it keeps to the last twenty. Saving it is what joins in the frame rate.
//!
//! Stopping a recording here saves it straight away and asks for run notes, as the
//! Windows build does. A recording stopped any other way - the CLI, the keyboard
//! shortcut, the 12-hour limit - is saved quietly in the background.
//!
//! Sessions are read from disk directly. A session is tens of thousands of rows and the
//! daemon writes them world-readable precisely so a client can open one rather than
//! drag it across the bus.

use crate::chart::{ChartStack, Sample};
use crate::worker::Command;
use adw::prelude::*;
use fw_helper_client::bench;
use fw_helper_client::{SessionInfo, Snapshot};
use fw_helper_core::notes::RunNotes;
use fw_helper_core::SessionMeta;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// How much of the recent past the live view keeps. Five minutes at 1 Hz — long enough
/// to watch a load settle past PL1's ~32 s averaging window several times over.
const LIVE_SAMPLES: usize = 300;

/// What the cards are showing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Viewing {
    Live,
    /// A recording in the daemon's directory, not yet saved: name and path.
    Unsaved(String, String),
    /// A recording in the benchmark folder.
    Saved(PathBuf),
}

pub struct MonitorPage {
    pub widget: gtk::Widget,
    chart: Rc<ChartStack>,
    live: RefCell<VecDeque<Sample>>,
    /// Ticks since the page started, which is the live view's time axis.
    elapsed: Cell<f64>,
    viewing: RefCell<Viewing>,

    status: gtk::Label,
    record_button: gtk::Button,
    open_list: gtk::ListBox,
    /// What each row of `open_list` opens.
    open_targets: RefCell<Vec<Viewing>>,
    live_button: gtk::Button,
    save_button: gtk::Button,
    notes_button: gtk::Button,
    summary_button: gtk::Button,
    delete_button: gtk::Button,
    summary: gtk::Label,

    commands: std::sync::mpsc::Sender<Command>,
    /// True when the daemon is reachable. Controls are built insensitive and only this
    /// turns them on, so a window with no daemon cannot look operable.
    connected: Cell<bool>,
    daemon_sessions: RefCell<Vec<SessionInfo>>,
    saved: RefCell<Vec<SessionMeta>>,
    /// The recording in progress at the last tick: label and path. A tick that finds
    /// none where there was one is a recording that has just stopped.
    recording: RefCell<Option<(String, String)>>,
    /// Set when the Stop button here was pressed, so the stop that follows asks for
    /// notes. Any other stop is saved without interrupting.
    stop_here: Cell<bool>,
    recording_secs: Cell<u64>,
    profile: RefCell<Option<String>>,
    show_battery: RefCell<Option<Box<dyn Fn()>>>,
}

/// The local offset from UTC at a moment, for the reports' clock times.
pub fn utc_offset(t: u64) -> i64 {
    glib::DateTime::from_unix_local(t as i64)
        .map(|d| d.utc_offset().as_seconds())
        .unwrap_or(0)
}

fn local_label(unix: u64) -> String {
    glib::DateTime::from_unix_local(unix as i64)
        .and_then(|d| d.format("%a %-d %b, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn button(label: &str, tip: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .tooltip_text(tip)
        .valign(gtk::Align::Center)
        .build()
}

impl MonitorPage {
    pub fn new(commands: std::sync::mpsc::Sender<Command>) -> Rc<Self> {
        bench::ensure_mangohud_dir();
        // Old frame logs are only worth keeping until a recording has used them.
        std::thread::spawn(bench::prune_mangohud);

        let chart = ChartStack::new();

        let record_button = button(
            "\u{25cf} Record",
            "Record a run: every reading, once a second",
        );
        record_button.add_css_class("suggested-action");
        record_button.set_sensitive(false);

        let open_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let open_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(460)
            .min_content_width(380)
            .child(&open_list)
            .build();
        let popover = gtk::Popover::builder().child(&open_scroll).build();
        let open_button = gtk::MenuButton::builder()
            .label("Open")
            .tooltip_text("Open a saved run, or a recording not yet saved")
            .popover(&popover)
            .valign(gtk::Align::Center)
            .build();

        let live_button = button("Live", "Back to the live view");
        let save_button = button(
            "Save",
            "Copy this recording to FW Benchmark, joining in the frame rate from MangoHud",
        );
        save_button.add_css_class("suggested-action");
        let notes_button = button("Notes", "Game settings and notes for this run");
        let summary_button = button("Summary", "Open this run's summary report in the browser");
        let delete_button = button("Delete", "Delete this run");
        delete_button.add_css_class("destructive-action");
        let compare_button = button("Compare\u{2026}", "Compare runs against a baseline");
        let battery_button = button("Battery", "Time on battery and where the charge went");

        // The status and Record share the first line - recording is the one action that
        // must never be wrapped out of sight. The rest wrap, so a narrow window grows a
        // row of buttons instead of a horizontal scrollbar.
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .css_classes(["title-4"])
            .label("Live \u{b7} last 5 minutes")
            .build();
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        top.append(&status);
        top.append(&record_button);
        let buttons = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(false)
            .max_children_per_line(10)
            .column_spacing(6)
            .row_spacing(6)
            .build();
        for b in [
            open_button.upcast_ref::<gtk::Widget>(),
            live_button.upcast_ref(),
            save_button.upcast_ref(),
            notes_button.upcast_ref(),
            summary_button.upcast_ref(),
            compare_button.upcast_ref(),
            battery_button.upcast_ref(),
            delete_button.upcast_ref(),
        ] {
            // A child that hides with its button, so a hidden one leaves no gap.
            let child = gtk::FlowBoxChild::builder()
                .child(b)
                .focusable(false)
                .build();
            b.bind_property("visible", &child, "visible")
                .sync_create()
                .build();
            buttons.append(&child);
        }
        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        bar.append(&top);
        bar.append(&buttons);

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
                "A recording keeps running after this window is closed. The frame rate comes \
                 from MangoHud: launch a game with MANGOHUD=1 MANGOHUD_CONFIGFILE=/usr/share/\
                 fw-helper/mangohud/fw-helper.conf %command% and it logs every frame for fw-helper \
                 to read. Saved runs, notes and reports are in ~/Documents/FW Benchmark.",
            )
            .build();
        let shortcut_row = crate::shortcut::row();

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        column.append(&bar);
        column.append(&summary);
        column.append(&chart.widget);
        column.append(&hint);
        column.append(&shortcut_row);

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
            status,
            record_button,
            open_list,
            open_targets: RefCell::new(Vec::new()),
            live_button,
            save_button,
            notes_button,
            summary_button,
            delete_button,
            summary,
            commands,
            connected: Cell::new(false),
            daemon_sessions: RefCell::new(Vec::new()),
            saved: RefCell::new(bench::saved_sessions()),
            recording: RefCell::new(None),
            stop_here: Cell::new(false),
            recording_secs: Cell::new(0),
            profile: RefCell::new(None),
            show_battery: RefCell::new(None),
        });

        page.connect_signals(&popover, &compare_button, &battery_button);
        page.sync_buttons();
        page
    }

    /// Show a saved run, as if picked from Open.
    #[allow(dead_code)]
    pub fn open_saved(self: &Rc<Self>, csv: PathBuf) {
        *self.saved.borrow_mut() = bench::saved_sessions();
        self.view(Viewing::Saved(csv));
    }

    /// A line describing the page's state, for the render example's checks.
    #[allow(dead_code)]
    pub fn debug_state(&self) -> String {
        let live = self.live.borrow();
        let last = live.back();
        format!(
            "status={:?} live_samples={} fps={:?} app={:?} pkg={:?} recording={:?} cards_visible={}",
            self.status.label(),
            live.len(),
            last.and_then(|s| s.fps),
            last.and_then(|s| s.fps_app.clone()),
            last.and_then(|s| s.package_w),
            self.recording.borrow(),
            self.chart.widget.is_visible(),
        )
    }

    /// What the Battery button does: switch the window to the Battery page.
    pub fn set_show_battery(&self, f: impl Fn() + 'static) {
        *self.show_battery.borrow_mut() = Some(Box::new(f));
    }

    fn connect_signals(
        self: &Rc<Self>,
        popover: &gtk::Popover,
        compare: &gtk::Button,
        battery: &gtk::Button,
    ) {
        let this = Rc::clone(self);
        popover.connect_show(move |_| this.rebuild_open_list());

        let this = Rc::clone(self);
        let pop = popover.clone();
        self.open_list.connect_row_activated(move |_, row| {
            let target = this
                .open_targets
                .borrow()
                .get(row.index() as usize)
                .cloned();
            pop.popdown();
            if let Some(t) = target {
                this.view(t);
            }
        });

        let this = Rc::clone(self);
        self.record_button.connect_clicked(move |_| {
            // One control for two actions, so what it does depends on what the daemon
            // says is happening rather than on any local flag.
            if this.recording.borrow().is_some() {
                this.stop_here.set(true);
                let _ = this.commands.send(Command::StopRecording);
            } else {
                this.ask_to_record();
            }
        });

        let this = Rc::clone(self);
        self.live_button
            .connect_clicked(move |_| this.view(Viewing::Live));

        let this = Rc::clone(self);
        self.save_button.connect_clicked(move |_| {
            if let Viewing::Unsaved(_, path) = this.viewing.borrow().clone() {
                this.save_recording(path, false);
            }
        });

        let this = Rc::clone(self);
        self.notes_button.connect_clicked(move |_| {
            if let Viewing::Saved(csv) = this.viewing.borrow().clone() {
                this.ask_for_notes(csv, false);
            }
        });

        let this = Rc::clone(self);
        self.summary_button.connect_clicked(move |_| {
            if let Viewing::Saved(csv) = this.viewing.borrow().clone() {
                this.open_summary(&csv);
            }
        });

        let this = Rc::clone(self);
        self.delete_button
            .connect_clicked(move |_| this.confirm_delete());

        let this = Rc::clone(self);
        compare.connect_clicked(move |_| {
            let first = match &*this.viewing.borrow() {
                Viewing::Saved(csv) => Some(csv.clone()),
                _ => None,
            };
            crate::compare::present(&this.widget, first);
        });

        let this = Rc::clone(self);
        battery.connect_clicked(move |_| {
            if let Some(f) = &*this.show_battery.borrow() {
                f();
            }
        });
    }

    /// One tick of telemetry.
    pub fn update(self: &Rc<Self>, s: &Snapshot) {
        self.connected.set(true);
        *self.profile.borrow_mut() = s.profile.clone();

        let t = self.elapsed.get() + 1.0;
        self.elapsed.set(t);
        let mut sample = Sample::from_snapshot(t, s);
        if let Some((fps, app)) = bench::live_fps() {
            sample.fps = Some(fps);
            sample.fps_app = Some(app);
        }
        {
            let mut live = self.live.borrow_mut();
            live.push_back(sample);
            while live.len() > LIVE_SAMPLES {
                live.pop_front();
            }
        }

        *self.daemon_sessions.borrow_mut() = s.sessions.clone();
        self.sync_recording(s);

        if *self.viewing.borrow() == Viewing::Live {
            self.push_live();
        } else if let Viewing::Unsaved(name, _) = &*self.viewing.borrow() {
            // Pruned or deleted by someone else.
            if !s.sessions.iter().any(|x| &x.name == name) {
                drop(self.viewing.replace(Viewing::Live));
            }
        }
        self.sync_status();
        self.sync_buttons();
    }

    /// No daemon: stop pretending the controls do anything. Saved runs still open -
    /// they are files in the user's home and need no daemon.
    pub fn set_disconnected(&self) {
        self.connected.set(false);
        self.daemon_sessions.borrow_mut().clear();
        self.sync_buttons();
        self.status
            .set_label("No daemon \u{2014} nothing is being recorded");
        self.chart
            .set_placeholder("no connection to fw-helperd \u{2014} nothing to plot live");
    }

    fn push_live(&self) {
        let samples: Vec<Sample> = self.live.borrow().iter().cloned().collect();
        self.chart.set_samples(samples);
        self.summary.set_label("");
    }

    fn sync_recording(self: &Rc<Self>, s: &Snapshot) {
        let now = s
            .recording
            .as_ref()
            .map(|r| (r.label.clone(), r.path.clone()));
        self.recording_secs
            .set(s.recording.as_ref().map(|r| r.samples).unwrap_or(0));
        let before = self.recording.replace(now.clone());
        if let (Some((_, path)), None) = (before, &now) {
            // It has just stopped: save it, asking for notes only if the stop came from
            // this window's button.
            let ask = self.stop_here.replace(false);
            self.save_recording(path, ask);
        }
        match &s.recording {
            Some(_) => {
                self.record_button.set_label("\u{25a0} Stop");
                self.record_button.remove_css_class("suggested-action");
                self.record_button.add_css_class("destructive-action");
            }
            None => {
                self.record_button.set_label("\u{25cf} Record");
                self.record_button.remove_css_class("destructive-action");
                self.record_button.add_css_class("suggested-action");
            }
        }
    }

    fn sync_status(&self) {
        let recording = self.recording.borrow().clone();
        let rec_text = recording.as_ref().map(|(label, _)| {
            let secs = self.recording_secs.get();
            format!(
                "\u{25cf} Recording \u{201c}{label}\u{201d} {}:{:02}",
                secs / 60,
                secs % 60
            )
        });
        let text = match &*self.viewing.borrow() {
            Viewing::Live => match rec_text {
                Some(r) => format!("Live \u{b7} {r}"),
                None => "Live \u{b7} last 5 minutes".to_string(),
            },
            Viewing::Saved(csv) => {
                let meta = self.saved.borrow().iter().find(|m| &m.path == csv).cloned();
                let base = meta
                    .map(|m| format!("{} \u{b7} {}", m.label, local_label(m.started_unix)))
                    .unwrap_or_else(|| csv.display().to_string());
                match rec_text {
                    Some(r) => format!("{base} \u{b7} {r}"),
                    None => base,
                }
            }
            Viewing::Unsaved(name, _) => {
                let info = self
                    .daemon_sessions
                    .borrow()
                    .iter()
                    .find(|x| &x.name == name)
                    .map(|x| format!("{} \u{b7} {}", x.label, local_label(x.started_unix)));
                format!(
                    "{} \u{b7} not saved yet",
                    info.unwrap_or_else(|| name.clone())
                )
            }
        };
        self.status.set_label(&text);
    }

    fn sync_buttons(&self) {
        let connected = self.connected.get();
        let viewing = self.viewing.borrow().clone();
        self.record_button.set_sensitive(connected);
        self.live_button.set_visible(viewing != Viewing::Live);
        self.save_button
            .set_visible(matches!(viewing, Viewing::Unsaved(..)));
        let saved = matches!(viewing, Viewing::Saved(_));
        self.notes_button.set_visible(saved);
        self.summary_button.set_visible(saved);
        self.delete_button.set_visible(viewing != Viewing::Live);
        self.delete_button
            .set_sensitive(saved || (connected && matches!(viewing, Viewing::Unsaved(..))));
    }

    fn view(self: &Rc<Self>, target: Viewing) {
        *self.viewing.borrow_mut() = target.clone();
        match target {
            Viewing::Live => self.push_live(),
            Viewing::Unsaved(name, path) => self.load(Path::new(&path), &name),
            Viewing::Saved(csv) => {
                let name = csv
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                self.load(&csv, &name);
            }
        }
        self.sync_status();
        self.sync_buttons();
    }

    fn load(&self, path: &Path, name: &str) {
        match fw_helper_core::Session::read(path) {
            Ok(session) => {
                let samples: Vec<Sample> = session.rows.iter().map(Sample::from_row).collect();
                self.summary.set_label(&summarise(&session));
                self.chart.set_samples(samples);
            }
            Err(e) => {
                self.summary.set_label("");
                self.chart
                    .set_placeholder(&format!("cannot read {name}: {e}"));
                self.chart.set_samples(Vec::new());
            }
        }
    }

    /// The Open popover: saved runs first, then the daemon's unsaved recordings.
    fn rebuild_open_list(self: &Rc<Self>) {
        *self.saved.borrow_mut() = bench::saved_sessions();
        while let Some(child) = self.open_list.first_child() {
            self.open_list.remove(&child);
        }
        let mut targets = Vec::new();

        let saved = self.saved.borrow().clone();
        let saved_names: Vec<String> = saved.iter().map(|m| m.name.clone()).collect();
        for m in &saved {
            let notes = bench::load_notes(&m.path);
            let mut sub = vec![local_label(m.started_unix)];
            if bench::frames_path(&m.path).exists() {
                sub.push("frame times".into());
            }
            if let Some(g) = notes.and_then(|n| n.game_settings) {
                sub.push(g);
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&m.label).as_str())
                .subtitle(glib::markup_escape_text(&sub.join(" \u{b7} ")).as_str())
                .activatable(true)
                .build();
            self.open_list.append(&row);
            targets.push(Viewing::Saved(m.path.clone()));
        }

        for x in self.daemon_sessions.borrow().iter() {
            if saved_names.contains(&x.name) {
                continue;
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&x.label).as_str())
                .subtitle(
                    glib::markup_escape_text(&format!(
                        "{} \u{b7} {} \u{b7} not saved",
                        local_label(x.started_unix),
                        human_bytes(x.bytes)
                    ))
                    .as_str(),
                )
                .activatable(true)
                .build();
            self.open_list.append(&row);
            targets.push(Viewing::Unsaved(x.name.clone(), x.path.clone()));
        }

        if targets.is_empty() {
            let row = adw::ActionRow::builder()
                .title("Nothing recorded yet")
                .subtitle("Press Record, play, then Stop")
                .activatable(false)
                .build();
            self.open_list.append(&row);
        }
        *self.open_targets.borrow_mut() = targets;
    }

    fn window(&self) -> Option<gtk::Window> {
        self.widget.root().and_downcast::<gtk::Window>()
    }

    /// Ask for a name, then record. The suggestion is the game running now and the
    /// profile, since that is what a run is usually told apart by.
    fn ask_to_record(self: &Rc<Self>) {
        let game = self.live.borrow().back().and_then(|s| s.fps_app.clone());
        let suggestion = match (game, self.profile.borrow().clone()) {
            (Some(g), Some(p)) => format!("{g} {p}"),
            (Some(g), None) => g,
            (None, _) => String::new(),
        };
        let entry = gtk::Entry::builder()
            .text(&suggestion)
            .placeholder_text("e.g. game and settings")
            .activates_default(true)
            .max_length(60)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading("Record")
            .body("Name (optional) - it becomes the file name, and the run's title in reports.")
            .extra_child(&entry)
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("record", "Record")]);
        dialog.set_response_appearance("record", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("record"));
        dialog.set_close_response("cancel");
        let this = Rc::clone(self);
        let typed = entry.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "record" {
                let label = typed.text().trim().to_string();
                let label = if label.is_empty() {
                    "session".into()
                } else {
                    label
                };
                let _ = this.commands.send(Command::StartRecording(label));
            }
        });
        dialog.present(Some(&self.widget));
        entry.grab_focus();
    }

    /// Export a daemon recording into the benchmark folder, off the main thread - it
    /// reads every MangoHud log the recording overlaps, which can be tens of MB.
    fn save_recording(self: &Rc<Self>, path: String, ask_notes: bool) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let p = PathBuf::from(&path);
            let result = gio::spawn_blocking(move || bench::export(&p).map_err(|e| e.to_string()))
                .await
                .unwrap_or_else(|_| Err("the export thread panicked".into()));
            *this.saved.borrow_mut() = bench::saved_sessions();
            match result {
                Ok(out) => {
                    let viewing_it =
                        matches!(&*this.viewing.borrow(), Viewing::Unsaved(_, p) if *p == path);
                    if ask_notes || viewing_it {
                        this.view(Viewing::Saved(out.csv.clone()));
                    }
                    if ask_notes {
                        this.ask_for_notes(out.csv, true);
                    }
                }
                Err(e) => {
                    this.summary
                        .set_label(&format!("Could not save the recording: {e}"));
                }
            }
        });
    }

    /// Run notes: what a recording cannot measure. `offer_summary` makes the buttons
    /// open the report afterwards, which is what a just-stopped run wants.
    pub(crate) fn ask_for_notes(self: &Rc<Self>, csv: PathBuf, offer_summary: bool) {
        let notes = bench::load_notes(&csv).unwrap_or_default();
        let recorded: Vec<String> = [
            notes.bios.as_ref().map(|b| format!("BIOS {b}")),
            notes.ec.as_ref().map(|e| format!("EC {e}")),
            notes.fan_curve.as_ref().map(|f| format!("fan {f}")),
        ]
        .into_iter()
        .flatten()
        .collect();

        let settings = gtk::Entry::builder()
            .text(notes.game_settings.as_deref().unwrap_or_default())
            .placeholder_text("e.g. 1080p, Medium, XeSS Balanced, frame generation off")
            .max_length(120)
            .build();
        let text = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(6)
            .right_margin(6)
            .build();
        text.buffer()
            .set_text(notes.notes.as_deref().unwrap_or_default());
        let text_scroll = gtk::ScrolledWindow::builder()
            .min_content_height(90)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&text)
            .css_classes(["card"])
            .build();
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        for (label, w) in [
            ("Game settings", settings.upcast_ref::<gtk::Widget>()),
            ("Notes", text_scroll.upcast_ref::<gtk::Widget>()),
        ] {
            body.append(
                &gtk::Label::builder()
                    .label(label)
                    .xalign(0.0)
                    .css_classes(["heading"])
                    .build(),
            );
            body.append(w);
        }

        let name = csv
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let dialog = adw::AlertDialog::builder()
            .heading("Run notes")
            .body(if recorded.is_empty() {
                format!("{name}\nOnly what you type is saved.")
            } else {
                format!(
                    "{name}\nRecorded automatically: {}",
                    recorded.join(" \u{b7} ")
                )
            })
            .extra_child(&body)
            .build();
        if offer_summary {
            dialog.add_responses(&[("skip", "Summary Only"), ("save", "Save and Open Summary")]);
        } else {
            dialog.add_responses(&[("skip", "Cancel"), ("save", "Save")]);
        }
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("skip");
        let this = Rc::clone(self);
        dialog.connect_response(None, move |_, response| {
            if response == "save" {
                let buffer = text.buffer();
                let typed = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                let mut n = bench::load_notes(&csv).unwrap_or_default();
                n.game_settings = RunNotes::clean(&settings.text());
                n.notes = RunNotes::clean(&typed);
                if let Err(e) = bench::save_notes(&csv, &n) {
                    this.summary
                        .set_label(&format!("Could not save the notes: {e}"));
                    return;
                }
            }
            if offer_summary {
                this.open_summary(&csv);
            }
        });
        dialog.present(Some(&self.widget));
    }

    fn open_summary(&self, csv: &Path) {
        match bench::write_summary(csv, &utc_offset) {
            Ok(html) => open_file(&html, self.window().as_ref()),
            Err(e) => self
                .summary
                .set_label(&format!("Could not make the summary: {e}")),
        }
    }

    fn confirm_delete(self: &Rc<Self>) {
        let viewing = self.viewing.borrow().clone();
        let (what, detail) = match &viewing {
            Viewing::Live => return,
            Viewing::Saved(csv) => (
                csv.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string(),
                "Its frame times, notes and summary go with it. The daemon's own copy, if it still has one, is kept.",
            ),
            Viewing::Unsaved(name, _) => (name.clone(), "It was never saved, so this is the only copy."),
        };
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Delete {what}?"))
            .body(detail)
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let this = Rc::clone(self);
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            match &viewing {
                Viewing::Saved(csv) => {
                    if let Err(e) = bench::delete_saved(csv) {
                        this.summary.set_label(&format!("Could not delete it: {e}"));
                        return;
                    }
                    *this.saved.borrow_mut() = bench::saved_sessions();
                }
                Viewing::Unsaved(name, _) => {
                    let _ = this.commands.send(Command::DeleteSession(name.clone()));
                }
                Viewing::Live => {}
            }
            this.view(Viewing::Live);
        });
        dialog.present(Some(&self.widget));
    }
}

/// Open a file in its default application - for a report, the browser.
pub fn open_file(path: &Path, parent: Option<&gtk::Window>) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    launcher.launch(parent, gio::Cancellable::NONE, |_| {});
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

    let fps: Vec<f64> = rows.iter().filter_map(|r| r.fps).collect();
    if !fps.is_empty() {
        let mut sorted = fps.clone();
        sorted.sort_by(f64::total_cmp);
        parts.push(format!(
            "{:.0} fps median over {} s with frames",
            fw_helper_core::summary::percentile(&sorted, 50.0),
            fps.len()
        ));
    }
    if let (Some(m), Some(p)) = (mean(|r| r.package_w), peak(|r| r.package_w)) {
        parts.push(format!("power {m:.1} W mean, {p:.1} W peak"));
    }
    if let Some(limit) = rows.iter().rev().find_map(|r| r.pl1_w) {
        // The inference the power card exists to support, stated in words as well:
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
        (0, g) => format!("gpu limit reason asserted for {g}s"),
        (c, g) => format!("{c} cpu throttle events, gpu limit reason asserted for {g}s"),
    });

    parts.join("   \u{00b7}   ")
}
