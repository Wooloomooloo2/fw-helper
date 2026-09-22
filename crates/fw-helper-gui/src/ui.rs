//! Window construction and updates.

use crate::worker::{self, Command, Update};
use adw::prelude::*;
use fw_helper_client::Snapshot;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const CSS: &str = "
.stat-value  { font-size: 2.1rem; font-weight: 300; }
.stat-label  { font-size: 0.85rem; opacity: 0.6; }
.stat-card   { padding: 14px 18px; border-radius: 12px; }
.sensor-crit { opacity: 0.5; font-size: 0.8rem; }
/* One measurement per card on the Monitor page. The horizontal padding is shared with
   .chart-axis so the time ticks under the column line up with the plots above it. */
.chart-card  { padding: 8px 8px 6px 8px; }
.chart-axis  { padding: 0 8px; }
/* The overlay's numbers: tabular figures so a changing value does not shuffle the
   column width every second, which is what makes a 1 Hz readout feel unsettled. */
.overlay-value { font-size: 1.1rem; font-feature-settings: \"tnum\"; }
";

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Widgets that get updated on every tick, held so we mutate rather than rebuild.
/// How long a transient banner stays up before hiding itself.
///
/// Long enough to read a sentence you were not looking at, short enough that it is gone
/// before it becomes furniture. Only *transient* banners expire: a disconnected daemon
/// is a standing condition, and its banner stays until the condition does.
const BANNER_SECONDS: u32 = 10;

struct Widgets {
    title: adw::WindowTitle,
    banner: adw::Banner,
    /// Bumped every time the banner is written to.
    ///
    /// A timeout captures the value current when it was scheduled and hides the banner
    /// only if it still matches, so a newer message is never cut short by an older
    /// message's timer. Cheaper and safer than storing a `SourceId` and removing it:
    /// removing a source that has already fired is a warning at best.
    banner_generation: Rc<Cell<u64>>,
    power: gtk::Label,
    fan: gtk::Label,
    cpu_temp: gtk::Label,
    system: gtk::Label,
    system_caption: gtk::Label,
    cpu_load: gtk::Label,
    /// Captions carry the clock, and for the GPU the throttle reason when one is
    /// asserted — the number alone does not say whether the silicon is being held back.
    cpu_load_caption: gtk::Label,
    gpu_load: gtk::Label,
    gpu_load_caption: gtk::Label,
    profile: gtk::Label,
    battery: adw::ActionRow,
    sensors_group: adw::PreferencesGroup,
    /// Sensor rows are created on first sight and updated thereafter — rebuilding
    /// the list every second would make it flicker and lose scroll position.
    sensor_rows: HashMap<String, (adw::ActionRow, gtk::LevelBar)>,
    caps_group: adw::PreferencesGroup,
    cap_rows: HashMap<String, adw::ActionRow>,
    /// The groups holding every control, kept so they can be switched off wholesale
    /// when there is no daemon to receive what they would send. Per-row sensitivity is
    /// decided by `sync_controls` from a snapshot, which by definition cannot run while
    /// disconnected - so without this the controls keep whatever state they were built
    /// with, and a cold start with no daemon shows a fully live-looking window.
    system_group: adw::PreferencesGroup,
    save_group: adw::PreferencesGroup,
    auto_group: adw::PreferencesGroup,
    tuning_group: adw::PreferencesGroup,
    curve: Rc<crate::curve::CurveEditor>,
    monitor: Rc<crate::monitor::MonitorPage>,
    // Controls. Each is refreshed from telemetry, which means every update would
    // otherwise look like the user operating it — see `settling`.
    profile_row: adw::ComboRow,
    profile_names: Vec<String>,
    charge_row: adw::SpinRow,
    power_row: adw::SpinRow,
    fan_row: adw::ActionRow,
    fan_auto: gtk::Button,
    auto_ac: adw::ComboRow,
    auto_batt: adw::ComboRow,
    park_row: adw::ComboRow,
    /// Park levels in the order the combo lists them, so a selection maps back to a
    /// name without the UI hardcoding an index.
    park_levels: Vec<&'static str>,
    gpu_freq_row: adw::SpinRow,
    /// The GT's own ceiling (`rp0_freq`), kept so "no cap" can be recognised: the
    /// daemon reports the window, not a flag, and max == rp0 is what uncapped means.
    gpu_rp0: u32,
    save_entry: adw::EntryRow,
    delete_button: gtk::Button,
    saved_profiles: Vec<String>,
    /// Pending debounced sends, so a control that is still being adjusted issues one
    /// command rather than one per step.
    pending: HashMap<&'static str, glib::SourceId>,
    /// Controls whose new value has not been seen coming back yet, with what we are
    /// waiting for and since when.
    ///
    /// Held until **telemetry confirms the value**, not merely until the command
    /// returns. The snapshot in flight when a command completes was fetched before it,
    /// so releasing on the result alone writes the old value back for one tick — which
    /// is exactly the "it changed and then jumped back" people report. The deadline
    /// stops a change the daemon quietly declined from freezing the control forever.
    in_flight: HashMap<&'static str, (String, std::time::Instant)>,
}

pub fn build(app: &adw::Application) {
    let title = adw::WindowTitle::new("fw-helper", "connecting…");
    let header = adw::HeaderBar::builder().title_widget(&title).build();

    let banner = adw::Banner::builder().revealed(false).build();
    let banner_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    // The numbers worth seeing without scrolling: what the CPU is drawing, how hot it
    // is, how hard the fan is working, what the whole machine costs, and how busy the
    // two processors actually are. Draw without load is ambiguous — 15 W could be a
    // capped machine working hard or an idle one doing nothing.
    let power = stat_label();
    let fan = stat_label();
    let cpu_temp = stat_label();
    let system = stat_label();
    let (system_card, system_caption) = stat_card_with_caption(&system, "system");
    let cpu_load = stat_label();
    let (cpu_load_card, cpu_load_caption) = stat_card_with_caption(&cpu_load, "cpu load");
    let gpu_load = stat_label();
    let (gpu_load_card, gpu_load_caption) = stat_card_with_caption(&gpu_load, "gpu load");

    let stats = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(12)
        .column_homogeneous(true)
        .build();
    // Two by two rather than four across: four cards at this window width squeeze the
    // values into something you have to read twice.
    stats.attach(&stat_card(&power, "cpu package"), 0, 0, 1, 1);
    stats.attach(&stat_card(&cpu_temp, "cpu temperature"), 1, 0, 1, 1);
    stats.attach(&stat_card(&fan, "fan"), 0, 1, 1, 1);
    stats.attach(&system_card, 1, 1, 1, 1);
    // A third row rather than a wider one: the two-across rule above is what keeps
    // these readable at a glance.
    stats.attach(&cpu_load_card, 0, 2, 1, 1);
    stats.attach(&gpu_load_card, 1, 2, 1, 1);

    let profile = gtk::Label::builder().xalign(0.0).label("—").build();
    let profile_row = adw::ComboRow::builder()
        .title("Profile")
        .subtitle("sets the power limit and the fan curve together")
        .build();

    let battery = adw::ActionRow::builder().title("Battery").build();

    let charge_row = adw::SpinRow::builder()
        .title("Charge limit")
        .subtitle("stop charging at this percentage")
        .adjustment(&gtk::Adjustment::new(80.0, 20.0, 100.0, 5.0, 5.0, 0.0))
        .build();

    let power_row = adw::SpinRow::builder()
        .title("Power limit")
        .subtitle("sustained CPU watts, saved with the profile; ~32 s to settle")
        .adjustment(&gtk::Adjustment::new(25.0, 8.0, 25.0, 1.0, 1.0, 0.0))
        .build();

    let fan_auto = gtk::Button::builder()
        .label("Return to EC")
        .valign(gtk::Align::Center)
        .build();
    let fan_row = adw::ActionRow::builder().title("Fan").build();
    fan_row.add_suffix(&fan_auto);

    // M9. Both rows are honest about being unproven: the park level has a measured
    // justification (a hot thread on a slow core) but no measured *result* yet, and the
    // GPU cap has neither. See docs/framework_gaming_profile.md.
    let park_levels = vec!["none", "lpe", "p-only"];
    let park_row = adw::ComboRow::builder()
        .title("CPU cores")
        .subtitle("park the slower clusters so a hot thread lands on a P-core")
        .model(&gtk::StringList::new(&[
            "All cores",
            "Park LP-E",
            "P-cores only",
        ]))
        .build();

    let gpu_freq_row = adw::SpinRow::builder()
        .title("GPU clock ceiling")
        .subtitle("cap the GT to free package budget; full range when at maximum")
        .adjustment(&gtk::Adjustment::new(
            2500.0, 100.0, 2500.0, 50.0, 100.0, 0.0,
        ))
        .build();

    let tuning_group = adw::PreferencesGroup::builder()
        .title("Tuning")
        .description(
            "Per-workload levers, for older games and emulation. Parking the Atom \
             clusters forces a single hot thread onto a 4.8 GHz P-core instead of a \
             3.3 GHz one — the case where a game reports itself CPU-bound while no \
             core looks busy. Both settings are returned to normal whenever the \
             daemon stops.",
        )
        .build();
    tuning_group.add(&park_row);
    tuning_group.add(&gpu_freq_row);

    // Automatic switching. Off unless asked for, and "leave alone" is a real choice on
    // each side rather than a disguised default.
    let auto_ac = adw::ComboRow::builder().title("On AC").build();
    let auto_batt = adw::ComboRow::builder().title("On battery").build();
    // Save what is set now as a profile, and remove one that has a file.
    let save_entry = adw::EntryRow::builder()
        .title("Save current settings as")
        .build();
    let save_button = gtk::Button::builder()
        .label("Save")
        .valign(gtk::Align::Center)
        .build();
    save_entry.add_suffix(&save_button);

    let delete_button = gtk::Button::builder()
        .label("Delete")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    let delete_row = adw::ActionRow::builder()
        .title("Remove the selected profile")
        .subtitle("only profiles you saved have a file to remove")
        .build();
    delete_row.add_suffix(&delete_button);

    let save_group = adw::PreferencesGroup::builder()
        .title("Your profiles")
        .description(
            "A profile is a power limit and a fan curve under one name, plus the \
             power-profiles-daemon mode they run in. Saving captures all three as they \
             are set right now. The charge limit is left out on purpose: it is a \
             standing preference, not a performance choice.",
        )
        .build();
    save_group.add(&save_entry);
    save_group.add(&delete_row);

    let auto_group = adw::PreferencesGroup::builder()
        .title("Switch automatically")
        .description("Applied when the cable changes, not immediately")
        .build();
    auto_group.add(&auto_ac);
    auto_group.add(&auto_batt);

    let system_group = adw::PreferencesGroup::new();
    system_group.add(&profile_row);
    system_group.add(&power_row);
    system_group.add(&fan_row);
    system_group.add(&battery);
    system_group.add(&charge_row);

    let sensors_group = adw::PreferencesGroup::builder()
        .title("Temperatures")
        .build();
    let caps_group = adw::PreferencesGroup::builder()
        .title("Capabilities")
        .description("What this machine exposes. Unavailable items say why.")
        .build();

    // Started before the window is assembled so the curve editor can be handed a
    // command sender at construction. The update channel is depth 1 and its send
    // blocks, so nothing piles up in the moments before the receiver loop runs.
    let (rx, commands) = worker::spawn();

    let curve_editor = {
        let tx = commands.clone();
        crate::curve::CurveEditor::new(move |points| {
            let _ = tx.send(worker::Command::FanCurve(points));
        })
    };

    // Two columns split by role rather than by size: the left is what you set, the
    // right is the fan and the readings it reacts to. The curve editor is the tallest
    // thing in the window and the only one worth looking at while it updates, so
    // burying it under four groups of controls made it the one feature you had to go
    // hunting for.
    let column = || {
        gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build()
    };
    let left = column();
    left.append(&stats);
    left.append(&system_group);
    left.append(&tuning_group);
    left.append(&save_group);
    left.append(&auto_group);

    let right = column();
    right.append(&curve_editor.group);
    right.append(&sensors_group);
    right.append(&caps_group);

    let columns = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(18)
        .homogeneous(true)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    columns.append(&left);
    columns.append(&right);

    // One scroller around both, not one each. Two independent scroll areas side by
    // side put two scrollbars in the window and leave the user guessing which one
    // moves; the columns are close enough in length that scrolling them together
    // costs nothing.
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&columns)
        .build();

    // The second page. Its own scroller, because the chart is tall and has nothing to
    // do with the control column's scroll position.
    let monitor = crate::monitor::MonitorPage::new(commands.clone());

    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(
        &scroll,
        Some("control"),
        "Control",
        "preferences-system-symbolic",
    );
    stack.add_titled_with_icon(
        &monitor.widget,
        Some("monitor"),
        "Monitor",
        "utilities-system-monitor-symbolic",
    );

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&banner);
    outer.append(&stack);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));

    // The switcher takes the header's title slot, so the connection state that used to
    // live in the subtitle moves to the banner - which is already the mechanism for a
    // standing condition, and the place a disconnected daemon was reported anyway.
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header.set_title_widget(Some(&switcher));

    // Below the breakpoint the wide switcher does not fit; a bottom bar does.
    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
    toolbar.add_bottom_bar(&switcher_bar);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(960)
        .default_height(700)
        // A window with breakpoints must declare how small it is willing to get, or
        // libadwaita cannot know when a condition can be met - and says so, once per
        // layout pass. 360 is the usual adaptive floor and sits below the 800 the
        // breakpoint watches for, so the single-column layout is genuinely reachable.
        .width_request(360)
        .height_request(480)
        .content(&toolbar)
        .build();

    // Landscape is the default because the machine this runs on is a laptop, but it is
    // a preference, not a requirement: below this width the columns stack and the
    // window is exactly the single column it was before. Homogeneous has to come off
    // with the orientation, or stacking would force the two columns to equal heights.
    match adw::BreakpointCondition::parse("max-width: 800px") {
        Ok(condition) => {
            let breakpoint = adw::Breakpoint::new(condition);
            breakpoint.add_setter(
                &columns,
                "orientation",
                Some(&gtk::Orientation::Vertical.to_value()),
            );
            breakpoint.add_setter(&columns, "homogeneous", Some(&false.to_value()));
            // Narrow: the header has no room for the switcher, so it moves to the
            // bottom bar. Both are driven by the same stack, so the page does not change.
            breakpoint.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
            breakpoint.add_setter(&header, "title-widget", Some(&title.to_value()));
            window.add_breakpoint(breakpoint);
        }
        // A window that cannot adapt is worth having; one that refuses to open is not.
        Err(e) => eprintln!("fw-helper: no responsive breakpoint ({e})"),
    }

    // Nothing is operable until a snapshot says so. Building these live means the
    // window is briefly - or, with no daemon installed, permanently - a set of
    // controls that accept input and silently discard it.
    for group in [&system_group, &tuning_group, &save_group, &auto_group] {
        group.set_sensitive(false);
    }

    let widgets = Rc::new(RefCell::new(Widgets {
        title,
        banner,
        banner_generation,
        power,
        fan,
        cpu_temp: cpu_temp.clone(),
        system: system.clone(),
        system_caption,
        profile,
        battery,
        profile_row: profile_row.clone(),
        profile_names: Vec::new(),
        charge_row: charge_row.clone(),
        power_row: power_row.clone(),
        fan_row: fan_row.clone(),
        fan_auto: fan_auto.clone(),
        auto_ac: auto_ac.clone(),
        auto_batt: auto_batt.clone(),
        save_entry: save_entry.clone(),
        delete_button: delete_button.clone(),
        saved_profiles: Vec::new(),
        pending: HashMap::new(),
        in_flight: HashMap::new(),
        sensors_group,
        sensor_rows: HashMap::new(),
        caps_group,
        cap_rows: HashMap::new(),
        system_group,
        save_group,
        auto_group,
        tuning_group,
        curve: Rc::clone(&curve_editor),
        monitor: Rc::clone(&monitor),
        park_row: park_row.clone(),
        park_levels,
        gpu_freq_row: gpu_freq_row.clone(),
        gpu_rp0: 0,
        cpu_load: cpu_load.clone(),
        cpu_load_caption: cpu_load_caption.clone(),
        gpu_load: gpu_load.clone(),
        gpu_load_caption: gpu_load_caption.clone(),
    }));

    // Controls send commands; they never touch hardware from the main loop.
    {
        let w = Rc::clone(&widgets);
        let w2 = Rc::clone(&widgets);
        let tx = commands.clone();
        profile_row.connect_selected_notify(move |row| {
            trace(&format!("profile selected_notify -> {}", row.selected()));
            // A failed borrow *is* the guard. Every control here is both an input and a
            // display, and setting one from telemetry fires its changed signal exactly
            // as a click would - synchronously, while `apply` still holds the mutable
            // borrow. So being unable to borrow means this is our own write, not the
            // user's. A flag cannot do this job: the borrow panics before it is read,
            // and the panic is inside a C callback that cannot unwind, so it aborts.
            let Ok(w) = w.try_borrow() else {
                trace("profile: skipped, our own write");
                return;
            };
            if let Some(name) = w.profile_names.get(row.selected() as usize).cloned() {
                trace(&format!("profile: sending {name}"));
                drop(w);
                if let Ok(mut w) = w2.try_borrow_mut() {
                    w.in_flight
                        .insert("profile", (name.clone(), std::time::Instant::now()));
                    flash(&w, &format!("applying {name}…"));
                }
                let _ = tx.send(Command::Profile(name));
            }
        });
    }
    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        charge_row.connect_value_notify(move |row| {
            trace(&format!("charge value_notify -> {}", row.value()));
            if w.try_borrow().is_err() {
                trace("charge: skipped, our own write");
                return;
            }
            let value = row.value() as u8;
            debounce(&w, &tx, "charge", move || Command::ChargeLimit(value));
        });
    }
    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        power_row.connect_value_notify(move |row| {
            trace(&format!("power value_notify -> {}", row.value()));
            if w.try_borrow().is_err() {
                trace("power: skipped, our own write");
                return;
            }
            let value = row.value() as u32;
            debounce(&w, &tx, "power", move || Command::PowerLimit(value));
        });
    }
    {
        let tx = commands.clone();
        fan_auto.connect_clicked(move |_| {
            let _ = tx.send(Command::FanAuto);
        });
    }
    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        park_row.connect_selected_notify(move |row| {
            trace(&format!("park selected_notify -> {}", row.selected()));
            // Same borrow-as-guard reasoning as every other control here: a failed
            // borrow means this is our own write from telemetry, not the user's click.
            let Ok(w) = w.try_borrow() else {
                trace("park: skipped, our own write");
                return;
            };
            let Some(level) = w.park_levels.get(row.selected() as usize) else {
                return;
            };
            // Not debounced: a combo produces one event per choice, and parking twelve
            // cores is slow enough that coalescing would only hide it.
            let _ = tx.send(Command::ParkLevel((*level).to_string()));
        });
    }
    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        gpu_freq_row.connect_value_notify(move |row| {
            trace(&format!("gpu freq value_notify -> {}", row.value()));
            if w.try_borrow().is_err() {
                trace("gpu freq: skipped, our own write");
                return;
            }
            let mhz = row.value() as u32;
            // At the top of the range the user is asking for no cap, which the daemon
            // spells 0. Sending rp0 instead would leave a cap in place that happens to
            // equal the ceiling, and the restore path would then have something to undo.
            let rp0 = w.borrow().gpu_rp0;
            let value = if rp0 != 0 && mhz >= rp0 { 0 } else { mhz };
            debounce(&w, &tx, "gpufreq", move || Command::GpuMaxFreq(value));
        });
    }
    for which in ["ac", "battery"] {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        let row = if which == "ac" { &auto_ac } else { &auto_batt };
        row.connect_selected_notify(move |_| {
            let Ok(w) = w.try_borrow() else { return };
            // Both sides are sent together: the daemon holds one setting, so sending
            // only the side that changed would clear the other.
            let _ = tx.send(Command::AutoProfiles(
                auto_choice(&w, &w.auto_ac),
                auto_choice(&w, &w.auto_batt),
            ));
        });
    }

    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        save_button.connect_clicked(move |_| {
            // Immutable: every call below is a GTK setter, which takes &self.
            let Ok(w) = w.try_borrow() else {
                return;
            };
            let name = w.save_entry.text().trim().to_lowercase();
            if name.is_empty() {
                flash(&w, "give the profile a name first");
                return;
            }
            // The daemon validates the name properly and its message says what is
            // wrong, so do not duplicate the rules here.
            w.save_entry.set_text("");
            let _ = tx.send(Command::SaveProfile(name));
        });
    }
    {
        let w = Rc::clone(&widgets);
        let tx = commands.clone();
        delete_button.connect_clicked(move |_| {
            let Ok(w) = w.try_borrow() else { return };
            let selected = w.profile_row.selected() as usize;
            if let Some(name) = w.profile_names.get(selected).cloned() {
                let _ = tx.send(Command::DeleteProfile(name));
            }
        });
    }

    glib::spawn_future_local(async move {
        while let Ok(update) = rx.recv().await {
            let mut w = widgets.borrow_mut();
            match update {
                Update::Data(s) => apply(&mut w, &s),
                Update::Disconnected(why) => disconnected(&mut w, &why),
                Update::CommandResult { key, result } => {
                    // Only a failure releases the control here. A success keeps holding
                    // until telemetry actually reports the new value, because the
                    // snapshot already in flight predates the change.
                    if result.is_err() {
                        w.in_flight.remove(key);
                    }
                    if key == "fan" {
                        w.curve.applied(&result);
                    }
                    let msg = match result {
                        Ok(msg) => {
                            trace(&format!("command ok: {msg}"));
                            msg
                        }
                        // The daemon's messages are written to be read by a person and
                        // say what to do about it, so show them rather than a generic
                        // failure.
                        Err(msg) => {
                            trace(&format!("command FAILED: {msg}"));
                            msg
                        }
                    };
                    flash(&w, &msg);
                }
            }
        }
    });

    window.present();
}

