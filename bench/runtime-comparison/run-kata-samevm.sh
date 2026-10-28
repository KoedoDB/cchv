#!/bin/bash
# Kata + Cloud Hypervisor in a VM of the same size as cchv's (PostgreSQL 3 vCPUs, nginx 2), so that the container has one CPU less (2 and 1): Kata adds a vCPU to the container's limit, run-all.sh gives it
# the same usable CPUs and so a VM one vCPU larger; this is the other way round. Usage: run-kata-samevm.sh RESULTS_DIR   (as a transient unit with RT_KATA_SAME_VM=1 set; checked below)
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
[ "$RT_KATA_SAME_VM" = 1 ] || { echo "RT_KATA_SAME_VM=1 is not set"; exit 1; }
export RT_RESULTS="${1:?results dir}"; mkdir -p "$RT_RESULTS"
exec > >(tee -a "$RT_RESULTS/run.log") 2>&1
"$here/40-governor.sh"; "$here/50-environment.sh" "$RT_RESULTS"
( cd "$here" && python3 -c "import rtbench_blk as b; b.set_kata_config(False)" )   # Kata with virtio-fs, as shipped
step() { echo "=== $(date +%T) $*"; "$@" || echo "!!! failed: $*"; }
rt=kata-clh
reset() { "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1; }
reset
step python3 "$here/rtbench.py" stopcheck $rt
RT_PHASE=pg step python3 "$here/rtbench.py" start $rt
step python3 "$here/rtbench.py" pg $rt
reset
RT_PHASE=nginx step python3 "$here/rtbench.py" start $rt
step python3 "$here/rtbench.py" nginx $rt
"$here/50-environment.sh" "$RT_RESULTS" >/dev/null
echo "=== $(date +%T) done"
