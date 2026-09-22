# ADR 0014 — Parked cores and GPU frequency caps are fail-safe state

**Status:** accepted (2026-09-22)

## Context

M9 adds two levers that alter the machine persistently: taking CPU cores offline via
`cpu*/online`, and lowering the `xe` GT's frequency window. Both outlive the process
that set them.

ADR 0006 established the pattern for the fan: every route out of the daemon must return
`pwm1_enable=2`. The question here is whether these two levers need the same treatment,
and the answer is yes — with one lever needing it *more* than the fan does, and for a
reason that is easy to miss.

**A stuck fan has an escape route. A parked core does not.**

The fan is corrected by a reboot, and by the EC resuming control the moment
`pwm1_enable` returns to 2 — including from `ExecStopPost`, which systemd runs even
after `SIGKILL`. Nothing on the machine re-onlines a CPU except something that knows it
parked it. An offline core survives a daemon restart and presents to the user as a
machine that has quietly lost twelve cores, with no UI anywhere explaining why.

What is *not* true is that parking is dangerous. It costs throughput and nothing else,
which is why there is no watchdog here: the fan watchdog exists because a stuck-low fan
is silent and can damage the battery, and that has no analogue.

## Decision

Both levers restore on **every** route out of the process: clean exit, `SIGTERM`,
`SIGINT`, panic, resume — and, critically, from `ExecStopPost=`.

1. **The restore path takes no locks.** A panic can arrive while another thread holds
   one, and blocking in the panic hook would leave the process dying with cores parked.
   `TuningLease` is built from atomics for the same reason `FanLease` is.

2. **`ExecStopPost=` is the guarantee, not the startup reclaim.** `fw-helper-restore-fan`
   re-onlines parked cores and lifts a GPU cap alongside its original job. The daemon's
   startup reclaim is kept as a second line, but it is a weaker property and must not be
   mistaken for this one — see the measurement below.

3. **Restoring reads hardware, never memory.** The process cleaning up is by definition
   often a *fresh* one that never applied the setting. `rp0_freq` is the authority for
   what "uncapped" means; `cpu*/online` is the authority for what is parked.

4. **The topology is read once, while the machine is whole, and cached.** An offline CPU
   loses `cpufreq/` and `topology/`, so it cannot be classified at all.

5. **`cpu0` is never parkable.** The kernel exposes no `online` file for the boot CPU.
   It is modelled as permanently on rather than offered as a toggle that fails.

## Consequences

Restoring is cheap and idempotent: writing `1` to an online CPU and resetting an
uncapped GT are both no-ops, so the crash path can run unconditionally without needing
to detect anything.

The restore in `fw-helper-restore-fan` deliberately does **no** classification — it
writes `1` to every `cpu*/online` it finds. That is the one thing still knowable on a
machine whose topology has gone.

`fw-helperctl tune` prefers the daemon's cached clusters over its own probe, so a
parked machine is still described correctly.

## What this cost to learn

Both points below are from the first hardware test of this ADR, 2026-09-22, and both
were invisible to the unit tests that passed beforehand.

**The GPU cap survived a `SIGKILL`.** The restore was gated on an in-memory flag saying
"we capped it" — which a fresh process cleaning up after a killed one necessarily reads
as "nothing capped". The cores came back, because their loop was ungated; the GT stayed
pinned at 1200 MHz. The fix is point 3, and the general form is worth keeping: **a
crash-recovery path must not be conditional on state the crash destroyed.**

**Parking a cluster made that cluster unreachable.** Re-probing on a parked machine
merged LP-E into E, after which `to_park(Lpe)` returned nothing and `read()` said
`mixed` — the level actually in effect reported as parking no cores, so it could
neither be recognised nor undone by name. The fix is point 4.

**The startup reclaim was mistaken for `ExecStopPost` parity.** The first implementation
restored only at startup, and the hardware test appeared to pass because
`Restart=on-failure` happened to bring the daemon back five seconds later. That is not
the same guarantee: it does nothing for `systemctl stop` of a wedged process, a masked
unit, or a daemon uninstalled between the kill and the next boot. ADR 0006's property
comes from `ExecStopPost=` running on *every* stop of the unit, and this ADR claimed
parity it did not have until point 2 was implemented.

## Related

- [0006](0006-fail-safe-fan-control.md) — the pattern this follows, and the one it is
  measured against
- [0004](0004-sysfs-first-hardware-access.md) — every path goes through `Sysfs`
