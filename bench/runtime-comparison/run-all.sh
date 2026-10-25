#!/bin/bash
# The whole comparison, one runtime after the other, from a clean state each time. About 5 hours. Usage: run-all.sh [RESULTS_DIR]   (run it as a transient unit: systemd-run --unit=rtbench-run ...)
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
export RT_RESULTS="${1:-$HOME/rtbench-results/$(date +%Y%m%d)}"; mkdir -p "$RT_RESULTS"
exec > >(tee -a "$RT_RESULTS/run.log") 2>&1
"$here/40-governor.sh"; "$here/50-environment.sh" "$RT_RESULTS"
( cd "$here" && python3 -c "import rtbench_blk as b; b.set_kata_config(False)" )   # Kata with virtio-fs, as shipped
step() { echo "=== $(date +%T) $*"; "$@" || echo "!!! failed: $*"; }
# The VM is the same size for Kata and cchv in every test (rtbench.py: VM_SIZE): PostgreSQL 3 vCPUs and 2080 MiB, nginx 2 vCPUs and 320 MiB. cchv's pool is sized for one of the two at a time
# (RT_CCHV_VCPUS: 3 or 2), so the PostgreSQL part and the nginx part each start from a reset. rtbench.py stops when the VM it started is not the size it says.
for rt in runc runsc kata-clh cloudhv; do
  RT_CCHV_VCPUS=3 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  step python3 "$here/rtbench.py" stopcheck $rt
  RT_PHASE=pg step python3 "$here/rtbench.py" start $rt
  step python3 "$here/rtbench.py" pg $rt
  RT_CCHV_VCPUS=2 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  RT_PHASE=nginx step python3 "$here/rtbench.py" start $rt
  step python3 "$here/rtbench.py" nginx $rt
done
RT_CCHV_VCPUS=3 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
step python3 "$here/rtbench.py" pg runc-loop          # the control: runc on the same loop-device volume cchv gets
for rt in runc runsc kata-clh cloudhv; do
  RT_CCHV_VCPUS=3 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  for n in 1 8 16; do step python3 "$here/rtbench.py" density $rt $n; done
  RT_CCHV_VCPUS=2 "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1
  for n in 1 8 16; do step python3 "$here/rtbench.py" ndensity $rt $n; done
done
"$here/50-environment.sh" "$RT_RESULTS" >/dev/null
echo "=== $(date +%T) done"
