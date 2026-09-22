# Workload-shaped power profiles — Framework 13 Pro, Core Ultra X7 358H

**Status: corrected blueprint. The strategy survives; most of the original mechanisms did
not.** This file began as a generic "Linux gaming profile" write-up. Every path it wrote
has now been checked against this machine (2026-09-22), and every hardware claim against
the measurements already in [CLAUDE.md](../CLAUDE.md) and
[hardware-baseline.md](hardware-baseline.md). The corrections are kept in the open rather
than quietly edited out, because the original is the shape of advice that circulates on
forums for this laptop and it is useful to know exactly where it goes wrong.

**Nothing here is implemented.** The execution plan is M9 in [plan.md](plan.md), and it is
gated on a measurement that has not been taken yet.

---

## 1. What the original got wrong

### 1.1 Three of its four levers do not exist on this board

| The blueprint wrote | What is actually there |
|---|---|
| `/sys/class/drm/card0/gt_max_freq_mhz` | **Does not exist.** That is `i915` naming; this board runs `xe`, and the GPU is **`card1`** — `card0` is not the GPU at all. Both scripts guard the write with `if [ -f ... ]`, so the GPU steps **silently do nothing**. Profile B's entire thesis — cap the iGPU to 1000 MHz to liberate the package — never executes. The real node is `/sys/class/drm/card1/device/tile0/gt0/freq0/max_freq`, mode 644, and there is a second GT (`gt1`, the media engine, max 1200) |
| `/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw` | Wrong zone, and a trap this project already recorded. `intel-rapl:0`'s `long_term` reads **200 W** and binds nothing. PL1 lives at **`intel-rapl-mmio:0`** — which read 15 W when checked, i.e. exactly the setpoint `fw-helperd` was holding |
| `/sys/class/powercap/intel-rapl:0:0/constraint_0_power_limit_uw` | The zone exists, and it is **`enabled = 0` with `constraint_0_power_limit_uw = 0`**. Same for `intel-rapl:0:1` (uncore) and `:0:2` (dram). These are PP0/PP1, which Intel has been deprecating on client parts for years. **The independent CPU and GPU power rails the whole blueprint rests on are disabled domains here.** Whether writing and enabling them binds anything is the single most important open question, and it is unmeasured |
| `max_perf_pct` | Plausible but unverified, and it is the sibling of a known trap: `min_perf_pct` on this machine **accepts 75, reads back 75, and never applies** — under `intel_pstate` in active mode with HWP, the per-core policy is what maps to HWP.MIN. The known-good lever is `cpu*/cpufreq/scaling_max_freq` |

Zone map as measured:

```
intel-rapl:0        package-0   long_term 200 W (meaningless), max_power_uw 25 W
  intel-rapl:0:0    core        enabled 0, limit 0     <- blueprint's CPU starvation
  intel-rapl:0:1    uncore      enabled 0, limit 0     <- the GPU rail (our gpu_watts source)
  intel-rapl:0:2    dram        enabled 0, limit 0
intel-rapl:1        psys        enabled 0, limits 0
intel-rapl-mmio:0   package-0   long_term 15 W, short_term 60 W, peak 175 W   <- the real PL1
```

Note that `core` and `uncore` still publish a working `energy_uj`, which is where
`cpu_watts` and `gpu_watts` come from. **A rail you can measure is not necessarily a rail
you can limit** — that distinction is the whole of section 3.

### 1.2 Two of its hardware premises contradict our measurements

- **"Any combined CPU+GPU execution forces an aggressive global package drop to 25 W."**
  Not what happens. Measured 2026-09-21 on mains at PL1 35 W: `stress-ng --cpu 12` +
  FurMark draws **34.48 W, sitting at the limit**. The package is not dropping. What is
  happening is that the *cores* are clamped to ~11 W and the package total is arithmetic
  on top of that — so it *falls* to 18.48 W under a light GPU load and *rises* to 34.48 W
  under a heavy one. The blueprint has reconstructed the phenomenon from package totals,
  which is precisely the error this project already made once and had to retract.

- **"The iGPU tile has a strict firmware ceiling of 15W–16W, locking it to ~1900 MHz
  instead of 2500 due to transient PL4/EDP limits."** Two errors in one sentence. The
  uncore rail was measured at **21.74 W** under FurMark, not 15–16. And the 1950 MHz
  figure was settled on 2026-09-21 and is **not a clamp**: Intel specifies 2500 as
  *Graphics Max Dynamic Frequency* at an **80 W Maximum Turbo Power**, and this is a
  ~35–38 W part. Replicated independently on kernel 7.3-rc3 (`cur=2500 act=1900-2000`).
  `pl4` in `throttle/reasons` means **"GPU busy"**, not "GPU clamped" — it is asserted on
  a machine reaching a genuine 2500. **Do not reopen this** without an `act_freq` figure
  above 2000 from a saturating load on mains.

