//! What one recorded run did, in numbers: the arithmetic behind the run summary and the
//! compare report.
//!
//! Ported from the Windows sibling's `RunSummary` so that a summary means the same on
//! both, and pure - rows in, figures out - so it is tested against fixed rows rather
//! than against a game.
//!
//! ## Steady seconds
//!
//! A recording includes things nobody means to measure: the game loading, compiling
//! shaders, sitting in a menu running uncapped at 400 fps. So the figures cover only
//! the **steady** seconds:
//!
//! - with a game (a program presenting frames), the seconds it was presenting, from
//!   [`TRIM_SECS`] after its first frame - as long as at least 20 s are left;
//! - minus any second above [`MENU_FACTOR`] times the median frame rate, taken as a
//!   menu or loading screen. Real dips stay in; only the uncapped highs go;
//! - without a game, every second.

use crate::frames::{Frame, MAX_FRAME_MS};
use crate::session::Row;

/// Seconds trimmed from the start of play: loading and shader compiles.
pub const TRIM_SECS: u64 = 10;
/// Below this many steady seconds a 5th percentile is too noisy to compare runs on.
pub const SHORT_RUN_SECS: usize = 60;
/// Seconds above this multiple of the median frame rate are menus or loading screens.
pub const MENU_FACTOR: f64 = 2.0;

/// Min / mean / max / median / 5th percentile of one column over the steady seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    pub median: f64,
    pub p5: f64,
    pub count: usize,
}

impl Stat {
    pub fn of(values: impl IntoIterator<Item = f64>) -> Option<Self> {
        let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        Some(Self {
            min: v[0],
            avg: v.iter().sum::<f64>() / v.len() as f64,
            max: v[v.len() - 1],
            median: percentile(&v, 50.0),
            p5: percentile(&v, 5.0),
            count: v.len(),
        })
    }
}

/// One value of a text column and its share of the steady seconds (0-1).
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    pub value: String,
    pub fraction: f64,
}

/// Frame times over the steady seconds. The lows are the frame rate of the 99th and
/// 99.9th percentile frame time - the usual definition (PresentMon, CapFrameX), and
/// what shows stutter that one-second averages hide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStat {
    pub count: usize,
    pub avg_fps: f64,
    pub low1: Option<f64>,
    pub low01: Option<f64>,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl FrameStat {
    /// Fewer frames than these and a percentile is a handful of frames.
    pub const MIN_FOR_1: usize = 100;
    pub const MIN_FOR_01: usize = 1000;

    pub fn of(frame_ms: impl IntoIterator<Item = f64>) -> Option<Self> {
        let mut v: Vec<f64> = frame_ms
            .into_iter()
            .filter(|x| *x > 0.0 && x.is_finite())
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let (p99, p999) = (percentile(&v, 99.0), percentile(&v, 99.9));
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        Some(Self {
            count: v.len(),
            avg_fps: 1000.0 / mean,
            low1: (v.len() >= Self::MIN_FOR_1).then(|| 1000.0 / p99),
            low01: (v.len() >= Self::MIN_FOR_01).then(|| 1000.0 / p999),
            p99_ms: p99,
            max_ms: v[v.len() - 1],
        })
    }
}

/// A summarised column: its key (the CSV-ish name the reports use), a label, a unit and
/// how to read it from a row.
pub struct Column {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub get: fn(&Row) -> Option<f64>,
}

/// The CPU temperature with a usable limit: coretemp's package sensor, else PECI.
pub fn cpu_c(r: &Row) -> Option<f64> {
    r.coretemp_c.or(r.peci_c)
}

/// What the package spent outside the cores and the iGPU: fabric, memory controller,
/// I/O. Clamped at zero - the rails are sampled a moment apart and can overshoot.
pub fn rest_w(r: &Row) -> Option<f64> {
    Some((r.package_w? - r.cpu_w? - r.gpu_w?).max(0.0))
}

