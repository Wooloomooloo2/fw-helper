//! Compare projects: several recordings against one of them, the baseline.
//!
//! Ported from the Windows sibling's `CompareProject` and `CompareReport`, with the
//! same project file format (`{"name", "baseline", "runs": [{"file", "name"}]}`), so
//! a project made on one reads on the other. Stored as
//! `FW Benchmark/projects/<name>.json`; the report is `<name>.html` beside it.
//!
//! The report is static SVG with hover titles: no script, and it prints.

use crate::report::{e, n0, pct, short_date, throttle_meaning, w1, CSS_VARS};
use fw_helper_core::json::{self, Value};
use fw_helper_core::notes::RunNotes;
use fw_helper_core::summary::{RunSummary, Share, SHORT_RUN_SECS, TRIM_SECS};
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub const MAX_RUNS: usize = 12;
/// Steady lengths further apart than this ratio are flagged: heat soak makes long runs
/// slower.
pub const LENGTH_RATIO: f64 = 2.0;
pub const PROJECT_META: &str = "fwhelper-project";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareRun {
    /// A file name inside the benchmark folder, or a full path to one elsewhere.
    pub file: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompareProject {
    pub name: String,
    pub baseline: usize,
    pub runs: Vec<CompareRun>,
}

impl CompareProject {
    pub fn baseline_index(&self) -> usize {
        if self.runs.is_empty() {
            0
        } else {
            self.baseline.min(self.runs.len() - 1)
        }
    }

    pub fn to_json(&self) -> String {
        json::object([
            ("name", Some(Value::String(self.name.clone()))),
            (
                "baseline",
                Some(Value::Number(self.baseline_index() as f64)),
            ),
            (
                "runs",
                Some(Value::Array(
                    self.runs
                        .iter()
                        .map(|r| {
                            json::object([
                                ("file", Some(Value::String(r.file.clone()))),
                                ("name", Some(Value::String(r.name.clone()))),
                            ])
                        })
                        .collect(),
                )),
            ),
        ])
        .pretty()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let v = json::parse(text)?;
        let runs = v
            .get("runs")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                let file = r.str_field("file").filter(|f| !f.trim().is_empty())?;
                Some(CompareRun {
                    file,
                    name: r.str_field("name").unwrap_or_default(),
                })
            })
            .collect();
        let mut p = Self {
            name: v.str_field("name").unwrap_or_default(),
            baseline: v
                .get("baseline")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0) as usize,
            runs,
        };
        p.baseline = p.baseline_index();
        Ok(p)
    }

    /// A recording in the benchmark folder is stored by file name, so the folder can
    /// move; anything else by its full path.
    pub fn store(bench_dir: &Path, csv: &Path) -> String {
        match (csv.parent(), csv.file_name().and_then(|n| n.to_str())) {
            (Some(dir), Some(name)) if dir == bench_dir => name.to_string(),
            _ => csv.display().to_string(),
        }
    }

    pub fn resolve(bench_dir: &Path, stored: &str) -> PathBuf {
        let p = Path::new(stored);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            bench_dir.join(p)
        }
    }
}

/// A project file's path for a name typed by the user.
pub fn project_path(projects_dir: &Path, name: &str) -> PathBuf {
    let clean: String = name
        .chars()
        .map(|c| if c == '/' || c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clean = clean.trim_matches('.');
    let clean = if clean.is_empty() { "project" } else { clean };
    projects_dir.join(format!("{clean}{}", crate::bench::PROJECT_SUFFIX))
}

/// One run as the report sees it.
pub struct Entry {
    pub name: String,
    pub file: String,
    pub summary: RunSummary,
    pub notes: Option<RunNotes>,
}

/// Load every run of a project that can still be read. A run whose file has gone is
/// left out with a message on stderr rather than failing the whole comparison.
pub fn entries(p: &CompareProject) -> Vec<Entry> {
    let dir = crate::bench::bench_dir();
    p.runs
        .iter()
        .filter_map(|r| {
            let csv = CompareProject::resolve(&dir, &r.file);
            match crate::bench::summarise(&csv) {
                Ok((session, summary, notes)) => Some(Entry {
                    name: if r.name.trim().is_empty() {
                        session.meta.label.clone()
                    } else {
                        r.name.clone()
                    },
                    file: r.file.clone(),
                    summary,
                    notes,
                }),
                Err(e) => {
                    eprintln!("compare: skipping {}: {e}", csv.display());
                    None
                }
            }
        })
        .collect()
}

/// Save a project and write its report beside it; returns the report's path.
pub fn save_and_report(
    p: &CompareProject,
    utc_offset: &dyn Fn(u64) -> i64,
) -> std::io::Result<PathBuf> {
    let dir = crate::bench::projects_dir();
    std::fs::create_dir_all(&dir)?;
    let path = project_path(&dir, &p.name);
    std::fs::write(&path, p.to_json())?;
    let runs = entries(p);
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let html = html(&p.name, &runs, p.baseline_index(), &file, utc_offset);
    let out = path.with_extension("html");
    std::fs::write(&out, html)?;
    Ok(out)
}

/// Saved projects, by name.
pub fn list_projects() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(crate::bench::projects_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let proj = CompareProject::from_json(&std::fs::read_to_string(&p).ok()?).ok()?;
            let name = if proj.name.is_empty() {
                p.file_stem()?.to_str()?.to_string()
            } else {
                proj.name
            };
            Some((name, p))
        })
        .collect();
    out.sort();
    out
}

