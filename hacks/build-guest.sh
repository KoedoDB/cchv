#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Containerised build for the full guest (kernel and root FS)
# Usage:
#   hacks/build-guest.sh [--arch <arch>]
# The optional <arch> can be x86-64 or arm64.

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  host_user_group="$1"
  shift

  apk add --no-cache erofs-utils file

  mkdir -p /opt/build-guest
  cd /opt/build-guest
  cp -a /host/* .

  build_if_missing "$host_user_group" /host/hacks/build-guest-kernel.sh -- vmlinux vmlinux.kconfig
  build_if_missing "$host_user_group" /host/hacks/build-static-rust.sh crates/agent/cloudhv-agent -- cloudhv-agent

  cd guest/rootfs
  cp /host/cloudhv-agent .

  ./build-rootfs.sh cloudhv-agent

  # build-rootfs.sh downloads crun itself. Check what it installed against checksums.txt, so a changed upstream file fails the
  # build here (this check can go once build-rootfs.sh uses download_verified: docs/supply-chain.md).
  case "$(uname -m)" in
    x86_64)  crun_arch="amd64" ;;
    aarch64) crun_arch="arm64" ;;
    *) echo "ERROR: unsupported architecture $(uname -m)"; exit 1 ;;
  esac
  verify_sha256 rootfs-base/bin/crun "crun-${CRUN_VERSION}-linux-${crun_arch}-disable-systemd"

  cp rootfs.erofs /host/
  chown "$host_user_group" /host/rootfs.erofs
}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  echo -e "\n ------=======#######  Full Guest Build #######=======-------\n"
  docker_wrapper ${@}
  echo -e "\n ------=======#######  Full Guest Build done #######=======-------\n"
  ls -lah vmlinux vmlinux.kconfig rootfs.erofs
fi