pub const COLUMNS: &[Column] = &[
    Column {
        key: "fps",
        label: "FPS",
        unit: "",
        get: |r| r.fps,
    },
    Column {
        key: "package_w",
        label: "Package",
        unit: "W",
        get: |r| r.package_w,
    },
    Column {
        key: "cpu_w",
        label: "CPU cores",
        unit: "W",
        get: |r| r.cpu_w,
    },
    Column {
        key: "gpu_w",
        label: "GPU",
        unit: "W",
        get: |r| r.gpu_w,
    },
    Column {
        key: "rest_w",
        label: "Rest of package",
        unit: "W",
        get: rest_w,
    },
    Column {
        key: "system_w",
        label: "System (battery)",
        unit: "W",
        get: |r| r.system_w,
    },
    Column {
        key: "cpu_pct",
        label: "CPU load",
        unit: "%",
        get: |r| r.cpu_pct,
    },
    Column {
        key: "gpu_pct",
        label: "GPU load",
        unit: "%",
        get: |r| r.gpu_pct,
    },
    Column {
        key: "cpu_mhz",
        label: "CPU clock (busy cores)",
        unit: "MHz",
        get: |r| r.cpu_mhz_busy.map(|v| v as f64),
    },
    Column {
        key: "gpu_mhz",
        label: "GPU clock",
        unit: "MHz",
        // 0 is the GT asleep in RC6 at the instant of the read, not a clock: averaging
        // it in would report an idle-ish GPU as running at a fraction of its speed.
        get: |r| r.gpu_mhz.filter(|&v| v > 0).map(|v| v as f64),
    },
    Column {
        key: "gpu_req_mhz",
        label: "GPU clock asked",
        unit: "MHz",
        get: |r| r.gpu_mhz_req.map(|v| v as f64),
    },
    Column {
        key: "cpu_c",
        label: "CPU temp",
        unit: "\u{b0}C",
        get: cpu_c,
    },
    Column {
        key: "ddr_c",
        label: "DDR temp",
        unit: "\u{b0}C",
        get: |r| r.ddr_c,
    },
    Column {
        key: "battery_c",
        label: "Battery temp",
        unit: "\u{b0}C",
        get: |r| r.battery_c,
    },
    Column {
        key: "board_c",
        label: "Board temp (hottest)",
        unit: "\u{b0}C",
        get: |r| r.board_c,
    },
    Column {
        key: "fan_rpm",
        label: "Fan",
        unit: "rpm",
        get: |r| r.fan_rpm.map(|v| v as f64),
    },
    Column {
        key: "mem_used_gb",
        label: "RAM used",
        unit: "GB",
        get: |r| r.mem_used_mb.map(|v| v as f64 / 1024.0),
    },
];

/// What one recorded run did.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    /// The program that presented frames for the most seconds.
    pub game: Option<String>,
    pub start_unix: u64,
    pub end_unix: u64,
    /// Indices of the rows that count.
    pub steady: Vec<usize>,
    pub trimmed: bool,
    pub menu_secs: usize,
    pub stats: Vec<(&'static str, Stat)>,
    pub profiles: Vec<Share>,
    /// What held the GPU clock, over the seconds the GPU clock was read.
    pub gpu_throttle: Vec<Share>,
    /// Whether the CPU logged thermal throttle events.
    pub cpu_throttle: Vec<Share>,
    pub fan: Vec<Share>,
    pub on_ac_fraction: f64,
    pub power_target: Option<u32>,
    pub warnings: Vec<String>,
    pub frames: Option<FrameStat>,
}

impl RunSummary {
    pub fn stat(&self, key: &str) -> Option<Stat> {
        self.stats.iter().find(|(k, _)| *k == key).map(|(_, s)| *s)
    }

    pub fn length_secs(&self) -> u64 {
        self.end_unix.saturating_sub(self.start_unix)
    }

