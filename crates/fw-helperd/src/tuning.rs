//! Core parking and GPU frequency, held by the daemon — **ADR 0014**.
//!
//! Both levers are *fail-safe state* in the same sense as the fan (ADR 0006): if this
//! process dies holding them, the machine is left altered and nothing else will put it
//! back. Parking is the worse of the two, and worse than the fan, for one specific
//! reason:
//!
//! > A stuck fan is corrected by a reboot, or by the EC taking the fan back when
//! > `pwm1_enable` returns to 2. **Nothing re-onlines a CPU except something that knows
//! > it parked it.** An offline core survives a daemon restart, and presents to the user
//! > as a machine that has quietly lost twelve cores with no UI anywhere explaining why.
//!
//! So every route out of this process restores: clean exit, `SIGTERM`, `SIGINT`, panic,
//! and resume. As with [`crate::fan::FanLease`], the restore path **takes no locks** —
//! a panic can arrive while another thread holds one, and blocking in the panic hook
//! would leave the process dying with cores still parked.
//!
//! Unlike the fan there is no watchdog here, and that is deliberate. A parked core is
//! not a thermal hazard; it costs throughput and nothing else. The fan watchdog exists
//! because a stuck-low fan is silent and dangerous, which has no analogue in this file.

use fw_helper_core::tune::{CoreParking, CoreSet, GpuFreq, ParkLevel, TuneError};
use fw_helper_core::Sysfs;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Sentinel for `gpu_max`: no cap applied by us.
const NO_CAP: u32 = 0;

pub struct TuningLease {
    fs: Sysfs,
    /// Topology, resolved once at startup. CPU numbering does not change at run time,
    /// and re-probing inside the panic hook would allocate.
    set: CoreSet,
    /// Every CPU this daemon may ever have taken offline. Precomputed so the restore
    /// path is a fixed list of writes with no discovery and no allocation.
    parkable: Vec<u32>,
    level: AtomicU32,
    holding_cores: AtomicBool,
    /// The GT frequency window to put back, captured before the first cap.
    gpu_restore_max: AtomicU32,
    gpu_restore_min: AtomicU32,
    gpu_max: AtomicU32,
}

impl TuningLease {
    /// Probe the machine, restoring it first if a previous instance left it parked.
    ///
    /// Order matters: a daemon started on a parked machine - exactly what happens after
    /// `SIGKILL` plus `Restart=` - would otherwise cache a topology in which the parked
    /// cluster does not exist, and carry that wrong shape for its whole life. Bringing
    /// the cores back first costs one pass over `cpu*/online`, which needs no
    /// classification at all.
    pub fn new(fs: Sysfs) -> Self {
        let mut set = CoreSet::probe(&fs);
        if !set.is_complete() {
            for cpu in set.cpus().iter().filter(|c| c.parkable) {
                let _ = fs.write_string(
                    &format!("sys/devices/system/cpu/cpu{}/online", cpu.num),
                    "1",
                );
            }
            set = CoreSet::probe(&fs);
        }
        let parkable = set
            .cpus()
            .iter()
            .filter(|c| c.parkable)
            .map(|c| c.num)
            .collect();
        Self {
            fs,
            set,
            parkable,
            level: AtomicU32::new(ParkLevel::None as u32),
            holding_cores: AtomicBool::new(false),
            gpu_restore_max: AtomicU32::new(NO_CAP),
            gpu_restore_min: AtomicU32::new(NO_CAP),
            gpu_max: AtomicU32::new(NO_CAP),
        }
    }

