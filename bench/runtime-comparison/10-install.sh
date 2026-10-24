#!/bin/bash
# Installs the four runtimes for the comparison under /opt/rtbench (run on the benchmark host as a user that may sudo). Idempotent: a file that is there and
# matches is not downloaded again. Nothing is started here (see 20-start.sh), and 90-cleanup.sh removes it all.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
OUT="${1:-$HOME/rtbench-results/$(date +%Y%m%d)}"; mkdir -p "$OUT"
DL="$RT_DATA/dl"; sudo mkdir -p "$DL" "$RT_ROOT/bin" "$RT_RUN"; sudo chown "$(id -u)" "$DL"
fetch() { # url file
  [ -s "$DL/$2" ] || curl -fL --retry 3 -o "$DL/$2" "$1"
}
{
echo "# $(date -u +%FT%TZ) $(uname -r)"
# 1. Docker (static; its own dockerd and containerd; the containerd image store is on by default in 29)
fetch "https://download.docker.com/linux/static/stable/x86_64/docker-$DOCKER_VERSION.tgz" docker.tgz
[ -x "$RT_ROOT/docker/dockerd" ] || { sudo mkdir -p "$RT_ROOT"; sudo tar -xzf "$DL/docker.tgz" -C "$RT_ROOT"; }
echo "docker-$DOCKER_VERSION.tgz sha256 $(sha256sum "$DL/docker.tgz" | cut -d' ' -f1)"
# 2. gVisor
base="https://storage.googleapis.com/gvisor/releases/release/$GVISOR_RELEASE/x86_64"
fetch "$base/gvisor.tar.zstd" gvisor.tar.zstd; fetch "$base/gvisor.tar.zstd.sha512" gvisor.tar.zstd.sha512
(cd "$DL" && sha512sum -c gvisor.tar.zstd.sha512 >/dev/null) || { echo "gVisor checksum mismatch"; exit 1; }
[ -x "$RT_ROOT/gvisor/runsc" ] || { sudo rm -rf "$RT_ROOT/gvisor"; sudo mkdir -p "$RT_ROOT/gvisor"; sudo tar --zstd -xf "$DL/gvisor.tar.zstd" -C "$RT_ROOT/gvisor"; }
echo "gvisor $GVISOR_RELEASE sha512 verified: $(cut -c1-16 "$DL/gvisor.tar.zstd.sha512")"
# 3. Kata Containers (static bundle; its files name /opt/kata)
fetch "https://github.com/kata-containers/kata-containers/releases/download/$KATA_VERSION/kata-static-$KATA_VERSION-amd64.tar.zst" kata.tar.zst
[ -x /opt/kata/bin/containerd-shim-kata-v2 ] || sudo tar --zstd -xf "$DL/kata.tar.zst" -C /
echo "kata-static-$KATA_VERSION-amd64.tar.zst sha256 $(sha256sum "$DL/kata.tar.zst" | cut -d' ' -f1)"
# 4. cchv (the files of the release, already downloaded and verified against cchv_sha256 on the machine that built the benchmark; they are copied here by 05-copy-cchv.sh)
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon cloud-hypervisor vmlinux rootfs.erofs; do
  [ -s "$DL/cchv/$f" ] || { echo "missing $DL/cchv/$f (run 05-copy-cchv.sh)"; exit 1; }
  echo "cchv $CCHV_TAG $f sha256 $(sha256sum "$DL/cchv/$f" | cut -d' ' -f1)"
done
} | tee "$OUT/versions.txt"
