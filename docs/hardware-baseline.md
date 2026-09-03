# Hardware baseline

Captured 2026-08-18 from the target machine. Re-generate with `scripts/fw-probe.sh`.

## Machine

| | |
|---|---|
| Vendor / product | Framework — `Laptop 13 Pro (Intel Core Ultra Series 3)` |
| Board | `FRANMJCP07` |
| BIOS | `03.02` |
| CPU | Intel Core Ultra X7 358H, 16 logical CPUs |
| OS | Ubuntu 24.04.4 LTS |
| Kernel | 7.0.0-29-generic |

## Confirmed present

### Embedded controller
`/dev/cros_ec` exists (`crw------- root:root`).
EC firmware **`sakura-3.0.2-cf48815`** (built 2026-05-26), board version 12,
Nuvoton `npcx9m3f`. Loaded modules:
`cros_ec`, `cros_ec_lpcs`, `cros_ec_proto`, `cros_ec_dev`, `cros_ec_chardev`,
`cros_ec_sysfs`, `cros_ec_debugfs`, `cros_ec_hwmon`, `cros_charge_control`,
`cros_kbd_led_backlight`, `leds_cros_ec`, `gpio_cros_ec`.

### Fan + EC thermal — `hwmon11` (`cros_ec`)
Standard hwmon interface, **no `ectool` required**:

> **The index moved.** On 2026-08-21 the same node came up as `hwmon9`, not `hwmon11`.
> This was the predicted instability, now observed rather than assumed — always resolve
> `cros_ec` by its `name` file, never by index.

| Attribute | Value at capture | Notes |
|---|---|---|
| `pwm1_enable` | `2` | 2 = EC automatic, 1 = manual |
| `pwm1` | `0` | 0–255 duty when manual |
| `fan1_input` | `0` | RPM, fan idle |
| `fan1_target` | `0` | |

EC temperature sensors:

| Sensor | Label | Reading |
|---|---|---|
| `temp1` | `local_f75397@4c` | 36.85 °C |
| `temp2` | `cpu_f75303@4d` | 36.85 °C |
| `temp3` | `battery_temp@b` | 30.85 °C |
| `temp4` | `ddr_f75303@4d` | 36.85 °C |
| `temp5` | `peci-temp` | 43.85 °C — CPU package, primary curve input |

#### How manual control actually behaves

Measured 2026-08-21 driving `FanControl` (M3) as root, spinning up and releasing. Three of
these were assumptions before this run, and two of them were wrong.

- **`pwm1` is not writable while the EC owns the fan.** Writing it with `pwm1_enable=2`
  fails with `EOPNOTSUPP` (errno 95) — it is not silently ignored, it is refused. So the
  duty *cannot* be pre-loaded before taking control, and the window between
  `pwm1_enable=1` and the first duty write cannot be closed from userspace. Keep that
  window to adjacent statements.
- **Duty round-trips through whole percent.** The EC stores a percentage, so an 8-bit
  count comes back up to one count away. Verification must use a tolerance, not equality
  — unlike the charge limit, where exact read-back is correct:

  | Requested | 77 | 90 | 100 | 128 | 150 | 180 | 200 | 230 | 255 |
  |---|---|---|---|---|---|---|---|---|---|
  | Observed | 77 | 89 | 99 | 128 | 150 | 181 | 199 | 230 | 255 |

  Every point matches `round(round(d / 2.55) × 2.55)`. Max observed error ±1 count.
- **Under EC control, `pwm1` reports firmware's own duty.** Corrected 2026-08-21: an
  earlier note here said `pwm1` "goes to 0" a few seconds after firmware reclaims the
  fan. It goes to *firmware's current duty*, which is 0 only because the machine was
  idle. Measured under load at 68.8 °C with `pwm1_enable=2`: `pwm1` read **64** and the
  fan turned at 2302 rpm — against a table where duty 65 gives 2296 rpm. It does still
  take a few seconds to stop reflecting the duty *we* last wrote, so it is not a signal
  of *who owns* the fan; read `pwm1_enable` for that.

  **This is worth acting on.** `FirmwareFloor` currently reconstructs firmware's duty by
  inverting an RPM table, composing two measured tables and inheriting the interpolation
  error of both. If `pwm1` can simply be read while the EC owns the fan, the floor can be
  *measured* rather than modelled, and the knee gap closes without needing the learned
  observation mechanism. Confirm across the temperature range before changing working
  safety code.
