# fw-helper — implementation plan

A G-Helper-equivalent for the Framework Laptop 13 on Ubuntu.

**Read first:** [hardware-baseline.md](hardware-baseline.md) for what the board actually
exposes, and [adr/](adr/) for why the architecture is shaped the way it is.

## What this is

One tray application that unifies the scattered firmware knobs on a Framework 13 into named
profiles you can switch with a hotkey — fan curve, power limits, charge limit, performance
profile — the way G-Helper does for ASUS laptops on Windows.

## Scope

**In scope (v1)**

| Feature | Mechanism | Confidence |
|---|---|---|
| Performance profiles | PPD D-Bus, layered ([0005](adr/0005-delegate-to-power-profiles-daemon.md)) | High — verified present |
| Fan curves | `cros_ec` hwmon `pwm1` | High — verified working on hardware |
| Power limits (PL1/PL2) | `intel-rapl-mmio` powercap | High — verified regulating to ±2% |
| Battery charge limit | ~~sysfs, via module param~~ **failed** ([0008](adr/0008-charge-limit-via-module-parameter.md)) | **None** — the EC charges past the limit; needs the custom EC command |
| Live telemetry | hwmon + powercap `energy_uj` | High |
| Keyboard backlight | `/sys/class/leds/chromeos::kbd_backlight` | High |

**Explicitly out of scope**

- **Undervolting** — not possible on this platform ([0007](adr/0007-no-undervolting.md))
- **GPU control** — no dGPU on FW13. FW16 users are served by LACT
- **Peripherals** — G-Helper's mouse/keyboard support is ASUS-specific
- **FW16 LED matrix** — `inputmodule-rs` already does this well
- **Replacing TLP / PPD** — we are a client, not a competitor

## Repository layout

```
fw-helper/
  crates/
    fw-helper-core/     shared types, profile schema, D-Bus interface definitions
    fw-helperd/         privileged daemon  -> /usr/libexec/fw-helperd
    fw-helper-gui/      GTK4 tray + window -> /usr/bin/fw-helper
    fw-helperctl/       CLI over the same D-Bus API
    fw-helper-restore-fan/  crash-path safety binary (ADR 0006)
  data/
    org.fwhelper.Daemon1.conf      D-Bus system bus policy
    org.fwhelper.policy            polkit actions
    fw-helperd.service             systemd unit
  docs/
  scripts/
    fw-probe.sh
```

## Milestones

Each milestone is independently useful and independently shippable. Do not start the GUI
until M1–M4 work from the CLI.

---

### M0 — Baseline and open questions  ✅ mostly done

Establish what the hardware does before designing around it.

- [x] Read-only hardware survey (`scripts/fw-probe.sh`)
- [x] Capture results in `docs/hardware-baseline.md`
- [x] ADRs 0001–0007
- [x] Q2 / Q5 answered from `max_power_uw`: authoritative zone is `intel-rapl-mmio:0`,
      envelope is 25 W PL1 / 60 W PL2
- [x] **Q1 answered — RAPL writes stick.** No firmware lock bit. M4 viable
- [x] **Q4 answered — `pwm1` drives the fan** (0 → 4681 RPM at 63% duty) and the EC reclaims
      control cleanly on release. M3 viable
- [x] **Q3 answered** — driver refuses to load by design; override via module parameter
      ([ADR 0008](adr/0008-charge-limit-via-module-parameter.md)). M2 viable
- [x] **Q6 answered — PL1 governs draw**, regulating to within 2% of setpoint
      (25 W → 24.67 W, 15 W → 14.68 W). 10 W of limit buys 12 °C

**Exit:** reached. Every gating question came back green; no milestone is cut. All four
hardware mechanisms (fan, power, charge, profiles) are verified on the target board before
a line of application code exists.

---

### M1 — Read-only foundation

No hardware writes at all. Prove the plumbing. Split in two so the hardware layer lands
dependency-free and stays that way.

#### M1a — hardware layer, zero dependencies  ✅ complete

Deliberately std-only: no external crates, so it builds and tests anywhere, including CI
with no network, and there is no dependency-resolution risk to debug alongside logic bugs.

- [x] Cargo workspace, `rust-toolchain.toml`, GitHub Actions (fmt, clippy, test)
- [x] `Sysfs` — every path rooted, so fixtures replace hardware
      ([0004](adr/0004-sysfs-first-hardware-access.md))
- [x] `EnergySampler` — wrap correction, multi-wrap and suspend detection, 0.1 W
      quantization ([0009](adr/0009-power-telemetry-rate-limited-and-quantized.md))
