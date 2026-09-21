//! What the machine is *doing*: CPU, memory and GPU load, plus the counters that say
//! when something was being held back.
//!
//! Everything here is a **counter delta**, and follows [`crate::EnergySampler`]'s
//! discipline exactly: the first sample only establishes a reference point and yields
//! nothing, a gap too long to trust is discarded, and a delta that cannot be believed
//! is reported as `None` rather than interpolated. A plausible wrong number in a
//! monitoring graph is worse than a gap — it is indistinguishable from a measurement.
//!
//! ## Why the GPU is read from `/proc`, of all places
//!
//! Three sources exist on this board and two of them are wrong:
//!
//! - `gt0/gtidle/idle_residency_ms` is **RC6 residency**, not busy time. "Not in RC6"
//!   includes powered-but-idle, so it reported 27-55% busy on a completely idle machine
//!   (measured 2026-09-04). It is the obvious source and it does not measure this.
//! - The `xe` **PMU** (`engine-active-ticks`) is correct, but reaching it needs
//!   `perf_event_open`, which lives in systemd's `@debug` syscall group. The unit sets
//!   `SystemCallFilter=@system-service`, which excludes it — so this would mean widening
//!   a sandbox the daemon deliberately narrowed, for a number.
//! - `/proc/<pid>/fdinfo/<fd>` publishes `drm-cycles-<engine>` and
//!   `drm-total-cycles-<engine>` per DRM client (the kernel's `drm-usage-stats`
//!   contract). Accurate, plain file reads, no sandbox change, no dependency — and it
//!   also says *which process* is using the GPU, which the other two cannot.
//!
//! `drm-total-cycles-*` is a **GT-wide free-running clock**, not a per-client lifetime:
//! measured across three processes of very different ages it read 4846074419573,
//! 4846078603831 and 4846097985026, differing only by the interval between the reads.
//! So it is the denominator, and the numerator is the summed per-client delta.
//!
//! The scan costs something — 7475 fdinfo files on a normal desktop — so it is gated by
//! [`UsageSampler::set_gpu_scan`] and only runs when someone is actually watching.

use crate::{EnergySampler, Sysfs};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Longest gap between samples that still yields a rate. Beyond this the machine was
/// most likely suspended, and a rate computed across a suspend is fiction.
const MAX_GAP: Duration = Duration::from_secs(60);

/// Package-level thermal throttling. Per-core counters exist alongside these; the
/// package one is what a user means by "did it throttle".
const CPU_THROTTLE: &str = "sys/devices/system/cpu/cpu0/thermal_throttle";

/// How often the full `/proc` sweep for new DRM clients runs.
///
/// The sweep opens every file descriptor on the machine — 7475 of them on an ordinary
/// desktop — and is syscall-bound rather than compute-bound, so it cannot be optimised
/// away; it can only be run less often. Between sweeps only the descriptors already
/// known to carry a DRM client are re-read, one per client.
///
/// Measured on the reference machine, release build, 2026-09-04:
///
/// | | cost per sample |
/// |---|---|
/// | naive: every descriptor, every tick | 145 ms |
/// | + sweep rationed to 10 s | 20 ms |
/// | + one descriptor per client, `comm` cached | **5-11 ms** |
///
/// So roughly 2% of one core while something is watching, and nothing at all when
/// [`UsageSampler::set_gpu_scan`] is off — which is the daemon's default.
///
/// The cost is that a client appearing mid-interval is not counted until the next
/// sweep. That is immaterial here: a newly-seen client contributes nothing on its first
/// sample anyway (see [`engine_busy`]), and anything worth graphing runs for minutes.
const FULL_SWEEP: Duration = Duration::from_secs(10);

/// The only DRM driver whose load reporting is verified on the reference machine.
///
/// `i915` publishes `drm-engine-<class>` in nanoseconds instead of cycles and would be
/// a near-identical parse, but no i915 machine has been measured here — so it reports
/// as unavailable with the driver named, rather than shipping an unverified code path.
const SUPPORTED_DRM_DRIVER: &str = "xe";

/// How many times [`UsageSampler::read_gpu_mhz`] retries before accepting that the GT
/// is parked. Each read is one small sysfs file, so the whole burst costs microseconds
/// and stops early the moment it sees a non-zero.
const GPU_FREQ_READS: u32 = 8;

/// Per-engine busy percentages, busiest first.
pub type EngineLoad = Vec<(String, f64)>;

/// A process and its share of the GPU, as `(comm, percent)`.
pub type GpuClient = (String, f64);

/// One sample of machine load. Every field is `Option` or empty when it could not be
/// read, and absence is never rendered as zero — see the module note.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Aggregate across all cores, 0-100.
    pub cpu_percent: Option<f64>,
    /// Mean of every core's `scaling_cur_freq`, idle cores included.
    ///
    /// Useful as a package-wide figure next to a package-wide wattage, but it is not
    /// the speed anything ran at — see [`Self::cpu_mhz_busy`].
    pub cpu_mhz: Option<u64>,
    /// The clock the cores that were **executing** actually ran at, weighted by how
    /// busy each was over the interval. turbostat calls this `Bzy_MHz`.
    ///
    /// This is the CPU's answer to the GPU's `act_freq`, and the distinction is the
    /// same one: [`Self::cpu_mhz`] averages in fifteen parked cores and reports 1.8 GHz
    /// for a machine whose working core is at 4.5, exactly as `cur_freq` reports 2500
    /// for a GPU running at 1950. `None` when nothing executed — a figure for "the
    /// speed work ran at" has no meaning when no work ran, and zero would be a lie.
    pub cpu_mhz_busy: Option<u64>,
    /// The RAPL `core` rail: what the CPU cores alone are drawing.
    ///
    /// Not the package — see [`crate::Telemetry::package_watts`] for that. Package
    /// covers cores, iGPU and uncore together, so on a GPU-heavy workload the two
    /// diverge sharply, and it is the pair that says where the watts went.
    pub cpu_watts: Option<f64>,
    pub mem_used_kb: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub swap_used_kb: Option<u64>,
    /// The **busiest** engine, not the sum: engines run in parallel, so summing them
    /// yields figures above 100% for a client doing render and copy at once.
    pub gpu_percent: Option<f64>,
    /// Per-engine busy, busiest first. `rcs` is render, `ccs` compute, `bcs` copy,
    /// `vcs`/`vecs` media.
    pub gpu_engines: EngineLoad,
    pub gpu_mhz: Option<u64>,
    /// What the driver *asked* for (`cur_freq`), as against [`Self::gpu_mhz`], which is
    /// what happened (`act_freq`).
    ///
    /// Carried so the gap can be shown rather than asserted. It reads a constant 2500 on
    /// the reference board while the GPU runs at 1950, and it is the number Mission
    /// Center, nvtop and turbostat's `GFXMHz` all display as "the clock".
    pub gpu_mhz_requested: Option<u64>,
    /// The RAPL `uncore` rail: the iGPU's own draw.
    ///
    /// The iGPU sits inside the CPU package, so this is a *subset* of
    /// [`crate::Telemetry::package_watts`], never an addition to it. `core + uncore`
    /// does not equal package either — the package figure also carries fabric, memory
    /// controller and other uncore blocks this zone excludes.
    pub gpu_watts: Option<f64>,
    /// Throttle reasons the GPU is asserting right now, e.g. `pl1`, `thermal`.
    ///
    /// The CPU has no equivalent readable flag, which is why a power-limit graph draws
    /// the PL1 setpoint over the trace instead: draw pinned to the line *is* PL1
    /// binding, and that inference is the only one available for the package.
    pub gpu_throttle: Vec<String>,
    /// Package thermal-throttle events **during this interval**, not since boot.
    pub cpu_throttle_events: u64,
    pub cpu_throttle_ms: u64,
    /// Whichever process used the GPU most this interval, as `(comm, percent)`.
    pub top_gpu_client: Option<GpuClient>,
}