    /// Median FPS per average package watt.
    pub fn fps_per_watt(&self) -> Option<f64> {
        let fps = self.stat("fps")?.median;
        let w = self.stat("package_w")?.avg;
        (w > 0.0).then(|| fps / w)
    }

    pub fn from_rows(rows: &[Row], frames: Option<&[Frame]>) -> Self {
        if rows.is_empty() {
            return Self {
                warnings: vec!["The recording is empty.".into()],
                ..Self::default()
            };
        }

        // The game: the program presenting frames for the most seconds.
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for r in rows {
            if let (Some(fps), Some(app)) = (r.fps, r.fps_app.as_deref()) {
                if fps > 0.0 && !app.is_empty() {
                    match counts.iter_mut().find(|(a, _)| *a == app) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((app, 1)),
                    }
                }
            }
        }
        let game: Option<String> = counts
            .iter()
            .max_by_key(|(_, n)| *n)
            .map(|(a, _)| a.to_string());
        let playing = |r: &Row| match &game {
            None => true,
            Some(g) => r.fps_app.as_deref() == Some(g.as_str()) && r.fps.is_some_and(|f| f > 0.0),
        };

        let (mut first, mut last) = (0, rows.len() - 1);
        if game.is_some() {
            first = rows.iter().position(|r| playing(r)).unwrap_or(0);
            last = rows
                .iter()
                .rposition(|r| playing(r))
                .unwrap_or(rows.len() - 1);
        }

        // Trim the start, as long as at least 20 s are left.
        let mut from = first;
        let t = |i: usize| rows[i].unix_time;
        while from < last && t(from).saturating_sub(t(first)) < TRIM_SECS {
            from += 1;
        }
        let mut trimmed = false;
        if t(last).saturating_sub(t(from)) >= 20 {
            trimmed = from > first;
        } else {
            from = first;
        }

        let mut steady: Vec<usize> = (from..=last).filter(|&i| playing(&rows[i])).collect();
        let mut menu_secs = 0;
        if game.is_some() && !steady.is_empty() {
            let mut fps: Vec<f64> = steady.iter().filter_map(|&i| rows[i].fps).collect();
            fps.sort_by(f64::total_cmp);
            let median = percentile(&fps, 50.0);
            let before = steady.len();
            steady.retain(|&i| rows[i].fps.is_some_and(|f| f <= MENU_FACTOR * median));
            menu_secs = before - steady.len();
        }
        let kept: Vec<&Row> = steady.iter().map(|&i| &rows[i]).collect();

        let stats = COLUMNS
            .iter()
            .filter_map(|c| Stat::of(kept.iter().filter_map(|r| (c.get)(r))).map(|s| (c.key, s)))
            .collect();

        let n = kept.len().max(1) as f64;
        let on_ac_fraction = kept.iter().filter(|r| r.on_ac == Some(true)).count() as f64 / n;
        let profiles = shares(&kept, |r| r.profile.clone());
        let with_gpu: Vec<&Row> = kept
            .iter()
            .copied()
            .filter(|r| r.gpu_mhz.is_some_and(|v| v > 0))
            .collect();
        let gpu_throttle = shares(&with_gpu, |r| {
            Some(r.gpu_throttle.clone().unwrap_or_else(|| "none".into()))
        });
        let cpu_throttle = shares(&kept, |r| {
            Some(
                if r.throttle_events > 0 {
                    "thermal"
                } else {
                    "none"
                }
                .into(),
            )
        });
        let fan = shares(&kept, |r| {
            r.fan_mode.as_deref().map(|m| {
                match m {
                    "auto" => "EC auto",
                    "manual" => "fw-helper curve",
                    other => other,
                }
                .to_string()
            })
        });

