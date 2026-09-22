#!/usr/bin/env bash
# phase5-bench.sh ARM  -  one arm of the M9 Phase 5 parking benchmark.
#
#   ARM = none | lpe | p-only
#
# The experiment varies EXACTLY ONE THING: the park level. Everything else -- PL1,
# the PPD position, the fan curve, the game's own settings -- is held fixed, because
# comparing `retro` against `balanced` would vary PL1, PPD, curve and parking at once
# and make any result unattributable.
#
# Usage, one arm at a time, all three on mains in one sitting:
#
#     ./scripts/phase5-bench.sh none
#     ./scripts/phase5-bench.sh lpe
#     ./scripts/phase5-bench.sh p-only
#     ./scripts/phase5-report.sh
#
# Run the arms in a randomised order if you repeat them: a machine heat-soaks, and
# three arms in a fixed order confound "later" with "hotter".
set -uo pipefail

ARM=${1:-}
case "$ARM" in
  none|lpe|p-only) ;;
  *) echo "usage: $0 none|lpe|p-only"; exit 2;;
esac

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT="${PHASE5_OUT:-$REPO/scratchpad/phase5}/$ARM"
SOTTR="$HOME/.steam/steam/steamapps/compatdata/750920/pfx/drive_c/users/steamuser/Documents/Shadow of the Tomb Raider"
CTL=${CTL:-fw-helperctl}

mkdir -p "$OUT"
# Log everything from here, INCLUDING the prechecks -- they are what keeps failing, and
# a check that aborts before logging starts is a check whose failure nobody can read.
# `exec > >(tee)` keeps the output on screen and in the file at once.
LOG="$OUT/bench.log"
exec > >(tee "$LOG") 2>&1

[ "$(cat /sys/class/power_supply/AC*/online 2>/dev/null | head -1)" = 1 ] || {
  echo "!! ON BATTERY. The profile is re-applied on a power-source change, which would"
  echo "!! silently reset the park level mid-run. Plug in."; exit 1; }
# NOT `status`: it falls back to reading sysfs directly when the daemon is absent and
# exits 0, so it is useless as a liveness check. `profile` needs D-Bus and exits 1.
#
# Retried, because `systemctl restart` returns as soon as the unit is "started" and the
# daemon still has to probe hardware, read profiles.d and claim the bus name after that.
# Running this straight after a reinstall raced it, and the failure surfaced as the
# profile check below reporting a missing profile -- which is why that check now gets
# the error text instead of discarding it.
PROFILES=""
for _ in $(seq 20); do
  if PROFILES=$($CTL profile 2>&1); then break; fi
  sleep 0.5
done
if [ -z "$PROFILES" ] || ! $CTL profile >/dev/null 2>&1; then
  echo "!! fw-helperd is not answering after 10s, and every step here needs it."
  echo "!! Last error was:"
  printf '%s\n' "$PROFILES" | sed 's/^/!!   /' | head -3
  echo "!!   sudo systemctl start fw-helperd"
  echo "!! (scratchpad/tune-levers-probe.sh stops it on purpose and does not restart it.)"
  exit 1
fi
[ -d "$SOTTR" ] || { echo "!! no SOTTR output directory at $SOTTR"; exit 1; }

# The daemon can be live, answering, and still be a build that has never heard of the
# profile this benchmark is built on. Check the thing we actually depend on, and check
# the binary behind it -- `apt`/`install` both no-op silently on an unchanged version,
# so the install log is not evidence.
if ! printf '%s\n' "$PROFILES" | grep -qE '^\*?[[:space:]]*retro$'; then
  echo "!! the running fw-helperd does not know the \`retro\` profile, so this"
  echo "!! benchmark cannot fix its baseline. It is serving an older binary:"
  echo "!!   cargo build --release --all && sudo ./scripts/install-dev.sh --systemd"
  echo "!! What it did report:"
  printf '%s\n' "$PROFILES" | sed 's/^/!!   /' | head -12
  exit 1
fi
if [ -r /usr/libexec/fw-helperd ] && [ -r "$REPO/target/release/fw-helperd" ]; then
  a=$(md5sum /usr/libexec/fw-helperd | cut -d' ' -f1)
  b=$(md5sum "$REPO/target/release/fw-helperd" | cut -d' ' -f1)
  [ "$a" = "$b" ] || echo "   note: installed fw-helperd differs from target/release/ -- \
you are benchmarking the installed one"
fi

echo "== arm: $ARM   $(date -Is)"
# `retro` first, every time: it fixes PL1 35 W, PPD performance and the curve. Applying
# it also SETS a park level (lpe), so the explicit override below has to come after it,
# not before -- the same ordering trap as `fan auto` versus a profile.
echo "-- fixing the baseline (profile retro: PL1 35 W, PPD performance)"
$CTL profile retro >/dev/null || { echo "!! could not apply retro"; exit 1; }
sleep 2
echo "-- setting park level to $ARM"
$CTL tune park "$ARM" >/dev/null || { echo "!! could not set park level"; exit 1; }

echo "-- machine now:"
$CTL tune 2>/dev/null | sed -n '/^CPU topology/,/^$/p' | sed 's/^/     /'
echo "    $($CTL status 2>/dev/null | grep -iE '^ +power limit +[0-9]' | tr -s ' ')"
echo "    profile: $($CTL profile 2>/dev/null | sed -n 's/^\* *//p')"

# Mark the clock so we can find the files this run produces. SOTTR names them by
# timestamp, so "newer than this" is unambiguous.
MARK=$(mktemp); trap 'rm -f "$MARK"' EXIT
sleep 1

SESSION="sottr-$ARM-$(date +%H%M%S)"
echo
echo "-- recording as $SESSION"
$CTL record start "$SESSION" >/dev/null || { echo "!! could not start recording"; exit 1; }

echo
echo "   >>> Run the in-game benchmark now (Options -> Graphics -> Run Benchmark)."
echo "   >>> Note the CPU / GPU fps split on the RESULT SCREEN - it is not in any file."
echo "   >>> Press Enter here when the benchmark has finished."
read -r _

$CTL record stop >/dev/null
echo "-- recording stopped"

# Collect whatever SOTTR wrote after the mark.
found=0
while IFS= read -r f; do
  cp -v "$f" "$OUT/" | sed 's/^/     /'
  found=1
done < <(find "$SOTTR" -maxdepth 1 -name 'SOTTR_*.txt' -newer "$MARK" 2>/dev/null)

if [ "$found" = 0 ]; then
  echo "!! SOTTR wrote no new result files. Did the benchmark actually run to the end?"
  echo "!! (this log: $LOG)"
  exit 1
fi
echo "$SESSION" > "$OUT/session"
echo
echo "-- arm $ARM done. Results and this log are in scratchpad/phase5/$ARM/"