impl Usage {
    pub fn mem_percent(&self) -> Option<f64> {
        let (used, total) = (self.mem_used_kb?, self.mem_total_kb?);
        (total > 0).then(|| used as f64 * 100.0 / total as f64)
    }

    /// True when anything was actively being held back this interval.
    pub fn throttled(&self) -> bool {
        self.cpu_throttle_events > 0 || !self.gpu_throttle.is_empty()
    }
}

/// Jiffies from `/proc/stat`'s aggregate line. Two counters, so `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

/// A DRM device we can read load from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DrmDevice {
    /// PCI address exactly as fdinfo spells it, e.g. `0000:00:02.0`. Matching on this
    /// keeps a second GPU's clients out of the sum.
    pdev: String,
    /// The GT whose frequency and throttle reasons are published, e.g.
    /// `sys/class/drm/card1/device/tile0/gt0`. gt0 is render/compute, gt1 media.
    gt: String,
}

/// A descriptor already known to carry a DRM client, remembered between samples.
///
/// One entry per **client**, not per descriptor: a process may hold the same client
/// through several dup'd fds, each publishing the identical counters, so re-reading all
/// of them every tick is pure waste. `comm` is captured at sweep time because it does
/// not change and reading it costs a second file per client per tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KnownClient {
    id: u64,
    comm: String,
    path: String,
}

/// Cycle counters for one DRM client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ClientCounters {
    comm: String,
    /// engine name -> cycles this client has spent on it
    engines: HashMap<String, u64>,
}

/// One sweep of every DRM client on one device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GpuCounters {
    /// `drm-client-id` -> counters. Keyed by client id, not by fd: a process may hold
    /// the same client through several dup'd descriptors and each one publishes the
    /// full set, so counting per fd would multiply a client's usage by its fd count.
    per_client: HashMap<u64, ClientCounters>,
    /// engine -> the GT-wide free-running clock, the denominator.
    total: HashMap<String, u64>,
}

/// Resolve one RAPL rail by name and pair it with a sampler sized to its counter.
///
/// Both halves must come from the same zone: `max_energy_range_uj` differs between
/// zones, and a sampler given the wrong width mis-corrects a wrap into a large wrong
/// number rather than failing.
fn rail(fs: &Sysfs, name: &str) -> Option<(String, EnergySampler)> {
    let zone = fs.find_powercap(name)?;
    let range = fs.read_u64(&format!("{zone}/max_energy_range_uj")).ok()?;
    Some((zone, EnergySampler::new(range)))
}

/// Polls `/proc` and the DRM node for machine load.
///
/// Holds the previous counters, so it must be the same instance tick after tick.
#[derive(Debug)]
pub struct UsageSampler {
    fs: Sysfs,
    drm: Option<DrmDevice>,
    gpu_scan: bool,
    last: Option<Instant>,
    cpu: Option<CpuTimes>,
    /// Per-core counters, indexed by the number in `cpuN`. Kept alongside the aggregate
    /// because a busy-weighted clock needs to know which cores did the work.
    cpu_cores: Option<Vec<CpuTimes>>,
    gpu: Option<GpuCounters>,
    /// Descriptors last seen carrying a client of our device, one per client.
    gpu_clients: Vec<KnownClient>,
    gpu_swept: Option<Instant>,
    throttle: Option<(u64, u64)>,
    /// `(zone path, sampler)` for the `core` and `uncore` RAPL rails. Resolved once by
    /// name at construction; `None` when the zone is absent on this machine.
    cpu_rail: Option<(String, EnergySampler)>,
    gpu_rail: Option<(String, EnergySampler)>,
}

impl UsageSampler {
    pub fn new(fs: Sysfs) -> Self {
        let drm = find_drm(&fs);
        let fs2 = fs.clone();
        Self {
            fs,
            drm,
            // Off by default. The scan is the only part of this with a real cost, and a
            // daemon with nobody watching should not pay it every second.
            gpu_scan: false,
            last: None,
            cpu: None,
            cpu_cores: None,
            gpu: None,
            gpu_clients: Vec::new(),
            gpu_swept: None,
            throttle: None,
            cpu_rail: rail(&fs2, "core"),
            gpu_rail: rail(&fs2, "uncore"),
        }
    }

    /// Whether the GPU client scan runs. Turning it off drops the reference point, so
    /// the first sample after turning it back on yields no percentage rather than one
    /// covering the whole gap.
    pub fn set_gpu_scan(&mut self, on: bool) {
        if self.gpu_scan != on {
            self.gpu_scan = on;
            self.gpu = None;
            // Force a sweep on the next sample: the descriptor list is certainly stale
            // by the time anyone starts watching again.
            self.gpu_swept = None;
            self.gpu_clients.clear();
        }
    }

    pub fn gpu_scan_enabled(&self) -> bool {
        self.gpu_scan
    }

    /// Whether this machine has a DRM device whose load we can actually read.
    pub fn gpu_available(&self) -> bool {
        self.drm.is_some()
    }

    /// Drop every reference point. Call on resume from suspend, exactly as
    /// [`crate::EnergySampler::invalidate`] is called: `/proc/stat` keeps counting
    /// across s2idle while the GT clock does not, so a delta spanning a suspend is
    /// meaningless in both directions.
    pub fn invalidate(&mut self) {
        self.last = None;
        self.cpu = None;
        self.cpu_cores = None;
        self.gpu = None;
        self.gpu_swept = None;
        self.gpu_clients.clear();
        self.throttle = None;
        for rail in [self.cpu_rail.as_mut(), self.gpu_rail.as_mut()]
            .into_iter()
            .flatten()
        {
            rail.1.invalidate();
        }
    }

    pub fn sample(&mut self, now: Instant) -> Usage {
        let elapsed = self
            .last
            .replace(now)
            .map(|prev| now.saturating_duration_since(prev));
        // A first sample, a zero interval, or a gap long enough to be a suspend. The
        // counters are still refreshed below so the *next* sample has a reference.
        let usable = matches!(elapsed, Some(dt) if !dt.is_zero() && dt <= MAX_GAP);

        let mut u = Usage::default();
        self.sample_cpu(&mut u, usable);
        self.sample_memory(&mut u);
        self.sample_throttle(&mut u, usable);
        self.sample_gpu(&mut u, usable, now);
        self.sample_rails(&mut u, now);
        u
    }

    /// Per-rail power, from the `core` and `uncore` RAPL zones.
    ///
    /// Deliberately **not** gated on `usable`: [`EnergySampler`] keeps its own
    /// reference point and applies its own gap rule, so handing it every reading lets
    /// it recover on the sample after a stall rather than the one after that. It is
    /// also the component that knows about counter wrap, which the `usable` flag does
    /// not model.
    ///
    /// These zones are MSR-backed `intel-rapl`, not the `intel-rapl-mmio` zone used for
    /// the package figure. That split is deliberate: mmio exposes only package and
    /// dram, so it cannot answer "how much of this is the GPU", while the *limit*
    /// fields under `intel-rapl` are the ones known to be meaningless here (see the
    /// 200 W `long_term` trap). Energy counters and limit fields are different claims
    /// from the same tree, and only the latter is untrustworthy.
    fn sample_rails(&mut self, u: &mut Usage, now: Instant) {
        u.cpu_watts = Self::sample_rail(self.fs.clone(), self.cpu_rail.as_mut(), now);
        u.gpu_watts = Self::sample_rail(self.fs.clone(), self.gpu_rail.as_mut(), now);
    }