pub const SETTING_NAMES: [&str; 9] = [
    "Game",
    "Game settings",
    "Profile",
    "Fan",
    "Fan curve",
    "Power",
    "PL1",
    "BIOS",
    "Kernel",
];

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

/// A run's settings as recorded, in [`SETTING_NAMES`] order.
pub fn settings(r: &Entry) -> [String; 9] {
    let s = &r.summary;
    let n = r.notes.as_ref();
    let or = |v: Option<&String>, d: &str| v.cloned().unwrap_or_else(|| d.to_string());
    [
        s.game.clone().unwrap_or_else(|| "none detected".into()),
        or(n.and_then(|n| n.game_settings.as_ref()), "not noted"),
        mixed(&s.profiles),
        mixed(&s.fan),
        or(n.and_then(|n| n.fan_curve.as_ref()), "not recorded"),
        if s.on_ac_fraction >= 1.0 {
            "mains".into()
        } else if s.on_ac_fraction == 0.0 {
            "battery".into()
        } else {
            format!("mains {:.0} %", s.on_ac_fraction * 100.0)
        },
        s.power_target
            .map(|w| format!("{w} W"))
            .unwrap_or_else(|| "?".into()),
        or(n.and_then(|n| n.bios.as_ref()), "not recorded"),
        or(n.and_then(|n| n.kernel.as_ref()), "not recorded"),
    ]
}

/// Indexes into [`SETTING_NAMES`] of the settings that are not the same in every run.
pub fn differences(runs: &[Entry]) -> Vec<usize> {
    let all: Vec<[String; 9]> = runs.iter().map(settings).collect();
    (0..SETTING_NAMES.len())
        .filter(|&i| all.iter().any(|s| s[i] != all[0][i]))
        .collect()
}

fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// What makes these runs hard to compare.
pub fn warnings(runs: &[Entry], utc_offset: &dyn Fn(u64) -> i64) -> Vec<String> {
    let mut w = Vec::new();
    if runs.len() < 2 {
        w.push("A comparison needs at least two runs.".into());
    }
    let mut games: Vec<String> = runs.iter().filter_map(|r| r.summary.game.clone()).collect();
    games.sort();
    games.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if games.len() > 1 {
        w.push(format!(
            "The runs are of different games ({}): their FPS cannot be compared, only their power.",
            games.join(", ")
        ));
    }
    let no_game: Vec<String> = runs
        .iter()
        .filter(|r| r.summary.game.is_none())
        .map(|r| r.name.clone())
        .collect();
    if !no_game.is_empty() {
        w.push(format!(
            "No game was detected in {}, so {} no FPS.",
            list(&no_game),
            if no_game.len() == 1 {
                "it has"
            } else {
                "they have"
            }
        ));
    }
    let source = |s: &RunSummary| {
        if s.on_ac_fraction >= 1.0 {
            0
        } else if s.on_ac_fraction == 0.0 {
            1
        } else {
            2
        }
    };
    let mut sources: Vec<u8> = runs.iter().map(|r| source(&r.summary)).collect();
    sources.sort();
    sources.dedup();
    if sources.len() > 1 {
        w.push("Some runs were on battery and some on mains: on battery a heavy GPU load is held by PL2.".into());
    }
    let lengths: Vec<usize> = runs
        .iter()
        .map(|r| r.summary.steady.len())
        .filter(|&n| n > 0)
        .collect();
    if lengths.len() > 1 {
        let (lo, hi) = (
            *lengths.iter().min().unwrap(),
            *lengths.iter().max().unwrap(),
        );
        if hi as f64 > LENGTH_RATIO * lo as f64 {
            w.push(format!(
                "The runs differ a lot in length ({lo} s to {hi} s of steady play): longer runs include more heat soak."
            ));
        }
    }
    let short: Vec<String> = runs
        .iter()
        .filter(|r| (1..SHORT_RUN_SECS).contains(&r.summary.steady.len()))
        .map(|r| r.name.clone())
        .collect();
    if !short.is_empty() {
        w.push(format!(
            "{} {} under {SHORT_RUN_SECS} s of steady play: compare medians only.",
            list(&short),
            if short.len() == 1 { "has" } else { "have" }
        ));
    }
    let mut days: Vec<String> = runs
        .iter()
        .filter(|r| r.summary.start_unix > 0)
        .map(|r| {
            let d = short_date(r.summary.start_unix, utc_offset);
            d.rsplit_once(' ').map(|(a, _)| a.to_string()).unwrap_or(d)
        })
        .collect();
    days.dedup();
    let mut unique = days.clone();
    unique.sort();
    unique.dedup();
    if unique.len() > 1 {
        w.push(format!(
            "The runs were recorded on different days ({}). Check BIOS and kernel under Settings below - \
             a firmware update moves these numbers (BIOS 03.04 changed the GPU's limits).",
            days.join(", ")
        ));
    }
    // Each run's own warnings not already covered, said once however many runs share it.
    let mut own: Vec<(String, Vec<String>)> = Vec::new();
    for r in runs {
        for x in &r.summary.warnings {
            if x.starts_with("No game")
                || x.starts_with("Only ")
                || x.starts_with("The run was on battery")
            {
                continue;
            }
            match own.iter_mut().find(|(t, _)| t == x) {
                Some((_, names)) => names.push(r.name.clone()),
                None => own.push((x.clone(), vec![r.name.clone()])),
            }
        }
    }
    for (text, names) in own {
        w.push(if names.len() == runs.len() && runs.len() > 1 {
            format!("Every run: {text}")
        } else {
            format!("{}: {text}", list(&names))
        });
    }
    w
}