/// The value telemetry should report once a command has taken effect.
fn expected(cmd: &Command) -> String {
    match cmd {
        Command::Profile(name) => name.clone(),
        Command::PowerLimit(w) => w.to_string(),
        Command::ChargeLimit(v) => v.to_string(),
        Command::FanAuto => "auto".to_string(),
        Command::FanCurve(_) => "curve".to_string(),
        Command::AutoProfiles(ac, batt) => format!("{ac}/{batt}"),
        Command::ParkLevel(level) => level.clone(),
        // 0 means "no cap", which the daemon reports back as max == rp0 rather than as
        // a zero, so the two have to be spelled the same way here.
        Command::GpuMaxFreq(mhz) => mhz.to_string(),
        Command::SaveProfile(name) | Command::DeleteProfile(name) => name.clone(),
        // Nothing to wait for: the recording controls are driven directly by what the
        // daemon reports it is recording, so there is no widget to hold meanwhile.
        Command::StartRecording(_) | Command::StopRecording | Command::DeleteSession(_) => {
            String::new()
        }
    }
}

/// Trace control activity when FW_HELPER_DEBUG_WIDGETS is set.
fn trace(what: &str) {
    if std::env::var_os("FW_HELPER_DEBUG_WIDGETS").is_some() {
        eprintln!("ui: {what}");
    }
}