        let mut warnings = Vec::new();
        if game.is_none() {
            warnings.push(
                "No game was presenting frames - or MangoHud was not logging - so every \
                 second is counted and there is no FPS."
                    .to_string(),
            );
        }
        if !steady.is_empty() && steady.len() < SHORT_RUN_SECS {
            warnings.push(format!(
                "Only {} s of steady play: the 5th percentile is not reliable below {SHORT_RUN_SECS} s, \
                 so compare medians only.",
                steady.len()
            ));
        }
        if !kept.is_empty() && on_ac_fraction < 1.0 {
            warnings.push(if on_ac_fraction == 0.0 {
                "The run was on battery: a heavy GPU load hits PL2 on battery and clocks lower than on mains."
                    .to_string()
            } else {
                format!(
                    "The charger was unplugged for {:.0} % of the run.",
                    (1.0 - on_ac_fraction) * 100.0
                )
            });
        }
        if profiles.len() > 1 {
            let names: Vec<&str> = profiles.iter().map(|s| s.value.as_str()).collect();
            warnings.push(format!(
                "The fw-helper profile changed during the run: {}.",
                names.join(", ")
            ));
        }

        let frames = frames
            .filter(|f| !f.is_empty())
            .and_then(|f| FrameStat::of(steady_frames(rows, &steady, f)));

        Self {
            game,
            start_unix: rows[0].unix_time,
            end_unix: rows[rows.len() - 1].unix_time,
            steady,
            trimmed,
            menu_secs,
            stats,
            profiles,
            gpu_throttle,
            cpu_throttle,
            fan,
            on_ac_fraction,
            power_target: kept.iter().find_map(|r| r.pl1_w),
            warnings,
            frames,
        }
    }
}

/// The frame times that fall in steady seconds. A row covers the second before it was
/// taken, so a frame belongs to the first row at or after it, if that is within 1.5 s.
pub fn steady_frames(rows: &[Row], steady: &[usize], frames: &[Frame]) -> Vec<f64> {
    let mut is_steady = vec![false; rows.len()];
    for &i in steady {
        is_steady[i] = true;
    }
    let mut sorted: Vec<&Frame> = frames.iter().filter(|f| f.ms <= MAX_FRAME_MS).collect();
    sorted.sort_by(|a, b| a.unix.total_cmp(&b.unix));
    let mut out = Vec::new();
    let mut i = 0;
    for f in sorted {
        while i < rows.len() && (rows[i].unix_time as f64) < f.unix {
            i += 1;
        }
        if i == rows.len() {
            break;
        }
        if is_steady[i] && rows[i].unix_time as f64 - f.unix <= 1.5 {
            out.push(f.ms);
        }
    }
    out
}

/// Linear-interpolated percentile of sorted values.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        1 => sorted[0],
        n => {
            let rank = p / 100.0 * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = (lo + 1).min(n - 1);
            sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
        }
    }
}

