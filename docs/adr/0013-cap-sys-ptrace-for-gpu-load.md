# 0013 — The daemon holds `CAP_SYS_PTRACE`, and nothing else

- **Status:** Accepted
- **Date:** 2026-09-20

## Context

M8 reads per-client GPU load from `/proc/<pid>/fdinfo/<fd>`, where the `xe` driver
publishes `drm-cycles-<engine>` per DRM client. That source was chosen in 0.6.0 partly
*because* it needed no privilege the daemon did not already have — the `xe` PMU was the
textbook answer and was rejected for costing a sandbox widening (`perf_event_open` lives
in systemd's `@debug` group, excluded by our own `SystemCallFilter=@system-service`).

The premise was wrong, and it took a packaged daemon to show it.

Opening the fdinfo of a process owned by another uid goes through
`ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`, which for a different uid needs
`CAP_SYS_PTRACE`. **Being root does not supply it.** The kernel grants root nothing
except through capabilities, and `fw-helperd.service` set `CapabilityBoundingSet=` empty
([ADR 0003](0003-privileged-daemon-split.md) drops everything not needed), so the daemon
ran as uid 0 holding a capability set of exactly zero: `CapEff: 0000000000000000`.

Every process driving the GPU on a desktop belongs to the desktop user. So every read
failed with `EACCES`, and `gpu_percent` was **never published by any packaged daemon** —
not to the window, not to the HUD line, not into a recorded session. Measured
2026-09-20 against 0.6.0: 30 consecutive `watch` samples with an empty `gpu%` column,
while `gpu_mhz` — an ordinary sysfs read of the GT — arrived on every tick and made the
GPU look present throughout.

It passed in development because `FW_HELPERD_SESSION_BUS=1` runs the daemon **as the
user**, reading the user's own processes. The 2026-09-04 measurement that chose this
source (4.1% idle, attributed to `firefox-bin`) was real, and verified the parsing rather
than the deployed path.

## Decision

**`fw-helperd.service` grants exactly one capability, `CAP_SYS_PTRACE`, ambient and
bounded.**

```ini
CapabilityBoundingSet=
AmbientCapabilities=
...
CapabilityBoundingSet=CAP_SYS_PTRACE
AmbientCapabilities=CAP_SYS_PTRACE
```

The empty pair stays first and resets the set, so the grant is additive to nothing: the
bounding set then holds `CAP_SYS_PTRACE` and no other capability is available to the
process even if it tried to acquire one. Verified on hardware:
`CapEff: 0000000000080000`, `CapBnd` identical.

This is a **read-only** use of the capability. No `ptrace(2)` call is made anywhere in
the codebase, and `@debug` stays out of the syscall filter — the capability is needed
solely to satisfy the access check on an `open()` of a `/proc` file.

**And a capability must probe the path it promises, sandbox included.** `gpu usage
available` was logged at every startup beside a permanently blank number, because
`Capabilities::probe` asked which DRM driver was loaded and stopped there.
`usage::fdinfo_blocked` now asks the rest of the question — uid 0 without the capability
— and reports a reason naming the fix.

## Consequences

**Positive**

- GPU load reaches the window, the HUD line and recorded sessions on the deployed path,
  which is the only path users have. Verified: idle desktop 12–15% with attribution, a
  48-row session with `gpu_pct` and `gpu_top` populated on every row.
- The failure explains itself if the sandbox changes again, rather than presenting as a
  working feature that reports nothing.

**Negative**

- The daemon can read any process's `/proc` state, and could attach to any process. It is
  already uid 0 with write access to `pwm1` and the powercap constraints, so this widens
  what a compromised daemon can *read* rather than what it can ultimately do — but the
  empty capability set was real hardening and this is strictly less of it.
- It is the widest single thing in the unit. That is why it is the only grant, and why
  the bounding set is set rather than left open.

## Alternatives considered

- **The `xe` PMU.** Still rejected, and now for a sharper reason than 0.6.0 had: it needs
  `perf_event_open` from `@debug` *and* `perf_event_paranoid` is 4 on this machine. That
  is a syscall-filter widening plus a system-wide sysctl change, against one capability.
- **Sample the GPU in the unprivileged client and hand it to the daemon.** The client runs
  as the user and needs no capability at all — but a recording has to outlive the window
  being closed, which is the whole point of recording living in the daemon, and a game
  session recorded with no GUI open would have no GPU column. It also inverts the trust
  direction, letting an unprivileged process write numbers into a root daemon's file.
- **Drop GPU load from recordings.** Rejected: it is the figure MangoHud cannot supply on
  this board (i915-only support against an `xe` device), which is a large part of why
  fw-helper publishes a HUD line at all.
- **Accept root without capabilities and read only root's own processes.** That is what
  shipped, and it measures nothing: no process driving the GPU runs as root.

## Note

The general lesson is the one this project keeps relearning in different clothes: check
`/proc/<pid>/status` `CapEff`, not `Uid`, before concluding a root daemon can read
something — and test the deployed configuration, because development mode differed here
in the one property that decided the outcome.