    fn sample_rail(
        fs: Sysfs,
        rail: Option<&mut (String, EnergySampler)>,
        now: Instant,
    ) -> Option<f64> {
        let (zone, sampler) = rail?;
        let uj = fs.read_u64(&format!("{zone}/energy_uj")).ok()?;
        sampler.sample(uj, now).map(EnergySampler::quantize)
    }

    fn sample_cpu(&mut self, u: &mut Usage, usable: bool) {
        // Per-core busy first: the weights the clock is averaged with come from the
        // same interval as the clock itself.
        let cores_now = read_cpu_times_per_core(&self.fs);
        let cores_prev = match cores_now.clone() {
            Some(now) => self.cpu_cores.replace(now),
            None => self.cpu_cores.take(),
        };
        let weights = match (usable, &cores_now, &cores_prev) {
            (true, Some(now), Some(prev)) if now.len() == prev.len() => Some(
                now.iter()
                    .zip(prev)
                    .map(|(n, p)| n.busy.saturating_sub(p.busy) as f64)
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let (mean, busy) = self.read_cpu_clocks(weights.as_deref());
        u.cpu_mhz = mean;
        u.cpu_mhz_busy = busy;

        let Some(now) = read_cpu_times(&self.fs) else {
            self.cpu = None;
            return;
        };
        let previous = self.cpu.replace(now);
        if !usable {
            return;
        }
        let Some(prev) = previous else { return };
        let total = now.total.saturating_sub(prev.total);
        let busy = now.busy.saturating_sub(prev.busy);
        if total > 0 {
            u.cpu_percent = Some((busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0));
        }
    }

    /// Both CPU clocks: the flat mean across cores, and the busy-weighted one.
    ///
    /// `weights[i]` is the jiffies core `i` spent executing this interval. With no
    /// weights — first sample, or a gap long enough to be a suspend — only the mean is
    /// returned, because a weighted average of an unknown interval is not a measurement.
    ///
    /// Under `intel_pstate` in active mode `scaling_cur_freq` is derived from APERF and
    /// MPERF, so it is already an achieved figure per core rather than a requested one.
    /// What it is not is *aggregated* honestly: averaging a boosting core with fifteen
    /// parked ones answers a question nobody asked.
    fn read_cpu_clocks(&self, weights: Option<&[f64]>) -> (Option<u64>, Option<u64>) {
        let dir = self.fs.path("sys/devices/system/cpu");
        let mut sum = 0u64;
        let mut n = 0u64;
        let mut wsum = 0.0f64;
        let mut wtotal = 0.0f64;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return (None, None);
        };
        for entry in entries.flatten() {
            let Some(base) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if !base.starts_with("cpu") || !base[3..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let rel = format!("sys/devices/system/cpu/{base}/cpufreq/scaling_cur_freq");
            if let Ok(khz) = self.fs.read_u64(&rel) {
                sum += khz;
                n += 1;
                if let Some(w) =
                    weights.and_then(|w| base[3..].parse::<usize>().ok().and_then(|i| w.get(i)))
                {
                    wsum += khz as f64 * w;
                    wtotal += w;
                }
            }
        }
        let mean = (n > 0).then(|| sum / n / 1000);
        // wtotal of zero means every core was idle all interval. There is no "speed the
        // work ran at" in that case, and reporting the mean here would quietly relabel
        // the diluted figure as the honest one.
        let busy = (wtotal > 0.0).then(|| (wsum / wtotal / 1000.0).round() as u64);
        (mean, busy)
    }

    fn sample_memory(&mut self, u: &mut Usage) {
        let Ok(text) = self.fs.read_string("proc/meminfo") else {
            return;
        };
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
        };
        let total = field("MemTotal");
        // MemAvailable, not MemFree: free excludes the page cache, which the kernel
        // will hand back on demand, so it reads as though a healthy machine is full.
        let available = field("MemAvailable");
        u.mem_total_kb = total;
        u.mem_used_kb = match (total, available) {
            (Some(t), Some(a)) => Some(t.saturating_sub(a)),
            _ => None,
        };
        u.swap_used_kb = match (field("SwapTotal"), field("SwapFree")) {
            (Some(t), Some(f)) => Some(t.saturating_sub(f)),
            _ => None,
        };
    }

    fn sample_throttle(&mut self, u: &mut Usage, usable: bool) {
        let count = self
            .fs
            .read_u64(&format!("{CPU_THROTTLE}/package_throttle_count"));
        let ms = self
            .fs
            .read_u64(&format!("{CPU_THROTTLE}/package_throttle_total_time_ms"));
        let (Ok(count), Ok(ms)) = (count, ms) else {
            self.throttle = None;
            return;
        };
        let previous = self.throttle.replace((count, ms));
        if !usable {
            return;
        }
        let Some((prev_count, prev_ms)) = previous else {
            return;
        };
        // Deltas, so this reads as "throttled during this interval" rather than a
        // since-boot total that never comes back down once it has moved.
        u.cpu_throttle_events = count.saturating_sub(prev_count);
        u.cpu_throttle_ms = ms.saturating_sub(prev_ms);
    }

    /// The GPU's **achieved** clock, as the highest of a short burst of reads.
    ///
    /// Three things make the naive single read wrong, and all of them were measured
    /// on the reference machine rather than guessed:
    ///
    /// 1. `act_freq` reads **0** whenever the GT is in RC6 at that instant. Under a
    ///    genuinely bursty load it is zero most of the time — sampled at 50 Hz under
    ///    `stress-ng --gpu`, 70 of 400 reads were zero while the GPU was plainly busy.
    ///    A once-per-second read therefore drops the clock from the UI at random.
    /// 2. So a burst is taken and the **maximum** kept. A mean over reads that include
    ///    RC6 zeros would report a frequency the GPU never ran at; the maximum is a
    ///    clock it actually reached.
    /// 3. `cur_freq` is **not** an acceptable fallback, however tempting. It is the
    ///    DVFS *request* and reads a constant 2500 on this board while `act_freq` sits
    ///    at 1400 — four separate people reported "my GPU runs at 2.5 GHz" from tools
    ///    showing that node. Never publish the request as the achieved clock.
    ///
    /// Returns `Some(0)` when every read was zero — the GT is parked, which is a fact
    /// worth displaying — and `None` only when the node could not be read at all.
    fn read_gpu_mhz(&self, gt: &str) -> Option<u64> {
        let path = format!("{gt}/freq0/act_freq");
        let mut best: Option<u64> = None;
        for _ in 0..GPU_FREQ_READS {
            let Ok(mhz) = self.fs.read_u64(&path) else {
                break;
            };
            best = Some(best.map_or(mhz, |b: u64| b.max(mhz)));
            if mhz > 0 {
                break; // caught it awake; no reason to keep looking
            }
        }
        best
    }

    fn sample_gpu(&mut self, u: &mut Usage, usable: bool, now: Instant) {
        let Some(dev) = self.drm.clone() else { return };

        // Frequency and throttle reasons are instantaneous reads, not deltas, and cost
        // two file reads — worth having even when the client scan is switched off.
        u.gpu_mhz = self.read_gpu_mhz(&dev.gt);
        u.gpu_mhz_requested = self.fs.read_u64(&format!("{}/freq0/cur_freq", dev.gt)).ok();
        u.gpu_throttle = read_throttle_reasons(&self.fs, &dev.gt);

        if !self.gpu_scan {
            return;
        }
        let Some(counters) = self.scan_gpu(&dev.pdev, now) else {
            self.gpu = None;
            return;
        };
        let previous = self.gpu.replace(counters.clone());
        if !usable {
            return;
        }
        let Some(prev) = previous else { return };
        let (engines, top) = engine_busy(&prev, &counters);
        u.gpu_percent = engines.first().map(|(_, pct)| *pct);
        u.gpu_engines = engines;
        u.top_gpu_client = top;
    }

    /// Cycle counters for every DRM client of `pdev`.
    ///
    /// Two tiers, because the naive version does not fit in a 1 Hz budget. A full
    /// `/proc` sweep runs at most every [`FULL_SWEEP`] to discover *which* descriptors
    /// carry a client; every other sample re-reads only those. On the reference machine
    /// that is 7475 files occasionally against a few dozen the rest of the time.
    fn scan_gpu(&mut self, pdev: &str, now: Instant) -> Option<GpuCounters> {
        let due = self
            .gpu_swept
            .is_none_or(|last| now.saturating_duration_since(last) >= FULL_SWEEP);
        if due {
            self.gpu_clients = sweep_drm_clients(&self.fs, pdev);
            self.gpu_swept = Some(now);
        }

        let mut out = GpuCounters::default();
        let known = std::mem::take(&mut self.gpu_clients);
        // A descriptor that no longer parses has gone — the process exited, or closed
        // the device. Dropping it keeps the fast path proportional to live clients
        // rather than to every client seen since the daemon started.
        self.gpu_clients = known
            .into_iter()
            .filter(|client| {
                self.fs
                    .read_string(&client.path)
                    .ok()
                    .and_then(|text| parse_client(&text, pdev, &mut out, || client.comm.clone()))
                    .is_some()
            })
            .collect();

        (!out.per_client.is_empty()).then_some(out)
    }
}

/// Walk every process's descriptors and return one entry per DRM client of `pdev`.
///
/// This is the expensive half. See [`FULL_SWEEP`] for why it is rationed, and
/// [`KnownClient`] for why the result is deduplicated by client id rather than listing
/// every descriptor: on the reference machine 106 descriptors resolve to far fewer
/// clients, and re-reading the duplicates was most of the steady-state cost.
fn sweep_drm_clients(fs: &Sysfs, pdev: &str) -> Vec<KnownClient> {
    let mut out: Vec<KnownClient> = Vec::new();
    let Ok(procs) = std::fs::read_dir(fs.path("proc")) else {
        return out;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dir = format!("proc/{pid}/fdinfo");
        let Ok(fds) = std::fs::read_dir(fs.path(&dir)) else {
            // Ordinary: the process exited between the two readdirs, or it belongs
            // to another user we have no ptrace access to. Neither is worth reporting
            // per process - `fdinfo_blocked` says it once, as a capability, when the
            // second is going to be true of every process on the machine.
            continue;
        };
        let mut comm: Option<String> = None;
        for fd in fds.flatten() {
            let Some(name) = fd.file_name().to_str().map(String::from) else {
                continue;
            };
            let path = format!("{dir}/{name}");
            let Ok(text) = fs.read_string(&path) else {
                continue;
            };
            // Cheap rejection first: almost every descriptor is a socket or a regular
            // file whose fdinfo is four lines long.
            if !text.contains("drm-client-id") || !claims_device(&text, pdev) {
                continue;
            }
            let Some(id) = field(&text, "drm-client-id").and_then(|v| v.parse().ok()) else {
                continue;
            };
            if out.iter().any(|c| c.id == id) {
                continue; // another descriptor onto a client we already have
            }
            let comm = comm
                .get_or_insert_with(|| {
                    fs.read_string(&format!("proc/{pid}/comm"))
                        .unwrap_or_else(|_| pid.clone())
                })
                .clone();
            out.push(KnownClient { id, comm, path });
        }
    }
    out
}

/// The value of one `key: value` line, if present.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Whether this fdinfo belongs to the device we are measuring, rather than a second GPU.
fn claims_device(text: &str, pdev: &str) -> bool {
    text.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(k, v)| k.trim() == "drm-pdev" && v.trim() == pdev)
    })
}