- [x] `Capabilities::probe` — every knob resolves to available, or a reason with a fix
- [x] `Monitor` — temps, fan RPM, package power, battery, platform profile
- [x] `fw-helperctl status` / `watch`
- [x] Fixture tests: a synthetic Framework 13 tree, plus a bare-root degradation case
- [x] **Built and tested** on rustc 1.97.1 — 15 tests green, clippy clean under
      `-D warnings`, rustfmt clean
- [x] Cross-checked against `scripts/fw-probe.sh` on real hardware: capabilities, sensor
      labels, critical thresholds and control-sensor selection all agree
- [x] Verified package power under root: `EnergySampler` reads 1.6–1.7 W idle against the
      1.77 W measured independently by `scripts/q6-pl1-load-test.sh`. Two implementations,
      same counter, agreeing

**Exit: reached.**

#### M1b — daemon and D-Bus  ✅ complete

- [x] `fw-helperd` on the system bus as `org.fwhelper.Daemon1` (zbus 5.19)
- [x] `Capabilities`, `Telemetry`, `CriticalTemperatures`, `Version` properties, with
      PropertiesChanged emitted only when the published view actually changes
- [x] 1 Hz poll cap and 0.1 W quantization enforced; no on-demand sampling method
      ([0009](adr/0009-power-telemetry-rate-limited-and-quantized.md))
- [x] logind `PrepareForSleep` → `Monitor::on_resume()`; degrades to the sampler's
      max-gap check if logind is unreachable
- [x] `fw-helperctl` prefers D-Bus, falls back to direct sysfs when the daemon is absent
- [x] D-Bus policy, hardened systemd unit, `scripts/install-dev.sh`
- [x] CI: `cargo deny`, and a gate failing the build if `fw-helper-core` gains a dependency
      ([0010](adr/0010-dependency-boundary.md))
- [x] **Verified as root on the system bus**: `fw-helperctl` running as uid 1000 reads
      package power (3.2 W idle, tracking load live) from a root daemon over D-Bus

**Exit: reached.** The privilege split is real: the daemon opens a counter the client
cannot, and the client renders it with no privileges of its own.

---

### M2 — Battery charge limit  ✅ **complete — charging verified to stop**

> **Corrected 2026-08-26.** This section previously read "✅ complete, verified on
> hardware". It was not. **The charge limit has never stopped charging.** With the
> threshold at 80 the battery charged to 100% (`charge_now` 4 804 000 of `charge_full`
> 4 806 000; `status=Charging` across 120 samples spanning the crossing), and no UEFI
> battery limit is set.
>
> Every box ticked below is accurate as written — and every one of them sits *upstream*
> of the question that matters. Write, read-back, persistence, suspend re-apply and
> reboot re-apply were all verified; whether charging stops was never tested at all.
> **Read-back is not efficacy.** See the Outcome section of
> [ADR 0008](adr/0008-charge-limit-via-module-parameter.md) for why the approach could
> not have worked, and ADR 0012 for what replaces it.
>
> The boxes stay ticked rather than being erased: they record work that was genuinely
> done, and the milestone is not "complete" regardless. The lesson is that a checklist
> can be entirely true and still add up to nothing.
>
> **Rebuilt 2026-08-26.** The limit now goes through Framework's custom EC command
> (`0x3E03` over `/dev/cros_ec`, [ADR 0012](adr/0012-charge-limit-via-custom-ec-command.md)),
> which a hardware probe showed to be the mechanism that governs charging — it reported
> `max=100` while sysfs reported 80. The wire format, byte order and read-modify-write are
> unit-tested against a fake transport, and the D-Bus round-trip is verified.
>
> **Closed 2026-08-26 on the test it named.** The battery charged from below the limit,
> on AC, and stopped at exactly 80%: `capacity=80`, `status=Not charging`, `current_now=0`,
> `charge_now` 3 859 000 of `charge_full` 4 821 000, AC still connected. Repeatable via
> `scripts/q2-charge-limit-efficacy.sh`.

Smallest real feature, lowest risk, immediately useful.

- Ship `/etc/modprobe.d/fw-helper.conf` with `probe_with_fwk_charge_control=1`, then use
  standard `charge_control_end_threshold` ([ADR 0008](adr/0008-charge-limit-via-module-parameter.md))
- Opt-in install step, not a silent postinst — it changes who governs charging
- ~~Read back after write; a persistent mismatch means a UEFI battery limit is fighting us.
  Report that specifically, not as a generic failure~~ — done, and it never fires: this
  firmware leaves the value in place and ignores it, so there is no mismatch to detect
- polkit action `org.fwhelper.set-charge-limit`
- Persist across reboot; re-apply on resume

- [x] `ChargeControl` in core: range check (20–100), read-back verification, errors
      that name the fix rather than describing the symptom
- [x] `SetChargeLimit` D-Bus method behind polkit action `org.fwhelper.set-charge-limit`
      (`auth_admin_keep`, so a slider does not prompt on every movement)
