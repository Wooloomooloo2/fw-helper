//! Stacked strip charts: what the machine did, over time.
//!
//! Five plots sharing one time axis rather than one plot with five scales. That is not
//! a layout preference — power in watts, temperature in Celsius and load in percent have
//! no common scale, and putting two of them on one pair of axes invents a correlation
//! the data does not contain. Small multiples are the fix.
//!
//! ## How the power strip answers the question this feature exists for
//!
//! There is no readable flag for "the CPU package is power limited". The GPU publishes
//! real throttle reasons (`reason_pl1` and friends) but the package publishes none. So
//! the PL1 setpoint is drawn *over* the power trace as a threshold line: draw sitting on
//! the line **is** PL1 binding, and reading that off the graph is the same inference
//! `scripts/sustained-perf-test.sh` makes in prose when it says the limit is the binding
//! constraint. It is also how the 35 W ceiling was found in the first place.
//!
//! ## Colour
//!
//! Two palettes, one per theme, selected rather than flipped — `adw::StyleManager` says
//! which. Both were checked with the data-viz validator on the sets actually used
//! together: worst all-pairs CVD ΔE 13.0 light / 13.2 dark against a floor of 8, and
//! normal-vision 19.6 / 19.3 against a floor of 15. Magenta and yellow fall below 3:1
//! contrast on the light surface, so every series is named with its value in the strip
//! header — identity is never carried by colour alone.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// One plotted moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Seconds from the start of what is being shown.
    pub t: f64,
    pub cpu_pct: Option<f64>,
    pub gpu_pct: Option<f64>,
    pub mem_pct: Option<f64>,
    pub package_w: Option<f64>,
    /// The limit in force at this moment, drawn as the threshold the trace is read
    /// against. Per sample rather than per session because it can change mid-run.
    pub pl1_w: Option<f64>,
    pub cpu_c: Option<f64>,
    pub battery_c: Option<f64>,
    pub board_c: Option<f64>,
    pub fan_rpm: Option<f64>,
    /// Anything was being held back this second: CPU thermal events, or a GPU throttle
    /// reason. Drawn as a mark rather than a series, because it is an event.
    pub throttled: bool,
}

impl Sample {
    /// The live case: one tick of what the daemon is publishing.
    pub fn from_snapshot(t: f64, s: &fw_helper_client::Snapshot) -> Self {
        let temp = |label: &str| s.temps.iter().find(|r| r.label == label).map(|r| r.celsius);
        // The board sensors are named after chip part numbers, so they are found by
        // excluding the ones we do know - the same reasoning the daemon uses.
        let board = s
            .temps
            .iter()
            .filter(|r| {
                let l = r.label.as_str();
                l != fw_helper_core::PACKAGE_TEMP_LABEL
                    && !l.starts_with("peci")
                    && !l.starts_with("battery")
            })
            .map(|r| r.celsius)
            .max_by(f64::total_cmp);

        Self {
            t,
            cpu_pct: s.load.cpu_percent,
            gpu_pct: s.load.gpu_percent,
            mem_pct: s.load.mem_percent(),
            package_w: s.package_watts,
            pl1_w: s.power_limit.map(f64::from),
            // Prefer the coretemp package sensor, whose critical point really is Tjmax;
            // fall back to whatever the daemon nominates as the control sensor.
            cpu_c: temp(fw_helper_core::PACKAGE_TEMP_LABEL).or_else(|| {
                s.control_sensor
                    .as_ref()
                    .and_then(|label| temp(label.as_str()))
            }),
            battery_c: s
                .temps
                .iter()
                .find(|r| r.label.starts_with("battery"))
                .map(|r| r.celsius),
            board_c: board,
            fan_rpm: s.fan_rpm.map(|v| v as f64),
            throttled: s.load.throttled(),
        }
    }

