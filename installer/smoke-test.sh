#!/usr/bin/env bash
# Smoke test of installer/install.sh. A throwaway container plays the node (its own filesystem is the "host", HOST=/);
# nsenter, systemctl, systemd-run, tc and mkfs.erofs are stubs. It checks the order of things and the failure paths
# (nothing is registered with containerd when a check fails; a second run changes nothing), not systemd or containerd.
#
# Needs docker. Usage: bash installer/smoke-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE="debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587"
docker run --rm -v "$PWD/installer:/installer:ro" "$IMAGE" bash /installer/smoke-test-inner.sh