fn best(runs: &[Entry], get: impl Fn(&Entry) -> Option<f64>) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, r) in runs.iter().enumerate() {
        if let Some(v) = get(r) {
            if best.is_none_or(|(_, b)| v > b) {
                best = Some((i, v));
            }
        }
    }
    best.map(|(i, _)| i)
}

pub fn fastest(runs: &[Entry]) -> Option<usize> {
    best(runs, |r| r.summary.stat("fps").map(|s| s.median))
}

pub fn most_efficient(runs: &[Entry]) -> Option<usize> {
    best(runs, |r| r.summary.fps_per_watt())
}

/// Relative change from the baseline.
pub fn change(value: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    match (value, baseline) {
        (Some(v), Some(b)) if b != 0.0 => Some((v - b) / b),
        _ => None,
    }
}

/// "+11 %", "\u{2212}8 %", or "same" within half a percent.
pub fn signed(c: Option<f64>) -> String {
    match c {
        None => String::new(),
        Some(c) if c.abs() < 0.005 => "same".into(),
        Some(c) => format!(
            "{}{:.0} %",
            if c > 0.0 { "+" } else { "\u{2212}" },
            (c * 100.0).abs()
        ),
    }
}

fn signed_w(d: f64) -> String {
    if d.abs() < 0.05 {
        "same power".into()
    } else {
        format!("{}{:.1} W", if d > 0.0 { "+" } else { "\u{2212}" }, d.abs())
    }
}

/// One line per run against the baseline, then which is fastest and most efficient.
pub fn conclusions(runs: &[Entry], baseline: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(b) = runs.get(baseline) else {
        return lines;
    };
    let bs = &b.summary;
    for (i, r) in runs.iter().enumerate() {
        if i == baseline {
            continue;
        }
        let s = &r.summary;
        let mut parts = Vec::new();
        if let (Some(f), Some(bf)) = (s.stat("fps"), bs.stat("fps")) {
            parts.push(
                format!(
                    "{} FPS ({:.0} \u{2192} {:.0})",
                    signed(change(Some(f.median), Some(bf.median))),
                    bf.median,
                    f.median
                )
                .replace("same FPS", "the same FPS"),
            );
        }
        if let (Some(l), Some(bl)) = (
            s.frames.and_then(|f| f.low1),
            bs.frames.and_then(|f| f.low1),
        ) {
            parts.push(format!(
                "1 % low {} ({bl:.0} \u{2192} {l:.0})",
                signed(change(Some(l), Some(bl)))
            ));
        }
        if let (Some(p), Some(bp)) = (s.stat("package_w"), bs.stat("package_w")) {
            parts.push(format!(
                "{} package ({:.1} \u{2192} {:.1} W)",
                signed_w(p.avg - bp.avg),
                bp.avg,
                p.avg
            ));
        }
        if let Some(c) = change(s.fps_per_watt(), bs.fps_per_watt()) {
            parts.push(format!("FPS per watt {}", signed(Some(c))));
        }
        if parts.is_empty() {
            continue;
        }
        let mut line = format!("{} against {}: {}.", r.name, b.name, parts.join(", "));
        if let (Some(l), Some(bl)) = (s.gpu_throttle.first(), bs.gpu_throttle.first()) {
            if l.value != bl.value {
                let _ = write!(
                    line,
                    " The GPU clock was held mostly by {} instead of {}.",
                    l.value, bl.value
                );
            }
        }
        lines.push(line);
    }
    if runs.len() > 1 {
        if let (Some(f), Some(eff)) = (fastest(runs), most_efficient(runs)) {
            let fps = |i: usize| {
                runs[i]
                    .summary
                    .stat("fps")
                    .map(|s| s.median)
                    .unwrap_or_default()
            };
            let fpw = |i: usize| runs[i].summary.fps_per_watt().unwrap_or_default();
            lines.push(if f == eff {
                format!(
                    "{} is both the fastest ({:.0} FPS) and the most efficient ({:.2} FPS/W).",
                    runs[f].name,
                    fps(f),
                    fpw(f)
                )
            } else {
                format!(
                    "Fastest: {} ({:.0} FPS). Most efficient: {} ({:.2} FPS/W, {:.0} FPS).",
                    runs[f].name,
                    fps(f),
                    runs[eff].name,
                    fpw(eff),
                    fps(eff)
                )
            });
        }
    }
    lines
}