/// The aggregate `cpu` line of `/proc/stat`, as (busy, total) jiffies.
/// `/proc/stat`'s per-core lines, indexed by the number in `cpuN`.
///
/// Returns `None` rather than a short vector if the lines are not contiguous from
/// `cpu0`: the index is used to look up a core's frequency, so a gap would silently
/// weight the wrong core.
fn read_cpu_times_per_core(fs: &Sysfs) -> Option<Vec<CpuTimes>> {
    let text = fs.read_string("proc/stat").ok()?;
    let mut out: Vec<CpuTimes> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(idx) = name.strip_prefix("cpu").filter(|d| !d.is_empty()) else {
            continue;
        };
        if idx.parse::<usize>().ok()? != out.len() {
            return None;
        }
        let values: Vec<u64> = fields.filter_map(|v| v.parse().ok()).collect();
        if values.len() < 5 {
            return None;
        }
        let total: u64 = values.iter().sum();
        out.push(CpuTimes {
            busy: total.checked_sub(values[3] + values[4])?,
            total,
        });
    }
    (!out.is_empty()).then_some(out)
}

fn read_cpu_times(fs: &Sysfs) -> Option<CpuTimes> {
    let text = fs.read_string("proc/stat").ok()?;
    let line = text.lines().next()?;
    let mut fields = line.split_whitespace();
    // The aggregate line is `cpu`; `cpu0`, `cpu1`... are the per-core ones below it.
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields.filter_map(|v| v.parse().ok()).collect();
    // user nice system idle iowait [irq softirq steal guest guest_nice]
    if values.len() < 5 {
        return None;
    }
    let total: u64 = values.iter().sum();
    // idle + iowait are the two that are not work. iowait is famously unreliable as a
    // measure of anything, but it is unambiguously *not* the CPU executing.
    let idle = values[3] + values[4];
    Some(CpuTimes {
        busy: total.checked_sub(idle)?,
        total,
    })
}