- [x] polkit fails **closed** — an unreachable polkit denies rather than allows
- [x] State persisted to `/var/lib/fw-helper/state`, re-applied at startup and on resume,
      because the threshold survives neither
- [x] `fw-helperctl charge-limit N`, and `ChargeLimit` on the interface
- [x] `--enable-charge-control` as an explicit opt-in, never a side effect of installing
- [x] **Write path verified on hardware** (2026-08-21). `sudo fw-helperctl charge-limit 80`
      returned promptly — the polkit hang is fixed — and the daemon's own read-back
      confirmed it. `charge_control_end_threshold` reads `80`, D-Bus `ChargeLimit` reports
      `80%`, and `/var/lib/fw-helper/state` was created with `charge_limit=80`. No UEFI
      override: `NotApplied` did not fire
- [x] **Module parameter survives a cold boot** (2026-08-21). The drop-in had only ever been
      proven after a live `modprobe -r`/`modprobe`. After a genuine reboot,
      `charge_control_end_threshold` was present at `100` with
      `probe_with_fwk_charge_control=Y`. See [ADR 0008](adr/0008-charge-limit-via-module-parameter.md)
- [x] **Holds across suspend/resume** (2026-08-21). After `systemctl suspend` and wake,
      `charge_control_end_threshold` reads `80`. The daemon logged both
      `resumed from sleep` and `re-applied charge limit 80%`, so the logind signal is
      received, consumed once, and the post-resume write succeeds
- [x] Made that question answerable. `reapply_charge_limit` now reads before writing and
      returns a `Reapply` outcome, so `still 80%; nothing to re-apply` and
      `is 100%, expected 80%; re-applying` are distinct log lines. Covered by the first
      four unit tests in the daemon crate, against a rooted fixture
- [x] **Firmware does not reset the threshold across suspend** (2026-08-21). First suspend
      on the instrumented build logged `resumed from sleep` followed by
      `charge limit still 80%; nothing to re-apply`, against a journal-confirmed
      s2idle cycle 09:35:12–09:35:40. So the resume hook is **insurance, not a
      requirement**, for the charge limit on this machine. One ~28 s s2idle cycle on
      battery is one data point, not a law — but every future resume now adds to it for
      free, and a contradicting line would be conspicuous. Do not assume this generalises
      to M3–M5: the EC has far more reason to reset a fan or power limit than a charge
      threshold, so each knob earns this verdict separately
- [x] **Re-applied after a reboot** (2026-08-21). Confirmed across a journal-verified
      reboot — boot `-1` ended 09:47:24, boot `0` began 09:47:37. The daemon was left down
      for 27 minutes afterwards; `charge_control_end_threshold` read `100` throughout, so
      the value is indeed lost at boot. Starting the daemon at 10:15:06 logged, before any
      client command was issued:

      ```
      persisted charge limit: 80%
      charge limit is 100%, expected 80%; re-applying
      re-applied charge limit 80%
      ```

      sysfs then read `80` and D-Bus reported `80%`. The instrumented read-before-write is
      what makes this decisive: `charge limit still 80%` would have meant the threshold
      survived and the persistence path was never exercised. It did not fire

**Exit: met.** `fw-helperctl charge-limit 80` holds across both a reboot and a
suspend/resume cycle — by re-application at startup in the first case, and, as it turns
out, without needing any intervention in the second.

---

### M3 — Fan control

The highest-value and highest-risk feature. Do not shortcut
[ADR 0006](adr/0006-fail-safe-fan-control.md).

- [x] **The learned firmware floor persists across restarts** (2026-08-21). Written to
  `/var/lib/fw-helper/state` as `fan_floor=54:0,56:0,...`, periodically (60 s) *and* on
  clean shutdown — periodically because `SIGKILL` is a supported way for this daemon to
  end, and everything learned since the last write would go with it. The write goes
  through the `Daemon` rather than the poll loop, because the state file also holds the
  charge limit and two writers would silently drop one. Malformed entries are skipped
  rather than failing the load, since a damaged floor costs some quiet while refusing the
  file would also lose the charge limit.
  **Verified**: 11 observations saved after a heating cycle (firmware silent through
  44–62 °C), `restored 11 firmware fan floor observations` on restart, floor preserved.
  The run's final check landed at 31.9 °C rather than the intended 55 °C, so the
  behavioural payoff at mid temperatures rests on the restored entries plus the earlier
  51.9 °C verification, not on this run
