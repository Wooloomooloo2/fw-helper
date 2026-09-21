//! Recorded monitoring sessions: what the machine did while you were doing something
//! else.
//!
//! A session is a **CSV file**, deliberately. It is what
//! `scripts/sustained-perf-test.sh` already produces, so a recorded game session and a
//! `--pl1 35` benchmark run open in the same spreadsheet and the columns line up; it
//! needs no dependency in a crate that is not allowed any (ADR 0010); and it stays
//! readable when this program is not installed any more.
//!
//! Rows are read back by **column name**, not position, so a file written by an older
//! version still loads when columns are added.
//!
//! ## Time
//!
//! Filenames carry a UTC stamp and each row carries an absolute Unix time. Rendering
//! that in the user's own timezone needs the zone database, which means a dependency,
//! so the split is: this crate stores unambiguous absolute time, and whatever presents
//! a session converts it. A local-time filename produced by guessing the offset would
//! be wrong twice a year.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many recorded sessions are kept. The oldest is removed when a new one would
/// exceed this.
///
/// At 1 Hz a row is around 150 bytes, so an eight-hour session is roughly 4 MB and the
/// whole retained set is well under 150 MB. A monitor that quietly fills `/var` is a
/// bug, and the bound belongs somewhere visible rather than in a user's disk-usage
/// investigation six months from now.
pub const MAX_SESSIONS: usize = 20;

/// A single recording stops itself here. Twelve hours is longer than any session
/// anybody intends and short enough that a Record button left on overnight by accident
/// costs one file, not the disk.
pub const MAX_SESSION_SECS: u64 = 12 * 60 * 60;

/// Rows between flushes. A daemon killed with `SIGKILL` loses at most this many
/// samples; buffering nothing would mean a write syscall every second forever.
const FLUSH_EVERY: u32 = 10;

/// Column order as written. Read back by name, so this may be appended to freely;
/// the round-trip test is what keeps [`Row::to_csv`] and [`HEADER`] in step.
pub const HEADER: &str = "t_s,unix_time,cpu_pct,cpu_mhz,gpu_pct,gpu_mhz,gpu_top,\
mem_used_mb,mem_total_mb,package_w,system_w,pl1_w,peci_c,coretemp_c,battery_c,board_c,\
fan_rpm,fan_duty,fan_mode,throttle_events,throttle_ms,gpu_throttle,profile,on_ac,\
battery_pct,cpu_w,gpu_w";

/// One sample. Every field is optional because every source can be absent — a machine
/// with no battery, a GPU we cannot read, a power figure discarded as untrustworthy.
/// An empty CSV cell means "not known", and is never read back as zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    /// Seconds since the recording started.
    pub t_s: u64,
    /// Absolute time, so a session can be lined up against an external log.
    pub unix_time: u64,
    pub cpu_pct: Option<f64>,
    pub cpu_mhz: Option<u64>,
    pub gpu_pct: Option<f64>,
    pub gpu_mhz: Option<u64>,
    /// Process using the GPU most this second.
    pub gpu_top: Option<String>,
    pub mem_used_mb: Option<u64>,
    pub mem_total_mb: Option<u64>,
    /// CPU package draw. The iGPU is inside the package, so this covers cores and
    /// graphics together; [`Self::cpu_w`] and [`Self::gpu_w`] split it.
    pub package_w: Option<f64>,
    /// Whole-machine draw, measurable only on battery.
    pub system_w: Option<f64>,
    /// The sustained power limit in force. Recorded every row because it is the line a
    /// power graph is read against: draw sitting on it *is* PL1 binding, and that is
    /// the only signal available for the package.
    pub pl1_w: Option<u32>,
    pub peci_c: Option<f64>,
    pub coretemp_c: Option<f64>,
    pub battery_c: Option<f64>,
    pub board_c: Option<f64>,
    pub fan_rpm: Option<u64>,
    pub fan_duty: Option<u8>,
    /// `auto` or `manual`. A fan reading means something different in each.
    pub fan_mode: Option<String>,
    /// Throttle events **during this second**, not since boot.
    pub throttle_events: u64,
    pub throttle_ms: u64,
    /// GPU throttle reasons asserted this second, `+`-joined.
    pub gpu_throttle: Option<String>,
    pub profile: Option<String>,
    pub on_ac: Option<bool>,
    pub battery_pct: Option<u64>,
    /// RAPL `core` rail: the CPU cores alone.
    pub cpu_w: Option<f64>,
    /// RAPL `uncore` rail: the iGPU alone. A subset of [`Self::package_w`], and
    /// `cpu_w + gpu_w` is less than it — the package also carries fabric and the
    /// memory controller, which neither rail covers.
    pub gpu_w: Option<f64>,
}

