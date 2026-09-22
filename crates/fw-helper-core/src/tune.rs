//! Core parking and GPU frequency capping — the two levers M9's profiles add.
//!
//! Both exist for reasons that were measured rather than reasoned, and both are more
//! modest than their names suggest. Read `docs/framework_gaming_profile.md` before
//! changing anything here.
//!
//! **Core parking is a placement tool, not a power one.** Horizon Zero Dawn reports
//! CPU FPS 34 against GPU FPS 45 — CPU-bound — while no thread exceeds 50% and the
//! busiest core sits at 47%. That is a latency-bound critical thread, and on a hybrid
//! part the usual cause is the scheduler putting it on an E-core at 3.7 GHz or an LP-E
//! core at 3.3 GHz instead of a P-core at 4.8. Parking the Atom clusters forces the
//! issue. Whether it *also* raises the surviving cores' clock by concentrating the
//! power budget is a separate question — M9 Phase 0 question E.
//!
//! **The GPU cap is unproven.** Lowering `max_freq` is at least the right direction:
//! this board already showed that *raising* `min_freq` to 2500 is honoured by the
//! driver (`cur_freq` duly reads 2500) and ignored by the silicon (`act_freq` stays at
//! 1850). A request to go slower has a better chance of being obeyed than a request to
//! go faster, but "better chance" is not a measurement — hence [`GpuFreq::probe`]
//! reporting availability while the profile that uses it stays gated on Phase 0.
//!
//! Three things here are not obvious:
//!
//! - **`cpu0` has no `online` file** and can never be parked. This is not a quirk of
//!   this board; x86 kernels pin the boot CPU unless `CONFIG_BOOTPARAM_HOTPLUG_CPU0`
//!   is set. A UI must show it as permanently on, never as a toggle that fails.
//! - **The kernel's own masks do not separate E from LP-E.** `cpu_core` is `0-3` and
//!   `cpu_atom` is `4-15`; both Atom clusters share one PMU. The split has to come
//!   from `core_id` (32+ for LP-E on this part) or `cpuinfo_max_freq`.
//! - **CPU numbering is not a promise.** Everything here resolves clusters at runtime,
//!   the same discipline as resolving hwmon by `name`.

use crate::Sysfs;
use std::fmt;

const CPU_BASE: &str = "sys/devices/system/cpu";
const DRM_BASE: &str = "sys/class/drm";

/// Which physical cluster a CPU belongs to.
///
/// Ordered slowest-to-fastest so `park` levels can be expressed as "everything below
/// this", and so a sort puts the fast cores last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cluster {
    /// Low-power efficient cores, on the SoC tile. 3.3 GHz on the reference part.
    LpE,
    /// Efficient cores on the compute tile. 3.7 GHz.
    E,
    /// Performance cores. 4.7–4.8 GHz.
    P,
}

impl Cluster {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P => "P",
            Self::E => "E",
            Self::LpE => "LP-E",
        }
    }
}

impl fmt::Display for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One logical CPU, as discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpu {
    pub num: u32,
    pub cluster: Cluster,
    /// `cpuinfo_max_freq`, in kHz. 0 if the node was missing.
    pub max_khz: u32,
    /// False for `cpu0`, which the kernel does not expose an `online` file for.
    pub parkable: bool,
}

/// How much of the machine a profile wants running.
///
/// Three levels rather than a free-form CPU mask, because the useful choices are
/// cluster-shaped and because a mask invites someone to park every P-core. Graduated
/// rather than binary because the workloads disagree: an older single-threaded game or
/// Dolphin wants [`ParkLevel::PCoresOnly`], while RPCS3 is heavily multithreaded for SPU
/// emulation and would likely *lose* there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParkLevel {
    /// Everything online. The default, and what any uninstall or crash must restore.
    #[default]
    None,
    /// Park the LP-E cluster only. Removes the slowest cores — where a migrated hot
    /// thread suffers most — while keeping 12 of 16 for threaded work.
    Lpe,
    /// Park both Atom clusters, leaving the P-cores. Maximum single-thread.
    PCoresOnly,
}

impl ParkLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lpe => "lpe",
            Self::PCoresOnly => "p-only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "all" | "" => Some(Self::None),
            "lpe" | "lp-e" => Some(Self::Lpe),
            "p-only" | "ponly" | "p" => Some(Self::PCoresOnly),
            _ => None,
        }
    }

    /// Clusters this level takes offline.
    fn parks(self) -> &'static [Cluster] {
        match self {
            Self::None => &[],
            Self::Lpe => &[Cluster::LpE],
            Self::PCoresOnly => &[Cluster::LpE, Cluster::E],
        }
    }
}

