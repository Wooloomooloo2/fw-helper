//! The `org.fwhelper.Daemon1` interface.
//!
//! Properties are read-only and unauthenticated; every method that touches hardware
//! goes through `authorize` first, per action, failing closed (ADR 0003).

use crate::fan::FanLease;
use crate::state::State;
use crate::tuning::TuningLease;
use crate::watchdog::Watchdog;
use crate::{polkit, wire};
use fw_helper_core::tune::ParkLevel;
use fw_helper_core::{Capabilities, PowerLimit, Sysfs, Telemetry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zbus::zvariant::OwnedValue;

/// Outcome of [`Daemon::reapply_charge_limit`].
///
/// `AlreadyCorrect` versus `Corrected` is the distinction worth having: it says whether
/// firmware actually reset the threshold, which no amount of unconditional writing can
/// reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reapply {
    /// No limit persisted; nothing to do.
    NothingPersisted,
    /// Hardware already held the persisted value. Firmware left it alone.
    AlreadyCorrect,
    /// Hardware disagreed and has been corrected. Firmware reset it, or something else did.
    Corrected,
    /// The re-apply was attempted and failed. Reason already logged.
    Failed,
}

/// The handles the daemon shares with the poll loop.
///
/// A struct rather than five more arguments: they are all `Arc`s of similar shape, and
/// transposing two of them would compile.
pub struct Shared {
    pub fan: Arc<FanLease>,
    /// The EC transport. Injected rather than constructed here so the charge-limit
    /// paths stay testable: an ioctl has no fixture-tree equivalent (ADR 0012).
    pub ec: Arc<dyn crate::ec::EcTransport>,
    pub watchdog: Arc<Watchdog>,
    pub axis: Arc<crate::ppd::ProfileAxis>,
    pub applied_ppd: Arc<std::sync::atomic::AtomicU8>,
    /// Behind a mutex because saving a profile changes it at run time.
    pub profiles: Arc<Mutex<Vec<fw_helper_core::Profile>>>,
    /// The recording in progress, shared with the poll loop that feeds it rows.
    pub recording: Arc<crate::record::Recording>,
    /// Set when a client reads telemetry, so the poll loop knows the GPU scan is
    /// worth its cost.
    pub watchers: Arc<crate::record::Watchers>,
    /// Core parking and GPU frequency. Shared with `main`, which restores it on every
    /// exit path — ADR 0014.
    pub tuning: Arc<TuningLease>,
}

pub struct Daemon {
    fs: Sysfs,
    caps: Capabilities,
    /// Framework's custom EC command is the only mechanism that governs charging on
    /// this board; sysfs holds a number that does nothing (ADR 0012).
    ec: Arc<dyn crate::ec::EcTransport>,
    /// Behind a mutex for the same reason `state` is, and it matters more than it
    /// looks. Updating it used to need `&mut self`, so the poll loop took zbus's
    /// interface **write** lock every second. Any method awaiting something slow holds
    /// the read lock meanwhile — and a polkit password prompt is very slow — so the
    /// poll loop stalled for the length of the prompt. Measured 2026-08-22: a prompt
    /// stopped the heartbeat for 6 s and the fan watchdog took the fan back, mid-dialog.
    latest: Mutex<Telemetry>,
    /// Behind a mutex so write methods can take `&self`. A `&mut self` method makes
    /// zbus hold the interface **write** lock for the whole call, and a polkit prompt
    /// can legitimately take tens of seconds — which would stall telemetry for every
    /// client while a password dialog sits on screen.
    state: Mutex<State>,
    /// Shared with `main`, which releases it on every shutdown path. Lock-free by
    /// design so the panic hook can use it (ADR 0006).
    fan: Arc<FanLease>,
    /// Consulted before granting manual fan control, so the fan is never handed to a
    /// daemon that has stopped minding it.
    watchdog: Arc<Watchdog>,
    /// The PPD axis (ADR 0005). Shared with the poll loop, which follows it.
    axis: Arc<crate::ppd::ProfileAxis>,
    /// The PPD profile we set ourselves. Shared with the poll loop so it can tell our
    /// own echo from a genuine slider move — setting PPD makes PPD emit a change, and
    /// the two are indistinguishable at the receiving end.
    applied_ppd: Arc<std::sync::atomic::AtomicU8>,
    /// Built-ins merged with anything in `/etc/fw-helper/profiles.d/`.
    profiles: Arc<Mutex<Vec<fw_helper_core::Profile>>>,
    /// Machine load, published separately from telemetry because it is sampled
    /// differently — the GPU half only runs while somebody is watching.
    usage: Mutex<fw_helper_core::Usage>,
    recording: Arc<crate::record::Recording>,
    watchers: Arc<crate::record::Watchers>,
    /// Lock-free for the same reason `fan` is: the panic hook restores through it and
    /// must not block (ADR 0014).
    tuning: Arc<TuningLease>,
}

impl Daemon {
    pub fn new(fs: Sysfs, caps: Capabilities, state: State, shared: Shared) -> Self {
        Self {
            fs,
            caps,
            ec: shared.ec,
            latest: Mutex::new(Telemetry::default()),
            state: Mutex::new(state),
            fan: shared.fan,
            watchdog: shared.watchdog,
            axis: shared.axis,
            applied_ppd: shared.applied_ppd,
            profiles: shared.profiles,
            usage: Mutex::new(fw_helper_core::Usage::default()),
            recording: shared.recording,
            watchers: shared.watchers,
            tuning: shared.tuning,
        }
    }

    /// The temperature a fan decision should be based on, if any sensor is readable.
    fn control_celsius(&self) -> Option<f64> {
        self.latest
            .lock()
            .ok()
            .and_then(|t| t.control_temp().map(|c| c.celsius))
    }