- **"Take the 4 LP-E cores offline to eradicate idle leakage current."** Backwards. The
  LP-E cores sit on the SoC tile and exist to take background work *off* the compute tile.
  Parking them pushes that work onto the P/E cores and wakes the compute tile more often.
  There *is* a good reason to park cores on this machine — see §4 — but it is not this one.

### 1.3 What it gets right

The **strategy** is sound, and our own data argues for it more strongly than the document
does. If an active GPU costs the cores ~18 W whatever the GPU is actually doing, then
budget spent on an idle domain is budget burned. The direct evidence:

> Cyberpunk 2077, 1080p Medium, XeSS Balanced — **PL1 25 W: 48.01 fps. PL1 35 W: 48.16
> fps.** Ten watts bought 0.3%.

Two further things it has right by accident:

- Capping GPU `max_freq` **downward** is a different proposition from the `min_freq` peg
  this project already proved inert. Lowering a DVFS ceiling is a request the driver can
  honour; raising a floor is a request something below it can ignore. Worth testing.
- Its instinct that the two tiles should be budgeted separately is correct even if the
  named mechanism is dead. `scaling_max_freq` and `gt0/freq0/max_freq` may reach the same
  end by a different route.

---

## 2. The real lever set

Everything below was read off this machine on 2026-09-22.

| Lever | Path | Status |
|---|---|---|
| Package PL1 | `intel-rapl-mmio:0/constraint_0_power_limit_uw` | **Shipped and verified.** 8–35 W; 35 is the measured ceiling, not the declared 25 |
| Package PL2 | `intel-rapl-mmio:0/constraint_1_power_limit_uw` | Reads 60 W, never touched. `constraint_1_max_power_uw` reads **0** — unset, not "no power". Do not clamp a slider to it |
| Package PL4 | `constraint_2_power_limit_uw`, 175 W | A microsecond current ceiling. Not a thermal budget; leave alone |
| CPU rail cap | `intel-rapl:0:0` | **Disabled domain. Unproven — gate M9 on it** |
| GPU rail cap | `intel-rapl:0:1` | **Disabled domain. Unproven** |
| CPU frequency cap | `cpu*/cpufreq/scaling_max_freq`, per core | Known-good. This is what the `min_perf_pct` trap entry points at |
| GPU frequency cap | `card1/device/tile0/gt0/freq0/max_freq` (644) | Untested, plausible. `gt1` is the media GT |
| Core parking | `cpu*/online` | Available, with one hard exception below |
| EPP / governor | per-core `energy_performance_preference` | **PPD owns these — do not write them** (ADR 0005). `governor=performance` is near-no-op under HWP anyway |

### CPU topology, measured

No SMT: 16 CPUs, 16 distinct `core_id`s.

| Cluster | CPUs | `core_id` | Max MHz | `cpu_capacity` | PMU |
|---|---|---|---|---|---|
| P | **0–3** | 0, 4, 8, 12 | 4700 (cpu1: 4800) | 1005–1024 | `cpu_core` |
| E | **4–11** | 16–23 | 3700 | 701 | `cpu_atom` |
| LP-E | **12–15** | 32–35 | 3300 | 625 | `cpu_atom` |

**`cpu0` has no `online` file and can never be parked.** Any UI must show it as
permanently on rather than offering a toggle that fails.

The kernel's `cpu_core`/`cpu_atom` masks only split P from Atom; separating E from LP-E
needs `core_id` ≥ 32 or `cpu_capacity`/`cpuinfo_max_freq`. Resolve at runtime — this is
the same discipline as resolving hwmon by `name`, and the numbering is not a promise.

### GPU frequency nodes, measured

```
/sys/class/drm/card1/device/tile0/gt0/freq0/     (render/compute)
    act_freq  cur_freq  max_freq(644)  min_freq(644)
    rp0_freq 2500   rpa_freq 2500   rpe_freq 900   rpn_freq 100
    throttle/{reasons,status,reason_pl1,reason_pl2,reason_pl4,reason_thermal,...}
/sys/class/drm/card1/device/tile0/gt1/freq0/     (media)
    rp0_freq 1200   rpe_freq 400   rpn_freq 100
```

`act_freq` reads **0 in RC6**, so it cannot be point-sampled at 1 Hz — burst-sample and
keep the highest, as `usage.rs` already does.

---

## 3. The open question that gates everything

**Does starving one domain give the other anything?**

The measured facts do not yet answer this, and they point slightly against it. The core
clamp is **the same size whether the GPU then draws 4 W or 22 W** — a fixed reservation,
not a mis-allocation. If the reservation is symmetric and unconditional, then capping the
CPU harder frees nothing for the GPU, and Profile A is dead however correctly it is
implemented.

