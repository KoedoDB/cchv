#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Containerised build for the containerd/cloud-hypervisor system extension image.
# Usage:
#   hacks/build-sysext.sh [--arch <arch>]
# The optional <arch> can be x86-64 or arm64. The resulting image is containerd-cloudhypervisor.raw.

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  local host_user_group="$1"

  apk add --no-cache erofs-utils wget

  mkdir -p /opt/build-sysext
  cd /opt/build-sysext
  cp -a /host/* .

  build_if_missing "$host_user_group" /host/hacks/build-guest.sh -- vmlinux vmlinux.kconfig rootfs.erofs
  build_if_missing "$host_user_group" /host/hacks/build-static-rust.sh containerd-shim-cloudhv -- containerd-shim-cloudhv-v1
  build_if_missing "$host_user_group" /host/hacks/build-static-rust.sh cloudhv-sandbox-daemon -- cloudhv-sandbox-daemon
  build_if_missing "$host_user_group" /host/hacks/build-host-deps.sh -- mkfs.erofs

  cd sysext
  mkdir -p root/usr/bin \
           root/usr/share/cloudhv/guest \
           root/usr/libexec/cni

  cp /host/vmlinux /host/vmlinux.kconfig /host/rootfs.erofs root/usr/share/cloudhv/guest/
  cp /host/containerd-shim-cloudhv-v1 \
     /host/cloudhv-sandbox-daemon \
     /host/mkfs.erofs \
        root/usr/bin

  local arch="$(translate_arch)"
  sed -i "s/^ARCHITECTURE=.*/ARCHITECTURE=${arch}/" \
      root/usr/lib/extension-release.d/extension-release.containerd-cloudhypervisor

  # CNI plugins
  local cni_arch="${arch}"
  if [ "${cni_arch}" = "x86-64" ] ; then
    cni_arch="amd64"
  fi
  cni_tgz="cni-plugins-linux-${cni_arch}-${CNI_VERSION}.tgz"
  download_verified \
    "https://github.com/containernetworking/plugins/releases/download/${CNI_VERSION}/${cni_tgz}" "${cni_tgz}" "${cni_tgz}"
  tar -C root/usr/libexec/cni -xzf "${cni_tgz}"

  # Cloud hypervisor
  local ch_sufx=""
  if [ "${arch}" = "arm64" ] ; then
    ch_sufx="-aarch64"
  fi
  download_verified \
    "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/cloud-hypervisor-static${ch_sufx}" \
    root/usr/bin/cloud-hypervisor "cloud-hypervisor-${CH_VERSION}-static${ch_sufx}"
  chmod 755 root/usr/bin/cloud-hypervisor

  # The daemon's console on the guest: the serial port of the architecture (daemon.json has a placeholder).
  local console="console=ttyS0"
  if [ "${arch}" = "arm64" ] ; then
    console="console=ttyAMA0"
  fi
  sed -i "s/\${KERNEL_CONSOLE}/${console}/" root/usr/share/cloudhv/daemon.json

  mkfs.erofs --all-root --exclude-regex '.*\.gitkeep' containerd-cloudhypervisor.raw root
  cp containerd-cloudhypervisor.raw /host
  chown "$host_user_group" /host/containerd-cloudhypervisor.raw
}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  echo -e "\n ------=======####### Building the system extension #######=======-------\n"
  docker_wrapper ${@}
  echo -e "\n ------=======#######  Sysext Build done #######=======-------\n"
fi