- **The EC reclaims cleanly and promptly**, confirming Q4 on a second occasion: fan went
  to 0 rpm within 4 s of release, at 41 °C.

#### The EC's own curve: measured, and hysteretic

Read directly from `pwm1` while `pwm1_enable=2` (see above). Measured 2026-08-21.

| Temp | EC duty, **heating** | EC duty, **cooling** |
|---|---|---|
| 44.9 °C | — | 51 |
| 48.9 °C | — | 66 |
| 54.9 °C | **0** | **82** |
| 58.9 °C | **0** | **87** |
| 61.9 °C | **0** | **92** |
| 64.8 °C | **0** | — |
| 66.8 °C | 59 | — |

**The hysteresis is enormous.** At 61.9 °C firmware runs the fan off or at duty 92
depending only on which way the temperature is going, and it does not stop the fan until
below 44.9 °C. Climbing from cold it kept the fan entirely off past 64.8 °C.

The fan-start point is **not a fixed temperature**: 66.8 °C on a 16-core step, 72.8 °C on
a gentler 2-core climb. Note the direction — the *slower* ramp started *later*, which is
the opposite of what response lag predicts, so firmware is likely triggering on something
other than instantaneous `peci-temp`.

Consequences, both acted on in [ADR 0011](adr/0011-quiet-is-a-legitimate-choice.md):

- The `EC_CURVE` points recorded earlier in this document (2020 rpm at 53.9 °C, 2925 at
  64.8 °C) are **descending-branch** measurements, taken under sustained load. They are
  not what firmware does at those temperatures while heating, which is nothing at all.
- "Never quieter than firmware" needs a branch named, or it means nothing.

#### RAPL, driven by the daemon (M4)

Measured 2026-08-21 through `fw-helperctl power-limit`, confirming the daemon reproduces
what Q6 achieved by writing RAPL directly.

| PL1 setpoint | Sustained | Error | CPU |
|---|---|---|---|
| 15 W | **15.02 W** | **+0.1%** | 62.2 °C |
| 25 W | 23.57 W | −5.7% | 84.8 °C |

The 15 W case matches Q6 (14.68 W, 64.8 °C) closely. The 25 W temperature is ~8 °C above
Q6's 76.8 °C, almost certainly heat soak — it was measured after a long session of
repeated load runs — so **the "10 W ≈ 12 °C" figure still rests on Q6's controlled run**,
not on this comparison.

The ramp is a textbook demonstration of why the averaging window matters. Under a **15 W**
limit:

```
t+10s  25.2 W     t+30s  13.7 W
t+20s  24.5 W     t+100..150s  15.02 W steady
```

`constraint_0_time_window_us` is ~32 s. A reading at t+20 shows 24.5 W under a 15 W limit
and looks like the control does nothing.

Two more facts worth having:

- **`constraint_1_max_power_uw` reads `0`.** That is "unset", not "no power permitted" —
  the same trap shape as `temp*_max` reporting -273150. Clamping a UI to `max_power_uw`
  is right for PL1 (25 W) and would silently zero PL2. Validate before trusting it.
- **RAPL survives suspend.** After a 30 s s2idle cycle the daemon logged `power limit
  still 15 W; nothing to re-apply`. Established by reading before writing, the same way
  the charge limit question was settled. One cycle, so the re-apply hook stays.

#### Thermal limits, and what protects what

| Sensor | crit | Self-protecting? |
|---|---|---|
| `coretemp` package + every core | **100.0 °C** (Tjmax) | **Yes** — the CPU throttles |
| `peci-temp` | 119.8 °C | Reports *above* Tjmax; not a usable limit |
| `local_f75397@4c`, `cpu_f75303@4d` | 87.8 °C | No |
| `ddr_f75303@4d` | 86.8 °C | No |
| `battery_temp@b` | **49.9 °C** | **No** — and it is the lowest on the board |

