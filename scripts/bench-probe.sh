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
#   power at the PL1 setpoint + GPU busy high   -> power limited, raise PL1
#   power well under setpoint + GPU busy ~100%  -> GPU bound (compute or bandwidth)
#   power well under setpoint + GPU busy low    -> CPU bound; check the top thread
#   GPU busy high but act_freq below max        -> GPU stalling, typically memory

set -uo pipefail
SECS=${1:-60}
GT=/sys/class/drm/card1/device/tile0/gt0

# hwmon indices are not stable across boots; resolve the EC by name.
EC=""
for d in /sys/class/hwmon/hwmon*; do
    [[ "$(cat "$d/name" 2>/dev/null)" == cros_ec ]] && { EC=$d; break; }
done

find_renderer() {
    local best="" bestn=0 pid n
    for pid in $(pgrep -u "$(id -u)" -f -i 'horizon|\.exe' 2>/dev/null); do
        n=$(grep -ls "drm-driver" /proc/"$pid"/fdinfo/* 2>/dev/null | wc -l)
        (( n > bestn )) && { bestn=$n; best=$pid; }
    done
    echo "$best"
}
PID=${2:-$(find_renderer)}
[[ -n "$PID" ]] || { echo "no DRM client found; is the game running?" >&2; exit 1; }
echo "renderer pid $PID ($(tr -d '\0' < /proc/"$PID"/comm 2>/dev/null))"

cycles() { grep -h "drm-cycles" /proc/"$PID"/fdinfo/* 2>/dev/null | awk '{s+=$2} END{print s+0}'; }
total()  { grep -h "drm-total-cycles" /proc/"$PID"/fdinfo/* 2>/dev/null | awk '{s+=$2} END{print s+0}'; }

watts() {
    busctl get-property org.fwhelper.Daemon1 /org/fwhelper/Daemon1 \
        org.fwhelper.Daemon1 Telemetry 2>/dev/null \
        | grep -oE '"package_watts" d [0-9.]+' | awk '{print $3}'
}

setpoint=$(awk '{printf "%.0f", $1/1000000}' \
    /sys/class/powercap/intel-rapl-mmio:0/constraint_0_power_limit_uw 2>/dev/null)
echo "PL1 setpoint: ${setpoint} W"
printf "\n%5s %8s %7s %8s %8s %7s  %s\n" t power gpu% act_mhz cur_mhz cpu_C top_thread

c1=$(cycles); t1=$(total)
for (( i = 1; i <= SECS; i++ )); do
    sleep 1
    c2=$(cycles); t2=$(total)
    busy=$(awk -v a=$((c2-c1)) -v b=$((t2-t1)) 'BEGIN{ printf "%.1f", (b>0)? 100*a/b : 0 }')
    c1=$c2; t1=$t2

    top=$(top -H -b -n1 -p "$PID" 2>/dev/null | tail -n +8 | awk 'NR==1{printf "%s %s%%", $12, $9}')
    temp=$(cat "$EC/temp5_input" 2>/dev/null | awk '{printf "%.0f", $1/1000}')
    printf "%4ss %7s W %6s%% %8s %8s %6s  %s\n" \
        "$i" "$(watts)" "$busy" \
        "$(cat $GT/freq0/act_freq 2>/dev/null)" "$(cat $GT/freq0/cur_freq 2>/dev/null)" \
        "$temp" "$top"
done
