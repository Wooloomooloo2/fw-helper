//! power-profiles-daemon client.
//!
//! ADR 0005: PPD owns `platform_profile` and EPP, and GNOME's power slider is wired to
//! it. We delegate that axis and layer our own knobs on top. Writing those paths
//! ourselves would be last-writer-wins against the desktop's own UI, which is the worst
//! bug class in this project — the slider silently overrides us, or we silently override
//! it, and the UI shows a state that is not real.
//!
//! Two details, both measured on the target machine rather than assumed:
//!
//! - **PPD owns both bus names.** `org.freedesktop.UPower.PowerProfiles` and the older
//!   `net.hadess.PowerProfiles` are both registered by the same process, and both serve
//!   the interface under the newer name at `/org/freedesktop/UPower/PowerProfiles`. We
//!   prefer the newer destination and fall back to the older.
//! - **`ActiveProfile` is a writable property that emits change signals.** So switching
//!   is a property write, and following the slider is a `PropertiesChanged` subscription
//!   rather than a poll.

//! ## Why the connection is not a one-shot
//!
//! PPD is D-Bus-activatable, so *our own probe* is what starts it. On a busy boot that
//! took 26.9 s, our call hit the ~25 s D-Bus timeout, and we concluded PPD was absent —
//! then wrote `platform_profile` directly, ADR 0005's forbidden path, for the whole
//! session. PPD appeared 23 ms after we gave up. Measured: 26.8 s to a wrong verdict at
//! boot, 4 ms to the right one on restart. It also blocked startup for those 27 s.
//!
//! So the probe is **bounded** ([`PROBE_TIMEOUT`]) and a failed probe is **not final**.
//! We watch `NameOwnerChanged` on both bus names and adopt PPD whenever it appears,
//! releasing it if it goes away. The proxy is therefore mutable state, which is why it
//! sits behind a mutex — and why nothing here holds that mutex across an await.

use fw_helper_core::{Ppd, Sysfs};
use std::sync::{Arc, Mutex};

const PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const PREFERRED: &str = "org.freedesktop.UPower.PowerProfiles";
const LEGACY: &str = "net.hadess.PowerProfiles";

/// How long the startup probe waits before deciding PPD is not here *yet*.
///
/// Short on purpose. The old behaviour inherited D-Bus's ~25 s default, which is a
/// sensible timeout for a call that must succeed and a terrible one for a question we
/// can ask again later — and it blocked startup for the whole wait. Two seconds is long
/// enough for an already-running PPD (measured: 4 ms) and short enough that a boot-time
/// miss costs nothing, because [`ProfileAxis::watch_for_ppd`] will catch it.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a `NameOwnerChanged` means for the axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerEvent {
    /// One of PPD's names gained an owner: adopt it.
    Appeared,
    /// One of PPD's names lost its owner: release the proxy.
    Vanished,
    /// Some other name entirely.
    Irrelevant,
}

/// Classify a `NameOwnerChanged`, kept separate from the bus plumbing so the semantics
/// can be tested. The subtlety is that both fields are empty strings rather than absent
/// when there is no owner, so "appeared" is `new_owner` being **non-empty** — not merely
/// present.
fn classify(name: &str, old_owner: &str, new_owner: &str) -> OwnerEvent {
    if name != PREFERRED && name != LEGACY {
        return OwnerEvent::Irrelevant;
    }
    match (old_owner.is_empty(), new_owner.is_empty()) {
        (_, false) => OwnerEvent::Appeared,
        (false, true) => OwnerEvent::Vanished,
        // Neither owner: nothing changed that we can act on.
        (true, true) => OwnerEvent::Irrelevant,
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    assume_defaults = false
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, profile: &str) -> zbus::Result<()>;
}

/// How the PPD axis is being driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Delegating to PPD, as ADR 0005 requires.
    Ppd,
    /// PPD is absent, so we write `platform_profile` ourselves. Reported in
    /// capabilities, because it means the GNOME slider is not in the loop.
    DirectSysfs,
    /// Neither available.
    None,
}

/// The callback the poll loop installs to hear about slider moves. Stored so it can be
/// **re-armed** when PPD is adopted after startup — without it, adopting the proxy would
/// give us a working `set` but leave the desktop slider unwatched, which is half of
/// ADR 0005 silently missing.
type OnChange = Arc<Mutex<Option<Box<dyn FnMut(Ppd) + Send>>>>;

