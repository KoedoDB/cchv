#!/bin/bash
# Takes the benchmark off the host: units, the second Docker and its data, the runtimes, the loop-device volume group, the firewall rule, the bridge, and puts the CPU governor back.
# The results (in the user's home) and these scripts stay. Packages installed by 12-prereqs.sh stay (erofs-utils is small); remove them by hand if you want.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
[ -z "$RT_BINDS" ] || { echo "RT_CONF is moved (a host with its own cchv): clean up by hand; this script removes /opt/kata and the volume group rtvg"; exit 1; }
"$here/21-stop.sh" 2>/dev/null
sudo pkill -f 'containerd-shim.* -namespace rtbench' 2>/dev/null; sudo pkill -x cloud-hyperviso 2>/dev/null; sudo pkill -x virtiofsd 2>/dev/null; sleep 1
sudo systemctl reset-failed 'rtbench-*' 2>/dev/null
mountpoint -q "$RT_DATA/mnt-ctl" && sudo umount "$RT_DATA/mnt-ctl"
sudo vgchange -an rtvg >/dev/null 2>&1; sudo vgremove -f rtvg >/dev/null 2>&1
dev="$(sudo losetup -j "$RT_DATA/loop.img" 2>/dev/null | cut -d: -f1 | head -1)"; [ -n "$dev" ] && sudo losetup -d "$dev"
"$here/40-governor.sh" restore
sudo umount /run/cloudhv/erofs-cache 2>/dev/null; sudo find /run/cloudhv -mindepth 1 -delete 2>/dev/null; sudo rmdir /run/cloudhv 2>/dev/null
sudo rm -rf "$RT_RUN" /opt/kata /opt/cloudhv "$RT_ROOT" "$RT_DATA"
echo "cleaned: $(ls -d /opt/rtbench /opt/kata /opt/cloudhv /var/lib/rtbench 2>/dev/null | wc -l) paths left; processes: $(pgrep -fc 'rtbench|cloudhv|cloud-hyperviso')"