impl fmt::Display for ParkLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub enum TuneError {
    /// No hybrid topology found, or no parkable cores.
    Unsupported(String),
    OutOfRange {
        mhz: u32,
        min: u32,
        max: u32,
    },
    Io(std::io::Error),
    /// Written, read back, and the kernel disagrees.
    NotApplied {
        what: String,
        requested: String,
        observed: String,
    },
}

impl fmt::Display for TuneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "{why}"),
            Self::OutOfRange { mhz, min, max } => write!(
                f,
                "{mhz} MHz is outside the GPU's range {min}-{max} MHz, as reported by \
                 rpn_freq and rp0_freq"
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::NotApplied {
                what,
                requested,
                observed,
            } => write!(
                f,
                "set {what} to {requested} but it reads {observed}; something overrode it"
            ),
        }
    }
}

impl std::error::Error for TuneError {}

impl From<std::io::Error> for TuneError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------------

/// The machine's CPUs, resolved at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreSet {
    cpus: Vec<Cpu>,
    /// False when the probe ran with cores already offline, so the classification
    /// could not be trusted. A caller holding a level over time should re-probe once
    /// the machine is whole rather than act on a degraded set.
    complete: bool,
}

impl CoreSet {
    /// Discover clusters without trusting CPU numbering.
    ///
    /// The kernel's hybrid PMU masks (`/sys/devices/cpu_core/cpus`) separate P from
    /// Atom and nothing more, so LP-E is identified by `core_id` — on Panther Lake the
    /// LP-E cores sit in their own numbering block well above the compute tile's — with
    /// `cpuinfo_max_freq` as the fallback when `core_id` is unavailable or unhelpful.
    pub fn probe(fs: &Sysfs) -> Self {
        let mut cpus = Vec::new();
        let p_mask = read_cpu_list(fs, "sys/devices/cpu_core/cpus");

        let Ok(entries) = std::fs::read_dir(fs.path(CPU_BASE)) else {
            return Self {
                cpus,
                complete: false,
            };
        };
        let mut nums: Vec<u32> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let rest = name.strip_prefix("cpu")?;
                rest.parse::<u32>().ok()
            })
            .collect();
        nums.sort_unstable();

        // Two passes: the LP-E split needs to know what the rest of the machine looks
        // like before it can call an outlier.
        let raw: Vec<(u32, Option<u32>, u32)> = nums
            .iter()
            .map(|&n| {
                // Both of these vanish while a CPU is offline, which is why a probe
                // taken on a parked machine is degraded. Recorded rather than worked
                // around: there is nothing left in sysfs to classify such a core by.
                let core_id = fs
                    .read_u64(&format!("{CPU_BASE}/cpu{n}/topology/core_id"))
                    .ok()
                    .map(|v| v as u32);
                let max_khz = fs
                    .read_u64(&format!("{CPU_BASE}/cpu{n}/cpufreq/cpuinfo_max_freq"))
                    .unwrap_or(0) as u32;
                (n, core_id, max_khz)
            })
            .collect();

        // An Atom core belongs to LP-E if its core_id sits in a distinctly higher block
        // than the other Atom cores. On the reference part E is 16-23 and LP-E is 32-35.
        let atom_ids: Vec<u32> = raw
            .iter()
            .filter(|(n, _, _)| !is_p(*n, &p_mask, &raw))
            .filter_map(|(_, id, _)| *id)
            .collect();
        let lpe_floor = lpe_boundary(&atom_ids);

        for (n, core_id, max_khz) in raw.iter().copied() {
            let cluster = if is_p(n, &p_mask, &raw) {
                Cluster::P
            } else if lpe_floor.is_some_and(|f| core_id.is_some_and(|id| id >= f)) {
                Cluster::LpE
            } else {
                Cluster::E
            };
            cpus.push(Cpu {
                num: n,
                cluster,
                max_khz,
                parkable: fs.exists(&format!("{CPU_BASE}/cpu{n}/online")),
            });
        }
        // A core with no `cpuinfo_max_freq` is one the kernel has taken down, and its
        // cluster is a guess. Say so rather than publish a shape that will change when
        // it comes back.
        let complete = cpus.iter().all(|c| c.max_khz > 0);
        Self { cpus, complete }
    }

    /// Whether every core was online when this was probed.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn cpus(&self) -> &[Cpu] {
        &self.cpus
    }

    pub fn in_cluster(&self, c: Cluster) -> Vec<&Cpu> {
        self.cpus.iter().filter(|x| x.cluster == c).collect()
    }

    /// Clusters present, fastest first — the order a UI should list them.
    pub fn clusters(&self) -> Vec<Cluster> {
        let mut seen: Vec<Cluster> = self.cpus.iter().map(|c| c.cluster).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.reverse();
        seen
    }

    /// CPUs a level takes offline. Never includes an unparkable core, so `cpu0`
    /// cannot be selected even by a level that names its cluster.
    pub fn to_park(&self, level: ParkLevel) -> Vec<u32> {
        let parks = level.parks();
        self.cpus
            .iter()
            .filter(|c| c.parkable && parks.contains(&c.cluster))
            .map(|c| c.num)
            .collect()
    }

    /// A short human summary, for logs and for the capability reason.
    pub fn summary(&self) -> String {
        self.clusters()
            .iter()
            .map(|&c| {
                let list = self.in_cluster(c);
                let mhz = list.first().map(|x| x.max_khz / 1000).unwrap_or(0);
                format!("{} x{} @{mhz}MHz", c.as_str(), list.len())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `cpu_core` is authoritative when present; otherwise fall back to "fastest tier".
fn is_p(n: u32, p_mask: &[u32], raw: &[(u32, Option<u32>, u32)]) -> bool {
    if !p_mask.is_empty() {
        return p_mask.contains(&n);
    }
    let top = raw.iter().map(|(_, _, k)| *k).max().unwrap_or(0);
    top > 0 && raw.iter().any(|(m, _, k)| *m == n && *k == top)
}

/// The `core_id` at which LP-E begins, if the Atom cores fall into two clear blocks.
///
/// Returns `None` when they do not — a part with a single Atom tier has no LP-E, and
/// guessing one would park cores for no reason.
fn lpe_boundary(atom_ids: &[u32]) -> Option<u32> {
    if atom_ids.len() < 2 {
        return None;
    }
    let mut ids: Vec<u32> = atom_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    // The largest gap, if it is bigger than the spacing within either block.
    let mut best = (0u32, 0usize);
    for i in 1..ids.len() {
        let gap = ids[i] - ids[i - 1];
        if gap > best.0 {
            best = (gap, i);
        }
    }
    // A gap of 1 is contiguous numbering: one block, no LP-E.
    (best.0 > 4).then(|| ids[best.1])
}

fn read_cpu_list(fs: &Sysfs, rel: &str) -> Vec<u32> {
    let Ok(s) = fs.read_string(rel) else {
        return Vec::new();
    };
    parse_cpu_list(&s)
}

/// `0-3`, `4-11,14`, `2` — the kernel's cpulist format.
pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    out.extend(a..=b);
                }
            }
            None => {
                if let Ok(n) = part.parse::<u32>() {
                    out.push(n);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------
// Core parking
// ---------------------------------------------------------------------------------

pub struct CoreParking<'a> {
    fs: &'a Sysfs,
    set: CoreSet,
}

impl<'a> CoreParking<'a> {
    /// Probe the topology now.
    ///
    /// **Do not use this on a machine that may already have cores parked.** An offline
    /// CPU loses its `cpufreq/` and `topology/` directories, so it cannot be classified
    /// — measured 2026-09-22, parking 12-15 made the next probe report a single Atom
    /// tier of twelve, at which point `to_park(Lpe)` returns nothing and the level that
    /// is *currently in effect* reports as parking no cores at all. Anything holding a
    /// level across time wants [`Self::with_set`] and a topology read while the machine
    /// was whole.
    pub fn new(fs: &'a Sysfs) -> Self {
        let set = CoreSet::probe(fs);
        Self { fs, set }
    }

    /// Use a topology captured earlier, when every core was online.
    pub fn with_set(fs: &'a Sysfs, set: CoreSet) -> Self {
        Self { fs, set }
    }

    pub fn core_set(&self) -> &CoreSet {
        &self.set
    }

    /// Whether anything can be parked at all.
    pub fn is_supported(&self) -> bool {
        self.set.cpus.iter().any(|c| c.parkable) && self.set.clusters().len() > 1
    }

    fn online_path(n: u32) -> String {
        format!("{CPU_BASE}/cpu{n}/online")
    }

    pub fn is_online(&self, n: u32) -> bool {
        match self.fs.read_u64(&Self::online_path(n)) {
            Ok(v) => v == 1,
            // No `online` file means the CPU cannot be offlined, so it is online.
            Err(_) => true,
        }
    }

    /// The level currently in effect, or `None` if the machine is in some other state
    /// — a user having offlined cores by hand, say. Reported rather than corrected.
    pub fn read(&self) -> Option<ParkLevel> {
        let offline: Vec<u32> = self
            .set
            .cpus
            .iter()
            .filter(|c| c.parkable && !self.is_online(c.num))
            .map(|c| c.num)
            .collect();
        for level in [ParkLevel::None, ParkLevel::Lpe, ParkLevel::PCoresOnly] {
            let mut want = self.set.to_park(level);
            want.sort_unstable();
            if want == offline {
                return Some(level);
            }
        }
        None
    }

    /// Apply a level, then verify. Onlines first, so a transition between two parked
    /// levels never passes through a state with fewer cores than either.
    pub fn apply(&self, level: ParkLevel) -> Result<(), TuneError> {
        if !self.is_supported() {
            return Err(TuneError::Unsupported(
                "no hybrid topology found; nothing useful to park".into(),
            ));
        }
        let park = self.set.to_park(level);
        for cpu in self.set.cpus.iter().filter(|c| c.parkable) {
            if !park.contains(&cpu.num) {
                self.set_online(cpu.num, true)?;
            }
        }
        for n in &park {
            self.set_online(*n, false)?;
        }
        match self.read() {
            Some(got) if got == level => Ok(()),
            other => Err(TuneError::NotApplied {
                what: "core parking".into(),
                requested: level.to_string(),
                observed: other
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "mixed".into()),
            }),
        }
    }

    /// Bring every core back. The restore path — see ADR 0014.
    ///
    /// Deliberately infallible in aggregate: it tries every CPU and reports the last
    /// error, because a panic handler cannot afford to stop at the first failure and
    /// leave the rest offline.
    pub fn restore_all(&self) -> Result<(), TuneError> {
        let mut last = Ok(());
        for cpu in self.set.cpus.iter().filter(|c| c.parkable) {
            if let Err(e) = self.set_online(cpu.num, true) {
                last = Err(e);
            }
        }
        last
    }

    fn set_online(&self, n: u32, online: bool) -> Result<(), TuneError> {
        if self.is_online(n) == online {
            return Ok(());
        }
        self.fs
            .write_string(&Self::online_path(n), if online { "1" } else { "0" })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------
// GPU frequency
// ---------------------------------------------------------------------------------

/// The render/compute GT's frequency controls.
///
/// Resolved by walking `/sys/class/drm` rather than assuming `card0`: on the reference
/// machine the GPU is **`card1`**, and every circulating script that writes
/// `card0/gt_max_freq_mhz` is both on the wrong card and using `i915` naming this
/// `xe` board does not have.
pub struct GpuFreq<'a> {
    fs: &'a Sysfs,
    base: Option<String>,
}

impl<'a> GpuFreq<'a> {
    pub fn new(fs: &'a Sysfs) -> Self {
        Self {
            fs,
            base: find_gt0(fs),
        }
    }

    pub fn is_supported(&self) -> bool {
        self.base.is_some()
    }

    pub fn path(&self) -> Option<&str> {
        self.base.as_deref()
    }

    fn attr(&self, name: &str) -> Option<String> {
        self.base.as_ref().map(|b| format!("{b}/{name}"))
    }

    fn read_mhz(&self, name: &str) -> Option<u32> {
        self.fs.read_u64(&self.attr(name)?).ok().map(|v| v as u32)
    }

    /// `(rpn_freq, rp0_freq)` — the hardware's own floor and ceiling.
    pub fn range(&self) -> Option<(u32, u32)> {
        Some((self.read_mhz("rpn_freq")?, self.read_mhz("rp0_freq")?))
    }

    pub fn max(&self) -> Option<u32> {
        self.read_mhz("max_freq")
    }

    /// The driver's preferred floor. `reset` returns `min_freq` here rather than to
    /// `rpn_freq`, because `rpn` is the absolute hardware minimum and not where the
    /// driver idles.
    pub fn rpe(&self) -> Option<u32> {
        self.read_mhz("rpe_freq")
    }

    pub fn min(&self) -> Option<u32> {
        self.read_mhz("min_freq")
    }

    /// What the driver is asking for. **Not the achieved clock** — on this board it
    /// reads a constant 2500 under load while `act_freq` reads 1850-1950, and four
    /// separate tools report it as the GPU's speed.
    pub fn requested(&self) -> Option<u32> {
        self.read_mhz("cur_freq")
    }

    /// The clock actually reached. Reads **0 in RC6**, so a single sample of an idle
    /// or bursty GPU is meaningless — burst-sample and keep the highest.
    pub fn achieved(&self) -> Option<u32> {
        self.read_mhz("act_freq")
    }

    /// Cap the GT, lowering `min_freq` first if it would otherwise sit above the new
    /// ceiling — the kernel rejects a `max_freq` below `min_freq`.
    pub fn set_max(&self, mhz: u32) -> Result<(), TuneError> {
        let (rpn, rp0) = self.range().ok_or_else(|| {
            TuneError::Unsupported("no xe GT frequency controls found under /sys/class/drm".into())
        })?;
        if mhz < rpn || mhz > rp0 {
            return Err(TuneError::OutOfRange {
                mhz,
                min: rpn,
                max: rp0,
            });
        }
        if self.min().is_some_and(|m| m > mhz) {
            self.fs
                .write_string(&self.attr("min_freq").unwrap(), &mhz.to_string())?;
        }
        self.fs
            .write_string(&self.attr("max_freq").unwrap(), &mhz.to_string())?;
        match self.max() {
            Some(got) if got == mhz => Ok(()),
            other => Err(TuneError::NotApplied {
                what: "gpu max_freq".into(),
                requested: format!("{mhz} MHz"),
                observed: other
                    .map(|v| format!("{v} MHz"))
                    .unwrap_or_else(|| "nothing".into()),
            }),
        }
    }

    /// Hand the full range back. The restore path.
    pub fn reset(&self) -> Result<(), TuneError> {
        let (rpn, rp0) = self
            .range()
            .ok_or_else(|| TuneError::Unsupported("no xe GT frequency controls found".into()))?;
        // Ceiling first, then floor: the reverse order transiently asks for min > max.
        self.fs
            .write_string(&self.attr("max_freq").unwrap(), &rp0.to_string())?;
        let rpe = self.read_mhz("rpe_freq").unwrap_or(rpn);
        self.fs
            .write_string(&self.attr("min_freq").unwrap(), &rpe.to_string())?;
        Ok(())
    }
}

/// Find `card*/device/tile0/gt0/freq0`, preferring the card that has one.
fn find_gt0(fs: &Sysfs) -> Option<String> {
    let mut cards: Vec<String> = std::fs::read_dir(fs.path(DRM_BASE))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            // `card1`, not `card1-eDP-1`.
            (n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit())).then_some(n)
        })
        .collect();
    cards.sort();
    for card in cards {
        for tile in ["tile0", "tile1"] {
            let rel = format!("{DRM_BASE}/{card}/device/{tile}/gt0/freq0");
            if fs.exists(&format!("{rel}/max_freq")) {
                return Some(rel);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_cpu_lists() {
        assert_eq!(parse_cpu_list("0-3\n"), vec![0, 1, 2, 3]);
        assert_eq!(
            parse_cpu_list("4-11,14"),
            vec![4, 5, 6, 7, 8, 9, 10, 11, 14]
        );
        assert_eq!(parse_cpu_list("2"), vec![2]);
        assert!(parse_cpu_list("").is_empty());
    }

    #[test]
    fn lpe_split_needs_a_real_gap() {
        // Reference part: E is 16-23, LP-E is 32-35.
        assert_eq!(
            lpe_boundary(&[16, 17, 18, 19, 20, 21, 22, 23, 32, 33, 34, 35]),
            Some(32)
        );
        // A single contiguous Atom tier has no LP-E to find.
        assert_eq!(lpe_boundary(&[16, 17, 18, 19]), None);
        assert_eq!(lpe_boundary(&[]), None);
    }

    #[test]
    fn park_levels_round_trip() {
        for l in [ParkLevel::None, ParkLevel::Lpe, ParkLevel::PCoresOnly] {
            assert_eq!(ParkLevel::parse(l.as_str()), Some(l));
        }
        assert_eq!(ParkLevel::parse("LP-E"), Some(ParkLevel::Lpe));
        assert_eq!(ParkLevel::parse("nonsense"), None);
    }
}