/// Render a value, or an empty cell when it is not known.
fn cell<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn num(v: &Option<f64>, places: usize) -> String {
    v.map(|x| format!("{x:.places$}")).unwrap_or_default()
}

/// Strip anything that would corrupt a row. Commas and quotes turn one field into two;
/// a newline turns one row into two, which is worse because it still parses.
fn safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            ',' | '"' | '\n' | '\r' => '_',
            c => c,
        })
        .collect()
}

impl Row {
    pub fn to_csv(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},\
             {},{}",
            self.t_s,
            self.unix_time,
            num(&self.cpu_pct, 1),
            cell(&self.cpu_mhz),
            num(&self.gpu_pct, 1),
            cell(&self.gpu_mhz),
            self.gpu_top.as_deref().map(safe).unwrap_or_default(),
            cell(&self.mem_used_mb),
            cell(&self.mem_total_mb),
            num(&self.package_w, 2),
            num(&self.system_w, 2),
            cell(&self.pl1_w),
            num(&self.peci_c, 1),
            num(&self.coretemp_c, 1),
            num(&self.battery_c, 1),
            num(&self.board_c, 1),
            cell(&self.fan_rpm),
            cell(&self.fan_duty),
            self.fan_mode.as_deref().map(safe).unwrap_or_default(),
            self.throttle_events,
            self.throttle_ms,
            self.gpu_throttle.as_deref().map(safe).unwrap_or_default(),
            self.profile.as_deref().map(safe).unwrap_or_default(),
            self.on_ac
                .map(u8::from)
                .map(|v| v.to_string())
                .unwrap_or_default(),
            cell(&self.battery_pct),
            num(&self.cpu_w, 2),
            num(&self.gpu_w, 2),
        )
    }

    /// Parse one row against a header index. Unknown columns are ignored and missing
    /// ones stay `None`, so a file from another version still loads.
    fn from_csv(line: &str, index: &ColumnIndex) -> Option<Self> {
        let fields: Vec<&str> = line.split(',').collect();
        let get = |name: &str| -> Option<&str> {
            let raw = fields.get(*index.0.get(name)?)?.trim();
            (!raw.is_empty()).then_some(raw)
        };
        let f = |name: &str| get(name).and_then(|v| v.parse::<f64>().ok());
        let u = |name: &str| get(name).and_then(|v| v.parse::<u64>().ok());
        let s = |name: &str| get(name).map(ToString::to_string);

        Some(Self {
            // A row with no time is not a row; everything else may be absent.
            t_s: u("t_s")?,
            unix_time: u("unix_time").unwrap_or_default(),
            cpu_pct: f("cpu_pct"),
            cpu_mhz: u("cpu_mhz"),
            gpu_pct: f("gpu_pct"),
            gpu_mhz: u("gpu_mhz"),
            gpu_top: s("gpu_top"),
            mem_used_mb: u("mem_used_mb"),
            mem_total_mb: u("mem_total_mb"),
            package_w: f("package_w"),
            system_w: f("system_w"),
            pl1_w: u("pl1_w").map(|v| v as u32),
            peci_c: f("peci_c"),
            coretemp_c: f("coretemp_c"),
            battery_c: f("battery_c"),
            board_c: f("board_c"),
            fan_rpm: u("fan_rpm"),
            fan_duty: u("fan_duty").map(|v| v as u8),
            fan_mode: s("fan_mode"),
            throttle_events: u("throttle_events").unwrap_or_default(),
            throttle_ms: u("throttle_ms").unwrap_or_default(),
            gpu_throttle: s("gpu_throttle"),
            profile: s("profile"),
            on_ac: u("on_ac").map(|v| v == 1),
            battery_pct: u("battery_pct"),
            cpu_w: f("cpu_w"),
            gpu_w: f("gpu_w"),
        })
    }
}

