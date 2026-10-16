#!/bin/sh
# Run a command in the fuzz image (fuzz/Dockerfile) with the repository at /w and the shimkit repository at /containerd-shimkit.
# At most FUZZ_CPUS (default 4) cores are used. Usage: fuzz/in-docker.sh cargo fuzz build
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SHIMKIT=${SHIMKIT_DIR:-$ROOT/../containerd-shimkit}
# With FUZZ_SRC (a snapshot made by snapshot.sh) the sources are the snapshot's; fuzz/ is always this directory.
if [ -n "${FUZZ_SRC:-}" ]; then SRC=$FUZZ_SRC/cch; SHIMKIT=$FUZZ_SRC/shimkit; else SRC=$ROOT; fi
exec docker run --rm --cpus="${FUZZ_CPUS:-4}" --tmpfs /tmp:rw,exec,size=3g \
  -v "$SRC":/w -v "$ROOT/fuzz":/w/fuzz -v "$SHIMKIT":/containerd-shimkit \
  -v cchv-fuzz-cargo:/usr/local/cargo/registry -v cchv-fuzz-git:/usr/local/cargo/git -v cchv-fuzz-target:/fuzz-target \
  -w /w/fuzz ${FUZZ_DOCKER_ARGS:-} cchv-fuzz nice -n 19 "$@"
