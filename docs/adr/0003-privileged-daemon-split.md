# 0003 — Privileged daemon + unprivileged GUI over D-Bus

- **Status:** Accepted; the per-action polkit split is superseded by [0015](0015-one-polkit-action-for-hardware.md)
- **Date:** 2026-08-18

## Context

Every knob we want requires root:

| Knob | Path | Why root |
|---|---|---|
| Fan duty | `/sys/class/hwmon/hwmon*/pwm1` | root-owned |
| Power limits | `/sys/class/powercap/intel-rapl:0/constraint_*_power_limit_uw` | mode `644`, root-owned |
| Charge limit | `/dev/cros_ec` (fallback path) | `crw------- root:root` |
| Platform profile | `/sys/firmware/acpi/platform_profile` | root-owned |
| Package power *reads* | `/sys/class/powercap/*/energy_uj` | mode `0400`, see below |

Running the whole GUI as root is not acceptable: it would mean a GTK application, its theme
engine, and its entire dependency tree executing with full privilege on the user's session bus.

Note the last row: even *reading* package power requires root. `energy_uj` was restricted as
the mitigation for **PLATYPUS / CVE-2020-8694**, where RAPL energy readings served as a side
channel to recover AES keys. So the privilege boundary is not only about writes — an
unprivileged GUI cannot render a power graph without a privileged helper feeding it.

State must also outlive the GUI. A fan curve has to keep running when no window is open, and
must be restored correctly across suspend/resume ([0006](0006-fail-safe-fan-control.md)).

## Decision

Two components:

- **`fw-helperd`** — root systemd service. Owns all hardware access and all policy state.
  Exposes `org.fwhelper.Daemon1` on the **system** bus.
- **`fw-helper`** — unprivileged GUI (window + tray). Holds no hardware access whatsoever;
  every action is a D-Bus method call.

Authorization is via **polkit**, action prefix `org.fwhelper.`.

## Interface sketch

```
org.fwhelper.Daemon1                      (system bus, /org/fwhelper/Daemon1)

  Properties (read-only, change-signalled)
    Telemetry          a{sv}    temps, fan RPM, package power, battery state
    ActiveProfile      s
    AvailableProfiles  as
    Capabilities       a{sb}    per-knob: is it actually usable on this board?

  Methods
    SetProfile(s name)                     -> polkit: org.fwhelper.set-profile     (auth_admin_keep)
    SetFanCurve(s profile, a(uu) points)   -> polkit: org.fwhelper.manage-fan      (auth_admin_keep)
    SetChargeLimit(y percent)              -> polkit: org.fwhelper.set-charge-limit(auth_admin_keep)  [now org.fwhelper.configure, ADR 0015]
    SetPowerLimits(u pl1_uw, u pl2_uw)     -> polkit: org.fwhelper.set-power       (auth_admin_keep)
    ResetToFirmwareDefaults()              -> polkit: org.fwhelper.reset           (yes)
```

`Capabilities` is load-bearing. Given the open questions in the hardware baseline (charge
control not registered, RAPL lock bit unverified), the daemon probes at startup and reports
what genuinely works. **The GUI greys out what the daemon says is unavailable rather than
offering controls that silently do nothing.**

## Consequences

**Positive**

- Attack surface confined to one small, auditable binary with no UI toolkit linked in.
- Policy survives GUI exit, logout, and suspend.
- Headless use (`fw-helperctl`, scripts, CI) is free — it is the same D-Bus API.
- polkit gives per-action policy: an admin can allow charge-limit changes without allowing
  power-limit changes.

**Negative**

- Two processes, an IPC boundary, and a schema to version.
- **polkit can block indefinitely.** `CheckAuthorization` with `AllowUserInteraction`
  never returns when no authentication agent can service the caller — which is the
  case for any process not attached to a login session (no `XDG_SESSION_ID`), such as
  a shell spawned outside the desktop session. Two rules follow, and both are load
  bearing: check without interaction first so already-authorized callers answer
  immediately, and put a hard timeout on the interactive call. A write method must
  also take `&self`, never `&mut self` — the latter makes zbus hold the interface
  write lock for the duration, so one pending prompt stalls telemetry for every client.
- polkit prompts are intrusive if configured naively. Mitigate with `auth_admin_keep`
  (session-scoped) and by treating `ResetToFirmwareDefaults` as always-allowed — returning
  hardware to a safe state should never require a password.

## Alternatives considered

- **setuid helper binary.** Rejected: no per-action policy, no async telemetry, and a classic
  source of privilege-escalation bugs.
- **udev rules loosening sysfs permissions to a `fw-helper` group.** Tempting for the sysfs
  knobs and it removes the daemon entirely for those. Rejected as the primary mechanism:
  it grants blanket persistent write access to power limits for any process running as that
  user, with no audit trail and no way to enforce safe ranges. May still be used as an
  *optional* fast path for read-only telemetry.
- **GUI runs as root.** Rejected outright.
