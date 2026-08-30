//! Framework's custom EC charge-limit command, expressed as bytes.
//!
//! Encoding and decoding only — no I/O. The transport is an `ioctl` on
//! `/dev/cros_ec`, which needs libc and therefore lives in the daemon (ADR 0010).
//! Keeping the wire format here means the fiddly part — which is the byte *order* —
//! is unit-tested with no hardware and no root.
//!
//! Why this exists at all: the standard CrOS EC charge command is present on this
//! board and does nothing, because Framework's EC also implements a custom one that
//! overrides it. Measured 2026-08-26 with the standard threshold at 80%, this command
//! reported `max=100 min=0` — two independent limits, and this is the one that
//! governs. See ADR 0012, and the Outcome section of ADR 0008 for what it replaces.
//!
//! Definitions verified against `FrameworkComputer/framework-system`,
//! `framework_lib/src/chromium_ec/{command,commands}.rs` — not from memory. An earlier
//! lookup produced `0x3E07` for the command id, which is wrong.

/// `EcCommands::ChargeLimitControl`.
pub const CHARGE_LIMIT_CONTROL: u32 = 0x3E03;

/// Bytes in a charge-limit request: `modes`, `max_percentage`, `min_percentage`.
pub const REQUEST_LEN: usize = 3;
/// Bytes the EC returns for a `Get`: `max_percentage`, `min_percentage`.
pub const GET_RESPONSE_LEN: usize = 2;

/// `ChargeLimitControlModes`. A bitfield in Framework's source, but the modes we use
/// are issued singly.
pub mod mode {
    /// Disable all settings; charging goes back to being handled automatically.
    pub const DISABLE: u8 = 0x01;
    /// Set the maximum and minimum percentage.
    pub const SET: u8 = 0x02;
    /// Read the current setting. The only mode that returns a response.
    pub const GET: u8 = 0x08;
    /// Charge to full this once, without clearing the limit.
    pub const OVERRIDE: u8 = 0x80;
}

/// A charge window as the EC holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeLimits {
    pub min: u8,
    pub max: u8,
}

/// Ask the EC what it currently holds.
///
/// The `0xFF` padding is what Framework's own tool sends; the EC ignores it for a
/// `Get`, but sending the same bytes keeps us on the tested path.
pub fn get_request() -> [u8; REQUEST_LEN] {
    [mode::GET, 0xFF, 0xFF]
}

/// Set the charge window.
///
/// Note the order: **`max` precedes `min` on the wire**, which is the reverse of how
/// the pair reads in most of this codebase and the easiest thing here to get backwards.
/// Getting it wrong sets a *minimum* of 80%, which on a battery sitting above 80 would
/// look exactly like success.
pub fn set_request(limits: ChargeLimits) -> [u8; REQUEST_LEN] {
    [mode::SET, limits.max, limits.min]
}

/// Hand charging back to the EC's own management.
pub fn disable_request() -> [u8; REQUEST_LEN] {
    [mode::DISABLE, 0xFF, 0xFF]
}

/// Decode a `Get` response. `None` if the EC returned too few bytes to trust.
pub fn parse_limits(response: &[u8]) -> Option<ChargeLimits> {
    if response.len() < GET_RESPONSE_LEN {
        return None;
    }
    Some(ChargeLimits {
        max: response[0],
        min: response[1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_id_is_the_one_verified_against_framework_source() {
        // Pinned deliberately. A wrong opcode here is not a compile error and not a
        // runtime error either - the EC would simply answer a different question.
        //
        // Two summary lookups disagreed (0x3E07 vs 0x3E03) and this was settled by
        // reading the enum with its neighbours. Corroborated 2026-08-30 by
        // CrOS_EC_Python, an unrelated third-party library, which lists the same value.
        assert_eq!(CHARGE_LIMIT_CONTROL, 0x3E03);
    }

    #[test]
    fn set_puts_max_before_min_on_the_wire() {
        // The single most dangerous transposition in this module: swapping these sets
        // a minimum rather than a maximum, and on a battery already above the value
        // that failure is indistinguishable from success.
        let req = set_request(ChargeLimits { min: 0, max: 80 });
        assert_eq!(req, [mode::SET, 80, 0]);
    }

    #[test]
    fn get_request_matches_framework_tool() {
        assert_eq!(get_request(), [0x08, 0xFF, 0xFF]);
    }

    #[test]
    fn parses_the_response_in_max_min_order() {
        // The probe run on hardware returned exactly these bytes while the standard
        // sysfs threshold read 80 - which is the measurement this whole module exists
        // because of.
        assert_eq!(
            parse_limits(&[100, 0]),
            Some(ChargeLimits { min: 0, max: 100 })
        );
    }

    #[test]
    fn refuses_a_short_response_rather_than_inventing_a_limit() {
        assert_eq!(parse_limits(&[]), None);
        assert_eq!(parse_limits(&[80]), None);
    }

    #[test]
    fn round_trips_through_the_wire_format() {
        let want = ChargeLimits { min: 20, max: 80 };
        let req = set_request(want);
        // What the EC would echo back to a Get, in its own order.
        assert_eq!(parse_limits(&[req[1], req[2]]), Some(want));
    }
}
