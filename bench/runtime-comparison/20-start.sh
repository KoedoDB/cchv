#!/bin/bash
# Starts the benchmark's own Docker (a second dockerd with its own containerd, data root, socket and bridge; the host's Docker is not touched) and the cchv
# sandbox daemon, as transient systemd units. 21-stop.sh stops them.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
sudo systemctl reset-failed "rtbench-*" 2>/dev/null || true
sudo mkdir -p "$RT_RUN" "$RT_DATA/docker" "$RT_CRUN" "$RT_CONF" "$RT_DATA/erofs-cache" "$RT_CRUN/erofs-cache"
# The host's FORWARD policy is DROP and br_netfilter is loaded, so traffic between two containers on the benchmark's bridge needs a rule (in DOCKER-USER, which exists for rules of one's own;
# removed by 21-stop.sh)
# (a host without Docker has no DOCKER-USER chain and a FORWARD policy of ACCEPT: nothing to add)
if sudo iptables -S DOCKER-USER >/dev/null 2>&1; then sudo iptables -C DOCKER-USER -i rtbench0 -o rtbench0 -j ACCEPT 2>/dev/null || sudo iptables -I DOCKER-USER 1 -i rtbench0 -o rtbench0 -j ACCEPT; fi
# The bridge is made here (Docker's -b and --bip exclude each other; the host's Docker owns docker0 and 172.17/16)
ip link show rtbench0 >/dev/null 2>&1 || { sudo ip link add rtbench0 type bridge; sudo ip addr add "$RT_BRIDGE_IP" dev rtbench0; sudo ip link set rtbench0 up; }
# Docker: the runtimes. runc is Docker's own; runsc by path; Kata and cchv are containerd shims found on the PATH (12-prereqs.sh).
sudo tee "$RT_DATA/daemon.json" >/dev/null <<JSON
{
  "features": {"containerd-snapshotter": true},
  "data-root": "$RT_DATA/docker",
  "exec-root": "$RT_RUN/docker",
  "pidfile": "$RT_RUN/docker.pid",
  "hosts": ["$RT_SOCK"],
  "bridge": "rtbench0",
  "fixed-cidr": "$RT_SUBNET",
  "containerd": "$RT_RUN/containerd.sock",
  "containerd-namespace": "rtbench",
  "containerd-plugins-namespace": "rtbench-plugins",
  "iptables": false,
  "ip6tables": false,
  "ip-forward": false,
  "ip-masq": false,
  "userland-proxy": false,
  "exec-opts": ["native.cgroupdriver=cgroupfs"],
  "cgroup-parent": "rtbench",
  "default-runtime": "runc",
  "runtimes": {
    "runsc": {"path": "$RT_ROOT/gvisor/runsc"},
    "kata-clh": {"runtimeType": "io.containerd.kata-clh-runtime-rs.v2"},
    "cloudhv": {"runtimeType": "io.containerd.cloudhv.v1"}
  }
}
JSON
# cchv: the daemon and the shim's configuration (the keys are those of docs/try-with-docker.md; the volume keys are the ones of docs/sandbox-daemon.md)
sudo tee "$RT_CONF/daemon.json" >/dev/null <<JSON
{
  "pool_size": 3, "default_vcpus": ${RT_CCHV_VCPUS:-2}, "default_memory_mb": 320,
  "cloud_hypervisor_binary": "$RT_ROOT/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux", "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "socket_path": "/run/cloudhv/daemon.sock", "state_dir": "/run/cloudhv/state",
  "ch_rootless": true, "allow_proc_netns": true,
  "emptydir_dir": "$RT_DATA/cchv-emptydir",
  "volume_block_prefixes": ["${RT_VG}-${RT_LVP//-/--}"], "volume_block_format_if_blank": true,
  "erofs_cache_dir": "/run/cloudhv/erofs-cache"
}
JSON
sudo tee "$RT_CONF/config.json" >/dev/null <<JSON
{
  "cloud_hypervisor_binary": "$RT_ROOT/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux", "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "default_vcpus": ${RT_CCHV_VCPUS:-2}, "default_memory_mb": 320,
  "daemon_socket": "/run/cloudhv/daemon.sock", "mkfs_erofs_path": "/usr/bin/mkfs.erofs",
  "require_pod_cgroup": false, "erofs_cache_max_mib": 20480, "erofs_cache_dir": "/run/cloudhv/erofs-cache",
  "volume_source_roots": ["$RT_DATA/docker/containers", "$RT_DATA/docker/volumes"]
}
JSON
sudo mkdir -p "$RT_DATA/cchv-emptydir" "$RT_CRUN/state" "$RT_DATA/erofs-cache" "$RT_CRUN/erofs-cache"
# erofs_cache_dir (cchv v0.4: the images are on the disk by default, /var/lib/cloud-hypervisor/erofs-cache, which on stg-db-001 is the host's own cache) is set to the
# directory below, so that the benchmark keeps its own: bind-mounted from $RT_DATA, i.e. on the disk as well.
# Under Docker the shim builds one erofs image of the container's root file system per container (the key is the directory of the container's own mount; under Kubernetes the key is the image's, so
# pods of one image share it). /run is a small tmpfs (3.2 GB here) and a PostgreSQL image is 538 MiB, so the cache is a directory on the disk, bind-mounted over /run/cloudhv/erofs-cache.
# (with RT_CONF moved the units get this directory by BindPaths= and the host's /run/cloudhv is not touched)
[ -n "$RT_BINDS" ] || mountpoint -q "$RT_CRUN/erofs-cache" || sudo mount --bind "$RT_DATA/erofs-cache" "$RT_CRUN/erofs-cache"
PATHS="$RT_ROOT/bin:$RT_ROOT/docker:/usr/sbin:/usr/bin:/sbin:/bin"
if ! systemctl is-active --quiet rtbench-cchv-daemon; then
  # Two cchv daemons side by side only work with their own /run/cloudhv (RT_BINDS, from RT_CONF): without it this one would replace the socket of the host's daemon.
  if [ -z "$RT_BINDS" ] && systemctl is-active --quiet cloudhv-sandbox-daemon; then echo "refusing: the host's own cloudhv-sandbox-daemon is running and RT_CONF is not set (see env.sh)"; exit 1; fi
  sudo systemd-run --unit=rtbench-cchv-daemon --property=Restart=no $RT_BINDS -E CLOUDHV_MEMORY_RESTORE_MODE=Copy -E RUST_LOG=info \
    "$RT_ROOT/bin/cloudhv-sandbox-daemon" /opt/cloudhv/daemon.json >/dev/null
