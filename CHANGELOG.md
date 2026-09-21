# Changelog

## 0.6.3 — 2026-09-21

### Added

- **CPU and GPU now each report utilisation, power and clock.** Power comes from the
  RAPL `core` and `uncore` zones — `uncore` is the iGPU — published as `cpu_watts` and
  `gpu_watts`. The zones are resolved by their `name` rather than by index, for the same
  reason hwmon is: `intel-rapl:0:1` describes where a zone sits in the tree, not what it
  measures.
  - The window's load cards read `cpu load · 1.8 GHz · 3.2 W` and
    `gpu load · 1.9 GHz · 4.2 W`. A GPU throttle reason still displaces the clock, but no
    longer the wattage, so the two cards stay comparable.
  - The Monitor page's power strip draws `package`, `cpu` and `gpu` as three lines.
    Deliberately not stacked: cores plus iGPU is *less* than the package, which also
    carries fabric and the memory controller, and a stacked area would assert an equality
    the hardware does not have.
  - Recorded sessions gain `cpu_w` and `gpu_w` columns. Appended to the header, and rows
    are parsed by name, so older recordings still load.
  - The HUD line carries the figure too: `GPU 93% 1850MHz 9.7W`.

### Fixed

- **The GPU clock vanished from the window at idle.** `act_freq` reads 0 whenever the GT
  is in RC6 at the instant of the read, and on an idle desktop that is most of the time,
  so a once-per-second read dropped the clock at random while the CPU — whose
  `scaling_cur_freq` always answers — kept showing one. The read now takes a short burst
  and keeps the highest, stopping as soon as it catches the GT awake, and a genuinely
  parked GPU renders as `parked` rather than as a missing field.

  `cur_freq` is **not** used as a fallback, though it always answers. It is the DVFS
  *request*: it reads a constant 2500 on this board while `act_freq` sits at 1950 under
  a saturating load. Four separate people on identical hardware reported "my GPU runs at
  2.5 GHz" from tools showing that node — Mission Center, nvtop and turbostat's `GFXMHz`
  among them. Publishing the request as the achieved clock would have made fw-helper the
  fifth.

## 0.6.2 — 2026-09-20

### Fixed

- **A recorded session's `t_s` column repeated and skipped seconds.** It was truncated
  from a monotonic `Instant`, so a few milliseconds of tick jitter either side of a whole
  second became a whole second of error. Measured on the first session recorded through
  the packaged daemon: `0 1 1 3 3 5 5 6 8 8 …` across 48 rows whose `unix_time` column
  advanced by exactly 1 every row. It rounds now, which moves the flip point to the
  half-second — where no sample sits — so it takes accumulated drift rather than jitter to
  produce a repeat.


## 0.6.1 — 2026-09-20

### Fixed

- **GPU load was never recorded by the packaged daemon.** It worked in development and
  nowhere else. Per-client load is read from `/proc/<pid>/fdinfo`, and opening the
  fdinfo of a process owned by someone else goes through
  `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)` — which needs `CAP_SYS_PTRACE`. Being
  root does not supply it: the kernel grants root nothing except through capabilities,
  and `fw-helperd.service` set `CapabilityBoundingSet=` empty, so the daemon ran as
  uid 0 holding **none**. Every GPU client belongs to the desktop user, so every read
  failed with `EACCES` and `gpu_percent` was never published — in the window, the HUD
  line, or a recorded session. `gpu_mhz`, an ordinary sysfs read, arrived every tick and
  made the GPU look present throughout.

  Session-bus development mode runs as the user and reads the user's own processes,
  which is why this only existed once packaged. The unit now grants exactly one
  capability, `CAP_SYS_PTRACE`, read-only and with the bounding set holding it to that.

- **`gpu usage available` was logged beside a permanently blank number.** The capability
  asked which DRM driver was loaded and stopped there, so it could not see the sandbox
  that was blocking it. It now probes the whole path and, when blocked, says which
  capability the unit is missing.

### Changed