- [x] **Curve engine** (`fw-helper-core/src/curve.rs`, 2026-08-21): validated
  interpolated temp→duty points, asymmetric hysteresis, ramp limiting. Reached over D-Bus
  as `SetFanCurve` and from `fw-helperctl fan curve [T:D,...]`. The curve produces a
  *request*; the firmware floor and battery guard are applied on top every tick, so a
  badly drawn curve is bounded exactly as a pinned duty is, and smoothing never delays a
  safety response. Hysteresis is asymmetric — rising is followed at once, falling is
  damped by 2 °C — because heat should be answered immediately while quiet can arrive
  late. Configurable *sensor* is deferred: `control_temp()` picks `peci-temp`, and curve
  editing proper belongs to the GUI in M6.
  **Verified on hardware** under 3 minutes of 16-core load: held 70.8–80.8 °C at duty
  92–112, with **2 duty direction reversals across 45 settled samples** (no hunting) and
  no step beyond the ramp limit. Coming down it beat firmware at every point measured —
  duty 77 vs ~90 at 60 °C, 61 vs ~82 at 55 °C, 38 vs ~74 at 50 °C.
  A run also showed the curve asking for 0 at 55 °C and getting 61, because the daemon
  had just restarted and the observed floor was empty, so the cold-start model applied.
  Fixed below.
- **Where the benefit actually is — revised 2026-08-21.** The paragraph below was written
  from Q6 data that turned out to be firmware's *descending* branch. Measured while
  heating, firmware is silent right through the 55–70 °C band and does not start the fan
  until 66–73 °C, so there is little to win on the way up. The win is on the way **down**:
  firmware's hysteresis holds the fan at duty 50–90 all the way to 44.9 °C after a load
  spike. Measured, the curve beats it by 13–36 duty counts through that range.
- **Original M0 reasoning, retained for the record** (from Q6): the stock EC curve sits at 0 rpm at 43.9 °C and
  still 0 rpm at 44.9 °C, but ~2900 rpm at 64.8 °C, and only reaches ~3100 rpm at 76.8 °C.
  So the fan-start knee is above 45 °C and the ramp to ~2900 rpm is compressed into a narrow
  band, then flattens. The audible win is a gentler curve through the 55–70 °C
  band — which is exactly where a 15 W profile parks the machine. Fan and power profiles
  compose; design them together, not separately