// ---------- the page ----------

const CW: f64 = 760.0;
const LABEL_W: f64 = 190.0;
const VALUE_W: f64 = 120.0;
const ROW_H: f64 = 34.0;

fn nice(max: f64) -> f64 {
    if max <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(max.log10().floor());
    let m = max / p;
    (if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 2.5 {
        2.5
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    }) * p
}

fn f1(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}\u{2026}", s.chars().take(n - 1).collect::<String>())
    }
}

fn svg_open(o: &mut String, rows: usize, label: &str) {
    let _ = write!(
        o,
        "<svg viewBox=\"0 0 {CW} {}\" role=\"img\" aria-label=\"{}\">",
        rows as f64 * ROW_H + 22.0,
        e(label)
    );
}

fn axis(o: &mut String, max: f64, x: &dyn Fn(f64) -> f64, rows: usize, unit: &str) {
    let step = nice(max / 5.0);
    let mut t = 0.0;
    while t <= max + 1e-9 {
        let _ = write!(
            o,
            "<line x1=\"{0}\" x2=\"{0}\" y1=\"14\" y2=\"{1}\" style=\"stroke:var(--grid)\"/>\
             <text x=\"{0}\" y=\"10\" text-anchor=\"middle\" class=\"ax\">{2}{3}</text>",
            f1(x(t)),
            f1(rows as f64 * ROW_H + 16.0),
            f1(t),
            if t + step > max + 1e-9 {
                format!(" {unit}")
            } else {
                String::new()
            }
        );
        t += step;
    }
}

fn row_label(o: &mut String, runs: &[Entry], r: usize, baseline: Option<usize>) {
    let y = 18.0 + r as f64 * ROW_H + ROW_H / 2.0;
    let _ = write!(
        o,
        "<text x=\"{}\" y=\"{}\" text-anchor=\"end\" class=\"rl{}\">{}</text>",
        LABEL_W - 10.0,
        f1(y + 4.0),
        if Some(r) == baseline { " base" } else { "" },
        e(&clip(&runs[r].name, 26))
    );
}

fn tile(o: &mut String, key: &str, value: &str, detail: &str) {
    let _ = write!(
        o,
        "<div class=\"tile\"><div class=\"eyebrow\">{}</div><div class=\"k\">{value}</div><div class=\"d\">{detail}</div></div>",
        e(key)
    );
}

fn run_cell(runs: &[Entry], r: usize, baseline: usize) -> String {
    format!(
        "{}{}<br><span class=\"u mono\">{}</span>",
        e(&runs[r].name),
        if r == baseline {
            " <span class=\"tag\">baseline</span>"
        } else {
            ""
        },
        e(&runs[r].file)
    )
}

