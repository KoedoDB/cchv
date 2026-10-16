#!/usr/bin/env sh
#
# Build a minimal guest rootfs for Cloud Hypervisor microVMs.
#
# Contents:
# - cloudhv-agent binary (statically linked, as PID 1)
# - crun binary (statically linked, lightweight OCI runtime)
# - Minimal /etc (passwd, group)
# - Essential directory structure
#
# No shell, no busybox, no package manager — absolute minimum for containers.
#
# Usage: ./build-rootfs.sh <path-to-cloudhv-agent-binary>
#
set -euo pipefail

AGENT_BINARY="${1:?Usage: build-rootfs.sh <path-to-cloudhv-agent-binary>}"
ROOTFS_DIR="rootfs-base"
IMAGE_FILE="rootfs.erofs"

echo "=== Building minimal guest rootfs ==="

# Verify agent binary exists and is static
if [ ! -f "${AGENT_BINARY}" ]; then
    echo "ERROR: Agent binary not found: ${AGENT_BINARY}"
    exit 1
fi

if file "${AGENT_BINARY}" | grep -q "dynamically linked"; then
    echo "WARNING: Agent binary is dynamically linked. Should be static (musl)."
fi

# Clean previous build
rm -f "${IMAGE_FILE}"

echo "Installing agent binary as /init..."
cp "${AGENT_BINARY}" "${ROOTFS_DIR}/bin/cloudhv-agent"
chmod 755 "${ROOTFS_DIR}/bin/cloudhv-agent"

# Install crun (lightweight OCI runtime, must be statically linked for the VM guest)
echo "Installing crun..."
CRUN_VERSION="1.30.1"
ARCH=$(uname -m)
# SHA-256 of the release binaries (the same as hacks/checksums.txt). Taken from the downloads of 2026-10-16 (the amd64 one twice,
# identical) and checked against the release's detached .asc signatures, which verify with the maintainer's key
# AC40 4C1C 0BF7 35C6 3FF4 D562 263D 6DF2 E163 E1EA (signing subkey AF60FCA3CDAA6DEAD157EA3A67E38F7A8BA21772). The binary runs as root
# in every VM, so a changed download must stop the build. Check crun's security advisories before changing the version, and update the
# hashes (here and in hacks/checksums.txt) with it.
case "${ARCH}" in
    x86_64)
        CRUN_ARCH="amd64"
        CRUN_SHA256="e7b059390e27d0f283e1debbe89d9c5847870f3d8325f31ccd954533fdd6ea78" ;;
    aarch64)
        CRUN_ARCH="arm64"
        CRUN_SHA256="6ac05ea41343cf91b12832c69f9a2a8c6dc5380032f4c0e02a46cc6aba8804f5" ;;
    *) echo "Unsupported arch: ${ARCH}"; exit 1 ;;
esac
wget -q "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/crun-${CRUN_VERSION}-linux-${CRUN_ARCH}-disable-systemd" \
    -O "${ROOTFS_DIR}/bin/crun"
if ! echo "${CRUN_SHA256}  ${ROOTFS_DIR}/bin/crun" | sha256sum -c - >/dev/null 2>&1; then
    echo "ERROR: crun ${CRUN_VERSION} (${CRUN_ARCH}) does not match its pinned SHA-256"
    rm -f "${ROOTFS_DIR}/bin/crun"
    exit 1
fi
chmod 755 "${ROOTFS_DIR}/bin/crun"

# Verify crun is static (dynamically-linked crun from the host won't work in the VM)
if file "${ROOTFS_DIR}/bin/crun" | grep -q "dynamically linked"; then
    echo "ERROR: crun binary is dynamically linked — must be static for guest rootfs"
    exit 1
fi

# Agent configuration
if [ -f "agent.conf" ]; then
    cp agent.conf "${ROOTFS_DIR}/etc/cloudhv-agent.conf"
fi

# Create filesystem image
echo "Creating root filesystem image"
mkfs.erofs --all-root --exclude-regex '.*\.gitkeep' "${IMAGE_FILE}" "${ROOTFS_DIR}"

# Report size
IMAGE_ACTUAL_SIZE=$(du -sh "${IMAGE_FILE}" | cut -f1)
ROOTFS_SIZE=$(du -sh "${ROOTFS_DIR}" | cut -f1)
echo "=== Rootfs built ==="
echo "  Contents:"
find "${ROOTFS_DIR}" -type f -not -name '*.gitkeep' -exec ls -lh {} \; | awk '{print "    " $5 " " $9}'
echo "----"
echo "  Directory: ${ROOTFS_SIZE} (${ROOTFS_DIR}/)"
echo "  Image:     ${IMAGE_ACTUAL_SIZE} (${IMAGE_FILE})"
