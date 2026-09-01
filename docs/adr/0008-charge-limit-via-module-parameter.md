# 0008 — Charge limit via `probe_with_fwk_charge_control`, not a custom EC command

- **Status:** **Superseded by [0012](0012-charge-limit-via-custom-ec-command.md)** — failed on
  hardware; see [Outcome](#outcome-2026-08-26--the-approach-does-not-work)
- **Date:** 2026-08-18

## Context

`BAT1` has no `charge_control_end_threshold` despite `cros_charge_control` being loaded.
`dmesg` gives the reason directly:

```
cros-charge-control cros-charge-control.6.auto: Framework charge control detected, preventing load
```

This is deliberate upstream behaviour, not a bug. From the LKML series that added the driver:

> Framework laptops implement a custom charge control EC command, while the upstream CrOS EC
> command is also present and functional but can get overridden by the custom one. Until
> Framework make both commands compatible or remove their custom one, the driver doesn't load
> on those machines.

So the board has **two** working charge-control mechanisms:

1. **Framework's custom EC command** — what the UEFI setup screen's battery limit uses, and
   what `framework_tool` drives
2. **The standard CrOS EC command** — present and functional, but the custom one can override it

The driver declines to load rather than race the firmware. Upstream provides an opt-out:

```
/sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control   # bool, 0644, currently N
```

> If the user knows they are not going to use the custom command they can use a module
> parameter to load cros_charge-control anyways.

## Decision

**Load `cros_charge-control` with `probe_with_fwk_charge_control=1`** and use the resulting
standard `charge_control_end_threshold` sysfs attribute.

Ship a modprobe drop-in:

```
# /etc/modprobe.d/fw-helper.conf
options cros_charge-control probe_with_fwk_charge_control=1
```

**Do not** implement Framework's custom EC command.

The condition upstream attaches to the parameter — "if the user knows they are not going to
use the custom command" — becomes an install-time requirement we own: **fw-helper is the
charge-limit authority, and the UEFI battery limit setting must be left at default.**

## Rationale

This follows [ADR 0004](0004-sysfs-first-hardware-access.md) exactly. The standard sysfs
attribute is the sanctioned interface; the module parameter is upstream's own supported
escape hatch, not a hack around it. We also get UPower and any other standard consumer
seeing the same value, for free.

Implementing the custom command instead would mean shipping a second, parallel
implementation of a mechanism the kernel already exposes, in order to interoperate with a
BIOS setting we are asking the user not to use anyway.

## Consequences

**Positive**

- M2 becomes a sysfs read/write. No `ioctl`, no host command encoding, no
  `framework_system` dependency for this feature.
- Standard interface, so UPower/GNOME and monitoring tools stay consistent with us.
- Dies naturally: if Framework ever reconciles the two commands upstream, the driver loads
  on its own and our modprobe drop-in becomes a no-op.

**Negative**

- **Requires a module reload, and therefore an install step with a reboot** (or an explicit
  `modprobe -r` / `modprobe`). Not a pure userspace install.
- **Conflicts with the UEFI battery limit setting.** If the user has set one there, the
  custom command may override us unpredictably. The daemon must detect a mismatch between
  the requested and observed limit and surface it rather than silently losing.
- Ties us to a module parameter whose name is not a stable ABI. Detect its presence; do not
  assume it.

## Implementation notes

- Probe order: use `charge_control_end_threshold` if present → else check whether the
  parameter exists and tell the user how to enable it → else report the capability
  unavailable ([ADR 0003](0003-privileged-daemon-split.md)). **Never fail silently.**
- After writing a limit, read it back. A persistent mismatch means the custom command is
  fighting us — surface it as "a BIOS battery limit appears to be set", which is actionable,
  rather than as a generic error.
- Installer must not enable this without consent: it changes how the machine's charge limit
  is governed. Make it an explicit opt-in step, not a postinst side effect.

## Verification (2026-08-21)

Appended after the fact; the decision above is unchanged.

- **The drop-in takes effect at boot.** Until now the module parameter had only ever been
  proven to work after a live `modprobe -r cros_charge_control && modprobe cros_charge_control`.
  Measured across a genuine reboot: `charge_control_end_threshold` present at `100`, and
  `/sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control` reads `Y`. The
  drop-in needs no initramfs step and no note of caution.
- **Firmware does not reset the threshold across suspend.** Measured on an instrumented
  build that reads before re-applying: after an s2idle cycle the daemon logged
  `charge limit still 80%; nothing to re-apply`. The persist-and-re-apply machinery this
  ADR calls for is therefore correct but, for suspend specifically, unnecessary on this
  firmware. It remains necessary across a reboot, where the value is genuinely lost.
- **Across a reboot the value is genuinely lost, and startup restores it.** Measured over a
  journal-verified reboot with the daemon left down for 27 minutes:
  `charge_control_end_threshold` read `100` the whole time. Starting the daemon logged
  `persisted charge limit: 80%` / `charge limit is 100%, expected 80%; re-applying` /
  `re-applied charge limit 80%`, before any client command was issued. This is the case
  that makes the persistence path in this ADR load-bearing rather than defensive.
- **The write path works, and nothing overrode it.** `charge-limit 80` wrote and read back
  `80`; `ChargeError::NotApplied` did not fire. This machine has no UEFI battery limit set,
  so the override path this ADR anticipates remains *unexercised* — it is implemented and
  unit-tested against a fixture, but has never been triggered by real firmware. Treat it as
  designed-for, not demonstrated.

## Outcome (2026-08-26) — the approach does not work

Appended after the fact. The decision above is left intact as the record of what was
tried; this section records that it failed.

**The charge limit has never worked on this machine.** Not "works but unverified", not
"works except when a UEFI limit is set" — the feature has never once stopped charging.

Measured, with `probe_with_fwk_charge_control=Y` and the drop-in in place:

| | |
|---|---|
| `charge_control_end_threshold` | `80` |
| `capacity` | `100` |
| `charge_now` / `charge_full` | 4 804 000 / 4 806 000 |
| `status` across the crossing | `Charging` throughout, 120 samples, 88% → 93%, +282 mAh |

No battery limit is set in UEFI setup, so the escape hatch this ADR names — "if the user
knows they are not going to use the custom command" — was honoured and the override
happened anyway.

### Why the precondition was never satisfiable

The ADR read upstream's condition as a promise the *user* could make. It is not. The
custom EC command is not something a user opts into; it is what the EC firmware itself
runs. Leaving the UEFI setting at default does not stand the custom command down, it only
leaves it unconfigured — and an unconfigured custom command still wins over the standard
one. So forcing the binding produced a working sysfs attribute wired to the mechanism the
kernel had already judged to be the losing one. The driver's refusal to bind was the
correct verdict about this hardware, and the module parameter is an escape hatch for
machines where the user has genuinely displaced the custom command, which this is not.

### Why our own detection never fired

The ADR specified: write, read back, and report a persistent mismatch as "a BIOS battery
limit appears to be set". That anticipates firmware that **writes the value back**. This
firmware does something the design did not consider — it leaves `80` sitting in the
attribute, unchanged and readable, and ignores it. The read-back therefore always agrees,
and `ChargeError::NotApplied` cannot fire for the failure that actually occurs.

This is the general lesson, and it is worth more than the ADR was: **read-back is not
efficacy.** Every layer M2 verified — write, read-back, persistence, suspend re-apply,
reboot re-apply — sits upstream of the question "does charging stop". Five green checks,
none of them the one that mattered. The only honest test for this feature is watching
`charge_now` and `status` across the threshold.

### What this ADR got right

The `Verification (2026-08-21)` section above already flagged the override path as
"implemented and unit-tested against a fixture, but ... never been triggered by real
firmware. Treat it as designed-for, not demonstrated." That was accurate and appropriately
hedged. What went wrong was downstream of it: `docs/plan.md` marked M2 complete and the
capability probe reported `Cap::Yes`, both of which claimed more than this ADR ever did.

### Consequence

Replaced by the alternative this ADR considered and rejected — implementing Framework's
custom EC charge command — under the reconsideration trigger it set for itself:
"if interoperating with the UEFI setting becomes a requirement rather than something we
ask users to avoid." It is now a requirement, because the custom command governs the board
whether or not anybody configures it.

Until that lands, `Capabilities::probe` reports the charge limit **unavailable** whenever
the binding was forced, and points the user at the UEFI battery limit, which does work.
The modprobe drop-in stays for now: it is inert rather than harmful, and removing it is
part of the replacement's install story, not of this correction.


## Alternatives considered

- **Implement Framework's custom EC command over `/dev/cros_ec`.** Rejected as the default:
  duplicates a kernel-provided mechanism and contradicts ADR 0004. Reconsider if the module
  parameter is ever removed, or if interoperating with the UEFI setting becomes a
  requirement rather than something we ask users to avoid.
- **Shell out to `framework_tool`.** Rejected — see ADR 0004; it is not packaged for Ubuntu.
- **Ship no charge limit at all.** Rejected: it is the single most requested laptop battery
  feature and the cheapest to deliver here.

## Sources

- [PATCH v5 5/5 — power: supply: cros_charge-control: don't load if Framework control is present](https://lkml.iu.edu/2406.3/08725.html)
- [PATCH v4 5/5 — same series, v4](https://lkml.iu.edu/hypermail/linux/kernel/2406.2/00329.html)
- [PATCH v4 0/5 — ChromeOS Embedded Controller charge control driver](https://lkml.rescloud.iu.edu/2406.2/00328.html)

## Retired — 2026-09-01

*Appended, not revised.* [ADR 0012](0012-charge-limit-via-custom-ec-command.md) superseded
the decision; this records that its **artefacts** are now gone too, which is a separate
step and was deliberately not folded into that change.

Removed: `data/fw-helper-enable-charge-control`, `data/fw-helper.modprobe.conf`, and
`install-dev.sh --enable-charge-control`, which now exits with an explanation rather than
silently doing nothing.

`/etc/modprobe.d/fw-helper.conf` is now **deleted** on install rather than merely reported.
Earlier versions printed "safe to remove" and left it, which is advice rather than a fix.
The distinction matters because the drop-in is not inert in the harmless sense: it forces
`cros_charge-control` to bind, and a bound driver produces a `charge_control_end_threshold`
that accepts a value, reads it back, persists across suspend and reboot, and does not stop
charging. That appearance is the whole reason this ADR survived as long as it did. Leaving
the file in place leaves the trap armed for the next person to read that attribute and
believe it.

The removal matches on file **content**, not just the path, so a drop-in that happens to
share the name but is not ours is never touched. It was never a dpkg conffile — the package
shipped it to `/usr/share`, and only the opt-in step copied it into `/etc` — so removing it
is cleaning up after ourselves rather than deleting a user's configuration.

One thing the removal cannot do: the module parameter **survives until reboot**. A machine
that had the drop-in will still have `charge_control_end_threshold` for the rest of the
session, inert. Both the postinst and `install-dev.sh` say so, because a capability
outliving the config that enabled it is a trap this project has already been caught by once.
