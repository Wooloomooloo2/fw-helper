//! Run notes: what a recording cannot measure.
//!
//! Written beside an exported session as `<session>.notes.json`. fw-helper fills in the
//! firmware, its own version and the fan setup when the recording is exported; the user
//! adds the game settings and free notes when it stops, or later. The summary shows
//! them and the compare report lists the ones that differ, so a forgotten change - a
//! different preset, a BIOS update - shows up. Same fields as the Windows build's
//! `RunContext`, in the same camelCase, so a notes file reads the same on both.

use crate::json::{self, Value};

pub const SUFFIX: &str = ".notes.json";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunNotes {
    // Filled in by fw-helper.
    pub bios: Option<String>,
    pub ec: Option<String>,
    pub kernel: Option<String>,
    pub app: Option<String>,
    pub fan_curve: Option<String>,
    // Typed by the user.
    pub game_settings: Option<String>,
    pub notes: Option<String>,
}

impl RunNotes {
    pub fn to_json(&self) -> String {
        let s = |v: &Option<String>| v.clone().map(Value::String);
        json::object([
            ("bios", s(&self.bios)),
            ("ec", s(&self.ec)),
            ("kernel", s(&self.kernel)),
            ("app", s(&self.app)),
            ("fanCurve", s(&self.fan_curve)),
            ("gameSettings", s(&self.game_settings)),
            ("notes", s(&self.notes)),
        ])
        .pretty()
    }

    /// Notes from a file's text. Unreadable notes are no notes - they are a nicety
    /// beside a recording, never a reason to refuse to show it.
    pub fn from_json(text: &str) -> Option<Self> {
        let v = json::parse(text).ok()?;
        Some(Self {
            bios: v.str_field("bios"),
            ec: v.str_field("ec"),
            kernel: v.str_field("kernel"),
            app: v.str_field("app"),
            fan_curve: v.str_field("fanCurve"),
            game_settings: v.str_field("gameSettings"),
            notes: v.str_field("notes"),
        })
    }

    /// Trimmed, or `None` when empty, so an emptied box removes the line.
    pub fn clean(text: &str) -> Option<String> {
        let t = text.trim();
        (!t.is_empty()).then(|| t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_round_trip_and_leave_out_what_is_empty() {
        let n = RunNotes {
            bios: Some("03.04".into()),
            game_settings: Some("1080p Medium, XeSS Balanced".into()),
            notes: Some("Night City market\nfelt smooth".into()),
            ..RunNotes::default()
        };
        let text = n.to_json();
        assert!(text.contains("\"gameSettings\""));
        assert!(!text.contains("\"ec\""));
        assert_eq!(RunNotes::from_json(&text), Some(n));
    }

    #[test]
    fn a_windows_notes_file_reads() {
        let n =
            RunNotes::from_json(r#"{"bios":"03.04","fanCurve":"EC auto","notes":"x"}"#).unwrap();
        assert_eq!(n.fan_curve.as_deref(), Some("EC auto"));
    }

    #[test]
    fn clean_drops_blank_text() {
        assert_eq!(RunNotes::clean("  \n "), None);
        assert_eq!(RunNotes::clean(" a "), Some("a".into()));
    }
}
