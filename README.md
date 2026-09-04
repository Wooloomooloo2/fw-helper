# fw-helper

[![ci](https://github.com/Wooloomooloo2/fw-helper/actions/workflows/ci.yml/badge.svg)](https://github.com/Wooloomooloo2/fw-helper/actions/workflows/ci.yml)

Firmware control for the **Framework Laptop 13** on Ubuntu — fan curves, power limits,
battery charge limit, and performance profiles in one tray application.

Inspired by [G-Helper](https://github.com/seerge/g-helper), which does this for ASUS laptops
on Windows. This shares no code with it — see
[ADR 0001](docs/adr/0001-separate-repository.md).

> **Status: 0.1.0, first public preview.** Fan control (with a graphical curve editor),
> power limits, the battery charge limit and profiles all work and are verified on real
> hardware. Expect rough edges and read the
> [known issues](CHANGELOG.md#known-issues) before filing anything — several are already
> measured and understood. See [docs/plan.md](docs/plan.md) for what is measured and what
> is assumed, and [CHANGELOG.md](CHANGELOG.md) for the release notes.

## Why

The pieces already exist on Linux — `powercap`, `cros_ec` hwmon, power-profiles-daemon,
`fw-fanctrl` — but they are scattered across sysfs paths, CLIs, and daemons that do not know
about each other. fw-helper is the unifying layer: named profiles that set all of them at
once, switchable from a tray icon or a hotkey.

## What works, honestly

| Feature | Status |
|---|---|
| Live telemetry | **Working** — temps, fan RPM, CPU and whole-machine power, battery life |
| Capability detection | **Working** — every knob reports available, or why not |
| Battery charge limit | **Working** — verified to actually stop charging: from below the limit, on AC, it halted at exactly 80% with `current_now=0`. Drives Framework's custom EC command ([ADR 0012](docs/adr/0012-charge-limit-via-custom-ec-command.md)); the standard sysfs interface is inert on this board |
| Fan control | **Working** — manual duty or a curve, with every ADR 0006 safety layer verified on hardware |
| Power limits (PL1) | **Working** — a 15 W setpoint held 15.02 W sustained, +0.1% |
| Performance profiles | **Working** — layered over power-profiles-daemon; the GNOME slider stays in sync |
| Custom profiles | **Working** — saved from the app, or written by hand in `/etc/fw-helper/profiles.d/` |
| Fan curve editing | **Working** — graphical editor in the app, or `fw-helperctl fan curve` |
| Session recording | **Working** — record a run, come back and read the graph. CPU/GPU/RAM load, temps, power against the limit, fan, throttle events |
| In-game numbers | **Working via MangoHud** — no ordinary window can be drawn over a fullscreen game on Wayland, so fw-helper feeds the overlay that can. See below |
| PL2 (short-term limit) | Not touched — it governs burst response, not sustained thermals |
| **Undervolting** | **Not possible.** See below |

Every mechanism above was exercised on real hardware before any application code was written,
and the measurements are recorded in [docs/hardware-baseline.md](docs/hardware-baseline.md).
Several of them contradicted what the design assumed — the EC's fan curve has ~20 °C of
hysteresis, its duty read-back is quantized to whole percent, and this machine reaches
92.8 °C in ordinary use rather than the 76.8 °C an early test suggested.

### Undervolting is not available

Intel locked the undervolting MSR as the mitigation for
[Plundervolt / CVE-2019-11157](https://nvd.nist.gov/vuln/detail/CVE-2019-11157). It is
unreachable on Core Ultra parts and Framework's BIOS does not expose an unlock. This is a
firmware/silicon limitation, not something a Linux application can work around.

Power limits (PL1/PL2) deliver most of what people actually want from undervolting on a
laptop — lower sustained temperatures, quieter fan, longer battery. Measured on this machine:
dropping the sustained limit from 25 W to 15 W took the CPU from **76.8 °C to 64.8 °C** under
full load. See [ADR 0007](docs/adr/0007-no-undervolting.md).

## Recording a session

The question this started from was *"is 35 W actually the ceiling on this laptop?"*, and
answering it once took a bespoke script and four manual runs. Now:

```bash
fw-helperctl record start "cyberpunk at 35W"
# ... play something ...
fw-helperctl record stop
```

Recording happens in the **daemon**, not the app, for two reasons. `energy_uj` is
root-only (the PLATYPUS mitigation), so an unprivileged recorder cannot read package
power at all; and a recording has to survive the window being closed, which is the whole
point of coming back to look at it later.

Open the **Monitor** tab in the app to read one back: five plots on a shared time axis —
load, power, temperature, fan, memory. Sessions are plain CSV under
`/var/lib/fw-helper/sessions/`, world-readable, with columns matching
`scripts/sustained-perf-test.sh`, so a recorded game session and a benchmark run drop
into the same spreadsheet.

**How to tell whether the power limit was the thing holding you back.** The package
publishes no "I am power limited" flag — unlike the GPU, which publishes real throttle
reasons. So the PL1 setpoint is drawn as a dashed line over the power trace: draw sitting
on the line *is* PL1 binding. That inference is how the 35 W ceiling was found.

The last 20 sessions are kept, and a single recording stops itself after 12 hours.

## In-game numbers, and why fw-helper does not draw them

On GNOME/Wayland an ordinary window **cannot** be kept above a fullscreen window — Mutter
implements no protocol for it, so this is not something an application can work around.
`fw-helper --overlay` gives a compact readout that works beside a borderless game or on a
second display, and is covered by a genuinely fullscreen one.

For numbers *inside* a fullscreen game, use [MangoHud](https://github.com/flightlessmango/MangoHud).
It is not a window: the Vulkan loader loads it into the game's own process and it paints
into the frame before presentation, so the compositor never learns an overlay exists.
fw-helper feeds it what it cannot know:

```
MANGOHUD=1 MANGOHUD_CONFIGFILE=/usr/share/fw-helper/mangohud/fw-helper.conf %command%
```

as Steam launch options. That adds the power limit being enforced, who owns the fan, the
active profile, the battery pack temperature, and whether a recording is running.

**It also adds GPU load, which MangoHud cannot show on this laptop.** Measured with
MangoHud 0.6.9.1: its Intel support is i915-only, and this board runs `xe`, so it
disables `gpu_stats` outright and falls back to `intel_gpu_top`, which needs
`perf_event_open` and is blocked by `kernel.perf_event_paranoid=4`. fw-helper reads the
per-engine counters out of `/proc/<pid>/fdinfo` instead, which needs no permission.

## Hardware support

Developed against:

- Framework Laptop 13 Pro (Intel Core Ultra Series 3), board `FRANMJCP07`, BIOS 03.02
- Ubuntu 24.04 LTS, kernel 7.0

Other Framework 13 Intel boards will likely work — the daemon probes capabilities at startup
and disables what it cannot drive. AMD boards and the Framework 16 are **not** targeted for
v1; AMD in particular needs a different power-limiting mechanism (`ryzenadj` rather than RAPL).

## Requirements

- A kernel new enough for `cros_ec_hwmon` (fan control)
- power-profiles-daemon (optional but recommended — we integrate with it rather than
  replacing it, so the GNOME power slider keeps working)
- **No `ectool` or `framework_tool` needed** — we use kernel interfaces directly
  ([ADR 0004](docs/adr/0004-sysfs-first-hardware-access.md))

## Safety

Manual fan control is genuinely risky: once userspace takes over, the EC stops managing the
fan and holds the last duty written — so a crashed daemon can leave the fan stuck low under
load. [ADR 0006](docs/adr/0006-fail-safe-fan-control.md) specifies the mitigations
(restore-on-exit, crash-path binary, watchdog, a floor that is never quieter than firmware,
and a critical-temperature override that hands control back unconditionally).

`kill -9` recovery is a release gate, not a nice-to-have. It is measured: EC control was
restored **0.27 s** after the daemon was killed under load.

What the fan floor protects is worth stating precisely, because it changed once the hardware
was measured. The CPU throttles itself at 100 °C, so a fan held too low costs performance
rather than hardware. The component with no protection of its own is the **battery**, whose
sensor reports a 49.9 °C limit — by far the lowest on the board
([ADR 0011](docs/adr/0011-quiet-is-a-legitimate-choice.md)).

## Install

```bash
./scripts/build-deb.sh                   # builds fw-helper_<version>_amd64.deb
sudo apt install ./fw-helper_*.deb
```

That installs the daemon, the GTK window (`fw-helper`, also in your app grid) and the CLI,
enables the service, and creates `/etc/fw-helper/profiles.d/` for your own profiles.

Nothing takes manual control of the fan until you ask it to.

The charge limit needs no opt-in step. If a previous version left
`/etc/modprobe.d/fw-helper.conf` behind, installing removes it: it belonged to the
superseded mechanism ([ADR 0008](docs/adr/0008-charge-limit-via-module-parameter.md)) and
was not merely useless — it produced a charge limit that reads back correctly and does not
stop charging, which is what made that mechanism look right for weeks
([ADR 0012](docs/adr/0012-charge-limit-via-custom-ec-command.md)).

## Or run it from source

```bash
cargo build --all
cargo run -p fw-helperctl -- status      # reads sysfs directly; no daemon needed
```

For the full picture, run the daemon. It needs the D-Bus policy installed first:

```bash
sudo ./scripts/install-dev.sh --systemd  # policy, polkit, unit, and start it
fw-helperctl status                      # unprivileged, via D-Bus
fw-helperctl profile                     # quiet | balanced | performance
fw-helperctl watch 10                    # live power, fan, CPU temp at 1 Hz
```

Package power needs a **root daemon** — `energy_uj` is `0400`, the PLATYPUS mitigation. The
client stays unprivileged and holds no hardware access. Without the daemon, `fw-helperctl`
falls back to reading sysfs directly and everything except package power still works.
Nothing in `fw-helperctl` writes to hardware.

Tests need neither hardware nor root; they run against synthetic sysfs fixtures:

```bash
cargo test --all
```

## Poking at your own machine

```bash
./scripts/fw-probe.sh                    # read-only survey
sudo ./scripts/fw-probe.sh --write-test  # also tests whether writes are honoured
```

`--write-test` briefly changes power limits and fan duty, then restores them. It only ever
spins the fan *up*. Read it before running it.

## Prior art

- [framework_system](https://github.com/FrameworkComputer/framework-system) — Framework's own tooling; our reference for EC host commands
- [fw-fanctrl](https://github.com/TamtamHero/fw-fanctrl) — established fan curve daemon
- [LACT](https://github.com/ilya-zlobintsev/LACT) — GPU control, for Framework 16
- [inputmodule-rs](https://github.com/FrameworkComputer/inputmodule-rs) — FW16 LED matrix
- [CrOS_EC_Python](https://github.com/Steve-Tech/CrOS_EC_Python) — third-party EC library, and the
  clearest catalogue of Framework EC command numbers we have found. It independently confirms the
  `0x3E03` charge-limit opcode of [ADR 0012](docs/adr/0012-charge-limit-via-custom-ec-command.md),
  which two summary lookups had disagreed about
- [YAFI](https://github.com/Steve-Tech/YAFI) — GTK4 EC GUI in Python, Linux and Windows. Overlaps on
  fan, temperatures and charge limit; adds LED control; has no power limits and no daemon. The
  daemon is the main architectural difference: it is what makes resume re-apply, the fan watchdog
  and `kill -9` recovery possible at all

## Licence

GPL-3.0. See [ADR 0001](docs/adr/0001-separate-repository.md#licensing-note).
