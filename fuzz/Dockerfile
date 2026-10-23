# The image the fuzz targets are built and run in: a nightly Rust (cargo-fuzz needs it), cargo-fuzz, protoc (containerd-client needs it),
# and the C++ compiler libFuzzer is built with. Usage: see README.md.
FROM rust:1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler clang libclang-dev && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install nightly --profile minimal && rustup default nightly
RUN cargo install cargo-fuzz --locked
ENV CARGO_TARGET_DIR=/fuzz-target CARGO_BUILD_JOBS=4
WORKDIR /w
