//! Frame times, from MangoHud's log.
//!
//! The Windows sibling counts frames itself, from ETW present events. Linux has no
//! equivalent a separate process can watch: a frame is presented inside the game's own
//! process, through the Vulkan loader, and the only thing positioned to see it is a
//! layer loaded into that process. MangoHud is that layer, it is already installed for
//! the HUD, and with logging on it writes **one row per frame** (`log_interval=0`)
//! which a reader can follow while the game is still running - measured 2026-11-16
//! with MangoHud 0.8.4: the file grew 35 KB -> 63 KB between two reads four seconds
//! apart under `vkcube`.
//!
//! So fw-helper reads MangoHud's log rather than counting anything (ADR 0016).
//!
//! ## The log
//!
//! ```text
//! os,cpu,gpu,ram,kernel,driver,cpuscheduler
//! Ubuntu 26.10,Intel Core Ultra X7 358H,...
//! fps,frametime,cpu_load,...,cpu_mhz,elapsed
//! 117.865,8.48431,2.73292,...,2712,16874705
//! ```
//!
//! `frametime` is milliseconds; `elapsed` is **nanoseconds since logging started**, not
//! a wall-clock time. The file name carries the start to the second in *local* time
//! (`vkcube_2026-11-16_23-32-57.csv`). Rather than trust that - a whole second of
//! uncertainty, and a time zone - the caller passes the file's modification time, which
//! is when its newest row was written, and every row is placed relative to that one.

/// One presented frame: when it finished, and how long it took.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// Unix seconds, fractional.
    pub unix: f64,
    pub ms: f64,
}

/// Longer than this and the game was not presenting (a loading screen, alt-tab), so it
/// is not a frame time anyone would call a stutter. Same cut as the Windows build.
pub const MAX_FRAME_MS: f64 = 1000.0;

/// The header of fw-helper's own frames file, written beside an exported session.
pub const SIDECAR_HEADER: &str = "unix_s,frame_ms";

/// The program a MangoHud log belongs to and the local time it started, from its name.
///
/// `HorizonZeroDawnRemastered_2026-09-26_20-14-03.csv` -> `HorizonZeroDawnRemastered`.
/// The program name may itself contain underscores, so the date is taken from the end.
pub fn log_name(file_name: &str) -> Option<(String, [u32; 6])> {
    let stem = file_name.strip_suffix(".csv")?;
    let mut parts = stem.rsplitn(3, '_');
    let time = parts.next()?;
    let date = parts.next()?;
    let app = parts.next()?;
    let nums = |s: &str| -> Option<Vec<u32>> { s.split('-').map(|p| p.parse().ok()).collect() };
    let (d, t) = (nums(date)?, nums(time)?);
    if d.len() != 3 || t.len() != 3 || app.is_empty() {
        return None;
    }
    Some((app.to_string(), [d[0], d[1], d[2], t[0], t[1], t[2]]))
}

/// Parse a MangoHud log. `end_unix` is the file's modification time, which anchors the
/// newest complete row; every other row is placed by its `elapsed` relative to that.
///
/// Rows that do not parse are skipped - the last one of a file still being written is
/// usually torn. Returns no frames for a file that is not a MangoHud log at all.
pub fn parse_mangohud(text: &str, end_unix: f64) -> Vec<Frame> {
    let mut lines = text.lines();
    let header = loop {
        match lines.next() {
            Some(l) if l.starts_with("fps,") => break l,
            Some(_) => continue,
            None => return Vec::new(),
        }
    };
    let cols: Vec<&str> = header.split(',').collect();
    let (Some(ft), Some(el)) = (
        cols.iter().position(|c| *c == "frametime"),
        cols.iter().position(|c| *c == "elapsed"),
    ) else {
        return Vec::new();
    };

    let mut raw: Vec<(f64, f64)> = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        // A torn row has fewer fields, or a truncated last one that still parses - so
        // demand the full width, and a monotonic clock.
        if f.len() != cols.len() {
            continue;
        }
        let (Ok(ms), Ok(ns)) = (f[ft].trim().parse::<f64>(), f[el].trim().parse::<f64>()) else {
            continue;
        };
        if let Some(&(_, prev)) = raw.last() {
            if ns < prev {
                continue;
            }
        }
        raw.push((ms, ns));
    }
    let Some(&(_, last_ns)) = raw.last() else {
        return Vec::new();
    };
    raw.into_iter()
        .filter(|(ms, _)| *ms > 0.0 && ms.is_finite())
        .map(|(ms, ns)| Frame {
            unix: end_unix - (last_ns - ns) / 1e9,
            ms,
        })
        .collect()
}

