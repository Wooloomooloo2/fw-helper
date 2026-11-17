//! The run summary: one recording as a self-contained HTML page, opened in the browser
//! and written beside the recording.
//!
//! Ported from the Windows sibling's `SummaryReport` so the two read alike: system
//! fonts only and the data embedded as JSON, so it works offline and can be kept or
//! shared as one file. Pure - rows and figures in, a string out.
//!
//! What differs from Windows is what the numbers are read against. There is no Windows
//! power mode here; the line on the power chart is the PL1 fw-helper was enforcing, and
//! the GPU's limit reasons are `xe`'s, whose meanings this machine has measured (see
//! [`throttle_meaning`]).

use fw_helper_core::json::{number, quote};
use fw_helper_core::notes::RunNotes;
use fw_helper_core::session::Row;
use fw_helper_core::summary::{FrameStat, RunSummary, Share, COLUMNS, MENU_FACTOR, TRIM_SECS};
use std::fmt::Write;

/// Ties a report to its recording, so a client can find the CSV from the page.
pub const SESSION_META: &str = "fwhelper-session";

/// What an `xe` GPU limit reason means, in words - as measured on this machine, not as
/// the names suggest. `pl4` in particular is asserted whenever the GPU is busy and is
/// not by itself a sign of being held back (CLAUDE.md traps).
pub fn throttle_meaning(reason: &str) -> String {
    match reason {
        "none" => "not limited: the GPU ran at the clock it asked for".into(),
        "pl1" => "the sustained package power limit (PL1)".into(),
        "pl2" => "the short-term power limit (PL2) - on battery a heavy GPU load hits this and clocks 1300-1500 MHz instead of ~1950".into(),
        "pl4" => "the current limit (PL4). On this machine xe reports it whenever the GPU is busy, so it marks load more than a limit".into(),
        "thermal" => "temperature".into(),
        "prochot" => "PROCHOT, a platform 'too hot' signal".into(),
        "ratl" => "the running-average thermal limit".into(),
        "vr_tdc" => "the voltage regulator's current limit".into(),
        "vr_thermalert" => "the voltage regulator's temperature".into(),
        r if r.contains('+') => "several limits at once".into(),
        r => r.to_string(),
    }
}

