#!/bin/sh
# Run a command under a memory guard.
#
# Terminates the child's whole process group when MemAvailable drops below a
# safety floor (default: 10% of MemTotal). The floor is a percentage, so it
# scales with the host instead of hardcoding gigabytes.
#
# Env overrides:
#   EE_TEST_MEM_FLOOR_PCT  floor as percent of MemTotal (default 10)
#   EE_TEST_MEM_POLL_SECS  sampling interval in seconds (default 2)
#
# Usage: scripts/run-tests-memguard.sh <command> [args...]
set -u

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <command> [args...]" >&2
    exit 64
fi

floor_pct=${EE_TEST_MEM_FLOOR_PCT:-10}
poll_secs=${EE_TEST_MEM_POLL_SECS:-2}
total_kb=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
min_kb=$((total_kb * floor_pct / 100))

# Own session/process group so cargo's spawned test binaries die with the run.
if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
else
    "$@" &
fi
pid=$!

kill_tree() {
    signal=$1
    # Negative pid targets the process group when setsid created one.
    kill "-$signal" -- "-$pid" 2>/dev/null || kill "-$signal" "$pid" 2>/dev/null || true
}

while kill -0 "$pid" 2>/dev/null; do
    avail_kb=$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)
    if [ "$avail_kb" -lt "$min_kb" ]; then
        echo "memguard: MemAvailable=${avail_kb}kB below ${min_kb}kB (${floor_pct}% of ${total_kb}kB); killing pid $pid" >&2
        kill_tree TERM
        sleep 2
        kill_tree KILL
        wait "$pid" 2>/dev/null
        exit 137
    fi
    sleep "$poll_secs"
done

wait "$pid"