**Battery temperature barely moves under CPU load.** Measured across five minutes of
16-core load with firmware driving the fan, on battery power: `battery_temp` went
**31.9 → 33.9 °C** while `peci-temp` went 40.9 → 78.8 °C. That is 16 °C of headroom below
its 49.9 °C crit. It lags heavily and was still rising during the cooldown. During that
cooldown, with the fan still running hard, it fell to **26.9 °C — below its idle
baseline** — so airflow does cool it. What is *not* measured is a long run with the fan
held low, which is what a user-authored curve will allow — nor, at the time, anything at
all about charging, which turned out to be the hotter case (below).

**Charging is a hotter source than the CPU, and it was measured much later.** The
paragraph above is about CPU load *on battery*, and it was the only battery thermal data
for six weeks. Measured 2026-09-01 while **charging** on mains at 75%, machine otherwise
idle: `battery_temp@b` reached **41.9 °C** — above every figure above, and 8 °C from its
49.9 °C crit. Two earlier readings fill in the shape: **37.9 °C** merely warm from a game
and not charging (2026-08-30), and the **33.9 °C** five-minute 16-core peak above.

So the ordering is the opposite of what was assumed: the pack is hottest when the CPU is
doing nothing and the charger is working. This matters beyond the number, because the fan
curve follows `peci-temp` alone — charger heat cannot move the fan at all except through
the battery guard, and at 41.9 °C the CPU read 52.9 °C and was asking for nothing.

**41.9 °C is exactly ADR 0011's guard ramp start** (`crit - 8`), a margin chosen when
33.9 °C was the known peak. The guard duly fired for the first time in the project's life,
holding the fan at duty 43. It worked; the question it raises is whether a threshold that
ordinary charging reaches is in the right place. **Not yet answered** — one sample is not a
measurement, and no constant has been changed. What is needed is `battery_temp@b` logged
against time across a full charge cycle, to find the actual peak and how long it is held.

**The CPU protects itself at 100 °C.** Constraining the fan costs performance, not
hardware. The components with no protection of their own are the battery above all, then
the board and DDR sensors — nothing currently watches any of them.

**Peak temperature in ordinary use is higher than M0 suggested.** M0's PL1 test recorded
76.8 °C under sustained full load. Measured under ordinary multi-core load with firmware
driving the fan, `peci-temp` reached **92.8 °C**, firmware choosing duty 94/255. Any
threshold set from the 76.8 °C figure is set below normal operation.

#### Duty → RPM, measured

Full sweep 2026-08-21 at ~39 °C, descending from 180 so the fan never had to start
from rest at a duty below stiction, 8 s settling per point. This is the table the
firmware-floor clamp inverts.

| Duty | 0 | 20 | 30 | 40 | 50 | 65 | 77 | 90 | 100 | 120 | 150 | 180 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| RPM | 0 | **0** | 1107 | 1512 | 1879 | 2296 | 2693 | 3052 | 3355 | 3840 | 4551 | 5201 |

Two things matter here and neither was guessable:

- **Stiction sits between 20 and 30.** Duty 20 leaves the fan stopped; duty 30 turns it
  at 1107 rpm. Any duty in 1–29 is a stopped fan wearing a costume, which is why
  `STICTION_DUTY` exists and why a "quiet" setting of 25 must be refused rather than
  accepted and ignored.
- **The curve is concave, not affine.** A linear fit through the three high points
  measured earlier (120/160/181) predicts 1343 rpm at duty 0 and puts 2925 rpm at duty
  77. The real answer is ~85. Extrapolating that fit would have set the floor *below*
  firmware — the one direction that matters. Interpolate within the table; do not fit
  a line to it.

The table is also slightly **optimistic under load**: at 65.8 °C, duty 84 produced
2808 rpm where the table predicts ~2886. Hence the small margin added to every
non-zero floor duty.

Also `hwmon1` (`acpi_fan`) exposes `fan1_input` / `fan1_target` / `power1_input` (read-only view).
`hwmon10` is `coretemp` (per-core die temps).

