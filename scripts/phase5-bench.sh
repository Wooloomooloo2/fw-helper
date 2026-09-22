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

[ "$(cat /sys/class/power_supply/AC*/online 2>/dev/null | head -1)" = 1 ] || {
  echo "!! ON BATTERY. The profile is re-applied on a power-source change, which would"
  echo "!! silently reset the park level mid-run. Plug in."; exit 1; }
# NOT `status`: it falls back to reading sysfs directly when the daemon is absent and
# exits 0, so it is useless as a liveness check. `profile` needs D-Bus and exits 1.
# Phase 0's probe stops the daemon deliberately, so arriving here with it down is the
# normal way to trip this.
$CTL profile >/dev/null 2>&1 || {
  echo "!! fw-helperd is not answering, and every step here needs it."
  echo "!!   sudo systemctl start fw-helperd"
  echo "!! (scratchpad/tune-levers-probe.sh stops it on purpose and does not restart it.)"
  exit 1
}
[ -d "$SOTTR" ] || { echo "!! no SOTTR output directory at $SOTTR"; exit 1; }

# The daemon can be live, answering, and still be a build that has never heard of the
# profile this benchmark is built on. Check the thing we actually depend on, and check
# the binary behind it -- `apt`/`install` both no-op silently on an unchanged version,
# so the install log is not evidence.
if ! $CTL profile 2>/dev/null | grep -qE '^\*?\s*retro$'; then
  echo "!! the running fw-helperd does not know the \`retro\` profile, so this"
  echo "!! benchmark cannot fix its baseline. It is serving an older binary:"
  echo "!!   cargo build --release --all && sudo ./scripts/install-dev.sh --systemd"
  exit 1
fi
if [ -r /usr/libexec/fw-helperd ] && [ -r "$REPO/target/release/fw-helperd" ]; then
  a=$(md5sum /usr/libexec/fw-helperd | cut -d' ' -f1)
  b=$(md5sum "$REPO/target/release/fw-helperd" | cut -d' ' -f1)
  [ "$a" = "$b" ] || echo "   note: installed fw-helperd differs from target/release/ -- \
you are benchmarking the installed one"
fi

mkdir -p "$OUT"

echo "== arm: $ARM"
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
echo "     PL1 $($CTL status 2>/dev/null | grep -i 'power limit' | head -1 | tr -s ' ')"

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
  exit 1
fi
echo "$SESSION" > "$OUT/session"
echo
echo "-- arm $ARM done. Results in scratchpad/phase5/$ARM/"