/// Column name to position, built from the header line of the file being read.
struct ColumnIndex(std::collections::HashMap<String, usize>);

impl ColumnIndex {
    fn parse(header: &str) -> Self {
        Self(
            header
                .split(',')
                .enumerate()
                .map(|(i, name)| (name.trim().to_string(), i))
                .collect(),
        )
    }
}

/// A session on disk, without its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// File stem, and the handle every interface uses to name a session.
    pub name: String,
    /// What the user called it.
    pub label: String,
    pub path: PathBuf,
    pub started_unix: u64,
    pub bytes: u64,
}

/// A session with its rows loaded.
#[derive(Debug, Clone)]
pub struct Session {
    pub meta: SessionMeta,
    pub rows: Vec<Row>,
}

impl Session {
    pub fn duration_secs(&self) -> u64 {
        self.rows.last().map(|r| r.t_s).unwrap_or_default()
    }

    /// Load a session. Malformed rows are skipped rather than failing the file: a
    /// recording truncated by a power cut is still worth reading up to the cut.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines();
        let header = lines.next().unwrap_or_default();
        let index = ColumnIndex::parse(header);
        let rows: Vec<Row> = lines
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| Row::from_csv(l, &index))
            .collect();
        Ok(Self {
            meta: meta_for(path)?,
            rows,
        })
    }
}

/// Sessions in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<SessionMeta> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionMeta> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "csv"))
        .filter_map(|p| meta_for(&p).ok())
        .collect();
    out.sort_by(|a, b| {
        b.started_unix
            .cmp(&a.started_unix)
            .then(a.name.cmp(&b.name))
    });
    out
}

fn meta_for(path: &Path) -> std::io::Result<SessionMeta> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or_default();
    Ok(SessionMeta {
        label: label_of(&name),
        started_unix: started_of(&name).unwrap_or_default(),
        name,
        path: path.to_path_buf(),
        bytes,
    })
}

/// Remove the oldest sessions until at most `keep` remain. Returns what was deleted.
pub fn prune(dir: &Path, keep: usize) -> Vec<PathBuf> {
    let sessions = list(dir);
    let mut removed = Vec::new();
    for old in sessions.into_iter().skip(keep) {
        if fs::remove_file(&old.path).is_ok() {
            removed.push(old.path);
        }
    }
    removed
}

/// Delete one session by name. The name is a file stem and is validated as one — a
/// caller passing `../../etc/passwd` must not be able to reach outside `dir`.
pub fn delete(dir: &Path, name: &str) -> std::io::Result<()> {
    if name.is_empty() || !name.chars().all(is_name_char) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name:?} is not a session name"),
        ));
    }
    fs::remove_file(dir.join(format!("{name}.csv")))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Append rows to a session file.
#[derive(Debug)]
pub struct Recorder {
    path: PathBuf,
    file: BufWriter<fs::File>,
    label: String,
    started_unix: u64,
    started: Instant,
    samples: u64,
    since_flush: u32,
}

impl Recorder {
    /// Create `dir` if needed, prune to [`MAX_SESSIONS`] and start a new file.
    pub fn start(dir: &Path, label: &str) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        // Prune to one below the cap: the session about to be created takes the slot.
        prune(dir, MAX_SESSIONS.saturating_sub(1));