- Safety, in this order — **build these before the curve is user-editable**:
  - [x] **the lease mechanism itself** (`fw-helper-core/src/fan.rs`, 2026-08-21).
        `take_manual` / `set_duty` / `release`, every write verified by read-back, and
        **any failure after the mode switch releases before returning** — an error raised
        while still holding the fan at an unverified duty is the exact state ADR 0006
        exists to prevent. `set_duty` refuses while the EC owns the fan rather than
        issuing a write the firmware rejects. 14 tests, no hardware
  - [x] **Verified on hardware as root** (2026-08-21): took control at 180/255, fan 0 →
        5041 rpm, ramped to 120/255 → 3795 rpm, released, EC back to 0 rpm within 4 s.
        Both refusal paths fired without touching hardware. The run corrected two
        assumptions baked into the first draft, now recorded in the baseline:
        **`pwm1` is refused with `EOPNOTSUPP` while the EC owns the fan** (so the takeover
        window cannot be closed by pre-writing the duty, only kept short), and
        **duty round-trips through whole percent** (write 180, read 181), so verification
        needs a tolerance where M2's charge limit needed exact equality
  - [x] **`fw-helper-restore-fan`** (2026-08-21) — its own crate, depending on nothing but
        the dependency-free core: no async runtime, no D-Bus, 478 KB. Not yet wired to
        `ExecStopPost`; there is still no installed unit
  - [x] **restore `pwm1_enable=2` on exit, signal and panic** (`fw-helperd/src/fan.rs`,
        2026-08-21). `FanLease` is deliberately lock-free — a panic can happen while
        another thread holds a lock, and a hook that blocks there would leave the process
        alive with the fan held. Releases are unconditional: the flag saying whether we
        hold the lease can be wrong in exactly the case that matters, and the write is
        idempotent. Startup also reclaims a fan left manual by a previous instance,
        reading before writing so the log says which case it was.
        **SIGTERM verified on hardware**: fan pinned at 181/255 and 5093 rpm, `pkill
        -TERM`, `pwm1_enable` back to `2` with `released manual fan control` in the log.
        The panic path is implemented and unit-tested but **has not been triggered live**
  - [x] **`ExecStopPost` verified** (2026-08-21). Unit installed via
        `install-dev.sh --systemd`, fan taken at duty 120 under 16-core load, then
        `kill -9` on the daemon. **EC control restored in 0.27 s**, against a 5 s gate.
        The journal is decisive: `Main process exited, code=killed, status=9/KILL`
        followed by `fw-helper-restore-fan: fan was manual, now EC automatic`. `SIGKILL`
        runs no handler and the watchdog thread dies with the process, so this is the one
        failure nothing in-process can cover
  - [x] **watchdog thread with independent timer** (`fw-helperd/src/watchdog.rs`,
        2026-08-21). A real OS thread, not a `tokio` task — the failure being guarded
        against includes the runtime not scheduling anything, and a task waiting on that
        same runtime would be wedged alongside everything else, which is worse than no
        watchdog because it looks like protection. The trip condition is read from
        `pwm1_enable`, not from our own flag, so a failed release is retried on the next
        tick rather than forgotten. The heartbeat is the telemetry poll loop, because
        that is what proves the runtime is still turning.
        **Verified on hardware** with fault injection (`FW_HELPERD_DEBUG_WEDGE_AFTER`,
        which blocks every tokio worker): fan pinned at 181/255, heartbeat stopped, and
        6.0 s later `WATCHDOG: fan returned to EC control` with the process still alive.
  - [x] **Manual fan control is refused on a stale heartbeat.** Fell out of the test
        rather than the design. `zbus` serves the interface from its **own executor, not
        the tokio runtime**, so blocking all 32 tokio worker slots left D-Bus answering
        normally. The poll loop can therefore be dead while `SetFanDuty` still runs, and
        without a guard the fan would be handed to a daemon that is not minding it, taken
        back by the watchdog 5 s later, and handed over again on the next call. Verified:
        `refusing manual fan control: this daemon's telemetry loop has not run for 7s`
  - [x] **firmware-floor clamp** (`fw-helper-core/src/floor.rs`, 2026-08-21). The EC's
        curve cannot be read, so it is reconstructed by composing two measured tables:
        what firmware does at a temperature (RPM), and what a duty produces (RPM). The
        duty→RPM sweep was run specifically for this — the three points known before
        were all at the high end, and a line through them put 2925 rpm at duty 77 when
        the real answer is ~85, which would have set the floor *below* firmware.
        `observe()` raises the floor from live EC behaviour, which is what closes the
        large gap the static table has across the knee (44.9 → 53.9 °C).
        **Enforced every poll tick, not only when a duty is requested** — clamping at
        request time protects nothing, since a duty chosen at idle becomes stuck-low the
        moment the machine is loaded.
        **Verified on hardware**: asked for a *stopped* fan at 36.9 °C (permitted, since
        firmware is also silent there), then 16 cores of load. The daemon walked the duty
        0 → 31 → … → 92 as the machine reached 74.8 °C, and back down to 0 at 44.9 °C.
        Against firmware's own measured curve: 2280 rpm at 53.9 °C (EC: 2020), 3024 at
        64.8 °C (EC: 2925), 3202 at ~75 °C (EC: 3100 at 76.8).
        The first run **failed this comparison** at 2808 rpm vs the EC's 2925, because
        enforcement judged "are we below the floor" against the hardware's quantized
        read-back with `DUTY_TOLERANCE` of slack — so a real three-count deficit hid
        inside a tolerance meant for verifying writes. Decisions are now compared
        exactly, and drift against hardware separately. Unit tests did not catch this;
        comparing against measured firmware behaviour did
  - [x] **The floor reads firmware instead of modelling it** (2026-08-21,
        [ADR 0011](adr/0011-quiet-is-a-legitimate-choice.md)). `pwm1` reports the EC's own
        duty while it owns the fan, confirmed across 60 samples matching the duty→RPM
        table to within 2.5%. Only the ascending branch is recorded — firmware's
        descending duty is hysteresis (0 vs 92 at the same 61.9 °C), not a requirement.
        Observations are credited across the whole span since the last sample, because the
        die sensor climbs ~4 °C/s and 1 Hz sampling otherwise skips most buckets, and only
        when firmware's duty was unchanged at both ends. Thresholds retuned against a
        measured 92.8 °C peak rather than M0's unrepresentative 76.8 °C.
        **Verified**: `fan 0` honoured at 51.9 °C, where the modelled floor demanded 63.
        Hardware caught three defects that unit tests passed: the quantized read-back, a
        floor deficit hiding inside `DUTY_TOLERANCE`, and direction derived per-sample
        reading a quantized cooldown as "steady" and recording the descending branch
  - [x] **Battery guard** (`fw-helper-core/src/battery.rs`, 2026-08-21). The battery is
        the one component with a low limit (crit 49.9 °C) and no protection of its own —
        the CPU throttles, the battery simply degrades. It raises the floor independently
        of the CPU, and takes the fan back entirely near its limit.
        **Sized as a backstop, not a response to observed risk**: measured, the battery
        rises only 2 °C (31.9 → 33.9) across five minutes of 16-core load, leaving 16 °C
        of headroom, so the guard fires at no temperature yet seen. The unmeasured case it
        exists for is a fan held low for far longer, which the curve engine will make
        possible. Airflow *does* reach the battery — during a post-load cooldown with the
        fan running hard it fell to 26.9 °C, below its idle baseline
  - [x] **`temp*_crit`-derived ceiling override, with sanity validation**
        (`fw-helper-core/src/ceiling.rs`, 2026-08-21). Derived from the control
        sensor's critical point minus a 15 °C margin — intervening *at* crit would be
        intervening after firmware already considers things critical — capped at 100 °C
        and falling back to 90 °C when no plausible value is available. Not knowing the
        limit is a reason to be more cautious, so the fallback is below the cap. Every
        value is validated: the -273150 case is not hypothetical, and trusting it would
        put the ceiling at absolute zero and disable manual fan control permanently.
        **The ordering with the floor is a compile-time assertion**, not a convention:
        full duty is demanded several degrees before any ceiling can fire. That matters
        because of a measured fact that runs against intuition — the EC's curve tops out
        near 3100 rpm while full duty reaches ~5200, so releasing to firmware *reduces*
        airflow. It is the last resort, not the next step up.
        **Verified on hardware** with `FW_HELPERD_DEBUG_CEILING_C=55` (the override can
        only ever lower the ceiling): took the fan at 39.9 °C, released it at 55.9 °C,
        refused to give it back at 57.9 °C with a message naming the fix, and allowed it
        again at 44.9 °C
  - [x] **refuse manual control if the sensor is unreadable** (point 6). No temperature
        means no floor, and no floor means an unbounded duty. Losing the sensor *while*
        holding the fan hands it back to firmware, which has its own sensors
