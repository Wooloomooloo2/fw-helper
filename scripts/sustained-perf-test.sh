#!/usr/bin/env bash
# sustained-perf-test.sh — 5 minutes of maximum load, sampled every 10 s:
# package power AND a performance score for every interval.
#
# Q6 answered "does PL1 govern sustained draw" with two steady-state means. It could not
# show the SHAPE of a run: the turbo shoulder, where PL1 engages, whether throughput droops
# as the chassis heat-soaks. That shape is the whole question when asking whether a higher
# power limit buys anything.
#
#   sudo ./scripts/sustained-perf-test.sh [options]
#     --pl1 W        PL1 setpoint (default: the zone's max_power_uw, 25 W on this board)
#     --secs N       total load duration (default 300)
#     --interval N   sample window (default 10)
#     --fan DUTY     take the fan to a fixed duty; 0 or 30-255 (default: leave it to the EC)
#     --method M     stress-ng --cpu-method (default matrixprod, as M0 and Q6 used)
#     --label NAME   tag for the CSV filename
#     --out PATH     CSV path (default ./perf-<label>-<timestamp>.csv)
#     --monitor      generate NO load and change NOTHING — just log power, temperature,
#                    fan and frequency while some other workload runs. This is the mode
#                    for benchmarking a game: leave fw-helperd running, apply a profile
#                    with `fw-helperctl profile turbo`, start the benchmark, and let this
#                    record what the machine actually did. The game reports the score;
#                    this reports what it cost.
#
# Restores PL1, the power profile, the fan and fw-helperd on every exit path.

set -uo pipefail

PL1_W=""; SECS=300; INTERVAL=10; FAN_DUTY=""; METHOD=matrixprod; LABEL=""; OUT=""
MONITOR=no
while (( $# )); do
    case "$1" in
        --pl1)      PL1_W=$2; shift 2 ;;
        --secs)     SECS=$2; shift 2 ;;
        --interval) INTERVAL=$2; shift 2 ;;
        --fan)      FAN_DUTY=$2; shift 2 ;;
        --method)   METHOD=$2; shift 2 ;;
        --label)    LABEL=$2; shift 2 ;;
        --out)      OUT=$2; shift 2 ;;
        --monitor)  MONITOR=yes; shift ;;
        -h|--help)  sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

# Checked here, before PL1_W picks up its default — after that it is always non-empty
# and this would reject every monitor run.
if [[ "$MONITOR" == yes ]]; then
    for conflict in "$PL1_W:--pl1" "$FAN_DUTY:--fan"; do
        [[ -n "${conflict%%:*}" ]] && {
            echo "ERROR: ${conflict#*:} changes machine state, which --monitor exists not to do." >&2
            echo "       Set the state you want first (fw-helperctl profile <name>), then monitor it." >&2
            exit 2
        }
    done
fi

[[ $EUID -eq 0 ]] || { echo "ERROR: needs root (energy_uj is 0400 — PLATYPUS mitigation)" >&2; exit 1; }
if [[ "$MONITOR" == no ]]; then
    command -v stress-ng >/dev/null || {
        echo "ERROR: stress-ng is required. A shell busy-loop is not power-dense enough to" >&2
        echo "       reach the limit, and gives no bogo-op count to score. apt install stress-ng" >&2
        exit 1
    }
fi

ZONE=/sys/class/powercap/intel-rapl-mmio:0
[[ -e $ZONE/constraint_0_power_limit_uw ]] || { echo "ERROR: no MMIO RAPL zone" >&2; exit 1; }

# hwmon indices are not stable across boots — resolve by name.
find_hwmon() { for d in /sys/class/hwmon/hwmon*; do [[ "$(cat "$d/name" 2>/dev/null)" == "$1" ]] && { echo "$d"; return; }; done; }
EC=$(find_hwmon cros_ec)
CORETEMP=$(find_hwmon coretemp)
[[ -n "$EC" ]] || echo "WARNING: no cros_ec hwmon — no temperatures, no fan, no thermal guard" >&2

