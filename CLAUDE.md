# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

`fw-helper` — firmware control for the **Framework Laptop 13 Pro** on Ubuntu: fan curves, power
limits, battery charge limit, performance profiles. Same product idea as
[G-Helper](https://github.com/seerge/g-helper) (ASUS/Windows), sharing **no code** with it.

Target machine: Framework Laptop 13 Pro, Intel Core Ultra X7 358H, board `FRANMJCP07`,
BIOS 03.02, EC `sakura-3.0.2`, Ubuntu 24.04, kernel 7.0.

## Current state

| Milestone | Status |
|---|---|
| M0 — baseline & architecture | complete: 10 ADRs, all 6 hardware questions answered empirically |
| M1a — hardware layer | complete, verified on hardware |
| M1b — daemon + D-Bus | complete, verified unprivileged against a root daemon |
| M2 — battery charge limit | **complete, and charging verified to stop** (2026-08-26): ADR 0008's sysfs mechanism was inert, so the limit now goes through Framework's custom EC command `0x3E03` over `/dev/cros_ec` (ADR 0012). Halted at exactly 80% on AC from below the limit, `current_now=0` |
| M3 — fan control | **complete**: all six ADR 0006 safety points and the curve engine verified on hardware |
| M4 — power limits | PL1 control complete and verified (15 W setpoint → 15.02 W sustained) |
| M5 — profiles | complete: PPD delegation, user profiles, save/delete, AC/battery switching |
| M6 — GUI | **complete**: profile, save/delete, power limit, charge limit, fan release, auto-switching, and the fan curve editor in a two-column adaptive window |
| M7 — packaging | **complete**: install, GNOME app-grid launch and `apt remove` (fan back to the EC, `pwm1_enable=2`) all verified on hardware |
| M8 — recording & monitoring | **complete and verified on hardware** (2026-09-20): a session recorded against the packaged daemon, 48 rows with GPU load and attribution on every one. Two defects found doing it, both fixed — GPU load was **published by no packaged daemon** at all (uid 0 with an empty capability set cannot read another user's `fdinfo`; see traps), and `t_s` was truncated rather than rounded. The Monitor page now draws one card per measurement. Extended 2026-09-21 (0.6.3/0.6.4): CPU and GPU each report utilisation, power and **achieved** clock — per-rail watts from RAPL `core`/`uncore`, a busy-weighted CPU clock, and a `clock (achieved)` strip drawing the GPU's requested clock beside its real one. Only `gpu_watts` is confirmed on hardware so far |
| M9 — two game profiles + benchmarking | **levers built and verified on hardware (2026-09-22); the profiles themselves are not.** Core parking (3 levels) and GPU frequency capping ship through CLI, D-Bus and GUI, with ADR 0014 restore-on-everything. `game` (25 W, parks nothing) and `retro` (35 W, parks LP-E) ship as built-ins. Still to come: Phase 0, which decides whether the GPU cap is worth keeping, and Phase 5 — the first time anything measures whether parking actually helps. The circulating blueprint for this laptop was checked path by path (2026-09-22): three of its four mechanisms do not exist here. See `docs/framework_gaming_profile.md` and M9 in `docs/plan.md` |

Read `docs/plan.md` for milestones and `docs/hardware-baseline.md` for what the board
actually exposes. **Do not re-derive hardware facts — they are measured and recorded.**

### Resume here

**Newest (2026-09-26): the GPU trades clock against CPU clock** - see the "GPU clock is traded against CPU clock" trap. Quiet reaches a sustained **2200 MHz** GPU because EPP `power` slows the cores; an all-core CPU cap reproduces it but made CP2077 and RE4 stutter. Phase E (Atom-only caps) freed **no** GPU clock - the P-core absorbed it - and the main-thread gain it showed instead **did not survive HZD** (CPU FPS 32 -> 27). Every CPU-clock lever tried is now closed for games. What remains open is Phase 5 below: parking, on Shadow of the Tomb Raider. **Check a game's Steam launch options before benchmarking it** - see the `taskset` trap. Before that session:

**Start with M9 Phase 0, then the two profiles.** Session of 2026-09-22 planned two game
profiles and **built the levers they need**: core parking at three levels and GPU
frequency capping, through core, daemon, D-Bus, CLI (`fw-helperctl tune`) and a Tuning
group in the GUI, with ADR 0014 restore-on-everything. **Verified on hardware through
the packaged daemon**, including the one that matters: after `pkill -9` holding
`p-only` and a 1200 MHz cap, `ExecStopPost` re-onlined 12 cores and lifted the cap in
the **same second**. Two defects were found doing it and both are now traps below.
`game` and `retro` now ship as built-ins - `game` is PL1 **25 W** and parks nothing
(the measured optimum: CP2077 scores 48.01 fps there against 48.16 at 35 W), `retro` is
35 W and parks **LP-E only**. **Neither sets a GPU cap**, and Phase 0 says keep it that
way: the cap **works** (1950 -> 1200 MHz exactly) but buys the CPU only **+4.9%** for a
38% GPU clock sacrifice, and **nothing at all** when the GPU is idle - which is the
emulation case `retro` exists for. **Phase 0 is done** (2026-09-22, see
`docs/framework_gaming_profile.md` 3.3). Parking is worth **+2.2%** single-thread
throughput and +4.7% clock from budget concentration - and separately, E1 caught a
single hot thread running **100% on an E-core**, which is the placement pathology the
whole `retro` argument rests on, observed directly. **Phase 5 is still the real test**:
Shadow of the Tomb Raider's split CPU/GPU frame rates, because a parking win should move
the CPU number and leave the GPU number alone. Read
`docs/framework_gaming_profile.md` (corrected lever set, measured topology, the issue #263
assessment) then M9 in `docs/plan.md`, and run:

```
sudo systemctl stop fw-helperd && sudo ./scratchpad/tune-levers-probe.sh 2>&1 | tee ~/tune-probe.log
```

**Must be on mains.** Three questions, ~10 min: does `gt0/freq0/max_freq` genuinely cap the
GPU; does capping it give the CPU anything; and is core parking a placement tool or a power
one.

**The honest framing that shapes M9, and it is the opposite of what the name suggests.**
There is almost no withheld GPU performance here - CP2077 at PL1 **25 W scores 48.01 fps
and 35 W scores 48.16**. So `game` is about *not wasting budget* (25 W, cooler, ~156 rpm
quieter, same fps), not about an unlock. The headroom is in **single-thread**: HZD reports
**CPU FPS 34 against GPU FPS 45 while no thread exceeded 50%** - a hot thread on a slow
core. `retro` parks the Atom clusters to force it onto a 4.8 GHz P-core. Three park levels,
because RPCS3 is heavily multithreaded and would likely *lose* at P-cores-only.

**Topology, measured 2026-09-22** (no SMT): P-cores **0-3** (4700/4800 MHz, `cpu_core`),
E-cores **4-11** (3700), LP-E **12-15** (3300, `core_id` 32-35). **`cpu0` has no `online`
file and can never be parked.** The GPU is **`card1`**, not card0, and runs `xe` - the
i915-era `card0/gt_max_freq_mhz` path every forum script writes **does not exist here**,
and those scripts guard it with `if [ -f ]`, so they silently do nothing.

**Benchmark instrument: Shadow of the Tomb Raider**, installed, built-in benchmark, and it
**reports CPU and GPU frame rates separately** - so a parking change shows as the CPU
number moving while the GPU number does not. CP2077 for GPU-bound verdicts, HZD as the
negative control that should refuse to move.

**Framework issue #263** claims the EC's 80 W PL4 clamps the GPU to 1900 MHz. Assessed
2026-09-22: **real observation, wrong diagnosis** - see `framework_gaming_profile.md` §3.2.
Their central argument (throttling at PL4 while package power is 13-17 W) is void because
PL4 is a microsecond ceiling invisible in a 1 Hz average. But note it punctures our `psys`
elimination: we dismissed psys because the RAPL zone is disabled, while the EC programs
**`PSYSPL2` at 75-89 W** directly. Fourth time the knob Linux offers is not the one holding
the value. Also: sysfs `peak_power` reads **175 W** while the EC reports PL4 75-80 W.

---

Previous session ended 2026-09-21. **M0-M8 complete. Shipped 0.6.3 and 0.6.4.** The session
was mostly a hardware investigation conducted in public on the Framework forum and
Reddit, and it ended by **retiring a "finding" this file had recorded as fact**.

**The GPU had no ceiling. The number everyone was comparing was the wrong sysfs node.**

`cur_freq` is the DVFS *request* and reads a constant **2500** on this board; `act_freq`
is what happened and reads **1850-1950** under a saturating load on mains. Four people on
identical hardware reported "2.5 GHz" from tools reading the former - Mission Center,
nvtop, and **turbostat's `GFXMHz`**, so Intel's own tool does it too. Confirmed
independently by a correspondent on kernel **7.3-rc3** posting `cur=2500 act=1900-2000`,
which also killed the kernel theory: 7.0 and 7.3-rc3 give the same clock, so **do not
spend a Secure Boot detour on a mainline kernel**. Intel specifies 2500 as *Graphics Max
Dynamic Frequency* at an **80 W** Maximum Turbo Power; this is a ~35-38 W part.

**What actually caught it was frame rate, not sysfs** - their FurMark did 1383 frames in
33 s at 1646x1069 and ours beat it at 1920x1080, so two GPUs supposedly 550 MHz apart were
performing identically. Physical cross-checks outrank instrument readings; that is the
transferable lesson, and it was the **user** who spotted it after this file had already
been updated twice with the wrong conclusion.

Eliminated along the way, each measured: SR-IOV PF mode (`xe.max_vfs=0` genuinely
disables it - `mode: none` - no change), PL4 (it means "GPU busy"), GPU demand, Mesa
version and vendor, graphics API, and the kernel.

**The one thing still unexplained, and it is worth picking up first.**

An active GPU **caps the cores at ~11 W / ~2100 MHz**, down from 29.27 W and
3831-3951 MHz, and **the clamp is the same size whether the GPU then draws 4 W or 22 W**.
It is a fixed reservation, not a mis-allocation.

Do **not** reason from the package total - an earlier version of this section did and
invented a second phenomenon out of it. Package = clamped CPU + whatever the GPU asks
for, so it falls under a light GPU load (18.48 W) and rises under a heavy one (34.48 W,
at PL1) purely as arithmetic. One effect, not two.

Measured out, all of them: PL1, PL2, thermal, PROCHOT (EC `0x3E22` reads 0000), the ring
interconnect, `psys` (RAPL zone disabled, limits 0 — **but see #263: the EC programs `PSYSPL2` at 75-89 W directly, so this elimination used the wrong instrument and is not settled**), DPTF (`INT3400` bound but `current_uuid` and
`available_uuids` both **empty**, so no policy loaded), and HWP (`IA32_HWP_REQUEST` is
`0x3505`, max 53 of a highest-performance 53, **unchanged in every phase** - nothing asks
for less, so the silicon is refusing).

**New instrument for this**: the CPU *does* publish a throttle reason, in
**`MSR_CORE_PERF_LIMIT_REASONS` at `0x64f`** - `0x690` is not implemented on this part.
Needs `msr-tools` and the `msr` module, both present. **Bit 8 is what `xe` calls `pl4`**,
calibrated on-machine against the driver's own text rather than taken from a summary. It
is the only live reason on the cores under load - but it is set in the *healthy*
configurations too, so it does not by itself explain the clamp.

Best remaining hypothesis, unproven: margin held against combined CPU+GPU current peaks.
The next test is whether the clamp is **binary or graduated** - does one trivial GPU
client (`vkcube`, ~1 W) trigger the full clamp, or does it scale with GPU activity?
Binary points at a policy triggered by "graphics active"; graduated points at budget
arithmetic. Script written: `scratchpad/gpu-sweep.sh` (close FurMark first - it aborts if
running, because a stray instance silently invalidated one run's baseline).

**What shipped, and what has NOT been verified.**

0.6.3 added per-rail power (`cpu_w`/`gpu_w`, from RAPL `core` and `uncore`, resolved by
name not index) and fixed the GPU clock vanishing at idle - `act_freq` reads 0 in RC6, so
a once-per-second read dropped it at random. It now bursts and keeps the highest, and a
parked GT renders as `parked`. 0.6.4 added a **"clock (achieved)"** Monitor strip and a
true CPU clock: `cpu_mhz` was the flat mean over all sixteen cores, which reads **1528
MHz with one core pegged at 4288**. `cpu_mhz_busy` weights by time executed
(turbostat `Bzy_MHz`) and is `None` for an idle interval.

**`gpu_watts` is confirmed live** - the packaged 0.6.3 HUD read `GPU 2% 0.1W`. Everything
else is fixture-tested only. **Run `scratchpad/turbostat-crosscheck.sh` with
`vkmark --run-forever` up**: `CorWatt`, `GFXWatt` and `Bzy_MHz` are exactly our three new
numbers, from Intel's own tool reading the same counters by a different route. That is
one command away and is the only thing between "passes 277 tests" and "verified".

Also never done: **Mission Center** was going to be installed to watch it report 2.50 GHz
beside an `act_freq` of 1950 (the flatpak download timed out on mobile data; `flathub` is
a *system* remote, so `--user` needs the remote adding first). Cosmetic now.

**Measurement traps this session cost time on** - all in the harness, not the hardware:

- **`vkmark -p immediate` exits on Wayland here.** The Wayland surface offers only
  `MAILBOX` and `FIFO`; immediate exists on the X11 surfaces, which is where the usual
  advice comes from. vkmark does not fall back, it prints `Selected present mode
  Immediate is not supported` and quits. Use **`-p mailbox`** - unthrottled, where fifo
  would vsync-cap the load. Its `apiVersion has value of 0` line is a harmless
  validation-layer warning from vkmark 2017.08, not the failure; read past it.
- **`vkmark`'s heaviest scene is `effect2d:kernel=edge`**, a full-screen convolution.
  Windowed, `shading` gave act median **900 MHz** against effect2d's **1300** and
  desktop's **1450**. The default scene list is mostly light.
- **An occluded `vkcube`/`vkmark` window renders nothing.** Mutter stops sending frame
  callbacks. It looks alive and draws 3.5 W instead of 7.2.
- **`vkmark` defaults to an 800x600 window** and only reaches ~72% GPU occupancy, so it
  is a light load, not a saturating one. `--fullscreen -p immediate --run-forever`.
- **`sudo` strips `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`**, so a GUI load launched from a
  root script never starts. Use `runuser -u $SUDO_USER -- env ...`, and never send its
  stderr to `/dev/null`.
- **mawk has no `and()`** - that is gawk. A bit-decoding column came back silently blank
  for a whole run. Decode in python.
- **A leftover FurMark invalidated a "cpu only" baseline**, because the script only
  *started* it for the last phase and never checked whether it was already running.

**Still open from previous sessions** (2, 4 and 5 below are unchanged and still worth
doing; 1 and 3 are now closed or moot):

**Cross-check the recorder against the trusted instrument.** Record a session while
`sudo ./scripts/sustained-perf-test.sh --monitor` runs over the same window. They must
agree on watts and temperatures.

**The descent test - still the highest-value unproven claim.** Draw a curve reaching duty
0 by 55 C, heat with `stress-ng`, and listen on the way **down** - firmware holds duty
50-90 to 44.9 C, so ours should be silent where firmware would not be (ADR 0011).

**The fan pulse from the third cause, still unverified.** *Charging, pack warm.* Needs the
pack near 41.9 C, which so far has only happened while charging. **A long FurMark run on
mains while charging is the closest thing yet to a way to force it** - the pack reached
36.9 C during this session's runs. Record it, and log `battery_temp@b` against time for
the guard's margin.

Q7 replication through the recorder (PL1 25/30/35/40 W) is now **moot for GPU work** -
25 W and 35 W are indistinguishable because the GPU is at its 1950 MHz ceiling and fully
occupied. Still meaningful for CPU-bound loads.

**Open defects, in severity order:**

- **The battery guard has now FIRED, while charging, and the margin looks too tight.**
  2026-09-01 ~22:55 on mains at 75%: the pack reached **41.9 C**, which is `crit - 8` and
  the ramp start exactly, and the guard held the fan at duty 43 while the CPU sat at
  52.9 C needing nothing. It behaved as designed. What it says is that the situation the
  margin was sized for is now **routine**: 41.9 C charging against 37.9 C seen warm and
  not charging, against the **33.9 C** five-minute 16-core peak the 8 C was chosen from.
  Charging heat finally has a number, and it is the highest yet. **No constant has been
  changed on one sample** - measure a full charge cycle first, `battery_temp@b` against
  time, and find the actual peak before touching `RAMP_BELOW_CRIT_C`.

- **The battery guard was sized against CPU heat, and charging is a different source.**
  Its ramp starts at crit - 8 C = 41.9 C, a margin chosen because the pack peaked at
  **33.9 C** under five minutes of 16-core load (2026-08-21, not charging). Observed
  2026-08-30 while merely warm from a game and *not* charging: **37.9 C** - already above
  that "peak under full load" and 4 C from the ramp. Charging heat has never been
  measured, and the charge rate has not either: the limit holds at 80% so nothing has been
  sampled crossing it. The curve also follows `peci-temp` alone, so charger heat cannot
  move the fan except through this guard, which has never fired. Measure before changing
  any constant - `battery.rs` says outright that a guard firing often means either the
  thresholds are wrong or the situation is new, and this would be the second.
- **A one-off floor anomaly is permanent, and one of them is now audible.** Floors only
  ever rise within a bucket, so a single bad sample sticks forever. The 2026-09-01 table
  reads `38:0, 40:51, 42:0, 44:0 ... 52:0` - a duty of 51 at 40 C with silence either side,
  which no ascending-branch curve can produce. Its consequence is a **fan that pulses at
  idle**: `peci-temp` dithers 39.9 <-> 40.9 C, the 40 bucket demands 53, and the journal
  logs `moved 0 -> 54` / `moved 54 -> 0` every couple of minutes, indefinitely. The earlier
  instance was `62:184` against neighbours of 79, roughly 5200 rpm.

  Two separate causes, and the fix addresses both:

  - *No outlier rejection.* Corroboration by repetition is not enough - whatever produced
    `40:51` held for seconds, so it would have corroborated itself. The usable signal is
    that firmware's ascending branch is **monotone in temperature**, so a bucket
    contradicted by several consecutive hotter ones is the outlier. Suppression is at read
    time and non-destructive, so a hotter bucket relearning restores the cooler one.
  - *No hysteresis at a bucket edge.* Even a clean monotone table flaps where the sensor
    dithers across a boundary - `54:0` next to `56:66` is a 0 <-> 68 oscillation at 1 Hz.
    Raise immediately, lower only after the temperature has fallen clear of the boundary,
    the same Schmitt trigger [`Direction`] already uses for rising/falling.

  This also settles the standing question about `54:51`: its hotter neighbours are `56:66`,
  `58:74`, `60:79`, all higher, so it is consistent with a monotone curve and **stands as a
  real observation**. Only `40:51` is rejected.

**Verified on hardware and no longer in doubt:**

- **GPU load reads correctly, and near zero at idle** (2026-09-04). 4.1% on an idle
  desktop, attributed to `firefox-bin`, with the media engine at 0.6% while video was
  playing - against the 27-55% the rejected `gtidle` source reported on the same idle
  machine. Under load it tracked to 41%. This is the measurement that chose the source.
  **Read the caveat**: that run was a *session-bus* daemon, running as the user. The
  packaged daemon published `gpu_percent` **not once** - see the `CapEff` trap - so what
  2026-09-04 verified was the parsing, not the deployed path.
- **GPU load reaches a recorded session, through the packaged daemon** (2026-09-20).
  `CapEff` is `0000000000080000` - `CAP_SYS_PTRACE` and nothing else - and the HUD line
  reads `GPU 13% | PL1 20W | ...` where it had no GPU field at all an hour earlier. Idle
  desktop 12-15%, attributed (`code` at 5.8%). This is the deployed path, which the
  2026-09-04 measurement was not.
- **The `CAP_SYS_PTRACE` grant survives a cold boot** (2026-09-20, the reboot that ended
  that session). Three minutes in, `CapEff` and `CapAmb` both read
  `0000000000080000` and the HUD line carried `GPU 7%`. Every earlier confirmation had
  followed a `systemctl restart` in a running session; the unit being started by PID 1 in
  early boot, with the ambient set applied there, had never been exercised. It is now.
- **`gpu_watts` reaches the HUD from the packaged daemon** (2026-09-21). `GPU 2% 0.1W` from
  installed 0.6.3, so the RAPL `uncore` rail is readable through the unit's sandbox with no
  change to it. The other new figures — `cpu_watts`, `cpu_mhz_busy` — are fixture-tested
  only; `scripts`-adjacent `scratchpad/turbostat-crosscheck.sh` checks all three against
  turbostat's `CorWatt`/`GFXWatt`/`Bzy_MHz` in one run.
- **MangoHud loads and parses the shipped config**, confirmed by running `vkcube` under
  it: `parsing config: .../fw-helper.conf`. The same run is what revealed MangoHud
  disables `gpu_stats` entirely on this board.
- **The HUD line is published every tick** and carries what MangoHud cannot see:
  `GPU 1% | PL1 15W | fan 0rpm 0% fw | quiet | pack 30C`.
- **The charge limit stops charging** (above). The mechanism, the write path through
  polkit and D-Bus, and the efficacy test all check out.
- **The packaged stack comes up clean from cold**, serving all five capabilities and
  re-applying the charge and power limits.
- **A curve drawn in the GUI drives a real fan**, end to end: at 35.9 C, where firmware
  would have the fan off, a hand-drawn curve held 3389 rpm.
- **PPD adoption works on a cold boot** (2026-09-01, 0.5.1). The probe missed, startup did
  not block, and adoption landed one second later: `PPD did not answer within 2s` at
  13:58:17, `listening on org.fwhelper.Daemon1` the same second, `PPD appeared at
  org.freedesktop.UPower.PowerProfiles; adopting it` at 13:58:18. `grep 'ordering cycle'`
  over the boot is empty and the unit is `active (running)`. The 0.5.1 unit fix holds.
  Caveat on the message: `probe()` tries both bus names sequentially, each bounded by
  `PROBE_TIMEOUT`, so the real worst case is **2 x 2 s** - the log said "within 2s" while
  the timestamps said 4.
- **The ADR 0008 leftovers are gone from a real machine.** After installing over the old
  layout: `/etc/modprobe.d/fw-helper.conf` absent, `probe_with_fwk_charge_control=N`, and
  `charge_control_end_threshold` **does not exist** on BAT1 - while the daemon still serves
  `charge limit available` at 85%. The capability rests on ADR 0012's EC path alone.
- **The charge limit re-apply works against the EC path** - and the EC does **not** persist
  it. See the trap table: the limit came back at 100% and only the daemon's re-apply from
  `/var/lib/fw-helper/state` restored 85%.

**Housekeeping:** `Cargo.lock` is gitignored. For a workspace shipping binaries that is
arguably wrong - the `libc` dependency added for ADR 0012 is not captured anywhere in
version control. Not changed unilaterally.

**Testing discipline, which this project keeps proving the hard way.** Roughly a dozen
defects across two sessions were invisible to unit tests and appeared only on hardware or in
the real UI. They fall into two families:

- *A constant or model chosen by reasoning rather than measurement* — the EC's percent
  quantization, its 20 °C of curve hysteresis, thresholds set from a 76.8 °C peak when the
  real one is 92.8 °C.
- *Plumbing that only fails outside the happy path* — the interactive polkit branch had
  never once executed because every earlier test ran as root; `systemctl enable --now` does
  not restart, so a fix sat unused on disk through three test rounds; zbus caches properties,
  which only a long-lived client can reveal.

**Verify the thing you are testing is the thing you built.** The daemon logs its own binary
age at startup for this reason. A GUI smoke test also passed while never building a window,
because an instance was already running and GTK is single-instance — kill any instance first
and treat "still alive when the timeout fires" as the pass.

**Fault injection**, never set in production:

```bash
FW_HELPERD_DEBUG_WEDGE_AFTER=15   # blocks every tokio worker; proves the watchdog
FW_HELPERD_DEBUG_CEILING_C=55     # lowers the ceiling into reach; can only ever lower it
FW_HELPER_DEBUG_WIDGETS=1         # GUI: traces control signals and command results
```

**Open, and deliberately not done:**

- The **panic path** is implemented and unit-tested but has never been triggered live.
- Floor observations only ever **rise** within a bucket and now persist, so a one-off
  anomaly is sticky. Errs loud, costing quiet rather than safety.
- The **battery guard has never fired** and is sized so it should not (ADR 0011).
- **PL2 is untouched**; its `max_power_uw` reads 0.
- The curve's **sensor is not configurable** — `control_temp()` picks `peci-temp`.
- `/etc/sudoers.d/fw-helper-dev` grants passwordless `install-dev.sh`. It is scoped to a
  script in a writable directory, so treat it as standing root and remove it when done.

**Running hardware tests:** hand the user the command prefixed with `!` and `tee` the output
to a file — terminal output does not always reach the transcript.

## Layout

```
crates/
  fw-helper-core/     hardware logic. ZERO dependencies, enforced in CI (ADR 0010)
  fw-helper-client/   D-Bus proxy + decoded Snapshot, shared by CLI and GUI
  fw-helperd/         root daemon, owns all hardware access
  fw-helperctl/       CLI; prefers D-Bus, falls back to direct sysfs
  fw-helper-gui/      libadwaita window (binary: `fw-helper`), unprivileged
data/                 D-Bus policy, polkit policy, systemd unit, modprobe drop-in
scripts/              fw-probe.sh, q6-pl1-load-test.sh, install-dev.sh
```

## Commands

```bash
cargo test --all                          # no hardware, no root, no network
cargo clippy --all-targets -- -D warnings # CI gate
cargo fmt --all                           # CI gate
cargo build --release --all               # ALWAYS build release too, see traps

sudo ./scripts/install-dev.sh             # D-Bus + polkit policy, CLI shim on PATH
sudo ./scripts/install-dev.sh --uninstall

sudo sh -c './target/debug/fw-helperd >/tmp/fw-helperd.log 2>&1 &'
sudo pkill -x fw-helperd
fw-helperctl status | watch [secs] | charge-limit N
fw-helperctl fan 180 | fan 0 | fan auto    # duty 0 or 30-255, clamped up to the firmware floor
fw-helperctl fan curve | fan curve 55:0,70:65,85:120   # follow a temp->duty curve
fw-helperctl power-limit 15               # sustained CPU watts; ~32s to take effect
fw-helperctl profile | profile quiet      # quiet|balanced|performance|turbo|max; moves the GNOME slider
fw-helperctl record                       # what is recording, and what has been recorded
fw-helperctl record start "a name" | record stop | record rm NAME
fw-helperctl hud                          # one status line; what MangoHud's exec= reads
/etc/fw-helper/profiles.d/*.conf          # user profiles; see data/example-profile.conf
./target/debug/fw-helper                  # the GUI
./target/debug/fw-helper --overlay        # compact readout; an ORDINARY window (see traps)

./scripts/fw-probe.sh                     # read-only hardware survey
sudo ./scripts/fw-probe.sh --write-test   # writes and restores; read it first
sudo ./scripts/q6-pl1-load-test.sh        # PL1 efficacy; also M4's regression test
sudo ./scripts/sustained-perf-test.sh --pl1 35 --fan 200   # 5 min under load, power+score per 10 s
sudo ./scripts/sustained-perf-test.sh --monitor            # log only; for benchmarking a game
```

`FW_HELPERD_SESSION_BUS=1` runs daemon and clients on the session bus — development only,
avoids needing root and an installed policy.

## Hard rules

**Never leave the fan in a state nobody is managing.** Once `pwm1_enable=1` the EC stops
managing it and holds the last duty **forever** — through a crash, a deadlock, a suspend.
Stuck-high is merely loud; stuck-low looks identical from outside and is silent by
definition. Every path taking manual control must restore `pwm1_enable=2` on exit, signal,
panic and suspend. ADR 0006, non-negotiable, and `kill -9` recovery is a release gate.

Note what the danger *is*, since ADR 0011 sharpened it: the CPU throttles at Tjmax (100 °C)
and protects itself, so a fan held too low costs performance rather than hardware. What has
no protection of its own is the **battery** (crit 49.9 °C) and the board/DDR sensors
(~87 °C). A user choosing quiet is making a trade, not a mistake — but a *daemon* that dies
holding the fan made the choice for them, which is the thing all of ADR 0006 exists to
prevent.

**Every hardware write follows the same pattern**, established in M2:
1. polkit check first, per action, failing **closed**
2. validate range before checking support, so a typo reports as a typo
3. write, then **read back and verify** — a silent override is the expected failure here
4. persist and re-apply on resume, because firmware resets things
5. errors name the fix, not the symptom

**Write methods take `&self`, never `&mut self`.** With `&mut self` zbus holds the interface
write lock for the whole call, so one pending polkit prompt stalls telemetry for every
client. Put mutable state behind a mutex and never hold it across an await.

**`fw-helper-core` stays dependency-free.** std only. External crates belong in the daemon,
client, and GUI. CI fails the build if core gains a dependency.

**Never write hardware paths directly.** Everything goes through `Sysfs`, which carries a
filesystem root so fixtures replace hardware (ADR 0004).

**Capabilities must explain themselves.** `Cap::Yes` or `Cap::No(reason)` where the reason
tells the user how to fix it. Never a dead control with no explanation.

## Traps

All of these cost real time once. Do not rediscover them.

| Trap | Reality |
|---|---|
| A **verified** PL1 write still does not stick | Read back 25 W, was 33 W seconds later — above the advertised `max_power_uw`, so that field does not bind firmware either. Switching `platform_profile` makes firmware re-derive PL1 asynchronously. Re-assert on a timer; an immediate read-back cannot see it |
| Setting PPD **echoes back** | Our own `ActiveProfile` write emits a change signal indistinguishable from the user moving the GNOME slider. Mark what you set, or you apply everything twice |
| `constraint_1_max_power_uw` = **0** | Unset, not "no power allowed" — same trap as `temp*_max` = -273150. Clamping a slider to `max_power_uw` is right for PL1 (25 W) and silently zeroes PL2. Validate first |
| **Root cannot overwrite your file in `/tmp`** | `fs.protected_regular=2` blocks root `O_CREAT`ing a file owned by another user in a sticky world-writable dir. Test scripts run as both users across a session; put their data outside `/tmp` |
| `intel-rapl:0` `long_term` = **200 W** | Meaningless; its own `max_power_uw` is 25 W. Use **`intel-rapl-mmio:0`**. Clamp any UI to `max_power_uw` |
| `peak_power` = 175 W | PL4, a microsecond current ceiling. Not a thermal budget |
| `temp*_max` = **-273150** | Unset (0 K). Only `temp*_crit` is usable, validated to 0–150 °C first |
| `fan1_target` stays `0` under manual control | Read `fan1_input` for actual RPM |
| hwmon indices | Not stable across boots. Resolve by `name` (`cros_ec`) |
| `energy_uj` is `0400` | PLATYPUS/CVE-2020-8694. Root only; republishing is rate-limited and quantized (ADR 0009) |
| Energy counter wraps | Every ~2.9 h at 25 W. Single wrap is correctable; multi-wrap and suspend are not — discard, never interpolate |
| PL1 averages over ~32 s | Any power measurement must span longer or it reads turbo as steady state |
| Charge control absent by default | Driver refuses to bind on Framework by design. Needs `probe_with_fwk_charge_control=1` (ADR 0008) |
| Undervolting | **Impossible.** Plundervolt mitigation locks the MSR. Do not add a disabled control (ADR 0007) |
| **polkit `AllowUserInteraction` hangs forever** | When no authentication agent can service the caller — any process without `XDG_SESSION_ID`. Check without interaction first, then bound the interactive call |
| `pwm1` write while `pwm1_enable=2` | **`EOPNOTSUPP`**, not silently ignored. The duty cannot be pre-loaded before taking control, so the takeover window is real — keep the mode switch and first duty write adjacent |
| Fan duty read-back ≠ what you wrote | The EC stores whole percent: write 180, read 181. Verify with `DUTY_TOLERANCE`, not equality. `pwm1` is also zeroed a few seconds *after* release, so it never tells you who owns the fan — read `pwm1_enable` |
| `PrepareForSleep` does **not** wait for you | It is a notification, not a request for permission. Without a logind **delay inhibitor lock**, a pre-suspend write races the suspend. Also: `rtcwake -m mem` writes `/sys/power/state` directly and never emits the signal at all, so it cannot test any of this |
| Handing the fan back to the EC **reduces** airflow | Firmware's curve tops out near 3100 rpm; manual reaches ~5200. Releasing is a last resort that defers to firmware's *whole* thermal protection, not a cooling escalation. Demand full duty first |
| **The EC's fan curve is hysteretic** | At 61.9 °C firmware runs duty **0** heating and **92** cooling, and holds the fan on down to 44.9 °C. "Never quieter than firmware" is meaningless without naming a branch. Only the ascending branch says what a temperature needs (ADR 0011) |
| Temperature direction needs **hysteresis of its own** | `peci-temp` is quantized to ~1 °C, so a cooldown reads as long runs of identical values. Deriving rising/falling from consecutive samples treats those as "steady" — count steady as rising and you record the whole descending branch. Carrying direction through plateaus is necessary and **not sufficient**: the sensor also dithers, and one 1 °C blip flipped the direction back to rising for the rest of a descent. A measured cooldown, 49.9 → 38.9 °C, held 17 falls, 104 steady samples and **7 upward blips**. Needs a real hysteresis band on the flip, not just plateau carry — `fw_helper_core::Direction` |
| The CPU **protects itself** at Tjmax | `coretemp` crit = 100 °C on every core. A constrained fan costs performance, not hardware. `peci-temp` crit reads 119.8 °C, *above* Tjmax, so it is not a usable limit. What has no protection is the **battery** (crit 49.9 °C) |
| The machine reaches **92.8 °C** in normal use | Not 76.8 °C — that was one M0 PL1 test. Two thresholds were set from the lower figure and both sat below normal operation |
| Fan duty→RPM is **concave** | A line through the high points (120/160/181) predicts 1343 rpm at duty 0 and puts 2925 rpm at duty 77; the measured answer is ~85. Fitting a line would set the firmware floor *below* firmware. Interpolate the measured table |
| Fan stiction is between duty 20 and 30 | Duty 20 = 0 rpm, duty 30 = 1107 rpm. A duty of 1–29 is a stopped fan, not a slow one. Refuse it; do not accept and ignore it |
| **zbus does not run on the tokio runtime** | With default features zbus 5 uses its own `async-io` executor. Blocking every tokio worker leaves D-Bus answering normally with stale telemetry, so "the daemon is wedged" is not all-or-nothing. Never infer daemon health from the interface responding |
| `cat > "$file"` **follows symlinks** | An older `install-dev.sh` left `/usr/local/bin/fw-helperctl` as a symlink into `target/release/`. The newer one wrote the shim through it, overwriting the real binary, which then exec'd itself forever at 100% CPU — and clobbered cargo's hardlinked artifact so it would not rebuild. `rm -f` before writing, always |
| **zbus proxies cache properties** | A property with no change signal is fetched once and frozen for the proxy's life. The daemon signals only `Telemetry` and `CriticalTemperatures`, so a long-lived client saw a stale profile list and a stale power limit. The CLI cannot show this — it builds a fresh proxy per run. Mark properties `emits_changed_signal = "false"` or emit the signal |
| An "active profile" derived from **PPD alone** is wrong | PPD has three positions and any number of profiles can share one, so a user profile reports back as whichever built-in shares its axis — and a client that trusts the report moves its selection there. Report what was applied, while PPD still agrees |
| **zbus handlers are not on the tokio runtime** | `tokio::time::timeout` in an interface method panics with "there is no reactor running" and takes the connection's executor thread down. Hand timer work to a captured `Handle`. Cost three test rounds because the path only runs for *unprivileged* callers |
| `systemctl enable --now` does **not** restart | It starts a unit only if it is not already running, so every reinstall after the first leaves the old process serving the old binary while the files on disk look new. Use `enable` + `restart` |
| A long method **stalls the poll loop** | The loop took zbus's interface *write* lock every tick; any method awaiting a polkit prompt holds the read lock meanwhile. A password dialog stopped the heartbeat for 6 s and the fan watchdog took the fan back. Keep published state behind its own mutex and read-lock only |
| `fw-helperctl status` **exits 0 with no daemon** | By design — it falls back to reading sysfs directly, which is useful for a human and useless as a liveness check in a script. Cost one aborted benchmark run: the precheck passed, then the first real command died with `ServiceUnknown`. Use a subcommand that genuinely needs D-Bus (`profile` exits 1), or ask systemd. Note `scratchpad/tune-levers-probe.sh` **stops the daemon on purpose and does not restart it**, so "down" is a normal state to arrive in |
| **Stale binary on PATH** | Bit us twice, both times looking like a broken daemon. `install-dev.sh` now installs a shim resolving the newest build per invocation. Still: build release *and* debug |
| `apt install ./pkg.deb` **silently no-ops** on an unchanged version | Same family as the stale binary, one layer up. Rebuilding the `.deb` after a fix does not change `0.0.1`, so apt reports "already the newest version", installs nothing, and the fix is tested against the old payload. `dpkg -i` reinstalls regardless. **md5sum the installed binary against `target/release/`** rather than trusting the install log |
| A capability can **outlive the config that enables it** | `charge_control_end_threshold` exists whenever the module was *loaded* with `probe_with_fwk_charge_control=1`, including by a drop-in deleted since — the parameter survives until reboot. The postinst read that node and concluded the machine was set up, so it stayed silent about a capability one reboot from vanishing. Test the **persistent config** (`/etc/modprobe.d/fw-helper.conf`), not the runtime symptom |
| Applying a profile **re-takes the fan** | A profile carries a fan curve, so `profile performance` puts the daemon back in control of `pwm1` and undoes a `fan auto` issued before it. Anything needing the EC to own the fan — learning the firmware floor, above all — must order `fan auto` **last**, and must not straddle an AC/battery transition, which re-applies the profile and takes the fan back the same way |
| The daemon **fights** `q6-pl1-load-test.sh` | The script predates the daemon owning PL1. It writes 15 W for its `LIMITED` arm; the daemon re-asserts its own setpoint within seconds (`power limit was 15 W, expected 25 W; re-applied`), so the arm measures the daemon's budget and the script concludes `NO EFFECT ... Cut M4`. It is an artifact — power settling from 30.47 W to 24.95 W *is* PL1 governing. Stop the daemon, or use plain `stress-ng` when all you need is heat |
| A **verified** charge limit that does nothing | `charge_control_end_threshold` accepts 80, reads back 80, persists and re-applies across suspend and reboot — and the EC charges straight through it: 88% → 93%, +282 mAh, `status=Charging` throughout. Every layer M2 tested passed; none of them tested whether charging *stops*. **Read-back is not efficacy.** Fixed in ADR 0012 by driving Framework's custom EC command instead; `scripts/q2-charge-limit-efficacy.sh` is now the check that counts |
| `max_power_uw` is a **declaration, not a bound** | It reads 25 W and this board honours 35 W: setpoints of 30 and 35 W held to within 0.2% for 26 straight intervals, worth +8.9% and +15.9% throughput. Clamping the UI to it cost ~16% of the machine. The real ceiling is 35 W and firmware enforces it invisibly — a 40 W setpoint stays in the register and still draws 35.07 W, cold, with zero throttle events. Same shape as the charge limit: the knob Linux offers is not the one holding the value (Q7) |
| **Two charge limits exist, and sysfs is the wrong one** | This board runs Framework's custom EC charge command *and* the standard CrOS one. They hold independent values: measured with `charge_control_end_threshold` at 80, the custom command reported `max=100` — and 100 is what happened. Forcing `cros_charge-control` to bind with `probe_with_fwk_charge_control=1` produces a working-looking sysfs attribute wired to the losing mechanism. The kernel's refusal to bind was a correct verdict about the hardware, not an inconvenience to route around (ADR 0012) |
| An **opcode from memory** is a coin flip | Looking up `EC_CMD_CHARGE_LIMIT_CONTROL` returned `0x3E07` from one summary and `0x3E03` from another. The real answer is **`0x3E03`**, settled by reading the enum with its neighbours and since corroborated by [CrOS_EC_Python](https://github.com/Steve-Tech/CrOS_EC_Python), an unrelated implementation. A wrong opcode is not a compile error and often not a runtime error either — the EC simply answers a different question. Pin it in a test, and **prefer a real implementation to a summary** — `CrOS_EC_Python` is the clearest catalogue of Framework EC command numbers we have found, and would have skipped the detour |
| A **crash-recovery path must not depend on state the crash destroyed** | The daemon's GPU restore was gated on an in-memory "we capped it" flag — which the fresh process cleaning up after a `SIGKILL` necessarily reads as "nothing capped". Measured 2026-09-22: the cores came back (their loop was ungated) and the GT stayed pinned at **1200 MHz** through the kill, the restart *and* the startup reclaim. Restore paths read **hardware**: `rp0_freq` is the authority for what uncapped means, `cpu*/online` for what is parked (ADR 0014) |
| An **offline CPU loses `cpufreq/` and `topology/`** | So a machine cannot be classified while it is parked. Measured 2026-09-22: parking 12-15 made the next probe merge LP-E into E and report `E x12`, after which `to_park(Lpe)` returned nothing and `read()` said `mixed` — **the level actually in effect reported as parking no cores**, so it could neither be recognised nor undone by name. `cpu*/online` is the only thing that survives. Read the topology once while the machine is whole and cache it (`CoreParking::with_set`); restore before probing at startup, or a daemon restarted onto a parked machine caches the wrong shape for its whole life |
| A **startup reclaim is not `ExecStopPost` parity** | ADR 0014's first implementation restored only at daemon startup and the hardware test appeared to pass — because `Restart=on-failure` happened to bring the daemon back. It is a weaker guarantee: nothing for `systemctl stop` of a wedged process, a masked unit, or a daemon uninstalled between the kill and the next boot. ADR 0006's property comes from **`ExecStopPost=` running on every stop of the unit**. `fw-helper-restore-fan` now re-onlines cores and lifts a GPU cap too, and does **no classification** — it writes 1 to every `cpu*/online` it finds, the one thing still knowable on a machine whose topology has gone. Verified 2026-09-22: all three restores logged in the **same second** as the `kill -9` |
| `RestartSec=5` outlasts a short `sleep` | A crash-path test that checks the machine 3 s after `pkill -9` reads a state the restart has not reached yet. Cost one round of "the restore is broken" that was really "the test looked too early". Either sleep past `RestartSec`, or — better — assert on `ExecStopPost`'s own journal lines, which are emitted immediately |
| A **disconnected** GUI still looks operable | Sensitivity is decided by `sync_controls` from a snapshot, which cannot run with no daemon — so controls keep whatever state they were built with. Cold-started against no daemon, every control accepted input and discarded it, which reads as "the app does nothing" rather than "nothing is installed". Build controls insensitive; gate the groups on connection, and let per-row capability sensitivity sit underneath |
| **A CI gate nobody watches is not a gate** | CI went red on 2026-08-18, the day the GTK4 GUI crate landed, and stayed red for a month across nine pushes — noticed only when a failure email got read. `ubuntu-latest` carries no `libgtk-4-dev`/`libadwaita-1-dev`, so `cargo clippy --all-targets` died in `gtk4-sys`'s build script in **17 seconds**, and every step after it — `cargo test --all`, the D-Bus XML check, the PPD-ordering check, the ADR 0010 dependency check — never ran once. A job that fails in seconds is failing *before* your code, not because of it. Meanwhile `cargo deny` was rejecting the workspace's own crates: the allowlist said `GPL-3.0` and the crates declare `GPL-3.0-only`, a different SPDX identifier, and `wildcards = "deny"` caught our own path dependencies (needs `allow-wildcard-paths` **and** `publish.workspace = true` on each member, since workspace fields are not inherited unless asked for) |
| **XML comments forbid `--`** | Used as an em dash it broke the D-Bus policy; dbus-daemon skipped the file silently and surfaced it as `AccessDenied` much later. Validated in CI now |
| A **cyclic `After=` deletes your unit silently** | `After=power-profiles-daemon.service` looks harmless and cost a whole boot. PPD is `After=multi-user.target`; anything `WantedBy=multi-user.target` that orders after PPD closes a loop, and systemd breaks it by deleting *your* start job. The unit then reads `enabled` / `inactive (dead)` — **not `failed`** — with an empty `journalctl -u`, because the process never existed. The evidence is in the system journal under `multi-user.target`: grep the boot for `ordering cycle`. Never order against a D-Bus-activated service; adopt it at run time |
| MSRV silently picks stale deps | At `rust-version = "1.74"` the resolver chose zbus 3 while 5 existed. **Check what resolved, not just that it resolved** |
| The EC charge limit is **volatile across a reboot** | It is not stored in the EC's own persistent config: measured 2026-09-01, a limit of 85% set before a clean shutdown came back reading **100%**, and only the daemon's startup re-apply from `/var/lib/fw-helper/state` restored it (`charge limit is 100%, expected 85%; re-applying`). So a boot where the daemon does not start is a boot that charges to 100% — which is exactly what the cyclic `After=` produced. Same for PL1: firmware came back at 35 W |
| A floor is **two guards**, and the log named the wrong one | The enforced floor is `max(firmware floor, battery ramp)`, but the message printed the **CPU** temperature whatever the source. So a battery guard firing for the first time ever — pack parked on `crit - 8` while charging, dithering across it — read as `52.9 C puts the firmware floor at 43/255` alternating with `at 0/255`, i.e. a fan oscillating at a constant temperature for no reason. A constant temperature with a changing floor means **the floor came from another sensor** |
| Hysteresis must go **before** the `max`, not after | Holding the composed floor looks equivalent and is not: the held value is then released by whichever sensor moves first, so the CPU cooling 2 °C stands down a battery guard the CPU never triggered. Each guard's hold is keyed on the sensor that justifies it |
| A floor bucket edge makes the fan **flap** | The floor is a step function over 2 °C buckets and `peci-temp` dithers ~1 °C, so a temperature sitting on a boundary alternates between the two buckets' duties every tick — `moved 0 -> 54` / `moved 54 -> 0` forever, audible at idle. A step function read from a dithering sensor needs hysteresis of its own, exactly as `Direction` does. Raise on the instant value, lower only after clearing the boundary |
| **GPU "idle residency" is not idle** | `gt0/gtidle/idle_residency_ms` is **RC6** residency, so "not in RC6" counts as busy and includes powered-but-doing-nothing. It reported **27-55% busy on a completely idle machine**. It is the obvious source and it does not measure this. Use `/proc/<pid>/fdinfo` `drm-cycles-*` |
| `perf_event_open` is blocked by **our own unit** | It lives in systemd's `@debug` syscall group, and `fw-helperd.service` sets `SystemCallFilter=@system-service`, which excludes it. So the `xe` PMU - the textbook way to read GPU busy - costs a sandbox widening. `perf_event_paranoid` is also **4** on this machine, so nothing unprivileged can use it either. Check `systemd-analyze syscall-filter @system-service` before assuming a syscall is available |
| **Root is not enough to read another process's `fdinfo`** | The kernel grants root nothing except through capabilities, and the unit set `CapabilityBoundingSet=` empty — so the packaged daemon ran as uid 0 holding **zero** capabilities. Opening `/proc/<pid>/fdinfo/<fd>` of another uid goes through `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`, which needs `CAP_SYS_PTRACE`; every GPU client belongs to the desktop user, so every read was EACCES and `gpu_percent` was **never published** — window, HUD line and recorded sessions alike. Passed in development only because session-bus mode runs as the user. Fixed with `AmbientCapabilities=CAP_SYS_PTRACE`. Check `/proc/<pid>/status` `CapEff`, not `Uid`, before concluding a root daemon can read something |
| A capability can be **`Yes` beside a permanently blank number** | `gpu usage available` was logged at every startup while nothing was ever measured: the probe asked which DRM driver was loaded and stopped there, and the sandbox that actually blocked it was never part of the question. A capability has to probe the whole path it is promising, sandbox included — `usage::fdinfo_blocked` |
| `drm-total-cycles-*` is a **GT clock**, not a per-client total | Verified across three processes of very different ages: 4846074419573 / 4846078603831 / 4846097985026, differing only by the interval between reads. So it is the denominator. And a process holding one DRM client through several **dup'd** descriptors publishes the full counter set on each - summing them multiplies its usage by its descriptor count, so deduplicate by `drm-client-id` |
| **Unlinking an open file does not fail the writes** | Deleting a session while it is being recorded looked like it would surface as a write error. It does not: the descriptor keeps addressing the now-nameless inode, the rows go nowhere, and `stop` returns a path that no longer exists. `DeleteSession` refuses the active recording for this reason. Corollary: a test that removes a directory to simulate a full disk proves nothing |
| **MangoHud shows no GPU on this laptop** | 0.6.9.1, measured 2026-09-04: its Intel support is **i915-only** and this board is `xe`, so it logs "no discrete/integrated i915 devices found" and *disables `gpu_stats`*; the `intel_gpu_top` fallback then hits the same `perf_event_paranoid` wall. Do not enable `gpu_stats` in the shipped config - fw-helper supplies the figure through `exec=` instead |
| **No window can sit above a fullscreen game on GNOME/Wayland** | Mutter implements no protocol for it (`wlr-layer-shell` is a wlroots thing and `gtk4-layer-shell` is not installed here anyway). This is not something an application can work around. MangoHud is not a counter-example: it is **not a window**: the Vulkan loader loads it into the game's own process and it paints into the frame before presentation, so the compositor never learns an overlay exists. Its layer JSON is in `/usr/share/vulkan/implicit_layer.d/` |
| **GApplication parses argv and rejects what it does not know** | `fw-helper --overlay` died with "Unknown option --overlay" before any of our code ran. Read our own flags from `std::env::args`, then hand GTK only the program name via `run_with_args` |
| Floor observations are **monotone or wrong** | Firmware's ascending-branch duty cannot fall as temperature rises, so `40:51` sitting between `38:0` and `52:0` is not a curiosity — it is proof that one of the two is bad, and the isolated one loses. This internal contradiction is the only outlier detector available; repetition is not one, because whatever produced the bad sample held it for seconds and would corroborate itself |
| `min_perf_pct` **accepts a value it never applies** | Written 75, reads back 75 — and `scaling_min_freq` stays at 400000 on all 16 cores while APERF/MPERF says the P-cores are running at 1738 MHz. Under `intel_pstate` in **active** mode with HWP, the per-core policy is what maps to HWP.MIN, so write `cpu*/cpufreq/scaling_min_freq` instead. Same family as `max_power_uw` and the sysfs charge limit: the knob Linux offers is not the one holding the value |
| `governor=performance` is **nearly a no-op under HWP** | With `intel_pstate` active and HWP enabled it only forces EPP to 0, so if EPP already reads `performance` nothing changes at all. Measured across two Horizon Zero Dawn runs: P-cores 1808 → 1858 MHz, package 18.5 → 18.6 W, average fps 34 → 34. Frequency selection stays with the hardware, which scales on **per-core** utilisation — a workload spread thin across 16 cores never looks busy enough to boost, however much it needs single-thread speed |
| `pl4` in `throttle/reasons` means **"GPU busy"**, not "GPU clamped" | It reads `pl4` on essentially every sample whenever the GPU is loaded and `none` whenever it is idle — measured 2026-09-21 at 25/25 samples in both a GPU-only and a GPU+CPU phase, `none` in the idle and CPU-only phases. **This corrects an earlier reading of the same data**: five runs showing `pl4` while the GPU sat at 1850 MHz were recorded as PL4 *holding* it there, which was correlation. The counterexample this originally rested on — a forum report seeing `pl4` asserted at a full 2500 MHz — is **probably not one**, since every 2500 report turned out to be reading `cur_freq`; corrected 2026-09-22. What still stands is the load/idle correlation, and we have **no confirmed `act_freq` above 2000 with `pl4` asserted**. Framework issue #263 makes the opposite inference from the same data; see `docs/framework_gaming_profile.md` §3.2 for why it does not hold. PL4 is still a microsecond current ceiling that cannot be seen in a 1 Hz power average — just don't infer a frequency limit from it |
| **PL1 binds only below the PL4 ceiling** | Measured with Cyberpunk 2077, which unlike HZD actually asks for the watts: **15 -> 25 W gave +31% fps** (36.65 -> 48.01), **25 -> 35 W gave +0.3%** (48.16). At 15 W the driver reports `pl1` on every sample and the package sits exactly at 15.0 W; at 25 W and 35 W it reports `pl4` on every sample and the package stops at **21.0 / 21.1 W** with the limit unreached. So PL1 is a real lever up to ~21 W and inert above it. **Revisit that 21 W**: the combined CPU+GPU clamp above is a competing explanation for the wall, and a better-supported one than PL4. Five HZD runs at 30/35 W showed a 2.1 W spread and 33 -> 34 fps only because that title never reached the ceiling at all — do not generalise a null result from a workload that was not asking |
| The GPU stalls at ~1900 MHz here, but **that is not the hardware's ceiling** | `rp0_freq`, `rpa_freq` and `max_freq` all read 2500 on `gt0`, constant across all three platform profiles, and nothing measured on this machine has ever exceeded **1950**. But two independent reports on the *same* Framework 13 / Core Ultra X7 358H, same BIOS 03.02, same EC sakura-3.0.2, same kisak Mesa, reach a genuine **2500** — both on kernel **7.2.6**, where this machine runs **7.0**. Neither has tried 7.0, so "7.2.6 and 7.3-rc behave the same" (a claim about the power inversion below) says nothing about the ceiling. **Kernel 7.0's `xe` is the leading suspect**; SR-IOV PF mode and PL4 are both eliminated. Open confound: the 2026-09-21 `vkmark` run was on **battery**, though nothing in sysfs is lowered on DC |
| On **battery** a heavy GPU load hits `pl2`, and it is a real limit | FurMark at 1920x1080 asserts `pl2` on **every** sample on DC and the GPU runs **1300-1500 MHz**; on mains, same benchmark and the same 92-94% busy, `pl2` never appears and it runs **1850-1950** (2026-09-21). So a GPU measurement taken on battery is taken under a power limit that does not show up in any sysfs *limit* field — PL1 still reads 35 W, PL2 60 W, `platform_profile` still `performance`. Unlike `pl4`, which merely means "GPU busy", `pl2` appearing is worth acting on: it says the reading is power-bound. Check `AC online` before trusting any GPU figure |
| The CPU **does** publish a throttle reason — in an MSR, not sysfs | `MSR_CORE_PERF_LIMIT_REASONS` at **`0x64f`** works on this part (`0x690` is not implemented); graphics is `0x6b0`, ring `0x6b1`. Low 16 bits are live status, high 16 a sticky log that `wrmsr 0 0` clears. **Bit 8 is what `xe` calls `pl4`** — calibrated on-machine, not from a summary: on the same rows, gfx bit 8 set <-> sysfs `pl4`, gfx bits clear <-> sysfs `none`. So bit 8 is the electrical/current category (EDP/ICCmax), and it is the **only** live reason on the cores under load — PL1, PL2, thermal and PROCHOT appear in the sticky history and never live. Needs `msr-tools` and the `msr` module, both present. This corrects the standing claim that the package publishes no throttle reason, which is true only of sysfs |
| **The ~11 W core clamp did not reproduce**, and the next row may be wrong | Measured 2026-09-22 on mains at PL1 35 W with `stress-ng --cpu 12` + fullscreen `vkmark`, reading turbostat directly: **`CorWatt` 27.10 W, package 34.26 W** — against the **~11 W** cores and **18.48 W** package the row below records for nominally the same load. Capping the GPU to 1200 MHz then moved `CorWatt` to 28.59 and throughput **+4.9%**, so the clamp is also **graduated**, not the fixed reservation that row claims. An idle GPU gives 37.49 W and +21%, so a clamp is certainly real — its *size* and *rigidity* are what is in doubt. Likeliest difference is GPU load character (effect2d is bandwidth-bound and drew 3.5 W at 1950 MHz; FurMark's ALU load took 21.74 W) — except the row below asserts character does not matter, so that hypothesis contradicts the thing it would explain. **One of the two runs is measuring something other than what it says. Do not cite either number until this is settled**: repeat with FurMark on mains and read `CorWatt` directly rather than inferring cores from a package total, which has already invented one phenomenon in this project |
| An active GPU **caps the cores at ~11 W**, whatever the GPU is actually doing | The cores drop from **29.27 W / 3831-3951 MHz** to **~11 W / ~2100 MHz** the moment the GPU is non-idle, and **the clamp is the same size whether the GPU then draws 4 W or 22 W**. It is a fixed reservation, not a mis-estimate of demand. **Do not read the package total as the effect** — an earlier note here did, and invented a phenomenon: package = clamped CPU + whatever the GPU asks for, so it *falls* under a light GPU load and *rises* under a heavy one purely as arithmetic. One effect, not two. Quantified on this machine, mains, PL1 35 W (2026-09-21), all with `stress-ng --cpu 12`: **alone** 30.83 W / cores 3831-3951 MHz; **+ vkmark** (partial GPU) **18.48 W** / cores 2029-2700; **+ FurMark** (saturating GPU) **34.48 W at PL1** / cores 2004-2162, uncore taking 21.74 W. Note the core column is flat at 10-12 W across both GPU cases: only the GPU's own consumption differs. Corroborated on kernel 7.3-rc3 by a correspondent: FurMark + `stress-ng` 32 W at PL1, `vkmark` + `stress-ng` 30 -> 17 W, real games 20-23 W against a 33 W PL1. Measured 2026-09-21 on battery — `stress-ng --cpu 12` alone **25.31 W** with a top core at **3508 MHz**; add `vkmark` and the package falls to **19.39 W** with the top core at **2056 MHz**, while the GPU barely moves (1850 median). `vkmark` by itself is 11.33 W. Corroborated on identical hardware on AC and kernel 7.2.6 at a larger amplitude: 40 W / 3800 MHz alone, **15-17 W / 1970 MHz** combined. The CPU absorbs the entire reduction. Not PL1 (35 W, never approached), not thermal, not PROCHOT (EC `0x3E22` reads 0000 under load). Mechanism still unknown. **What has been eliminated**, all measured rather than argued: PL1 and PL2 (not live in `MSR_CORE_PERF_LIMIT_REASONS`, and the package sits at half the limit), thermal (not live, temps far below), PROCHOT (not live; EC `0x3E22` reads 0000), the ring interconnect (`0x6b1` live bits zero), `psys` (RAPL zone disabled, limits 0 — **but see #263: the EC programs `PSYSPL2` at 75-89 W directly, so this elimination used the wrong instrument and is not settled**), DPTF (`INT3400` bound but `current_uuid` and `available_uuids` both **empty**, so no policy loaded), and HWP (`IA32_HWP_REQUEST` is `0x3505`, max 53 of a highest-performance 53, **unchanged across every phase** — nothing asks for less, so the silicon is refusing). The only live reason on the cores is **bit 8, the electrical/current category**, and it is set in the healthy configurations too, so it does not discriminate. Best remaining hypothesis, unproven: margin held back against combined CPU+GPU current peaks, which a steady load does not produce and a duty-cycling one does. **Consequence for measurement**: any CPU figure taken while the GPU is partly busy is taken under this clamp, so a "CPU-bound" verdict from a game may be this and not the workload. **Consequence for fw-helper**: it is why PL1 above ~21 W is inert for games, and why a GPU-bound game is **not** being starved. CP2077 runs at 96-97% GPU occupancy drawing 21 W while FurMark reaches 21.7 W on the graphics rail alone — because `busy%` measures occupancy, not intensity, and a game's shaders are far less dense than a power virus's. The GPU is at its 1950 MHz ceiling and fully occupied, so there is no withheld budget for a higher PL1 to release. `cpu_w`/`gpu_w` are the instrument for chasing this: they say which rail the missing watts were not spent on |
| A game's **Steam launch options** can pin it to four cores | HZD carried `taskset -c 0-3 %command%`, left from an old experiment, and it cost a whole afternoon's benchmarks (2026-09-26): all 119 game threads plus wineserver on the P-cores alone, **23 fps against 32** unpinned, package ~15 W, which read as "power stuck at 15 W". Invisible from inside the game and from every sysfs node - `Cpus_allowed_list` in `/proc/<pid>/status` is where it shows, and the pin enters at `reaper`, below Steam's `sh`. Wine names the process `HorizonZeroDawnRemastered.exe` with a backslash path, so `pgrep -f Foo.exe` misses it; use `pgrep Foo`. Incidentally it answers the `p-only` question for HZD: P-cores only is about **-28%** |
| **GPU clock is traded against CPU clock**, continuously - there is no threshold | Measured 2026-09-25, `scratchpad/gpu-cpu-tradeoff.sh` (mains, PL1 35 W, fullscreen vkmark 100% awake + `stress-ng --cpu 4`; log `~/gpu-cpu-tradeoff.log`). Balanced: GPU act p50 **1950**, cores Bzy 1975 MHz, 5219 ops/s. Power-saver: GPU **2200** p50 and p90, cores 1254 MHz, 2993 ops/s (-43%). **EPP alone carries it**: balanced + EPP `power` gives 2150/2200; power-saver + EPP `balance_performance` falls back to 1950. EPB is inert both ways, and every power-limit MSR (0x610, 0x601, 0x65c, PP0/PP1 policy) is identical across profiles. Capping `scaling_max_freq` in balanced traces the curve: none/3800-2400 -> GPU 1950 (caps above ~2 GHz do not bind, the cores are already held there); **2000 -> 2050, CPU -14%; 1600 -> 2100-2150, -30%; 1200 -> 2200, -47%**. Package stays 20-22 W, far under PL1, and cores+GPU watts are not constant - so this is the bit-8 electrical/current limit, not a power budget: core bit 8 goes from 20/20 samples to 6/20 as the cores slow. The exchange rate worsens toward the top (the last 5% of GPU clock costs 16% of CPU). `scaling_max_freq` is not PPD's node, so this is a lever fw-helper can own without ADR 0005 conflict. **Replicated at 8 threads** (`~/gpu-cpu-tradeoff-8t.log`): the GPU clock at each cap is the same - **2000 -> 2050, CPU -11%; 1600 -> 2150, -25%; 1200 -> 2200, -41%** - so the knee does not move with thread count. **A cap beats EPP `power` at the same GPU clock**: at 8 threads cap 1600 and EPP `power` both give GPU 2150, but the cap keeps **+11%** more CPU work (8038 vs 7243 ops/s; +21% at 4 threads) - EPP slows every core, a cap only trims the top. So cap ~1600 is the fastest CPU found that keeps the GPU within ~2% of quiet. Looked like a lead at 4 threads and **did not replicate**: `low-power` platform_profile + EPP `balance_performance` was +14% CPU over balanced at 4 threads and +1% at 8 - noise, drop it. **In real games an all-core cap is a net loss so far** (2026-09-26, by eye, no frame-time log): CP2077 benchmark 47 -> 49 fps at a 1600 cap with visibly worse pacing; RE4 "really jerky" played at the same cap on balanced, PL1 20 W. An all-core cap slows the main thread, whose frame time is what is felt, so average fps is the wrong measure of this lever. Do **not** ship an all-core cap. **Phase E, 2026-09-26: capping only the Atoms gives the GPU nothing** (`~/gpu-cpu-tradeoff-E.log`; one thread pinned to P-core 1, six on cpus 4-15, balanced, PL1 35 W). Atom caps 2000/1600/1200/800 all leave the GPU at **1950**, core watts barely move (8.98 -> 8.43 W) and core bit 8 stays 17-20/20 - because **the P-core refills what the Atoms give up**: main-thread work rises **+14% / +33% / +45%** at Atom caps 1600/1200/800 (1272 -> 1840 ops/s) while background falls -15% / -38% / -61%. The all-core control reproduced D (1600 -> GPU 2150, main -32%). So the GPU gains only when **total** core draw falls, whichever cores it comes from, and there is no subset of cores whose slowing frees GPU clock while the main thread keeps its speed. For GPU-bound titles the trade is the all-core one or nothing. **The by-product is the useful finding**: an Atom cap is a strong *main-thread* lever under a GPU load - +33% at 1200 against parking's measured +2.2% (Phase 0, idle GPU) - and unlike parking it keeps the background threads running. That looked like `retro`'s premise by a better route - **and HZD refutes it** (2026-09-26, `game` profile, PL1 25 W, state proven before and after each run by `scratchpad/bench-state.sh`, log `~/hzd-runs.log`). CPU FPS / 95% / 99%, GPU FPS: **uncapped 32 / 18 / 15, 44**; **Atoms 1200: 27 / 13 / 11, 44**; **Atoms 1200 + LP-E parked: 27 / 13 / 11, 45**. The cap costs **-16% CPU FPS and a third of the 95% low**, and the GPU number does not move, exactly as a CPU-side change should read. HZD's limit is not one hot thread on a P-core: it is 119 threads under vkd3d-proton, and the workers on the E-cores are on the critical path. The synthetic result was real but measured the wrong shape of load. **Do not build an Atom cap.** Parking LP-E on top made no difference either way |
| GPU `min_freq` is **not** a frequency peg — but `max_freq` **is** a real cap | The SteamOS-style "pin the GPU to max" trick does not work here. Writing 2500 to `gt0/freq0/min_freq` is accepted and `cur_freq` duly reads 2500 — the driver is genuinely requesting maximum — while `act_freq` stays at 1850. A DVFS floor is a request; whatever holds this GPU sits below it. **The other direction works exactly**, measured 2026-09-22: `max_freq` 1200 gives `act_freq` a median and max of **1200** under a saturating load, where uncapped is 1950. So lowering the ceiling binds and raising the floor does not — do not generalise from one to the other. `xe` exposes no SLPC or GuC frequency state in debugfs on kernel 7.0, so GuC's enforced limits cannot be read at all |
| `sriov_numvfs = 0` does **not** mean SR-IOV is off | `sriov_info` reads `enabled: yes, mode: SR-IOV PF` on this machine while `sriov_numvfs` is 0. Zero VFs means nothing to arbitrate *between*, so per-VF power budgeting cannot be a mechanism — but PF mode itself is active, and GuC manages power differently under it. Read `sriov_info`, not the VF count, before concluding SR-IOV is irrelevant — and note it lives in **restricted debugfs** (`/sys/kernel/debug/dri/*/sriov_info`), not next to the `sriov_*` attributes in sysfs. `xe.max_vfs=0` on the kernel cmdline **does** switch it off (2026-09-20: `enabled: no, mode: none`, `totalvfs` 0) and the 1850 MHz ceiling did **not** move — see the GPU ceiling note for how strong that is |
| GPU `act_freq` is an **instantaneous** sample, and a synthetic load never reaches the ceiling | `act_freq` reads **0** whenever the GT is in RC6 at the moment of the read, so a 1 Hz poll of a bursty load reports mostly zeros and cannot be averaged or turned into a duty cycle. Worse, the obvious load is the wrong one: sampled at 50 Hz under `stress-ng --gpu`, `act_freq` spread across **950-1850 MHz with only 2 of 400 samples at 1850** and 70 genuine zeros — it is light and bursty and never asks for maximum clocks, so a peak read off it says nothing about a ceiling. `vkcube` is worse still: occlude its window and Mutter stops sending frame callbacks, so it renders **nothing** while still looking alive (package 3.5 W against 7.2 W for `stress-ng`, and 4.3-6.3 W when it was genuinely visible). Only a title that saturates the GPU — CP2077 reaches 96-97% busy — can probe the frequency ceiling |
| A game can be **title-limited, not machine-limited** | Cyberpunk 2077 at PL1 **15 W** scores 36.65 fps; Horizon Zero Dawn Remastered at **35 W** scores 34 — same machine, both 1080p Medium with a balanced upscaler. CP2077 reaches 96-97% GPU busy and responds properly to power; HZD never exceeded 72% GPU busy or 50% on any core while reporting itself CPU-bound. Before attributing a frame rate to firmware, check the number against a second title |
| A game's **own instrumentation outranks `top`** | Horizon Zero Dawn reports CPU FPS 34 against GPU FPS 45 — CPU-bound — while no thread exceeded 50% and the busiest core sat at 47%. Both are true: the engine measures CPU *frame time* along the critical path, which counts blocking, and utilisation measures execution. A latency-bound CPU limit is invisible to per-core or per-thread load. The tell is arithmetic — ~190 ms of CPU time per frame across 16 cores producing a 29 ms frame is ~40% parallel efficiency, the signature of vkd3d-proton translation overhead rather than slow silicon |

## Coexistence

power-profiles-daemon is active and owns `platform_profile` + EPP. **Delegate to it over
D-Bus; never write those paths directly** — last-writer-wins against the GNOME power slider
is the worst bug class here (ADR 0005).

## Conventions

- **ADRs are append-only.** Superseding means a new ADR plus a status change on the old one.
  Index in `docs/adr/README.md`.
- **Verify before designing.** M0's value was that four of six starting assumptions were
  wrong. When hardware behaviour is uncertain, write a probe script and measure.
- **State what is verified vs assumed.** The docs distinguish these deliberately, including
  when something is implemented but not yet demonstrated.
- Commit messages: what changed and why, with the measurement if there was one.

## Reference numbers

Measured on the target machine, not estimated:

- Idle: 1.77 W, 43.9 °C, fan 0 rpm
- PL1 25 W → 24.67 W sustained, 76.8 °C, ~3100 rpm (M0, controlled)
- PL1 15 W → 14.68 W sustained, 64.8 °C, ~2925 rpm (M0, controlled)
- PL1 15 W → **15.02 W, +0.1%**, 62.2 °C — driven through the daemon (M4)
- **10 W of power limit buys ~12 °C.** Why ADR 0007 can drop undervolting. Rests on the
  M0 runs; M4's 25 W figure was heat-soaked and does not re-confirm it
- **The real PL1 ceiling is 35 W, not the 25 W `max_power_uw` declares** (Q7, 2026-08-28).
  30 W → 30.06 W sustained, +8.9% throughput; 35 W → 35.08 W, +15.9%. A 40 W setpoint holds
  in the register and still settles at 35.07 W, cold, with zero throttle events — a firmware
  power budget, not a thermal limit. Efficiency falls ~9% per 5 W step
- Tjmax **100 °C** (`coretemp` crit). Peak in ordinary use **92.8 °C**, not 76.8 °C
- **The battery is hottest while charging, not under load.** `battery_temp@b` reached
  **41.9 C** charging at 75% on mains with the CPU idle (2026-09-01) — against 37.9 C warm
  from a game not charging, and 33.9 C at the five-minute 16-core peak. Crit is 49.9 C and
  ADR 0011's guard ramp starts at 41.9 C, so ordinary charging now reaches it
- Duty → RPM is **concave**: 30→1107, 50→1879, 77→2693, 90→3052, 120→3840, 180→5201 rpm.
  Stiction between duty 20 and 30
- **GPU load costs 5-11 ms per sample** and only while something is watching. The naive
  version - every file descriptor on the machine, every tick - was 7475 files and
  **145 ms in release**, which is syscall-bound and cannot be optimised, only rationed
  (full sweep every 10 s, one descriptor per client between sweeps)
- **Tjmax is on `coretemp`'s `Package id 0`, published as `cpu-package`**: crit exactly
  100 °C. `peci-temp` declares 119.85 °C, *above* Tjmax, so only the former can be drawn
  as a limit
- **The EC's curve is hysteretic**, and the descending branch is what M0 recorded. Climbing,
  firmware is silent past 64.8 °C and starts at 66–73 °C. Falling, it holds duty 50–90 all
  the way to 44.9 °C — duty 0 vs 92 at the same 61.9 °C.
  **That descent is where a custom curve wins**, not the "flat top" M0 predicted: measured,
  the built-in curve beats firmware by 13–36 duty counts through 50–60 °C
- **A real game loads this machine to 18.6 W and 63 °C, and nothing moves it** (Horizon Zero
  Dawn Remastered, 1080p Medium + FSR Balanced, 2026-09-20). Five runs across PL1 30/35 W,
  a removed 40 fps cap, `governor=performance` + `hwp_dynamic_boost`, and `min_perf_pct=75`:
  package **18.5–18.7 W**, P-cores **~1850 MHz of 4700**, GPU **1850 MHz of 2500** clamped
  by `pl4`, peci **63 °C of 100**, **zero throttle events**, average **33 → 34 fps**.
  The game's own counters read CPU FPS 34 against GPU FPS 45 — CPU-bound, in a workload
  where no thread exceeded 50%. This is the profile of a **translation-layer** limit
  (vkd3d-proton), not a firmware one, and it is the case where every knob fw-helper owns is
  correctly irrelevant. Useful as a negative control: if a change appears to move these
  numbers, suspect the measurement
- **Cyberpunk 2077 2.31**, 1080p Medium, XeSS SR 2.0 Balanced, no RT, no frame generation
  (2026-09-20). Avg fps / 1% low / 0.1% low, from the game's own per-frame CSV:
  **15 W: 36.65 / 23.34 / 16.65** — package 15.0 W at the limit, `pl1` every sample, GPU
  1184 MHz, one core pegged 63/64 samples at 1108 MHz.
  **25 W: 48.01 / 34.32 / 31.09** — package 21.0 W *under* the limit, `pl4` every sample,
  GPU 1852 MHz, nothing pegged.
  **35 W: 48.16 / 34.68 / 28.48** — package 21.1 W, `pl4`, GPU 1850 MHz. Indistinguishable
  from 25 W.
  **Run gaming at PL1 25 W**: identical fps to 35 W for 10 W less and ~156 rpm less fan.
  The apparent CPU bottleneck at 15 W was the cap starving the cores, not a CPU limit —
  it vanishes entirely at 25 W
- **This machine's GPU stalls at ~1900 MHz of a declared 2500 — but the hardware does 2500.**
  Nothing measured here has ever exceeded 1950: 148 samples at 1850 across two titles, and
  `vkmark` — the first load that genuinely saturates the GPU, 500/500 awake samples at
  11.33 W — peaked at **1900** (2026-09-21, on battery). `gt0` is 12 Xe cores x 8 EU = 96 EUs.
  Four forum/Reddit reports on identical hardware claim a real 2500 — **but every one of them
  is reading `cur_freq`, not `act_freq`** (2026-09-21). Settled by the matched run: FurMark on
  **mains** at **92-94% GPU busy**, the same tool and the same saturation as the screenshot
  claiming 2.50 GHz, gives `act_freq` **1850-1950** while `cur_freq` reads **exactly 2500 on
  every sample** — which is the figure their tool displays. Under FurMark on this machine
  `cur_freq` reads a constant **2500** while `act_freq` reads **1100-1650**; `cur_freq` is the
  DVFS *request*, not the achieved clock. The tell was **frame rate, not sysfs**: their FurMark
  run does 1383 frames in 33 s = 42 fps at 1646x1069, and on mains ours beats it at the larger
  1920x1080 — so *we are faster* while allegedly clocked 550-1100 MHz lower. Two GPUs at the
  same performance are at the same speed. **Treat the whole "everyone else reaches 2500" thread
  as unverified** until someone posts `act_freq`. Eliminated: **power** (4.3-6.3 W with a 2500 MHz floor
  requested still gave only 1950), **PL4** (asserted on a machine reaching 2500),
  **SR-IOV PF mode** (`xe.max_vfs=0` disabled it with no change, 2026-09-20; and a
  correspondent runs a live VF at 2500), and — decisively — **insufficient GPU demand**: the
  clock does not track saturation here. `vkmark` at **72% busy** gave **1900 MHz**, while
  CP2077 at **96-97% busy**, on AC and drawing 21 W of a 25 W limit, gave **1852**. The *more*
  saturated load clocked *lower*, which no demand-driven DVFS story explains, and it clears
  the battery confound too since the CP2077 runs were on mains. **The cleanest external
  comparison** (2026-09-21, screenshot): another Arc B390 (PTL) running FurMark 2.10.2 GL at
  **93% GPU utilisation reads 2.50/2.50 GHz** — *less* saturated than our CP2077 and 650 MHz
  higher, on **older** Mesa (26.2.2 vs our 26.2.3) and a different API (OpenGL, not Vulkan).
  So Mesa version, Mesa vendor and graphics API are all eliminated as well — though see above:
  that screenshot's 2.50 GHz is most likely `cur_freq`, in which case it is not a comparison at
  all. **Mission Center displays "Clock Speed: 2.50 GHz / 2.50 GHz" as current/max**; confirm
  what it sources before citing any screenshot of it as a frequency measurement. **SUPERSEDED 2026-09-25 - see the "GPU clock is traded against CPU clock" trap: 1950 is the GPU's share while the cores run their ~2 GHz, and it reaches a sustained 2200 when they give current back.** Was: SETTLED: ~1950 MHz under a saturating load on mains is simply
  what this GPU does. There is no ceiling and there never was.** Independently replicated by
  a correspondent on kernel **7.3-rc3** posting `cur=2500 act=1900-2000 pl4` — identical to
  this machine on 7.0, which kills the kernel theory outright — and they caught `nvtop`
  reporting 2500 as a third tool showing the request. **Do not reopen this** without an
  `act_freq` figure above 2000 from a saturating load on mains. **Why 2500 was never reachable**: Intel
  specifies it as *Graphics Max Dynamic Frequency* at an **80 W Maximum Turbo Power**. This
  is a ~35-38 W part, so ~1950 is what the silicon does in this envelope — that number was a
  spec for a different power class, not a target. It also explains why 25 W and 35 W give
  identical fps in CP2077: past ~21-25 W, more package budget does not buy GPU clock. A
  correspondent PL1-limited at ~25 W (`reasons` reads `pl1`, package 24.7 W) reaches the same
  ~1950 as this machine unlimited at 35 W (`reasons` reads `pl4`). Also unexplained, and now
  probably irrelevant: `gt0/freq0/power_profile` reads `[base] power_saving`.
  Open question: user documentation suggests 2800-2900 MHz, source unidentified
