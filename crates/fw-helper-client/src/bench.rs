//! The benchmark folder: recordings the user keeps, with their frame times, notes and
//! reports.
//!
//! The daemon records into `/var/lib/fw-helper/sessions`, which it owns, keeps to the
//! last twenty, and which an unprivileged process cannot write. Everything a person
//! adds to a recording - notes, a summary, a comparison - is theirs, so it lives in
//! their home: **`~/Documents/FW Benchmark`**, the folder the Windows build uses at the
//! same user's request (ADR 0016).
//!
//! A recording gets there by being **exported**: copied out of the daemon's directory,
//! with the frame rate joined in from MangoHud's log (which is also in the user's home,
//! and which the daemon therefore never sees). Exporting is idempotent - it rewrites
//! the copy and the frames file, and keeps any notes already typed - so the GUI and the
//! CLI may both do it for the same recording without coordinating.

use fw_helper_core::frames::{self, Frame};
use fw_helper_core::notes::{self, RunNotes};
use fw_helper_core::session::{self, Row, Session, SessionMeta, HEADER};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FRAMES_SUFFIX: &str = ".frames.csv";
pub const REPORT_SUFFIX: &str = ".html";
pub const PROJECT_SUFFIX: &str = ".json";

/// MangoHud logs older than this are deleted. Logging every frame of every game runs
/// to ~20 MB an hour at 60 fps; a recording that wanted them has exported them by now.
pub const MANGOHUD_KEEP_DAYS: u64 = 14;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/Documents/FW Benchmark`, honouring the XDG documents directory if it was moved.
/// `FW_HELPER_BENCH_DIR` overrides it, for tests and for keeping runs elsewhere.
pub fn bench_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FW_HELPER_BENCH_DIR") {
        return PathBuf::from(dir);
    }
    documents_dir().join("FW Benchmark")
}

fn documents_dir() -> PathBuf {
    let home = home();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    if let Ok(text) = fs::read_to_string(config.join("user-dirs.dirs")) {
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("XDG_DOCUMENTS_DIR=") {
                let v = v.trim_matches('"');
                let path = match v.strip_prefix("$HOME") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => PathBuf::from(v),
                };
                // `$HOME/` alone is how xdg-user-dirs says "disabled".
                if path != home {
                    return path;
                }
            }
        }
    }
    home.join("Documents")
}

pub fn projects_dir() -> PathBuf {
    bench_dir().join("projects")
}

/// Where the shipped MangoHud config tells MangoHud to log: its `output_folder`.
///
/// MangoHud expands `~` there but will not create the folder, and with no folder it
/// logs nothing and says only "Failed to write log file" on the game's stderr
/// (measured 2026-11-16, 0.8.4). So every client creates it - see
/// [`ensure_mangohud_dir`].
pub fn mangohud_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FW_HELPER_MANGOHUD_DIR") {
        return PathBuf::from(dir);
    }
    home().join(".local/share/fw-helper/mangohud")
}

pub fn ensure_mangohud_dir() {
    let _ = fs::create_dir_all(mangohud_dir());
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

fn mtime(path: &Path) -> Option<f64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

/// One MangoHud log file.
#[derive(Debug, Clone)]
pub struct MangoLog {
    pub path: PathBuf,
    pub app: String,
    /// When its newest row was written, which anchors every row's time.
    pub modified: f64,
}

pub fn mangohud_logs() -> Vec<MangoLog> {
    let Ok(entries) = fs::read_dir(mangohud_dir()) else {
        return Vec::new();
    };
    let mut logs: Vec<MangoLog> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            let (app, _) = frames::log_name(&name)?;
            Some(MangoLog {
                modified: mtime(&path)?,
                path,
                app,
            })
        })
        .collect();
    logs.sort_by(|a, b| b.modified.total_cmp(&a.modified));
    logs
}

/// Delete MangoHud logs older than [`MANGOHUD_KEEP_DAYS`]. Returns how many went.
pub fn prune_mangohud() -> usize {
    let cutoff = unix_now() - (MANGOHUD_KEEP_DAYS * 86_400) as f64;
    mangohud_logs()
        .into_iter()
        .filter(|l| l.modified < cutoff)
        .filter(|l| fs::remove_file(&l.path).is_ok())
        .count()
}

/// Every program's frames between two times, from whichever logs cover them.
pub fn frames_between(from_unix: f64, to_unix: f64) -> Vec<(String, Vec<Frame>)> {
    let mut out: Vec<(String, Vec<Frame>)> = Vec::new();
    // A log last written before the window opened cannot hold a frame inside it.
    for log in mangohud_logs()
        .into_iter()
        .filter(|l| l.modified >= from_unix)
    {
        let Ok(text) = fs::read_to_string(&log.path) else {
            continue;
        };
        let mut f: Vec<Frame> = frames::parse_mangohud(&text, log.modified)
            .into_iter()
            .filter(|f| f.unix > from_unix && f.unix <= to_unix)
            .collect();
        if f.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(a, _)| *a == log.app) {
            Some((_, all)) => all.append(&mut f),
            None => out.push((log.app.clone(), f)),
        }
    }
    for (_, f) in &mut out {
        f.sort_by(|a, b| a.unix.total_cmp(&b.unix));
    }
    // Most frames first: that is the game, anything else is a launcher or a tool.
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    out
}

/// The frame rate right now, and which program it is, if a game is running under
/// MangoHud with fw-helper's config. Reads only the tail of the newest log, so it is
/// cheap enough to call every second.
pub fn live_fps() -> Option<(f64, String)> {
    let now = unix_now();
    let log = mangohud_logs().into_iter().next()?;
    // Not written for a few seconds: the game is gone, or paused in the background.
    if now - log.modified > 3.0 {
        return None;
    }
    let mut file = fs::File::open(&log.path).ok()?;
    let mut head = String::new();
    // The header is the third line; 2 KB holds the first three comfortably.
    file.by_ref().take(2048).read_to_string(&mut head).ok()?;
    let header: Vec<&str> = head.lines().take(3).collect();
    let len = file.metadata().ok()?.len();
    let tail_from = len.saturating_sub(96 * 1024);
    file.seek(SeekFrom::Start(tail_from)).ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    // Drop the partial line the seek landed in.
    let tail = if tail_from > 0 {
        tail.split_once('\n').map(|(_, t)| t).unwrap_or("")
    } else {
        tail.split_once(header.last()?)
            .map(|(_, t)| t)
            .unwrap_or("")
    };
    let text = format!("{}\n{tail}", header.join("\n"));
    let f = frames::parse_mangohud(&text, log.modified);
    let last = f.last()?.unix;
    let fps = frames::per_second(&f, &[last.ceil() as u64]);
    fps[0].map(|v| (v, log.app))
}

/// What an export produced.
#[derive(Debug, Clone)]
pub struct Exported {
    pub csv: PathBuf,
    pub frames: Option<PathBuf>,
    pub notes: PathBuf,
    /// The program whose frames were joined in, if any.
    pub game: Option<String>,
}

/// Copy a daemon recording into the benchmark folder with its frame rate, frame
/// times and the notes fw-helper can fill in itself.
pub fn export(session_path: &Path) -> std::io::Result<Exported> {
    let session = Session::read(session_path)?;
    let dir = bench_dir();
    fs::create_dir_all(&dir)?;
    let name = session.meta.name.clone();
    let mut rows = session.rows;

    let mut game = None;
    let mut frames_path = None;
    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        let all = frames_between(first.unix_time as f64 - 1.0, last.unix_time as f64);
        let times: Vec<u64> = rows.iter().map(|r| r.unix_time).collect();
        let per_app: Vec<(&str, Vec<Option<f64>>)> = all
            .iter()
            .map(|(app, f)| (app.as_str(), frames::per_second(f, &times)))
            .collect();
        for (i, row) in rows.iter_mut().enumerate() {
            // The most-framed program that presented this second; [`frames_between`]
            // already put the game first.
            if let Some((app, fps)) = per_app
                .iter()
                .find_map(|(app, v)| v[i].map(|fps| (app, fps)))
            {
                row.fps = Some(fps);
                row.fps_app = Some(app.to_string());
            }
        }
        if let Some((app, f)) = all.first() {
            game = Some(app.clone());
            let p = dir.join(format!("{name}{FRAMES_SUFFIX}"));
            write_atomic(&p, &frames::to_sidecar(f))?;
            frames_path = Some(p);
        }
    }

    let csv = dir.join(format!("{name}.csv"));
    let mut text = String::with_capacity(rows.len() * 180 + HEADER.len());
    text.push_str(HEADER);
    text.push('\n');
    for r in &rows {
        text.push_str(&r.to_csv());
        text.push('\n');
    }
    write_atomic(&csv, &text)?;

    // Notes: what fw-helper knows, kept under whatever the user has already typed.
    let notes_path = notes_path(&csv);
    let mut n = load_notes(&csv).unwrap_or_default();
    let auto = auto_notes(&rows);
    n.bios = n.bios.or(auto.bios);
    n.ec = n.ec.or(auto.ec);
    n.kernel = n.kernel.or(auto.kernel);
    n.app = n.app.or(auto.app);
    n.fan_curve = n.fan_curve.or(auto.fan_curve);
    write_atomic(&notes_path, &n.to_json())?;

    Ok(Exported {
        csv,
        frames: frames_path,
        notes: notes_path,
        game,
    })
}

/// Whatever fw-helper can say about a run without asking: firmware, kernel, its own
/// version, and who drove the fan.
fn auto_notes(rows: &[Row]) -> RunNotes {
    let read = |p: &str| fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let ec = read("/sys/class/chromeos/cros_ec/version").and_then(|t| {
        t.lines()
            .find_map(|l| l.strip_prefix("RW version:"))
            .map(|v| v.trim().to_string())
    });
    let manual = rows
        .iter()
        .filter(|r| r.fan_mode.as_deref() == Some("manual"))
        .count();
    let fan = if rows.is_empty() {
        None
    } else if manual * 2 > rows.len() {
        let profile = rows.iter().rev().find_map(|r| r.profile.clone());
        Some(match profile {
            Some(p) => format!("fw-helper curve ({p})"),
            None => "fw-helper curve".into(),
        })
    } else {
        Some("EC auto".into())
    };
    RunNotes {
        bios: read("/sys/class/dmi/id/bios_version"),
        ec,
        kernel: read("/proc/sys/kernel/osrelease"),
        app: Some(env!("CARGO_PKG_VERSION").to_string()),
        fan_curve: fan,
        ..RunNotes::default()
    }
}

/// Write via a temporary file and a rename, so a reader never sees half a file and two
/// exporters racing each other each leave a whole one.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// `foo.csv` -> `foo.notes.json`.
pub fn notes_path(csv: &Path) -> PathBuf {
    sibling(csv, notes::SUFFIX)
}

pub fn frames_path(csv: &Path) -> PathBuf {
    sibling(csv, FRAMES_SUFFIX)
}

pub fn report_path(csv: &Path) -> PathBuf {
    sibling(csv, REPORT_SUFFIX)
}

fn sibling(csv: &Path, suffix: &str) -> PathBuf {
    let stem = csv.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    csv.with_file_name(format!("{stem}{suffix}"))
}

pub fn load_notes(csv: &Path) -> Option<RunNotes> {
    RunNotes::from_json(&fs::read_to_string(notes_path(csv)).ok()?)
}

pub fn save_notes(csv: &Path, n: &RunNotes) -> std::io::Result<()> {
    write_atomic(&notes_path(csv), &n.to_json())
}

pub fn load_frames(csv: &Path) -> Vec<Frame> {
    fs::read_to_string(frames_path(csv))
        .map(|t| frames::from_sidecar(&t))
        .unwrap_or_default()
}

/// Recordings in the benchmark folder, newest first.
pub fn saved_sessions() -> Vec<SessionMeta> {
    session::list(&bench_dir())
        .into_iter()
        // `x.frames.csv` lists as a session named `x.frames`; a real name has no dot.
        .filter(|m| !m.name.contains('.'))
        .collect()
}

/// Delete a saved recording and everything beside it.
pub fn delete_saved(csv: &Path) -> std::io::Result<()> {
    for p in [frames_path(csv), notes_path(csv), report_path(csv)] {
        let _ = fs::remove_file(p);
    }
    fs::remove_file(csv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings_share_the_stem() {
        let csv = Path::new("/x/FW Benchmark/cp-20261116T220000Z.csv");
        assert_eq!(
            notes_path(csv),
            Path::new("/x/FW Benchmark/cp-20261116T220000Z.notes.json")
        );
        assert_eq!(
            frames_path(csv),
            Path::new("/x/FW Benchmark/cp-20261116T220000Z.frames.csv")
        );
        assert_eq!(
            report_path(csv),
            Path::new("/x/FW Benchmark/cp-20261116T220000Z.html")
        );
    }
}