# Map the EC's labelled sensors to their temp*_input paths.
declare -A T=()
if [[ -n "$EC" ]]; then
    for f in "$EC"/temp*_label; do
        [[ -e "$f" ]] || continue
        T["$(cat "$f")"]="${f%_label}_input"
    done
fi
read_c() { local p=${T[$1]:-}; [[ -n "$p" ]] && awk -v v="$(cat "$p" 2>/dev/null || echo 0)" 'BEGIN{printf "%.1f", v/1000}' || echo ""; }

# --- guards -----------------------------------------------------------------
# The CPU protects itself at Tjmax (100 C) and peci-temp's own "crit" reads 119.8 C, above
# Tjmax, so it is not a usable limit — high peci is data, not a fault. What has no
# protection of its own is the battery and the board sensors. Abort 5 C short of their crit.
BATT_ABORT_C=45.0
BOARD_ABORT_C=82.0

MAXE=$(cat $ZONE/max_energy_range_uj)
ORIG_PL1=$(cat $ZONE/constraint_0_power_limit_uw)
ZONE_MAX_W=$(( $(cat $ZONE/constraint_0_max_power_uw) / 1000000 ))
[[ -z "$PL1_W" ]] && PL1_W=$ZONE_MAX_W

if (( PL1_W > ZONE_MAX_W )); then
    printf 'NOTE: %s W is above the zone-declared max of %s W.\n' "$PL1_W" "$ZONE_MAX_W"
    printf '      Firmware has itself parked 33 W here, so the field is a declaration, not a\n'
    printf '      bound — but whether OUR write is honoured is exactly what is untested.\n\n'
fi

if [[ -n "$FAN_DUTY" ]]; then
    if (( FAN_DUTY > 0 && FAN_DUTY < 30 )); then
        echo "ERROR: duty $FAN_DUTY is a stopped fan, not a slow one (stiction is between 20 and 30)." >&2
        exit 2
    fi
    (( FAN_DUTY > 255 )) && { echo "ERROR: duty must be 0 or 30-255" >&2; exit 2; }
    [[ -n "$EC" ]] || { echo "ERROR: --fan needs the cros_ec hwmon" >&2; exit 1; }
fi

# --- state we borrow and must give back -------------------------------------
DAEMON_WAS_ACTIVE=no
ORIG_PROFILE=""
FAN_TAKEN=no
FAN_WATCHDOG=""
LOAD_PID=""
WORK=$(mktemp -d /var/tmp/fw-perf-XXXXXX)

cleanup() {
    local rc=$?
    trap - EXIT INT TERM HUP
    [[ -n "$LOAD_PID" ]] && kill -TERM "$LOAD_PID" 2>/dev/null
    echo
    echo "[restore]"
    if [[ "$FAN_TAKEN" == yes ]]; then
        echo 2 > "$EC/pwm1_enable" 2>/dev/null
        printf '  fan -> EC (pwm1_enable reads %s)\n' "$(cat "$EC/pwm1_enable" 2>/dev/null)"
    fi
    [[ -n "$FAN_WATCHDOG" ]] && kill "$FAN_WATCHDOG" 2>/dev/null
    echo "$ORIG_PL1" > $ZONE/constraint_0_power_limit_uw 2>/dev/null
    printf '  PL1 -> %s W (reads %s W)\n' "$((ORIG_PL1/1000000))" "$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))"
    if [[ -n "$ORIG_PROFILE" ]] && command -v powerprofilesctl >/dev/null; then
        powerprofilesctl set "$ORIG_PROFILE" 2>/dev/null && printf '  power profile -> %s\n' "$ORIG_PROFILE"
    fi
    if [[ "$DAEMON_WAS_ACTIVE" == yes ]]; then
        systemctl start fw-helperd 2>/dev/null && echo "  fw-helperd restarted"
    fi
    rm -rf "$WORK"
    [[ -n "$OUT" && -f "$OUT" && -n "${SUDO_USER:-}" ]] && chown "$SUDO_USER" "$OUT" 2>/dev/null
    [[ -f "$OUT" ]] && printf '  CSV: %s\n' "$OUT"
    return $rc
}
trap cleanup EXIT INT TERM HUP

