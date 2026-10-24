#!/bin/bash
# Records what the numbers were measured on and with: the machine, the kernel and its mitigations, the governor, the versions and digests of everything, the configurations. Usage: 50-environment.sh RESULTS_DIR
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
OUT="${1:?results dir}"; mkdir -p "$OUT"
{
echo "date: $(date -u +%FT%TZ)"; echo "kernel: $(uname -r)"
lscpu | grep -E "Model name|^CPU\(s\)|Thread|Core|MHz|L3" ; free -m | sed -n 2p
echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"; echo "--- mitigations"
for f in /sys/devices/system/cpu/vulnerabilities/*; do echo "$(basename "$f"): $(cat "$f")"; done
echo "--- disks"; lsblk -d -o NAME,SIZE,MODEL,ROTA | grep -E "nvme|sd"; mount | grep -E " / " ; cat /proc/mdstat | head -4
echo "--- versions"
$RT_DOCKER version --format 'docker {{.Server.Version}} containerd {{range .Server.Components}}{{if eq .Name "containerd"}}{{.Version}}{{end}}{{end}} runc {{range .Server.Components}}{{if eq .Name "runc"}}{{.Version}}{{end}}{{end}}'
$RT_ROOT/gvisor/runsc --version | head -1
cat /opt/kata/VERSION 2>/dev/null; /opt/kata/bin/cloud-hypervisor --version 2>&1 | head -1; sudo /opt/kata/runtime-rs/bin/containerd-shim-kata-v2 --version 2>&1 | head -2
sudo $RT_ROOT/bin/cloud-hypervisor --version 2>&1 | head -1
echo "cchv ${CCHV_TAG}: $(sha256sum $RT_DATA/dl/cchv/* | awk '{print substr($1,1,12)" "$2}' | sed "s#$RT_DATA/dl/cchv/##" | tr '\n' ';')"
echo "--- images"; for i in $POSTGRES_IMAGE $NGINX_IMAGE alpine:3.24; do $RT_DOCKER image inspect "$i" --format '{{index .RepoDigests 0}}' 2>/dev/null || echo "$i (digest not known)"; done
echo "--- kata (guest kernel and config in use)"; grep -E "^(default_vcpus|default_memory|shared_fs|virtio_fs_daemon|kernel|image|block_device_driver|sandbox_cgroup_only)" /opt/kata/share/defaults/kata-containers/runtime-rs/configuration-clh-runtime-rs.toml | head -12
echo "--- cchv daemon.json"; sudo cat /opt/cloudhv/daemon.json; echo "--- cchv config.json"; sudo cat /opt/cloudhv/config.json
echo "--- dockerd"; sudo cat $RT_DATA/daemon.json
} > "$OUT/environment.txt" 2>&1
echo "written $OUT/environment.txt"
