//! The Battery page: where the charge went.
//!
//! Reads the daemon's battery log straight from disk - it is world-readable precisely
//! so that this page and `fw-helperctl battery` need no D-Bus round trip and work with
//! the daemon stopped. All the arithmetic is `fw_helper_core::battery_log`; this file
//! only lays it out. Refreshes every few seconds while the page is on screen, and does
//! nothing at all while it is not.

use crate::chart::Palette;
use adw::prelude::*;
use fw_helper_core::battery_log::{
    history, last_charged_to, last_discharge, log_path, parse, spans, BatteryState, Entry, Event,
    Period, Segment, Share, Span, Tier,
};
use fw_helper_core::{Monitor, Sysfs};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const REFRESH_SECS: u32 = 5;
/// Days shown in the Days table. The log holds more; a month of rows is what fits on a
/// screen and answers "what did this week look like".
const DAYS_SHOWN: usize = 31;

fn utc_offset(t: u64) -> i64 {
    gtk::glib::DateTime::from_unix_local(t as i64)
        .map(|d| d.utc_offset().as_seconds())
        .unwrap_or(0)
}

fn local(t: u64, fmt: &str) -> String {
    gtk::glib::DateTime::from_unix_local(t as i64)
        .ok()
        .and_then(|d| d.format(fmt).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2 h 14 min`, `14 min`, `40 s`.
fn span(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h >= 48 {
        return format!("{} d {} h", h / 24, h % 24);
    }
    match (h, m) {
        (0, 0) => format!("{secs} s"),
        (0, m) => format!("{m} min"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// `1 h 05` - compact, for table cells.
fn hm(secs: u64) -> String {
    if secs == 0 {
        return "\u{2014}".into();
    }
    format!("{}:{:02}", secs / 3600, secs % 3600 / 60)
}

fn watts(s: &Share) -> String {
    s.watts()
        .map(|w| format!("{w:.1} W"))
        .unwrap_or_else(|| "\u{2014}".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Day,
    Week,
}

/// What the chart draws, captured at refresh so drawing is a pure function of it.
#[derive(Default)]
struct ChartData {
    spans: Vec<Span>,
    from: u64,
    to: u64,
}

pub struct BatteryPage {
    pub widget: gtk::Widget,
    status_row: adw::ActionRow,
    left_row: adw::ActionRow,
    charged_row: adw::ActionRow,
    health_row: adw::ActionRow,
    discharge_group: adw::PreferencesGroup,
    discharge_grid: RefCell<gtk::Grid>,
    chart: gtk::DrawingArea,
    chart_data: Rc<RefCell<ChartData>>,
    range: Cell<Range>,
    tier: Cell<Tier>,
    history_box: gtk::Box,
    footer: gtk::Label,
}

fn cell(text: &str, xalign: f32, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(xalign)
        .css_classes(classes.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .build()
}

fn toggle_pair(labels: &[&str]) -> (gtk::Box, Vec<gtk::ToggleButton>) {
    let bx = gtk::Box::builder()
        .css_classes(["linked"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build();
    let mut buttons: Vec<gtk::ToggleButton> = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        let b = gtk::ToggleButton::builder()
            .label(*l)
            .active(i == 0)
            .build();
        if let Some(first) = buttons.first() {
            b.set_group(Some(first));
        }
        bx.append(&b);
        buttons.push(b);
    }
    (bx, buttons)
}

impl BatteryPage {
    pub fn new() -> Rc<Self> {
        let status_row = adw::ActionRow::builder().title("Battery").build();
        let left_row = adw::ActionRow::builder()
            .title("Time left")
            .subtitle("at this discharge's screen-on average")
            .visible(false)
            .build();
        let charged_row = adw::ActionRow::builder().title("Last charged to").build();
        let health_row = adw::ActionRow::builder().title("Health").build();
        let now_group = adw::PreferencesGroup::builder().title("Now").build();
        for r in [&status_row, &left_row, &charged_row, &health_row] {
            now_group.add(r);
        }

        let discharge_group = adw::PreferencesGroup::builder()
            .title("This discharge")
            .description("where the charge went, by what the screen was doing")
            .build();
        let discharge_grid = gtk::Grid::builder()
            .column_spacing(24)
            .row_spacing(6)
            .margin_top(6)
            .build();
        discharge_group.add(&discharge_grid);

        let chart = gtk::DrawingArea::builder()
            .content_height(170)
            .hexpand(true)
            .css_classes(["card"])
            .build();
        let (range_box, range_buttons) = toggle_pair(&["24 hours", "7 days"]);
        let chart_group = adw::PreferencesGroup::builder()
            .title("Charge")
            .header_suffix(&range_box)
            .build();
        let legend = cell(
            "shaded: green on mains \u{00b7} light tint screen off \u{00b7} heavier tint asleep \u{00b7} \
             a break in the line is time with no daemon running",
            0.0,
            &["dim-label", "caption"],
        );
        legend.set_wrap(true);
        let chart_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        chart_box.append(&chart);
        chart_box.append(&legend);
        chart_group.add(&chart_box);

        let (tier_box, tier_buttons) = toggle_pair(&["Days", "Months", "Years"]);
        let history_group = adw::PreferencesGroup::builder()
            .title("History")
            .description("time and energy on battery; average draw is energy over time")
            .header_suffix(&tier_box)
            .build();
        let history_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        // The table is wider than a phone-width window; scroll it rather than squash it.
        let history_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&history_box)
            .build();
        history_group.add(&history_scroll);

        let footer = cell("", 0.0, &["dim-label", "caption"]);
        footer.set_wrap(true);

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        for w in [
            now_group.upcast_ref::<gtk::Widget>(),
            discharge_group.upcast_ref(),
            chart_group.upcast_ref(),
            history_group.upcast_ref(),
            footer.upcast_ref(),
        ] {
            column.append(w);
        }
        let clamp = adw::Clamp::builder()
            .maximum_size(1000)
            .child(&column)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let chart_data = Rc::new(RefCell::new(ChartData::default()));
        {
            let data = Rc::clone(&chart_data);
            chart.set_draw_func(move |_, cr, w, h| {
                let palette = Palette::for_theme(adw::StyleManager::default().is_dark());
                draw_chart(cr, w, h, &data.borrow(), palette);
            });
        }

        let page = Rc::new(Self {
            widget: scroll.upcast(),
            status_row,
            left_row,
            charged_row,
            health_row,
            discharge_group,
            discharge_grid: RefCell::new(discharge_grid),
            chart,
            chart_data,
            range: Cell::new(Range::Day),
            tier: Cell::new(Tier::Day),
            history_box,
            footer,
        });

        for (b, r) in range_buttons.iter().zip([Range::Day, Range::Week]) {
            let this = Rc::clone(&page);
            b.connect_toggled(move |b| {
                if b.is_active() {
                    this.range.set(r);
                    this.refresh();
                }
            });
        }
        for (b, t) in tier_buttons
            .iter()
            .zip([Tier::Day, Tier::Month, Tier::Year])
        {
            let this = Rc::clone(&page);
            b.connect_toggled(move |b| {
                if b.is_active() {
                    this.tier.set(t);
                    this.refresh();
                }
            });
        }

        // Refresh on becoming visible, and on a timer only while visible: a hidden
        // ViewStack page is unmapped, so `is_mapped` is exactly "someone can see it".
        {
            let this = Rc::clone(&page);
            page.widget.connect_map(move |_| this.refresh());
        }
        {
            let weak = Rc::downgrade(&page);
            gtk::glib::timeout_add_seconds_local(REFRESH_SECS, move || match weak.upgrade() {
                Some(p) => {
                    if p.widget.is_mapped() {
                        p.refresh();
                    }
                    gtk::glib::ControlFlow::Continue
                }
                None => gtk::glib::ControlFlow::Break,
            });
        }
        page
    }

    fn refresh(&self) {
        let path = log_path();
        let now = now_secs();
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(e) => {
                self.status_row.set_subtitle(&format!(
                    "No battery log at {} ({e}). fw-helperd 0.6.5 or later writes it.",
                    path.display()
                ));
                return;
            }
        };

        // The live reading closes the last interval, so the current discharge and the
        // chart run right up to now rather than to the last 1 % step.
        let fs = Sysfs::new("/");
        let t = Monitor::new(fs.clone()).sample();
        let live = Entry {
            unix_time: now,
            event: Event::Heartbeat,
            state: BatteryState {
                on_ac: t.on_ac,
                // Someone is looking at this page.
                screen_on: Some(true),
                status: t.battery_status.clone(),
                percent: t.battery_percent,
                wh: t.battery_wh,
                wh_full: t.battery_wh_full,
                cycles: None,
            },
        };

        self.refresh_now(&entries, &live, &t, &fs, now);
        self.refresh_discharge(&entries, &live, now);

        let from = match self.range.get() {
            Range::Day => now.saturating_sub(86_400),
            Range::Week => now.saturating_sub(7 * 86_400),
        };
        *self.chart_data.borrow_mut() = ChartData {
            spans: spans(&entries, Some(&live))
                .into_iter()
                .filter(|s| s.to > from)
                .collect(),
            from,
            to: now,
        };
        self.chart.queue_draw();

        self.refresh_history(&entries, &live);

        let since = entries.first().map(|e| e.unix_time).unwrap_or(now);
        self.footer.set_label(&format!(
            "Logging since {} \u{00b7} {} lines \u{00b7} {} \u{00b7} updated {}",
            local(since, "%a %-d %b %H:%M"),
            entries.len(),
            path.display(),
            local(now, "%H:%M:%S"),
        ));
    }

    fn refresh_now(
        &self,
        entries: &[Entry],
        live: &Entry,
        t: &fw_helper_core::Telemetry,
        fs: &Sysfs,
        now: u64,
    ) {
        let pct = t
            .battery_percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "?".into());
        let status = t.battery_status.clone().unwrap_or_default();
        let (title, subtitle) = match t.on_ac {
            Some(false) => {
                let d = last_discharge(entries, Some(live));
                let dur = d
                    .as_ref()
                    .filter(|d| d.ongoing)
                    .map(|d| format!(" for {}", span(d.to - d.from)))
                    .unwrap_or_default();
                (
                    format!("{pct}, on battery{dur}"),
                    t.system_watts
                        .map(|w| format!("drawing {w:.1} W right now"))
                        .unwrap_or_default(),
                )
            }
            _ => (
                format!("{pct}, on mains"),
                // "Not charging" on mains is the charge limit holding, which is the
                // one reading people tend to mistake for a fault.
                match status.as_str() {
                    "Not charging" => "not charging: the charge limit is holding".into(),
                    "Discharging" => {
                        "discharging on mains: settling down to the charge limit".into()
                    }
                    s => s.to_lowercase(),
                },
            ),
        };
        self.status_row.set_title(&title);
        self.status_row.set_subtitle(&subtitle);

        let left = last_discharge(entries, Some(live))
            .filter(|d| d.ongoing)
            .and_then(|d| d.screen_on.watts())
            .filter(|w| *w > 0.5)
            .zip(t.battery_wh);
        match left {
            Some((w, wh)) => {
                self.left_row.set_visible(true);
                self.left_row
                    .set_title(&format!("About {} left", span((wh / w * 3600.0) as u64)));
                self.left_row.set_subtitle(&format!(
                    "{wh:.1} Wh at this discharge's screen-on average of {w:.1} W"
                ));
            }
            None => self.left_row.set_visible(false),
        }

        match last_charged_to(entries) {
            Some((when, p)) => {
                self.charged_row.set_title(&format!("Last charged to {p}%"));
                self.charged_row.set_subtitle(&format!(
                    "unplugged {} ({} ago)",
                    local(when, "%a %-d %b %H:%M"),
                    span(now.saturating_sub(when))
                ));
            }
            None => {
                self.charged_row.set_title("Last charged to");
                self.charged_row.set_subtitle("no unplug logged yet");
            }
        }

        let bat = std::fs::read_dir(fs.path("sys/class/power_supply"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| format!("sys/class/power_supply/{}", e.file_name().to_string_lossy()))
            .find(|rel| fs.read_string(&format!("{rel}/type")).ok().as_deref() == Some("Battery"));
        let mut parts = Vec::new();
        if let Some(rel) = bat {
            let r = |f: &str| fs.read_u64(&format!("{rel}/{f}")).ok();
            let pair = match (r("charge_full"), r("charge_full_design")) {
                (Some(f), Some(d)) => Some((f, d)),
                _ => r("energy_full").zip(r("energy_full_design")),
            };
            if let Some((f, d)) = pair.filter(|(_, d)| *d > 0) {
                parts.push(format!(
                    "{:.0}% of design capacity",
                    f as f64 * 100.0 / d as f64
                ));
            }
            if let Some(c) = r("cycle_count") {
                parts.push(format!("{c} cycles"));
            }
        }
        if let Some(full) = t.battery_wh_full {
            parts.push(format!("{full:.1} Wh when full"));
        }
        self.health_row.set_title("Health");
        self.health_row.set_subtitle(&parts.join(" \u{00b7} "));
    }

    fn refresh_discharge(&self, entries: &[Entry], live: &Entry, now: u64) {
        // Rebuilt rather than updated: four rows, every few seconds, while visible.
        let grid = gtk::Grid::builder()
            .column_spacing(24)
            .row_spacing(6)
            .margin_top(6)
            .build();
        let Some(d) = last_discharge(entries, Some(live)) else {
            self.discharge_group.set_title("On battery");
            grid.attach(
                &cell("Nothing logged on battery yet.", 0.0, &["dim-label"]),
                0,
                0,
                4,
                1,
            );
            self.swap_grid(grid);
            return;
        };
        let pct = |p: Option<u64>| p.map(|p| format!("{p}%")).unwrap_or("?".into());
        if d.ongoing {
            self.discharge_group.set_title("This discharge");
            self.discharge_group.set_description(Some(&format!(
                "since {} \u{00b7} {} \u{2192} {}",
                local(d.from, "%H:%M"),
                pct(d.start_percent),
                pct(d.end_percent)
            )));
        } else {
            self.discharge_group.set_title("Last discharge");
            self.discharge_group.set_description(Some(&format!(
                "{} \u{2013} {} ({} ago) \u{00b7} {} \u{2192} {}",
                local(d.from, "%a %H:%M"),
                local(d.to, "%H:%M"),
                span(now.saturating_sub(d.to)),
                pct(d.start_percent),
                pct(d.end_percent)
            )));
        }
        for (i, h) in ["", "time", "energy", "average"].iter().enumerate() {
            grid.attach(
                &cell(h, if i == 0 { 0.0 } else { 1.0 }, &["dim-label", "caption"]),
                i as i32,
                0,
                1,
                1,
            );
        }
        let total = d.total();
        let rows: [(&str, &Share); 4] = [
            ("Screen on", &d.screen_on),
            ("Screen off", &d.screen_off),
            ("Asleep", &d.asleep),
            ("Total", &total),
        ];
        for (r, (name, s)) in rows.iter().enumerate() {
            let classes: &[&str] = if *name == "Total" { &["heading"] } else { &[] };
            let row = r as i32 + 1;
            grid.attach(&cell(name, 0.0, classes), 0, row, 1, 1);
            grid.attach(&cell(&span(s.secs), 1.0, &["numeric"]), 1, row, 1, 1);
            grid.attach(
                &cell(&format!("{:.1} Wh", s.wh), 1.0, &["numeric"]),
                2,
                row,
                1,
                1,
            );
            grid.attach(&cell(&watts(s), 1.0, &["numeric"]), 3, row, 1, 1);
        }
        if d.unknown_secs > 0 {
            grid.attach(
                &cell(
                    &format!(
                        "{} with no daemon running is left out",
                        span(d.unknown_secs)
                    ),
                    0.0,
                    &["dim-label", "caption"],
                ),
                0,
                6,
                4,
                1,
            );
        }
        self.swap_grid(grid);
    }

    fn swap_grid(&self, grid: gtk::Grid) {
        let old = self.discharge_grid.replace(grid.clone());
        self.discharge_group.remove(&old);
        self.discharge_group.add(&grid);
    }

    fn refresh_history(&self, entries: &[Entry], live: &Entry) {
        while let Some(child) = self.history_box.first_child() {
            self.history_box.remove(&child);
        }
        let mut periods: Vec<Period> = history(entries, Some(live), self.tier.get(), &utc_offset);
        // A period spent entirely on mains is a row of dashes; it says nothing the
        // absence of a row does not.
        periods.retain(|p| p.on_battery().secs > 0 || p.unplugs > 0);
        if self.tier.get() == Tier::Day {
            periods.truncate(DAYS_SHOWN);
        }
        let grid = gtk::Grid::builder()
            .column_spacing(18)
            .row_spacing(6)
            .margin_top(6)
            .build();
        let heads = [
            "",
            "on battery",
            "screen on",
            "screen off",
            "asleep",
            "used",
            "avg on",
            "avg off",
            "unplugs",
            "charged to",
        ];
        for (i, h) in heads.iter().enumerate() {
            grid.attach(
                &cell(h, if i == 0 { 0.0 } else { 1.0 }, &["dim-label", "caption"]),
                i as i32,
                0,
                1,
                1,
            );
        }
        let today = self.tier.get() == Tier::Day;
        for (r, p) in periods.iter().enumerate() {
            let label = if today {
                // A weekday reads faster than a date for the last week or two.
                let noon = p.key;
                let dt = gtk::glib::DateTime::from_local(
                    noon.year as i32,
                    noon.month as i32,
                    noon.day as i32,
                    12,
                    0,
                    0.0,
                )
                .ok()
                .and_then(|d| d.format("%a %-d %b").ok())
                .map(|s| s.to_string());
                dt.unwrap_or_else(|| p.key.label())
            } else {
                p.key.label()
            };
            let total = p.on_battery();
            let charged = p
                .average_charged_to()
                .map(|c| format!("{c:.0}%"))
                .unwrap_or_else(|| "\u{2014}".into());
            let cells = [
                hm(total.secs),
                hm(p.screen_on.secs),
                hm(p.screen_off.secs),
                hm(p.asleep.secs),
                if total.secs > 0 {
                    format!("{:.1} Wh", total.wh)
                } else {
                    "\u{2014}".into()
                },
                watts(&p.screen_on),
                watts(&p.screen_off),
                if p.unplugs > 0 {
                    p.unplugs.to_string()
                } else {
                    "\u{2014}".into()
                },
                charged,
            ];
            let row = r as i32 + 1;
            grid.attach(&cell(&label, 0.0, &[]), 0, row, 1, 1);
            for (c, text) in cells.iter().enumerate() {
                grid.attach(&cell(text, 1.0, &["numeric"]), c as i32 + 1, row, 1, 1);
            }
        }
        if periods.is_empty() {
            grid.attach(
                &cell("Nothing logged yet.", 0.0, &["dim-label"]),
                0,
                1,
                10,
                1,
            );
        }
        self.history_box.append(&grid);
    }
}

// ---- chart --------------------------------------------------------------------------

const PAD_L: f64 = 38.0;
const PAD_R: f64 = 10.0;
const PAD_T: f64 = 10.0;
const PAD_B: f64 = 20.0;

fn draw_chart(cr: &gtk::cairo::Context, w: i32, h: i32, d: &ChartData, p: Palette) {
    let (w, h) = (f64::from(w), f64::from(h));
    let (pw, ph) = (w - PAD_L - PAD_R, h - PAD_T - PAD_B);
    if pw <= 0.0 || ph <= 0.0 || d.to <= d.from {
        return;
    }
    let range = (d.to - d.from) as f64;
    let x = |t: u64| PAD_L + (t.max(d.from) - d.from) as f64 / range * pw;
    let y = |pct: f64| PAD_T + (1.0 - pct / 100.0) * ph;
    let (ir, ig, ib) = p.ink;

    // State shading first, under everything.
    for s in &d.spans {
        let alpha_colour = match s.segment {
            Segment::Ac => Some((p.power, 0.18)),
            Segment::ScreenOff => Some((p.ink, 0.07)),
            Segment::Asleep => Some((p.ink, 0.16)),
            Segment::ScreenOn | Segment::Gap => None,
        };
        if let Some(((r, g, b), a)) = alpha_colour {
            cr.set_source_rgba(r, g, b, a);
            let (x0, x1) = (x(s.from), x(s.to));
            cr.rectangle(x0, PAD_T, (x1 - x0).max(0.5), ph);
            let _ = cr.fill();
        }
    }

    // Grid: 0, 50, 100 %.
    cr.set_line_width(1.0);
    cr.set_font_size(10.0);
    for pct in [0.0, 50.0, 100.0] {
        cr.set_source_rgba(ir, ig, ib, 0.12);
        cr.move_to(PAD_L, y(pct).round() + 0.5);
        cr.line_to(PAD_L + pw, y(pct).round() + 0.5);
        let _ = cr.stroke();
        cr.set_source_rgba(ir, ig, ib, 0.55);
        let label = format!("{pct:.0}%");
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(PAD_L - 6.0 - ext.width(), y(pct) + ext.height() / 2.0);
            let _ = cr.show_text(&label);
        }
    }

    // Time ticks: every 6 h for a day, every local midnight for a week.
    let step: u64 = if range <= 2.0 * 86_400.0 {
        6 * 3600
    } else {
        86_400
    };
    let off = utc_offset(d.to);
    let first = ((d.from as i64 + off) / step as i64 + 1) * step as i64 - off;
    let mut t = first.max(0) as u64;
    while t < d.to {
        let xt = x(t).round() + 0.5;
        cr.set_source_rgba(ir, ig, ib, 0.10);
        cr.move_to(xt, PAD_T);
        cr.line_to(xt, PAD_T + ph);
        let _ = cr.stroke();
        let label = if step == 86_400 {
            local(t, "%a")
        } else {
            local(t, "%H:%M")
        };
        cr.set_source_rgba(ir, ig, ib, 0.55);
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(xt - ext.width() / 2.0, h - 6.0);
            let _ = cr.show_text(&label);
        }
        t += step;
    }

    // The charge. Linear within an interval - the log records each 1 % step, so the
    // straight line between two lines is never more than a percent off. Broken across
    // gaps, where nothing is known.
    let (br, bg, bb) = p.battery;
    cr.set_source_rgba(br, bg, bb, 1.0);
    cr.set_line_width(2.0);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    let mut drawing = false;
    for s in &d.spans {
        let (Some(a), Some(b)) = (s.percent_from, s.percent_to) else {
            drawing = false;
            continue;
        };
        if s.segment == Segment::Gap {
            drawing = false;
            continue;
        }
        // Clip an interval that starts before the window to where it enters.
        let (t0, mut p0) = (s.from.max(d.from), a as f64);
        if s.from < d.from && s.to > s.from {
            let f = (d.from - s.from) as f64 / (s.to - s.from) as f64;
            p0 = a as f64 + (b as f64 - a as f64) * f;
        }
        if !drawing {
            cr.move_to(x(t0), y(p0));
            drawing = true;
        } else {
            cr.line_to(x(t0), y(p0));
        }
        cr.line_to(x(s.to), y(b as f64));
    }
    let _ = cr.stroke();
}
