#!/bin/bash
# A clean state between runs: stops the benchmark's Docker, containerd and the cchv daemon, kills what they left (shims, VMMs of VMs left to their pods), empties the erofs cache and starts again.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
# pkill -x cloud-hyperviso below kills every Cloud Hypervisor of the host: not on a host whose own cchv daemon runs
if [ -n "$RT_BINDS" ] && systemctl is-active --quiet cloudhv-sandbox-daemon; then echo "refusing: the host's own cloudhv-sandbox-daemon is running (stop it first)"; exit 1; fi
"$here/21-stop.sh" >/dev/null 2>&1
sudo pkill -f 'containerd-shim.* -namespace rtbench' 2>/dev/null; sudo pkill -x cloud-hyperviso 2>/dev/null; sudo pkill -x virtiofsd 2>/dev/null; sleep 2
[ -n "$RT_BINDS" ] || sudo umount "$RT_CRUN/erofs-cache" 2>/dev/null; sudo find "$RT_CRUN/erofs-cache" -mindepth 1 -delete 2>/dev/null   # what an earlier run left in the tmpfs under the bind mount
sudo find "$RT_CRUN/state" "$RT_DATA/erofs-cache" -mindepth 1 -delete 2>/dev/null; sudo rm -rf "$RT_RUN/containerd" 2>/dev/null
"$here/20-start.sh" | tail -2