- **The Monitor page draws one card per measurement.** Load, CPU package power,
  temperature, fan and memory each get their own container, y-axis and — where one
  exists — the limit that measurement is read against: PL1 over the power trace, Tjmax
  and the battery's critical point over temperature. They share one time axis at the
  foot of the column. Previously the five strips were drawn onto one continuous surface,
  where a dashed threshold could as easily be read as belonging to the strip above it.


## 0.6.0 — 2026-09-04

M8: record what the machine does, and read it back as a graph.

### Added

- **Session recording.** `fw-helperctl record start [name]` / `record stop`, or the
  Record button on the app's new **Monitor** tab. Rows go to
  `/var/lib/fw-helper/sessions/<name>-<UTC stamp>.csv` at 1 Hz, world-readable, with
  columns a superset of `scripts/sustained-perf-test.sh`'s so a game session and a
  benchmark run open in the same spreadsheet. The last 20 sessions are kept and a single
  recording stops itself after 12 hours.

  Recording lives in the **daemon**, not the app. Not a preference: `energy_uj` is 0400
  under the PLATYPUS mitigation, so an unprivileged recorder cannot read package power at
  all — and a recording has to outlive the window being closed, which is the whole point.

- **CPU, GPU and memory load** (`fw_helper_core::usage`), published as a new `Usage`
  D-Bus property and shown in the app, the CLI's `watch`, and the overlay.

  The GPU figure took three attempts and only the third is honest:

  | Source | Verdict |
  |---|---|
  | `gt0/gtidle/idle_residency_ms` | **Wrong.** It is RC6 residency, so "not in RC6" counts as busy — it reported 27–55% on a completely idle machine |
  | `xe` PMU `engine-active-ticks` | **Correct but costly.** Needs `perf_event_open`, which is in systemd's `@debug` group and excluded by our own `SystemCallFilter=@system-service`. Not worth widening the sandbox for |
  | `/proc/<pid>/fdinfo` `drm-cycles-*` | **Shipped.** Accurate, no sandbox change, no dependency, and it names the process using the GPU |

  `drm-total-cycles-*` was verified to be a GT-wide free-running clock, not a per-client
  lifetime, by reading it from three processes of very different ages.