/// How long to keep showing a value telemetry has not confirmed.
///
/// Long enough for a slow write to land, short enough that a silently ignored change
/// does not leave the control lying about the machine indefinitely.
const CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a control must sit still before its value is sent.
///
/// A spin row emits a change per step, so holding `+` or typing a two-digit number
/// fires several. Each is a D-Bus call through polkit, and they race: the last to land
/// wins, which is not necessarily the one the user stopped on. 400 ms is below the
/// threshold where a control feels laggy and far above the gap between key repeats.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// Send `cmd` once the user stops adjusting `key`.
fn debounce(
    widgets: &Rc<RefCell<Widgets>>,
    tx: &std::sync::mpsc::Sender<Command>,
    key: &'static str,
    make: impl Fn() -> Command + 'static,
) {
    let tx = tx.clone();
    // Cancel whatever was queued for this control: only the value it settles on matters.
    if let Ok(mut w) = widgets.try_borrow_mut() {
        if let Some(id) = w.pending.remove(key) {
            id.remove();
        }
    }
    let w2 = Rc::clone(widgets);
    let id = glib::timeout_add_local_once(DEBOUNCE, move || {
        if let Ok(mut w) = w2.try_borrow_mut() {
            w.pending.remove(key);
        }
        trace(&format!("{key}: debounce fired, sending"));
        let cmd = make();
        if let Ok(mut w) = w2.try_borrow_mut() {
            w.in_flight
                .insert(key, (expected(&cmd), std::time::Instant::now()));
        }
        let _ = tx.send(cmd);
    });
    if let Ok(mut w) = widgets.try_borrow_mut() {
        w.pending.insert(key, id);
    }
}

