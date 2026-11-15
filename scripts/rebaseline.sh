#!/usr/bin/env bash
# Re-baseline the GPU/CPU power behaviour after a firmware or kernel change.
#
# Recreates, in one committed script, the measurements the CLAUDE.md traps rest on -
# "GPU clock is traded against CPU clock", the bit-8 electrical limit, the ~1950 MHz
# GPU stall - which were all taken on BIOS 03.02 / kernel 7.0 by scratchpad scripts
# that were never committed and are now lost. The Windows sibling found BIOS 03.04
# stops PL4 binding the GPU, so every one of those findings is suspect until this
# has been run on 03.04.
#
# Phases (each: SETTLE s to settle, then SAMPLE s measured):
#   cpu-only        stress-ng --cpu N alone                   (03.02: cores ~3.8-3.9 GHz)
#   gpu-only        fullscreen vkmark alone                   (03.02: act 1850-1950, pl4)
#   both            stress-ng + vkmark, PPD balanced          (03.02: GPU 1950, Bzy ~1975)
#   both-cap2000/1600/1200   scaling_max_freq caps on all cores (03.02: GPU 2050/2150/2200)
#   both-powersaver PPD power-saver (EPP power), no cap       (03.02: GPU 2200, CPU -43%)
#
# The daemon is STOPPED for the run (it re-asserts PL1 and follows PPD, either of which
# would move the thing being measured) and PL1 is pinned at 35 W. The fan goes back to
# the EC via the unit's ExecStopPost. On exit - normal, error or Ctrl-C - loads are
# killed, frequency caps and the PPD profile are restored, and the daemon restarted.
#
#   sudo ./scripts/rebaseline.sh 2>&1 | tee ~/rebaseline-$(date +%Y%m%d-%H%M).log
#
# Needs: mains power, stress-ng, msr-tools, turbostat, vkmark, a logged-in Wayland session.
set -euo pipefail

THREADS=${THREADS:-4}
SETTLE=${SETTLE:-20}
SAMPLE=${SAMPLE:-20}
SCENE=${SCENE:-effect2d:kernel=edge}   # heaviest vkmark scene here (CLAUDE.md traps)
PL1_W=${PL1_W:-35}

GT=/sys/class/drm/card1/device/tile0/gt0/freq0
RAPL=/sys/class/powercap/intel-rapl-mmio:0
die() { echo "ABORT: $*" >&2; exit 1; }

# ---- prechecks: each of these once cost a run -------------------------------------
[[ $EUID -eq 0 ]] || die "run with sudo"
[[ -n ${SUDO_USER:-} ]] || die "run via sudo from your desktop session (need SUDO_USER)"
for t in stress-ng rdmsr turbostat vkmark powerprofilesctl python3; do
    command -v "$t" >/dev/null || die "$t missing (sudo apt install -y stress-ng msr-tools)"