- Fan control reached over D-Bus (`SetFanDuty` / `SetFanAuto`, polkit action
  `org.fwhelper.set-fan`) and from `fw-helperctl fan <duty|auto>`. **This is not a
  curve** — it pins one duty with no temperature feedback. A flat `MIN_DUTY` floor of
  77/255 stands in for the firmware-floor clamp until that exists; `fan auto` is the
  route to a genuinely quieter fan, since the EC may run it slower safely
- `status` names the fan's owner. An RPM shown without saying the EC curve is
  suspended is indistinguishable from a stuck fan, which is the failure ADR 0006 asks
  the UI to make visible
- [x] **logind `PrepareForSleep` handling** (2026-08-21). A suspended process is not
      minding anything — the watchdog thread is frozen alongside everything else — so for
      the whole sleep there would be nothing between the fan and whatever duty it was
      left holding. The fan is released on the way down and taken back after the wake.
      **The signal alone is not sufficient**: `PrepareForSleep(true)` is a notification,
      not a request for permission, and logind does not wait for handlers. A **delay
      inhibitor lock** is what actually buys the time to write `pwm1_enable=2`, and it is
      dropped last, after the release, because it is what holds suspend open.
      The restore re-runs the full clamp rather than replaying the raw duty: the machine
      may wake warmer than it slept, so the floor is computed from telemetry read after
      the wake. A pending restore of duty **0** is tracked with a separate flag rather
      than a sentinel — 0 is a legitimate setting, and the one a quiet-machine user is
      most likely to have chosen.
      **Verified on hardware** via `systemctl suspend` (not `rtcwake`, which writes
      `/sys/power/state` directly, skips logind, and would test nothing): the lock was
      confirmed held in `systemd-inhibit --list`, the log shows the release before
      suspend and the restore after resume, and the fan came back at duty 120.
      Incidentally a second confirmation that firmware does not reset the charge limit
      across suspend — `charge limit still 80%; nothing to re-apply`

**Exit:** `kill -9` on the daemon under sustained load restores EC fan control within 5 s.
Suspend/resume leaves the fan in a correct state. Curve holds a target temperature under
`stress-ng` without audible hunting. **Gated on Q4.**

---

### M4 — Power limits  ✅ core control complete, verified on hardware

- [x] **PL1 write path** (`fw-helper-core/src/power.rs`, 2026-08-21) via
  `intel-rapl-mmio:0`, reached as `SetPowerLimit` behind polkit action
  `org.fwhelper.set-power-limit`, and from `fw-helperctl power-limit N`. Same pattern as
  every write since M2: validate range before support, write, read back, persist,
  re-apply at startup and on resume.
  **Verified**: 15 W setpoint held **15.02 W (+0.1%)** at 62.2 °C under 16-core load,
  against Q6's 14.68 W / 64.8 °C written directly. The 25 W case measured 23.57 W at
  84.8 °C, ~8 °C above Q6, almost certainly heat soak from a long session — so the
  10 W ≈ 12 °C figure still rests on Q6's controlled run, not on this one.
  **RAPL survives suspend**: `power limit still 15 W; nothing to re-apply`.
  PL2 is deliberately untouched: it governs burst responsiveness, not sustained thermals,
  and its `max_power_uw` reads 0 (unset), which would clamp any naive UI to zero
