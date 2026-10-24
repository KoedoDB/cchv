#!/bin/bash
# What the host needs besides the downloads: erofs-utils (the cchv shim builds the container's read-only root file system with mkfs.erofs) and the symlinks that put
# the shims on the PATH of the benchmark's containerd. Packages are installed with apt on the benchmark host (a build host).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
command -v mkfs.erofs >/dev/null || sudo apt-get install -y erofs-utils >/dev/null
dpkg -s lvm2 >/dev/null 2>&1 || sudo apt-get install -y lvm2 >/dev/null
B="$RT_ROOT/bin"; sudo mkdir -p "$B" /opt/cloudhv
# Kata 4.x takes its configuration from the name of the handler: containerd-shim-kata-clh-runtime-rs-v2 uses configuration-clh-runtime-rs.toml (KATA_CONF_FILE is refused for files outside the shipped ones)
sudo ln -sf /opt/kata/runtime-rs/bin/containerd-shim-kata-v2 "$B/containerd-shim-kata-clh-runtime-rs-v2"
sudo ln -sf "$RT_ROOT/gvisor/containerd-shim-runsc-v1" "$B/containerd-shim-runsc-v1"
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon cloud-hypervisor; do sudo install -m 0755 "$RT_DATA/dl/cchv/$f" "$B/$f"; done
# Kata's configuration is chosen by runtime-rs/configuration.toml, which ships pointing at QEMU: point it at the Cloud Hypervisor configuration (run-kata.sh / rtbench_blk.py change it for the
# direct-I/O variant). Check with `pgrep -a cloud-hypervisor` while a Kata container runs: the first benchmark run used QEMU without anyone noticing.
sudo ln -sf configuration-clh-runtime-rs.toml /opt/kata/share/defaults/kata-containers/runtime-rs/configuration.toml
sudo install -m 0644 "$RT_DATA/dl/cchv/vmlinux" /opt/cloudhv/vmlinux
sudo install -m 0644 "$RT_DATA/dl/cchv/rootfs.erofs" /opt/cloudhv/rootfs.erofs
echo "prerequisites ok"