/// The profile a combo is pointing at, or empty for "leave alone" (index 0).
fn auto_choice(w: &Widgets, row: &adw::ComboRow) -> String {
    match row.selected() {
        0 => String::new(),
        i => w
            .profile_names
            .get(i as usize - 1)
            .cloned()
            .unwrap_or_default(),
    }
}

/// Push the daemon's view into the controls, and enable only what this machine can do.
///
/// Called with the widgets mutably borrowed, which is what stops the changed signals
/// these writes fire from being mistaken for user input.
fn sync_controls(w: &mut Widgets, s: &Snapshot) {
    // Never overwrite a control the user is still adjusting, or one whose command has
    // not come back yet. Its value is newer than the daemon's, and writing the old one
    // back is exactly what makes a setting appear to snap away the instant it changes.
    // Release anything telemetry has now confirmed, or that has waited long enough.
    let observed = [
        ("profile", s.profile.clone().unwrap_or_default()),
        (
            "power",
            s.power_limit.map(|v| v.to_string()).unwrap_or_default(),
        ),
        (
            "charge",
            s.charge_limit.map(|v| v.to_string()).unwrap_or_default(),
        ),
        ("park", s.park_level.0.clone()),
        (
            "gpufreq",
            // Uncapped is reported as max == rp0, and requested as 0. Normalise, or a
            // "remove the cap" command would never be seen to land and the row would
            // sit held until the timeout.
            match (s.gpu_freq_window.1, s.gpu_freq_window.3) {
                (max, rp0) if rp0 != 0 && max >= rp0 => "0".to_string(),
                (max, _) => max.to_string(),
            },
        ),
    ];
    for (key, now) in observed {
        if let Some((want, since)) = w.in_flight.get(key) {
            if *want == now || since.elapsed() > CONFIRM_TIMEOUT {
                w.in_flight.remove(key);
            }
        }
    }

    let charge_busy = w.pending.contains_key("charge") || w.in_flight.contains_key("charge");
    let power_busy = w.pending.contains_key("power") || w.in_flight.contains_key("power");
    let profile_busy = w.in_flight.contains_key("profile");

    // The profile list can change under us: user profiles are read at daemon startup,
    // so a daemon restart can add or remove entries.
    let names: Vec<String> = s.profiles.clone();
    let names_changed = names != w.profile_names;
    if names_changed {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        w.profile_row.set_model(Some(&gtk::StringList::new(&refs)));
        w.profile_names = names;
    }
    if let Some(active) = &s.profile {
        if !profile_busy {
            if let Some(i) = w.profile_names.iter().position(|n| n == active) {
                w.profile_row.set_selected(i as u32);
            }
        }
    }
    w.profile_row.set_sensitive(
        !w.profile_names.is_empty() && s.capability("power limit").is_some_and(|(ok, _)| ok),
    );
    // Say what the machine is actually set to, not just which profile is named. A
    // profile is a power budget plus a curve, and the budget is the part people change
    // by hand afterwards - so it can differ from what the profile itself specifies.
    let mut applied = Vec::new();
    if let Some(w_) = s.power_limit {
        applied.push(format!("{w_} W"));
    }
    match s.fan_mode.as_deref() {
        Some("curve") => applied.push("fan on a curve".into()),
        Some("manual") => applied.push("fan set by hand".into()),
        Some("unavailable") | None => {}
        Some(_) => applied.push("fan left to the EC".into()),
    }
    if matches!(s.profile_backend.as_deref(), Some("platform_profile")) {
        applied.push("GNOME slider not in sync".into());
    }
    w.profile_row.set_subtitle(&if applied.is_empty() {
        "power limit and fan curve".to_string()
    } else {
        applied.join(" · ")
    });

    // Sensitivity follows the *capability*, not the presence of a value. On this board
    // the attribute exists, accepts 80 and reads back 80 while the EC charges past it
    // to full - so "we have a number" is exactly the evidence that misled us. Show the
    // number, because it is genuinely what the kernel holds, but do not let it be
    // edited or described as if it governed anything.
    let charge_ok = s.capability("charge limit").is_some_and(|(ok, _)| ok);
    if let Some(v) = s.charge_limit {
        if !charge_busy {
            w.charge_row.set_value(f64::from(v));
        }
    }
    match (charge_ok, s.charge_limit) {
        (true, Some(_)) => {
            w.charge_row.set_sensitive(true);
            w.charge_row
                .set_subtitle("stop charging at this percentage");
        }
        _ => {
            w.charge_row.set_sensitive(false);
            // A dead control with no explanation is the thing the capability system
            // exists to prevent, so borrow the daemon's reason.
            w.charge_row.set_subtitle(
                s.capability("charge limit")
                    .map(|(_, why)| why)
                    .filter(|why| !why.is_empty())
                    .unwrap_or("unavailable on this machine"),
            );
        }
    }

    match (s.power_limit, s.power_limit_max) {
        (Some(v), Some(max)) => {
            // Clamp to what the zone actually admits, never to the MSR zone's
            // fictional 200 W.
            w.power_row.adjustment().set_upper(f64::from(max));
            if !power_busy {
                w.power_row.set_value(f64::from(v));
            }
            w.power_row.set_sensitive(true);
        }
        _ => {
            w.power_row.set_sensitive(false);
            w.power_row.set_subtitle(
                s.capability("power limit")
                    .map(|(_, why)| why)
                    .filter(|why| !why.is_empty())
                    .unwrap_or("unavailable on this machine"),
            );
        }
    }

    // ------------------------------------------------------------------------------
    // M9 — parking and the GPU ceiling
    // ------------------------------------------------------------------------------
    let park_busy = w.in_flight.contains_key("park");
    let parkable: u32 = s.cpu_clusters.iter().map(|(_, _, _, n)| n).sum();
    if s.cpu_clusters.len() > 1 && parkable > 0 {
        w.park_row.set_sensitive(true);
        // Name the clusters the daemon actually found rather than assuming this part's
        // shape; a machine with two tiers should not be told about a third.
        let shape: Vec<String> = s
            .cpu_clusters
            .iter()
            .map(|(name, cpus, mhz, _)| format!("{}x{name} {mhz}MHz", cpus.len()))
            .collect();
        w.park_row.set_subtitle(&match s.park_level.1.as_str() {
            // A state no level describes: someone offlined cores by hand. Say so
            // rather than showing whichever entry happens to be selected.
            "mixed" => format!("{} — some cores were offlined by hand", shape.join(", ")),
            _ => shape.join(", "),
        });
        if !park_busy {
            if let Some(i) = w.park_levels.iter().position(|l| *l == s.park_level.0) {
                w.park_row.set_selected(i as u32);
            }
        }
    } else {
        w.park_row.set_sensitive(false);
        w.park_row
            .set_subtitle("no hybrid CPU topology here, so there is nothing useful to park");
    }

    let gpu_busy = w.pending.contains_key("gpufreq") || w.in_flight.contains_key("gpufreq");
    let (_, gpu_max, gpu_rpn, gpu_rp0) = s.gpu_freq_window;
    if gpu_rp0 > 0 {
        w.gpu_rp0 = gpu_rp0;
        w.gpu_freq_row.adjustment().set_lower(f64::from(gpu_rpn));
        w.gpu_freq_row.adjustment().set_upper(f64::from(gpu_rp0));
        if !gpu_busy {
            w.gpu_freq_row.set_value(f64::from(gpu_max));
        }
        w.gpu_freq_row.set_sensitive(true);
        // Show both clocks, because the difference is the whole point: cur_freq is the
        // DVFS request and reads a constant 2500 here under load, act_freq is what
        // happened, and act_freq reads 0 whenever the GT is parked in RC6.
        let (req, act) = s.gpu_clocks;
        w.gpu_freq_row.set_subtitle(&match (req, act) {
            (0, _) => "cap the GT to free package budget".to_string(),
            (r, 0) => format!("asking {r} MHz; GT parked"),
            (r, a) => format!("asking {r} MHz, reaching {a} MHz"),
        });
    } else {
        w.gpu_freq_row.set_sensitive(false);
        w.gpu_freq_row
            .set_subtitle("no xe GT frequency controls found under /sys/class/drm");
    }

    // The auto-switch combos list "leave alone" first, then every profile.
    if names_changed {
        let mut entries: Vec<&str> = vec!["leave alone"];
        entries.extend(w.profile_names.iter().map(String::as_str));
        for row in [&w.auto_ac, &w.auto_batt] {
            row.set_model(Some(&gtk::StringList::new(&entries)));
        }
    }
    let (want_ac, want_batt) = &s.auto_profiles;
    for (row, want) in [(&w.auto_ac, want_ac), (&w.auto_batt, want_batt)] {
        let index = w
            .profile_names
            .iter()
            .position(|n| n == want)
            .map(|i| i as u32 + 1)
            .unwrap_or(0);
        row.set_selected(index);
    }

    // Deleting is only meaningful for a profile that has a file. A user file may
    // replace a built-in, so the daemon is asked which those are rather than guessing
    // from the name.
    w.saved_profiles = s.saved_profiles.clone();
    let selected = w.profile_row.selected() as usize;
    let deletable = w
        .profile_names
        .get(selected)
        .is_some_and(|n| w.saved_profiles.contains(n));
    w.delete_button.set_sensitive(deletable);

    // Handing the fan back is only meaningful while we hold it.
    let ours = matches!(s.fan_mode.as_deref(), Some("manual") | Some("curve"));
    w.fan_auto.set_sensitive(ours);

    if std::env::var_os("FW_HELPER_DEBUG_WIDGETS").is_some() {
        eprintln!(
            "widgets: profile sensitive={} model={} selected={} | auto_ac model={} \
             | power sensitive={} | charge sensitive={} | fan_auto sensitive={}\n\
             widgets: park sensitive={} selected={} sub={:?} | gpu sensitive={} \
             value={} range={}..{} sub={:?}",
            w.profile_row.is_sensitive(),
            w.profile_row.model().map(|m| m.n_items()).unwrap_or(0),
            w.profile_row.selected(),
            w.auto_ac.model().map(|m| m.n_items()).unwrap_or(0),
            w.power_row.is_sensitive(),
            w.charge_row.is_sensitive(),
            w.fan_auto.is_sensitive(),
            w.park_row.is_sensitive(),
            w.park_row.selected(),
            w.park_row
                .subtitle()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            w.gpu_freq_row.is_sensitive(),
            w.gpu_freq_row.value(),
            w.gpu_freq_row.adjustment().lower(),
            w.gpu_freq_row.adjustment().upper(),
            w.gpu_freq_row
                .subtitle()
                .map(|s| s.to_string())
                .unwrap_or_default(),
        );
    }
}

