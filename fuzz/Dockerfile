# The image the fuzz targets are built and run in: a nightly Rust (cargo-fuzz needs it), cargo-fuzz, protoc (containerd-client needs it),
# and the C++ compiler libFuzzer is built with. Usage: see README.md.
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler clang libclang-dev && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install nightly --profile minimal && rustup default nightly
RUN cargo install cargo-fuzz --locked
ENV CARGO_TARGET_DIR=/fuzz-target CARGO_BUILD_JOBS=4
WORKDIR /w
