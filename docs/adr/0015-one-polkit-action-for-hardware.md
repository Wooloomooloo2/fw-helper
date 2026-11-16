# ADR 0015 — One polkit action for every hardware setting

**Status:** accepted (2026-11-16). Supersedes the per-action polkit split in ADR 0003.

## Context

ADR 0003 gave each knob its own polkit action — charge limit, fan, power limit, and
later tuning — all `auth_admin_keep`, on the argument that "an admin can allow
charge-limit changes without allowing power-limit changes".

In use, the user reported a password prompt "every time I change any setting". The
cause was the split itself. polkit retains an `auth_admin_keep` grant **per action**
(and for about five minutes), so:

- each of the four groups in the window asked for the password the first time it was
  touched, even seconds after another group had been unlocked;
- `SetProfile` sets a power limit and takes the fan, so it checked two actions in
  sequence and raised **two dialogs back to back** for one click.

That is the failure every `auth_admin_keep` comment in the policy was written to avoid:
a prompt frequent enough to be dismissed without reading. The distinction it bought is
one nobody configures on a single-user laptop, and anyone who does want it can still
write a polkit rule keyed on the method — though not on the action, which is the cost.

## Decision

Every hardware-changing method checks one action, **`org.fwhelper.configure`**,
`auth_admin_keep` for the active session and `auth_admin` otherwise. `org.fwhelper.record`
stays separate, because it is granted without a prompt and changes no hardware.

## Consequences

- One password unlocks every control for polkit's retention window; a profile switch
  prompts at most once.
- An admin can no longer grant, say, the charge limit while withholding the fan through
  polkit action ids. Considered and accepted.
- The action ids `org.fwhelper.set-charge-limit`, `set-fan`, `set-power-limit` and
  `set-tuning` no longer exist. A local rule naming them stops matching and falls back to
  the default, which asks — so the change fails safe, never open.
- Not changed: the retention window itself is polkit's, not ours. Prompting less often
  than every ~5 minutes of inactivity needs a local rule (`polkit.Result.YES` for
  `org.fwhelper.configure`), which remains the user's choice to make.