        let started_unix = unix_now();
        let name = format!("{}-{}", slug(label), stamp(started_unix));
        let path = dir.join(format!("{name}.csv"));
        let mut file = BufWriter::new(fs::File::create(&path)?);
        writeln!(file, "{HEADER}")?;
        file.flush()?;
        // Readable by the unprivileged GUI, which parses these directly rather than
        // pulling tens of thousands of rows across D-Bus.
        set_world_readable(&path);

        Ok(Self {
            path,
            file,
            label: label.to_string(),
            started_unix,
            started: Instant::now(),
            samples: 0,
            since_flush: 0,
        })
    }

    pub fn push(&mut self, row: &Row) -> std::io::Result<()> {
        writeln!(self.file, "{}", row.to_csv())?;
        self.samples += 1;
        self.since_flush += 1;
        if self.since_flush >= FLUSH_EVERY {
            self.file.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    pub fn finish(mut self) -> PathBuf {
        let _ = self.file.flush();
        self.path
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn name(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string()
    }

    pub fn started_unix(&self) -> u64 {
        self.started_unix
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Whether this recording has reached [`MAX_SESSION_SECS`] and should stop itself.
    pub fn is_full(&self) -> bool {
        self.elapsed_secs() >= MAX_SESSION_SECS
    }
}

#[cfg(unix)]
fn set_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
}

#[cfg(not(unix))]
fn set_world_readable(_path: &Path) {}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A filesystem-safe version of whatever the user typed.
fn slug(label: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppresses a leading dash
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("session");
    }
    out.truncate(40);
    out.trim_end_matches('-').to_string()
}

/// `YYYYmmddTHHMMSSZ` — sortable, and explicitly UTC rather than silently so.
fn stamp(unix: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// The label a session name started life as.
fn label_of(name: &str) -> String {
    // `<slug>-<stamp>`; the stamp is a fixed 16 characters ending in Z.
    match name.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 16 && tail.ends_with('Z') => head.to_string(),
        _ => name.to_string(),
    }
}

fn started_of(name: &str) -> Option<u64> {
    let (_, tail) = name.rsplit_once('-')?;
    if tail.len() != 16 || !tail.ends_with('Z') {
        return None;
    }
    let n = |a: usize, b: usize| tail.get(a..b)?.parse::<u64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(4, 6)?, n(6, 8)?);
    let (h, mi, s) = (n(9, 11)?, n(11, 13)?, n(13, 15)?);
    Some(unix_from_civil(y as i64, mo as u32, d as u32, h, mi, s))
}

/// Days from the Unix epoch to a civil date, and back.
///
/// Howard Hinnant's `days_from_civil`. Spelled out because `fw-helper-core` takes no
/// dependencies (ADR 0010) and a date library is a large thing to pull in to name a
/// file. Valid for any date this program could plausibly see.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn civil(unix: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

