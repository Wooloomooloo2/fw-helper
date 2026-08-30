//! `fw-helper` — desktop UI.
//!
//! Holds **no** hardware access whatsoever. Everything comes from `fw-helperd` over
//! D-Bus, and this process runs entirely unprivileged (ADR 0003).
//!
//! Controls mirror what the daemon exposes; anything it reports as unavailable is
//! shown disabled with the reason rather than silently omitted.

mod curve;
mod ui;
mod units;
mod worker;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "org.fwhelper.Gui";

/// `SIGINT` and `SIGTERM`. Spelled out rather than pulling in libc for two integers.
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| ui::load_css());
    app.connect_activate(ui::build);

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

    app.run()
}