# --- set up maximum performance ---------------------------------------------
if [[ "$MONITOR" == yes ]]; then
    PL1_READBACK=$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))
    printf 'monitor mode: observing only. fw-helperd, PL1 (%s W), the profile and the fan\n' "$PL1_READBACK"
    printf '              are left exactly as they are.\n'
fi

# fw-helperd owns PL1 and re-asserts its own setpoint within seconds — it would fight this
# script exactly the way it fights q6-pl1-load-test.sh. Stop it, and give it back after.
if [[ "$MONITOR" == yes ]]; then
    :
elif systemctl is-active --quiet fw-helperd 2>/dev/null; then
    DAEMON_WAS_ACTIVE=yes
    echo "stopping fw-helperd (it re-asserts its own PL1 setpoint and would fight this test)"
    systemctl stop fw-helperd
    sleep 1
elif pgrep -x fw-helperd >/dev/null; then
    echo "ERROR: an fw-helperd is running but not under systemd. It will re-assert PL1 and" >&2
    echo "       invalidate this test. Stop it first: sudo pkill -x fw-helperd" >&2
    exit 1
fi

# Never write platform_profile directly — delegate to PPD (ADR 0005).
if [[ "$MONITOR" == no ]] && command -v powerprofilesctl >/dev/null; then
    ORIG_PROFILE=$(powerprofilesctl get 2>/dev/null)
    if powerprofilesctl set performance 2>/dev/null; then
        echo "power profile: $ORIG_PROFILE -> performance"
        # Switching platform_profile makes firmware re-derive PL1 asynchronously, so let it
        # settle before we set ours — and we re-assert every interval regardless.
        sleep 3
    else
        echo "WARNING: could not set the performance profile; running at $ORIG_PROFILE" >&2
        ORIG_PROFILE=""
    fi
fi

if [[ "$MONITOR" == no ]]; then
echo "$(( PL1_W * 1000000 ))" > $ZONE/constraint_0_power_limit_uw 2>/dev/null
PL1_READBACK=$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))
fi
if [[ "$MONITOR" == no ]] && (( PL1_READBACK != PL1_W )); then
    printf 'WARNING: wrote %s W, register reads %s W — firmware refused the value.\n' "$PL1_W" "$PL1_READBACK"
    printf '         The run continues at %s W; that refusal is itself the answer.\n\n' "$PL1_READBACK"
fi

if [[ -n "$FAN_DUTY" ]]; then
    # A watchdog that survives kill -9 on this script. pwm1_enable=1 means the EC has
    # stopped managing the fan and holds the last duty FOREVER — through a crash, a
    # deadlock, a suspend (ADR 0006). The EXIT trap covers every ordinary path; this
    # covers the one it cannot.
    setsid bash -c "while kill -0 $$ 2>/dev/null; do sleep 1; done; echo 2 > $EC/pwm1_enable" \
        >/dev/null 2>&1 &
    FAN_WATCHDOG=$!
    # pwm1 cannot be pre-loaded: writing it while pwm1_enable=2 returns EOPNOTSUPP. The
    # takeover window is real, so keep these two writes adjacent.
    echo 1 > "$EC/pwm1_enable"
    echo "$FAN_DUTY" > "$EC/pwm1"
    FAN_TAKEN=yes
    printf 'fan: manual, duty %s (watchdog pid %s restores the EC if this script is killed)\n' \
        "$FAN_DUTY" "$FAN_WATCHDOG"
fi