    /// Always built from the **cached** topology, never a fresh probe.
    ///
    /// An offline CPU loses `cpufreq/` and `topology/`, so re-probing while parked
    /// collapses LP-E into E - measured 2026-09-22, after which `to_park(Lpe)` returns
    /// nothing and the level currently in effect reports as parking no cores at all.
    /// The topology is fixed for the life of the machine, so reading it once is also
    /// the cheaper thing to do.
    fn parking(&self) -> CoreParking<'_> {
        CoreParking::with_set(&self.fs, self.set.clone())
    }

    fn gpu(&self) -> GpuFreq<'_> {
        GpuFreq::new(&self.fs)
    }

    pub fn core_set(&self) -> &CoreSet {
        &self.set
    }

    pub fn parking_supported(&self) -> bool {
        self.parking().is_supported()
    }

    pub fn gpu_supported(&self) -> bool {
        self.gpu().is_supported()
    }

    /// What the machine is actually in, read fresh. `None` means a state no level
    /// describes — someone offlined cores by hand, which is reported rather than
    /// corrected.
    pub fn observed_level(&self) -> Option<ParkLevel> {
        self.parking().read()
    }

    pub fn requested_level(&self) -> ParkLevel {
        match self.level.load(Ordering::SeqCst) {
            1 => ParkLevel::Lpe,
            2 => ParkLevel::PCoresOnly,
            _ => ParkLevel::None,
        }
    }

    pub fn set_level(&self, level: ParkLevel) -> Result<(), TuneError> {
        self.parking().apply(level)?;
        self.level.store(level as u32, Ordering::SeqCst);
        // Held only while something is actually parked, so a shutdown at level None
        // says nothing rather than claiming a restore it did not perform.
        self.holding_cores
            .store(level != ParkLevel::None, Ordering::SeqCst);
        Ok(())
    }

    pub fn gpu_window(&self) -> Option<(u32, u32)> {
        let gpu = self.gpu();
        Some((gpu.min()?, gpu.max()?))
    }

    pub fn gpu_range(&self) -> Option<(u32, u32)> {
        self.gpu().range()
    }

    /// `(requested, achieved)` — `cur_freq` and `act_freq`. See [`GpuFreq`] for why
    /// these are different numbers and why four separate tools report the first one.
    pub fn gpu_clocks(&self) -> (Option<u32>, Option<u32>) {
        let gpu = self.gpu();
        (gpu.requested(), gpu.achieved())
    }

    pub fn gpu_cap(&self) -> Option<u32> {
        match self.gpu_max.load(Ordering::SeqCst) {
            NO_CAP => None,
            v => Some(v),
        }
    }

    pub fn set_gpu_max(&self, mhz: Option<u32>) -> Result<(), TuneError> {
        let gpu = self.gpu();
        match mhz {
            Some(mhz) => {
                // Capture the window we are displacing, once, before the first cap —
                // not on every call, or a second cap would record the first as the
                // thing to restore.
                if self.gpu_restore_max.load(Ordering::SeqCst) == NO_CAP {
                    if let (Some(max), Some(min)) = (gpu.max(), gpu.min()) {
                        self.gpu_restore_max.store(max, Ordering::SeqCst);
                        self.gpu_restore_min.store(min, Ordering::SeqCst);
                    }
                }
                gpu.set_max(mhz)?;
                self.gpu_max.store(mhz, Ordering::SeqCst);
            }
            None => {
                gpu.reset()?;
                self.gpu_max.store(NO_CAP, Ordering::SeqCst);
                self.gpu_restore_max.store(NO_CAP, Ordering::SeqCst);
                self.gpu_restore_min.store(NO_CAP, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Whether the machine is altered - **read from hardware, not from memory**, so a
    /// fresh process cleaning up after a killed one reports honestly rather than
    /// announcing nothing while it quietly puts twelve cores back.
    pub fn holding(&self) -> bool {
        if self.holding_cores.load(Ordering::SeqCst) || self.gpu_cap().is_some() {
            return true;
        }
        // `None` is "mixed", a state no level describes - which is also something worth
        // announcing when we are about to change it.
        let parked = self.observed_level() != Some(ParkLevel::None);
        let capped = self
            .gpu_window()
            .zip(self.gpu_range())
            .is_some_and(|((_, max), (_, rp0))| max < rp0);
        parked || capped
    }

    /// Put everything back. **Lock-free and allocation-free on the hot path**, callable
    /// from a signal handler, a panic hook, or a normal shutdown.
    ///
    /// Tries every CPU rather than stopping at the first failure: a partial restore
    /// that gave up early would leave exactly the state this exists to prevent. Returns
    /// whether everything came back.
    pub fn restore_now(&self) -> bool {
        let mut ok = true;

        for n in &self.parkable {
            let path = format!("sys/devices/system/cpu/cpu{n}/online");
            // Write unconditionally rather than reading first: a read that fails would
            // otherwise be taken as "already online" and skip a core that is not.
            if self.fs.write_string(&path, "1").is_err() {
                // EBUSY on an already-online CPU is normal on some kernels; only treat
                // it as failure if it really is still offline.
                if self.fs.read_u64(&path).ok() == Some(0) {
                    ok = false;
                }
            }
        }
        self.holding_cores.store(false, Ordering::SeqCst);

        // **Never gated on `gpu_max`.** That flag lives in this process's memory, and
        // the case this whole file exists for is a *fresh* process cleaning up after a
        // killed one, where it necessarily reads "nothing capped". Measured 2026-09-22:
        // gating on it left the GT pinned at 1200 MHz through a SIGKILL and restart,
        // while the cores - restored by the ungated loop above - came back correctly.
        // The hardware's own `rp0_freq` is the authority for what "uncapped" means.
        let gpu = self.gpu();
        if let (Some((min, max)), Some((rpn, rp0))) = (self.gpu_window(), self.gpu_range()) {
            let saved_max = self.gpu_restore_max.load(Ordering::SeqCst);
            let saved_min = self.gpu_restore_min.load(Ordering::SeqCst);
            // Prefer the window we displaced; fall back to the hardware's own range,
            // which is all a fresh process has to work with.
            let (want_max, want_min) = if saved_max != NO_CAP {
                (saved_max, saved_min)
            } else {
                (rp0, gpu.rpe().unwrap_or(rpn))
            };
            if max != want_max || min != want_min {
                // Ceiling first: the reverse order transiently asks for min > max.
                if let Some(p) = gpu.path().map(|p| p.to_string()) {
                    let a = self
                        .fs
                        .write_string(&format!("{p}/max_freq"), &want_max.to_string());
                    let b = self
                        .fs
                        .write_string(&format!("{p}/min_freq"), &want_min.to_string());
                    ok &= a.is_ok() && b.is_ok();
                } else {
                    ok = false;
                }
            }
        }
        self.gpu_max.store(NO_CAP, Ordering::SeqCst);
        self.gpu_restore_max.store(NO_CAP, Ordering::SeqCst);
        self.gpu_restore_min.store(NO_CAP, Ordering::SeqCst);
        ok
    }
}
