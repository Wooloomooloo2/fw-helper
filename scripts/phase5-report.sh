#!/usr/bin/env bash
# phase5-report.sh - compare the parking arms once all three have run.
#
# Reports average fps, 1% low and 0.1% low from SOTTR's per-frame deltas, plus what our
# own recorder saw. The lows matter more than the average here: a thread stranded on a
# slow core shows up as frame-time spikes before it shows up as a lower mean.
set -uo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "${PHASE5_OUT:-$REPO/scratchpad/phase5}"
cd "${PHASE5_OUT:-$REPO/scratchpad/phase5}" || { echo "no results yet"; exit 1; }

python3 - "$@" <<'PY'
import glob, os, statistics, sys

def lows(path):
    """Average fps, and the mean fps of the slowest 1% and 0.1% of frames."""
    d = []
    with open(path, errors="replace") as fh:
        next(fh, None)                      # header
        for line in fh:
            parts = line.split(",")
            if len(parts) < 3:
                continue
            try:
                delta = float(parts[2])
            except ValueError:
                continue
            # A zero delta is the first frame, not an infinitely fast one.
            if delta > 0:
                d.append(delta)
    if not d:
        return None
    # Drop the first 2% as load-in: the benchmark starts its capture before the scene
    # has settled, and those frames are not what any arm is being judged on.
    d = d[max(1, len(d) // 50):]
    worst = sorted(d, reverse=True)
    def fps(xs):
        return 1000.0 / (sum(xs) / len(xs))
    n1 = max(1, len(worst) // 100)
    n01 = max(1, len(worst) // 1000)
    return len(d), fps(d), fps(worst[:n1]), fps(worst[:n01])

ARMS = ["none", "lpe", "p-only"]
rows = []
for arm in ARMS:
    ft = sorted(glob.glob(f"{arm}/SOTTR_*frametimes*.txt"))
    if not ft:
        continue
    r = lows(ft[-1])
    if not r:
        continue
    frames, avg, p1, p01 = r
    rows.append((arm, frames, avg, p1, p01, os.path.basename(ft[-1])))

if not rows:
    print("no frametime files found under scratchpad/phase5/*/")
    sys.exit(1)

print(f"{'arm':<8} {'frames':>7} {'avg fps':>9} {'1% low':>8} {'0.1% low':>9}")
print("-" * 46)
for arm, frames, avg, p1, p01 in [(r[0], r[1], r[2], r[3], r[4]) for r in rows]:
    print(f"{arm:<8} {frames:>7} {avg:>9.2f} {p1:>8.2f} {p01:>9.2f}")

base = next((r for r in rows if r[0] == "none"), None)
if base and len(rows) > 1:
    print()
    print("against `none`:")
    for arm, _, avg, p1, p01, _f in rows:
        if arm == "none":
            continue
        print(f"  {arm:<8} avg {100*(avg/base[2]-1):+6.2f}%   "
              f"1% low {100*(p1/base[3]-1):+6.2f}%   "
              f"0.1% low {100*(p01/base[4]-1):+6.2f}%")
    print()
    print("Read this against the run-to-run spread, which nothing here measures: one run")
    print("per arm cannot tell +2% from noise. If the arms are within a couple of percent,")
    print("the honest answer is 'no measurable effect on this title', and that is a")
    print("result worth having -- `retro` would then be a hypothesis that did not pay off.")
PY

echo
echo "== what our own recorder saw"
for arm in none lpe p-only; do
    [ -f "$arm/session" ] || continue
    s=$(cat "$arm/session")
    # The recorder appends its own timestamp to the label, so match on the prefix.
    f=$(ls -1t /var/lib/fw-helper/sessions/"$s"*.csv 2>/dev/null | head -1)
    [ -n "$f" ] && [ -r "$f" ] || { printf '  %-8s no readable session for %s\n' "$arm" "$s"; continue; }
    python3 - "$arm" "$f" <<'PY'
import csv, sys, statistics
arm, path = sys.argv[1], sys.argv[2]
cols = {}
with open(path) as fh:
    for row in csv.DictReader(fh):
        for k, v in row.items():
            try:
                cols.setdefault(k, []).append(float(v))
            except (TypeError, ValueError):
                pass
def m(k):
    xs = [x for x in cols.get(k, []) if x == x]
    return statistics.median(xs) if xs else float("nan")
print(f"  {arm:<8} gpu {m('gpu_pct'):5.1f}% @{m('gpu_mhz'):6.0f}MHz  "
      f"cpu {m('cpu_pct'):5.1f}% @{m('cpu_mhz'):6.0f}MHz  "
      f"pkg {m('package_w'):5.2f}W  peci {m('peci_c'):5.1f}C  fan {m('fan_rpm'):.0f}rpm")
PY
done
echo
echo "  A high gpu% means the title was GPU-bound and parking had nothing to win."