# --- measurement primitives --------------------------------------------------
avg_mhz() {
    local sum=0 n=0 f
    for f in /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq; do
        [[ -r "$f" ]] || continue
        sum=$(( sum + $(cat "$f") )); n=$(( n + 1 ))
    done
    (( n > 0 )) && echo $(( sum / n / 1000 )) || echo 0
}
coretemp_max() {
    local m=0 v f
    [[ -n "$CORETEMP" ]] || { echo ""; return; }
    for f in "$CORETEMP"/temp*_input; do
        [[ -r "$f" ]] || continue
        v=$(cat "$f"); (( v > m )) && m=$v
    done
    awk -v v="$m" 'BEGIN{printf "%.1f", v/1000}'
}
throttle_count() { cat /sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_count 2>/dev/null || echo 0; }
throttle_ms()    { cat /sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms 2>/dev/null || echo 0; }
yaml_num() { awk -v k="$2:" '$1==k {print $2; exit}' "$1"; }

# --- run ---------------------------------------------------------------------
NCPU=$(nproc)
STAMP=$(date +%Y%m%d-%H%M%S)
[[ -z "$LABEL" ]] && LABEL="pl1-${PL1_W}w"
[[ -z "$OUT" ]] && OUT="$PWD/perf-${LABEL}-${STAMP}.csv"
INTERVALS=$(( SECS / INTERVAL ))

printf '\n\033[1m== sustained performance: %s s at PL1 %s W, %s x %s s ==\033[0m\n' \
    "$SECS" "$PL1_READBACK" "$INTERVALS" "$INTERVAL"
printf '   load: stress-ng --cpu %s --cpu-method %s\n' "$NCPU" "$METHOD"
printf '   score: bogo-ops/s over each window, from a fresh %s s stressor run\n' "$INTERVAL"
printf '   PL1 averages over ~32 s, so the first %s intervals are turbo, not steady state\n\n' \
    "$(( 32 / INTERVAL + 1 ))"

printf 'interval,t_end_s,watts,bogo_ops,score_ops_s,ops_per_joule,peci_c,coretemp_c,battery_c,board_c,fan_rpm,avg_mhz,pl1_w,throttle_events,throttle_ms\n' > "$OUT"

printf '  idle: %s C peci, %s rpm\n\n' "$(read_c peci-temp)" "$(cat "$EC/fan1_input" 2>/dev/null || echo '?')"
printf '  \033[1m%3s  %8s  %10s  %9s  %7s %7s %7s  %6s  %6s\033[0m\n' \
    "int" "watts" "score" "ops/J" "peci" "batt" "board" "rpm" "MHz"

BEST=0; ABORTED=""
declare -a W_ARR=() S_ARR=()
TC0=$(throttle_count); TM0=$(throttle_ms)
RUN_START=$(date +%s%N)
LOAD_NS=0