/// The frame rate for each recorded second.
///
/// A row stamped `t` covers the second before it, `(t - 1, t]`, which is how the
/// daemon samples. The rate is frames over the time they took rather than a bare count,
/// so a second the game only half-filled - it started, or stopped - is not read as a
/// collapse. A second with no frames at all is `None`, not zero: the game was not
/// presenting, which is different from presenting slowly.
///
/// `frames` must be sorted by time.
pub fn per_second(frames: &[Frame], times: &[u64]) -> Vec<Option<f64>> {
    times
        .iter()
        .map(|&t| {
            let (lo, hi) = (t as f64 - 1.0, t as f64);
            let start = frames.partition_point(|f| f.unix <= lo);
            let end = frames.partition_point(|f| f.unix <= hi);
            let window = &frames[start..end];
            let busy: f64 = window
                .iter()
                .filter(|f| f.ms <= MAX_FRAME_MS)
                .map(|f| f.ms)
                .sum();
            (busy > 0.0).then(|| window.len() as f64 * 1000.0 / busy)
        })
        .collect()
}

/// fw-helper's frames file: the frame times of one recording, for its 1% lows.
/// Whether these rows are individual frames, or one averaged row per interval.
///
/// MangoHud with `log_interval=0` writes a row per frame, each `elapsed` one frame
/// time after the last. With `log_interval=1000` (fw-helper's once-a-second config) it
/// writes one row a second whose `frametime` is that second's **average** - measured
/// 2026-11-17, 0.8.4: rows 1000 ms apart, `frametime` 8.33 at `fps` 120.1. Averages
/// still give the right frame rate per second, but percentiles of them are not frame
/// times, so such a log yields no 1 % lows. Told apart by the spacing: a per-frame
/// log's rows are about one frame time apart, an interval log's many.
pub fn per_frame(frames: &[Frame]) -> bool {
    if frames.len() < 3 {
        return true;
    }
    let mut gaps: Vec<f64> = frames
        .windows(2)
        .map(|w| (w[1].unix - w[0].unix) * 1000.0)
        .collect();
    let mut ms: Vec<f64> = frames.iter().map(|f| f.ms).collect();
    gaps.sort_by(f64::total_cmp);
    ms.sort_by(f64::total_cmp);
    gaps[gaps.len() / 2] <= 3.0 * ms[ms.len() / 2]
}

pub fn to_sidecar(frames: &[Frame]) -> String {
    let mut out = String::with_capacity(frames.len() * 24 + 20);
    out.push_str(SIDECAR_HEADER);
    out.push('\n');
    for f in frames {
        out.push_str(&format!("{:.4},{:.3}\n", f.unix, f.ms));
    }
    out
}

