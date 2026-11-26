#!/usr/bin/env sh
# Machine description for benchmark results (sourced by bench-tails.sh and
# run-benchmarks.sh). POSIX sh; Linux (the target) and macOS (Intel or Apple Silicon).

os="$(uname -s)"
arch="$(uname -m)"

_sys() { sysctl -n "$1" 2>/dev/null || echo "?"; }
_first_line() { if [ -r "$1" ]; then head -1 "$1"; else echo "?"; fi; }

# Largest CPU cache, bytes, as the OS reports it.
largest_cache() {
    largest=0
    if [ "$os" = Linux ]; then
        for f in /sys/devices/system/cpu/cpu0/cache/index*/size; do
            [ -r "$f" ] || continue
            v="$(cat "$f")"                       # e.g. "32K", "1024K", "16M"
            case "$v" in
                *K) b=$(( ${v%K} * 1024 )) ;;
                *M) b=$(( ${v%M} * 1048576 )) ;;
                *)  b="$v" ;;
            esac
            [ "$b" -gt "$largest" ] && largest="$b"
        done
    else
        # Intel Macs report hw.l3cachesize; Apple Silicon reports per-cluster L2s
        # (perflevel0 = performance cores).
        for key in hw.l3cachesize hw.l2cachesize hw.perflevel0.l2cachesize; do
            v="$(sysctl -n "$key" 2>/dev/null || echo 0)"
            case "$v" in
                ''|\?*) v=0 ;;
            esac
            [ "$v" -gt "$largest" ] 2>/dev/null && largest="$v"
        done
    fi
    echo "$largest"
}

# A short machine identifier for file and folder names.
machine_name() {
    if [ "$os" = Linux ]; then
        m="$(_first_line /sys/class/dmi/id/product_name)"
    else
        m="$(_sys machdep.cpu.brand_string)"      # "Apple M2 Pro" / "Intel(R) Core(TM) i7-8569U ..."
        [ "$m" = "?" ] && m="$(_sys hw.model)"
    fi
    printf '%s' "$m" | sed 's/(R)//g; s/(TM)//g; s/ CPU.*//; s/Intel Core //' \
        | tr -c 'A-Za-z0-9.-' '-' | tr -s '-' | sed 's/^-//; s/-$//'
}

# The machine, one line per fact.
describe() {
    if [ "$os" = Linux ]; then
        echo "model:      $(_first_line /sys/class/dmi/id/product_name)"
        echo "cpu:        $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ //')"
        echo "cores:      $(grep -c ^processor /proc/cpuinfo) logical; isolated: $(cat /sys/devices/system/cpu/isolated 2>/dev/null || echo ?)"
        echo "memory:     $(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1048576 )) GiB"
        echo "caches:     $(for f in /sys/devices/system/cpu/cpu0/cache/index*; do printf 'L%s %s %s; ' "$(cat "$f/level")" "$(cat "$f/type")" "$(cat "$f/size")"; done)"
        echo "os:         $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") (kernel $(uname -r))"
        echo "cmdline:    $(cat /proc/cmdline)"
        echo "governor:   $(_first_line /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
        echo "pinned to:  ${BENCH_CPU:-(not pinned)}"
    else
        echo "model:      $(_sys hw.model) ($arch)"
        echo "cpu:        $(_sys machdep.cpu.brand_string)"
        if [ "$(_sys hw.nperflevels)" != "?" ] && [ "$(_sys hw.nperflevels)" -gt 1 ] 2>/dev/null; then
            echo "cores:      $(_sys hw.perflevel0.physicalcpu) performance + $(_sys hw.perflevel1.physicalcpu) efficiency"
            echo "caches:     P-cluster L2 $(( $(_sys hw.perflevel0.l2cachesize) / 1048576 )) MiB, E-cluster L2 $(( $(_sys hw.perflevel1.l2cachesize) / 1048576 )) MiB, L1d $(( $(_sys hw.perflevel0.l1dcachesize) / 1024 )) KiB (per OS report)"
        else
            echo "cores:      $(_sys hw.physicalcpu) physical, $(_sys hw.ncpu) logical"
            echo "caches:     L2 $(( $(sysctl -n hw.l2cachesize 2>/dev/null || echo 0) / 1024 )) KiB, L3 $(( $(sysctl -n hw.l3cachesize 2>/dev/null || echo 0) / 1024 )) KiB (per OS report)"
        fi
        echo "memory:     $(( $(_sys hw.memsize) / 1073741824 )) GiB"
        echo "os:         $(sw_vers -productName 2>/dev/null) $(sw_vers -productVersion 2>/dev/null)"
        echo "power:      $(pmset -g batt 2>/dev/null | head -1 | sed "s/.*'\(.*\)'.*/\1/" || echo ?)"
    fi
    echo "rustc:      $(rustc -V)"
    echo "commit:     $(git rev-parse --short HEAD 2>/dev/null || echo '?')$(git diff --quiet 2>/dev/null || echo ' (uncommitted changes)')"
    echo "date:       $(date)"
}

loadavg() {
    if [ "$os" = Linux ]; then cut -d' ' -f1-3 /proc/loadavg; else _sys vm.loadavg; fi
}
