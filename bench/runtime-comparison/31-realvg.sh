#!/bin/bash
# The cchv database volume in a REAL volume group (RT_VG, e.g. the host's `vg` on its RAID 1; see env.sh), where 30-loopvg.sh makes a group on a loop device. One logical volume,
# ${RT_LVP}pgdata (8 GiB): cchv's PostgreSQL volume, and Kata's block-device runs use it too (one run at a time, zeroed before each). The density runs make ${RT_LVP}* volumes of 1 GiB
# each (rtbench.py) in the same group. Nothing else in the group is touched.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
sudo vgs "$RT_VG" >/dev/null
sudo lvs "$RT_VG/${RT_LVP}pgdata" >/dev/null 2>&1 || { sudo lvcreate -y -L 8G -n "${RT_LVP}pgdata" "$RT_VG" >/dev/null; sudo blkdiscard --zeroout "/dev/$RT_VG/${RT_LVP}pgdata"; }
echo "volume group $RT_VG: $(sudo lvs --noheadings -o lv_name,lv_size "$RT_VG" | tr -s ' ' | tr '\n' ';') free: $(sudo vgs --noheadings -o vg_free --units g "$RT_VG" | tr -d ' ')"
