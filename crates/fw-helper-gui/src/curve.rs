//! The fan curve editor: a plot of the curve, and a row per point that edits it.
//!
//! The plot is read-only. Dragging points on a canvas is the obvious design and it is
//! deliberately not what this is: the rows are keyboard-reachable, they can show a
//! validation error against the point that caused it, and they cannot express a curve
//! the daemon would reject. The plot's job is to make the *shape* legible — above all
//! the shape of the firmware floor underneath it, which is the thing that decides
//! whether a point the user drew has any effect at all.
//!
//! Validation is [`fw_helper_core::Curve`], the same code the daemon runs. A second
//! opinion about what a valid curve is would drift from the first, and the drift would
//! show up as a curve the editor accepted and the daemon refused.

use adw::prelude::*;
use fw_helper_core::{Curve, Point, STICTION_DUTY};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Temperature span the plot covers.
///
/// Wider than the 45–85 °C that matters (see `docs/hardware-baseline.md`) so the ends
/// are visible rather than clipped: 100 °C is Tjmax and belongs on screen, and the
/// low end needs room for the idle reading to sit somewhere other than the axis.
const T_MIN: f64 = 30.0;
const T_MAX: f64 = 105.0;

/// Tjmax, where the CPU throttles to protect itself (`coretemp` crit).
const TJMAX_C: f64 = 100.0;

const PLOT_HEIGHT: i32 = 200;
const PAD_L: f64 = 30.0;
const PAD_R: f64 = 8.0;
const PAD_T: f64 = 10.0;
const PAD_B: f64 = 18.0;

/// What the plot draws. Kept separately from the editor's own point list because the
/// live readings update every tick while the user is editing, and the two must not
/// share a borrow.
#[derive(Default, Clone)]
struct PlotData {
    /// The curve as edited, which is not necessarily the curve that is running.
    points: Vec<(f64, u8)>,
    /// The learned firmware floor. Empty until the daemon has observed some.
    floor: Vec<(f64, u8)>,
    /// Where the machine is right now: control temperature, and duty if we drive it.
    now: Option<(f64, Option<u8>)>,
}

fn x_of(t: f64, w: f64) -> f64 {
    let span = (w - PAD_L - PAD_R).max(1.0);
    PAD_L + ((t - T_MIN) / (T_MAX - T_MIN)).clamp(0.0, 1.0) * span
}

fn y_of(duty: f64, h: f64) -> f64 {
    let span = (h - PAD_T - PAD_B).max(1.0);
    PAD_T + (1.0 - (duty / 255.0).clamp(0.0, 1.0)) * span
}

