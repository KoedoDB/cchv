#!/usr/bin/env bash
# Smoke test of installer/install.sh. A throwaway container plays the node (its own filesystem is the "host", HOST=/);
# nsenter, systemctl, systemd-run, tc and mkfs.erofs are stubs. It checks the order of things and the failure paths
# (nothing is registered with containerd when a check fails; a second run changes nothing), not systemd or containerd.
#
# Needs docker. Usage: bash installer/smoke-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE="debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad"
docker run --rm -v "$PWD/installer:/installer:ro" "$IMAGE" bash /installer/smoke-test-inner.sh