pub fn from_sidecar(text: &str) -> Vec<Frame> {
    let mut lines = text.lines();
    if lines.next() != Some(SIDECAR_HEADER) {
        return Vec::new();
    }
    lines
        .filter_map(|l| {
            let (a, b) = l.split_once(',')?;
            Some(Frame {
                unix: a.parse().ok()?,
                ms: b.parse().ok()?,
            })
        })
        .filter(|f| f.ms > 0.0 && f.ms.is_finite())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_once_a_second_log_is_told_from_a_per_frame_one() {
        // As MangoHud wrote it with log_interval=1000, 2026-11-17.
        let interval = "fps,frametime,elapsed
120.098,8.32655,1000658915
117.761,8.4918,2000946783
119.204,8.38898,3001097007
120.352,8.30897,4001269139
";
        let f = parse_mangohud(interval, 1000.0);
        assert_eq!(f.len(), 4);
        assert!(!per_frame(&f));
        // Its per-second rate is still right: one averaged row a second.
        let fps = per_second(&f, &[1000]);
        assert!((fps[0].unwrap() - 120.35).abs() < 0.1, "{fps:?}");
        assert!(per_frame(&parse_mangohud(LOG, 1000.0)));
    }

    /// Shaped exactly like the log MangoHud 0.8.4 wrote on this machine, 2026-11-16.
    const LOG: &str = "os,cpu,gpu,ram,kernel,driver,cpuscheduler
Ubuntu 26.10,Intel Core Ultra X7 358H,Intel(R) Arc(tm) B390 (PTL),31861784,7.3.0-10-generic,,powersave
fps,frametime,cpu_load,cpu_power,gpu_load,cpu_temp,gpu_temp,gpu_core_clock,gpu_mem_clock,gpu_vram_used,gpu_power,ram_used,swap_used,process_rss,cpu_mhz,elapsed
117.865,8.48431,2.73292,0,0,54,0,900,0,0,0,10.8857,1.86997,0,2712,16874705
121.426,8.23545,2.73292,0,0,54,0,900,0,0,0,10.8857,1.86997,0,2712,25111188
119.805,8.34693,2.73292,0,0,54,0,900,0,0,0,10.8857,1.86997,0,2712,33458067
120.322,8.31104,2.73292,0,0,54,0,900,0,0,0,10.8857,1.86997,0,2712,4176
";

    #[test]
    fn the_name_gives_the_program_and_start() {
        assert_eq!(
            log_name("vkcube_2026-11-16_23-32-57.csv"),
            Some(("vkcube".into(), [2026, 11, 16, 23, 32, 57]))
        );
        assert_eq!(
            log_name("Horizon_Zero_Dawn_2026-09-26_20-14-03.csv")
                .unwrap()
                .0,
            "Horizon_Zero_Dawn"
        );
        assert_eq!(log_name("summary.csv"), None);
        assert_eq!(log_name("notes_x_y.csv"), None);
    }

    #[test]
    fn rows_are_placed_relative_to_the_newest() {
        let f = parse_mangohud(LOG, 1000.0);
        // The torn last row (its `elapsed` cut to 4176) goes backwards and is dropped.
        assert_eq!(f.len(), 3);
        assert!((f[2].unix - 1000.0).abs() < 1e-9);
        assert!((f[0].unix - (1000.0 - (33458067.0 - 16874705.0) / 1e9)).abs() < 1e-9);
        assert!((f[1].ms - 8.23545).abs() < 1e-9);
    }

    #[test]
    fn something_that_is_not_a_log_gives_nothing() {
        assert!(parse_mangohud("a,b,c\n1,2,3\n", 0.0).is_empty());
        assert!(parse_mangohud("", 0.0).is_empty());
    }

    #[test]
    fn a_second_reads_frames_over_the_time_they_took() {
        // 60 frames of 10 ms in (99, 100]: busy for 0.6 s, so 100 fps - not 60.
        let frames: Vec<Frame> = (0..60)
            .map(|i| Frame {
                unix: 99.3 + i as f64 * 0.01,
                ms: 10.0,
            })
            .collect();
        let fps = per_second(&frames, &[99, 100, 101]);
        assert_eq!(fps[0], None);
        assert!((fps[1].unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(fps[2], None, "a second with no frames is not zero fps");
    }

    #[test]
    fn a_frame_on_the_boundary_belongs_to_one_second_only() {
        let frames = [Frame {
            unix: 100.0,
            ms: 16.0,
        }];
        let fps = per_second(&frames, &[100, 101]);
        assert!(fps[0].is_some());
        assert!(fps[1].is_none());
    }

    #[test]
    fn the_sidecar_round_trips() {
        let frames = vec![
            Frame {
                unix: 1_763_330_000.125,
                ms: 16.667,
            },
            Frame {
                unix: 1_763_330_000.142,
                ms: 17.0,
            },
        ];
        assert_eq!(from_sidecar(&to_sidecar(&frames)), frames);
        assert!(from_sidecar("time_s,frame_ms\n1,2\n").is_empty());
    }
}