pub struct ProfileAxis {
    /// `None` until PPD is found, and again if it goes away. Mutable state, so it is
    /// behind a mutex — and per the project's hard rule, the guard is never held across
    /// an await: callers clone the proxy out and drop the lock before calling.
    proxy: Mutex<Option<PowerProfilesProxy<'static>>>,
    /// Kept so a proxy can be rebuilt when PPD appears. `None` when there is no system
    /// bus at all, which is the one case adoption cannot help with.
    conn: Option<zbus::Connection>,
    fs: Sysfs,
    on_change: OnChange,
    /// The live `ActiveProfile` subscription, aborted before a new one replaces it so a
    /// restarted PPD does not leave two streams calling the same callback.
    watch_task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl ProfileAxis {
    /// Connect to PPD, preferring the newer bus name.
    ///
    /// Bounded by [`PROBE_TIMEOUT`], and **not** the last word: a miss here is expected
    /// at boot and is corrected by [`Self::watch_for_ppd`].
    pub async fn connect(conn: &zbus::Connection, fs: Sysfs) -> Self {
        let axis = Self {
            proxy: Mutex::new(None),
            conn: Some(conn.clone()),
            fs,
            on_change: Arc::new(Mutex::new(None)),
            watch_task: Mutex::new(None),
        };
        let started = std::time::Instant::now();
        match axis.probe().await {
            Some(dest) => eprintln!(
                "power profiles: delegating to PPD at {dest} ({} ms)",
                started.elapsed().as_millis()
            ),
            None => eprintln!(
                "power profiles: PPD did not answer within {:?}. Writing platform_profile \
                 directly until it appears — the GNOME power slider is not in the loop \
                 meanwhile. PPD is D-Bus-activatable, so this probe may itself be what \
                 starts it",
                PROBE_TIMEOUT
            ),
        }
        axis
    }

    /// Try both bus names, storing the first proxy that answers. Returns the name used.
    async fn probe(&self) -> Option<&'static str> {
        let conn = self.conn.as_ref()?;
        for dest in [PREFERRED, LEGACY] {
            let built = PowerProfilesProxy::builder(conn)
                .destination(dest)
                .and_then(|b| b.path(PATH))
                .map(|b| b.build());
            let Ok(fut) = built else { continue };
            let Ok(proxy) = fut.await else { continue };
            // Constructing a proxy contacts nothing, so force a round trip: without it
            // an absent PPD looks identical to a present one until the first real call
            // fails. Bounded, because that round trip is what used to block startup.
            match tokio::time::timeout(PROBE_TIMEOUT, proxy.active_profile()).await {
                Ok(Ok(_)) => {
                    if let Ok(mut held) = self.proxy.lock() {
                        *held = Some(proxy);
                    }
                    return Some(dest);
                }
                // Timed out or refused: try the other name, then give up for now.
                _ => continue,
            }
        }
        None
    }

    /// No PPD at all: used when even the system bus is unreachable.
    pub fn disconnected(fs: Sysfs) -> Self {
        Self {
            proxy: Mutex::new(None),
            conn: None,
            fs,
            on_change: Arc::new(Mutex::new(None)),
            watch_task: Mutex::new(None),
        }
    }

