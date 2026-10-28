#!/bin/bash
# A clean state between runs: stops the benchmark's Docker, containerd and the cchv daemon, kills what they left (shims, VMMs of VMs left to their pods), empties the erofs cache and starts again.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
# Only the benchmark's own VMMs are killed (by the path of the binary, as rtbench.py does): a host that runs cchv for real (stg-db-001) has VMs of its own under /usr/local/bin.
"$here/21-stop.sh" >/dev/null 2>&1
sudo pkill -f 'containerd-shim.* -namespace rtbench' 2>/dev/null; sudo pkill -f "^($RT_ROOT|/opt/kata)/bin/cloud-hypervisor " 2>/dev/null; sudo pkill -f "^($RT_ROOT|/opt/kata).*virtiofsd" 2>/dev/null; sleep 2
[ -n "$RT_BINDS" ] || sudo umount "$RT_CRUN/erofs-cache" 2>/dev/null; sudo find "$RT_CRUN/erofs-cache" -mindepth 1 -delete 2>/dev/null   # what an earlier run left in the tmpfs under the bind mount
sudo find "$RT_CRUN/state" "$RT_DATA/erofs-cache" -mindepth 1 -delete 2>/dev/null; sudo rm -rf "$RT_RUN/containerd" 2>/dev/null
"$here/20-start.sh" | tail -2