- Write PL1/PL2 via `intel-rapl-mmio:0` — confirmed as the authoritative zone (Q2)
- Envelope is **8-35 W PL1 / 60 W PL2**, and PL1 regulates to ±2% of setpoint (Q6).
  Q5 said 25 W from `max_power_uw`; Q7 measured the enforced ceiling at **35 W**
- Profile values. The first three come from Q6 (10 W ≈ 12 °C); `turbo` and `max` from Q7,
  where the temperatures are measured under a pinned duty-200 fan:

  | Profile | PPD | PL1 | Sustained CPU temp | vs 25 W |
  |---|---|---|---|---|
  | Quiet | power-saver | 15 W | ~65 °C | — |
  | Balanced | balanced | 20 W | ~71 °C | — |
  | Performance | performance | 25 W (stock) | ~77 °C | baseline |
  | Turbo | performance | 30 W | 76 °C core | +8.9% |
  | Max | performance | 35 W | 84 °C core | +15.9% |

  `turbo` and `max` share the `performance` PPD position. The GNOME slider still lands on
  `performance` — `Profile::canonical_name_for` is what keeps that unambiguous, and the
  extra two are reached by name.
- Never expose the MSR zone's bogus 200 W as a slider maximum. **And do not clamp the UI to
  `max_power_uw` either**: it declares 25 W on a board that honours 35 W, and trusting it
  cost ~16% of the machine's throughput (Q7). `PowerLimit::max_watts` treats the declared
  value as a floor on the answer, never a ceiling
- **Any power measurement must average over more than the ~32 s PL1 window.** Sampling sooner
  reads turbo as steady state — the Q6 run showed 29 W at t+25 s under a 25 W limit
- Validate writes stick — read back after a delay, and downgrade the capability to
  unavailable if firmware reverts them
- Clamp to a sane range; refuse values that would make the machine unusable
- Re-apply on resume (firmware commonly resets these)

**Exit:** already demonstrated by `scripts/q6-pl1-load-test.sh` — the milestone is to make the
daemon do what that script proved possible, and to re-run the script as a regression test.

---

### M5 — Profiles  ✅ core complete, verified on hardware

Ties M2–M4 together into the actual product.

- [x] **Profile schema** (`fw-helper-core/src/profile.rs`): PPD profile + fan curve + PL1.
      **No built-in sets a charge limit**, though ADR 0005's sketch did: battery longevity
      is a standing preference, not a performance choice, and someone capping at 80% to
      preserve the pack should not have that undone by asking for speed for an hour. The
      field exists so a user profile can opt in
- [x] **Delegates the PPD axis and subscribes to `ActiveProfile`**
      (`fw-helperd/src/ppd.rs`). Measured: PPD owns *both* bus names, and both serve the
      interface under the newer name at `/org/freedesktop/UPower/PowerProfiles`.
      `ActiveProfile` is a writable property that emits change signals, so switching is a
      property write and following is a subscription, not a poll. PPD gets its own
      system-bus connection, since it is always on the system bus even when this daemon
      runs on the session bus for development
- [x] **Fallback to direct `platform_profile`** when PPD is absent, reported as
      `ProfileBackend` so a client can say why the GNOME slider is not in sync
- [x] Three defaults: quiet 15 W, balanced 20 W, performance 25 W, each with its own curve
- [x] **User profiles in `/etc/fw-helper/profiles.d/`** (`fw-helperd/src/profiles.rs`,
      2026-08-22). Same trivial `key=value` format as the state file — four fields and a
      curve, not a configuration language — and parsed in the daemon, since core stays
      free of config handling (ADR 0010). Two rules:
      **a file naming a built-in replaces it**, including for the GNOME slider, which is
      how `quiet` is customised rather than accumulating a near-duplicate beside it; and
      **a profile under a new name is selectable by hand but never auto-applied when the
      slider moves**, because PPD has three positions and two user profiles both claiming
      `power-saver` would make the slider's destination a coin toss.
      One bad file is skipped by name, not fatal. `data/example-profile.conf` ships as
      documentation and is parsed by a test, because documentation rots.
      **Verified**: `silent` added at 12 W, a `quiet.conf` replacing the built-in gave
      10 W *including via the slider*, a file with `ppd = turbo` was reported as
      `line 2: unknown ppd "turbo"` and skipped without affecting the others, and an
      unknown name was refused with the list of what exists
- [x] **AC/battery auto-switching** (2026-08-22). Mains is resolved by the supply's
      `type` being `Mains`, never by name — this board calls it `ACAD`. **Off unless
      asked for**: a machine that changes behaviour when a cable is plugged in, without
      having been told to, is a machine behaving strangely. Edge-triggered, and the first
      reading is a baseline rather than a transition, so starting the daemon does not
      count as plugging in and override what the user last chose
