#!/bin/bash
# Fixes the CPU frequency governor for the runs (performance) and records what it was; `40-governor.sh restore` puts it back.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
F="$RT_DATA/governor.before"
if [ "${1:-}" = restore ]; then
  [ -f "$F" ] && { g="$(cat "$F")"; for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo "$g" | sudo tee "$c" >/dev/null; done; echo "governor restored to $g"; }
  exit 0
fi
[ -f "$F" ] || cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor | sudo tee "$F" >/dev/null
for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance | sudo tee "$c" >/dev/null; done
echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) (was $(cat "$F"))"
