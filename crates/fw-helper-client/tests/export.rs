//! Exporting a recording joins MangoHud's frames into it. One test, because it points
//! the benchmark and MangoHud folders at temporary ones through the environment, which
//! is process-wide.

use fw_helper_client::bench;
use fw_helper_core::session::{Recorder, Row, Session};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn export_joins_frames_keeps_notes_and_is_repeatable() {
    let root = std::env::temp_dir().join(format!("fw-helper-export-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (daemon, saved, mango) = (root.join("daemon"), root.join("saved"), root.join("mango"));
    fs::create_dir_all(&mango).unwrap();
    std::env::set_var("FW_HELPER_BENCH_DIR", &saved);
    std::env::set_var("FW_HELPER_MANGOHUD_DIR", &mango);
    std::env::set_var("FW_HELPER_SESSION_DIR", &daemon);

    // A ten-second recording, as the daemon writes it: no fps.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let start = now - 20;
    let mut rec = Recorder::start(&daemon, "cp2077 test").unwrap();
    for i in 0..10 {
        rec.push(&Row {
            t_s: i,
            unix_time: start + i,
            package_w: Some(20.0),
            fan_mode: Some("manual".into()),
            profile: Some("game".into()),
            ..Row::default()
        })
        .unwrap();
    }
    let session_path = rec.finish();

    // A MangoHud log: 60 fps of 16 ms frames from start+2 to start+8, last row at
    // start+8 exactly, which is what the file's mtime will say.
    let mut log = String::from(
        "os,cpu,gpu,ram,kernel,driver,cpuscheduler\nUbuntu,x,y,1,k,,p\nfps,frametime,elapsed\n",
    );
    let frames = 6 * 60;
    for k in 0..frames {
        let ns = (k as u64) * 1_000_000_000 / 60;
        log.push_str(&format!("60,16,{ns}\n"));
    }
    let last_ns = (frames as u64 - 1) * 1_000_000_000 / 60;
    let log_path = mango.join("Cyberpunk2077_2026-11-16_22-00-00.csv");
    fs::write(&log_path, &log).unwrap();
    let end = start + 8;
    fs::File::options()
        .write(true)
        .open(&log_path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(end))
        .unwrap();
    // And an unrelated old log that must be ignored.
    let old = mango.join("vkcube_2026-11-01_10-00-00.csv");
    fs::write(&old, &log).unwrap();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(start - 3600))
        .unwrap();

    let out = bench::export(&session_path).unwrap();
    assert_eq!(out.game.as_deref(), Some("Cyberpunk2077"));
    let s = Session::read(&out.csv).unwrap();
    assert_eq!(s.rows.len(), 10);
    let first_frame = end as f64 - last_ns as f64 / 1e9;
    for r in &s.rows {
        let covered = (r.unix_time as f64) > first_frame && r.unix_time <= end;
        assert_eq!(
            r.fps.is_some(),
            covered,
            "row at {} (frames {first_frame}..{end})",
            r.unix_time
        );
        if let Some(f) = r.fps {
            assert!(
                (f - 62.5).abs() < 0.01,
                "16 ms frames are 62.5 fps, got {f}"
            );
            assert_eq!(r.fps_app.as_deref(), Some("Cyberpunk2077"));
        }
    }
    assert_eq!(bench::load_frames(&out.csv).len(), frames);

    // Notes: filled in automatically, and what the user types survives a re-export.
    let mut n = bench::load_notes(&out.csv).unwrap();
    assert_eq!(n.app.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(n.fan_curve.as_deref(), Some("fw-helper curve (game)"));
    n.game_settings = Some("1080p Medium".into());
    bench::save_notes(&out.csv, &n).unwrap();
    bench::export(&session_path).unwrap();
    assert_eq!(
        bench::load_notes(&out.csv)
            .unwrap()
            .game_settings
            .as_deref(),
        Some("1080p Medium")
    );

    // The export pruned the old log, which no recording covers. The game's log is
    // still kept: its last frame is under ten minutes old.
    assert!(
        !old.exists(),
        "a log no recording needs survived the export"
    );
    assert!(log_path.exists());

    // Once the raw log is gone, saving the run again keeps the frame rate it had.
    fs::remove_file(&log_path).unwrap();
    let again = bench::export(&session_path).unwrap();
    assert_eq!(again.game.as_deref(), Some("Cyberpunk2077"));
    assert!(again.frames.is_some());
    let s = Session::read(&out.csv).unwrap();
    assert!(s.rows.iter().filter(|r| r.fps.is_some()).count() >= 6);
    assert_eq!(bench::load_frames(&out.csv).len(), frames);

    // The folder lists the recording once - not its frames file as a second session.
    let listed = bench::saved_sessions();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].label, "cp2077-test");

    bench::delete_saved(&out.csv).unwrap();
    assert!(fs::read_dir(&saved).unwrap().next().is_none());
    let _ = fs::remove_dir_all(&root);
}
