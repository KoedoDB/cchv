#!/bin/bash
# The Kata part of the comparison, re-run with the Cloud Hypervisor configuration (the first run used Kata's default configuration, which is QEMU: its results are kept as "kata-qemu"),
# and the variant with the data on a block device (virtio-blk, direct I/O) for PostgreSQL and nginx. Usage: run-kata.sh RESULTS_DIR
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
export RT_RESULTS="${1:?results dir}"; exec > >(tee -a "$RT_RESULTS/run-kata.log") 2>&1
step() { echo "=== $(date +%T) $*"; "$@" || echo "!!! failed: $*"; }
reset() { "$here/22-reset.sh" >/dev/null 2>&1; python3 "$here/rtbench.py" prepare >/dev/null 2>&1; }
cd "$here"
# 1. Kata + Cloud Hypervisor as shipped (virtio-fs)
reset; python3 -c "import rtbench_blk as b; b.set_kata_config(False)"
step python3 rtbench.py stopcheck kata-clh
step python3 rtbench.py start kata-clh
step python3 rtbench.py pg kata-clh
step python3 rtbench.py nginx kata-clh
for n in 1 8 16; do step python3 rtbench.py density kata-clh $n; done
# 2. the block device variant, direct I/O
reset
step python3 rtbench_blk.py pg --direct
step python3 rtbench_blk.py nginx --direct
# 3. the block device variant with Kata's default cache mode (PostgreSQL only)
reset
step python3 rtbench_blk.py pg
python3 -c "import rtbench_blk as b; b.set_kata_config(False)"
echo "=== $(date +%T) done"
