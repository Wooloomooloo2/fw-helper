#!/usr/bin/env bash
# bench-probe.sh — sample what actually limits a game benchmark.
#
#   ./scripts/bench-probe.sh [seconds] [pid]
#
# Needs no root: package power comes from fw-helperd over D-Bus, GPU utilisation
# from the renderer's own fdinfo, both readable as you.
#
# Run it, then immediately start the benchmark run.
#
# Reading the output:
#   power at the PL1 setpoint + rcs high    -> power limited, raise PL1
#   power under setpoint + rcs ~100%        -> GPU bound (compute or bandwidth)
#   power under setpoint + rcs low          -> CPU bound; check the top thread
#   rcs high but act_mhz well below cur_mhz -> GPU stalling, typically memory
#
# rcs is the render engine and ccs the compute engine; both do graphics work here.

set -uo pipefail
SECS=${1:-60}
GT=/sys/class/drm/card1/device/tile0/gt0

# hwmon indices are not stable across boots; resolve the EC by name.
EC=""
for d in /sys/class/hwmon/hwmon*; do
    [[ "$(cat "$d/name" 2>/dev/null)" == cros_ec ]] && { EC=$d; break; }
done

# Pick the process doing the most render work *right now*, by rate rather than by
# accumulated total: the compositor has been running for days and would otherwise
# always win. Name-agnostic, so a game under Proton, Wine or native looks the same.
rcs_of() { grep -h "drm-cycles-rcs:" /proc/"$1"/fdinfo/* 2>/dev/null | awk '{s+=$2} END{print s+0}'; }

find_renderer() {
    local pid c candidates=() before=() after=()
    for pid in $(ls /proc | grep -E '^[0-9]+$'); do
        [[ -r /proc/$pid/fdinfo && -O /proc/$pid ]] || continue
        grep -qlm1 "drm-driver" /proc/"$pid"/fdinfo/* 2>/dev/null || continue
        candidates+=("$pid"); before+=("$(rcs_of "$pid")")
    done
    (( ${#candidates[@]} )) || return
    sleep 1
    local best="" bestd=0 i d
    for i in "${!candidates[@]}"; do
        pid=${candidates[$i]}
        [[ -d /proc/$pid ]] || continue
        d=$(( $(rcs_of "$pid") - ${before[$i]} ))
        (( d > bestd )) && { bestd=$d; best=$pid; }
    done
    echo "$best"
}
PID=${2:-$(find_renderer)}
[[ -n "$PID" ]] || { echo "no DRM client found; is the game running?" >&2; exit 1; }
echo "renderer pid $PID ($(tr -d '\0' < /proc/"$PID"/comm 2>/dev/null))"

# fdinfo reports cycles PER ENGINE (rcs render, ccs compute, bcs blit, vcs video),
# and every DRM client of the process reports the SAME drm-total-cycles-<eng> --- it is
# the GT-wide clock, not a per-client total. So the busy fraction is
#     sum(drm-cycles-<eng> over clients) / drm-total-cycles-<eng> taken ONCE.
# Summing the denominator across clients, or across idle engines, deflates the result
# several-fold and makes a fully loaded GPU look idle.
cycles() { grep -h "drm-cycles-$1:" /proc/"$PID"/fdinfo/* 2>/dev/null | awk '{s+=$2} END{print s+0}'; }
total()  { grep -hm1 "drm-total-cycles-$1:" /proc/"$PID"/fdinfo/* 2>/dev/null | head -1 | awk '{print $2+0}'; }

watts() {
    busctl get-property org.fwhelper.Daemon1 /org/fwhelper/Daemon1 \
        org.fwhelper.Daemon1 Telemetry 2>/dev/null \
        | grep -oE '"package_watts" d [0-9.]+' | awk '{print $3}'
}

setpoint=$(awk '{printf "%.0f", $1/1000000}' \
    /sys/class/powercap/intel-rapl-mmio:0/constraint_0_power_limit_uw 2>/dev/null)
echo "PL1 setpoint: ${setpoint} W"
printf "\n%5s %8s %7s %7s %8s %8s %7s  %s\n" t power rcs% ccs% act_mhz cur_mhz cpu_C top_thread

r1=$(cycles rcs); c1=$(cycles ccs); t1=$(total rcs)
for (( i = 1; i <= SECS; i++ )); do
    sleep 1
    r2=$(cycles rcs); c2=$(cycles ccs); t2=$(total rcs)
    den=$((t2-t1))
    busy=$(awk -v a=$((r2-r1)) -v b="$den" 'BEGIN{ printf "%.1f", (b>0)? 100*a/b : 0 }')
    cbusy=$(awk -v a=$((c2-c1)) -v b="$den" 'BEGIN{ printf "%.1f", (b>0)? 100*a/b : 0 }')
    r1=$r2; c1=$c2; t1=$t2

    top=$(top -H -b -n1 -p "$PID" 2>/dev/null | tail -n +8 | awk 'NR==1{printf "%s %s%%", $12, $9}')
    temp=$(cat "$EC/temp5_input" 2>/dev/null | awk '{printf "%.0f", $1/1000}')
    printf "%4ss %7s W %6s%% %6s%% %8s %8s %6s  %s\n" \
        "$i" "$(watts)" "$busy" "$cbusy" \
        "$(cat $GT/freq0/act_freq 2>/dev/null)" "$(cat $GT/freq0/cur_freq 2>/dev/null)" \
        "$temp" "$top"
done