- [x] **Save the current settings as a profile** (2026-08-22): `fw-helperctl profile
      save NAME` captures the active PPD profile, power limit and fan curve into
      `/etc/fw-helper/profiles.d/NAME.conf`, which the user can then edit. Writer and
      parser round-trip exactly, and that is a test. The charge limit is deliberately not
      captured — it is a standing preference, and folding it in would make switching
      profiles change it later. `profile delete NAME` removes the file; a deleted file
      that was shadowing a built-in restores it

**Verified 2026-08-22**, both directions: `fw-helperctl profile quiet` set PPD to
power-saver and PL1 to 15 W; driving PPD to performance and balanced (the same property
the GNOME slider writes) made the daemon follow with 25 W and 20 W; a daemon restart
re-applied the persisted profile exactly once.

Two defects found by that run, both now fixed:

- **A verified power-limit write does not stick.** PL1 read back as 25 W and was 33 W
  seconds later — above the zone's own advertised 25 W maximum, so `max_power_uw` does not
  bind firmware either. Switching `platform_profile` appears to make firmware re-derive
  PL1 asynchronously. The limit is now re-asserted every tick, bounded at five corrections
  so we never fight firmware invisibly. This was M4's unimplemented "read back after a
  delay" bullet
- **Our own PPD write echoes back** as a change signal indistinguishable from a user
  moving the slider, so a restart applied its profile twice. The daemon marks the profile
  it set before asking

**Exit:** Moving the GNOME power slider applies the matching fan curve and power limits.
`fw-helperctl profile quiet` does the same and GNOME reflects it.

---

### M6 — GUI  🟡 read-only view plus controls; curve editing outstanding

The telemetry view was pulled forward to have something visible. What exists:
`crates/fw-helper-gui` (binary `fw-helper`), a libadwaita window showing live stats,
per-sensor temperature bars scaled to each sensor's own critical threshold, and
capabilities greyed out with their reason. Worker thread owns D-Bus so the main loop
never blocks; reconnects on its own if the daemon restarts.

Still to do below. Controls arrive as M2–M5 land.

### M6 (remainder) — GUI controls

- GTK4 + libadwaita main window: status, profile picker, curve editor, charge slider
- Tray icon via StatusNotifierItem. **Note:** stock Ubuntu GNOME needs the AppIndicator
  extension. Detect its absence and tell the user, rather than showing nothing
- Grey out controls the daemon reports as unavailable, with the reason on hover
- Show when a safety override is active and why (ADR 0006 makes this mandatory, not optional)
- Global hotkey for profile cycling via the desktop portal

**Exit:** Full feature parity with `fw-helperctl`, no root, nothing offered that does not work.

---

### M7 — Packaging  🟡 builds; not yet installed even once

- [x] **`.deb` for daemon + GUI** (`scripts/build-deb.sh`, 2026-08-22), with polkit policy,
  D-Bus conf, systemd unit, desktop entry and the example profile. Built with `dpkg-deb`
  from a staging tree rather than cargo-deb: no extra tooling, and the layout is visible in
  one place. Library dependencies come from `dpkg-shlibdeps` rather than being guessed,
  because the GUI links GTK4 and libadwaita and their sonames move
- [x] **Postinst does not enable charge control** — that changes which mechanism governs
  charging (ADR 0008), so it is `fw-helper-enable-charge-control`, run deliberately. The
  postinst says so when the interface is absent. Manual fan control is never taken unasked
- [x] **prerm stops the unit before the binaries go**, so `ExecStopPost` runs and the fan
  returns to the EC. Reasoned, **not yet demonstrated** — removing the package while it
  holds the fan is the test
- [ ] **Install it once.** Nobody has: dependency resolution, the service starting, and
  launching from the GNOME app grid are all unverified
- [x] README rewritten: it still claimed nothing wrote to hardware
- README documenting exactly which knobs work on which boards, and the honest limitations
  (no undervolting, and why)
- Consider a PPA

---

## Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Stuck-low fan cooks the machine | Severe | ADR 0006 in full; `kill -9` test is a release gate |
| RAPL locked by firmware | M4 cut | Q1 answers this in M0, before any code is written |
| UEFI battery limit fights us | Charge limit silently wrong | Read back after write; surface the conflict explicitly (ADR 0008) |
| PPD D-Bus name changes | Profiles break | Support both known names, prefer newer |
| Kernel/EC firmware drift | Silent breakage | Capability probing at startup; never assume |
| Scope creep toward FW16/AMD | Never ships | v1 is FW13 Intel only; revisit after M7 |

## Immediate next actions

1. `sudo ./scripts/fw-probe.sh --write-test` — answers Q1 and Q4
2. `sudo dmesg | grep -i charge` — answers Q3
3. Update `docs/hardware-baseline.md` with the results
4. Cut any milestone whose gating question failed, then start M1