fi
# The benchmark's own containerd. Docker 29 would otherwise use the host's (/run/containerd/containerd.sock) when it exists: found out the hard way, the first start shared the host's containerd;
# the image it pulled there was removed with `ctr -n moby images rm`.
sudo tee "$RT_DATA/containerd.toml" >/dev/null <<TOML
version = 3
root = "$RT_DATA/containerd"
state = "$RT_RUN/containerd"
disabled_plugins = ["io.containerd.grpc.v1.cri", "io.containerd.cri.v1.images", "io.containerd.cri.v1.runtime"]
[grpc]
  address = "$RT_RUN/containerd.sock"
TOML
if ! systemctl is-active --quiet rtbench-containerd; then
  sudo systemd-run --unit=rtbench-containerd $RT_BINDS --property=Delegate=yes --property=TasksMax=infinity --property=LimitNOFILE=1048576 \
    -E PATH="$PATHS" \
    "$RT_ROOT/docker/containerd" --config "$RT_DATA/containerd.toml" >/dev/null
  for i in $(seq 30); do [ -S "$RT_RUN/containerd.sock" ] && break; sleep 1; done
fi
if ! systemctl is-active --quiet rtbench-dockerd; then
  sudo systemd-run --unit=rtbench-dockerd --property=Delegate=yes --property=TasksMax=infinity --property=LimitNOFILE=1048576 \
    -E PATH="$PATHS" \
    "$RT_ROOT/docker/dockerd" --config-file "$RT_DATA/daemon.json" >/dev/null
fi
for i in $(seq 60); do $RT_DOCKER version >/dev/null 2>&1 && break; sleep 1; done
$RT_DOCKER info --format 'docker {{.ServerVersion}} runtimes={{range $k,$v := .Runtimes}}{{$k}} {{end}} driver={{.DriverStatus}}'
for i in $(seq 60); do sudo journalctl -u rtbench-cchv-daemon -o cat --no-pager 2>/dev/null | grep -q "pool initialized" && { echo "cchv pool ready"; break; }; sleep 1; done
