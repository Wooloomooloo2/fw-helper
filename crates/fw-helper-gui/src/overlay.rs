//! `fw-helper --overlay` — a compact always-visible readout.
//!
//! ## What this can and cannot do, stated plainly
//!
//! On GNOME/Wayland an ordinary client **cannot** ask to be kept above other windows,
//! and cannot position itself either: Mutter implements no protocol for it, so there is
//! no flag to set and nothing to work around. This window therefore behaves like any
//! other window — useful beside a borderless game, on a second display, or while
//! working; covered by a genuinely fullscreen game.
//!
//! For numbers *inside* a fullscreen game the answer is MangoHud, which is not a window
//! at all: the Vulkan loader loads it into the game's own process and it paints into the
//! frame before presentation, so the compositor never learns an overlay exists. fw-helper
//! feeds it the things it cannot know — see `data/mangohud/fw-helper.conf`.
//!
//! Consequently this deliberately does **not** try to remember its position. It could
//! not restore one.

use crate::worker::{self, Update};
use adw::prelude::*;
use fw_helper_client::Snapshot;
use gtk::glib;

/// One row of the readout: a fixed label and a value that changes.
struct Row {
    value: gtk::Label,
    detail: gtk::Label,
}

fn row(grid: &gtk::Grid, index: i32, name: &str) -> Row {
    let label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .css_classes(["stat-label"])
        .build();
    let value = gtk::Label::builder()
        .xalign(1.0)
        .label("\u{2014}")
        .css_classes(["overlay-value"])
        .build();
    let detail = gtk::Label::builder()
        .xalign(1.0)
        .label("")
        .css_classes(["stat-label"])
        .build();
    grid.attach(&label, 0, index, 1, 1);
    grid.attach(&value, 1, index, 1, 1);
    grid.attach(&detail, 2, index, 1, 1);
    Row { value, detail }
}

pub fn build(app: &adw::Application) {
    let title = adw::WindowTitle::new("fw-helper", "connecting\u{2026}");
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        .show_title(true)
        .build();
    header.add_css_class("flat");

    let grid = gtk::Grid::builder()
        .row_spacing(2)
        .column_spacing(14)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(14)
        .margin_end(14)
        .build();
    grid.set_column_homogeneous(false);

    let cpu = row(&grid, 0, "cpu");
    let gpu = row(&grid, 1, "gpu");
    let power = row(&grid, 2, "power");
    let fan = row(&grid, 3, "fan");
    let memory = row(&grid, 4, "memory");
    let battery = row(&grid, 5, "battery");

    // The value column is the one that should absorb the width, so the numbers stay
    // right-aligned against the same edge as the machine's state changes their length.
    for r in [&cpu, &gpu, &power, &fan, &memory, &battery] {
        r.value.set_hexpand(true);
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&grid));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("fw-helper")
        .default_width(280)
        .default_height(240)
        .resizable(true)
        .content(&toolbar)
        .build();

    let (rx, _commands) = worker::spawn();
    glib::spawn_future_local(async move {
        while let Ok(update) = rx.recv().await {
            match update {
                Update::Data(s) => {
                    title.set_subtitle(&subtitle(&s));
                    apply(&s, &cpu, &gpu, &power, &fan, &memory, &battery);
                }
                Update::Disconnected(_) => {
                    title.set_subtitle("no daemon");
                    for r in [&cpu, &gpu, &power, &fan, &memory, &battery] {
                        // Blank rather than stale: a temperature left on screen is read
                        // as the current one.
                        r.value.set_label("\u{2014}");
                        r.detail.set_label("");
                    }
                }
                Update::CommandResult { .. } => {}
            }
        }
    });

    window.present();
}

fn subtitle(s: &Snapshot) -> String {
    match (&s.recording, &s.profile) {
        (Some(r), _) => format!("recording {} \u{00b7} {} samples", r.label, r.samples),
        (None, Some(p)) => p.clone(),
        (None, None) => "connected".into(),
    }
}

fn apply(s: &Snapshot, cpu: &Row, gpu: &Row, power: &Row, fan: &Row, memory: &Row, battery: &Row) {
    let pct = |v: Option<f64>| {
        v.map(|x| format!("{x:.0}%"))
            .unwrap_or_else(|| "\u{2014}".into())
    };
    let temp = |label: &str| {
        s.temps
            .iter()
            .find(|r| r.label == label)
            .map(|r| format!("{:.0}\u{b0}C", r.celsius))
            .unwrap_or_default()
    };

    cpu.value.set_label(&pct(s.load.cpu_percent));
    cpu.detail.set_label(&{
        let t = temp(fw_helper_core::PACKAGE_TEMP_LABEL);
        if t.is_empty() {
            s.control_sensor
                .as_ref()
                .map(|l| temp(l))
                .unwrap_or_default()
        } else {
            t
        }
    });

    gpu.value.set_label(&pct(s.load.gpu_percent));
    gpu.detail.set_label(
        &s.load
            .gpu_mhz
            .map(|m| format!("{m} MHz"))
            .unwrap_or_default(),
    );

    power.value.set_label(
        &s.package_watts
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "\u{2014}".into()),
    );
    // The limit beside the draw, because the pair is the reading: sitting on the limit
    // is what being power limited looks like, and there is no flag that says so.
    power.detail.set_label(
        &s.power_limit
            .map(|w| format!("of {w} W"))
            .unwrap_or_default(),
    );

    fan.value.set_label(
        &s.fan_rpm
            .map(|r| format!("{r} rpm"))
            .unwrap_or_else(|| "\u{2014}".into()),
    );
    // Who owns the fan. A speed means a different thing under each, and someone who
    // cannot see which is which has no way to tell control from a stuck fan (ADR 0006).
    fan.detail.set_label(match s.fan_mode.as_deref() {
        Some("manual") => "fw-helper",
        Some("curve") => "curve",
        Some("auto") => "firmware",
        _ => "",
    });

    memory.value.set_label(&pct(s.load.mem_percent()));
    memory.detail.set_label(
        &s.load
            .mem_used_kb
            .map(|kb| format!("{:.1} GB", kb as f64 / 1_048_576.0))
            .unwrap_or_default(),
    );

    battery.value.set_label(
        &s.battery_percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "\u{2014}".into()),
    );
    // The pack temperature, not the CPU's: it is the one component with a low limit and
    // no protection of its own, and nothing else on screen reports it.
    battery.detail.set_label(&temp_of_battery(s));
}

fn temp_of_battery(s: &Snapshot) -> String {
    s.temps
        .iter()
        .find(|r| r.label.starts_with("battery"))
        .map(|r| format!("{:.0}\u{b0}C", r.celsius))
        .unwrap_or_default()
}