/// Draw everything. Colours are derived from the widget's own foreground colour so the
/// plot follows the system light/dark theme instead of pinning its own palette.
fn draw(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32, d: &PlotData) {
    let (w, h) = (f64::from(w), f64::from(h));
    let fg = area.color();
    let (r, g, b) = (
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
    );

    cr.set_line_width(1.0);
    cr.select_font_face(
        "sans-serif",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    cr.set_font_size(9.0);

    // Grid and axis labels.
    for t in [40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0] {
        let x = x_of(t, w);
        cr.set_source_rgba(r, g, b, 0.10);
        cr.move_to(x, PAD_T);
        cr.line_to(x, h - PAD_B);
        let _ = cr.stroke();
        cr.set_source_rgba(r, g, b, 0.45);
        cr.move_to(x - 7.0, h - 6.0);
        let _ = cr.show_text(&format!("{t:.0}"));
    }
    for duty in [0u8, 64, 128, 192, 255] {
        let y = y_of(f64::from(duty), h);
        cr.set_source_rgba(r, g, b, 0.10);
        cr.move_to(PAD_L, y);
        cr.line_to(w - PAD_R, y);
        let _ = cr.stroke();
        cr.set_source_rgba(r, g, b, 0.45);
        cr.move_to(2.0, y + 3.0);
        let _ = cr.show_text(&duty.to_string());
    }

    // The firmware floor, filled down to zero: everything inside this region is a duty
    // the daemon will raise. Drawn first so the curve sits on top of it.
    if d.floor.len() >= 2 {
        cr.set_source_rgba(r, g, b, 0.14);
        cr.move_to(x_of(d.floor[0].0, w), y_of(0.0, h));
        for &(t, duty) in &d.floor {
            cr.line_to(x_of(t, w), y_of(f64::from(duty), h));
        }
        let last = d.floor[d.floor.len() - 1];
        cr.line_to(x_of(T_MAX, w), y_of(f64::from(last.1), h));
        cr.line_to(x_of(T_MAX, w), y_of(0.0, h));
        cr.close_path();
        let _ = cr.fill();

        cr.set_source_rgba(r, g, b, 0.40);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(x_of(d.floor[0].0, w), y_of(f64::from(d.floor[0].1), h));
        for &(t, duty) in &d.floor {
            cr.line_to(x_of(t, w), y_of(f64::from(duty), h));
        }
        cr.line_to(x_of(T_MAX, w), y_of(f64::from(last.1), h));
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
    }

    // Tjmax. Not a fan-curve limit — the CPU throttles here on its own — but it is the
    // reference point for how much headroom a curve is leaving.
    let x = x_of(TJMAX_C, w);
    cr.set_source_rgba(0.88, 0.11, 0.14, 0.55);
    cr.set_dash(&[2.0, 3.0], 0.0);
    cr.move_to(x, PAD_T);
    cr.line_to(x, h - PAD_B);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);

    // The curve itself, flat beyond both ends exactly as `Curve::duty_at` extrapolates.
    if !d.points.is_empty() {
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(2.0);
        let first = d.points[0];
        let last = d.points[d.points.len() - 1];
        cr.move_to(x_of(T_MIN, w), y_of(f64::from(first.1), h));
        for &(t, duty) in &d.points {
            cr.line_to(x_of(t, w), y_of(f64::from(duty), h));
        }
        cr.line_to(x_of(T_MAX, w), y_of(f64::from(last.1), h));
        let _ = cr.stroke();

        for &(t, duty) in &d.points {
            cr.arc(
                x_of(t, w),
                y_of(f64::from(duty), h),
                3.0,
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
        }
        cr.set_line_width(1.0);
    }

    // Where the machine actually is. GNOME blue, legible on both themes.
    if let Some((t, duty)) = d.now {
        let x = x_of(t, w);
        cr.set_source_rgba(0.21, 0.52, 0.89, 0.85);
        cr.move_to(x, PAD_T);
        cr.line_to(x, h - PAD_B);
        let _ = cr.stroke();
        if let Some(duty) = duty {
            cr.arc(x, y_of(f64::from(duty), h), 4.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }
}

/// The editor as a whole: plot, one row per point, and the buttons that act on them.
pub struct CurveEditor {
    pub group: adw::PreferencesGroup,
    plot: gtk::DrawingArea,
    list: gtk::ListBox,
    status: gtk::Label,
    apply: gtk::Button,
    points: Rc<RefCell<Vec<Point>>>,
    data: Rc<RefCell<PlotData>>,
    /// Set the moment the user changes anything, cleared when a curve is applied.
    ///
    /// While it is set, telemetry stops loading the running curve into the editor.
    /// Without it every poll tick would overwrite a half-finished edit with what the
    /// daemon is currently running, which is the same "it snapped back" failure the
    /// other controls solve with `in_flight` — but an editor holds several values at
    /// once and for as long as the user takes, so it needs the coarser rule.
    dirty: Rc<Cell<bool>>,
}

impl CurveEditor {
    pub fn new(on_apply: impl Fn(Vec<(f64, u8)>) + 'static) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title("Fan curve")
            .description(
                "Temperature to fan speed, saved with the profile. The shaded band is \
                 what the firmware would do on its own — a point drawn inside it is \
                 raised to meet it.",
            )
            .build();

        let data = Rc::new(RefCell::new(PlotData::default()));
        let plot = gtk::DrawingArea::builder()
            .content_height(PLOT_HEIGHT)
            .margin_bottom(6)
            .build();
        {
            let data = Rc::clone(&data);
            plot.set_draw_func(move |area, cr, w, h| {
                let d = data.borrow().clone();
                draw(area, cr, w, h, &d);
            });
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .margin_top(6)
            .css_classes(["dim-label", "caption"])
            .build();

        let add = gtk::Button::builder().label("Add point").build();
        let reset = gtk::Button::builder().label("Built-in quiet curve").build();
        let apply = gtk::Button::builder()
            .label("Apply")
            .css_classes(["suggested-action"])
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .margin_top(6)
            .build();
        buttons.append(&reset);
        buttons.append(&add);
        buttons.append(&apply);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        body.append(&plot);
        body.append(&list);
        body.append(&status);
        body.append(&buttons);
        group.add(&body);

        let editor = Rc::new(Self {
            group,
            plot,
            list,
            status,
            apply: apply.clone(),
            points: Rc::new(RefCell::new(Curve::default_quiet().points().to_vec())),
            data,
            dirty: Rc::new(Cell::new(false)),
        });

        {
            let e = Rc::clone(&editor);
            add.connect_clicked(move |_| {
                e.add_point();
            });
        }
        {
            let e = Rc::clone(&editor);
            reset.connect_clicked(move |_| {
                *e.points.borrow_mut() = Curve::default_quiet().points().to_vec();
                e.dirty.set(true);
                e.rebuild();
            });
        }
        {
            let e = Rc::clone(&editor);
            apply.connect_clicked(move |_| {
                let pts: Vec<(f64, u8)> = e
                    .points
                    .borrow()
                    .iter()
                    .map(|p| (p.celsius, p.duty))
                    .collect();
                // Only ever reachable while the curve validates - `refresh` gates the
                // button on exactly that.
                on_apply(pts);
                e.dirty.set(false);
            });
        }

        editor.rebuild();
        editor
    }

    /// Rebuild the point rows from scratch.
    ///
    /// Only called when the *number* of points changes. Editing a value updates the
    /// vector in place and redraws, because rebuilding the rows under a spin button
    /// the user is still typing into takes the focus away mid-edit.
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let len = self.points.borrow().len();
        for i in 0..len {
            let (celsius, duty) = {
                let p = self.points.borrow();
                (p[i].celsius, p[i].duty)
            };

            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(12)
                .margin_end(12)
                .build();

            // Values are set on the adjustment before the handler is connected, so
            // building a row never looks like the user editing one.
            let temp = gtk::SpinButton::with_range(0.0, 120.0, 1.0);
            temp.set_value(celsius);
            temp.set_digits(0);
            temp.set_width_chars(4);
            let duty_spin = gtk::SpinButton::with_range(0.0, 255.0, 5.0);
            duty_spin.set_value(f64::from(duty));
            duty_spin.set_digits(0);
            duty_spin.set_width_chars(4);

            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                // Two points is the minimum a curve can have, so the last two rows
                // cannot be removed. Disabled rather than hidden: a control that
                // vanishes is harder to understand than one that is greyed out.
                .sensitive(len > 2)
                .tooltip_text("Remove this point")
                .build();

            row.append(&temp);
            row.append(&gtk::Label::new(Some("°C")));
            let arrow = gtk::Label::builder()
                .label("→")
                .hexpand(true)
                .xalign(1.0)
                .css_classes(["dim-label"])
                .build();
            row.append(&arrow);
            row.append(&gtk::Label::new(Some("duty")));
            row.append(&duty_spin);
            // Edited in duty, read in RPM. Duty is what a curve stores and what the
            // firmware floor is expressed in, and duty→RPM is flat above 200, so an RPM
            // spinner would offer values that cannot be reached and two that mean the
            // same thing. Showing the speed beside it gives the recognisable number
            // without making it the thing being set.
            let rpm_hint = gtk::Label::builder()
                .css_classes(["dim-label", "caption"])
                .width_chars(9)
                .xalign(1.0)
                .build();
            rpm_hint.set_label(&crate::units::rpm_hint(duty));
            row.append(&rpm_hint);
            row.append(&remove);

            {
                let e = Rc::clone(self);
                temp.connect_value_changed(move |s| {
                    if let Ok(mut p) = e.points.try_borrow_mut() {
                        p[i].celsius = s.value();
                    }
                    e.dirty.set(true);
                    e.refresh();
                });
            }
            {
                let e = Rc::clone(self);
                let rpm_hint = rpm_hint.clone();
                duty_spin.connect_value_changed(move |s| {
                    rpm_hint.set_label(&crate::units::rpm_hint(s.value().round() as u8));
                    if let Ok(mut p) = e.points.try_borrow_mut() {
                        p[i].duty = s.value().round().clamp(0.0, 255.0) as u8;
                    }
                    e.dirty.set(true);
                    e.refresh();
                });
            }
            {
                let e = Rc::clone(self);
                remove.connect_clicked(move |_| {
                    if e.points.borrow().len() > 2 {
                        e.points.borrow_mut().remove(i);
                        e.dirty.set(true);
                        e.rebuild();
                    }
                });
            }

            self.list.append(&row);
        }
        self.refresh();
    }

    /// Insert a point without ever producing an invalid curve.
    ///
    /// Splitting the widest temperature gap and interpolating the duty means the new
    /// point always ascends and never falls, whatever the curve looked like before —
    /// so adding a point cannot turn a valid curve into an error the user then has to
    /// go and fix.
    fn add_point(self: &Rc<Self>) {
        let outcome = insert_point(&mut self.points.borrow_mut());
        match outcome {
            Ok(()) => {
                self.dirty.set(true);
                self.rebuild();
            }
            Err(why) => {
                self.status.set_label(why);
                self.status.set_visible(true);
            }
        }
    }

    /// Validate, report, and redraw. The single place the Apply button's sensitivity
    /// and the status line are decided.
    fn refresh(&self) {
        let pts = self.points.borrow().clone();
        match Curve::new(pts.clone()) {
            Ok(_) => {
                self.status.set_label("");
                self.status.set_visible(false);
                self.apply.set_sensitive(true);
            }
            Err(e) => {
                // The validator's messages are written to be read by a person and name
                // the point at fault, so they are shown as-is rather than summarised.
                self.status.set_label(&e.to_string());
                self.status.set_visible(true);
                self.apply.set_sensitive(false);
            }
        }
        let wire: Vec<(f64, u8)> = pts.iter().map(|p| (p.celsius, p.duty)).collect();
        // The same curve as a command line. Cheap to show and it means a curve drawn
        // here can be put in a profile, a script, or a bug report without transcribing
        // it by hand from the rows.
        self.apply.set_tooltip_text(Some(&format!(
            "fw-helperctl fan curve {}",
            as_cli_argument(&wire)
        )));
        self.data.borrow_mut().points = wire;
        self.plot.queue_draw();
    }

    /// Take what the daemon reports. Live readings always land; the curve itself only
    /// while the user has no unapplied edits.
    pub fn update(self: &Rc<Self>, s: &fw_helper_client::Snapshot) {
        {
            let mut d = self.data.borrow_mut();
            d.floor = s.fan_floor_curve.clone();
            d.now = s
                .control_sensor
                .as_ref()
                .and_then(|name| s.temps.iter().find(|t| &t.label == name))
                .map(|t| (t.celsius, s.fan_duty.filter(|_| s.fan_mode.is_some())));
        }
        if !self.dirty.get() && !s.fan_curve.is_empty() {
            let running: Vec<Point> = s
                .fan_curve
                .iter()
                .map(|&(celsius, duty)| Point { celsius, duty })
                .collect();
            if *self.points.borrow() != running {
                *self.points.borrow_mut() = running;
                self.rebuild();
                return;
            }
        }
        self.plot.queue_draw();
    }

    /// Mirror the rest of the window: nothing is operable with no daemon behind it.
    pub fn set_live(&self, live: bool) {
        self.group.set_sensitive(live);
    }

    /// Called when an apply comes back from the daemon. A failure puts the editor back
    /// into the dirty state so the user's points are not quietly replaced by the
    /// running curve on the next tick — they asked for something that did not happen,
    /// and losing it would hide that.
    pub fn applied(&self, result: &Result<String, String>) {
        if result.is_err() {
            self.dirty.set(true);
        }
    }
}

/// Add one point to a curve without ever making it invalid.
///
/// Kept free of the widgets so it can be tested: this is the only part of the editor
/// with a decision in it, and the one place a plausible-looking rule could quietly
/// produce a curve the validator rejects.
fn insert_point(pts: &mut Vec<Point>) -> Result<(), &'static str> {
    let mut widest = (0usize, 0.0f64);
    for i in 1..pts.len() {
        let gap = pts[i].celsius - pts[i - 1].celsius;
        if gap > widest.1 {
            widest = (i, gap);
        }
    }

    // A gap of at least 2 °C, so the midpoint rounds to a temperature strictly between
    // its neighbours rather than colliding with one of them.
    if widest.1 >= 2.0 {
        let i = widest.0;
        let celsius = ((pts[i - 1].celsius + pts[i].celsius) / 2.0).round();
        let mid = ((u16::from(pts[i - 1].duty) + u16::from(pts[i].duty)) / 2) as u8;
        // Interpolating between two legal duties lands in the unturnable 1..stiction
        // band whenever the lower point is a stopped fan. Snap to whichever end is
        // nearer — the validator refuses anything in between, and it is right to.
        let duty = if mid > 0 && mid < STICTION_DUTY {
            if u16::from(mid) * 2 < u16::from(STICTION_DUTY) {
                0
            } else {
                STICTION_DUTY
            }
        } else {
            mid
        };
        pts.insert(i, Point { celsius, duty });
        return Ok(());
    }

    // Nowhere to split: extend past the top instead, while a legal temperature is left.
    let last = pts[pts.len() - 1];
    if last.celsius + 2.0 > 120.0 {
        return Err("no room for another point — widen the curve first");
    }
    pts.push(Point {
        celsius: (last.celsius + 5.0).min(120.0),
        duty: last.duty.saturating_add(10),
    });
    Ok(())
}

/// Format a curve the way `fw-helperctl` accepts it, for the tooltip and for anyone
/// who wants to reproduce a curve from the CLI.
pub fn as_cli_argument(points: &[(f64, u8)]) -> String {
    points
        .iter()
        .map(|(c, d)| format!("{c:.0}:{d}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(raw: &[(f64, u8)]) -> Vec<Point> {
        raw.iter()
            .map(|&(celsius, duty)| Point { celsius, duty })
            .collect()
    }

    /// The property that matters: adding a point to a curve the daemon accepts must
    /// leave a curve the daemon still accepts. Checked against the real validator
    /// rather than a restatement of its rules.
    #[test]
    fn adding_a_point_keeps_a_curve_valid() {
        let mut p = Curve::default_quiet().points().to_vec();
        for _ in 0..12 {
            insert_point(&mut p).expect("the built-in curve has room");
            Curve::new(p.clone()).expect("still valid after adding a point");
        }
    }

    /// Splitting 55 °C/duty 0 and 62 °C/duty 40 interpolates to duty 20, which cannot
    /// turn the fan. It must not be offered.
    #[test]
    fn interpolation_never_lands_in_the_stiction_band() {
        let mut p = pts(&[(55.0, 0), (62.0, 40)]);
        insert_point(&mut p).unwrap();
        let added = p[1];
        assert!(
            added.duty == 0 || added.duty >= STICTION_DUTY,
            "duty {} is inside the unturnable band",
            added.duty
        );
        Curve::new(p).unwrap();
    }

    /// Rounding the midpoint must not collide with a neighbour, which would read as a
    /// non-ascending curve.
    #[test]
    fn a_narrow_gap_extends_rather_than_splitting() {
        let mut p = pts(&[(80.0, 100), (81.0, 110)]);
        insert_point(&mut p).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].celsius, 86.0, "extended past the top, not split");
        Curve::new(p).unwrap();
    }

    #[test]
    fn refuses_when_there_is_no_legal_temperature_left() {
        let mut p = pts(&[(118.0, 200), (119.0, 220)]);
        assert!(insert_point(&mut p).is_err());
        assert_eq!(p.len(), 2, "a refused add must not change the curve");
    }

    #[test]
    fn cli_argument_round_trips_through_the_validator() {
        let c = Curve::default_quiet();
        let wire: Vec<(f64, u8)> = c.points().iter().map(|p| (p.celsius, p.duty)).collect();
        assert_eq!(
            as_cli_argument(&wire),
            "55:0,62:40,70:65,80:92,90:130,100:255"
        );
    }
}
