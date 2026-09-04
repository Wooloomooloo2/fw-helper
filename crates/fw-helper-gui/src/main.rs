//! `fw-helper` — desktop UI.
//!
//! Holds **no** hardware access whatsoever. Everything comes from `fw-helperd` over
//! D-Bus, and this process runs entirely unprivileged (ADR 0003).
//!
//! Controls mirror what the daemon exposes; anything it reports as unavailable is
//! shown disabled with the reason rather than silently omitted.

mod chart;
mod curve;
mod monitor;
mod overlay;
mod ui;
mod units;
mod worker;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "org.fwhelper.Gui";
const OVERLAY_APP_ID: &str = "org.fwhelper.Gui.Overlay";

const USAGE: &str = "\
fw-helper \u{2014} Framework laptop firmware control

USAGE:
    fw-helper              the main window
    fw-helper --overlay    a compact readout

The overlay is an ordinary window. On GNOME/Wayland no ordinary window can be kept
above a fullscreen game, so for numbers inside one, use MangoHud - it is loaded into
the game itself. See /usr/share/fw-helper/mangohud/fw-helper.conf.
";

/// `SIGINT` and `SIGTERM`. Spelled out rather than pulling in libc for two integers.
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

fn main() -> glib::ExitCode {
    // A compact readout instead of the main window. Its own application id, because GTK
    // applications are single-instance: sharing one would make `--overlay` activate an
    // already-open main window and appear to do nothing.
    let overlay = std::env::args().any(|a| a == "--overlay");
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }

    let app = adw::Application::builder()
        .application_id(if overlay { OVERLAY_APP_ID } else { APP_ID })
        .build();
    app.connect_startup(|_| ui::load_css());
    if overlay {
        app.connect_activate(overlay::build);
    } else {
        app.connect_activate(ui::build);
    }

    // Quit cleanly on Ctrl-C and on SIGTERM.
    //
    // GTK installs no handler for either, and this app is normally started from a
    // terminal, so the obvious way to stop it is Ctrl-C. Exiting cleanly also releases
    // the application ID: GTK applications are single-instance, so a lingering process
    // makes the *next* launch silently activate the old window instead of starting the
    // new build — which looks exactly like a build that did nothing.
    for signal in [SIGINT, SIGTERM] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            eprintln!("signal {signal}, closing");
            app.quit();
            glib::ControlFlow::Break
        });
    }

    // GApplication parses argv itself and rejects anything it does not recognise, so
    // `--overlay` would be an "Unknown option" error before ever reaching us. Our flags
    // are read above and only the program name is handed on.
    let argv0 = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[argv0])
}