/// One line saying who is driving the fan and, when it is us, why it may not be doing
/// what was asked.
fn describe_fan(s: &Snapshot) -> String {
    match s.fan_mode.as_deref() {
        Some("curve") => {
            let duty = s.fan_duty.unwrap_or(0);
            format!("following a curve · {}", crate::units::describe_duty(duty))
        }
        Some("manual") => {
            let duty = s.fan_duty.unwrap_or(0);
            match s.fan_floor {
                // The floor is why a quiet setting may not be honoured, and saying so
                // is the difference between a bug and a decision (ADR 0006).
                Some(f) if f > 0 && u32::from(duty) <= u32::from(f) + 3 => {
                    format!(
                        "manual · {}, held at the firmware floor",
                        crate::units::describe_duty(duty)
                    )
                }
                _ => format!("manual · {}", crate::units::describe_duty(duty)),
            }
        }
        Some("unavailable") | None => "unavailable".to_string(),
        Some(_) => "EC automatic".to_string(),
    }
}

/// CPU and GPU load, with the clock in the caption.
///
/// Load is what makes the power reading interpretable: 15 W is a capped machine under
/// load or an idle one, and the two look identical without this. The GPU caption also
/// carries its throttle reason when one is asserted, because the GPU publishes real
/// reasons where the CPU package publishes none — for the CPU, read power against the
/// limit instead (see chart.rs).
fn set_load_cards(w: &Widgets, s: &Snapshot) {
    match s.load.cpu_percent {
        Some(pct) => w.cpu_load.set_label(&format!("{pct:.0}%")),
        None => w.cpu_load.set_label("—"),
    }
    // The busy-weighted clock, not the all-core mean. On a mostly idle machine the mean
    // averages in fifteen parked cores and reports ~1.8 GHz while the core doing the
    // work is at 4.5 — the same category of error as showing a GPU's requested clock.
    // Falls back to the mean only when nothing executed and there is no busy figure.
    w.cpu_load_caption.set_label(&caption(
        "cpu load",
        clock(s.load.cpu_mhz_busy.or(s.load.cpu_mhz)),
        watts(s.load.cpu_watts),
    ));

    match s.load.gpu_percent {
        Some(pct) => w.gpu_load.set_label(&format!("{pct:.0}%")),
        None => w.gpu_load.set_label("—"),
    }
    // A throttle reason displaces the clock: when the GPU is being held back, that is
    // the more useful of the two. It does not displace the wattage, which stays
    // comparable with the CPU card beside it.
    let gpu_first = match &s.load.gpu_throttle {
        Some(reason) if !reason.is_empty() => Some(reason.clone()),
        _ => clock(s.load.gpu_mhz),
    };
    w.gpu_load_caption
        .set_label(&caption("gpu load", gpu_first, watts(s.load.gpu_watts)));
}