### Power / performance
- `/sys/firmware/acpi/platform_profile` → `balanced`; choices `low-power balanced performance`
- `scaling_driver` → `intel_pstate`
- EPP choices → `default performance balance_performance balance_power power`
- **power-profiles-daemon is active**, driving *both* `intel_pstate` and `platform_profile`

### RAPL — `/sys/class/powercap/`
`intel-rapl:0` (`package-0`), `enabled=1`, all constraints mode `644` (root-writable):

| Constraint | `intel-rapl:0` limit | `intel-rapl-mmio:0` limit | `max_power_uw` | Window |
|---|---|---|---|---|
| 0 `long_term` (PL1) | **200 W** | **25 W** | **25 W** | 31.98 s |
| 1 `short_term` (PL2) | 60 W | 60 W | 0 | 976 us |
| 2 `peak_power` (PL4) | 175 W | 175 W | 0 | — |

**Read this table carefully — the raw numbers mislead.**

`max_power_uw` is the ceiling *the platform declares* for the constraint, and it is **25 W on
both zones**. The MSR zone's 200 W `long_term` is a limit set *above* the declared maximum,
which is the signature of a constraint that is not constraining anything — firmware parks an
unconstrained value there because real governance happens through MMIO and the EC.

> **Corrected 2026-08-28.** This section used to conclude "the machine's sustained power
> budget is 25 W, not 200 W". The first half of that is wrong. `max_power_uw` is a
> *declaration*, and this board does not enforce it: 30 W and 35 W setpoints are both
> honoured and held to within 0.2%. The real ceiling is **35 W**, and it is enforced
> somewhere below the OS. See **Q7**. The reasoning above is sound about the MSR zone and
> unsound about the number — it inferred an enforced limit from an advertised one, which is
> the same mistake as reading back a charge limit and calling it efficacy.

Three separate misreadings to avoid:

1. **A RAPL limit is a ceiling, not a draw or a target.** It caps a rolling average; it says
   nothing about actual consumption.
2. **200 W = "PL1 effectively disabled"**, not "PL1 is 200 W". Confirmed by `max_power_uw`.
3. **`peak_power` (PL4) is not a thermal number.** It is an instantaneous current-spike
   ceiling protecting the VRM and battery on a microsecond scale, and never becomes sustained
   heat. Note that its time window is empty while PL1's is ~32 s and PL2's is ~1 ms — the
   timescales are what give these numbers their meaning.

Effective envelope: **25 W sustained / 60 W burst**, consistent with a 13" chassis and with
the part's 1.9 GHz base / 4.7 GHz max frequency.

Also present: `intel-rapl:1` (`psys`).

### Telemetry needs root
`energy_uj` is mode `0400` root-only on both zones — this is the mitigation for
**PLATYPUS / CVE-2020-8694**, where RAPL energy readings were used as a side channel to
recover AES keys. Package-power telemetry therefore *cannot* be read by an unprivileged GUI.
This is independent reinforcement for the daemon split in
[ADR 0003](adr/0003-privileged-daemon-split.md): the GUI gets power readings over D-Bus or
not at all.

`max_energy_range_uj` = 262143328850 — the counter wraps, so deltas must handle rollover.

### Sensor caveat
Every `temp*_max` reads `-273150` (0 K — i.e. unset). **Only `temp*_crit` is usable.**
Observed: cpu 87.85 °C, ddr 86.85 °C, local 87.85 °C, battery 49.85 °C, peci 119.85 °C.

### LEDs
`chromeos::kbd_backlight`, `chromeos:multicolor:charging`, `chromeos:white:power`.

## Confirmed absent