pub fn html(
    project: &str,
    runs: &[Entry],
    baseline: usize,
    project_file: &str,
    utc_offset: &dyn Fn(u64) -> i64,
) -> String {
    let baseline = if runs.is_empty() {
        0
    } else {
        baseline.min(runs.len() - 1)
    };
    let mut o = String::with_capacity(32 * 1024);
    o.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    o.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(
        o,
        "<meta name=\"{PROJECT_META}\" content=\"{}\">",
        e(project_file)
    );
    let _ = write!(
        o,
        "<title>{} \u{b7} compare</title>\n<style>{CSS_VARS}{CHART_CSS}</style></head><body>\n<div class=\"wrap\">\n",
        e(project)
    );
    let _ = write!(
        o,
        "<header><div class=\"eyebrow\">fw-helper comparison \u{b7} {} runs</div><h1>{}</h1>",
        runs.len(),
        e(project)
    );
    if let Some(b) = runs.get(baseline) {
        let _ = write!(
            o,
            "<p class=\"lede\">Baseline <b>{}</b>. Every figure covers each run's steady seconds only \
             (the game presenting frames, from {TRIM_SECS} s in, menus left out), as in the run summaries.</p>",
            e(&b.name)
        );
    }
    o.push_str("</header>\n");

    let warn = warnings(runs, utc_offset);
    if !warn.is_empty() {
        o.push_str("<div class=\"warn\"><h3>Read with care</h3><ul>");
        for w in &warn {
            let _ = write!(o, "<li>{}</li>", e(w));
        }
        o.push_str("</ul></div>\n");
    }
    if runs.is_empty() {
        return end(o, project_file);
    }
    let b = &runs[baseline].summary;

    o.push_str("<div class=\"tiles\">");
    if let Some(fi) = fastest(runs) {
        let f = &runs[fi].summary;
        let med = f.stat("fps").map(|s| s.median).unwrap_or_default();
        tile(
            &mut o,
            "Fastest",
            &e(&runs[fi].name),
            &if fi == baseline {
                format!("{med:.0} FPS median: the baseline itself")
            } else {
                format!(
                    "{med:.0} FPS median, {} on the baseline",
                    signed(change(Some(med), b.stat("fps").map(|s| s.median)))
                )
            },
        );
        if let Some(limit) = f.gpu_throttle.first().map(|s| s.value.clone()) {
            let base_limit = b.gpu_throttle.first().map(|s| s.value.clone());
            let same = fi == baseline || Some(&limit) == base_limit.as_ref();
            tile(
                &mut o,
                "GPU held by",
                &if same {
                    e(&limit)
                } else {
                    format!(
                        "{} \u{2192} {}",
                        e(base_limit.as_deref().unwrap_or("?")),
                        e(&limit)
                    )
                },
                &if same {
                    format!(
                        "the same limit in the baseline and the fastest run: {}",
                        e(&throttle_meaning(&limit))
                    )
                } else {
                    format!("what held the GPU clock most of the time, baseline \u{2192} fastest. Now: {}", e(&throttle_meaning(&limit)))
                },
            );
        }
    }
    if let Some(ei) = most_efficient(runs) {
        let v = runs[ei].summary.fps_per_watt().unwrap_or_default();
        tile(
            &mut o,
            "Most efficient",
            &e(&runs[ei].name),
            &format!(
                "{v:.2} FPS per watt{}",
                if ei == baseline {
                    ": the baseline itself".to_string()
                } else {
                    format!(
                        ", {} on the baseline",
                        signed(change(Some(v), b.fps_per_watt()))
                    )
                }
            ),
        );
    }
    o.push_str("</div>\n");

    let concl = conclusions(runs, baseline);
    if !concl.is_empty() {
        o.push_str("<section><div class=\"eyebrow\">Conclusions</div><h2>What changed</h2><ul class=\"concl\">");
        for c in &concl {
            let _ = write!(o, "<li>{}</li>", e(c));
        }
        o.push_str("</ul></section>\n");
    }

    let mut section = 1;
    if runs.iter().any(|r| r.summary.stat("fps").is_some()) {
        let with: Vec<&Entry> = runs
            .iter()
            .filter(|r| r.summary.stat("fps").is_some())
            .collect();
        let lows = with
            .iter()
            .filter(|r| r.summary.frames.and_then(|f| f.low1).is_some())
            .count();
        let slow = if lows == with.len() {
            "1 % low"
        } else if lows == 0 {
            "5th percentile second (the slow seconds)"
        } else {
            "1 % low, or the 5th percentile second where a run has no frame times"
        };
        let _ = write!(
            o,
            "<section><div class=\"eyebrow\">{section} \u{b7} Frame rate</div><h2>FPS</h2><div class=\"chart\">\
             <div class=\"legend\"><span><i class=\"sw\" style=\"background:var(--fps)\"></i>Median</span>\
             <span><i class=\"sw\" style=\"background:var(--fps-2)\"></i>{}</span></div>",
            e(slow)
        );
        section += 1;
        fps_chart(&mut o, runs, baseline);
        o.push_str("</div><p class=\"note\">The 1 % low is the frame rate of the slowest 1 % of frames: far below the median means stutter.</p></section>\n");
    }
    if runs.iter().any(|r| r.summary.stat("package_w").is_some()) {
        let _ = write!(
            o,
            "<section><div class=\"eyebrow\">{section} \u{b7} Power</div><h2>Where the watts went</h2><div class=\"chart\">\
             <div class=\"legend\"><span><i class=\"sw\" style=\"background:var(--cpu)\"></i>CPU cores</span>\
             <span><i class=\"sw\" style=\"background:var(--gpu)\"></i>GPU</span>\
             <span><i class=\"sw\" style=\"background:var(--rest)\"></i>Rest of package</span>\
             <span><i class=\"sw dash\"></i>The run's PL1</span></div>"
        );
        section += 1;
        power_chart(&mut o, runs);
        o.push_str("</div></section>\n");
    }
    if runs.iter().any(|r| r.summary.fps_per_watt().is_some()) {
        let _ = write!(
            o,
            "<section><div class=\"eyebrow\">{section} \u{b7} Efficiency</div><h2>FPS per watt</h2><div class=\"chart\">"
        );
        section += 1;
        bars(
            &mut o,
            runs,
            baseline,
            |r| r.summary.fps_per_watt(),
            "FPS per watt",
        );
        o.push_str("</div><p class=\"note\">Median FPS \u{f7} average package watts. On battery this is what decides how long a game lasts.</p></section>\n");
    }

    // What differs
    let _ = write!(
        o,
        "<section><div class=\"eyebrow\">{section} \u{b7} Settings</div><h2>What differs</h2>"
    );
    section += 1;
    let diff = differences(runs);
    let all: Vec<[String; 9]> = runs.iter().map(settings).collect();
    if diff.is_empty() {
        o.push_str("<p>Every recorded setting is the same in all runs.</p>");
    } else {
        o.push_str("<div class=\"tbl\"><table><thead><tr><th>Run</th>");
        for &i in &diff {
            let _ = write!(o, "<th class=\"l\">{}</th>", e(SETTING_NAMES[i]));
        }
        o.push_str("</tr></thead><tbody>");
        for r in 0..runs.len() {
            let _ = write!(o, "<tr><td>{}</td>", run_cell(runs, r, baseline));
            for &i in &diff {
                let chg = r != baseline && all[r][i] != all[baseline][i];
                let _ = write!(
                    o,
                    "<td class=\"l{}\">{}</td>",
                    if chg { " chg" } else { "" },
                    e(&all[r][i])
                );
            }
            o.push_str("</tr>");
        }
        o.push_str("</tbody></table></div>");
    }
    let same: Vec<String> = (0..SETTING_NAMES.len())
        .filter(|i| !diff.contains(i))
        .map(|i| format!("{} {}", SETTING_NAMES[i], all[0][i]))
        .collect();
    if !same.is_empty() {
        let _ = write!(
            o,
            "<p class=\"note\">The same in every run: {}. Game settings come from each run's notes, typed when the recording stops or later from the Monitor page.</p>",
            e(&same.join(" \u{b7} "))
        );
    }
    o.push_str("</section>\n");

    // Every figure, run by run
    let _ = write!(
        o,
        "<section><div class=\"eyebrow\">{section} \u{b7} Figures</div><h2>Run by run</h2><div class=\"tbl\"><table><thead><tr><th>Run</th>"
    );
    type Get = fn(&RunSummary) -> Option<f64>;
    let cols: Vec<(&str, Get, usize, bool)> = vec![
        ("Median FPS", |s| s.stat("fps").map(|x| x.median), 0, true),
        ("1 % low", |s| s.frames.and_then(|f| f.low1), 0, true),
        ("5th pct FPS", |s| s.stat("fps").map(|x| x.p5), 0, true),
        ("Package W", |s| s.stat("package_w").map(|x| x.avg), 1, true),
        ("CPU W", |s| s.stat("cpu_w").map(|x| x.avg), 1, true),
        ("GPU W", |s| s.stat("gpu_w").map(|x| x.avg), 1, true),
        ("FPS/W", |s| s.fps_per_watt(), 2, true),
        ("CPU MHz", |s| s.stat("cpu_mhz").map(|x| x.avg), 0, false),
        ("GPU MHz", |s| s.stat("gpu_mhz").map(|x| x.avg), 0, false),
        ("CPU \u{b0}C", |s| s.stat("cpu_c").map(|x| x.avg), 0, false),
        ("Fan rpm", |s| s.stat("fan_rpm").map(|x| x.avg), 0, false),
        ("Steady s", |s| Some(s.steady.len() as f64), 0, false),
    ];
    let cols: Vec<_> = cols
        .into_iter()
        .filter(|c| runs.iter().any(|r| (c.1)(&r.summary).is_some()))
        .collect();
    for c in &cols {
        let _ = write!(o, "<th>{}</th>", e(c.0));
    }
    o.push_str("<th class=\"l\">GPU held by</th></tr></thead><tbody>");
    for r in 0..runs.len() {
        let s = &runs[r].summary;
        let _ = write!(o, "<tr><td>{}</td>", run_cell(runs, r, baseline));
        for c in &cols {
            let v = (c.1)(s)
                .map(|d| format!("{d:.*}", c.2))
                .unwrap_or_else(|| "\u{2013}".into());
            let delta = if c.3 && r != baseline {
                let ch = signed(change((c.1)(s), (c.1)(b)));
                if ch.is_empty() {
                    String::new()
                } else {
                    format!("<br><span class=\"u\">{ch}</span>")
                }
            } else {
                String::new()
            };
            let _ = write!(o, "<td>{v}{delta}</td>");
        }
        let held = s
            .gpu_throttle
            .first()
            .map(|t| format!("{} {}", t.value, pct(t.fraction)))
            .unwrap_or_else(|| "\u{2013}".into());
        let _ = write!(o, "<td class=\"l\">{}</td></tr>", e(&held));
    }
    o.push_str("</tbody></table></div><p class=\"note\">Each run's own summary has the second-by-second charts: open the run on the Monitor page and press Summary.</p></section>\n");
    end(o, project_file)
}