/// HTML-escape.
pub fn e(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn n0(v: Option<f64>) -> String {
    v.map(|d| format!("{d:.0}"))
        .unwrap_or_else(|| "\u{2013}".into())
}

pub(crate) fn w1(v: Option<f64>) -> String {
    v.map(|d| format!("{d:.1}"))
        .unwrap_or_else(|| "\u{2013}".into())
}

pub(crate) fn pct(f: f64) -> String {
    format!("{:.0} %", f * 100.0)
}

pub(crate) fn duration(secs: u64) -> String {
    if secs >= 60 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

pub(crate) fn format_stat(v: f64, unit: &str) -> String {
    match unit {
        "W" | "GB" => format!("{v:.1}"),
        _ => format!("{v:.0}"),
    }
}

/// Local wall-clock pieces for a Unix time, given the offset in force then.
pub(crate) fn local(unix: u64, utc_offset: &dyn Fn(u64) -> i64) -> (i64, u32, u32, u32, u32, u32) {
    let t = unix as i64 + utc_offset(unix);
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// "16 November 2026, 22:33".
pub(crate) fn long_date(unix: u64, off: &dyn Fn(u64) -> i64) -> String {
    let (y, m, d, h, mi, _) = local(unix, off);
    format!("{d} {} {y}, {h:02}:{mi:02}", MONTHS[(m - 1) as usize])
}

/// "16 Nov 22:33".
pub(crate) fn short_date(unix: u64, off: &dyn Fn(u64) -> i64) -> String {
    let (_, m, d, h, mi, _) = local(unix, off);
    format!("{d} {} {h:02}:{mi:02}", &MONTHS[(m - 1) as usize][..3])
}

fn tile(out: &mut String, key: &str, value: &str, detail: &str) {
    let _ = write!(
        out,
        "<div class=\"tile\"><div class=\"eyebrow\">{}</div><div class=\"k\">{value}</div><div class=\"d\">{detail}</div></div>",
        e(key)
    );
}

fn mixed(shares: &[Share]) -> String {
    match shares.len() {
        0 => "?".into(),
        1 => shares[0].value.clone(),
        _ => shares
            .iter()
            .take(3)
            .map(|s| format!("{} {:.0} %", s.value, s.fraction * 100.0))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The whole page.
pub fn summary_html(
    rows: &[Row],
    s: &RunSummary,
    notes: Option<&RunNotes>,
    label: &str,
    csv_name: &str,
    utc_offset: &dyn Fn(u64) -> i64,
) -> String {
    let mut o = String::with_capacity(64 * 1024 + rows.len() * 200);
    let title = s.game.clone().unwrap_or_else(|| "Run".into());
    let profile = s
        .profiles
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(" \u{2192} ");

    o.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    o.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(
        o,
        "<meta name=\"{SESSION_META}\" content=\"{}\">",
        e(csv_name)
    );
    let _ = write!(
        o,
        "<title>{} \u{b7} {}</title>\n<style>{CSS_VARS}{TIP_CSS}</style></head><body>\n<div class=\"wrap\">\n",
        e(&title),
        short_date(s.start_unix, utc_offset)
    );

    // Header
    o.push_str("<header>");
    let _ = write!(
        o,
        "<div class=\"eyebrow\">fw-helper run summary \u{b7} {}</div>",
        e(&long_date(s.start_unix, utc_offset))
    );
    let lbl = if label.is_empty() || Some(label) == s.game.as_deref() {
        String::new()
    } else {
        format!(" <span class=\"lbl\">{}</span>", e(label))
    };
    let _ = write!(o, "<h1>{}{lbl}</h1>", e(&title));
    let mut ctx = vec![
        format!(
            "Profile <b>{}</b>",
            e(if profile.is_empty() { "?" } else { &profile })
        ),
        format!("Fan <b>{}</b>", e(&mixed(&s.fan))),
        if s.on_ac_fraction >= 1.0 {
            "on mains".into()
        } else if s.on_ac_fraction == 0.0 {
            "on battery".into()
        } else {
            "partly on battery".into()
        },
    ];
    if let Some(pl1) = s.power_target {
        ctx.push(format!("PL1 <b>{pl1} W</b>"));
    }
    if let Some(n) = notes {
        if let Some(c) = n.fan_curve.as_deref().filter(|c| *c != "EC auto") {
            ctx.push(format!("fan curve <b>{}</b>", e(c)));
        }
        if let Some(g) = &n.game_settings {
            ctx.push(format!("game settings <b>{}</b>", e(g)));
        }
    }
    let _ = write!(o, "<p class=\"lede\">{}</p>", ctx.join(" \u{b7} "));
    if let Some(text) = notes.and_then(|n| n.notes.as_deref()) {
        let _ = write!(
            o,
            "<p class=\"usernote\">{}</p>",
            e(text).replace('\n', "<br>")
        );
    }
    let _ = write!(
        o,
        "<p class=\"note\">{} recorded. ",
        duration(s.length_secs())
    );
    match &s.game {
        None => o.push_str("No game was detected, so the figures cover every second."),
        Some(g) => {
            let _ = write!(
                o,
                "The figures cover the <b>{} s</b> in which {} was presenting frames",
                s.steady.len(),
                e(g)
            );
            if s.trimmed {
                let _ = write!(
                    o,
                    ", from {TRIM_SECS} s after it started (loading and shader compiles distort the start)"
                );
            }
            if s.menu_secs > 0 {
                let _ = write!(
                    o,
                    ". {} s at more than {MENU_FACTOR:.0}\u{d7} the median FPS were left out as menus or loading screens",
                    s.menu_secs
                );
            }
            o.push_str(". Only the unshaded seconds count.");
        }
    }
    o.push_str("</p></header>\n");

    if !s.warnings.is_empty() {
        o.push_str("<div class=\"warn\"><h3>Read with care</h3><ul>");
        for w in &s.warnings {
            let _ = write!(o, "<li>{}</li>", e(w));
        }
        o.push_str("</ul></div>\n");
    }

    // Tiles
    o.push_str("<div class=\"tiles\">");
    if let Some(fps) = s.stat("fps") {
        let detail = match s.frames {
            Some(FrameStat {
                low1: Some(l1),
                low01,
                ..
            }) => format!(
                "1 % low <b>{}</b>{} \u{b7} range {}\u{2013}{}",
                n0(Some(l1)),
                low01
                    .map(|v| format!(" \u{b7} 0.1 % low {v:.0}"))
                    .unwrap_or_default(),
                n0(Some(fps.min)),
                n0(Some(fps.max))
            ),
            _ => format!(
                "5th percentile {} \u{b7} range {}\u{2013}{}",
                n0(Some(fps.p5)),
                n0(Some(fps.min)),
                n0(Some(fps.max))
            ),
        };
        tile(&mut o, "Median FPS", &n0(Some(fps.median)), &detail);
    }
    if let Some(pkg) = s.stat("package_w") {
        tile(
            &mut o,
            "Package power",
            &format!("{}<small> W</small>", w1(Some(pkg.avg))),
            &format!(
                "CPU {} W \u{b7} GPU {} W \u{b7} rest {} W",
                w1(s.stat("cpu_w").map(|x| x.avg)),
                w1(s.stat("gpu_w").map(|x| x.avg)),
                w1(s.stat("rest_w").map(|x| x.avg))
            ),
        );
    }
    if let Some(fpw) = s.fps_per_watt() {
        tile(
            &mut o,
            "FPS per watt",
            &format!("{fpw:.2}"),
            "Median FPS \u{f7} average package watts. Higher is more efficient.",
        );
    }
    if let Some(top) = s.gpu_throttle.first() {
        let shown = if top.value == "none" {
            "nothing"
        } else {
            &top.value
        };
        tile(
            &mut o,
            "GPU clock held by",
            &e(shown),
            &format!(
                "{} of the time: {}",
                pct(top.fraction),
                e(&throttle_meaning(&top.value))
            ),
        );
    }
    if let Some(gm) = s.stat("gpu_mhz") {
        let load = n0(s.stat("gpu_pct").map(|x| x.avg));
        let detail = match s.stat("gpu_req_mhz") {
            Some(gr) => format!(
                "asked for {} MHz \u{b7} GPU load {load} %",
                n0(Some(gr.avg))
            ),
            None => format!("GPU load {load} %"),
        };
        tile(
            &mut o,
            "GPU clock",
            &format!("{}<small> MHz</small>", n0(Some(gm.avg))),
            &detail,
        );
    }
    if let Some(ct) = s.stat("cpu_c") {
        tile(
            &mut o,
            "CPU temperature",
            &format!("{}<small> \u{b0}C avg</small>", n0(Some(ct.avg))),
            &format!(
                "peak {} \u{b0}C \u{b7} fan {} rpm avg",
                n0(Some(ct.max)),
                n0(s.stat("fan_rpm").map(|x| x.avg))
            ),
        );
    }
    o.push_str("</div>\n");

    o.push_str("<section><div class=\"eyebrow\">1 \u{b7} Over time</div><h2>Second by second</h2>");
    o.push_str(
        "<p class=\"note\">Hover over (or tap) any chart for every reading at that second.</p>",
    );
    o.push_str("<div id=\"charts\"></div></section>\n");

    if s.stat("package_w").is_some() {
        o.push_str(
            "<section><div class=\"eyebrow\">2 \u{b7} Power</div><h2>Where the watts went</h2>",
        );
        o.push_str(
            "<div class=\"chart\" id=\"c-split\"><div class=\"legend\">\
             <span><i class=\"sw\" style=\"background:var(--cpu)\"></i>CPU cores</span>\
             <span><i class=\"sw\" style=\"background:var(--gpu)\"></i>GPU</span>\
             <span><i class=\"sw\" style=\"background:var(--rest)\"></i>Rest of package (fabric, memory controller, I/O)</span>",
        );
        if s.power_target.is_some() {
            o.push_str("<span><i class=\"sw dash\"></i>PL1 fw-helper was holding</span>");
        }
        o.push_str("</div></div>");
        o.push_str(
            "<p class=\"note\">Averages over the steady seconds. Cores and GPU share one package, and on this \
             machine the GPU's clock is traded against the cores' even far below PL1: slower cores leave \
             the GPU more current.</p></section>\n",
        );
    }

    let cpu_hot = s.cpu_throttle.iter().find(|x| x.value != "none");
    if !s.gpu_throttle.is_empty() || cpu_hot.is_some() {
        o.push_str("<section><div class=\"eyebrow\">3 \u{b7} Limits</div><h2>What held the clocks back</h2>");
        if !s.gpu_throttle.is_empty() {
            o.push_str("<div class=\"tbl\"><table><thead><tr><th>GPU clock limited by</th><th>Share of time</th><th></th></tr></thead><tbody>");
            for t in &s.gpu_throttle {
                let _ = write!(
                    o,
                    "<tr><td>{}</td><td>{}</td><td class=\"why\">{}</td></tr>",
                    e(&t.value),
                    pct(t.fraction),
                    e(&throttle_meaning(&t.value))
                );
            }
            o.push_str("</tbody></table></div>");
        }
        match cpu_hot {
            Some(h) => {
                let _ = write!(
                    o,
                    "<p>The CPU logged thermal throttle events in {} of the seconds.</p>",
                    pct(h.fraction)
                );
            }
            None => o.push_str("<p class=\"note\">The CPU logged no thermal throttling.</p>"),
        }
        o.push_str("</section>\n");
    }

    // Every figure
    o.push_str("<section><div class=\"eyebrow\">4 \u{b7} Figures</div><h2>Every reading</h2><div class=\"tbl\"><table>");
    o.push_str("<thead><tr><th>Reading</th><th>Average</th><th>Median</th><th>5th pct</th><th>Min</th><th>Max</th></tr></thead><tbody>");
    for c in COLUMNS {
        let Some(st) = s.stat(c.key) else { continue };
        let f = |v: f64| format_stat(v, c.unit);
        let u = if c.unit.is_empty() {
            String::new()
        } else {
            format!(" <span class=\"u\">{}</span>", e(c.unit))
        };
        let _ = write!(
            o,
            "<tr><td>{}</td><td>{}{u}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            e(c.label),
            f(st.avg),
            f(st.median),
            f(st.p5),
            f(st.min),
            f(st.max)
        );
    }
    o.push_str("</tbody></table></div>");
    if let Some(f) = s.frames {
        let _ = write!(
            o,
            "<p class=\"note\">Frame times: {} frames in the steady seconds, averaging {} FPS. ",
            f.count,
            n0(Some(f.avg_fps))
        );
        match f.low1 {
            Some(l1) => {
                let _ = write!(
                    o,
                    "1 % low {} FPS (99 % of frames took under {:.1} ms)",
                    n0(Some(l1)),
                    f.p99_ms
                );
            }
            None => {
                let _ = write!(
                    o,
                    "Too few frames for a 1 % low (under {})",
                    FrameStat::MIN_FOR_1
                );
            }
        }
        if let Some(l01) = f.low01 {
            let _ = write!(o, ", 0.1 % low {} FPS", n0(Some(l01)));
        }
        let _ = write!(
            o,
            "; the longest frame took {:.1} ms. A low far below the median means stutter, even when the average looks fine.</p>",
            f.max_ms
        );
    } else if s.stat("fps").is_some() {
        o.push_str("<p class=\"note\">This recording has no frame times, so there are no 1 % lows: the 5th percentile is of one-second averages.</p>");
    }
    o.push_str("</section>\n");

    let firmware = notes
        .filter(|n| n.bios.is_some() || n.ec.is_some())
        .map(|n| {
            format!(
                "Recorded on BIOS {}, EC {}{}{}. ",
                e(n.bios.as_deref().unwrap_or("?")),
                e(n.ec.as_deref().unwrap_or("?")),
                n.kernel
                    .as_deref()
                    .map(|k| format!(", kernel {}", e(k)))
                    .unwrap_or_default(),
                n.app
                    .as_deref()
                    .map(|a| format!(", fw-helper {}", e(a)))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let _ = writeln!(
        o,
        "<footer class=\"note\">{firmware}Made by fw-helper {} from <span class=\"mono\">{}</span>, in the FW Benchmark folder. \
         Open it from fw-helper's Monitor page to see it in the live charts, or to make this report again.</footer>",
        e(env!("CARGO_PKG_VERSION")),
        e(csv_name)
    );
    o.push_str("</div><div id=\"tip\" hidden></div>\n");

    // Data: one array per column over every row, plus the steady flags.
    let mut steady = vec![false; rows.len()];
    for &i in &s.steady {
        steady[i] = true;
    }
    let t0 = s.start_unix;
    let mut d = String::from("{");
    let arr = |vals: Vec<String>| format!("[{}]", vals.join(","));
    let _ = write!(
        d,
        "\"t\":{},",
        arr(rows
            .iter()
            .map(|r| (r.unix_time.saturating_sub(t0)).to_string())
            .collect())
    );
    let _ = write!(
        d,
        "\"clock\":{},",
        arr(rows
            .iter()
            .map(|r| {
                let (_, _, _, h, m, sec) = local(r.unix_time, utc_offset);
                format!("\"{h:02}:{m:02}:{sec:02}\"")
            })
            .collect())
    );
    let _ = write!(
        d,
        "\"steady\":{},",
        arr(steady.iter().map(|b| b.to_string()).collect())
    );
    let text = |f: fn(&Row) -> Option<String>| {
        arr(rows
            .iter()
            .map(|r| f(r).map(|v| quote(&v)).unwrap_or_else(|| "null".into()))
            .collect())
    };
    let _ = write!(d, "\"gpu_throttle\":{},", text(|r| r.gpu_throttle.clone()));
    let _ = write!(
        d,
        "\"throttle\":{},",
        text(|r| (r.throttle_events > 0).then(|| format!("{} thermal events", r.throttle_events)))
    );
    let _ = write!(d, "\"fps_app\":{},", text(|r| r.fps_app.clone()));
    let _ = write!(d, "\"profile\":{},", text(|r| r.profile.clone()));
    let _ = write!(
        d,
        "\"pl1\":{},",
        s.power_target
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into())
    );
    let avg = |k: &str| {
        s.stat(k)
            .map(|x| number(x.avg))
            .unwrap_or_else(|| "null".into())
    };
    let _ = write!(
        d,
        "\"avg\":{{\"cpu_w\":{},\"gpu_w\":{},\"rest_w\":{},\"package_w\":{}}}",
        avg("cpu_w"),
        avg("gpu_w"),
        avg("rest_w"),
        avg("package_w")
    );
    for c in COLUMNS {
        let vals = rows
            .iter()
            .map(|r| match (c.get)(r) {
                Some(v) if v.is_finite() => number((v * 10.0).round() / 10.0),
                _ => "null".into(),
            })
            .collect();
        let _ = write!(d, ",{}:{}", quote(c.key), arr(vals));
    }
    let _ = write!(
        d,
        ",\"battery_pct\":{}",
        arr(rows
            .iter()
            .map(|r| r
                .battery_pct
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()))
            .collect())
    );
    d.push('}');
    let _ = write!(o, "<script>const D = {d};\n{JS}</script>\n</body></html>\n");
    o
}

pub(crate) const CSS_VARS: &str = r#"
:root {
  --bg: #f7f8f9; --panel: #ffffff; --ink: #15171a; --ink-2: #4d525a; --ink-3: #7a808a; --rule: #dfe2e6; --grid: #eceef1;
  --shade: rgba(120,128,140,0.10); --fps: #6250d6; --fps-2: #b3aaf0; --cpu: #2a78d6; --gpu: #eb6834; --rest: #c3c8cf; --pkg: #c8384a;
  --mem: #8a5cd6; --batt: #12936b; --warn: #9a6a00; --warn-bg: #fff7e6; --chg: #fdf0d8;
  --f-display: "Cantarell", "Adwaita Sans", "Inter", system-ui, sans-serif;
  --f-body: "Cantarell", "Adwaita Sans", "Inter", system-ui, -apple-system, sans-serif;
  --f-mono: "Adwaita Mono", "DejaVu Sans Mono", ui-monospace, monospace;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #131518; --panel: #1b1e22; --ink: #eef0f2; --ink-2: #b8bdc5; --ink-3: #8a909a; --rule: #2e3238; --grid: #24282d;
    --shade: rgba(160,170,185,0.09); --fps: #9085e9; --fps-2: #4f4888; --cpu: #3987e5; --gpu: #d95926; --rest: #4a5058; --pkg: #e35d6c;
    --mem: #a585e6; --batt: #3cbf92; --warn: #e0b04a; --warn-bg: #2a2416; --chg: #33291a; color-scheme: dark;
  }
}
:root[data-theme="dark"] {
  --bg: #131518; --panel: #1b1e22; --ink: #eef0f2; --ink-2: #b8bdc5; --ink-3: #8a909a; --rule: #2e3238; --grid: #24282d;
  --shade: rgba(160,170,185,0.09); --fps: #9085e9; --fps-2: #4f4888; --cpu: #3987e5; --gpu: #d95926; --rest: #4a5058; --pkg: #e35d6c;
  --mem: #a585e6; --batt: #3cbf92; --warn: #e0b04a; --warn-bg: #2a2416; --chg: #33291a; color-scheme: dark;
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 15px/1.55 var(--f-body); }
.wrap { max-width: 860px; margin: 0 auto; padding: 36px 16px 56px; display: flex; flex-direction: column; gap: 30px; position: relative; }
.wrap > * { min-width: 0; max-width: 100%; }
h1, h2 { font-family: var(--f-display); margin: 0; text-wrap: balance; }
h1 { font-size: 2rem; line-height: 1.15; font-weight: 600; }
h1 .lbl { font-weight: 400; color: var(--ink-2); font-size: 1.3rem; }
h2 { font-size: 1.3rem; font-weight: 600; }
p { margin: 0; max-width: 72ch; }
header { display: flex; flex-direction: column; gap: 10px; }
.eyebrow { font: 500 0.72rem/1 var(--f-mono); text-transform: uppercase; letter-spacing: 0.08em; color: var(--ink-3); }
.lede { color: var(--ink-2); }
.lede b { color: var(--ink); font-weight: 600; }
.usernote { border-left: 3px solid var(--rule); padding: 2px 0 2px 12px; color: var(--ink-2); }
.note { font-size: 0.84rem; color: var(--ink-3); }
.mono { font-family: var(--f-mono); }
.warn { background: var(--warn-bg); border: 1px solid var(--rule); border-left: 3px solid var(--warn); border-radius: 6px; padding: 12px 16px; }
.warn h3 { margin: 0 0 4px; font: 600 0.95rem var(--f-display); color: var(--warn); }
.warn ul { margin: 0; padding-left: 1.1em; font-size: 0.9rem; color: var(--ink-2); }
.tiles { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; }
.tile { background: var(--panel); border: 1px solid var(--rule); border-radius: 6px; padding: 14px 16px; display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.tile .k { font: 600 1.55rem/1.1 var(--f-display); font-variant-numeric: tabular-nums; overflow-wrap: anywhere; }
.tile .k small { font-size: 0.9rem; color: var(--ink-3); font-weight: 500; }
.tile .d { font-size: 0.84rem; color: var(--ink-2); }
section { display: flex; flex-direction: column; gap: 12px; min-width: 0; }
.chart { background: var(--panel); border: 1px solid var(--rule); border-radius: 6px; padding: 12px 12px 6px; position: relative; }
.chart + .chart { margin-top: 10px; }
.chart h4 { margin: 0 4px 4px; font: 600 0.88rem var(--f-display); }
.chart svg { display: block; width: 100%; height: auto; font-family: var(--f-body); touch-action: pan-y; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 0.78rem; color: var(--ink-2); padding: 0 4px 6px; }
.legend span { display: inline-flex; align-items: center; gap: 6px; }
.sw { width: 11px; height: 11px; border-radius: 2px; display: inline-block; }
.sw.dash { width: 16px; height: 0; border-top: 2px dashed var(--ink-3); border-radius: 0; }
table { border-collapse: collapse; font-size: 0.86rem; width: 100%; }
th, td { text-align: right; padding: 6px 8px; border-bottom: 1px solid var(--rule); font-variant-numeric: tabular-nums; white-space: nowrap; vertical-align: top; }
th:first-child, td:first-child, .l { text-align: left; }
td.why { text-align: left; white-space: normal; color: var(--ink-2); }
td.chg { background: var(--chg); }
th { font-weight: 600; color: var(--ink-2); font-size: 0.78rem; }
tr:last-child td { border-bottom: 0; }
.u { color: var(--ink-3); font-size: 0.8em; }
.tag { font: 500 0.68rem var(--f-mono); text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-3); border: 1px solid var(--rule); border-radius: 3px; padding: 0 4px; }
.tbl { overflow-x: auto; background: var(--panel); border: 1px solid var(--rule); border-radius: 6px; padding: 6px 8px; }
@media (max-width: 600px) { .tiles { grid-template-columns: 1fr 1fr; } h1 { font-size: 1.6rem; } }
@media print { #tip { display: none; } .chart, .tile, .tbl { break-inside: avoid; } }
"#;

const TIP_CSS: &str = r#"
#tip { position: absolute; pointer-events: none; background: var(--ink); color: var(--bg); font: 0.76rem/1.5 var(--f-mono); padding: 7px 10px; border-radius: 4px; white-space: nowrap; z-index: 5; }
#tip b { font-weight: 600; }
#tip .dim { opacity: 0.65; }
"#;

const JS: &str = r##"
const NS = "http://www.w3.org/2000/svg";
const n = D.t.length;
function el(tag, attrs, parent, text) {
  const e = document.createElementNS(NS, tag);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (text != null) e.textContent = text;
  if (parent) parent.appendChild(e);
  return e;
}
const fmt = (v, d) => v == null ? "–" : v.toFixed(d);
const wrap = document.querySelector(".wrap"), tip = document.getElementById("tip");
wrap.appendChild(tip);

function readout(i) {
  const r = [`<b>${D.clock[i]}</b> <span class="dim">+${D.t[i]} s${D.steady[i] ? "" : " · not counted"}</span>`];
  const add = (label, parts) => { const p = parts.filter(x => x); if (p.length) r.push(`<span class="dim">${label}</span> ${p.join(" · ")}`); };
  add("FPS  ", [D.fps[i] != null ? `<b>${fmt(D.fps[i], 0)}</b>` + (D.fps_app[i] ? ` ${D.fps_app[i]}` : "") : null]);
  add("Power", [D.package_w[i] != null ? `pkg <b>${fmt(D.package_w[i], 1)} W</b>` : null, D.cpu_w[i] != null ? `CPU ${fmt(D.cpu_w[i], 1)}` : null,
                D.gpu_w[i] != null ? `GPU ${fmt(D.gpu_w[i], 1)}` : null, D.system_w[i] != null ? `system ${fmt(D.system_w[i], 1)}` : null]);
  add("Clock", [D.cpu_mhz[i] != null ? `CPU ${fmt(D.cpu_mhz[i], 0)}` : null,
                D.gpu_mhz[i] != null ? `GPU ${fmt(D.gpu_mhz[i], 0)}` + (D.gpu_req_mhz[i] != null ? `/${fmt(D.gpu_req_mhz[i], 0)}` : "") + " MHz" : null,
                D.gpu_throttle[i] ? `held by ${D.gpu_throttle[i]}` : null]);
  add("Load ", [D.cpu_pct[i] != null ? `CPU ${fmt(D.cpu_pct[i], 0)} %` : null, D.gpu_pct[i] != null ? `GPU ${fmt(D.gpu_pct[i], 0)} %` : null]);
  add("Temp ", [D.cpu_c[i] != null ? `CPU ${D.cpu_c[i]}` : null, D.ddr_c[i] != null ? `DDR ${D.ddr_c[i]}` : null,
                D.battery_c[i] != null ? `batt ${D.battery_c[i]} °C` : null, D.fan_rpm[i] != null ? `fan ${D.fan_rpm[i]} rpm` : null]);
  add("Mem  ", [D.mem_used_gb[i] != null ? `RAM ${fmt(D.mem_used_gb[i], 1)} GB` : null, D.profile[i] ? `profile ${D.profile[i]}` : null]);
  if (D.throttle[i]) r.push(`<span class="dim">CPU  </span> ${D.throttle[i]}`);
  return r.join("<br>");
}

const charts = [];
const W = 760, L = 46, R = 12, T = 8, H = 150;
function chart(title, series, opts) {
  opts = opts || {};
  if (!series.some(s => D[s.key].some(v => v != null))) return;
  const host = document.createElement("div");
  host.className = "chart";
  host.innerHTML = `<h4>${title}</h4><div class="legend">` +
    series.map(s => `<span><i class="sw" style="background:${s.color}"></i>${s.label}</span>`).join("") + "</div>";
  document.getElementById("charts").appendChild(host);
  const svg = el("svg", {viewBox: `0 0 ${W} ${H + 18}`, role: "img", "aria-label": title + " over time"}, host);
  let lo = opts.min ?? Infinity, hi = opts.max ?? -Infinity;
  if (opts.min == null || opts.max == null)
    series.forEach(s => D[s.key].forEach((v, i) => { if (v != null && (!opts.steadyScale || D.steady[i])) { if (opts.min == null) lo = Math.min(lo, v); if (opts.max == null) hi = Math.max(hi, v); } }));
  (opts.lines || []).forEach(l => hi = Math.max(hi, l.v));
  if (opts.min == null) lo = Math.max(0, Math.floor(lo * 0.9));
  if (!(hi > lo)) hi = lo + 1;
  const step = niceStep((hi - lo) / 4);
  if (opts.max == null) hi = Math.ceil(hi / step) * step;
  lo = Math.floor(lo / step) * step;
  const tMax = Math.max(1, D.t[n - 1]);
  const x = t => L + t / tMax * (W - L - R), y = v => T + (1 - (v - lo) / (hi - lo)) * (H - T);
  let start = null;
  for (let i = 0; i <= n; i++) {
    const out = i < n && !D.steady[i];
    if (out && start == null) start = i;
    if (!out && start != null) {
      const x0 = x(D.t[Math.max(0, start)]), x1 = x(D.t[Math.min(n - 1, i)]);
      el("rect", {x: x0, y: T, width: Math.max(1, x1 - x0), height: H - T, fill: "var(--shade)"}, svg);
      start = null;
    }
  }
  for (let v = lo; v <= hi + 1e-9; v += step) {
    el("line", {x1: L, x2: W - R, y1: y(v), y2: y(v), stroke: "var(--grid)", "stroke-width": 1}, svg);
    el("text", {x: L - 6, y: y(v) + 4, "text-anchor": "end", "font-size": 10.5, fill: "var(--ink-3)", "font-family": "var(--f-mono)"}, svg, +v.toFixed(1));
  }
  const tStep = niceStep(tMax / 6);
  for (let t = 0; t <= tMax; t += tStep)
    el("text", {x: x(t), y: H + 14, "text-anchor": "middle", "font-size": 10.5, fill: "var(--ink-3)", "font-family": "var(--f-mono)"}, svg, t >= 60 ? `${Math.floor(t / 60)}:${String(Math.round(t % 60)).padStart(2, "0")}` : `${t}s`);
  (opts.lines || []).forEach(l => {
    el("line", {x1: L, x2: W - R, y1: y(l.v), y2: y(l.v), stroke: "var(--ink-3)", "stroke-width": 1.2, "stroke-dasharray": "4 3"}, svg);
    el("text", {x: W - R - 4, y: y(l.v) - 4, "text-anchor": "end", "font-size": 10.5, fill: "var(--ink-3)"}, svg, l.label);
  });
  series.forEach(s => {
    let d = "", pen = false;
    D[s.key].forEach((v, i) => {
      if (v == null) { pen = false; return; }
      d += (pen ? "L" : "M") + x(D.t[i]).toFixed(1) + " " + y(Math.min(Math.max(v, lo), hi)).toFixed(1);
      pen = true;
    });
    el("path", {d, fill: "none", stroke: s.color, "stroke-width": s.width || 1.6, "stroke-linejoin": "round"}, svg);
  });
  const cross = el("line", {y1: T, y2: H, stroke: "var(--ink-2)", "stroke-width": 1, visibility: "hidden"}, svg);
  const dots = series.map(s => el("circle", {r: 3.5, fill: s.color, stroke: "var(--panel)", "stroke-width": 1.5, visibility: "hidden"}, svg));
  const hit = el("rect", {x: L, y: 0, width: W - L - R, height: H, fill: "transparent", tabindex: 0}, svg);
  const c = {host, svg, x, y, cross, dots, series, lo, hi};
  charts.push(c);
  const pick = clientX => {
    const r = svg.getBoundingClientRect();
    const t = ((clientX - r.left) / r.width * W - L) / (W - L - R) * tMax;
    let best = 0;
    for (let i = 1; i < n; i++) if (Math.abs(D.t[i] - t) < Math.abs(D.t[best] - t)) best = i;
    return best;
  };
  const move = (clientX, clientY) => show(pick(clientX), c, clientX, clientY);
  hit.addEventListener("mousemove", e => move(e.clientX, e.clientY));
  hit.addEventListener("touchstart", e => move(e.touches[0].clientX, e.touches[0].clientY), {passive: true});
  hit.addEventListener("touchmove", e => move(e.touches[0].clientX, e.touches[0].clientY), {passive: true});
  hit.addEventListener("mouseleave", hide);
  hit.addEventListener("blur", hide);
  hit.addEventListener("keydown", e => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    current = Math.min(n - 1, Math.max(0, (current ?? 0) + (e.key === "ArrowRight" ? 1 : -1)));
    const r = svg.getBoundingClientRect();
    show(current, c, r.left + x(D.t[current]) / W * r.width, r.top + 20);
  });
}
function niceStep(raw) {
  const p = Math.pow(10, Math.floor(Math.log10(raw || 1))), m = raw / p;
  return (m <= 1 ? 1 : m <= 2 ? 2 : m <= 2.5 ? 2.5 : m <= 5 ? 5 : 10) * p;
}
let current = null;
function show(i, from, clientX, clientY) {
  current = i;
  charts.forEach(c => {
    const xv = c.x(D.t[i]);
    c.cross.setAttribute("x1", xv); c.cross.setAttribute("x2", xv); c.cross.setAttribute("visibility", "visible");
    c.series.forEach((s, k) => {
      const v = D[s.key][i];
      if (v == null) { c.dots[k].setAttribute("visibility", "hidden"); return; }
      c.dots[k].setAttribute("cx", xv); c.dots[k].setAttribute("cy", c.y(Math.min(Math.max(v, c.lo), c.hi)));
      c.dots[k].setAttribute("visibility", "visible");
    });
  });
  tip.innerHTML = readout(i); tip.hidden = false;
  const wr = wrap.getBoundingClientRect(), tw = tip.offsetWidth, th = tip.offsetHeight;
  let left = clientX - wr.left + 14;
  if (left + tw > wr.width - 8) left = clientX - wr.left - tw - 14;
  let top = clientY - wr.top + 14;
  const hr = from.host.getBoundingClientRect();
  if (top + th > hr.bottom - wr.top + 40) top = clientY - wr.top - th - 14;
  tip.style.left = Math.max(4, left) + "px"; tip.style.top = Math.max(4, top) + "px";
}
function hide() {
  tip.hidden = true;
  charts.forEach(c => { c.cross.setAttribute("visibility", "hidden"); c.dots.forEach(d => d.setAttribute("visibility", "hidden")); });
}

chart("Frame rate", [{key: "fps", label: "FPS", color: "var(--fps)", width: 1.8}], {min: 0, steadyScale: true});
chart("Power", [{key: "package_w", label: "Package", color: "var(--pkg)", width: 1.8}, {key: "cpu_w", label: "CPU cores", color: "var(--cpu)"},
  {key: "gpu_w", label: "GPU", color: "var(--gpu)"}, {key: "system_w", label: "System (battery)", color: "var(--batt)"}],
  {min: 0, lines: D.pl1 ? [{v: D.pl1, label: `PL1 ${D.pl1} W`}] : []});
chart("CPU clock", [{key: "cpu_mhz", label: "Busy cores", color: "var(--cpu)"}], {min: 0});
chart("GPU clock", [{key: "gpu_req_mhz", label: "Asked", color: "var(--rest)"}, {key: "gpu_mhz", label: "Actual", color: "var(--gpu)", width: 1.8}], {min: 0});
chart("Load", [{key: "cpu_pct", label: "CPU", color: "var(--cpu)"}, {key: "gpu_pct", label: "GPU", color: "var(--gpu)"}], {min: 0, max: 100});
chart("Temperature", [{key: "cpu_c", label: "CPU", color: "var(--cpu)"}, {key: "ddr_c", label: "DDR", color: "var(--mem)"},
  {key: "battery_c", label: "Battery", color: "var(--batt)"}], {min: 20});
chart("Fan", [{key: "fan_rpm", label: "Fan rpm", color: "var(--ink-2)"}], {min: 0});
chart("Memory", [{key: "mem_used_gb", label: "RAM used", color: "var(--mem)"}], {min: 0});
chart("Battery", [{key: "battery_pct", label: "Charge", color: "var(--batt)"}], {min: 0, max: 100});

(function () {
  const host = document.getElementById("c-split");
  if (!host || D.avg.package_w == null) return;
  const parts = [["CPU cores", D.avg.cpu_w, "var(--cpu)"], ["GPU", D.avg.gpu_w, "var(--gpu)"], ["Rest", D.avg.rest_w, "var(--rest)"]];
  const max = Math.max(35, Math.ceil((D.pl1 || 0) / 5) * 5, Math.ceil(D.avg.package_w / 5) * 5);
  const SW = 760, SL = 10, SR = 60, bh = 26;
  const svg = el("svg", {viewBox: `0 0 ${SW} 64`, role: "img", "aria-label": "Average CPU, GPU and rest-of-package watts"}, host);
  const x = v => SL + v / max * (SW - SL - SR);
  for (let t = 0; t <= max; t += 5) {
    el("line", {x1: x(t), x2: x(t), y1: 14, y2: 18 + bh, stroke: "var(--grid)"}, svg);
    el("text", {x: x(t), y: 10, "text-anchor": "middle", "font-size": 10.5, fill: "var(--ink-3)", "font-family": "var(--f-mono)"}, svg, t + (t === max ? " W" : ""));
  }
  let x0 = 0;
  parts.forEach(([label, v, col]) => {
    if (v == null) return;
    const w = x(x0 + v) - x(x0);
    el("rect", {x: x(x0), y: 16, width: Math.max(0, w - 1.5), height: bh, fill: col, rx: 2}, svg);
    if (w > 70) el("text", {x: x(x0) + 6, y: 16 + bh / 2 + 4, "font-size": 11.5, fill: label === "Rest" ? "var(--ink)" : "#fff"}, svg, `${label} ${v.toFixed(1)} W`);
    x0 += v;
  });
  el("text", {x: x(D.avg.package_w) + 6, y: 16 + bh / 2 + 4, "font-size": 11.5, fill: "var(--ink)", "font-family": "var(--f-mono)"}, svg, D.avg.package_w.toFixed(1) + " W");
  if (D.pl1) el("line", {x1: x(D.pl1), x2: x(D.pl1), y1: 12, y2: 20 + bh, stroke: "var(--ink-2)", "stroke-width": 1.5, "stroke-dasharray": "3 3"}, svg);
})();
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_closes_every_door() {
        assert_eq!(
            e("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn dates_are_local_and_readable() {
        // 2026-11-16 22:33:00 UTC, one hour east.
        let off = |_: u64| 3600i64;
        assert_eq!(long_date(1_794_868_380, &off), "16 November 2026, 23:33");
        assert_eq!(short_date(1_794_868_380, &off), "16 Nov 23:33");
    }

    #[test]
    fn a_page_is_made_from_rows_and_a_hostile_label_stays_text() {
        let rows: Vec<Row> = (0..90u64)
            .map(|t| Row {
                t_s: t,
                unix_time: 1_794_868_380 + t,
                fps: Some(60.0),
                fps_app: Some("game</script>".into()),
                package_w: Some(20.0),
                cpu_w: Some(8.0),
                gpu_w: Some(9.0),
                gpu_mhz: Some(1950),
                gpu_throttle: Some("pl4".into()),
                on_ac: Some(true),
                pl1_w: Some(25),
                ..Row::default()
            })
            .collect();
        let s = RunSummary::from_rows(&rows, None);
        let html = summary_html(&rows, &s, None, "<b>label</b>", "x.csv", &|_| 0);
        assert!(html.contains("<meta name=\"fwhelper-session\" content=\"x.csv\">"));
        assert!(html.contains("&lt;b&gt;label&lt;/b&gt;"));
        // The app name appears in the embedded data, escaped so it cannot end the script.
        assert_eq!(html.matches("</script>").count(), 1);
        assert!(html.contains("Median FPS"));
    }
}