    /// Everything a fan decision needs from the thermal sensors, read from the same
    /// telemetry the daemon publishes.
    fn thermal(&self) -> crate::fan::Thermal {
        match self.latest.lock() {
            Ok(t) => crate::fan::Thermal::from_telemetry(&t),
            // A poisoned lock must not become "no sensor", which would read as a
            // reason to refuse rather than a reason to be careful.
            Err(e) => crate::fan::Thermal::from_telemetry(&e.into_inner().clone()),
        }
    }

    /// Authorize a caller for one action, or say why not.
    ///
    /// Every hardware-touching method starts here. Factored out because the sequence
    /// — identify the caller, then check the action, failing closed — must be
    /// identical for each one, and a method that forgets a step is a method that
    /// writes to hardware unauthenticated.
    async fn authorize(
        header: &zbus::message::Header<'_>,
        conn: &zbus::Connection,
        action: &str,
    ) -> zbus::fdo::Result<String> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| zbus::fdo::Error::AuthFailed("no caller identity".into()))?;
        polkit::check(conn, &sender, action)
            .await
            .map_err(zbus::fdo::Error::AuthFailed)?;
        Ok(sender)
    }

    /// Re-apply the persisted charge limit. Called at startup, because
    /// `charge_control_end_threshold` does not survive a reboot, and on resume,
    /// because firmware may reset it.
    ///
    /// Reads before writing, and this is not an optimisation — the write is cheap and
    /// idempotent. It is about what the log can tell us afterwards. Writing
    /// unconditionally emits the same line whether or not firmware disturbed anything,
    /// which is precisely the question the resume hook exists to answer. Separating the
    /// two cases makes every resume a free measurement of whether this hook is
    /// load-bearing on this hardware, rather than an assumption we keep re-asserting.
    ///
    /// Returns what happened, so callers and tests can distinguish the cases without
    /// scraping stderr.
    pub fn reapply_charge_limit(&self) -> Reapply {
        let Some(limit) = self.state.lock().ok().and_then(|s| s.charge_limit) else {
            return Reapply::NothingPersisted;
        };
        match crate::ec::read_charge_limit(&*self.ec) {
            Ok(observed) if observed == limit => {
                eprintln!("charge limit still {limit}%; nothing to re-apply");
                return Reapply::AlreadyCorrect;
            }
            Ok(observed) => {
                eprintln!("charge limit is {observed}%, expected {limit}%; re-applying");
            }
            // Fall through to the write deliberately. The usual cause is the EC being
            // unreachable, and `set_charge_limit` reports that with the actionable
            // message rather than the bare error we would have to invent here.
            Err(e) => {
                eprintln!("cannot read charge limit ({e}); re-applying {limit}% anyway");
            }
        }

        match crate::ec::set_charge_limit(&self.fs, &*self.ec, limit) {
            Ok(()) => {
                eprintln!("re-applied charge limit {limit}%");
                Reapply::Corrected
            }
            Err(e) => {
                eprintln!("could not re-apply charge limit {limit}%: {e}");
                Reapply::Failed
            }
        }
    }

    /// Record the learned firmware floor alongside the rest of the persisted state.
    ///
    /// Lives here because the state file is one file with one owner: writing it from
    /// the poll loop directly would race the charge-limit write and silently drop one.
    pub fn save_floor(&self, observations: Vec<(f64, u8)>) {
        if let Ok(mut state) = self.state.lock() {
            if state.floor == observations {
                return;
            }
            state.floor = observations;
            state.save();
        }
    }

    /// The power limit we are currently trying to hold, if any.
    ///
    /// Read by the poll loop to notice when the setpoint *changes*, which is what
    /// resets the correction budget. Every path that changes it — `set_profile`,
    /// `set_power_limit`, `record_profile` — writes this same field, so watching it
    /// catches all of them without each having to remember to signal.
    pub fn desired_power_limit(&self) -> Option<u32> {
        self.state.lock().ok().and_then(|s| s.power_limit)
    }

    /// Re-apply the persisted power limit.
    ///
    /// Same shape as [`Self::reapply_charge_limit`], and for the same reason: read
    /// before writing, so the log distinguishes "firmware left it alone" from "firmware
    /// reset it and we corrected it". Whether RAPL survives suspend on this hardware is
    /// not yet known, and this is how it gets answered rather than assumed.
    pub fn reapply_power_limit(&self) -> Reapply {
        let Some(watts) = self.state.lock().ok().and_then(|s| s.power_limit) else {
            return Reapply::NothingPersisted;
        };
        let pl = PowerLimit::new(&self.fs);
        match pl.read() {
            Ok(observed) if observed == watts => {
                eprintln!("power limit still {watts} W; nothing to re-apply");
                return Reapply::AlreadyCorrect;
            }
            Ok(observed) => {
                eprintln!("power limit is {observed} W, expected {watts} W; re-applying")
            }
            Err(e) => eprintln!("cannot read power limit ({e}); re-applying {watts} W anyway"),
        }
        match pl.set(watts) {
            Ok(()) => {
                eprintln!("re-applied power limit {watts} W");
                Reapply::Corrected
            }
            Err(e) => {
                eprintln!("could not re-apply power limit {watts} W: {e}");
                Reapply::Failed
            }
        }
    }

    /// Record which profile is active and what power budget it wants.
    ///
    /// **Must be called by every path that applies a profile**, not just the D-Bus one.
    /// `enforce_power_limit` re-asserts whatever this says, so a path that changes the
    /// hardware without recording it leaves a stale desired value that the enforcement
    /// loop then dutifully restores — measured: following the GNOME slider to
    /// performance applied 25 W, and the enforcement put it straight back to the 15 W a
    /// previous profile had recorded.
    pub fn record_profile(&self, name: &str, watts: u32) {
        if let Ok(mut state) = self.state.lock() {
            if state.profile.as_deref() == Some(name) && state.power_limit == Some(watts) {
                return;
            }
            state.profile = Some(name.to_string());
            state.power_limit = Some(watts);
            state.save();
        }
    }

    /// A snapshot of the known profiles.
    fn known(&self) -> Vec<fw_helper_core::Profile> {
        self.profiles.lock().map(|p| p.clone()).unwrap_or_default()
    }

    /// Look a profile up, or say what does exist.
    fn by_name(&self, name: &str) -> zbus::fdo::Result<fw_helper_core::Profile> {
        let known = self.known();
        known
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| {
                let names: Vec<&str> = known.iter().map(|p| p.name.as_str()).collect();
                zbus::fdo::Error::InvalidArgs(format!(
                    "no profile {name:?}; known profiles are {}",
                    names.join(", ")
                ))
            })
    }

    /// The profile configured for a power source, if any.
    pub fn auto_profile_for(&self, on_ac: bool) -> Option<String> {
        let s = self.state.lock().ok()?;
        if on_ac {
            s.profile_on_ac.clone()
        } else {
            s.profile_on_battery.clone()
        }
    }

    /// Re-assert the power limit if something moved it.
    ///
    /// **Our write can be verified and still not stick.** Measured 2026-08-22: after a
    /// profile set PL1 to 25 W and the read-back confirmed it, the zone read 33 W a few
    /// seconds later — above its own advertised `max_power_uw` of 25 W, so firmware is
    /// not bound by that field either. Switching `platform_profile` appears to make
    /// firmware re-derive PL1 asynchronously, and an immediate read-back cannot see it.
    ///
    /// Returns `(desired, observed)` when it corrected something, so the caller can
    /// decide how loudly to say so.
    pub fn enforce_power_limit(&self) -> Option<(u32, u32)> {
        let desired = self.state.lock().ok()?.power_limit?;
        let pl = PowerLimit::new(&self.fs);
        let observed = pl.read().ok()?;
        if observed == desired {
            return None;
        }
        pl.set(desired).ok()?;
        Some((desired, observed))
    }

    /// Called by the poll task. Returns true when the published view actually changed,
    /// so we only emit a PropertiesChanged signal when there is something to say.
    pub fn update(&self, t: Telemetry) -> bool {
        let Ok(mut latest) = self.latest.lock() else {
            return false;
        };
        let changed = *latest != t;
        *latest = t;
        changed
    }

    /// Publish the latest load sample. Called by the poll task alongside [`Self::update`].
    pub fn update_usage(&self, u: fw_helper_core::Usage) {
        if let Ok(mut latest) = self.usage.lock() {
            *latest = u;
        }
    }

    /// A snapshot of the control state a recorded row needs and telemetry does not
    /// carry: what limit we are enforcing, who owns the fan, which profile is on.
    pub fn record_context(&self) -> crate::record::Context {
        crate::record::Context {
            pl1_watts: self.state.lock().ok().and_then(|s| s.power_limit),
            fan_mode: Some(match self.fan.mode() {
                Some(fw_helper_core::FanMode::Manual) => "manual".to_string(),
                Some(m) => m.to_string(),
                None => "unavailable".to_string(),
            }),
            fan_duty: self.fan.duty(),
            profile: self.state.lock().ok().and_then(|s| s.profile.clone()),
        }
    }

    pub fn recording(&self) -> &Arc<crate::record::Recording> {
        &self.recording
    }
}