fn end(mut o: String, project_file: &str) -> String {
    let _ = write!(
        o,
        "<footer class=\"note\">Made by fw-helper {} from <span class=\"mono\">{}</span>. \
         Monitor \u{2192} Compare opens the project again, to add runs or make this report again.</footer>\n</div></body></html>\n",
        e(env!("CARGO_PKG_VERSION")),
        e(project_file)
    );
    o
}

fn fps_chart(o: &mut String, runs: &[Entry], baseline: usize) {
    let max = nice(
        runs.iter()
            .filter_map(|r| r.summary.stat("fps").map(|s| s.median))
            .fold(0.0, f64::max)
            * 1.05,
    );
    let x = |v: f64| LABEL_W + v / max * (CW - LABEL_W - VALUE_W);
    svg_open(o, runs.len(), "Median and low FPS per run");
    axis(o, max, &x, runs.len(), "FPS");
    let bm = runs[baseline].summary.stat("fps").map(|s| s.median);
    for (r, run) in runs.iter().enumerate() {
        row_label(o, runs, r, Some(baseline));
        let Some(f) = run.summary.stat("fps") else {
            continue;
        };
        let y = 18.0 + r as f64 * ROW_H + 6.0;
        let (low, low_name) = match run.summary.frames.and_then(|f| f.low1) {
            Some(l) => (l, "1 % low"),
            None => (f.p5, "5th percentile second"),
        };
        let ch = if r == baseline {
            String::new()
        } else {
            signed(change(Some(f.median), bm))
        };
        let _ = write!(
            o,
            "<g><title>{}: median {:.0} FPS, {low_name} {low:.0}, range {:.0}\u{2013}{:.0}</title>\
             <rect x=\"{LABEL_W}\" y=\"{}\" width=\"{}\" height=\"14\" rx=\"2\" style=\"fill:var(--fps)\"/>\
             <rect x=\"{LABEL_W}\" y=\"{}\" width=\"{}\" height=\"6\" rx=\"1.5\" style=\"fill:var(--fps-2)\"/>\
             <text x=\"{}\" y=\"{}\" class=\"val\">{:.0}<tspan class=\"dl\"> {}</tspan></text></g>",
            e(&run.name),
            f.median,
            f.min,
            f.max,
            f1(y),
            f1((x(f.median) - LABEL_W).max(0.0)),
            f1(y + 16.0),
            f1((x(low) - LABEL_W).max(0.0)),
            f1(x(f.median) + 6.0),
            f1(y + 12.0),
            f.median,
            e(&ch)
        );
    }
    o.push_str("</svg>");
}

