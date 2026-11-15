//! Render the Battery page to a PNG, to look at it without disturbing a running GUI.
//!
//!   FW_HELPERD_SESSION_BUS=1 XDG_RUNTIME_DIR=<dir with fw-helper-dev/battery/events.csv> \
//!     cargo run -p fw-helper-gui --example render-battery -- out.png [dark]
//!
//! Opens a window for about two seconds under its own application id - the real GUI is
//! single-instance, so a second copy of it would only raise the first - then paints the
//! page's whole content (not just the visible part of its scroller) and quits.

use adw::prelude::*;

#[path = "../src/chart.rs"]
#[allow(dead_code)]
mod chart;

#[path = "../src/battery.rs"]
mod battery;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "battery.png".into());
    let dark = args.get(2).map(String::as_str) == Some("dark");

    let app = adw::Application::builder()
        .application_id("org.fwhelper.RenderBattery")
        .build();
    app.connect_activate(move |app| {
        adw::StyleManager::default().set_color_scheme(if dark {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
        let page = battery::BatteryPage::new();
        let win = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1000)
            .default_height(900)
            .content(&page.widget)
            .build();
        win.present();
        let out = out.clone();
        let app = app.clone();
        gtk::glib::timeout_add_seconds_local_once(2, move || {
            // The scroller's content, which is allocated at its full natural height.
            let content = page
                .widget
                .downcast_ref::<gtk::ScrolledWindow>()
                .and_then(|s| s.child())
                .and_then(|v| v.first_child())
                .expect("scrolled content");
            let (w, h) = (content.width(), content.height());
            let paintable = gtk::WidgetPaintable::new(Some(&content));
            let snap = gtk::Snapshot::new();
            // Paint the window background first, so the PNG is not transparent.
            let bg = if dark { "#242424" } else { "#fafafa" };
            snap.append_color(
                &gtk::gdk::RGBA::parse(bg).unwrap(),
                &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
            );
            paintable.snapshot(&snap, f64::from(w), f64::from(h));
            let node = snap.to_node().expect("something to render");
            let renderer = win.native().and_then(|n| n.renderer()).expect("renderer");
            let texture = renderer.render_texture(node, None);
            texture.save_to_png(&out).expect("write png");
            eprintln!("wrote {out} ({w}x{h})");
            app.quit();
        });
    });
    app.run_with_args(&["render-battery"]);
}