    /// The historical case: one row of a recorded session.
    pub fn from_row(r: &fw_helper_core::Row) -> Self {
        Self {
            t: r.t_s as f64,
            cpu_pct: r.cpu_pct,
            gpu_pct: r.gpu_pct,
            mem_pct: match (r.mem_used_mb, r.mem_total_mb) {
                (Some(u), Some(t)) if t > 0 => Some(u as f64 * 100.0 / t as f64),
                _ => None,
            },
            package_w: r.package_w,
            pl1_w: r.pl1_w.map(f64::from),
            // The recorded file keeps both; coretemp is the one with a usable limit.
            cpu_c: r.coretemp_c.or(r.peci_c),
            battery_c: r.battery_c,
            board_c: r.board_c,
            fan_rpm: r.fan_rpm.map(|v| v as f64),
            throttled: r.throttle_events > 0 || r.gpu_throttle.is_some(),
        }
    }
}

/// Colours, chosen per theme rather than derived by flipping one set.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    ink: (f64, f64, f64),
    cpu: (f64, f64, f64),
    gpu: (f64, f64, f64),
    power: (f64, f64, f64),
    battery: (f64, f64, f64),
    board: (f64, f64, f64),
    fan: (f64, f64, f64),
    memory: (f64, f64, f64),
    /// Reserved for throttling, which is a state and not an identity.
    alert: (f64, f64, f64),
    /// The window background this is drawn on. Only used to punch a hole behind a
    /// threshold label so it stays readable where it crosses a series.
    surface: (f64, f64, f64),
}

const fn rgb(hex: u32) -> (f64, f64, f64) {
    (
        ((hex >> 16) & 0xff) as f64 / 255.0,
        ((hex >> 8) & 0xff) as f64 / 255.0,
        (hex & 0xff) as f64 / 255.0,
    )
}

impl Palette {
    pub fn for_theme(dark: bool) -> Self {
        if dark {
            Self {
                ink: rgb(0xffffff),
                cpu: rgb(0x3987e5),
                gpu: rgb(0xd95926),
                power: rgb(0x199e70),
                battery: rgb(0xd55181),
                board: rgb(0xc98500),
                fan: rgb(0x9085e9),
                memory: rgb(0x5aa95a),
                alert: rgb(0xe66767),
                surface: rgb(0x242424),
            }
        } else {
            Self {
                ink: rgb(0x0b0b0b),
                cpu: rgb(0x2a78d6),
                gpu: rgb(0xeb6834),
                power: rgb(0x1baf7a),
                battery: rgb(0xe87ba4),
                board: rgb(0xeda100),
                fan: rgb(0x4a3aa7),
                memory: rgb(0x008300),
                alert: rgb(0xe34948),
                surface: rgb(0xfafafa),
            }
        }
    }
}

/// Tjmax, where the CPU throttles to protect itself (`coretemp` crit). The usable
/// limit: `peci-temp` declares 119.8 °C, which is above Tjmax and cannot be a limit.
const TJMAX_C: f64 = 100.0;
/// The battery's critical temperature. Unlike the CPU it has no protection of its own.
const BATTERY_CRIT_C: f64 = 49.9;

const HEADER_H: f64 = 15.0;
const PLOT_H: f64 = 58.0;
const STRIP_GAP: f64 = 9.0;
const AXIS_H: f64 = 16.0;
const PAD_L: f64 = 38.0;
const PAD_R: f64 = 10.0;

fn strip_count() -> usize {
    5
}

/// Total height the widget needs.
pub fn natural_height() -> i32 {
    ((HEADER_H + PLOT_H + STRIP_GAP) * strip_count() as f64 + AXIS_H) as i32
}

/// How one series is read out of a sample.
type Accessor = fn(&Sample) -> Option<f64>;

