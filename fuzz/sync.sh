#!/bin/sh
# Copy files of the working tree into the snapshot (after a fix): fuzz/sync.sh crates/agent/src/spec.rs ...
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${FUZZ_SRC:-$HOME/.cchv-fuzz-src}
for f in "$@"; do cp "$ROOT/$f" "$OUT/cch/$f"; echo "synced $f"; done