- **The CPU package temperature** (`coretemp`'s `Package id 0`) is now published as
  `cpu-package`. It matters because its critical point is **Tjmax, 100 °C** — the usable
  one. `peci-temp` declares 119.8 °C, above Tjmax, so it can never be drawn as a limit.

- **`fw-helper --overlay`**, a compact readout, and **MangoHud integration**
  (`/usr/share/fw-helper/mangohud/fw-helper.conf`) for numbers inside a fullscreen game.
  See below for why these are two different things.

- **`fw-helperctl hud`** and `/run/fw-helper/hud`: one line of state, rewritten each
  tick. Read by MangoHud's `exec=`, which runs inside the game's frame loop — hence a
  file rather than a D-Bus call per frame.

### Fixed

- **`DeleteSession` on the recording in progress** would have unlinked the file while the
  daemon kept writing to it. That does not fail on Linux: the descriptor keeps addressing
  the now-nameless inode, so the rows go nowhere and `stop` hands back a path that no
  longer exists. It is refused now, and says to stop the recording first. Found by a test
  written to prove something else.

### Notes

- **MangoHud cannot show GPU load on this laptop**, measured with 0.6.9.1 on 2026-09-04:
  its Intel path is i915-only and this board is `xe`, so it logs "no discrete/integrated
  i915 devices found" and disables `gpu_stats`; its `intel_gpu_top` fallback hits the same
  `perf_event_paranoid` wall that ruled out the PMU for us. The shipped config therefore
  leaves `gpu_stats` off and takes the GPU figure from fw-helper instead.

- **No ordinary window can be drawn above a fullscreen game on GNOME/Wayland.** Mutter
  implements no protocol for it. `--overlay` is an ordinary window and behaves like one;
  MangoHud works because it is *not* a window — the Vulkan loader loads it into the game's
  own process and it paints into the frame before presentation.

- **The `/proc` sweep for DRM clients is rationed.** Opening every file descriptor on the
  machine is 7475 files here and measured **145 ms** even in release. It now runs at most
  every 10 s to discover clients, re-reads one descriptor per client in between, and does
  not run at all unless something is watching — 145 ms → **5–11 ms** per sample.

### Unverified

- Recording has **not been exercised against the packaged daemon**. It is gated by
  polkit, which is a system-bus service, so it cannot run in the session-bus development
  mode. The recording logic itself is covered by tests end to end, minus that gate.

## 0.5.2 — 2026-09-01

### Fixed

- **The battery guard's ramp edge pulsed the fan too, and the log blamed the CPU for it.**
  Found while checking the fix below, in the same evening's journal: `52.9 C puts the
  firmware floor at 43/255; moved 0 -> 43` alternating with `... at 0/255; moved 43 -> 0`
  every few seconds — at a **constant** temperature, which no bucket-edge effect can
  produce. The floor was not the firmware floor at all. The pack was charging and sitting
  at 41.9 °C, which is `crit - 8` and therefore the guard's ramp start exactly; its own
  1 °C quantization walked it back and forth across the threshold, and 43 is what the ramp
  returns one step up (`ceil(1/6 × 255)`).

  Hysteresis is now applied to the firmware floor and the battery ramp **separately,
  before they are combined**, each keyed on its own sensor. Composing first and holding
  the result would be a different and wrong thing: the held floor would be released by
  whichever sensor moved first, so the CPU cooling 2 °C could stand down a guard that only
  the battery had triggered.

  The message is split so it names the guard that actually demanded the duty — the old one
  printed the CPU's temperature whatever the cause, which is why a battery guard firing for
  the first time in the project's life read as the fan oscillating for no reason.

### Changed

- **The battery guard fired for the first time**, and it is not a malfunction — it did what
  ADR 0011 designed it to do. It is recorded here because of *when*: while **charging**,
  which is the heat source the 8 °C margin was never sized against. The pack reached
  41.9 °C at 75% on mains, against 37.9 °C previously seen warm-and-not-charging and the
  33.9 °C "peak under five minutes of 16-core load" the margin was actually chosen from.
  No constant has been changed — one sample is not a measurement, and `battery.rs` is
  explicit that a guard firing means either the thresholds are wrong or the situation is
  new. This is the second, and the situation now looks routine rather than exceptional.

- **The fan pulsed on and off at idle, indefinitely.** Observed on the 2026-09-01 boot: a
  machine sitting still, logging `fan: 40.9 C puts the firmware floor at 53/255; moved
  0 -> 54` and `fan: 39.9 C ... moved 54 -> 0` every couple of minutes with nothing
  thermal happening. Two independent causes, both fixed.

  *A sticky outlier in the learned floor table.* `/var/lib/fw-helper/state` held
  `38:0, 40:51, 42:0, 44:0 ... 52:0` — firmware supposedly needing duty 51 at 40 °C and
  silence for the twelve degrees above it, which no ascending-branch curve can produce.
  Floors only ever rise within a bucket, so it could never age out; the earlier instance
  was `62:184` against neighbours of 79, roughly 5200 rpm.

  Corroboration by repetition would not have caught it — whatever produced the sample held
  it for seconds at 1 Hz, so it corroborates itself. The usable signal is that firmware's
  ascending branch is **monotone in temperature**: an observation sitting at least 24 duty
  counts above each of its next three hotter neighbours contradicts them, and the weight of
  evidence is against the lone bucket. The highest of those neighbours stands in. The check
  is at read time and non-destructive, so a hotter bucket relearning restores the cooler one
  without hand-editing the state file. It also settles a standing question: `54:51` has
  `56:66`, `58:74` and `60:79` above it, so it is consistent with a monotone curve and
  **stands as a real observation**.

  The margin matters — the EC's own duty dithers, and the real table reads 153, 158, 166,
  156 across the plateau above 78 °C. Suppressing on any inconsistency would trim those for
  noise, in the band where airflow matters most.

  *No hysteresis at a bucket edge.* Even a clean table is a step function over 2 °C buckets
  read by a sensor quantized to ~1 °C, so a temperature parked on a boundary alternates
  between two duties every tick — `54:0` next to `56:66` is a 68-count swing. `FloorHold`
  applies the same Schmitt trigger `Direction` already uses for rising/falling, with the
  safe asymmetry: a floor **rises the instant** the table says so, and only its release
  waits, until the temperature has fallen a full bucket below where the held floor was last
  justified. It resets when the fan changes hands, so a floor justified in a previous
  session is never inherited.

## 0.5.1 — 2026-09-01

### Fixed

- **The daemon did not start at boot at all, and said nothing about it.** 0.5.0 was the
  first release carrying `After=power-profiles-daemon.service`, added in 0.4.0's boot-race
  work. PPD declares `After=multi-user.target display-manager.target`; we are
  `WantedBy=multi-user.target`. Ordering after PPD therefore orders us after the very
  target that pulls us in, and systemd broke the cycle the way it always does — by
  deleting a job, ours:

  ```
  multi-user.target: Found ordering cycle on fw-helperd.service/start
  Found dependency on power-profiles-daemon.service/start
  Found dependency on multi-user.target/start
  Job fw-helperd.service/start deleted to break ordering cycle
  ```

  The failure is quiet in a way worth naming. The unit reports `enabled` and
  `inactive (dead)` — not `failed` — and `journalctl -u fw-helperd` for that boot is
  empty, because the process never existed to log anything. The only evidence is four
  lines in the *system* journal, attributed to `multi-user.target`. The visible symptom
  was the GUI's banner: `The name org.fwhelper.Daemon1 was not provided by any .service
  files`.

  The ordering is removed, with the reasoning in the unit so it is not re-added, and CI
  now fails on any `After=`/`Before=`/`Requires=`/`Wants=` naming PPD. Nothing is lost:
  0.4.0 already recorded that ordering "narrows the window without closing it", since
  systemd considers a D-Bus-activated unit started before it owns its name. Adoption via
  `NameOwnerChanged` was already the actual fix — this boot is the proof it has to be.

## 0.5.0 — 2026-09-01

### Removed

- **The ADR 0008 workaround is retired, artefacts and all.**
  `fw-helper-enable-charge-control` and the modprobe drop-in are gone from the tree and
  the package, and `install-dev.sh --enable-charge-control` now exits with an explanation
  rather than silently doing nothing.

  **`/etc/modprobe.d/fw-helper.conf` is now deleted on install**, not merely reported.
  Earlier versions printed "safe to remove" and left it there, which is advice rather
  than a fix — and the file is not inert in the harmless sense. It forces
  `cros_charge-control` to bind, and a bound driver produces a
  `charge_control_end_threshold` that accepts a value, reads it back, persists across
  suspend and reboot, and **does not stop charging**. That appearance is the whole reason
  the superseded mechanism survived as long as it did, so leaving the file in place left
  the trap armed for the next person to read that attribute and believe it.

  The removal matches on file content rather than only the path, so a drop-in that
  happens to share the name but is not ours is never touched. It was never a dpkg
  conffile — the package shipped it to `/usr/share`, and only the opt-in step copied it
  into `/etc` — so this is cleaning up after ourselves rather than deleting a user's
  configuration.

  One thing it cannot do: the module parameter **survives until reboot**, so a machine
  that had the drop-in keeps an inert `charge_control_end_threshold` for the rest of the
  session. Both the postinst and `install-dev.sh` say so, because a capability outliving
  the config that enabled it is a trap this project has already been caught by once.

## 0.4.0 — 2026-08-31

### Fixed

- **The daemon no longer loses a boot race with power-profiles-daemon.** PPD is
  D-Bus-activatable, so *our own probe* is what starts it — and a single probe at startup
  turned that timing accident into a permanent verdict. On a busy boot the probe inherited
  D-Bus's ~25 s timeout, we concluded PPD was absent, and wrote `platform_profile`
  directly — ADR 0005's forbidden path — for the whole session. PPD appeared 23 ms after
  we gave up. Measured: 26.8 s to a wrong verdict at boot, 4 ms to the right one on
  restart, and startup blocked for those 27 s.

  The probe is now bounded at 2 s and a miss is no longer final. The daemon watches
  `NameOwnerChanged` on both of PPD's bus names and adopts it whenever it appears,
  releasing it only when *both* names are gone. The unit gains
  `After=power-profiles-daemon.service`, which narrows the window without closing it —
  systemd considers a D-Bus-activated unit started before it owns its name, which is
  precisely why adoption rather than ordering is the fix.

  Two details that would otherwise bite later: the `ActiveProfile` subscription is
  re-armed on adoption with the previous one aborted, so a restarted PPD cannot leave two
  streams applying the same slider move twice; and the proxy, now mutable state, sits
  behind a mutex that is never held across an await.

  No `Wants=` on the unit, deliberately. A machine without PPD is a supported
  configuration, and pulling PPD in would change that machine's behaviour just by
  installing us.

## 0.3.0 — 2026-08-30

Window changes, all four asked for directly.

### Added

- **Watt-hours in the battery row.** A percentage says how full the pack is; watt-hours
  say how much work is left in it, which is the figure that compares against a draw in
  watts. Shown as `58.0 of 72.5 Wh` beside the percentage and state. Read from
  `energy_now`/`energy_full` where a board offers them and from
  `charge_now`/`charge_full` scaled by `voltage_min_design` where it does not — the same
  two-family split the discharge rate already handles, and this machine reports only the
  second. Scaled by *nominal* voltage rather than `voltage_now`, which rises with state
  of charge and sags under load and would make a resting figure drift while nothing was
  happening.
- **Fan speed shown in RPM, alongside the duty rather than instead of it.** Every duty
  the window writes down now carries its speed: each curve point shows `~3840 rpm` beside
  its spinner, and the fan row reads `duty 120/255 (~3840 rpm)`. There is no unit toggle
  — the two answer different questions and neither replaces the other, so both are always
  visible. The spinners and the plot axis stay in duty, which is what a curve stores,
  what the spinners set and what the firmware floor is drawn in.
  `fw_helper_core::fan::rpm_for_duty` does the conversion by interpolating the measured
  table — never by fitting a line, which would put duty 77 near 2925 rpm where it
  actually turns about 2693.

### Changed

- **A profile now says it is a power limit and a fan curve, in the three places you
  would look:** the profile selector's subtitle, the power limit row, and the "Your
  profiles" group, which spells out what saving captures and why the charge limit is
  deliberately left out of it.
- **Transient banners hide themselves after ten seconds.** Only transient ones: a
  disconnected daemon is a standing condition and its banner stays until the condition
  does. A newer message is never cut short by an older message's timer.

### Notes on the RPM conversion

Two details are deliberate and both are tested.

**Stiction is a discontinuity, not a ramp.** Duty 20 measures 0 rpm and duty 30 measures
1107. Interpolating between them would report duty 25 as a fan turning at 554 rpm, and
there is no such state — the fan is either stopped or it has broken free. Anything below
duty 30 reads as stopped.

**The table is flat above duty 200.** Nothing above 200 has been measured; the last
segment climbs at roughly 30 rpm per duty count, and extrapolating that to 255 would
claim about 7450 rpm from a fan whose highest observed reading is 5886. Holding the last
measured value understates the top of the range instead of inventing it, and it is why
the plot axis is labelled in duty: the axis has room for one number, and an RPM axis
would put two gridlines on the same label.

The duty 200 figure — **5803 rpm** — is new, from the four Q7 sustained runs of
2026-08-28 which held the fan pinned there for five minutes apiece. It is the mean of
104 settled samples.

## 0.2.0 — 2026-08-29

Raises this machine's sustained power ceiling from 25 W to the 35 W it actually
honours, adds two profiles that use it, and fixes an enforcement bug that let a
profile's power limit silently not apply.

### The finding most worth passing upstream

**`constraint_0_max_power_uw` is a declaration, not a bound — at least on this board.**

The MMIO RAPL zone declares a 25 W maximum. It does not enforce it. Measured with
`scripts/sustained-perf-test.sh` (five minutes per setpoint, 30 × 10 s windows, package
power from `energy_uj` deltas, throughput from each window's own `bogo-ops`, fan pinned
at duty 200 so cooling is not a variable):

| PL1 setpoint | Sustained draw | Throughput | vs 25 W | Efficiency | Cores |
|---|---|---|---|---|---|
| 25 W | 25.06 W | 27 909 bogo-ops/s | — | 1110 ops/J | 72 °C |
| 30 W | 30.06 W | 30 405 bogo-ops/s | +8.9% | 1008 ops/J | 76 °C |
| 35 W | 35.08 W | 32 338 bogo-ops/s | +15.9% | 919 ops/J | 84 °C |
| 40 W | **35.07 W** | 32 320 bogo-ops/s | +15.8% | 918 ops/J | 84 °C |

Every setpoint held to within 0.2%. Clamping to the declared maximum was costing 15.9%
of the machine.

**35 W is the real ceiling, and it is a power budget rather than a thermal limit.** The
40 W setpoint stayed in the register for all 30 intervals — no drift, no refusal — and
still settled at 35.07 W. It clamped at interval 4, forty seconds in, with the board at
45.9 °C and cores at 78 °C; `package_throttle_count` stayed at 0 across every interval of
every run. Nothing in `/sys` enforces it: the USB-C adapter negotiates 100 W, the `psys`
zone's constraints read 0, and there is no `INT3400` device, so Intel DPTF is not
present. It is firmware or the EC — the same shape as the charge limit in 0.1.0, where
the knob Linux offers is not the one holding the value.

### Added

- **`turbo` (30 W) and `max` (35 W) profiles.** Both sit on the `performance` PPD
  position alongside `performance` itself; the GNOME slider still lands on
  `performance`, and the other two are reached by name. Their curves start earlier and
  climb harder, because above 25 W the curve rather than the budget does the thermal
  work.
- **`scripts/sustained-perf-test.sh`** — sustained load with package power *and* a
  throughput score per interval, so the shape of a run is visible rather than just its
  mean. `--monitor` generates no load and changes nothing, for recording what an
  external benchmark costs. `--stressor` selects a different stress-ng stressor, so the
  instruction mix can be varied.

### Fixed

- **A profile's power limit could silently not apply.** The daemon bounds how often it
  re-asserts PL1 against firmware and says "giving up until it is set again"; it never
  kept that promise. The reset lived only on the poll loop's `apply_profile` paths, and
  the D-Bus paths do not go through them — `set_profile`'s PPD echo is deliberately
  skipped so a profile is not applied twice, and the reset went with it. Observed here:
  the budget was spent one evening, `profile balanced` the next afternoon wrote 20 W and
  verified it, firmware re-derived 25 W moments later, and the machine ran for six hours
  at a budget no profile had asked for with nothing in the log saying so. The budget now
  resets whenever the setpoint changes, on every path.

### Changed

- `PowerLimit::max_watts` treats the zone's declared maximum as a *floor* on the answer
  rather than a ceiling. Hardware declaring more than we measured is still believed.
- `docs/hardware-baseline.md` gains **Q7**, and its earlier claim that "the machine's
  sustained power budget is 25 W" is corrected in place rather than quietly edited away.

### Still not established

That 35 W is sustainable beyond five minutes. Board temperature was still climbing when
the runs ended — 52.9 °C at interval 14, 56.9 °C at interval 30, no plateau — so these
runs show survivability, not steady state. All four also ran with the fan pinned at duty
200; what 35 W costs under a realistic curve is unmeasured.

## 0.1.0 — 2026-08-26

First public preview. Fan curves, sustained power limits, a battery charge limit and
performance profiles for the Framework Laptop 13 (Intel Core Ultra), on Ubuntu.

Everything below was measured on the target machine — board `FRANMJCP07`, BIOS 03.02,
EC `sakura-3.0.2`, Ubuntu 24.04, kernel 7.0. Where something is implemented but not
demonstrated, it says so.

### The finding most worth passing upstream

**On Framework hardware the standard CrOS EC charge-control interface is silently
inert, and forcing it to bind makes that worse rather than better.**

The kernel's `cros_charge-control` driver deliberately refuses to bind on Framework
laptops, because Framework's EC implements a custom charge-control command alongside the
standard one and the custom one can override it. It offers
`probe_with_fwk_charge_control=1` as an escape hatch, conditioned on the user "not going
to use the custom command".

That condition is not satisfiable. The custom command is not a user choice — it is what
the EC firmware runs, whether or not anything configures it. Forcing the binding produces
a `charge_control_end_threshold` attribute that accepts a value, reads it back, survives
suspend and reboot, and **governs nothing**:

| | |
|---|---|
| `charge_control_end_threshold` | 80 |
| Custom EC command reports | `max=100` |
| Result | charged 88% → 93% → 100%, `status=Charging` throughout |

Two independent limits exist and sysfs is wired to the losing one. The driver's refusal
to bind is a correct verdict about this hardware.

fw-helper now drives `EC_CMD_CHARGE_LIMIT_CONTROL` (`0x3E03`) over `/dev/cros_ec`
instead. Verified: charged from below the limit on AC and stopped at exactly 80% —
`status=Not charging`, `current_now=0`, `charge_now` 3 859 000 of `charge_full` 4 821 000.

See `docs/adr/0012-charge-limit-via-custom-ec-command.md`, and
`docs/adr/0008-charge-limit-via-module-parameter.md` for the approach it replaces and why
it failed.

### Works, verified on hardware

- **Fan control** — manual duty or a temperature curve, with a graphical editor. Never
  runs the fan slower than firmware would at the same temperature on its ascending
  branch, and hands control back to the EC on exit, crash, signal, suspend and watchdog
  timeout.
- **Battery charge limit** — verified to actually stop charging (above).
- **Power limits (PL1)** — a 15 W setpoint held 15.02 W sustained, +0.1%.
- **Performance profiles** — layered over power-profiles-daemon rather than replacing it,
  so the GNOME power slider keeps working and stays in sync. Custom profiles in
  `/etc/fw-helper/profiles.d/`.
- **Live telemetry** — temperatures, fan RPM, package and whole-machine power, battery.
- **Capability detection** — every knob reports available, or a reason it is not.
- **Packaging** — `.deb`, systemd unit, D-Bus and polkit policy. `apt remove` returns the
  fan to the EC.

### Known issues

- **The power-limit slider stops at 25 W, and 25 W is not a hardware limit.** It comes
  from `constraint_0_max_power_uw`, which does not bind firmware: with PL1 set to 35 W the
  package drew **31.94 W sustained**. The practical ceiling appears to be ~31 W, reached
  at 87 °C with the EC's fan at its own maximum. Raising the clamp needs an ADR and is not
  in this release.
- **A manually set power limit is discarded when a profile re-applies.** AC/battery
  transitions re-apply a profile, and a profile carries its own PL1, so a hand-set value
  silently reverts with no notification.
- **`fw-helperd` can lose a boot race with power-profiles-daemon.** PPD is D-Bus
  activatable, so our own probe triggers its activation; on a busy boot that took 26.9 s
  against a ~25 s timeout, and the daemon then wrote `platform_profile` directly — the
  path ADR 0005 forbids — for the whole session. Restarting the service fixes it until the
  next boot.
- **Fan floor observations only ever rise**, so a single anomalous sample is sticky and
  nothing undoes it. Errs loud rather than quiet.
- **Whether PL1 governs below 15 W is untested.** 15 W is verified; lower setpoints are
  accepted but have not been shown to hold under load.
- **PL2 is untouched** — its `max_power_uw` reads 0, which is "unset", not "no power".
- **The curve's control sensor is not configurable** — it is always `peci-temp`.
- **The panic path is implemented and unit-tested but has never fired live.**
- **Undervolting is impossible on this hardware** and deliberately absent — the
  Plundervolt mitigation locks the MSR. See ADR 0007.

### Notes for anyone testing this

- The charge limit needs no opt-in step any more. If you set up an earlier build,
  `/etc/modprobe.d/fw-helper.conf` is now inert and can be removed.
- `scripts/q2-charge-limit-efficacy.sh` will tell you whether a charge limit actually
  works on *your* machine. It refuses to run on battery, or when the battery is already
  above the limit — both conditions under which a broken limit looks healthy.
- Read `docs/hardware-baseline.md` before assuming anything about this board. Four of six
  starting assumptions in this project turned out to be wrong.