fn unix_from_civil(y: i64, mo: u32, d: u32, h: u64, mi: u64, s: u64) -> u64 {
    let days = days_from_civil(y, mo, d);
    (days.max(0) as u64) * 86_400 + h * 3600 + mi * 60 + s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!(
                "fw-helper-session-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_row() -> Row {
        Row {
            t_s: 42,
            unix_time: 1_757_000_000,
            cpu_pct: Some(63.4),
            cpu_mhz: Some(3200),
            gpu_pct: Some(91.2),
            gpu_mhz: Some(1950),
            gpu_top: Some("game-bin".into()),
            mem_used_mb: Some(20_864),
            mem_total_mb: Some(31_629),
            package_w: Some(34.82),
            system_w: None,
            pl1_w: Some(35),
            peci_c: Some(78.9),
            coretemp_c: Some(81.0),
            battery_c: Some(38.9),
            board_c: Some(62.1),
            fan_rpm: Some(4820),
            fan_duty: Some(180),
            fan_mode: Some("manual".into()),
            throttle_events: 2,
            throttle_ms: 140,
            gpu_throttle: Some("pl1+thermal".into()),
            profile: Some("max".into()),
            on_ac: Some(true),
            battery_pct: Some(80),
            // Deliberately not summing to package_w: the rails exclude fabric and the
            // memory controller, so a test that made them add up would enshrine an
            // arithmetic relationship the hardware does not have.
            cpu_w: Some(21.40),
            gpu_w: Some(9.65),
        }
    }

    #[test]
    fn a_row_survives_the_round_trip() {
        // This is what keeps HEADER and to_csv in step: they are written separately and
        // a column added to one but not the other shifts every field after it.
        let row = sample_row();
        let index = ColumnIndex::parse(HEADER);
        let back = Row::from_csv(&row.to_csv(), &index).expect("parse");
        assert_eq!(back, row);
    }

    #[test]
    fn the_header_and_the_row_have_the_same_number_of_columns() {
        assert_eq!(
            HEADER.split(',').count(),
            sample_row().to_csv().split(',').count()
        );
    }

    #[test]
    fn an_absent_reading_reads_back_as_absent_not_as_zero() {
        // The distinction is the whole point: 0 W is a measurement, an empty cell is
        // "we could not tell", and a graph must not draw the second as the first.
        let row = Row {
            t_s: 1,
            package_w: None,
            fan_rpm: None,
            ..Default::default()
        };
        let index = ColumnIndex::parse(HEADER);
        let back = Row::from_csv(&row.to_csv(), &index).expect("parse");
        assert_eq!(back.package_w, None);
        assert_eq!(back.fan_rpm, None);
        assert_eq!(back.throttle_events, 0);
    }

    #[test]
    fn columns_are_matched_by_name_so_a_reordered_file_still_loads() {
        // A file written by a version with different column order, or extra columns.
        let header = "unix_time,extra,package_w,t_s";
        let index = ColumnIndex::parse(header);
        let row = Row::from_csv("1757000000,ignored,25.5,7", &index).expect("parse");
        assert_eq!(row.t_s, 7);
        assert_eq!(row.unix_time, 1_757_000_000);
        assert_eq!(row.package_w, Some(25.5));
        // A column the old file did not have is absent, not garbage.
        assert_eq!(row.gpu_pct, None);
    }

    #[test]
    fn a_row_with_no_timestamp_is_not_a_row() {
        let index = ColumnIndex::parse(HEADER);
        assert!(Row::from_csv(",,,,", &index).is_none());
    }

    #[test]
    fn free_text_cannot_break_out_of_its_field() {
        // A process called `a,b` would otherwise shift every later column by one, and a
        // newline in one would split the row in two and still parse.
        let row = Row {
            t_s: 1,
            gpu_top: Some("evil,name\nnext".into()),
            ..Default::default()
        };
        let csv = row.to_csv();
        assert_eq!(csv.lines().count(), 1);
        assert_eq!(csv.split(',').count(), HEADER.split(',').count());
        let back = Row::from_csv(&csv, &ColumnIndex::parse(HEADER)).expect("parse");
        assert_eq!(back.gpu_top.as_deref(), Some("evil_name_next"));
    }

    #[test]
    fn recording_writes_a_readable_file_and_reads_back() {
        let d = Dir::new("roundtrip");
        let mut r = Recorder::start(&d.0, "Cyberpunk 2077").expect("start");
        let name = r.name();
        for t in 0..25 {
            r.push(&Row {
                t_s: t,
                package_w: Some(30.0 + t as f64),
                ..Default::default()
            })
            .expect("push");
        }
        let path = r.finish();

        let session = Session::read(&path).expect("read");
        assert_eq!(session.rows.len(), 25);
        assert_eq!(session.duration_secs(), 24);
        assert_eq!(session.rows[3].package_w, Some(33.0));
        assert_eq!(session.meta.label, "cyberpunk-2077");
        assert_eq!(session.meta.name, name);
    }

    #[test]
    fn a_truncated_recording_still_reads_up_to_the_cut() {
        // A daemon killed mid-write leaves a partial final line. Losing the whole
        // session because of it would be the wrong trade.
        let d = Dir::new("truncated");
        let path = d.0.join("crash-20260904T193200Z.csv");
        fs::write(
            &path,
            format!("{HEADER}\n0,1757000000,10.0\n1,1757000001,20.0\n2,17570000"),
        )
        .unwrap();
        let session = Session::read(&path).expect("read");
        assert_eq!(session.rows.len(), 3);
        assert_eq!(session.rows[0].cpu_pct, Some(10.0));
        // The truncated row kept its timestamp and lost the rest, which is honest.
        assert_eq!(session.rows[2].cpu_pct, None);
    }

    #[test]
    fn pruning_removes_the_oldest_first() {
        let d = Dir::new("prune");
        for (i, stamp) in ["20260901T100000Z", "20260902T100000Z", "20260903T100000Z"]
            .iter()
            .enumerate()
        {
            fs::write(
                d.0.join(format!("run{i}-{stamp}.csv")),
                format!("{HEADER}\n"),
            )
            .unwrap();
        }
        let removed = prune(&d.0, 2);
        assert_eq!(removed.len(), 1);
        let left = list(&d.0);
        assert_eq!(left.len(), 2);
        // Newest first, and the September 1st one is the one that went.
        assert_eq!(left[0].label, "run2");
        assert_eq!(left[1].label, "run1");
    }

    #[test]
    fn starting_a_session_keeps_the_total_at_the_cap() {
        let d = Dir::new("cap");
        for i in 0..MAX_SESSIONS + 3 {
            fs::write(
                d.0.join(format!("old{i:02}-202608{:02}T100000Z.csv", i + 1)),
                format!("{HEADER}\n"),
            )
            .unwrap();
        }
        let r = Recorder::start(&d.0, "new").expect("start");
        r.finish();
        assert_eq!(list(&d.0).len(), MAX_SESSIONS);
    }

    #[test]
    fn a_session_name_cannot_escape_its_directory() {
        let d = Dir::new("escape");
        for bad in ["../../etc/passwd", "..", "a/b", ""] {
            assert!(delete(&d.0, bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn labels_become_safe_file_names() {
        assert_eq!(slug("Cyberpunk 2077"), "cyberpunk-2077");
        assert_eq!(slug("  ../etc/passwd  "), "etc-passwd");
        assert_eq!(slug("!!!"), "session");
        assert_eq!(slug(""), "session");
        // And a name built from a slug is one `delete` will accept.
        assert!(slug("Cyberpunk 2077").chars().all(is_name_char));
    }

    #[test]
    fn the_stamp_round_trips_through_the_file_name() {
        // 2026-09-04T19:32:00Z
        let unix = unix_from_civil(2026, 9, 4, 19, 32, 0);
        let name = format!("{}-{}", slug("game"), stamp(unix));
        assert_eq!(name, "game-20260904T193200Z");
        assert_eq!(started_of(&name), Some(unix));
        assert_eq!(label_of(&name), "game");
    }

    #[test]
    fn the_calendar_handles_leap_days_and_centuries() {
        // Dates a naive /365 conversion gets wrong.
        for (y, m, d) in [
            (2024, 2, 29), // leap year
            (2000, 2, 29), // leap century
            (2100, 3, 1),  // NOT a leap year
            (1970, 1, 1),  // the epoch itself
            (2038, 1, 19), // past a 32-bit second count
        ] {
            let unix = unix_from_civil(y, m, d, 12, 0, 0);
            assert_eq!(civil(unix), (y, m, d, 12, 0, 0), "{y}-{m}-{d}");
        }
        assert_eq!(unix_from_civil(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    fn a_label_with_a_dash_is_not_mistaken_for_a_stamp() {
        let name = "my-long-label-20260904T193200Z";
        assert_eq!(label_of(name), "my-long-label");
        // A name with no stamp at all is its own label rather than losing a word.
        assert_eq!(label_of("bare"), "bare");
        assert_eq!(label_of("two-words"), "two-words");
    }
}
