# Changelog

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
