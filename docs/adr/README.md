# Architecture Decision Records

Format: [MADR](https://adr.github.io/madr/) (lightly adapted). One decision per file,
numbered sequentially, never renumbered. Superseding a decision means adding a new ADR
and setting the old one's status to `Superseded by NNNN`.

| # | Decision | Status |
|---|---|---|
| [0001](0001-separate-repository.md) | Separate repository, not a fork of G-Helper | Accepted |
| [0002](0002-rust-and-gtk4.md) | Rust for the daemon, GTK4/libadwaita for the GUI | Accepted |
| [0003](0003-privileged-daemon-split.md) | Privileged daemon + unprivileged GUI over D-Bus | Accepted |
| [0004](0004-sysfs-first-hardware-access.md) | Kernel sysfs first, raw EC commands only as fallback | Accepted |
| [0005](0005-delegate-to-power-profiles-daemon.md) | Delegate profile switching to power-profiles-daemon | Accepted |
| [0006](0006-fail-safe-fan-control.md) | Fan control must fail safe, never fail silent | Accepted (point 4 amended by 0011) |
| [0007](0007-no-undervolting.md) | No undervolting; RAPL power limits only | Accepted |
| [0008](0008-charge-limit-via-module-parameter.md) | Charge limit via `probe_with_fwk_charge_control` | Superseded by 0012 |
| [0009](0009-power-telemetry-rate-limited-and-quantized.md) | Power telemetry rate-limited and quantized | Accepted |
| [0010](0010-dependency-boundary.md) | `fw-helper-core` stays dependency-free | Accepted |
| [0011](0011-quiet-is-a-legitimate-choice.md) | Fan floor tracks firmware's behaviour, not CPU safety | Accepted |
| [0012](0012-charge-limit-via-custom-ec-command.md) | Charge limit via Framework's custom EC command | Accepted |
| [0013](0013-cap-sys-ptrace-for-gpu-load.md) | The daemon holds `CAP_SYS_PTRACE`, and nothing else | Accepted |