done
[[ -r $GT/act_freq ]] || die "$GT not found - is the GPU still card1/xe?"
ac=0; for s in /sys/class/power_supply/*; do
    [[ $(cat "$s/type" 2>/dev/null) == Mains && $(cat "$s/online") == 1 ]] && ac=1
done
[[ $ac == 1 ]] || die "not on mains - on battery pl2 binds the GPU (CLAUDE.md traps)"
for p in vkmark stress-ng furmark FurMark vkcube; do
    pgrep -x "$p" >/dev/null && die "$p already running - a stray load invalidated a baseline once"
done
modprobe msr

uid=$(id -u "$SUDO_USER")
RUNTIME=/run/user/$uid
WAYLAND=$(basename "$(ls "$RUNTIME"/wayland-[0-9] 2>/dev/null | head -1)")
[[ -n $WAYLAND ]] || die "no Wayland socket in $RUNTIME"

# ---- state to restore -------------------------------------------------------------
ORIG_PPD=$(powerprofilesctl get)
declare -A ORIG_MAX
for f in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_max_freq; do ORIG_MAX[$f]=$(cat "$f"); done
DAEMON_WAS_ACTIVE=$(systemctl is-active fw-helperd || true)
VK_PID=""; ST_PID=""

cleanup() {
    set +e
    [[ -n $VK_PID ]] && kill "$VK_PID" 2>/dev/null
    [[ -n $ST_PID ]] && kill "$ST_PID" 2>/dev/null
    pkill -x vkmark 2>/dev/null; pkill -x stress-ng 2>/dev/null
    for f in "${!ORIG_MAX[@]}"; do echo "${ORIG_MAX[$f]}" > "$f"; done
    powerprofilesctl set "$ORIG_PPD"
    [[ $DAEMON_WAS_ACTIVE == active ]] && systemctl start fw-helperd
    echo "restored: caps, PPD=$ORIG_PPD, daemon=$(systemctl is-active fw-helperd)"
}
trap cleanup EXIT

# ---- setup ------------------------------------------------------------------------
echo "== rebaseline $(date -Is)"
echo "bios=$(cat /sys/class/dmi/id/bios_version) kernel=$(uname -r) mesa=$(dpkg-query -W -f='${Version}' libgl1-mesa-dri 2>/dev/null)"
echo "threads=$THREADS settle=${SETTLE}s sample=${SAMPLE}s scene=$SCENE pl1=${PL1_W}W"
systemctl stop fw-helperd || true
echo $((PL1_W * 1000000)) > $RAPL/constraint_0_power_limit_uw
echo "PL1 now $(( $(cat $RAPL/constraint_0_power_limit_uw) / 1000000 )) W (daemon stopped)"

set_cap() {   # kHz, or "none"
    for f in "${!ORIG_MAX[@]}"; do
        if [[ $1 == none ]]; then echo "${ORIG_MAX[$f]}" > "$f"; else echo "$1" > "$f"; fi
    done
}

start_gpu() {
    runuser -u "$SUDO_USER" -- env XDG_RUNTIME_DIR="$RUNTIME" WAYLAND_DISPLAY="$WAYLAND" \
        vkmark --fullscreen -p mailbox -b "$SCENE" --run-forever >/tmp/rebaseline-vkmark.$$ 2>&1 &
    VK_PID=$!
    sleep 3
    kill -0 "$VK_PID" 2>/dev/null || { cat /tmp/rebaseline-vkmark.$$; die "vkmark exited"; }
}
stop_gpu() {
    [[ -n $VK_PID ]] && kill "$VK_PID" 2>/dev/null
    pkill -x vkmark 2>/dev/null || true
    wait "$VK_PID" 2>/dev/null || true; VK_PID=""; sleep 2
}

# Switching PPD moves platform_profile, and firmware then re-derives PL1 asynchronously
# (CLAUDE.md traps), so pin it at every phase start and report what held at the end.
pin_pl1() { echo $((PL1_W * 1000000)) > $RAPL/constraint_0_power_limit_uw; }

# One phase. $1 name, $2 cpu load y/n, $3 gpu load y/n.
phase() {
    local name=$1 cpu=$2 gpu=$3 dir
    dir=$(mktemp -d)
    pin_pl1
    [[ $gpu == y ]] && start_gpu
    if [[ $cpu == y ]]; then
        stress-ng --cpu "$THREADS" --metrics-brief -t $((SETTLE + SAMPLE)) >"$dir/stress" 2>&1 &
        ST_PID=$!
    fi
    sleep "$SETTLE"

    turbostat --quiet --Summary --show PkgWatt,CorWatt,GFXWatt,Bzy_MHz,Busy% \
        --interval 1 --num_iterations "$SAMPLE" >"$dir/turbo" 2>/dev/null &
    local tpid=$! end=$((SECONDS + SAMPLE))
    while (( SECONDS < end )); do
        # 10 Hz act_freq; 1 Hz reasons + limit-reason MSRs (core 0x64f, graphics 0x6b0)
        for _ in 1 2 3 4 5 6 7 8 9 10; do cat $GT/act_freq >>"$dir/act"; sleep 0.1; done
        echo "$(cat $GT/throttle/reasons) $(rdmsr -p 0 -c 0x64f) $(rdmsr -p 0 -c 0x6b0)" >>"$dir/reasons"
        cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | sort -n | tail -1 >>"$dir/temp"
    done
    wait "$tpid" 2>/dev/null || true
    echo $(( $(cat $RAPL/constraint_0_power_limit_uw) / 1000000 )) >"$dir/pl1"
    [[ -n $ST_PID ]] && { wait "$ST_PID" 2>/dev/null || true; ST_PID=""; }
    [[ $gpu == y ]] && stop_gpu

    python3 -I - "$name" "$dir" <<'PY'
import sys, statistics as st, re, os
name, d = sys.argv[1], sys.argv[2]
def lines(f):
    p = os.path.join(d, f)
    return open(p).read().split() if os.path.exists(p) else []
act = [int(x) for x in lines("act")]
awake = [a for a in act if a > 0]
gpu = (f"GPU act p50 {st.median(awake):.0f} p90 {sorted(awake)[int(.9*len(awake))-1]:.0f} "
       f"awake {100*len(awake)/len(act):.0f}%") if awake else "GPU idle"
rows, hdr = [], None
for ln in open(os.path.join(d, "turbo")).read().splitlines():
    f = ln.split()
    if "Bzy_MHz" in f: hdr = f; continue
    if hdr and len(f) == len(hdr):
        try: rows.append(dict(zip(hdr, map(float, f))))
        except ValueError: pass
def avg(k): return st.mean(r[k] for r in rows) if rows and k in rows[0] else float("nan")
reasons = open(os.path.join(d, "reasons")).read().splitlines()
xe = {}
cb8 = gb8 = 0
for r in reasons:
    *xr, core, gfx = r.split()
    xr = "+".join(xr) or "?"
    xe[xr] = xe.get(xr, 0) + 1
    cb8 += bool(int(core, 16) & 0x100); gb8 += bool(int(gfx, 16) & 0x100)
n = len(reasons)
ops = "-"
p = os.path.join(d, "stress")
if os.path.exists(p):
    for ln in open(p):
        if re.search(r"\bcpu\b", ln) and "bogo" not in ln:
            nums = re.findall(r"[\d.]+", ln.split("cpu", 1)[1])
            if len(nums) >= 5: ops = f"{float(nums[4]):.0f}"   # bogo ops/s (real time)
temps = [int(x)/1000 for x in lines("temp")]
print(f"{name:<16} {gpu:<34} pkg {avg('PkgWatt'):5.1f}W cor {avg('CorWatt'):5.1f}W "
      f"gfx {avg('GFXWatt'):5.1f}W Bzy {avg('Bzy_MHz'):5.0f}MHz busy {avg('Busy%'):4.1f}% "
      f"ops/s {ops:>6} | xe {' '.join(f'{k}:{v}' for k,v in sorted(xe.items()))} "
      f"| bit8 core {cb8}/{n} gfx {gb8}/{n} | PL1 {open(os.path.join(d,'pl1')).read().strip()}W | maxT {max(temps) if temps else float('nan'):.0f}C")
PY
    rm -rf "$dir"
}

powerprofilesctl set balanced
phase cpu-only        y n
phase gpu-only        n y
phase both            y y
for cap in 2000000 1600000 1200000; do
    set_cap $cap; phase "both-cap$((cap / 1000))" y y
done
set_cap none
powerprofilesctl set power-saver
sleep 5   # let firmware's PL1 re-derivation land before phase() re-pins it
phase both-powersaver y y
echo "== done $(date -Is)"
