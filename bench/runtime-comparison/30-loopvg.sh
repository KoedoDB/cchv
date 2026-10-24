#!/bin/bash
# A volume group for the cchv database volume (the only runtime that gets a raw block device) and for the control runs: a sparse file on the host's
# file system, as a loop device with direct I/O, with the group `rtvg` on it. This puts a loop device and the host's ext4 under the volume: say so wherever the
# numbers are used. Logical volumes: pgdata (cchv, formatted by the runtime), ctl (mounted on the host for the runc control run).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
SIZE="${1:-48G}"
sudo mkdir -p "$RT_DATA"
[ -f "$RT_DATA/loop.img" ] || sudo truncate -s "$SIZE" "$RT_DATA/loop.img"
dev="$(sudo losetup -j "$RT_DATA/loop.img" | cut -d: -f1 | head -1)"
[ -n "$dev" ] || dev="$(sudo losetup --direct-io=on -f --show "$RT_DATA/loop.img")"
sudo vgs rtvg >/dev/null 2>&1 || sudo vgcreate rtvg "$dev" >/dev/null
sudo lvs rtvg/pgdata >/dev/null 2>&1 || { sudo lvcreate -y -L 8G -n pgdata rtvg >/dev/null; sudo blkdiscard --zeroout /dev/rtvg/pgdata; }
sudo lvs rtvg/ctl >/dev/null 2>&1 || { sudo lvcreate -y -L 8G -n ctl rtvg >/dev/null; sudo mkfs.ext4 -q -E lazy_itable_init=0 /dev/rtvg/ctl; }
sudo mkdir -p "$RT_DATA/mnt-ctl"; mountpoint -q "$RT_DATA/mnt-ctl" || sudo mount /dev/rtvg/ctl "$RT_DATA/mnt-ctl"
echo "loop device $dev, volume group rtvg: $(sudo lvs --noheadings -o lv_name,lv_size rtvg | tr -s ' ' | tr '\n' ';')"