- **No `charge_control_end_threshold` on `BAT1`.** `cros_charge_control` is loaded but
  `/sys/class/power_supply/BAT1/extensions/` is **empty** — the driver has not registered
  against the battery. See [open question Q3](#open-questions).
- No `ectool`, `framework_tool`, `ryzenadj`, or `tlp` installed. `powerprofilesctl` is present.
- No discrete GPU (FW13).

## Open questions

These are unresolved by read-only probing and gate specific milestones.

**Q1 — Do RAPL writes actually stick?** *(answered — YES)*
Both zones accept writes and hold them:

```
intel-rapl-mmio:0   STICKS  (25W -> 20W, held 2s)   restored to 25W
intel-rapl:0        STICKS  (200W -> 195W, held 2s) restored to 200W
```

**No lock bit. M4 is viable.** Caveat: this proves writes are not *rejected*; it does not yet
prove the limit *governs* power draw. Effectiveness still needs a load test — see Q6.

**Q2 — `intel-rapl` (MSR) vs `intel-rapl-mmio` — which governs?** *(answered)*
Both declare `max_power_uw` = 25 W, but only the MMIO zone has `long_term` actually *set* to
25 W; the MSR zone parks 200 W above its own declared maximum. **Target `intel-rapl-mmio:0`
for PL1.** Remaining work is confirmation under load, not identification.
*Gates M4.*

**Q3 — Why is charge control not registered?** *(answered)*
None of the original hypotheses. The driver **deliberately refuses to load**:

```
[2.860075] cros-charge-control cros-charge-control.6.auto:
           Framework charge control detected, preventing load
```

Framework's EC implements a *custom* charge control command alongside the standard CrOS EC
one. Both work, but the custom one can override the standard one, and the UEFI setup screen's
battery limit uses the custom one — so upstream declines to load rather than race the
firmware. The supported override is a module parameter, present on this kernel:

```
/sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control   # bool, 0644, = N
```

Resolved in [ADR 0008](adr/0008-charge-limit-via-module-parameter.md): enable the parameter,
use standard sysfs, and treat the UEFI battery limit as off-limits.

**Q4 — Does writing `pwm1` actually move the fan?** *(answered — YES)*

```
baseline:     pwm1_enable=2  rpm=0
manual 160:   rpm=4681  (duty 63%)
restored:     pwm1_enable=2  rpm=0
```

Manual control works **and the EC reclaims cleanly** — the latter is what all of
[ADR 0006](adr/0006-fail-safe-fan-control.md)'s safety machinery depends on. **M3 is viable.**

Implementation note: `fan1_target` stayed `0` while under manual control. Read `fan1_input`
for actual RPM; do not trust `fan1_target` as a feedback signal.
Scale reference: 63% duty ≈ 4681 RPM, useful for curve design.

**Q5 — What is the true sustained limit?** *(answered — and the answer was wrong; see Q7)*
**25 W PL1 / 60 W PL2**, confirmed by `max_power_uw` = 25 W on both zones. The MSR zone's
200 W is not a real limit and the 175 W `peak_power` is a microsecond-scale current ceiling,
not a thermal budget. Remaining work is only to pick sensible per-profile values below 25 W
(e.g. 15 W quiet / 25 W balanced) and verify them under load via `energy_uj` deltas.
*Informs M4; no longer blocking.*

**Q6 — Does PL1 actually govern sustained draw?** *(answered — YES, tightly)*
Measured with `scripts/q6-pl1-load-test.sh` (`stress-ng --cpu 16 --cpu-method matrixprod`,
steady state = mean of the second 30 s of each 60 s sampling run):

| PL1 setpoint | Sustained draw | Error | CPU (peci) | Fan |
|---|---|---|---|---|
| 25 W (stock) | **24.67 W** | −1.3% | 76.8 °C | ~3100 rpm |
| 15 W | **14.68 W** | −2.1% | 64.8 °C | ~2925 rpm |
| idle | 1.77 W | — | 43.9 °C | 0 rpm |

**PL1 is a real control, regulated to within ~2% of setpoint.** Intel Dynamic Tuning is not
arbitrating it away. M4 ships as genuine functionality.

**Q7 — Does `max_power_uw` bind, and where is the real ceiling?** *(answered — it does not
bind, and the ceiling is 35 W)*
Measured 2026-08-28 with `scripts/sustained-perf-test.sh`: 5 minutes of
`stress-ng --cpu 16 --cpu-method matrixprod` per setpoint, sampled in 30 × 10 s windows,
package power from `energy_uj` deltas and a throughput score from each window's own
`bogo-ops`. Fan pinned at duty 200 (~5850 rpm) throughout so cooling is not a variable.
Steady state = mean of intervals 5–30, past PL1's ~32 s averaging window.

| PL1 setpoint | Sustained draw | Score (bogo-ops/s) | vs 25 W | Efficiency | Freq | peci | core | board | Throttle |
|---|---|---|---|---|---|---|---|---|---|
| 25 W | 25.06 W | 27 909 | — | 1110 ops/J | 2267 MHz | 85.8 °C | 72 °C | 48.9 °C | 0 |
| 30 W | 30.06 W | 30 405 | **+8.9%** | 1008 ops/J | 2479 MHz | 85.8 °C | 76 °C | 51.9 °C | 0 |
| 35 W | 35.08 W | 32 338 | **+15.9%** | 919 ops/J | 2632 MHz | 94.8 °C | 84 °C | 55.9 °C | 0 |
| 40 W | **35.07 W** | 32 320 | +15.8% | 918 ops/J | 2630 MHz | 94.8 °C | 84 °C | 56.9 °C | 0 |

**`constraint_0_max_power_uw` = 25 W does not bind.** Setpoints of 30 and 35 W were accepted
and held to within 0.2% for 26 consecutive intervals. Clamping fw-helper to the declared
value was costing **15.9%** of this machine's throughput.

**The real ceiling is 35 W, and it is a power budget rather than a thermal limit.** A 40 W
setpoint stayed in the register for all 30 intervals — no drift, no refusal — and still
settled at 35.07 W, matching the 35 W run to 0.06%. Three things rule out heat:

1. It clamped at **interval 4**, 40 s in, with the board at 45.9 °C and cores at 78 °C. The
   chassis was still cold; heat soak did not arrive until interval 27.
2. `package_throttle_count` stayed **0** across every interval of every run. Cores peaked at
   84 °C against a Tjmax of 100 °C.
3. Twenty-six intervals spanning 35.06–35.10 W is a governed setpoint. Thermal droop wanders.

Nothing in `/sys` exposes what enforces it. The candidates were checked and eliminated: the
USB-C adapter negotiates **20 V × 5 A = 100 W** (65 W spare, not starving); the `psys` zone's
constraints both read `0` (unset); there is no `INT3400` device, so Intel DPTF's platform
driver is not present; and Tjmax never engaged. It is firmware or the EC — the same shape as
the charge limit in [ADR 0012](adr/0012-charge-limit-via-custom-ec-command.md), where the
knob Linux offers is not the one holding the value.

**Returns diminish smoothly and efficiency falls about 9% per 5 W step.** 40% more power
across the range buys 15.9% more work. Whether that trade is worth making is a user choice,
which is why 30 W and 35 W ship as profiles (`turbo`, `max`) rather than as a new default.

*Not yet established:* that 35 W is sustainable beyond five minutes. Board temperature was
still climbing at interval 30 (52.9 °C at interval 14, 56.9 °C at interval 30, no plateau),
so these runs show survivability, not steady state. A 15-minute soak is the outstanding test.
All four runs also used duty 200; what 35 W costs under a *realistic* curve is unmeasured.

Two secondary findings worth carrying into design:

**10 W buys 12 °C.** This is the empirical basis for the profile values in the plan, and it
substantiates the claim in [ADR 0007](adr/0007-no-undervolting.md) that power limiting
delivers the thermal and acoustic outcome people actually want from undervolting.

**The stock EC fan curve has real headroom.** Dropping 12 °C moved the fan only ~7%
(3100 → 2925 rpm), and the machine was already at 2900 rpm at 64.8 °C while sitting at
0 rpm at 43.9 °C. Later observation during M1b narrows the knee further: **0 rpm at 44.9 °C
but ~2020 rpm at 53.9 °C**, so the fan starts somewhere in 45–54 °C and is already at two
thirds of its loaded speed by 54 °C. So the EC curve ramps steeply somewhere between ~45 °C and ~65 °C and then
flattens. A custom curve that is less aggressive in the 55–70 °C band is where M3's audible
benefit lives — see M3 notes.

*Answered. M4 unblocked.*

**Measurement note:** the first sample of the unconstrained run read 29.03 W — above the
25 W limit — because at t+25 s into load the ~32 s PL1 averaging window had not yet closed.
This is why the verdict averages only the second half of each run. Any future power
measurement must respect that window or it will read turbo as steady state.