/// `"<name> · <part> · <part>"`, skipping parts that are not known.
///
/// Built by joining rather than by formatting a fixed shape, because any of the three
/// can be missing independently — a machine with no readable `uncore` rail still has a
/// clock to show, and a parked GT still has a wattage.
fn caption(name: &str, a: Option<String>, b: Option<String>) -> String {
    std::iter::once(name.to_string())
        .chain(a)
        .chain(b)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// GHz, or `parked` for a GT sitting in RC6.
///
/// Zero is a real reading here rather than a missing one: the GPU genuinely is not
/// clocking. Showing "parked" says that, where dropping the field entirely reads as a
/// broken sensor — which is exactly how it looked before, since `act_freq` is zero
/// most of the time on an idle desktop.
fn clock(mhz: Option<u64>) -> Option<String> {
    match mhz? {
        0 => Some("parked".to_string()),
        mhz => Some(format!("{:.1} GHz", mhz as f64 / 1000.0)),
    }
}

fn watts(w: Option<f64>) -> Option<String> {
    Some(format!("{:.1} W", w?))
}

fn stat_label() -> gtk::Label {
    let l = gtk::Label::builder().label("—").build();
    l.add_css_class("stat-value");
    l
}

/// A stat card, handing back the caption label so it can be rewritten later.
///
/// Built explicitly rather than by walking the widget tree afterwards: an earlier
/// version fished the caption out with `first_child().last_child()`, which is wrong
/// here — the card *is* the box, so the first child is the value and it has no children
/// of its own. That downcast failed inside GTK's activate callback, which cannot
/// unwind, so it aborted the process rather than panicking.
fn stat_card_with_caption(value: &gtk::Label, caption: &str) -> (gtk::Widget, gtk::Label) {
    // Wrapping, because the system card's caption carries battery percentage and time
    // remaining. Without it the label sets the column width and the grid goes lopsided.
    let label = gtk::Label::builder()
        .label(caption)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(20)
        .build();
    label.add_css_class("stat-label");

    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    b.append(value);
    b.append(&label);
    b.add_css_class("stat-card");
    b.add_css_class("card");
    (b.upcast(), label)
}

fn stat_card(value: &gtk::Label, caption: &str) -> gtk::Widget {
    stat_card_with_caption(value, caption).0
}

/// Show a message that hides itself after [`BANNER_SECONDS`].
///
/// Takes `&Widgets`, not `&mut`: the generation counter is a `Cell`, so this works from
/// a shared borrow. That matters because half the callers are signal handlers holding
/// the widgets immutably, and requiring a mutable borrow would mean either a second
/// borrow or restructuring handlers that have their own reasons for the borrow they hold.
fn flash(w: &Widgets, message: &str) {
    w.banner.set_title(message);
    w.banner.set_revealed(true);

    let generation = w.banner_generation.get().wrapping_add(1);
    w.banner_generation.set(generation);
    let banner = w.banner.clone();
    let current = Rc::clone(&w.banner_generation);
    glib::timeout_add_seconds_local_once(BANNER_SECONDS, move || {
        // Superseded timers still fire; they just find a newer generation and do
        // nothing, which is what keeps a later message on screen for its full run.
        if current.get() == generation {
            banner.set_revealed(false);
        }
    });
}

fn disconnected(w: &mut Widgets, why: &str) {
    w.title.set_subtitle("daemon not running");
    // Outlive any pending flash: this condition does not expire on a timer.
    w.banner_generation
        .set(w.banner_generation.get().wrapping_add(1));
    w.banner
        .set_title(&format!("fw-helperd is not available — {why}"));
    w.banner.set_revealed(true);
    // Every reading is now stale. Blank all four cards, not the two that happen to
    // move fastest - a temperature left on screen is read as the current one.
    w.power.set_label("—");
    w.fan.set_label("—");
    w.cpu_temp.set_label("—");
    w.system.set_label("—");
    w.cpu_load.set_label("—");
    w.cpu_load_caption.set_label("cpu load");
    w.gpu_load.set_label("—");
    w.gpu_load_caption.set_label("gpu load");
    w.profile.set_label("—");
    // A control that cannot reach the daemon must not look operable. Switching the
    // groups off leaves each row's own sensitivity untouched underneath, so whatever
    // the capabilities said still applies once a snapshot comes back.
    set_controls_live(w, false);
    // The sensor and capability lists keep their contents - emptying them would make
    // the window jump - but they are dimmed to say they are no longer live.
    w.sensors_group.set_sensitive(false);
    w.caps_group.set_sensitive(false);
    w.monitor.set_disconnected();
}

/// Enable or disable the control groups as a whole.
fn set_controls_live(w: &Widgets, live: bool) {
    w.system_group.set_sensitive(live);
    w.save_group.set_sensitive(live);
    w.auto_group.set_sensitive(live);
    w.tuning_group.set_sensitive(live);
    w.curve.set_live(live);
}

fn apply(w: &mut Widgets, s: &Snapshot) {
    w.title.set_subtitle("Framework Laptop 13");
    // Undo a previous disconnect before `sync_controls` refines each row; the two
    // levels are independent, so a row the daemon says is unavailable stays off.
    set_controls_live(w, true);
    w.sensors_group.set_sensitive(true);
    w.caps_group.set_sensitive(true);
    w.curve.update(s);
    w.monitor.update(s);

    // Refresh the controls from the daemon. The signals this fires are suppressed by
    // the mutable borrow we are already holding - see the handlers.
    sync_controls(w, s);

    w.power.set_label(
        &s.package_watts
            .map(|v| format!("{v:.1} W"))
            .unwrap_or_else(|| "—".into()),
    );
    w.cpu_temp.set_label(
        &s.temps
            .iter()
            .find(|t| Some(t.label.as_str()) == s.control_sensor.as_deref())
            .or_else(|| s.temps.first())
            .map(|t| format!("{:.0} °C", t.celsius))
            .unwrap_or_else(|| "—".into()),
    );

    // Whole-machine draw exists only on battery: nothing reports it on mains, and the
    // CPU package figure is a fraction of it, so showing that here would mislead.
    // Caption assembled from whatever is actually known, so a missing reading drops a
    // clause instead of printing a placeholder.
    let mut caption = vec!["system".to_string()];
    if let Some(pct) = s.battery_percent {
        caption.push(format!("{pct}%"));
    }
    match (s.system_watts, s.on_ac) {
        (Some(watts), _) => {
            w.system.set_label(&format!("{watts:.1} W"));
            if let Some(m) = s.battery_minutes {
                caption.push(format!("{}h {:02}m left", m / 60, m % 60));
            }
        }
        (None, Some(true)) => {
            w.system.set_label("—");
            // Nothing reports whole-machine draw on mains, so say why rather than
            // leaving a dash with no explanation.
            caption.push("on mains".into());
        }
        _ => w.system.set_label("—"),
    }
    w.system_caption.set_label(&caption.join(" · "));

    set_load_cards(w, s);

    w.fan.set_label(&match s.fan_rpm {
        // The EC keeps the fan stopped below roughly 45 C, so zero is a state
        // worth naming rather than showing as "0 rpm".
        Some(0) => "off".to_string(),
        Some(r) => format!("{r} rpm"),
        None => "—".to_string(),
    });

    w.profile
        .set_label(s.platform_profile.as_deref().unwrap_or("—"));
    w.fan_row.set_subtitle(&describe_fan(s));

    // Percentage, state, and what is actually left in the pack. Assembled from
    // whatever is known rather than formatted as one string, so a machine reporting
    // less drops a clause instead of printing a placeholder — the same rule the system
    // caption follows. Watt-hours are the figure that compares against a draw in watts;
    // a percentage on its own cannot answer "how long will this last".
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = s.battery_percent {
        parts.push(format!("{p}%"));
    }
    if let Some(st) = s.battery_status.as_deref() {
        parts.push(st.to_string());
    }
    match (s.battery_wh, s.battery_wh_full) {
        (Some(wh), Some(full)) => parts.push(format!("{wh:.1} of {full:.1} Wh")),
        (Some(wh), None) => parts.push(format!("{wh:.1} Wh")),
        _ => {}
    }
    w.battery.set_subtitle(&if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    });

    for sensor in &s.temps {
        let entry = w.sensor_rows.get(&sensor.label).cloned();
        let (row, bar) = match entry {
            Some(pair) => pair,
            None => {
                let bar = gtk::LevelBar::builder()
                    .min_value(0.0)
                    .max_value(sensor.critical.unwrap_or(100.0))
                    .valign(gtk::Align::Center)
                    .width_request(90)
                    .build();
                let row = adw::ActionRow::builder().title(&sensor.label).build();
                row.add_suffix(&bar);
                w.sensors_group.add(&row);
                w.sensor_rows
                    .insert(sensor.label.clone(), (row.clone(), bar.clone()));
                (row, bar)
            }
        };

        let is_control = s.control_sensor.as_deref() == Some(sensor.label.as_str());
        let suffix = if is_control {
            "  · fan curve input"
        } else {
            ""
        };
        let crit = sensor
            .critical
            .map(|c| format!(" of {c:.0} °C"))
            .unwrap_or_default();
        row.set_subtitle(&format!("{:.1} °C{crit}{suffix}", sensor.celsius));
        bar.set_value(sensor.celsius.clamp(0.0, bar.max_value()));
    }

    for (name, ok, why) in &s.capabilities {
        let row = w.cap_rows.entry(name.clone()).or_insert_with(|| {
            let row = adw::ActionRow::builder().title(name).build();
            w.caps_group.add(&row);
            row
        });
        // Unavailable knobs keep their reason visible; a disabled control with no
        // explanation is the thing ADR 0003 exists to prevent.
        row.set_subtitle(if *ok { "available" } else { why });
        row.set_sensitive(*ok);
    }
}