/// Every `reason_*` under the GT's throttle directory that currently reads 1.
///
/// Enumerated rather than hardcoded: the reference kernel exposes eight
/// (`pl1`, `pl2`, `pl4`, `prochot`, `ratl`, `thermal`, `vr_tdc`, `vr_thermalert`) and a
/// newer one adding a ninth should show up without a code change.
fn read_throttle_reasons(fs: &Sysfs, gt: &str) -> Vec<String> {
    let rel = format!("{gt}/freq0/throttle");
    let Ok(entries) = std::fs::read_dir(fs.path(&rel)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter_map(|name| {
            let reason = name.strip_prefix("reason_")?.to_string();
            (fs.read_string(&format!("{rel}/{name}")).ok()? == "1").then_some(reason)
        })
        .collect();
    out.sort();
    out
}

/// Locate a DRM device whose load can be read.
///
/// Resolved by **driver**, never by index: `card1` on the reference machine, but DRM
/// minor numbers are assigned in probe order and are no more stable than hwmon's, which
/// is the same reasoning behind [`Sysfs::find_hwmon`].
fn find_drm(fs: &Sysfs) -> Option<DrmDevice> {
    for base in drm_cards(fs)? {
        let card = format!("sys/class/drm/{base}");
        let Ok(uevent) = fs.read_string(&format!("{card}/device/uevent")) else {
            continue;
        };
        if uevent_value(&uevent, "DRIVER") != Some(SUPPORTED_DRM_DRIVER) {
            continue;
        }
        let Some(pdev) = uevent_value(&uevent, "PCI_SLOT_NAME") else {
            continue;
        };
        // gt0 is render/compute and gt1 is media on this part. The render GT is the one
        // whose frequency and power throttling anyone means by "the GPU".
        let gt = format!("{card}/device/tile0/gt0");
        if !fs.exists(&format!("{gt}/freq0")) {
            continue;
        }
        return Some(DrmDevice {
            pdev: pdev.to_string(),
            gt,
        });
    }
    None
}

/// The DRM driver backing this machine's render node, so a capability can name it.
pub fn drm_driver(fs: &Sysfs) -> Option<String> {
    for base in drm_cards(fs)? {
        let rel = format!("sys/class/drm/{base}/device/uevent");
        let Ok(uevent) = fs.read_string(&rel) else {
            continue;
        };
        if let Some(driver) = uevent_value(&uevent, "DRIVER") {
            return Some(driver.to_string());
        }
    }
    None
}

/// `CAP_SYS_PTRACE`, which is what gates reading another process's `/proc/<pid>/fdinfo`.
const CAP_SYS_PTRACE: u64 = 1 << 19;

/// Why per-client GPU load cannot be read here, or `None` when it can.
///
/// Opening `/proc/<pid>/fdinfo/<fd>` of a process we do not own goes through
/// `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`, which for a different uid needs
/// `CAP_SYS_PTRACE`. Being root is **not** enough: the kernel grants root nothing except
/// through capabilities, and `fw-helperd.service` ran with `CapabilityBoundingSet=`
/// empty, so the daemon was uid 0 with a capability set of exactly zero. Measured
/// 2026-09-20: `gpu_mhz` arrived every tick, so the DRM device was found - and
/// `gpu_percent` was never once published, because every GPU client on the machine
/// belongs to the desktop user and not one of their fdinfo files could be opened.
///
/// Unprivileged is not blocked: a client running as the user reads its own processes,
/// which is every process driving the GPU. That is why this only appeared once the
/// daemon was packaged - session-bus development mode runs as the user.
pub fn fdinfo_blocked(fs: &Sysfs) -> Option<String> {
    let status = fs.read_string("proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
    };
    // The effective uid is the second field of `Uid: real effective saved fs`.
    let euid: u64 = field("Uid")?.split_whitespace().nth(1)?.parse().ok()?;
    if euid != 0 {
        // Running as a user: we see our own processes, which is all of the ones that
        // drive the GPU on a desktop.
        return None;
    }
    let effective = u64::from_str_radix(field("CapEff")?, 16).ok()?;
    (effective & CAP_SYS_PTRACE == 0).then(|| {
        "GPU load needs CAP_SYS_PTRACE to read other processes' /proc/<pid>/fdinfo; \
         add AmbientCapabilities=CAP_SYS_PTRACE to fw-helperd.service"
            .to_string()
    })
}

/// Card directories under `/sys/class/drm`, sorted so resolution is deterministic on a
/// machine with more than one GPU.
fn drm_cards(fs: &Sysfs) -> Option<Vec<String>> {
    let mut cards: Vec<String> = std::fs::read_dir(fs.path("sys/class/drm"))
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        // `card1-eDP-1` and friends are connectors hanging off the same device.
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    cards.sort();
    Some(cards)
}

fn uevent_value<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(str::trim)
}

/// Fold one fdinfo file into the running totals.
///
/// `comm` is a closure because resolving it costs a file read, and a process holding
/// several descriptors to the same client only needs it once.
fn parse_client(
    text: &str,
    pdev: &str,
    out: &mut GpuCounters,
    comm: impl FnOnce() -> String,
) -> Option<()> {
    let mut client_id: Option<u64> = None;
    let mut matches_device = false;
    let mut cycles: Vec<(String, u64)> = Vec::new();
    let mut totals: Vec<(String, u64)> = Vec::new();

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "drm-pdev" => matches_device = value == pdev,
            "drm-client-id" => client_id = value.parse().ok(),
            // Order matters: `drm-total-cycles-rcs` also starts with `drm-`, and the
            // narrower prefix has to be tested first or every total is counted as a
            // client's own usage.
            k if k.starts_with("drm-total-cycles-") => {
                if let (Some(eng), Ok(v)) = (k.strip_prefix("drm-total-cycles-"), value.parse()) {
                    totals.push((eng.to_string(), v));
                }
            }
            k if k.starts_with("drm-cycles-") => {
                if let (Some(eng), Ok(v)) = (k.strip_prefix("drm-cycles-"), value.parse()) {
                    cycles.push((eng.to_string(), v));
                }
            }
            _ => {}
        }
    }

    if !matches_device {
        return None;
    }
    let id = client_id?;

    // The GT clock is device-wide, so any client reporting it is as good as any other.
    // Take the highest seen: they are read microseconds apart and must not go backwards.
    for (engine, value) in totals {
        let slot = out.total.entry(engine).or_default();
        *slot = (*slot).max(value);
    }

    let client = out.per_client.entry(id).or_insert_with(|| ClientCounters {
        comm: comm(),
        engines: HashMap::new(),
    });
    for (engine, value) in cycles {
        // `max`, not `+=`: dup'd descriptors each publish the client's full total, so
        // adding them would multiply usage by the descriptor count.
        let slot = client.engines.entry(engine).or_default();
        *slot = (*slot).max(value);
    }
    Some(())
}

