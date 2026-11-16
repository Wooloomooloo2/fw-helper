//! Ctrl+Shift+F5 to start and stop a recording, from inside a game.
//!
//! The Windows build registers a global hotkey itself. A Wayland client cannot: there
//! is no protocol for a background application to grab a key. What GNOME offers instead
//! is a **custom keyboard shortcut**, run by the desktop itself, and that is what this
//! sets up - the same entry a person would make in Settings > Keyboard > Custom
//! Shortcuts, running `fw-helperctl record toggle`. The CLI saves the run when it stops,
//! so nothing needs the window open.
//!
//! Only offered where GNOME's schema exists; elsewhere the row is hidden and the
//! command is in the README for whatever the desktop's own shortcut settings are.

use adw::prelude::*;
use gtk::gio;
use std::rc::Rc;

const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
const CUSTOM: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
const PATH: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/fw-helper-record/";
pub const BINDING: &str = "<Control><Shift>F5";
pub const COMMAND: &str = "fw-helperctl record toggle";

fn schemas_present() -> bool {
    gio::SettingsSchemaSource::default().is_some_and(|src| {
        src.lookup(MEDIA_KEYS, true).is_some() && src.lookup(CUSTOM, true).is_some()
    })
}

fn installed() -> bool {
    schemas_present()
        && gio::Settings::new(MEDIA_KEYS)
            .strv("custom-keybindings")
            .iter()
            .any(|p| p.as_str() == PATH)
}

fn install() -> Result<(), String> {
    let entry = gio::Settings::with_path(CUSTOM, PATH);
    entry
        .set_string("name", "fw-helper: record")
        .map_err(|e| e.to_string())?;
    entry
        .set_string("command", COMMAND)
        .map_err(|e| e.to_string())?;
    entry
        .set_string("binding", BINDING)
        .map_err(|e| e.to_string())?;
    let keys = gio::Settings::new(MEDIA_KEYS);
    let mut list: Vec<String> = keys
        .strv("custom-keybindings")
        .iter()
        .map(|s| s.to_string())
        .collect();
    if !list.iter().any(|p| p == PATH) {
        list.push(PATH.to_string());
    }
    let refs: Vec<&str> = list.iter().map(String::as_str).collect();
    keys.set_strv("custom-keybindings", refs.as_slice())
        .map_err(|e| e.to_string())
}

fn remove() -> Result<(), String> {
    let keys = gio::Settings::new(MEDIA_KEYS);
    let list: Vec<String> = keys
        .strv("custom-keybindings")
        .iter()
        .map(|s| s.to_string())
        .filter(|p| p != PATH)
        .collect();
    let refs: Vec<&str> = list.iter().map(String::as_str).collect();
    keys.set_strv("custom-keybindings", refs.as_slice())
        .map_err(|e| e.to_string())?;
    let entry = gio::Settings::with_path(CUSTOM, PATH);
    for k in ["name", "command", "binding"] {
        entry.reset(k);
    }
    Ok(())
}

/// A line and a button for the Monitor page.
pub fn row() -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    if !schemas_present() {
        row.set_visible(false);
        return row;
    }
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .css_classes(["stat-label"])
        .build();
    let button = gtk::Button::builder().valign(gtk::Align::Center).build();
    row.append(&label);
    row.append(&button);

    let sync = Rc::new({
        let (label, button) = (label.clone(), button.clone());
        move |error: Option<String>| {
            let on = installed();
            label.set_label(&match (&error, on) {
                (Some(e), _) => format!("Could not change the shortcut: {e}"),
                (None, true) => {
                    "Ctrl+Shift+F5 starts and stops a recording, from inside a game too, \
                                 and saves it to FW Benchmark when it stops."
                        .to_string()
                }
                (None, false) => {
                    "A keyboard shortcut can start and stop recordings without leaving \
                                  a game. It runs \u{201c}fw-helperctl record toggle\u{201d}."
                        .to_string()
                }
            });
            button.set_label(if on {
                "Remove Shortcut"
            } else {
                "Add Ctrl+Shift+F5 Shortcut"
            });
        }
    });
    sync(None);
    let s = Rc::clone(&sync);
    button.connect_clicked(move |_| {
        let result = if installed() { remove() } else { install() };
        s(result.err());
    });
    row
}
