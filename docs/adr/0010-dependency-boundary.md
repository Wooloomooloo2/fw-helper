# 0010 — `fw-helper-core` stays dependency-free

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

M1a deliberately shipped `fw-helper-core` with zero external crates. M1b introduces D-Bus,
which means zbus and an async runtime. Measured at the point of adding them:

| Crate | Dependencies compiled on Linux |
|---|---|
| `fw-helper-core` | **0** |
| `fw-helperd` | 71 (92 in the lockfile, incl. Windows-gated crates never built here) |

That is a real step change for a project whose hardware layer currently has none, and
`fw-helperd` runs as root.

[ADR 0002](0002-rust-and-gtk4.md) chose zbus on ecosystem grounds and never addressed
footprint. This records the boundary that keeps the growth contained.

## Decision

**`fw-helper-core` takes no external dependencies. Ever.**

It contains the hardware logic — sysfs access, energy accounting, capability probing,
and later the fan curve engine and safety clamps. All of it is expressible in `std`.

External dependencies live in the crates at the edges: `fw-helperd` (zbus, tokio),
`fw-helper-gui` (gtk4, libadwaita), and `fw-helperctl` (zbus, for talking to the daemon).

Wire types are defined **in the daemon**, not in core. Core exposes plain Rust types; the
daemon converts them for D-Bus. Core does not derive `Serialize` or `zvariant::Type`, because
doing so would put serde in the hardware layer.

## Consequences

**Positive**

- The layer that touches hardware stays auditable by reading one crate. That matters
  disproportionately because it is the code that will eventually drive fans and power limits
  as root ([ADR 0006](0006-fail-safe-fan-control.md)).
- `cargo test -p fw-helper-core` needs no network, no hardware, no root, and takes
  milliseconds. Fast tests get run; slow ones get skipped.
- Supply-chain risk is concentrated in code that does IPC and drawing, not in code that
  writes to `pwm1`.
- Forces an explicit conversion at the D-Bus boundary, which is where interface-versioning
  decisions belong anyway.

**Negative**

- Hand-written conversions between core types and wire types. Mechanical, and it grows with
  the interface.
- No serde in core means no free config-file parsing. When profile persistence lands (M5),
  either the daemon owns parsing, or core gets a small hand-rolled parser. **Prefer the
  daemon owning it** — do not weaken this ADR for convenience.
- Occasionally reimplementing something small that a crate would provide. Acceptable while
  the answer stays measured in tens of lines; if it ever isn't, supersede this ADR rather
  than quietly adding a dependency.

## Enforcement

- CI runs `cargo tree -p fw-helper-core` and fails if it lists anything but itself.
- `cargo deny` checks licences and advisories across the workspace. This is a
  root-privileged daemon; an advisory check is proportionate, not ceremony.

## Note on MSRV

Adding zbus surfaced this: with `rust-version = "1.74"`, cargo's MSRV-aware resolver silently
selected **zbus 3.15.2** while 5.19.0 was available — a three-year-old API we would have had
to migrate off later, chosen for us without comment. MSRV was raised to **1.85** to get the
current release.

Raised again to **1.89** in 0.7.2 for zbus 6, which RustSec's advisories against the
crates zbus 5 was built from (RUSTSEC-2026-0370..0372) made necessary.

The lesson generalises: **check what the resolver actually picked, not just that it
resolved.** A build that succeeds against stale dependencies looks identical to one against
current ones.

## Alternatives considered

- **Let core depend on serde/zvariant for convenient wire types.** Rejected: it puts a
  derive-macro dependency chain into the hardware layer to save mechanical conversion code,
  and would make core's tests depend on the network.
- **One crate for everything.** Rejected: no boundary to enforce, and the fast hardware tests
  would be gated behind compiling a GUI toolkit.
- **Vendor zbus.** Rejected as disproportionate; `cargo deny` plus a lockfile is the
  appropriate control here.
