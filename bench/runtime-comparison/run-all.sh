#!/bin/bash
# The whole comparison, one runtime after the other, from a clean state each time. About 5 hours. Usage: run-all.sh [RESULTS_DIR]   (run it as a transient unit: systemd-run --unit=rtbench-run ...)
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
export RT_RESULTS="${1:-$HOME/rtbench-results/$(date +%Y%m%d)}"; mkdir -p "$RT_RESULTS"
exec > >(tee -a "$RT_RESULTS/run.log") 2>&1
"$here/40-governor.sh"; "$here/50-environment.sh" "$RT_RESULTS"
step() { echo "=== $(date +%T) $*"; "$@" || echo "!!! failed: $*"; }
for rt in runc runsc kata-clh cloudhv; do
  "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  step python3 "$here/rtbench.py" stopcheck $rt
  step python3 "$here/rtbench.py" start $rt
  step python3 "$here/rtbench.py" pg $rt
  step python3 "$here/rtbench.py" nginx $rt
done
"$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
step python3 "$here/rtbench.py" pg runc-loop          # the control: runc on the same loop-device volume cchv gets
for rt in runc runsc kata-clh cloudhv; do
  "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  for n in 1 8 16; do step python3 "$here/rtbench.py" density $rt $n; done
done
"$here/50-environment.sh" "$RT_RESULTS" >/dev/null
echo "=== $(date +%T) done"
