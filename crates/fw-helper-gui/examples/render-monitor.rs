//! Render the Monitor page showing a saved run to a PNG, to look at it without
//! disturbing a running GUI.
//!
//!   FW_HELPER_BENCH_DIR=<dir with a saved run> \
//!     cargo run -p fw-helper-gui --example render-monitor -- RUN.csv out.png [dark] [width]
//!
//! Opens a window for about two seconds under its own application id - the real GUI is
//! single-instance - then paints the page's whole content and quits.

use adw::prelude::*;

#[path = "../src/chart.rs"]
#[allow(dead_code)]
mod chart;
#[path = "../src/compare.rs"]
#[allow(dead_code)]
mod compare;
#[path = "../src/monitor.rs"]
#[allow(dead_code)]
mod monitor;
#[path = "../src/shortcut.rs"]
#[allow(dead_code)]
mod shortcut;
#[path = "../src/worker.rs"]
#[allow(dead_code)]
mod worker;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let csv = std::path::PathBuf::from(args.get(1).expect("a saved run's CSV"));
    let out = args.get(2).cloned().unwrap_or_else(|| "monitor.png".into());
    let dark = args.get(3).map(String::as_str) == Some("dark");
    let width: i32 = args.get(4).and_then(|w| w.parse().ok()).unwrap_or(1100);
    // "notes" or "compare" opens that dialog and renders the whole window instead.
    let dialog = args.get(5).cloned();

    let app = adw::Application::builder()
        .application_id("org.fwhelper.RenderMonitor")
        .build();
    app.connect_activate(move |app| {
        // Dialogs fade in, and an offscreen render never advances the animation.
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_enable_animations(false);
        }
        adw::StyleManager::default().set_color_scheme(if dark {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
        let (tx, _rx) = std::sync::mpsc::channel();
        let page = monitor::MonitorPage::new(tx);
        // "live" as the CSV argument: feed the page real snapshots from the daemon for
        // a few seconds instead - the path a running window takes every second.
        let live = csv.as_os_str() == "live";
        if live {
            let proxy = fw_helper_client::connect().expect("daemon").0;
            let page = page.clone();
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
                match fw_helper_client::Snapshot::fetch(&proxy) {
                    Ok(s) => page.update(&s),
                    Err(e) => eprintln!("snapshot: {e}"),
                }
                gtk::glib::ControlFlow::Continue
            });
        } else {
            page.open_saved(csv.clone());
        }
        let win = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(width)
            .default_height(900)
            .content(&page.widget)
            .build();
        win.present();
        {
            let (page, csv, dialog) = (page.clone(), csv.clone(), dialog.clone());
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                match dialog.as_deref() {
                    Some("notes") => page.ask_for_notes(csv, true),
                    Some("compare") => compare::present(&page.widget, Some(csv)),
                    _ => {}
                }
            });
        }
        let whole = dialog.is_some();
        let out = out.clone();
        let app = app.clone();
        gtk::glib::timeout_add_seconds_local_once(4, move || {
            let content: gtk::Widget = if whole {
                win.clone().upcast()
            } else {
                page.widget
                    .downcast_ref::<gtk::ScrolledWindow>()
                    .and_then(|s| s.child())
                    .and_then(|v| v.first_child())
                    .expect("scrolled content")
            };
            let (w, h) = (content.width(), content.height());
            let paintable = gtk::WidgetPaintable::new(Some(&content));
            let snap = gtk::Snapshot::new();
            let bg = if dark { "#242424" } else { "#fafafa" };
            snap.append_color(
                &gtk::gdk::RGBA::parse(bg).unwrap(),
                &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
            );
            paintable.snapshot(&snap, f64::from(w), f64::from(h));
            let node = snap.to_node().expect("something to render");
            let renderer = win.native().and_then(|n| n.renderer()).expect("renderer");
            renderer
                .render_texture(node, None)
                .save_to_png(&out)
                .expect("write png");
            eprintln!("wrote {out} ({w}x{h})");
            eprintln!("{}", page.debug_state());
            app.quit();
        });
    });
    app.run_with_args(&["render-monitor"]);
}