fn power_chart(o: &mut String, runs: &[Entry]) {
    let top = runs
        .iter()
        .map(|r| {
            r.summary
                .stat("package_w")
                .map(|s| s.avg)
                .unwrap_or_default()
                .max(r.summary.power_target.unwrap_or(0) as f64)
        })
        .fold(0.0, f64::max);
    let max = 35f64.max((top / 5.0).ceil() * 5.0);
    let x = |v: f64| LABEL_W + v / max * (CW - LABEL_W - VALUE_W);
    svg_open(
        o,
        runs.len(),
        "Average CPU, GPU and rest-of-package watts per run",
    );
    axis(o, max, &x, runs.len(), "W");
    for (r, run) in runs.iter().enumerate() {
        row_label(o, runs, r, None);
        let s = &run.summary;
        let Some(pkg) = s.stat("package_w") else {
            continue;
        };
        let y = 18.0 + r as f64 * ROW_H + 6.0;
        let avg = |k: &str| s.stat(k).map(|x| x.avg);
        let _ = write!(
            o,
            "<g><title>{}: package {} W = CPU {} + GPU {} + rest {}{}</title>",
            e(&run.name),
            w1(Some(pkg.avg)),
            w1(avg("cpu_w")),
            w1(avg("gpu_w")),
            w1(avg("rest_w")),
            s.power_target
                .map(|p| format!("; PL1 {p} W"))
                .unwrap_or_default()
        );
        let mut x0 = 0.0;
        for (key, colour) in [
            ("cpu_w", "var(--cpu)"),
            ("gpu_w", "var(--gpu)"),
            ("rest_w", "var(--rest)"),
        ] {
            let Some(v) = avg(key).filter(|v| *v > 0.0) else {
                continue;
            };
            let _ = write!(
                o,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"22\" rx=\"2\" style=\"fill:{colour}\"/>",
                f1(x(x0)),
                f1(y),
                f1((x(x0 + v) - x(x0) - 1.5).max(0.0))
            );
            x0 += v;
        }
        let mut label_x = x(pkg.avg);
        if let Some(pl) = s.power_target {
            let px = x(pl as f64);
            label_x = label_x.max(px);
            let _ = write!(
                o,
                "<line x1=\"{0}\" x2=\"{0}\" y1=\"{1}\" y2=\"{2}\" style=\"stroke:var(--ink-2)\" stroke-width=\"1.5\" stroke-dasharray=\"3 3\"/>",
                f1(px),
                f1(y - 3.0),
                f1(y + 25.0)
            );
        }
        let _ = write!(
            o,
            "<text x=\"{}\" y=\"{}\" class=\"val\">{} W</text></g>",
            f1(label_x + 6.0),
            f1(y + 15.0),
            w1(Some(pkg.avg))
        );
    }
    o.push_str("</svg>");
}

fn bars(
    o: &mut String,
    runs: &[Entry],
    baseline: usize,
    get: impl Fn(&Entry) -> Option<f64>,
    what: &str,
) {
    let max = nice(runs.iter().filter_map(&get).fold(0.0, f64::max) * 1.05);
    let x = |v: f64| LABEL_W + v / max * (CW - LABEL_W - VALUE_W);
    svg_open(o, runs.len(), &format!("{what} per run"));
    axis(o, max, &x, runs.len(), "");
    let bv = get(&runs[baseline]);
    for (r, run) in runs.iter().enumerate() {
        row_label(o, runs, r, Some(baseline));
        let Some(v) = get(run) else { continue };
        let y = 18.0 + r as f64 * ROW_H + 6.0;
        let ch = if r == baseline {
            String::new()
        } else {
            signed(change(Some(v), bv))
        };
        let _ = write!(
            o,
            "<g><title>{}: {v:.2} {}</title><rect x=\"{LABEL_W}\" y=\"{}\" width=\"{}\" height=\"22\" rx=\"2\" style=\"fill:var(--batt)\"/>\
             <text x=\"{}\" y=\"{}\" class=\"val\">{v:.2}<tspan class=\"dl\"> {}</tspan></text></g>",
            e(&run.name),
            e(what),
            f1(y),
            f1((x(v) - LABEL_W).max(0.0)),
            f1(x(v) + 6.0),
            f1(y + 15.0),
            e(&ch)
        );
    }
    o.push_str("</svg>");
    let _ = n0(None);
}

