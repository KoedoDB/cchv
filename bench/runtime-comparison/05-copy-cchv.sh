#!/bin/bash
# Copies the files of the cchv release to the benchmark host (run on the machine that has them) and checks them there against the checksums given in $CCHV_SHA
# (a file with lines "<sha256>  <name>": the release's checksums-sha256.txt, whose signature was verified with cosign). Usage: 05-copy-cchv.sh HOST DIR SHA_FILE
set -euo pipefail
host="$1"; dir="$2"; shafile="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
ssh "$host" "sudo mkdir -p $RT_DATA/dl/cchv && sudo chown \$(id -u) $RT_DATA/dl $RT_DATA/dl/cchv"
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon cloud-hypervisor vmlinux rootfs.erofs; do
  scp -q "$dir/$f" "$host:$RT_DATA/dl/cchv/$f"
  want="$(awk -v n="$f" '$2==n {print $1}' "$shafile")"
  have="$(ssh "$host" "sha256sum $RT_DATA/dl/cchv/$f | cut -d' ' -f1")"
  [ -n "$want" ] && [ "$want" = "$have" ] || { echo "checksum of $f differs: want '$want' have '$have'"; exit 1; }
  echo "ok $f"
done