#[zbus::interface(name = "org.fwhelper.Daemon1")]
impl Daemon {
    /// Per-knob availability. Clients disable controls this reports as unavailable
    /// rather than offering something that silently does nothing.
    #[zbus(property)]
    async fn capabilities(&self) -> HashMap<String, (bool, String)> {
        wire::capabilities_dict(&self.caps)
    }

    /// Live readings. Updated at most once per second and quantized to 0.1 W —
    /// see ADR 0009. There is deliberately no on-demand sampling method: a client
    /// must not be able to drive sampling cadence.
    #[zbus(property)]
    async fn telemetry(&self) -> HashMap<String, OwnedValue> {
        // Somebody is looking. The poll loop reads this to decide whether the GPU
        // client scan earns its cost this tick.
        self.watchers.touch();
        match self.latest.lock() {
            Ok(t) => wire::telemetry_dict(&t),
            Err(_) => Default::default(),
        }
    }

    /// Machine load: CPU, memory and GPU.
    ///
    /// Separate from `Telemetry` because it is sampled on different terms. The GPU
    /// figures require walking every process's file descriptors, so they are only
    /// collected while a client is reading this — their keys appear a tick or so after
    /// somebody starts watching and disappear again afterwards, which is a property of
    /// the sampler rather than of the hardware.
    #[zbus(property)]
    async fn usage(&self) -> HashMap<String, OwnedValue> {
        self.watchers.touch();
        match self.usage.lock() {
            Ok(u) => wire::usage_dict(&u),
            Err(_) => Default::default(),
        }
    }

