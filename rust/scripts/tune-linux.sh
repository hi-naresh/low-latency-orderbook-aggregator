#!/usr/bin/env bash
# Low-latency host tuning for running `ob` on Linux (step 21). See docs/TUNING.md for
# what each setting does and why.
#
# NOT run by this project's tests or CI, and never run on the development Mac. It
# changes system-wide settings and must be run as root on a machine dedicated to the
# feed. It prints every change; with --dry-run it changes nothing.
#
# Usage: sudo scripts/tune-linux.sh [--dry-run] FEED_CPUS   e.g.  sudo scripts/tune-linux.sh 2,3
#
# Boot-time settings (CPU isolation) can't be applied at run time; this script prints the
# kernel command line to add instead.
set -euo pipefail

dry=0
if [[ "${1:-}" == "--dry-run" ]]; then dry=1; shift; fi
cpus="${1:?usage: tune-linux.sh [--dry-run] FEED_CPUS (e.g. 2,3)}"
[[ "$(uname -s)" == Linux ]] || { echo "Linux only" >&2; exit 1; }
[[ $dry == 1 || $EUID == 0 ]] || { echo "run as root (or with --dry-run)" >&2; exit 1; }

run() { echo "+ $*"; [[ $dry == 1 ]] || eval "$@"; }

echo "== 1. Boot-time (add to GRUB_CMDLINE_LINUX, then update-grub and reboot):"
echo "   isolcpus=managed_irq,domain,$cpus nohz_full=$cpus rcu_nocbs=$cpus"
echo "   (no other tasks, timer ticks or RCU callbacks on the feed CPUs)"
echo "   current: $(cat /proc/cmdline)"

echo "== 2. CPU frequency: fixed at maximum (no ramp-up delay after idle)"
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    [[ -e $g ]] && run "echo performance > $g"
done

echo "== 3. Deep C-states off (wake-up from deep sleep costs tens of µs)"
# Holding /dev/cpu_dma_latency open with 0 forbids deep C-states while held; a daemon
# (e.g. tuned's latency-performance profile) normally does this.
echo "   use: tuned-adm profile latency-performance   (or keep /dev/cpu_dma_latency at 0)"

echo "== 4. Network-card interrupts away from the feed CPUs"
mask_all=$(printf '%x' $(( (1 << $(nproc)) - 1 )))
run "echo $mask_all > /proc/irq/default_smp_affinity"
echo "   (then steer the NIC's queue IRQs to a housekeeping CPU near the NIC;"
echo "    see /proc/interrupts and the NIC's driver docs)"

echo "== 5. Busy polling allowed for unprivileged sockets (ob live --busy-poll 50)"
run "sysctl -w net.core.busy_read=50 net.core.busy_poll=50"

echo "== 6. Transparent huge pages: madvise only (no surprise compaction stalls)"
[[ -e /sys/kernel/mm/transparent_hugepage/enabled ]] && \
    run "echo madvise > /sys/kernel/mm/transparent_hugepage/enabled"

echo "== 7. Allow memory locking and real-time priority for the ob user"
echo "   /etc/security/limits.d/ob.conf:   ob - memlock unlimited   and   ob - rtprio 99"

echo "== then run:  ob live --pin ${cpus} --rt --lock-memory --busy-poll 50 --quickack --kernel-timestamps --latency"