for (( i = 1; i <= INTERVALS; i++ )); do
    # Re-assert PL1: firmware re-derives it asynchronously, and with the daemon stopped
    # nothing else is watching. A drift here is a finding, not noise.
    CUR_PL1=$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))
    if (( CUR_PL1 != PL1_READBACK )); then
        printf '  !! PL1 drifted to %s W — re-asserting %s W\n' "$CUR_PL1" "$PL1_READBACK"
        echo "$(( PL1_READBACK * 1000000 ))" > $ZONE/constraint_0_power_limit_uw
    fi

    E1=$(cat $ZONE/energy_uj); T1=$(date +%s%N)
    # Frequency must be read while the load is on, so take it at the midpoint.
    rm -f "$WORK/mhz"
    ( sleep $(( INTERVAL / 2 )); avg_mhz > "$WORK/mhz" ) &
    MHZ_PID=$!
    if [[ "$MONITOR" == yes ]]; then
        sleep "$INTERVAL"
    else
        stress-ng --cpu "$NCPU" --cpu-method "$METHOD" -t "$INTERVAL" \
            --metrics-brief --yaml "$WORK/m.yaml" >/dev/null 2>&1 &
        LOAD_PID=$!
        wait "$LOAD_PID" 2>/dev/null; LOAD_PID=""
    fi
    E2=$(cat $ZONE/energy_uj); T2=$(date +%s%N)
    wait "$MHZ_PID" 2>/dev/null

    DE=$(( E2 - E1 )); (( DE < 0 )) && DE=$(( DE + MAXE ))   # counter wrap
    DT=$(( T2 - T1 ))
    WATTS=$(awk -v de="$DE" -v dt="$DT" 'BEGIN{printf "%.2f", de*1000/dt}')

    if [[ "$MONITOR" == yes ]]; then
        OPS=""; SCORE=""
    else
        OPS=$(yaml_num "$WORK/m.yaml" "bogo-ops")
        SCORE=$(yaml_num "$WORK/m.yaml" "bogo-ops-per-second-real-time")
        [[ -z "$OPS" ]] && { echo "  !! stress-ng produced no metrics for interval $i" >&2; OPS=0; SCORE=0; }
    fi
    if [[ "$MONITOR" == yes ]]; then
        SCORE="-"; OPJ="-"
    else
        SCORE=$(awk -v s="$SCORE" 'BEGIN{printf "%.1f", s}')
        OPJ=$(awk -v o="$OPS" -v de="$DE" 'BEGIN{printf "%.1f", (de>0)? o/(de/1000000) : 0}')
    fi

    PECI=$(read_c peci-temp); BATT=$(read_c battery_temp@b)
    B1=$(read_c local_f75397@4c); B2=$(read_c ddr_f75303@4d)
    BOARD=$(awk -v a="${B1:-0}" -v b="${B2:-0}" 'BEGIN{print (a>b)? a : b}')
    CORE=$(coretemp_max)
    RPM=$(cat "$EC/fan1_input" 2>/dev/null || echo 0)
    MHZ=$(cat "$WORK/mhz" 2>/dev/null || echo 0)   # mid-window, under load
    TC=$(( $(throttle_count) - TC0 )); TM=$(( $(throttle_ms) - TM0 ))

    printf '  %3s  %7s W  %10s  %9s  %6s%s %6s%s %6s%s  %6s  %6s\n' \
        "$i" "$WATTS" "$SCORE" "$OPJ" "$PECI" "C" "$BATT" "C" "$BOARD" "C" "$RPM" "$MHZ"

    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$i" "$(( i * INTERVAL ))" "$WATTS" "$OPS" "$SCORE" "$OPJ" "$PECI" "$CORE" "$BATT" \
        "$BOARD" "$RPM" "$MHZ" "$CUR_PL1" "$TC" "$TM" >> "$OUT"

    W_ARR+=("$WATTS")
    [[ "$MONITOR" == no ]] && S_ARR+=("$SCORE")
    LOAD_NS=$(( LOAD_NS + DT ))
    [[ "$MONITOR" == no ]] && BEST=$(awk -v b="$BEST" -v s="$SCORE" 'BEGIN{print (s>b)? s : b}')

    # Thermal guards. These protect what cannot protect itself; the CPU is left to Tjmax.
    if [[ -n "$BATT" ]] && awk -v v="$BATT" -v l="$BATT_ABORT_C" 'BEGIN{exit !(v>=l)}'; then
        ABORTED="battery reached ${BATT} C (crit 49.9 C)"; break
    fi
    if awk -v v="$BOARD" -v l="$BOARD_ABORT_C" 'BEGIN{exit !(v>=l)}'; then
        ABORTED="board sensor reached ${BOARD} C (crit ~87 C)"; break
    fi
done

# --- verdict ------------------------------------------------------------------
N=${#S_ARR[@]}
SETTLE=$(( 32 / INTERVAL + 1 ))           # intervals inside PL1's averaging window
TAIL=$(( N > SETTLE ? N - SETTLE : 0 ))   # how many settled intervals we have

RUN_NS=$(( $(date +%s%N) - RUN_START ))

printf '\n\033[1m== verdict ==\033[0m\n'
[[ -n "$ABORTED" ]] && printf '  \033[1mABORTED: %s\033[0m\n' "$ABORTED"

if [[ "$MONITOR" == yes ]]; then
    # No synthetic score to report — whatever was being benchmarked owns that number.
    # What this run contributes is what the machine spent producing it.
    awk -F, 'NR>1 {
        n++; w+=$3; mhz+=$12
        if ($3>pw) pw=$3
        if ($7>pk) pk=$7
        if ($8>ct) ct=$8
        if ($9>bt) bt=$9
        if ($10>bd) bd=$10
        if ($11>rp) rp=$11
        th=$14
    } END {
        if (n==0) { print "  no samples"; exit }
        printf "  mean draw         %.2f W   (peak %.2f W over %d samples)\n", w/n, pw, n
        printf "  mean frequency    %.0f MHz\n", mhz/n
        printf "  peak temps        peci %.1f C   core %.1f C   battery %.1f C   board %.1f C\n", pk, ct, bt, bd
        printf "  peak fan          %d rpm\n", rp
        printf "  throttle events   %s%s\n", th, (th+0>0) ? "   <- the CPU hit Tjmax; more power will not help" : ""
    }' "$OUT"