    /// The recording in progress as `(label, name, path, started_unix, samples)`.
    /// An empty label means nothing is being recorded.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn recording_session(&self) -> (String, String, String, u64, u64) {
        match self.recording.status() {
            Some(s) => (s.label, s.name, s.path, s.started_unix, s.samples),
            None => (String::new(), String::new(), String::new(), 0, 0),
        }
    }

    /// Recorded sessions as `(name, label, path, started_unix, bytes)`, newest first.
    ///
    /// The rows themselves are deliberately not served here. A session is tens of
    /// thousands of rows, the files are world-readable by design, and a client that
    /// wants to plot one opens it directly rather than dragging it across the bus.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn sessions(&self) -> Vec<(String, String, String, u64, u64)> {
        fw_helper_core::session::list(&crate::record::session_dir())
            .into_iter()
            .map(|m| {
                (
                    m.name,
                    m.label,
                    m.path.display().to_string(),
                    m.started_unix,
                    m.bytes,
                )
            })
            .collect()
    }

    /// Begin recording. Returns the file being written.
    async fn start_recording(
        &self,
        label: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        let sender = Self::authorize(&header, conn, polkit::actions::RECORD).await?;
        let path = self
            .recording
            .start(label)
            .map_err(zbus::fdo::Error::Failed)?;
        eprintln!("recording started by {sender}");
        Ok(path)
    }

    /// Stop recording. Returns the finished file.
    async fn stop_recording(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        Self::authorize(&header, conn, polkit::actions::RECORD).await?;
        self.recording.stop().map_err(zbus::fdo::Error::Failed)
    }

    /// Delete a recorded session by name.
    async fn delete_session(
        &self,
        name: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        Self::authorize(&header, conn, polkit::actions::RECORD).await?;

        // Never the recording in progress. Unlinking a file that is still open does not
        // fail the writes - the descriptor keeps addressing the now-nameless inode, so
        // the recording would carry on into nothing and hand back a path that no longer
        // exists when it stopped. Refuse, and say what to do instead.
        if self.recording.active_name().as_deref() == Some(name) {
            return Err(zbus::fdo::Error::Failed(format!(
                "{name} is being recorded right now; stop the recording first"
            )));
        }

        // The name is validated as a bare file stem inside `session::delete`; a caller
        // sending `../../etc/passwd` must not be able to reach outside the directory.
        fw_helper_core::session::delete(&crate::record::session_dir(), name)
            .map_err(|e| zbus::fdo::Error::Failed(format!("cannot delete {name:?}: {e}")))
    }

    /// Validated critical thresholds per sensor. Sensors reporting implausible
    /// values are omitted entirely rather than published as-is.
    #[zbus(property)]
    async fn critical_temperatures(&self) -> HashMap<String, f64> {
        match self.latest.lock() {
            Ok(t) => wire::critical_temps(&t),
            Err(_) => Default::default(),
        }
    }

    /// Interface version, so a client can refuse to talk to a daemon it does not
    /// understand. Bumped on breaking changes only.
    #[zbus(property)]
    async fn version(&self) -> u32 {
        1
    }

    /// Profiles this daemon knows, by name.
    #[zbus(property)]
    async fn profiles(&self) -> Vec<String> {
        self.known().iter().map(|p| p.name.clone()).collect()
    }

    /// Profiles that exist as files, and so can be deleted. Built-ins have none.
    #[zbus(property)]
    async fn saved_profiles(&self) -> Vec<String> {
        crate::profiles::saved_names()
    }

    /// The profile matching whatever PPD currently has active, or empty if unknown.
    ///
    /// Derived from PPD rather than from our own record of what we last applied, so it
    /// stays truthful when the user moves the GNOME slider (ADR 0005).
    #[zbus(property)]
    async fn active_profile(&self) -> String {
        let Some(ppd) = self.axis.active().await else {
            return String::new();
        };
        // Prefer the profile we actually applied, but only while PPD still agrees with
        // it. Deriving this from PPD alone cannot be right: PPD has three positions and
        // any number of profiles can share one, so selecting a user profile reported
        // back as whichever built-in shares its PPD axis - and a client that trusts the
        // report, as ours does, snaps its selection there a moment later.
        //
        // When PPD no longer matches, the slider has been moved somewhere else and the
        // canonical name for where it now is *is* the truth.
        let applied = self.state.lock().ok().and_then(|s| s.profile.clone());
        if let Some(name) = applied {
            if self.known().iter().any(|p| p.name == name && p.ppd == ppd) {
                return name;
            }
        }
        fw_helper_core::Profile::canonical_name_for(ppd).to_string()
    }

    /// How the profile axis is driven: `ppd`, `platform_profile`, or `none`.
    ///
    /// Worth publishing because `platform_profile` means the GNOME slider is *not* in
    /// the loop, and a user seeing the desktop disagree with us deserves to know why.
    #[zbus(property)]
    async fn profile_backend(&self) -> String {
        match self.axis.backend() {
            crate::ppd::Backend::Ppd => "ppd".into(),
            crate::ppd::Backend::DirectSysfs => "platform_profile".into(),
            crate::ppd::Backend::None => "none".into(),
        }
    }

    /// Switch profile: PPD axis, power budget and fan curve together.
    async fn set_profile(
        &self,
        name: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let profile = self.by_name(name)?;
        // A profile sets a power limit and takes the fan, so it needs both rights.
        let sender = Self::authorize(&header, conn, polkit::actions::SET_POWER_LIMIT).await?;
        Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;

        // Mark before asking, so the change signal cannot arrive before the flag.
        self.applied_ppd.store(
            crate::ppd_code(profile.ppd),
            std::sync::atomic::Ordering::SeqCst,
        );
        self.axis
            .set(profile.ppd)
            .await
            .map_err(zbus::fdo::Error::Failed)?;

        fw_helper_core::PowerLimit::new(&self.fs)
            .set(profile.pl1_watts)
            .map_err(|e| zbus::fdo::Error::Failed(format!("power limit: {e}")))?;

        self.fan
            .set_curve(profile.curve.clone(), self.thermal())
            .map_err(|e| zbus::fdo::Error::Failed(format!("fan curve: {e}")))?;

        // The M9 levers, applied last and **non-fatally**. A profile is a power budget
        // and a fan curve first; those two are what every existing profile means, and
        // failing the whole apply because a machine has no parkable cores would break
        // `quiet` on hardware that never had them. Reported, not raised.
        if self.tuning.parking_supported() {
            if let Err(e) = self.tuning.set_level(profile.park_cores) {
                eprintln!("profile {}: could not park cores: {e}", profile.name);
            }
        } else if profile.park_cores != ParkLevel::None {
            eprintln!(
                "profile {}: asks to park {} but this machine has no hybrid topology",
                profile.name, profile.park_cores
            );
        }
        if let Err(e) = self.tuning.set_gpu_max(profile.gpu_max_mhz) {
            eprintln!(
                "profile {}: could not set the GPU ceiling: {e}",
                profile.name
            );
        }

        if let Ok(mut state) = self.state.lock() {
            state.profile = Some(profile.name.to_string());
            state.power_limit = Some(profile.pl1_watts);
            state.save();
        }
        eprintln!("profile {} applied by {sender}", profile.name);
        Ok(())
    }

    /// Save what the machine is set to now as a profile, under `name`.
    ///
    /// Captures the current PPD profile, the current power limit, and the fan curve in
    /// use — the active profile's curve if the fan is following one, otherwise the
    /// curve of whichever profile is active. Writes it to
    /// `/etc/fw-helper/profiles.d/<name>.conf`, which is a plain file the user can edit
    /// afterwards.
    ///
    /// Saving under the name of a built-in replaces it, which is the documented way to
    /// customise `quiet` rather than accumulating a near-duplicate beside it.
    async fn save_profile(
        &self,
        name: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        let ppd = self
            .axis
            .active()
            .await
            .ok_or_else(|| zbus::fdo::Error::Failed("no power profile is active".into()))?;
        let watts = PowerLimit::new(&self.fs)
            .read()
            .map_err(|e| zbus::fdo::Error::Failed(format!("cannot read the power limit: {e}")))?;

        // Prefer the curve actually running; fall back to the active profile's.
        let curve = match self.fan.curve_points() {
            Some(points) if !points.is_empty() => fw_helper_core::Curve::new(
                points
                    .into_iter()
                    .map(|(celsius, duty)| fw_helper_core::Point { celsius, duty })
                    .collect(),
            )
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?,
            _ => fw_helper_core::Profile::for_ppd(ppd).curve,
        };

        let profile = fw_helper_core::Profile {
            name: name.to_string(),
            ppd,
            pl1_watts: watts,
            curve,
            // Captured from the machine, like the power limit and the curve: these are
            // performance choices, which is exactly what a profile is for.
            park_cores: self.tuning.observed_level().unwrap_or_default(),
            gpu_max_mhz: self.tuning.gpu_cap(),
            // Deliberately not captured: a charge limit is a standing preference, and
            // folding whatever it happens to be into a performance profile would make
            // switching profiles change it later, which nobody asked for.
            charge_limit: None,
        };
        profile
            .validate()
            .map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;

        let sender = Self::authorize(&header, conn, polkit::actions::SET_POWER_LIMIT).await?;
        let path = crate::profiles::save(&profile).map_err(zbus::fdo::Error::Failed)?;

        // Take effect now rather than at the next restart.
        if let Ok(mut known) = self.profiles.lock() {
            match known.iter().position(|p| p.name == profile.name) {
                Some(i) => known[i] = profile.clone(),
                None => known.push(profile.clone()),
            }
        }
        eprintln!(
            "profile {} saved by {sender}: ppd={} pl1={} W -> {}",
            profile.name,
            profile.ppd.as_str(),
            profile.pl1_watts,
            path.display()
        );
        Ok(path.display().to_string())
    }

    /// Delete a saved profile. Built-ins have no file and cannot be removed.
    async fn delete_profile(
        &self,
        name: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender = Self::authorize(&header, conn, polkit::actions::SET_POWER_LIMIT).await?;
        crate::profiles::delete(name).map_err(zbus::fdo::Error::Failed)?;

        if let Ok(mut known) = self.profiles.lock() {
            known.retain(|p| p.name != name);
            // A deleted file may have been shadowing a built-in, which now comes back.
            for built_in in fw_helper_core::Profile::built_ins() {
                if built_in.name == name {
                    known.push(built_in);
                }
            }
        }
        eprintln!("profile {name} deleted by {sender}");
        Ok(())
    }

    /// Profiles applied when the power source changes: `(on_ac, on_battery)`.
    ///
    /// Empty strings mean "leave it alone on that source", and both empty means the
    /// feature is off.
    #[zbus(property)]
    async fn auto_profiles(&self) -> (String, String) {
        match self.state.lock() {
            Ok(s) => (
                s.profile_on_ac.clone().unwrap_or_default(),
                s.profile_on_battery.clone().unwrap_or_default(),
            ),
            Err(_) => (String::new(), String::new()),
        }
    }

    /// Choose what to apply when the power source changes.
    ///
    /// Off by default and never inferred: a machine that changes behaviour when a cable
    /// is plugged in, without having been asked to, is a machine behaving strangely.
    async fn set_auto_profiles(
        &self,
        on_ac: &str,
        on_battery: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        for name in [on_ac, on_battery] {
            if !name.is_empty() {
                self.by_name(name)?;
            }
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_POWER_LIMIT).await?;
        if let Ok(mut state) = self.state.lock() {
            state.profile_on_ac = Some(on_ac.to_string()).filter(|v| !v.is_empty());
            state.profile_on_battery = Some(on_battery.to_string()).filter(|v| !v.is_empty());
            state.save();
        }
        eprintln!("auto profiles set by {sender}: ac={on_ac:?} battery={on_battery:?}");
        Ok(())
    }

    // ------------------------------------------------------------------------------
    // M9 — core parking and GPU frequency (ADR 0014)
    // ------------------------------------------------------------------------------

    /// The CPU clusters this machine has, as `(name, cpus, max_mhz, parkable_count)`.
    ///
    /// Published rather than left to the client to discover, for the same reason the
    /// capability list is: a GUI must not hardcode "P is 0-3". The reference part has
    /// three clusters and no SMT, but this crate's job is to say what *this* machine
    /// has. `parkable_count` is below `cpus` wherever `cpu0` is in the cluster.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn cpu_clusters(&self) -> Vec<(String, Vec<u32>, u32, u32)> {
        let set = self.tuning.core_set();
        set.clusters()
            .into_iter()
            .map(|c| {
                let cpus = set.in_cluster(c);
                (
                    c.as_str().to_string(),
                    cpus.iter().map(|x| x.num).collect(),
                    cpus.iter().map(|x| x.max_khz / 1000).max().unwrap_or(0),
                    cpus.iter().filter(|x| x.parkable).count() as u32,
                )
            })
            .collect()
    }

    /// `(requested, observed)` park level. They differ when someone has offlined cores
    /// by hand, in which case `observed` is `mixed` — reported, never silently fixed.
    #[zbus(property)]
    async fn park_level(&self) -> (String, String) {
        (
            self.tuning.requested_level().to_string(),
            self.tuning
                .observed_level()
                .map(|l| l.to_string())
                .unwrap_or_else(|| "mixed".into()),
        )
    }

    /// Park CPU cores by cluster: `none`, `lpe`, or `p-only`.
    ///
    /// Measured justification, and it is placement rather than power: Horizon Zero Dawn
    /// reports CPU FPS 34 against GPU FPS 45 while no thread exceeds 50% — a hot thread
    /// on a slow core. Whether it also concentrates the power budget is M9 Phase 0
    /// question E and is not yet answered.
    async fn set_park_level(
        &self,
        level: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        // Validate before checking support, so a typo reports as a typo rather than as
        // an unsupported machine.
        let parsed = ParkLevel::parse(&level).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!(
                "unknown park level {level:?}; expected none, lpe or p-only"
            ))
        })?;
        if !self.tuning.parking_supported() {
            return Err(zbus::fdo::Error::NotSupported(
                "no hybrid CPU topology found, so there is nothing useful to park".into(),
            ));
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_TUNING).await?;

        self.tuning
            .set_level(parsed)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        eprintln!("park level set to {parsed} by {sender}");
        Ok(())
    }

    /// `(min, max, rpn, rp0)` for the render GT, all MHz. Zeros when there is no GT.
    #[zbus(property)]
    async fn gpu_freq_window(&self) -> (u32, u32, u32, u32) {
        let (min, max) = self.tuning.gpu_window().unwrap_or((0, 0));
        let (rpn, rp0) = self.tuning.gpu_range().unwrap_or((0, 0));
        (min, max, rpn, rp0)
    }

    /// `(requested, achieved)` GPU clock in MHz — `cur_freq` and `act_freq`.
    ///
    /// **These are different numbers and the difference matters.** `cur_freq` is the
    /// DVFS request and reads a constant 2500 on this board under load; `act_freq` is
    /// what happened and reads 1850-1950. Mission Center, nvtop and turbostat's
    /// `GFXMHz` all display the former, which is why four separate reports claimed this
    /// GPU reaches 2.5 GHz. `act_freq` also reads 0 in RC6, published here as 0.
    #[zbus(property)]
    async fn gpu_clocks(&self) -> (u32, u32) {
        let (req, act) = self.tuning.gpu_clocks();
        (req.unwrap_or(0), act.unwrap_or(0))
    }

    /// Cap the GPU's maximum frequency in MHz, or 0 to hand the full range back.
    ///
    /// **Unproven on this hardware.** Lowering `max_freq` is at least the right
    /// direction — *raising* `min_freq` to 2500 is already known to be honoured by the
    /// driver and ignored by the silicon — but whether a cap binds is M9 Phase 0
    /// question C. The control exists so that question can be answered through the app.
    async fn set_gpu_max_freq(
        &self,
        mhz: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        if let Some((rpn, rp0)) = self.tuning.gpu_range() {
            if mhz != 0 && (mhz < rpn || mhz > rp0) {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "{mhz} MHz is outside the GPU's {rpn}-{rp0} MHz range"
                )));
            }
        }
        if !self.tuning.gpu_supported() {
            return Err(zbus::fdo::Error::NotSupported(
                "no xe GT frequency controls found under /sys/class/drm".into(),
            ));
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_TUNING).await?;

        self.tuning
            .set_gpu_max((mhz != 0).then_some(mhz))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        match mhz {
            0 => eprintln!("gpu frequency cap cleared by {sender}"),
            v => eprintln!("gpu capped at {v} MHz by {sender}"),
        }
        Ok(())
    }

    /// Sustained CPU power limit in watts, or 0 when unsupported.
    #[zbus(property)]
    async fn power_limit(&self) -> u32 {
        PowerLimit::new(&self.fs).read().unwrap_or(0)
    }

    /// The highest power limit this machine admits to, in watts.
    ///
    /// Published so a client can bound a slider to something real. **Never derive this
    /// from the MSR RAPL zone**, which reports 200 W while its own maximum says 25.
    #[zbus(property)]
    async fn power_limit_max(&self) -> u32 {
        PowerLimit::new(&self.fs).max_watts()
    }

    /// Set the sustained CPU power limit.
    ///
    /// The most effective thermal control here: measured, 10 W is worth about 12 °C.
    /// Note the effect is not immediate — the averaging window is ~32 s, so a power
    /// reading taken sooner shows turbo rather than the new steady state.
    async fn set_power_limit(
        &self,
        watts: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        if let fw_helper_core::Cap::No(reason) = &self.caps.power_limit {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_POWER_LIMIT).await?;

        PowerLimit::new(&self.fs)
            .set(watts)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        if let Ok(mut state) = self.state.lock() {
            state.power_limit = Some(watts);
            state.save();
        }
        eprintln!("power limit set to {watts} W by {sender}");
        Ok(())
    }

    /// Current charge limit as the EC reports it, or 0 when unsupported.
    ///
    /// Read from Framework's custom EC command, not from
    /// `charge_control_end_threshold` - the sysfs attribute holds a number that does
    /// not govern charging (ADR 0012).
    #[zbus(property)]
    async fn charge_limit(&self) -> u8 {
        crate::ec::read_charge_limit(&*self.ec).unwrap_or(0)
    }

    /// Set the battery charge limit.
    ///
    /// The first method in this interface that writes to hardware. Two things are
    /// non-negotiable and set the pattern for every write that follows: polkit is
    /// checked before touching anything, and the value is read back afterwards so a
    /// silent override surfaces as an error rather than as success.
    ///
    /// The read-back only became meaningful with ADR 0012. Through ADR 0008 it read a
    /// sysfs attribute that agreed with every write and governed nothing, so it
    /// reported success for a feature that had never once worked.
    async fn set_charge_limit(
        &self,
        percent: u8,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender = Self::authorize(&header, conn, polkit::actions::SET_CHARGE_LIMIT).await?;

        crate::ec::set_charge_limit(&self.fs, &*self.ec, percent)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        // Lock only to record the result. Never held across an await.
        if let Ok(mut state) = self.state.lock() {
            state.charge_limit = Some(percent);
            state.save();
        }
        eprintln!("charge limit set to {percent}% by {sender}");
        Ok(())
    }

    /// How the fan is currently driven: `auto`, `manual`, or `unavailable`.
    #[zbus(property)]
    async fn fan_mode(&self) -> String {
        match self.fan.mode() {
            // "manual" and "curve" are both us driving, but they mean different things
            // to a user: one is a number they chose, the other is a rule that keeps
            // choosing. Reporting them the same forces every client to infer it.
            Some(fw_helper_core::FanMode::Manual) if self.fan.curve_active() => "curve".into(),
            Some(m) => m.to_string(),
            None => "unavailable".into(),
        }
    }

    /// Current duty 0-255 as the EC reports it, or 0 when unavailable. Under EC
    /// control this reads 0 regardless of how fast the fan is actually turning, so it
    /// is only meaningful alongside `FanMode` - read `Telemetry`'s fan rpm for speed.
    #[zbus(property)]
    async fn fan_duty(&self) -> u8 {
        self.fan.duty().unwrap_or(0)
    }

    /// The lowest duty permitted right now, given the temperature.
    ///
    /// Published so a client can show the user why a slider will not go lower, rather
    /// than appearing to ignore them. 0 means the EC would have the fan off, so
    /// silence is allowed.
    #[zbus(property)]
    async fn fan_floor(&self) -> u8 {
        match self.control_celsius() {
            // The battery has an independent say: it can be warm while the CPU is idle.
            Some(c) => self
                .fan
                .floor_duty(c)
                .max(self.thermal().battery_floor_public()),
            None => u8::MAX,
        }
    }

    /// Take manual fan control and hold `duty` (0-255).
    ///
    /// Returns the duty the EC actually settled on, which may differ by a count or
    /// two: the EC stores whole percent, so 180 comes back as 181.
    ///
    /// **This is not a fan curve.** It pins one duty rather than following
    /// temperature. What it is not, any more, is unbounded: the duty is clamped up to
    /// the firmware floor for the current temperature, and that floor is re-enforced
    /// every poll tick, so a duty chosen at idle cannot stay put while the machine
    /// heats up. A request below the floor is honoured as far as the floor allows and
    /// the difference is logged rather than silently swallowed.
    async fn set_fan_duty(
        &self,
        duty: u8,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<u8> {
        // Capability before authorization: there is no point prompting for a password
        // to do something this machine cannot do, and the reason is more useful than
        // an auth failure would be.
        if let fw_helper_core::Cap::No(reason) = &self.caps.fan_control {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        // Refuse if our own heartbeat is stale.
        //
        // This is not hypothetical: zbus serves this interface from its own executor,
        // not from the tokio runtime (measured 2026-08-21 — blocking every tokio
        // worker left D-Bus answering normally). So the poll loop can be dead while
        // this method still runs, and without this check the fan would be handed to a
        // daemon that is not minding it, taken back by the watchdog five seconds
        // later, and handed over again on the next call.
        let stale = self.watchdog.since_beat();
        if stale > crate::watchdog::TIMEOUT {
            return Err(zbus::fdo::Error::Failed(format!(
                "refusing manual fan control: this daemon's telemetry loop has not run \
                 for {:.0}s, so nothing would be watching the fan. Restart fw-helperd",
                stale.as_secs_f64()
            )));
        }

        let sender = Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;

        // The temperature the floor is computed against comes from the same telemetry
        // the daemon publishes, and the staleness check above is what makes it
        // trustworthy enough to base a safety decision on.
        let celsius = self.control_celsius();
        let applied = self
            .fan
            .set_duty(duty, self.thermal())
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        if applied.clamped() {
            eprintln!(
                "fan: {sender} asked for duty {}/255; raised to {} to stay at or above \
                 the firmware floor ({}/255 at {:.1} C)",
                applied.requested,
                applied.settled,
                applied.floor,
                celsius.unwrap_or(f64::NAN)
            );
        } else {
            eprintln!("fan set to duty {}/255 by {sender}", applied.settled);
        }
        Ok(applied.settled)
    }

    /// The active fan curve as (temperature, duty) pairs, empty when none is running.
    #[zbus(property)]
    async fn fan_curve(&self) -> Vec<(f64, u8)> {
        self.fan.curve_points().unwrap_or_default()
    }

    /// The learned firmware floor as (temperature, duty) pairs, ascending, empty until
    /// something has been observed.
    ///
    /// `FanFloor` answers "how low may I go *now*"; this answers it across the range,
    /// which is what an editor needs to show the shape of a curve against the shape of
    /// the thing that overrides it. A point drawn under this band is not ignored — it
    /// is clamped up, every tick — and a user who cannot see the band has no way to
    /// tell that from a bug.
    ///
    /// These are observations of the EC's **ascending** branch only (ADR 0011); the
    /// descending branch runs far higher and says nothing about what a temperature
    /// needs. The list grows as the machine is used, so it is not stable for the life
    /// of a proxy — see the property-caching note on the client trait.
    #[zbus(property)]
    async fn fan_floor_curve(&self) -> Vec<(f64, u8)> {
        self.fan
            .floor_snapshot()
            .map(|(_revision, points)| points)
            .unwrap_or_default()
    }

    /// Follow a temperature → duty curve.
    ///
    /// Points must ascend in temperature and must not fall in duty; a duty between 1
    /// and the stiction threshold is refused, because it describes a stopped fan while
    /// looking like a slow one. The curve is a *request*: the firmware floor and the
    /// battery guard are applied on top of it every tick, so a badly drawn curve is
    /// bounded exactly as a pinned duty is.
    async fn set_fan_curve(
        &self,
        points: Vec<(f64, u8)>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<u8> {
        if let fw_helper_core::Cap::No(reason) = &self.caps.fan_control {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        let stale = self.watchdog.since_beat();
        if stale > crate::watchdog::TIMEOUT {
            return Err(zbus::fdo::Error::Failed(format!(
                "refusing fan control: this daemon's telemetry loop has not run for \
                 {:.0}s, so nothing would be following the curve. Restart fw-helperd",
                stale.as_secs_f64()
            )));
        }
        // Validate before authorizing: a malformed curve is a malformed curve whether
        // or not the caller could have been allowed to set a good one.
        let curve = fw_helper_core::Curve::new(
            points
                .into_iter()
                .map(|(celsius, duty)| fw_helper_core::Point { celsius, duty })
                .collect(),
        )
        .map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;

        let sender = Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;
        let applied = self
            .fan
            .set_curve(curve, self.thermal())
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        eprintln!(
            "fan: following a curve, requested by {sender}; first tick duty {}/255",
            applied.settled
        );
        Ok(applied.settled)
    }

    /// Hand the fan back to the EC.
    ///
    /// Deliberately requires the same authorization as taking control. It is the safe
    /// direction, but it is still a change to how the machine behaves, and a caller
    /// permitted to undo another user's setting without authenticating would be a
    /// surprise.
    async fn set_fan_auto(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        if let fw_helper_core::Cap::No(reason) = &self.caps.fan_control {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;

        self.fan
            .release()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        eprintln!("fan returned to EC control by {sender}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    const ATTR: &str = "sys/class/power_supply/BAT1/charge_control_end_threshold";

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Same rooted-sysfs trick as the core fixtures (ADR 0004): no hardware, no root.
    fn fixture(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helperd-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn daemon_with_ec(
        root: &Path,
        persisted: Option<u8>,
        ec: Arc<crate::ec::fake::FakeEc>,
    ) -> Daemon {
        let fs_ = Sysfs::new(root);
        let caps = Capabilities::probe(&fs_);
        let state = State {
            charge_limit: persisted,
            power_limit: None,
            profile: None,
            profile_on_ac: None,
            profile_on_battery: None,
            floor: Vec::new(),
        };
        let lease = Arc::new(crate::fan::FanLease::new(fs_.clone()));
        let wd = crate::watchdog::Watchdog::new(Arc::clone(&lease));
        wd.beat();
        let axis = Arc::new(crate::ppd::ProfileAxis::disconnected(fs_.clone()));
        Daemon::new(
            fs_.clone(),
            caps,
            state,
            Shared {
                fan: lease,
                watchdog: wd,
                axis,
                applied_ppd: Arc::new(std::sync::atomic::AtomicU8::new(0)),
                profiles: Arc::new(Mutex::new(fw_helper_core::Profile::built_ins())),
                ec,
                recording: Arc::new(crate::record::Recording::new()),
                watchers: Arc::new(crate::record::Watchers::default()),
                tuning: Arc::new(TuningLease::new(fs_.clone())),
            },
        )
    }

    fn write_attr(root: &Path, value: &str) {
        let p = root.join(ATTR);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, value).unwrap();
    }

    fn read_attr(root: &Path) -> String {
        fs::read_to_string(root.join(ATTR)).unwrap().trim().into()
    }

    // These read and write the EC, not sysfs. Under ADR 0008 they drove
    // `charge_control_end_threshold` and passed for years against a value that
    // governed nothing — which is precisely why the fixture now stands in for the
    // mechanism rather than for the attribute.

    #[test]
    fn reapply_reports_when_firmware_left_the_limit_alone() {
        let root = fixture("already");
        let ec = Arc::new(crate::ec::fake::FakeEc::new(0, 80));
        let d = daemon_with_ec(&root, Some(80), Arc::clone(&ec));

        assert_eq!(d.reapply_charge_limit(), Reapply::AlreadyCorrect);
        assert_eq!(ec.max(), 80);
    }

    #[test]
    fn reapply_corrects_when_firmware_reset_the_limit() {
        let root = fixture("reset");
        // What a reboot leaves behind: the EC back at its 100% default.
        write_attr(&root, "100\n");
        let ec = Arc::new(crate::ec::fake::FakeEc::new(0, 100));
        let d = daemon_with_ec(&root, Some(80), Arc::clone(&ec));

        assert_eq!(d.reapply_charge_limit(), Reapply::Corrected);
        assert_eq!(ec.max(), 80);
        // ...and the inert sysfs attribute is dragged along so UPower and GNOME do not
        // show a third, stale number. It follows the EC; it never leads.
        assert_eq!(read_attr(&root), "80");
    }

    #[test]
    fn reapply_does_nothing_without_a_persisted_limit() {
        let root = fixture("none");
        let ec = Arc::new(crate::ec::fake::FakeEc::new(0, 100));
        let d = daemon_with_ec(&root, None, Arc::clone(&ec));

        assert_eq!(d.reapply_charge_limit(), Reapply::NothingPersisted);
        // Untouched — an absent limit is not a request to set 100%.
        assert_eq!(ec.max(), 100);
    }

    #[test]
    fn reapply_fails_loudly_when_the_ec_will_not_take_the_command() {
        // A board that does not implement Framework's custom command at all.
        let root = fixture("unsupported");
        fs::create_dir_all(&root).unwrap();
        let d = daemon_with_ec(&root, Some(80), Arc::new(crate::ec::fake::FakeEc::dead()));

        assert_eq!(d.reapply_charge_limit(), Reapply::Failed);
    }
}