const CHART_CSS: &str = r#"
.concl { margin: 0; padding-left: 1.1em; display: flex; flex-direction: column; gap: 6px; max-width: 72ch; }
.chart .ax { font: 10.5px var(--f-mono); fill: var(--ink-3); }
.chart .rl { font-size: 12.5px; fill: var(--ink-2); }
.chart .rl.base { fill: var(--ink); font-weight: 600; }
.chart .val { font: 600 12px var(--f-mono); fill: var(--ink); }
.chart .dl { font-weight: 400; fill: var(--ink-3); }
.chart g:hover rect { opacity: 0.85; }
.tile .k { font-size: 1.3rem; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use fw_helper_core::session::Row;

    fn entry(name: &str, fps: f64, watts: f64) -> Entry {
        let rows: Vec<Row> = (0..120u64)
            .map(|t| Row {
                t_s: t,
                unix_time: 1_794_868_380 + t,
                fps: Some(fps),
                fps_app: Some("game".into()),
                package_w: Some(watts),
                cpu_w: Some(watts * 0.4),
                gpu_w: Some(watts * 0.4),
                gpu_mhz: Some(1950),
                gpu_throttle: Some("pl4".into()),
                on_ac: Some(true),
                pl1_w: Some(25),
                profile: Some("game".into()),
                ..Row::default()
            })
            .collect();
        Entry {
            name: name.into(),
            file: format!("{name}.csv"),
            summary: RunSummary::from_rows(&rows, None),
            notes: None,
        }
    }

    #[test]
    fn the_project_file_round_trips_and_matches_windows() {
        let p = CompareProject {
            name: "CP2077 PL1".into(),
            baseline: 1,
            runs: vec![
                CompareRun {
                    file: "a.csv".into(),
                    name: "25 W".into(),
                },
                CompareRun {
                    file: "/abs/b.csv".into(),
                    name: "35 W".into(),
                },
            ],
        };
        assert_eq!(CompareProject::from_json(&p.to_json()).unwrap(), p);
        // The Windows build writes camelCase with these keys.
        let w = CompareProject::from_json(
            r#"{"name":"x","baseline":5,"runs":[{"file":"s.csv","name":"A"},{"file":"","name":"dropped"}]}"#,
        )
        .unwrap();
        assert_eq!(w.runs.len(), 1);
        assert_eq!(
            w.baseline, 0,
            "an out-of-range baseline is clamped into the list"
        );
    }

    #[test]
    fn runs_inside_the_folder_are_stored_by_name() {
        let dir = Path::new("/home/u/Documents/FW Benchmark");
        assert_eq!(CompareProject::store(dir, &dir.join("a.csv")), "a.csv");
        assert_eq!(
            CompareProject::store(dir, Path::new("/tmp/b.csv")),
            "/tmp/b.csv"
        );
        assert_eq!(CompareProject::resolve(dir, "a.csv"), dir.join("a.csv"));
        assert_eq!(
            CompareProject::resolve(dir, "/tmp/b.csv"),
            Path::new("/tmp/b.csv")
        );
        assert_eq!(project_path(dir, " my/proj. "), dir.join("my proj.json"));
    }

    #[test]
    fn conclusions_name_the_change_and_the_winner() {
        let runs = vec![
            entry("25 W", 48.0, 21.0),
            entry("35 W", 48.2, 21.1),
            entry("15 W", 36.6, 15.0),
        ];
        let c = conclusions(&runs, 0);
        assert!(
            c[0].starts_with("35 W against 25 W: the same FPS (48 \u{2192} 48)"),
            "{}",
            c[0]
        );
        assert!(c[1].contains("\u{2212}24 % FPS"), "{}", c[1]);
        assert!(c.last().unwrap().starts_with("Fastest: 35 W"), "{c:?}");
        assert_eq!(fastest(&runs), Some(1));
        assert_eq!(most_efficient(&runs), Some(2));
    }

    #[test]
    fn signs_read_like_a_person_wrote_them() {
        assert_eq!(signed(change(Some(110.0), Some(100.0))), "+10 %");
        assert_eq!(signed(change(Some(92.0), Some(100.0))), "\u{2212}8 %");
        assert_eq!(signed(change(Some(100.2), Some(100.0))), "same");
        assert_eq!(signed(change(Some(1.0), Some(0.0))), "");
    }

    #[test]
    fn the_page_renders_with_and_without_runs() {
        let runs = vec![entry("A", 60.0, 20.0), entry("B", 66.0, 24.0)];
        let page = html("Test <project>", &runs, 0, "p.json", &|_| 0);
        assert!(page.contains("Test &lt;project&gt;"));
        assert!(page.contains("What changed"));
        assert!(page.contains("+10 %"));
        let empty = html("Empty", &[], 0, "p.json", &|_| 0);
        assert!(empty.contains("needs at least two runs"));
    }
}
