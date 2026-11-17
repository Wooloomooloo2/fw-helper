# ADR 0016 — Benchmark runs live in the user's home, and frames come from MangoHud

**Status:** accepted (2026-11-17)

## Context

The Windows sibling grew a benchmarking workflow (its ADRs 0017-0018 and plan phases
1-3): FPS in every recorded second, a per-frame log for 1 % lows, run notes, an HTML run
summary, and compare projects. Bringing it to Linux raised two questions the Windows
build answers differently because it is one process running as the user.

**Who owns the files.** Here recording happens in the root daemon (ADR 0003, ADR 0009:
`energy_uj` is root-only), into `/var/lib/fw-helper/sessions`, which it prunes to twenty.
Notes, reports and projects are things a person writes and keeps; an unprivileged client
cannot write the daemon's directory, and should not be able to.

**Where frames come from.** Windows counts presents from ETW, which any process may
watch. Linux has no equivalent: a frame is presented inside the game's process through
the Vulkan loader, and only a layer loaded into that process sees it. `perf` is closed
(`perf_event_paranoid=4`, and `@debug` is outside the unit's syscall filter).

## Decision

1. **Everything a user keeps is in `~/Documents/FW Benchmark`** (the XDG documents
   directory), the same folder the Windows build uses at the same user's request. A
   daemon recording reaches it by being **exported** by a client - the GUI when its Stop
   button or any other stop is seen, the CLI on `record stop`/`toggle`, or explicitly
   with Save / `bench save`. Export is idempotent and keeps typed notes, so the GUI and
   CLI may both do it without coordinating.

2. **Frames come from MangoHud's own log.** The shipped MangoHud config, already the
   recommended way to get numbers inside a fullscreen game, now also logs one row per
   frame (`log_interval=0`, `autostart_log=1`) to `~/.local/share/fw-helper/mangohud`.
   MangoHud 0.8.4 writes the log incrementally (measured 2026-11-16: 35 KB -> 63 KB in
   4 s under vkcube), so the GUI reads the newest log's tail for a live frame rate, and
   export joins frames into each recorded second. A row's `elapsed` is nanoseconds since
   logging began, so rows are placed relative to the file's mtime (when its newest row
   was written) rather than the second-resolution local time in its name.

3. **The daemon never reads the user's home**, and its recordings carry `fps`/`fps_app`
   columns it always leaves empty. The client fills them in its copy.

4. File formats follow the Windows build where they overlap: notes are camelCase JSON
   with the same keys; compare projects are `{name, baseline, runs: [{file, name}]}`.
   Core gets a std-only JSON reader/writer rather than a dependency (ADR 0010).

## Consequences

- A game launched without fw-helper's MangoHud config has no frame rate in fw-helper.
  The FPS card says so. A user's own MangoHud config can opt in with four lines.
- MangoHud does not create `output_folder`, and logs nothing without it ("Failed to
  write log file" on the game's stderr). Every fw-helper client creates it.
- Logging every frame costs disk: ~20 MB an hour at 60 fps, for every game run with the
  config, recorded or not. So a raw log is deleted as soon as nothing needs it: once its
  game has stopped writing it for ten minutes, unless a daemon recording not yet saved
  overlaps it; 14 days is the hard limit. A saved run holds its own `.frames.csv`, and
  re-saving it after its raw log has gone keeps the frame rate it already had. Every
  frame, not one row a second, because the 1 % and 0.1 % lows are computed from frame
  times - and stutter that a one-second average hides is what separated settings in
  this project's own game runs (CP2077 47 -> 49 fps average at a CPU cap, with visibly
  worse pacing). The Windows build records every frame too, through PresentMon.
- An unsaved recording still opens in the Monitor page from the daemon's directory, but
  has no frame rate until saved.
- The mtime anchor is wrong if a log is touched after MangoHud finished with it (copied
  with a new mtime, say). The FPS would then land in the wrong seconds. Not guarded.
- Not verified: that a GNOME custom shortcut (the Ctrl+Shift+F5 recording toggle) fires
  while a fullscreen game holds the keyboard. Xwayland games can inhibit compositor
  shortcuts; GNOME asks before allowing that.
