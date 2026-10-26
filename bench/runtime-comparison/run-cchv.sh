#!/bin/bash
# Only the cchv part of run-all.sh, for a re-measurement of cchv alone (a new cchv release, the other runtimes' results kept from an earlier run-all.sh).
# The same steps and the same VM sizes as run-all.sh (PostgreSQL 3 vCPUs and 2080 MiB, nginx 2 vCPUs and 320 MiB). Usage: run-cchv.sh RESULTS_DIR   (as a transient unit: systemd-run --unit=rtbench-run ...)
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
export RT_RESULTS="${1:?results dir}"; mkdir -p "$RT_RESULTS"
exec > >(tee -a "$RT_RESULTS/run.log") 2>&1
"$here/40-governor.sh"; "$here/50-environment.sh" "$RT_RESULTS"
step() { echo "=== $(date +%T) $*"; "$@" || echo "!!! failed: $*"; }
rt=cloudhv
RT_CCHV_VCPUS=3 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
step python3 "$here/rtbench.py" stopcheck $rt
RT_PHASE=pg step python3 "$here/rtbench.py" start $rt
step python3 "$here/rtbench.py" pg $rt
RT_CCHV_VCPUS=2 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
RT_PHASE=nginx step python3 "$here/rtbench.py" start $rt
step python3 "$here/rtbench.py" nginx $rt
RT_CCHV_VCPUS=3 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
for n in $RT_DENSITY_NS; do step python3 "$here/rtbench.py" density $rt $n; done
RT_CCHV_VCPUS=2 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
for n in $RT_DENSITY_NS; do step python3 "$here/rtbench.py" ndensity $rt $n; done
"$here/50-environment.sh" "$RT_RESULTS" >/dev/null
echo "=== $(date +%T) done"
