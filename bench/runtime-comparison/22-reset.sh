#!/bin/bash
# A clean state between runs: stops the benchmark's Docker, containerd and the cchv daemon, kills what they left (shims, VMMs of VMs left to their pods), empties the erofs cache and starts again.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
"$here/21-stop.sh" >/dev/null 2>&1
sudo pkill -f 'containerd-shim.* -namespace rtbench' 2>/dev/null; sudo pkill -x cloud-hyperviso 2>/dev/null; sudo pkill -x virtiofsd 2>/dev/null; sleep 2
sudo umount /run/cloudhv/erofs-cache 2>/dev/null; sudo find /run/cloudhv/erofs-cache -mindepth 1 -delete 2>/dev/null   # what an earlier run left in the tmpfs under the bind mount
sudo find /run/cloudhv/state "$RT_DATA/erofs-cache" -mindepth 1 -delete 2>/dev/null; sudo rm -rf "$RT_RUN/containerd" 2>/dev/null
"$here/20-start.sh" | tail -2