fn shares(rows: &[&Row], get: impl Fn(&Row) -> Option<String>) -> Vec<Share> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for r in rows {
        let v = get(r)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        match counts.iter_mut().find(|(k, _)| *k == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts
        .into_iter()
        .map(|(value, n)| Share {
            value,
            fraction: n as f64 / rows.len() as f64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: u64, fps: Option<f64>, app: Option<&str>, pkg: f64) -> Row {
        Row {
            t_s: t,
            unix_time: 1000 + t,
            fps,
            fps_app: app.map(String::from),
            package_w: Some(pkg),
            cpu_w: Some(pkg * 0.3),
            gpu_w: Some(pkg * 0.5),
            gpu_mhz: Some(1950),
            gpu_throttle: Some("pl4".into()),
            on_ac: Some(true),
            profile: Some("game".into()),
            pl1_w: Some(25),
            ..Row::default()
        }
    }

    /// 5 s of menu at 400 fps, 100 s of play at ~60, 3 s after quitting with no game.
    fn run() -> Vec<Row> {
        let mut rows = Vec::new();
        for t in 0..5 {
            rows.push(row(t, Some(400.0), Some("game"), 10.0));
        }
        for t in 5..105 {
            rows.push(row(t, Some(58.0 + (t % 5) as f64), Some("game"), 21.0));
        }
        for t in 105..108 {
            rows.push(row(t, None, None, 5.0));
        }
        rows
    }

    #[test]
    fn steady_seconds_skip_the_start_menus_and_the_tail() {
        let s = RunSummary::from_rows(&run(), None);
        assert_eq!(s.game.as_deref(), Some("game"));
        assert!(s.trimmed);
        // Play starts at t=0 (the menu presents frames too); 10 s trimmed, then the
        // menu seconds are already gone, and 105..108 never counted.
        assert!(s.steady.iter().all(|&i| (10..105).contains(&i)));
        assert_eq!(s.steady.len(), 95);
        let fps = s.stat("fps").unwrap();
        assert!(fps.max <= 62.0 && fps.min >= 58.0);
        assert!((s.stat("package_w").unwrap().avg - 21.0).abs() < 1e-9);
        assert!((s.stat("rest_w").unwrap().avg - 21.0 * 0.2).abs() < 1e-6);
        assert_eq!(s.power_target, Some(25));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert!((s.fps_per_watt().unwrap() - 60.0 / 21.0).abs() < 0.1);
    }

    #[test]
    fn uncapped_menu_seconds_inside_play_are_left_out() {
        let mut rows = run();
        for r in rows.iter_mut().skip(50).take(4) {
            r.fps = Some(300.0);
        }
        let s = RunSummary::from_rows(&rows, None);
        assert_eq!(s.menu_secs, 4);
        assert!(s.stat("fps").unwrap().max <= 62.0);
    }

    #[test]
    fn without_a_game_every_second_counts_and_says_so() {
        let rows: Vec<Row> = (0..30).map(|t| row(t, None, None, 15.0)).collect();
        let s = RunSummary::from_rows(&rows, None);
        assert!(s.game.is_none());
        assert_eq!(s.steady.len(), 30);
        assert!(s.stat("fps").is_none());
        assert!(s.warnings.iter().any(|w| w.starts_with("No game")));
        assert!(s.warnings.iter().any(|w| w.starts_with("Only 30 s")));
    }

    #[test]
    fn battery_and_profile_changes_are_warned_about() {
        let mut rows = run();
        for r in rows.iter_mut().skip(60) {
            r.on_ac = Some(false);
            r.profile = Some("quiet".into());
        }
        let s = RunSummary::from_rows(&rows, None);
        assert!(s.warnings.iter().any(|w| w.contains("unplugged")));
        assert!(s.warnings.iter().any(|w| w.contains("profile changed")));
    }

    #[test]
    fn frame_lows_come_from_steady_frames_only() {
        let rows = run();
        let mut frames = Vec::new();
        // 60 frames a second through the whole recording; one 100 ms hitch in the
        // trimmed start, and a 50 ms one in steady play.
        for t in 0..108u64 {
            for k in 0..60 {
                let unix = 1000.0 + t as f64 - 1.0 + (k as f64 + 0.5) / 60.0;
                frames.push(Frame { unix, ms: 16.6 });
            }
        }
        frames[3 * 60].ms = 100.0;
        frames[50 * 60].ms = 50.0;
        let s = RunSummary::from_rows(&rows, Some(&frames));
        let f = s.frames.unwrap();
        assert_eq!(f.count, 95 * 60);
        assert_eq!(
            f.max_ms, 50.0,
            "the hitch in the trimmed start must not count"
        );
        assert!(f.low1.is_some() && f.low01.is_some());
    }

    #[test]
    fn percentiles_interpolate() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 50.0), 3.0);
        assert!((percentile(&[0.0, 10.0], 5.0) - 0.5).abs() < 1e-9);
        let s = Stat::of([3.0, 1.0, 2.0, f64::NAN]).unwrap();
        assert_eq!((s.min, s.max, s.count), (1.0, 3.0, 3));
    }

    #[test]
    fn an_empty_recording_says_so() {
        let s = RunSummary::from_rows(&[], None);
        assert_eq!(s.warnings, vec!["The recording is empty.".to_string()]);
    }
}