/// Per-engine busy percentages, busiest first, and the heaviest client.
fn engine_busy(prev: &GpuCounters, now: &GpuCounters) -> (EngineLoad, Option<GpuClient>) {
    let mut per_engine: HashMap<&str, u64> = HashMap::new();
    // client -> its largest single-engine delta
    let mut per_client: HashMap<&str, u64> = HashMap::new();

    for (id, client) in &now.per_client {
        // A client that appeared during this interval contributes nothing yet. Counting
        // it from zero would credit its whole lifetime to one tick and spike to 100%.
        let Some(before) = prev.per_client.get(id) else {
            continue;
        };
        for (engine, cycles) in &client.engines {
            let Some(was) = before.engines.get(engine) else {
                continue;
            };
            let delta = cycles.saturating_sub(*was);
            *per_engine.entry(engine.as_str()).or_default() += delta;
            let slot = per_client.entry(client.comm.as_str()).or_default();
            *slot = (*slot).max(delta);
        }
    }

    let clock = |engine: &str| -> Option<u64> {
        let dt = now
            .total
            .get(engine)?
            .saturating_sub(*prev.total.get(engine)?);
        (dt > 0).then_some(dt)
    };

    let mut engines: EngineLoad = per_engine
        .iter()
        .filter_map(|(engine, delta)| {
            let dt = clock(engine)?;
            Some((
                (*engine).to_string(),
                (*delta as f64 * 100.0 / dt as f64).clamp(0.0, 100.0),
            ))
        })
        .collect();
    engines.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // The busiest engine's clock is the fairest reference for a client's share, and
    // every engine's clock is the same GT counter anyway.
    let reference = engines.first().and_then(|(engine, _)| clock(engine));
    let top = reference.and_then(|dt| {
        per_client
            .iter()
            .max_by_key(|(_, delta)| **delta)
            .filter(|(_, delta)| **delta > 0)
            .map(|(comm, delta)| {
                (
                    (*comm).to_string(),
                    (*delta as f64 * 100.0 / dt as f64).clamp(0.0, 100.0),
                )
            })
    });

    (engines, top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir()
                .join(format!("fw-helper-usage-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            Self(root)
        }

        fn write(&self, rel: &str, contents: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, contents).unwrap();
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.0)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    /// `/proc/self/status` as the kernel writes it, trimmed to the two lines that
    /// decide whether another process's fdinfo can be opened.
    fn status(euid: u64, cap_eff: u64) -> String {
        format!(
            "Name:\tfw-helperd\nUid:\t{euid}\t{euid}\t{euid}\t{euid}\nCapEff:\t{cap_eff:016x}\n"
        )
    }

    #[test]
    fn root_without_cap_sys_ptrace_cannot_read_gpu_clients() {
        // The defect this exists for: the packaged daemon ran as uid 0 with
        // `CapabilityBoundingSet=` empty, so it held no capabilities at all. Opening
        // another user's /proc/<pid>/fdinfo needs CAP_SYS_PTRACE, every GPU client
        // belongs to the desktop user, and so gpu_percent was never published - while
        // gpu_mhz, which is an ordinary sysfs read, arrived on every tick and made the
        // GPU look present. Measured on hardware 2026-09-20.
        let t = Tree::new("blocked");
        t.write("proc/self/status", &status(0, 0));
        let reason = fdinfo_blocked(&t.sysfs()).expect("root with no capabilities is blocked");
        // The reason has to name the fix, not the symptom.
        assert!(reason.contains("CAP_SYS_PTRACE"), "{reason}");
        assert!(reason.contains("fw-helperd.service"), "{reason}");
    }

    #[test]
    fn the_capability_unblocks_it() {
        let t = Tree::new("granted");
        t.write("proc/self/status", &status(0, CAP_SYS_PTRACE));
        assert_eq!(fdinfo_blocked(&t.sysfs()), None);
    }

    #[test]
    fn an_unprivileged_client_is_never_reported_as_blocked() {
        // Development mode runs as the user, which is the uid owning every process that
        // drives the GPU - so it reads them all without any capability. Reporting this
        // as blocked would have put a false explanation under a working number.
        let t = Tree::new("user");
        t.write("proc/self/status", &status(1000, 0));
        assert_eq!(fdinfo_blocked(&t.sysfs()), None);
    }

    /// One xe fdinfo, as the reference machine writes them.
    fn fdinfo(client: u64, pdev: &str, rcs: u64, total: u64) -> String {
        format!(
            "pos:\t0\nflags:\t02104002\nmnt_id:\t508\nino:\t975\n\
             drm-driver:\txe\ndrm-client-id:\t{client}\ndrm-pdev:\t{pdev}\n\
             drm-total-gtt:\t2366880 KiB\n\
             drm-cycles-rcs:\t{rcs}\ndrm-total-cycles-rcs:\t{total}\n\
             drm-cycles-bcs:\t0\ndrm-total-cycles-bcs:\t{total}\n"
        )
    }

    // --- /proc/stat -------------------------------------------------------------

    #[test]
    fn reads_the_aggregate_cpu_line_not_a_core() {
        let t = Tree::new("stat");
        // The per-core lines follow the aggregate and must not be picked up.
        t.write(
            "proc/stat",
            "cpu  100 10 40 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 0 0 0 0 0\nintr 12345\n",
        );
        let times = read_cpu_times(&t.sysfs()).expect("aggregate line");
        assert_eq!(times.total, 1000);
        // busy is everything that is not idle(800) + iowait(50)
        assert_eq!(times.busy, 150);
    }

    #[test]
    fn a_truncated_stat_line_is_not_a_reading() {
        let t = Tree::new("stat-short");
        t.write("proc/stat", "cpu  1 2 3\n");
        assert!(read_cpu_times(&t.sysfs()).is_none());
    }

    #[test]
    fn cpu_percent_needs_two_samples() {
        let t = Tree::new("cpu-pct");
        t.write("proc/stat", "cpu  100 0 0 900 0 0 0 0 0 0\n");
        let mut s = UsageSampler::new(t.sysfs());
        let t0 = Instant::now();

        // First sample only establishes the reference point.
        assert_eq!(s.sample(t0).cpu_percent, None);

        // 250 more jiffies of work out of 1000 elapsed.
        t.write("proc/stat", "cpu  350 0 0 1650 0 0 0 0 0 0\n");
        let pct = s.sample(at(t0, 1)).cpu_percent.expect("second sample");
        assert!((pct - 25.0).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn a_gap_long_enough_to_be_a_suspend_is_discarded() {
        let t = Tree::new("cpu-gap");
        t.write("proc/stat", "cpu  100 0 0 900 0 0 0 0 0 0\n");
        let mut s = UsageSampler::new(t.sysfs());
        let t0 = Instant::now();
        s.sample(t0);
        t.write("proc/stat", "cpu  350 0 0 1650 0 0 0 0 0 0\n");
        // 61 s: the machine was most likely asleep, and a rate across a suspend is
        // fiction in both directions.
        assert_eq!(s.sample(at(t0, 61)).cpu_percent, None);
    }

    #[test]
    fn invalidate_drops_the_reference_point() {
        let t = Tree::new("cpu-invalidate");
        t.write("proc/stat", "cpu  100 0 0 900 0 0 0 0 0 0\n");
        let mut s = UsageSampler::new(t.sysfs());
        let t0 = Instant::now();
        s.sample(t0);
        s.invalidate();
        t.write("proc/stat", "cpu  350 0 0 1650 0 0 0 0 0 0\n");
        assert_eq!(s.sample(at(t0, 1)).cpu_percent, None);
    }

    // --- /proc/meminfo ----------------------------------------------------------

    #[test]
    fn memory_used_is_total_minus_available_not_free() {
        let t = Tree::new("mem");
        // MemFree is tiny while MemAvailable is large — the ordinary state of a healthy
        // machine, and the case where using MemFree reads as "full".
        t.write(
            "proc/meminfo",
            "MemTotal:       32388396 kB\nMemFree:         2666416 kB\n\
             MemAvailable:   11021892 kB\nBuffers:          923624 kB\n\
             SwapTotal:       8388604 kB\nSwapFree:        5865876 kB\n",
        );
        let mut s = UsageSampler::new(t.sysfs());
        let u = s.sample(Instant::now());
        assert_eq!(u.mem_total_kb, Some(32_388_396));
        assert_eq!(u.mem_used_kb, Some(32_388_396 - 11_021_892));
        assert_eq!(u.swap_used_kb, Some(8_388_604 - 5_865_876));
        // 21.4 GB of 32.4 GB in use — the *used* share, not the available one.
        let pct = u.mem_percent().expect("percent");
        assert!((pct - 66.0).abs() < 1.0, "got {pct}");
    }

    // --- throttling -------------------------------------------------------------

    #[test]
    fn throttle_counters_report_the_interval_not_the_total_since_boot() {
        let t = Tree::new("throttle");
        t.write("proc/stat", "cpu  1 0 0 1 0 0 0 0 0 0\n");
        t.write(&format!("{CPU_THROTTLE}/package_throttle_count"), "40\n");
        t.write(
            &format!("{CPU_THROTTLE}/package_throttle_total_time_ms"),
            "5000\n",
        );
        let mut s = UsageSampler::new(t.sysfs());
        let t0 = Instant::now();
        let first = s.sample(t0);
        // A since-boot total would report 40 here and never come back down.
        assert_eq!(first.cpu_throttle_events, 0);

        t.write(&format!("{CPU_THROTTLE}/package_throttle_count"), "43\n");
        t.write(
            &format!("{CPU_THROTTLE}/package_throttle_total_time_ms"),
            "5120\n",
        );
        let u = s.sample(at(t0, 1));
        assert_eq!(u.cpu_throttle_events, 3);
        assert_eq!(u.cpu_throttle_ms, 120);
        assert!(u.throttled());
    }

    #[test]
    fn gpu_throttle_reasons_are_enumerated_from_the_directory() {
        let t = Tree::new("gpu-throttle");
        let gt = "sys/class/drm/card1/device/tile0/gt0";
        t.write(&format!("{gt}/freq0/throttle/reason_pl1"), "1\n");
        t.write(&format!("{gt}/freq0/throttle/reason_thermal"), "0\n");
        t.write(&format!("{gt}/freq0/throttle/reason_vr_tdc"), "1\n");
        // Not a reason_* file, and must not be mistaken for one.
        t.write(&format!("{gt}/freq0/throttle/status"), "1\n");
        assert_eq!(
            read_throttle_reasons(&t.sysfs(), gt),
            vec!["pl1".to_string(), "vr_tdc".to_string()]
        );
    }

    // --- DRM device resolution --------------------------------------------------

    #[test]
    fn the_drm_device_is_found_by_driver_not_by_index() {
        let t = Tree::new("drm-find");
        // card0 is a different driver; the xe device is card3, and neither index is
        // the reference machine's card1 — precisely the point.
        t.write(
            "sys/class/drm/card0/device/uevent",
            "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n",
        );
        t.write(
            "sys/class/drm/card3/device/uevent",
            "DRIVER=xe\nPCI_SLOT_NAME=0000:00:02.0\n",
        );
        t.write("sys/class/drm/card3/device/tile0/gt0/freq0/act_freq", "0\n");
        // A connector, not a device.
        t.write(
            "sys/class/drm/card3-eDP-1/device/uevent",
            "DRIVER=xe\nPCI_SLOT_NAME=0000:00:02.0\n",
        );

        let dev = find_drm(&t.sysfs()).expect("xe device");
        assert_eq!(dev.pdev, "0000:00:02.0");
        assert_eq!(dev.gt, "sys/class/drm/card3/device/tile0/gt0");
    }

    #[test]
    fn an_unsupported_driver_reports_its_name_rather_than_pretending() {
        let t = Tree::new("drm-i915");
        t.write(
            "sys/class/drm/card0/device/uevent",
            "DRIVER=i915\nPCI_SLOT_NAME=0000:00:02.0\n",
        );
        t.write("sys/class/drm/card0/device/tile0/gt0/freq0/act_freq", "0\n");
        assert!(find_drm(&t.sysfs()).is_none());
        // The capability layer needs the driver's name to explain itself.
        assert_eq!(drm_driver(&t.sysfs()).as_deref(), Some("i915"));
    }

    // --- fdinfo parsing ---------------------------------------------------------

    #[test]
    fn duplicated_descriptors_do_not_multiply_a_clients_usage() {
        // A process holding the same DRM client through three dup'd fds publishes the
        // full counter set on each. Summing them would treble its reported load.
        let mut out = GpuCounters::default();
        let text = fdinfo(21, "0000:00:02.0", 1_000, 10_000);
        for _ in 0..3 {
            parse_client(&text, "0000:00:02.0", &mut out, || "gnome-shell".into());
        }
        assert_eq!(out.per_client.len(), 1);
        assert_eq!(out.per_client[&21].engines["rcs"], 1_000);
    }

    #[test]
    fn clients_of_another_gpu_are_not_counted() {
        let mut out = GpuCounters::default();
        let text = fdinfo(7, "0000:03:00.0", 5_000, 10_000);
        parse_client(&text, "0000:00:02.0", &mut out, || "other".into());
        assert!(out.per_client.is_empty());
        assert!(out.total.is_empty());
    }

    #[test]
    fn the_gt_clock_is_not_read_as_a_clients_own_usage() {
        // `drm-total-cycles-rcs` also begins with `drm-`, so a careless prefix test
        // credits the whole GT clock to the client and every reading becomes 100%.
        let mut out = GpuCounters::default();
        parse_client(
            &fdinfo(1, "0000:00:02.0", 1_000, 4_000_000),
            "0000:00:02.0",
            &mut out,
            || "app".into(),
        );
        assert_eq!(out.per_client[&1].engines["rcs"], 1_000);
        assert_eq!(out.total["rcs"], 4_000_000);
    }

    // --- busy computation -------------------------------------------------------

    fn counters(entries: &[(u64, &str, &str, u64)], total: u64) -> GpuCounters {
        let mut c = GpuCounters::default();
        for (id, comm, engine, cycles) in entries {
            c.per_client
                .entry(*id)
                .or_insert_with(|| ClientCounters {
                    comm: (*comm).to_string(),
                    engines: HashMap::new(),
                })
                .engines
                .insert((*engine).to_string(), *cycles);
            c.total.insert((*engine).to_string(), total);
        }
        c
    }

    #[test]
    fn busy_is_the_delta_over_the_gt_clock_delta() {
        let prev = counters(&[(1, "game", "rcs", 0)], 0);
        let now = counters(&[(1, "game", "rcs", 750)], 1_000);
        let (engines, top) = engine_busy(&prev, &now);
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].0, "rcs");
        assert!((engines[0].1 - 75.0).abs() < 0.001, "got {:?}", engines[0]);
        let (comm, pct) = top.expect("a client");
        assert_eq!(comm, "game");
        assert!((pct - 75.0).abs() < 0.001);
    }

    #[test]
    fn the_busiest_engine_wins_rather_than_the_sum() {
        // Engines run in parallel. Summing 80% render and 40% copy would report 120%.
        let prev = counters(&[(1, "game", "rcs", 0), (1, "game", "bcs", 0)], 0);
        let now = counters(&[(1, "game", "rcs", 800), (1, "game", "bcs", 400)], 1_000);
        let (engines, _) = engine_busy(&prev, &now);
        assert_eq!(engines[0].0, "rcs");
        assert!((engines[0].1 - 80.0).abs() < 0.001);
        assert!((engines[1].1 - 40.0).abs() < 0.001);
        // And the published headline is the busiest, which is at most 100.
        assert!(engines[0].1 <= 100.0);
    }

    #[test]
    fn a_client_that_appeared_this_interval_contributes_nothing_yet() {
        // Counting a new client from zero would credit its entire lifetime to one tick
        // and pin the graph at 100% whenever anything launched.
        let prev = counters(&[(1, "shell", "rcs", 100)], 1_000);
        let mut now = counters(&[(1, "shell", "rcs", 200)], 2_000);
        now.per_client.insert(
            99,
            ClientCounters {
                comm: "game".into(),
                engines: HashMap::from([("rcs".to_string(), 900_000)]),
            },
        );
        let (engines, top) = engine_busy(&prev, &now);
        // Only the shell's 100 cycles over a 1000-cycle clock.
        assert!((engines[0].1 - 10.0).abs() < 0.001, "got {:?}", engines);
        assert_eq!(top.expect("client").0, "shell");
    }

    #[test]
    fn a_stalled_gt_clock_yields_no_reading_rather_than_a_division_by_zero() {
        let prev = counters(&[(1, "app", "rcs", 10)], 5_000);
        let now = counters(&[(1, "app", "rcs", 20)], 5_000);
        let (engines, top) = engine_busy(&prev, &now);
        assert!(engines.is_empty());
        assert!(top.is_none());
    }

    #[test]
    fn the_scan_is_off_until_asked_for() {
        let t = Tree::new("scan-gate");
        t.write("proc/stat", "cpu  1 0 0 1 0 0 0 0 0 0\n");
        let mut s = UsageSampler::new(t.sysfs());
        assert!(!s.gpu_scan_enabled());
        s.set_gpu_scan(true);
        assert!(s.gpu_scan_enabled());
    }
}

#[cfg(test)]
mod rail_tests {
    use super::*;
    use std::path::PathBuf;

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fw-helper-rail-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp tree");
            Self(dir)
        }
        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.0)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The whole reason [`Sysfs::find_powercap`] exists: the zone that measures the
    /// GPU is not at a fixed index, so a machine that orders them differently must
    /// still resolve `uncore` to the right directory.
    #[test]
    fn rails_are_found_by_name_not_by_zone_index() {
        let t = Tree::new("byname");
        let base = "sys/class/powercap";
        t.write(&format!("{base}/intel-rapl:0/name"), "package-0\n");
        // Reversed against the reference machine, where core is :0:0.
        t.write(
            &format!("{base}/intel-rapl:0/intel-rapl:0:0/name"),
            "uncore\n",
        );
        t.write(
            &format!("{base}/intel-rapl:0/intel-rapl:0:1/name"),
            "core\n",
        );

        let fs = t.sysfs();
        assert_eq!(
            fs.find_powercap("core").as_deref(),
            Some("sys/class/powercap/intel-rapl:0/intel-rapl:0:1")
        );
        assert_eq!(
            fs.find_powercap("uncore").as_deref(),
            Some("sys/class/powercap/intel-rapl:0/intel-rapl:0:0")
        );
        assert_eq!(fs.find_powercap("dram"), None);
    }

    /// A machine with no `uncore` zone reports no GPU wattage rather than zero.
    #[test]
    fn a_missing_rail_is_absent_not_zero() {
        let t = Tree::new("missing");
        let fs = t.sysfs();
        assert!(rail(&fs, "uncore").is_none());
    }

    /// Watts need two readings; the first can only establish the reference point.
    #[test]
    fn a_rail_needs_two_samples_before_it_reports_watts() {
        let t = Tree::new("twosamples");
        let zone = "sys/class/powercap/intel-rapl:0/intel-rapl:0:1";
        t.write(&format!("{zone}/name"), "uncore\n");
        t.write(&format!("{zone}/max_energy_range_uj"), "262143328850\n");
        t.write(&format!("{zone}/energy_uj"), "1000000\n");

        let fs = t.sysfs();
        let mut r = rail(&fs, "uncore");
        let t0 = Instant::now();
        assert_eq!(UsageSampler::sample_rail(fs.clone(), r.as_mut(), t0), None);

        // 5 J over 1 s is 5 W.
        t.write(&format!("{zone}/energy_uj"), "6000000\n");
        let w = UsageSampler::sample_rail(fs, r.as_mut(), t0 + Duration::from_secs(1))
            .expect("second sample yields watts");
        assert!((w - 5.0).abs() < 0.05, "expected ~5 W, got {w}");
    }

    /// `act_freq` reads 0 while the GT is in RC6. That is a fact about the GPU, not a
    /// failed read, and the two must stay distinguishable: the UI says "parked" for
    /// one and drops the field for the other.
    #[test]
    fn a_parked_gt_reads_zero_rather_than_unknown() {
        let t = Tree::new("parked");
        let gt = "sys/class/drm/card1/device/tile0/gt0";
        t.write(&format!("{gt}/freq0/act_freq"), "0\n");
        let s = UsageSampler::new(t.sysfs());
        assert_eq!(s.read_gpu_mhz(gt), Some(0));

        let t2 = Tree::new("unreadable");
        let s2 = UsageSampler::new(t2.sysfs());
        assert_eq!(
            s2.read_gpu_mhz("sys/class/drm/card1/device/tile0/gt0"),
            None
        );
    }

    /// The burst exists to catch a GT that is awake but duty-cycling. A single read
    /// landing on an RC6 instant is why the clock vanished from the window.
    #[test]
    fn a_nonzero_reading_wins_over_a_parked_one() {
        let t = Tree::new("awake");
        let gt = "sys/class/drm/card1/device/tile0/gt0";
        t.write(&format!("{gt}/freq0/act_freq"), "1850\n");
        let s = UsageSampler::new(t.sysfs());
        assert_eq!(s.read_gpu_mhz(gt), Some(1850));
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::path::PathBuf;

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fw-helper-clock-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp tree");
            Self(dir)
        }
        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        fn cpu(&self, n: usize, khz: u64) {
            self.write(
                &format!("sys/devices/system/cpu/cpu{n}/cpufreq/scaling_cur_freq"),
                &format!("{khz}\n"),
            );
        }
        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.0)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The reason this exists. One core at 4.5 GHz doing all the work, three parked at
    /// 800 MHz: the flat mean says 1.7 GHz, which is a speed nothing ran at.
    #[test]
    fn the_busy_clock_ignores_parked_cores() {
        let t = Tree::new("weighted");
        t.cpu(0, 4_500_000);
        for n in 1..4 {
            t.cpu(n, 800_000);
        }
        let s = UsageSampler::new(t.sysfs());
        let (mean, busy) = s.read_cpu_clocks(Some(&[100.0, 0.0, 0.0, 0.0]));
        assert_eq!(mean, Some(1725)); // (4500+800*3)/4
        assert_eq!(busy, Some(4500));
    }

    /// Weighted by how much each core ran, not merely by whether it did.
    #[test]
    fn the_busy_clock_weights_by_time_executed() {
        let t = Tree::new("proportional");
        t.cpu(0, 4_000_000);
        t.cpu(1, 2_000_000);
        let s = UsageSampler::new(t.sysfs());
        // Three quarters of the work at 4 GHz, one quarter at 2: 3500, not 3000.
        assert_eq!(s.read_cpu_clocks(Some(&[75.0, 25.0])).1, Some(3500));
    }

    /// An idle interval has no "speed the work ran at". Reporting the diluted mean here
    /// would relabel it as the honest figure, which is the whole error being avoided.
    #[test]
    fn an_idle_interval_has_no_busy_clock() {
        let t = Tree::new("idle");
        t.cpu(0, 900_000);
        t.cpu(1, 900_000);
        let s = UsageSampler::new(t.sysfs());
        let (mean, busy) = s.read_cpu_clocks(Some(&[0.0, 0.0]));
        assert_eq!(mean, Some(900));
        assert_eq!(busy, None);
    }

    /// No weights — first sample, or a gap long enough to be a suspend.
    #[test]
    fn without_weights_only_the_mean_is_reported() {
        let t = Tree::new("noweights");
        t.cpu(0, 3_000_000);
        let s = UsageSampler::new(t.sysfs());
        assert_eq!(s.read_cpu_clocks(None), (Some(3000), None));
    }

    #[test]
    fn per_core_times_are_indexed_by_core_number() {
        let t = Tree::new("percore");
        t.write(
            "proc/stat",
            "cpu  100 0 100 800 0 0 0 0 0 0\n\
             cpu0 50 0 50 400 0 0 0 0 0 0\n\
             cpu1 50 0 50 400 0 0 0 0 0 0\n\
             intr 12345\n",
        );
        let v = read_cpu_times_per_core(&t.sysfs()).expect("per-core times");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].busy, 100);
        assert_eq!(v[0].total, 500);
    }

    /// A gap would mean index N is not core N, so the weights would be applied to the
    /// wrong core's frequency. Refuse rather than mis-attribute.
    #[test]
    fn non_contiguous_cores_are_refused() {
        let t = Tree::new("gappy");
        t.write(
            "proc/stat",
            "cpu  100 0 100 800 0 0 0 0 0 0\n\
             cpu0 50 0 50 400 0 0 0 0 0 0\n\
             cpu2 50 0 50 400 0 0 0 0 0 0\n",
        );
        assert_eq!(read_cpu_times_per_core(&t.sysfs()), None);
    }
}