elif (( TAIL > 0 )); then
    SUM_W=0; SUM_S=0
    for (( j = SETTLE; j < N; j++ )); do
        SUM_W=$(awk -v a="$SUM_W" -v b="${W_ARR[$j]}" 'BEGIN{print a+b}')
        SUM_S=$(awk -v a="$SUM_S" -v b="${S_ARR[$j]}" 'BEGIN{print a+b}')
    done
    MEAN_W=$(awk -v s="$SUM_W" -v n="$TAIL" 'BEGIN{printf "%.2f", s/n}')
    MEAN_S=$(awk -v s="$SUM_S" -v n="$TAIL" 'BEGIN{printf "%.1f", s/n}')
    PEAK_S=${S_ARR[0]}; LAST_S=${S_ARR[$((N-1))]}

    printf '  setpoint          %s W\n' "$PL1_READBACK"
    printf '  sustained draw    %s W  (mean of intervals %s-%s, past the ~32 s window)\n' \
        "$MEAN_W" "$(( SETTLE + 1 ))" "$N"
    printf '  sustained score   %s bogo-ops/s\n' "$MEAN_S"
    printf '  first interval    %s bogo-ops/s   (turbo, before PL1 engages)\n' "$PEAK_S"
    printf '  droop             '
    awk -v p="$PEAK_S" -v l="$LAST_S" -v n="$N" 'BEGIN{
        printf "%.1f%% from interval 1 to %d\n", (p>0)? (p-l)*100/p : 0, n }'
    awk -v sp="$PL1_READBACK" -v mw="$MEAN_W" 'BEGIN{
        d = (mw - sp) * 100 / sp
        if (mw < sp * 0.9)
            printf "  NOTE: draw settled %.1f%% BELOW setpoint — something other than PL1 is\n        binding (thermal, or the EC). Raising PL1 further will buy nothing.\n", -d
        else if (mw > sp * 1.1)
            printf "  NOTE: draw ran %.1f%% ABOVE setpoint — PL1 is not the binding constraint here.\n", d
        else
            printf "  PL1 is the binding constraint: draw tracked setpoint to within %.1f%%.\n", (d<0)?-d:d
    }'
else
    printf '  too few intervals to separate turbo from steady state\n'
fi
if [[ "$MONITOR" == no ]]; then
    awk -v l="$LOAD_NS" -v r="$RUN_NS" 'BEGIN{
        printf "  duty cycle        %.1f%% of %.0f s wall clock was under load\n", l*100/r, r/1e9
        if (l*100/r < 95) {
            print "        the gaps between stressor runs are large enough to let the die cool;"
            print "        treat the droop figure as a lower bound"
        }
    }'

    printf '\n  Compare two setpoints by running twice and diffing the sustained score:\n'
    printf '    sudo %s --pl1 30 --label 30w\n' "$0"
    printf '    sudo %s --pl1 35 --label 35w\n' "$0"
    printf '  Let the machine cool between runs, or the second is heat-soaked and reads low.\n'
else
    printf '\n  Pair this with the benchmark'"'"'s own score. Run it once per profile —\n'
    printf '    fw-helperctl profile performance   (25 W)\n'
    printf '    fw-helperctl profile turbo         (30 W)\n'
    printf '    fw-helperctl profile max           (35 W)\n'
    printf '  — and the interesting number is frames per watt, not frames.\n'
fi
