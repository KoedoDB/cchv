# Lightweight installer image for the release workflow.
# This Dockerfile packages pre-built binaries (not compiled here).
# The release workflow builds each component separately and copies
# them into the build context before running this Dockerfile.
#
# Cloud Hypervisor is built from source (main branch) to get
# userfaultfd OnDemand restore (PR #7800). Switch to a released
# version once CH >= v51 is published.

# --- Stage 1: Build Cloud Hypervisor from source ---
FROM rust:1.89-bookworm AS ch-builder

ARG TARGETARCH
ARG CH_REPO=https://github.com/cloud-hypervisor/cloud-hypervisor.git
# Pinned to the commit of the v54.0 tag, never a branch: this image runs privileged on the host, so what it
# builds must not change with the repository. To update, change the SHA to the commit of a release tag
# (`git ls-remote <repo> 'refs/tags/vN.M^{}'`), after reading that release's notes and advisories.
ARG CH_COMMIT=038ce5a72d761380a02f714a4eb4ca63e0ea073d

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev musl-tools git && \
    rm -rf /var/lib/apt/lists/*

RUN git clone ${CH_REPO} /build/cloud-hypervisor

WORKDIR /build/cloud-hypervisor
RUN git checkout ${CH_COMMIT} && \
    test "$(git rev-parse HEAD)" = "${CH_COMMIT}" && \
    if [ "$TARGETARCH" = "arm64" ]; then \
      RUST_TARGET=aarch64-unknown-linux-musl; \
    else \
      RUST_TARGET=x86_64-unknown-linux-musl; \
    fi && \
    rustup target add $RUST_TARGET && \
    cargo build --release --target $RUST_TARGET --bin cloud-hypervisor && \
    cp target/$RUST_TARGET/release/cloud-hypervisor /opt/cloud-hypervisor && \
    chmod 755 /opt/cloud-hypervisor

# --- Stage 2: Installer image ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=ch-builder /opt/cloud-hypervisor /opt/cloudhv/cloud-hypervisor

COPY opt/cloudhv/ /opt/cloudhv/

RUN chmod +x /opt/cloudhv/containerd-shim-cloudhv-v1 \
             /opt/cloudhv/cloudhv-sandbox-daemon \
             /opt/cloudhv/cloud-hypervisor \
             /opt/cloudhv/install.sh

ENTRYPOINT ["/opt/cloudhv/install.sh"]