/// One plotted series: its name, its swatch colour, and where its value comes from.
/// The colour never travels alone - the name is always beside it in the header.
type Series = (&'static str, (f64, f64, f64), Accessor);

/// Which series a strip draws, and how its axis is labelled.
struct Strip {
    title: &'static str,
    series: Vec<Series>,
    /// Fixed lower bound, and an upper bound that may grow to fit the data.
    y_min: f64,
    y_floor_max: f64,
    /// Rendered next to a value in the header and on the axis.
    unit: &'static str,
    /// Horizontal threshold lines: `(value, label)`. Dashed, because these genuinely
    /// are thresholds — which is exactly why the grid behind them is not.
    thresholds: Vec<(f64, String)>,
    /// Draw the "something was throttled" marks along this strip's baseline.
    mark_throttle: bool,
}

fn strips(p: &Palette, samples: &[Sample]) -> Vec<Strip> {
    // The setpoint in force at the end of the window: what the trace should be read
    // against right now.
    let pl1 = samples.iter().rev().find_map(|s| s.pl1_w);
    let mut power_thresholds = Vec::new();
    if let Some(w) = pl1 {
        power_thresholds.push((w, format!("PL1 {w:.0} W")));
    }

    vec![
        Strip {
            title: "load",
            series: vec![("cpu", p.cpu, |s| s.cpu_pct), ("gpu", p.gpu, |s| s.gpu_pct)],
            y_min: 0.0,
            y_floor_max: 100.0,
            unit: "%",
            thresholds: Vec::new(),
            mark_throttle: false,
        },
        Strip {
            title: "cpu package power",
            series: vec![("draw", p.power, |s| s.package_w)],
            y_min: 0.0,
            y_floor_max: 40.0,
            unit: "W",
            thresholds: power_thresholds,
            // Throttle events belong here: this is the strip where being held back is
            // the thing under discussion.
            mark_throttle: true,
        },
        Strip {
            title: "temperature",
            series: vec![
                ("cpu", p.cpu, |s| s.cpu_c),
                ("battery", p.battery, |s| s.battery_c),
                ("board", p.board, |s| s.board_c),
            ],
            y_min: 20.0,
            y_floor_max: TJMAX_C,
            unit: "\u{b0}C",
            thresholds: vec![
                (TJMAX_C, "Tjmax 100\u{b0}".to_string()),
                (BATTERY_CRIT_C, "battery crit 50\u{b0}".to_string()),
            ],
            mark_throttle: false,
        },
        Strip {
            title: "fan",
            series: vec![("speed", p.fan, |s| s.fan_rpm)],
            y_min: 0.0,
            y_floor_max: 5200.0,
            unit: "rpm",
            thresholds: Vec::new(),
            mark_throttle: false,
        },
        Strip {
            title: "memory",
            series: vec![("used", p.memory, |s| s.mem_pct)],
            y_min: 0.0,
            y_floor_max: 100.0,
            unit: "%",
            thresholds: Vec::new(),
            mark_throttle: false,
        },
    ]
}

/// A column of the plot after downsampling: the mean drawn as the line, the range kept
/// so a spike between pixels is still visible.
#[derive(Debug, Clone, Copy)]
struct Column {
    x: f64,
    mean: f64,
    min: f64,
    max: f64,
}

/// Reduce a series to at most one column per pixel.
///
/// Min and max are carried alongside the mean deliberately. A session is hours of
/// samples in a few hundred pixels, and taking every nth sample would drop exactly the
/// brief spikes — a throttle, a fan surge — that someone opens this view to find.
fn columns(
    samples: &[Sample],
    accessor: Accessor,
    t_min: f64,
    t_span: f64,
    x_of: impl Fn(f64) -> f64,
    width_px: f64,
) -> Vec<Column> {
    let buckets = (width_px.max(1.0) as usize).clamp(1, 4096);
    let mut acc: Vec<Option<(f64, u32, f64, f64)>> = vec![None; buckets];

    for s in samples {
        let Some(v) = accessor(s) else { continue };
        let frac = if t_span > 0.0 {
            (s.t - t_min) / t_span
        } else {
            0.0
        };
        let i = ((frac * (buckets - 1) as f64).round() as isize).clamp(0, buckets as isize - 1);
        let slot = &mut acc[i as usize];
        *slot = Some(match *slot {
            None => (v, 1, v, v),
            Some((sum, n, lo, hi)) => (sum + v, n + 1, lo.min(v), hi.max(v)),
        });
    }

    acc.into_iter()
        .enumerate()
        .filter_map(|(i, slot)| {
            let (sum, n, min, max) = slot?;
            let frac = i as f64 / (buckets - 1).max(1) as f64;
            Some(Column {
                x: x_of(t_min + frac * t_span),
                mean: sum / f64::from(n),
                min,
                max,
            })
        })
        .collect()
}

/// What the widget is currently showing.
#[derive(Default)]
pub struct ChartData {
    pub samples: Vec<Sample>,
    /// Shown when there is nothing to plot, so an empty chart explains itself instead
    /// of looking broken.
    pub placeholder: String,
}

pub struct ChartView {
    pub widget: gtk::DrawingArea,
    data: Rc<RefCell<ChartData>>,
}

impl ChartView {
    pub fn new() -> Rc<Self> {
        let data: Rc<RefCell<ChartData>> = Rc::new(RefCell::new(ChartData {
            samples: Vec::new(),
            placeholder: "waiting for the first sample\u{2026}".into(),
        }));

        let widget = gtk::DrawingArea::builder()
            .content_height(natural_height())
            .hexpand(true)
            .build();

        let for_draw = Rc::clone(&data);
        widget.set_draw_func(move |_area, cr, w, h| {
            // The theme decision is made here rather than inside `draw`, which keeps
            // the drawing itself free of any GTK state - so it can be rendered to an
            // image surface and looked at without a display.
            let palette = Palette::for_theme(adw::StyleManager::default().is_dark());
            draw(cr, w, h, &for_draw.borrow(), palette);
        });

        Rc::new(Self { widget, data })
    }

    pub fn set_samples(&self, samples: Vec<Sample>) {
        {
            let mut d = self.data.borrow_mut();
            d.samples = samples;
        }
        self.widget.queue_draw();
    }

    pub fn set_placeholder(&self, text: &str) {
        self.data.borrow_mut().placeholder = text.to_string();
        self.widget.queue_draw();
    }
}

pub fn draw(cr: &gtk::cairo::Context, w: i32, h: i32, d: &ChartData, p: Palette) {
    let (w, h) = (f64::from(w), f64::from(h));
    let (r, g, b) = p.ink;

    cr.select_font_face(
        "sans-serif",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    cr.set_font_size(10.0);
    cr.set_line_width(1.0);

    if d.samples.is_empty() {
        cr.set_source_rgba(r, g, b, 0.45);
        cr.move_to(PAD_L, h / 2.0);
        let _ = cr.show_text(&d.placeholder);
        return;
    }

    let t_min = d.samples.first().map(|s| s.t).unwrap_or(0.0);
    let t_max = d.samples.last().map(|s| s.t).unwrap_or(1.0);
    let t_span = (t_max - t_min).max(1.0);
    let plot_w = (w - PAD_L - PAD_R).max(1.0);
    let x_of = move |t: f64| PAD_L + ((t - t_min) / t_span).clamp(0.0, 1.0) * plot_w;

    let all = strips(&p, &d.samples);
    let mut top = 0.0;
    for strip in &all {
        draw_strip(cr, &p, strip, d, top, w, &x_of, t_min, t_span, plot_w);
        top += HEADER_H + PLOT_H + STRIP_GAP;
    }
    draw_time_axis(cr, &p, top, w, t_min, t_max, &x_of);
}

#[allow(clippy::too_many_arguments)]
fn draw_strip(
    cr: &gtk::cairo::Context,
    p: &Palette,
    strip: &Strip,
    d: &ChartData,
    top: f64,
    w: f64,
    x_of: &impl Fn(f64) -> f64,
    t_min: f64,
    t_span: f64,
    plot_w: f64,
) {
    let (r, g, b) = p.ink;
    let plot_top = top + HEADER_H;
    let plot_bottom = plot_top + PLOT_H;

    // Upper bound: the strip's natural ceiling, raised only if the data actually goes
    // above it. Inflating unconditionally is what turns a percentage axis into 0-105
    // and a fan axis into 5.5k, so the headroom is added only where it is needed.
    let mut y_max: f64 = strip.y_floor_max;
    let mut data_max = f64::MIN;
    for (_, _, accessor) in &strip.series {
        for s in &d.samples {
            if let Some(v) = accessor(s) {
                data_max = data_max.max(v);
            }
        }
    }
    if data_max > y_max {
        y_max = data_max * 1.05;
    }
    // A threshold must never be drawn off the top of its own strip.
    for (value, _) in &strip.thresholds {
        y_max = y_max.max(*value);
    }
    let y_span = (y_max - strip.y_min).max(1.0);
    let y_of = |v: f64| plot_bottom - ((v - strip.y_min) / y_span).clamp(0.0, 1.0) * PLOT_H;

    // --- header: the title, then a swatch, name and current value per series.
    cr.set_source_rgba(r, g, b, 0.55);
    cr.move_to(PAD_L, top + 10.0);
    let _ = cr.show_text(strip.title);

    // Laid out right to left so the values line up with the plot's right edge, which is
    // where the most recent sample is.
    let mut cursor = w - PAD_R;
    for &(name, colour, accessor) in strip.series.iter().rev() {
        let latest = d.samples.iter().rev().find_map(accessor);
        let text = match latest {
            Some(v) if strip.unit == "rpm" => format!("{name} {v:.0} {}", strip.unit),
            Some(v) => format!("{name} {v:.0}{}", strip.unit),
            None => format!("{name} \u{2013}"),
        };
        let width = cr.text_extents(&text).map(|e| e.width()).unwrap_or(60.0);
        cursor -= width;
        // Text in ink, never in the series colour: the swatch beside it carries identity.
        cr.set_source_rgba(r, g, b, 0.75);
        cr.move_to(cursor, top + 10.0);
        let _ = cr.show_text(&text);
        cursor -= 6.0;
        cr.set_source_rgb(colour.0, colour.1, colour.2);
        cr.rectangle(cursor - 6.0, top + 3.5, 6.0, 6.0);
        let _ = cr.fill();
        cursor -= 14.0;
    }

    // --- grid: solid hairlines, one shade off the surface. Never dashed, so that
    // dashing is left to mean "threshold" and nothing else.
    cr.set_line_width(1.0);
    for step in 0..=2 {
        let v = strip.y_min + y_span * f64::from(step) / 2.0;
        let y = y_of(v).round() + 0.5;
        cr.set_source_rgba(r, g, b, 0.09);
        cr.move_to(PAD_L, y);
        cr.line_to(w - PAD_R, y);
        let _ = cr.stroke();
        if step > 0 {
            cr.set_source_rgba(r, g, b, 0.40);
            cr.move_to(4.0, y + 3.0);
            let _ = cr.show_text(&format_tick(v));
        }
    }

    // --- thresholds, dashed and labelled.
    //
    // Labels sit at the RIGHT edge. The strip title is at the left, and a threshold
    // near the top of its strip - which is the common case, since a limit is usually
    // close to the ceiling - put its label straight through that title.
    for (value, label) in &strip.thresholds {
        let y = y_of(*value).round() + 0.5;
        cr.set_source_rgba(r, g, b, 0.55);
        cr.set_dash(&[4.0, 3.0], 0.0);
        cr.move_to(PAD_L, y);
        cr.line_to(w - PAD_R, y);
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);

        let width = cr.text_extents(label).map(|e| e.width()).unwrap_or(50.0);
        // Above the line normally; below it when the line is too near the top of the
        // plot for a label to fit above without leaving the strip.
        let label_y = if y - plot_top < 11.0 {
            y + 10.0
        } else {
            y - 3.0
        };
        cr.move_to(w - PAD_R - width, label_y);
        let _ = cr.show_text(label);
    }

    // --- series.
    for (_, colour, accessor) in &strip.series {
        let cols = columns(&d.samples, *accessor, t_min, t_span, x_of, plot_w);
        if cols.is_empty() {
            continue;
        }
        // The range within each pixel column, so a spike narrower than a pixel is still
        // on screen rather than averaged away.
        cr.set_source_rgba(colour.0, colour.1, colour.2, 0.30);
        cr.set_line_width(1.0);
        for c in &cols {
            if c.max - c.min > f64::EPSILON {
                cr.move_to(c.x.round() + 0.5, y_of(c.min));
                cr.line_to(c.x.round() + 0.5, y_of(c.max));
            }
        }
        let _ = cr.stroke();

        cr.set_source_rgb(colour.0, colour.1, colour.2);
        cr.set_line_width(2.0);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        for (i, c) in cols.iter().enumerate() {
            if i == 0 {
                cr.move_to(c.x, y_of(c.mean));
            } else {
                cr.line_to(c.x, y_of(c.mean));
            }
        }
        let _ = cr.stroke();
    }

    // --- threshold labels, last of all.
    //
    // At the right edge, because the strip title is at the left and a limit usually
    // sits near the top of its own strip - which put the label straight through the
    // title. Drawn after the series and on a punched-out surface patch, because the
    // right edge is exactly where the most recent data is and a label competing with a
    // line for the same pixels loses.
    for (value, label) in &strip.thresholds {
        let y = y_of(*value).round() + 0.5;
        let width = cr.text_extents(label).map(|e| e.width()).unwrap_or(50.0);
        // Above the line normally; below it when the line is too close to the top of
        // the plot for a label to fit above without leaving the strip.
        let label_y = if y - plot_top < 11.0 {
            y + 10.0
        } else {
            y - 3.0
        };
        let x = w - PAD_R - width;

        cr.set_source_rgb(p.surface.0, p.surface.1, p.surface.2);
        cr.rectangle(x - 3.0, label_y - 8.0, width + 6.0, 11.0);
        let _ = cr.fill();

        cr.set_source_rgba(r, g, b, 0.70);
        cr.move_to(x, label_y);
        let _ = cr.show_text(label);
    }

    // --- throttle events: a state, so it wears the reserved alert colour and is drawn
    // as a mark on the baseline rather than as another line.
    if strip.mark_throttle {
        cr.set_source_rgb(p.alert.0, p.alert.1, p.alert.2);
        cr.set_line_width(2.0);
        for s in d.samples.iter().filter(|s| s.throttled) {
            let x = x_of(s.t).round() + 0.5;
            cr.move_to(x, plot_bottom);
            cr.line_to(x, plot_bottom - 5.0);
        }
        let _ = cr.stroke();
    }
}

fn format_tick(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if v.fract().abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn draw_time_axis(
    cr: &gtk::cairo::Context,
    p: &Palette,
    top: f64,
    w: f64,
    t_min: f64,
    t_max: f64,
    x_of: &impl Fn(f64) -> f64,
) {
    let (r, g, b) = p.ink;
    cr.set_source_rgba(r, g, b, 0.45);
    for step in 0..=4 {
        let t = t_min + (t_max - t_min) * f64::from(step) / 4.0;
        let x = x_of(t);
        let label = format_elapsed(t - t_min);
        let width = cr.text_extents(&label).map(|e| e.width()).unwrap_or(20.0);
        // The first and last labels are pulled inside the plot so neither is clipped.
        let x = match step {
            0 => x,
            4 => x - width,
            _ => x - width / 2.0,
        };
        cr.move_to(x.clamp(2.0, w - width - 2.0), top + 8.0);
        let _ = cr.show_text(&label);
    }
}

/// Elapsed time, in the largest unit that stays readable.
fn format_elapsed(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=99 => format!("{s}s"),
        100..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}