    /// The proxy, cloned out so the lock is released before any await.
    fn proxy(&self) -> Option<PowerProfilesProxy<'static>> {
        self.proxy.lock().ok().and_then(|p| p.clone())
    }

    pub fn backend(&self) -> Backend {
        if self.proxy().is_some() {
            Backend::Ppd
        } else if self.fs.exists(fw_helper_core::paths::PLATFORM_PROFILE) {
            Backend::DirectSysfs
        } else {
            Backend::None
        }
    }

    /// What PPD says is active right now.
    pub async fn active(&self) -> Option<Ppd> {
        match self.proxy() {
            Some(p) => Ppd::parse(&p.active_profile().await.ok()?),
            None => {
                // The fallback path: ACPI's names are not PPD's, so map what we can.
                let raw = self
                    .fs
                    .read_string(fw_helper_core::paths::PLATFORM_PROFILE)
                    .ok()?;
                match raw.as_str() {
                    "low-power" | "quiet" => Some(Ppd::PowerSaver),
                    "balanced" => Some(Ppd::Balanced),
                    "performance" => Some(Ppd::Performance),
                    _ => None,
                }
            }
        }
    }

    /// Ask for a PPD profile.
    pub async fn set(&self, ppd: Ppd) -> Result<(), String> {
        match self.proxy() {
            Some(p) => p
                .set_active_profile(ppd.as_str())
                .await
                .map_err(|e| format!("PPD refused {}: {e}", ppd.as_str())),
            None => {
                // ACPI accepts its own vocabulary, which is not PPD's.
                let value = match ppd {
                    Ppd::PowerSaver => "low-power",
                    Ppd::Balanced => "balanced",
                    Ppd::Performance => "performance",
                };
                self.fs
                    .write_string(fw_helper_core::paths::PLATFORM_PROFILE, value)
                    .map_err(|e| format!("cannot write platform_profile: {e}"))
            }
        }
    }

    /// Call `on_change` whenever PPD's active profile changes.
    ///
    /// This is the half of ADR 0005 that keeps the desktop authoritative: the user moves
    /// the GNOME slider, PPD tells us, and we apply the matching fan curve and power
    /// limit. Without it we would be a second, competing source of truth.
    ///
    /// The callback is **stored**, not just used, so adoption can re-arm it. Installing
    /// it before PPD exists is fine and expected.
    pub async fn watch<F>(&self, on_change: F)
    where
        F: FnMut(Ppd) + Send + 'static,
    {
        if let Ok(mut slot) = self.on_change.lock() {
            *slot = Some(Box::new(on_change));
        }
        self.arm_profile_watch().await;
    }

    /// Subscribe to `ActiveProfile` changes, replacing any previous subscription.
    ///
    /// Aborting the old task matters when PPD restarts: without it each adoption adds
    /// another stream feeding the same callback, and one slider move would apply a
    /// profile several times.
    async fn arm_profile_watch(&self) {
        let Some(proxy) = self.proxy() else {
            eprintln!("power profiles: no PPD to follow yet; the desktop slider is not wired in");
            return;
        };
        // Nothing to call yet. `watch` arms again once the poll loop installs one, so
        // arriving here first is harmless rather than a missed subscription.
        if self.on_change.lock().map(|c| c.is_none()).unwrap_or(true) {
            return;
        }

        let mut stream = proxy.receive_active_profile_changed().await;
        let sink = Arc::clone(&self.on_change);
        let task = tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(change) = stream.next().await {
                let Ok(name) = change.get().await else {
                    continue;
                };
                match Ppd::parse(&name) {
                    Some(ppd) => {
                        if let Ok(mut cb) = sink.lock() {
                            if let Some(f) = cb.as_mut() {
                                f(ppd);
                            }
                        }
                    }
                    None => eprintln!("power profiles: PPD reports unknown profile {name:?}"),
                }
            }
        });
        if let Ok(mut slot) = self.watch_task.lock() {
            if let Some(previous) = slot.replace(task.abort_handle()) {
                previous.abort();
            }
        }
    }

    /// Adopt PPD whenever it appears, and let go when it leaves.
    ///
    /// The fix for the boot race. PPD is D-Bus-activatable, so a cold boot can easily
    /// have us probing before it exists; a single probe at startup turns that timing
    /// accident into a permanent verdict for the whole session. `NameOwnerChanged` turns
    /// it back into a temporary one.
    ///
    /// Watched on **both** bus names because PPD registers both, and either may be the
    /// one that appears first.
    pub async fn watch_for_ppd(self: &std::sync::Arc<Self>) {
        let Some(conn) = self.conn.clone() else {
            // No system bus at all. Adoption cannot help, and `connect` already said so.
            return;
        };
        let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("power profiles: cannot watch for PPD appearing ({e})");
                return;
            }
        };
        let mut stream = match dbus.receive_name_owner_changed().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("power profiles: cannot watch for PPD appearing ({e})");
                return;
            }
        };

        let axis = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str();
                let old = args.old_owner().as_ref().map(|o| o.as_str()).unwrap_or("");
                let new = args.new_owner().as_ref().map(|o| o.as_str()).unwrap_or("");
                match classify(name, old, new) {
                    OwnerEvent::Appeared => {
                        if axis.proxy().is_some() {
                            continue; // Already delegating; the other name just showed up.
                        }
                        if let Some(dest) = axis.probe().await {
                            eprintln!(
                                "power profiles: PPD appeared at {dest}; adopting it and \
                                 leaving platform_profile alone from here"
                            );
                            axis.arm_profile_watch().await;
                        }
                    }
                    OwnerEvent::Vanished => {
                        // Only if both names are gone: PPD owns two, and losing one
                        // while it still owns the other is not PPD going away.
                        if axis.probe().await.is_none() {
                            if let Ok(mut held) = axis.proxy.lock() {
                                *held = None;
                            }
                            if let Ok(mut slot) = axis.watch_task.lock() {
                                if let Some(task) = slot.take() {
                                    task.abort();
                                }
                            }
                            eprintln!(
                                "power profiles: PPD went away; falling back to writing \
                                 platform_profile until it returns"
                            );
                        }
                    }
                    OwnerEvent::Irrelevant => {}
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ppds_own_names_matter() {
        assert_eq!(
            classify("org.example.Other", "", ":1.5"),
            OwnerEvent::Irrelevant
        );
        assert_eq!(classify(PREFERRED, "", ":1.5"), OwnerEvent::Appeared);
        assert_eq!(classify(LEGACY, "", ":1.5"), OwnerEvent::Appeared);
    }

    #[test]
    fn an_empty_new_owner_is_a_departure_not_an_arrival() {
        // The subtlety worth pinning: both fields are empty strings rather than absent
        // when there is no owner. Reading "new_owner is present" as "appeared" would
        // adopt PPD at the moment it exits.
        assert_eq!(classify(PREFERRED, ":1.5", ""), OwnerEvent::Vanished);
        assert_eq!(classify(PREFERRED, "", ""), OwnerEvent::Irrelevant);
    }

    #[test]
    fn a_replaced_owner_counts_as_appearing() {
        // PPD restarting hands the name straight from one unique name to another with
        // no empty phase. Treating that as anything but "appeared" would leave us
        // holding a proxy to a process that no longer exists.
        assert_eq!(classify(PREFERRED, ":1.5", ":1.9"), OwnerEvent::Appeared);
    }

    #[test]
    fn the_probe_timeout_is_far_below_the_dbus_default() {
        // The whole defect was inheriting D-Bus's ~25 s timeout for a question we can
        // ask again later, and blocking startup for it.
        assert!(PROBE_TIMEOUT < std::time::Duration::from_secs(5));
    }
}