Eliminated as mechanisms already, all by measurement: PL1, PL2, thermal, PROCHOT (EC
`0x3E22` reads 0000), the ring interconnect, `psys`, DPTF (`INT3400` bound, both UUID
lists empty), HWP (`IA32_HWP_REQUEST` unchanged in every phase), and SR-IOV PF mode. The
only live reason on the cores is bit 8 of `MSR_CORE_PERF_LIMIT_REASONS` (`0x64f`), the
electrical/current category — and it is set in the healthy configurations too.

Best remaining hypothesis, unproven: **margin held against combined CPU+GPU current
peaks.** If that is right, the useful question is whether the clamp is *binary* (any
graphics activity triggers the full reservation) or *graduated* (it scales with GPU
demand). Binary points at a policy; graduated points at budget arithmetic — and only
graduated leaves room for a profile to win anything.

`scratchpad/tune-levers-probe.sh` answers this and the three lever questions in one run.
It is M9 Phase 0 and nothing should be built before it reports.

---

## 4. The case for core parking, restated

The original's reason (idle leakage) is wrong. The real one is **scheduler placement**,
and this project already has the evidence:

> Horizon Zero Dawn Remastered reports **CPU FPS 34 against GPU FPS 45** — CPU-bound —
> **while no thread exceeded 50% and the busiest core sat at 47%.**

A latency-bound critical thread is invisible to per-core utilisation. The classic cause on
a hybrid part is the scheduler migrating that thread onto an E-core or an LP-E core, where
it runs at 3.3–3.7 GHz instead of 4.7–4.8 and pays a cache-locality penalty on the way.
Parking the Atom clusters forces the P-cores to take it.

This stands **independently of every power question above**. It costs nothing, it is
reversible, and it is the one lever here whose justification does not depend on the
outcome of §3. It is also testable the right way: frame rate, not sysfs.

---

## 5. Safety note, which is not optional

Core parking and frequency caps need the same **restore-on-exit obligation as the fan**
(ADR 0006): on exit, signal, panic and suspend. A daemon that dies holding cores offline
leaves a machine with missing CPUs and nothing in the UI explaining why — and unlike a
stuck fan, that state **survives a daemon restart**, because nothing re-onlines a CPU
except something that knows it parked it.

This is why M9 carries a new ADR rather than being a straightforward feature.

---

## 6. Corrected reference scripts

**These are for reading, not running.** They exist to show what the original's scripts
should have said. The daemon is the thing that should hold these values, because every
one of them needs read-back verification, a polkit gate, and a restore path — and because
`fw-helperd` re-asserts its own PL1 within seconds and will fight anything written behind
its back.

```bash
# Profile A — GPU-bound (AAA). Spend nothing on cores that are waiting on the GPU.
# CONDITIONAL on Phase 0 question D: if the core clamp is a fixed reservation,
# this frees nothing and is pointless. Do not ship it until that is measured.

PKG=/sys/class/powercap/intel-rapl-mmio:0          # NOT intel-rapl:0
GT=/sys/class/drm/card1/device/tile0/gt0/freq0     # NOT card0/gt_max_freq_mhz

echo 30000000 > $PKG/constraint_0_power_limit_uw   # PL1 30 W
for c in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_max_freq; do
    echo 2800000 > "$c"                            # NOT max_perf_pct
done
echo 2500 > $GT/max_freq                           # leave the GPU its full range
# Park nothing: a GPU-bound title is not helped by fewer cores, and the LP-E cores
# are doing background work that would otherwise wake the compute tile.
```

```bash
# Profile B — CPU-bound (esports, emulation). Get the GPU out of the way.
# The better bet of the two: nothing measured rules it out.

PKG=/sys/class/powercap/intel-rapl-mmio:0
GT=/sys/class/drm/card1/device/tile0/gt0/freq0

echo 35000000 > $PKG/constraint_0_power_limit_uw   # PL1 35 W, the measured ceiling
echo 1200 > $GT/max_freq                           # and min_freq too, or DVFS ignores it
echo 1200 > $GT/min_freq
for c in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_max_freq; do
    echo "$(cat "${c%scaling_max_freq}cpuinfo_max_freq")" > "$c"
done
for i in 4 5 6 7 8 9 10 11 12 13 14 15; do        # park E + LP-E; cpu0-3 are the P-cores
    echo 0 > /sys/devices/system/cpu/cpu$i/online   # cpu0 has no such file
done
```

## 7. Verification, and the rule that outranks the rest

Do not accept a read-back as proof. This project has now been caught three separate times
by a knob that accepts a value, reports it faithfully, and binds nothing — the sysfs
charge limit, `max_power_uw`, and `min_perf_pct`. The GPU clock question was settled not
by sysfs but by **frame rate**: two GPUs supposedly 550 MHz apart were performing
identically, and that arithmetic was what broke the tie.

So the acceptance test for M9 is two Cyberpunk 2077 runs at the same PL1, one plain and
one tuned, compared on the game's own per-frame CSV. If a tune does not move fps, it does
not work, whatever the counters say.
