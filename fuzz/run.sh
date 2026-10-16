#!/bin/sh
# Run one fuzz target:  fuzz/run.sh TARGET [SECONDS] [extra libFuzzer args]
# Uses one CPU core. Run at most as many of these at once as you want cores to be busy. The sources are the snapshot in
# $FUZZ_SRC (default ~/.cchv-fuzz-src, made by snapshot.sh) when it exists, else the working tree.
set -eu
cd "$(dirname "$0")"
T=$1; S=${2:-600}; shift; [ $# -gt 0 ] && shift || true
[ -d "${FUZZ_SRC:-$HOME/.cchv-fuzz-src}/cch" ] && export FUZZ_SRC=${FUZZ_SRC:-$HOME/.cchv-fuzz-src}
DICT=""
case $T in
  agent_spec) DICT="-dict=dict/oci.dict" ;;
  daemon_validate|daemon_request) DICT="-dict=dict/daemon.dict" ;;
esac
mkdir -p "corpus/$T" "seeds/$T"
FUZZ_CPUS=1 ./in-docker.sh cargo fuzz run -O "$T" "corpus/$T" "seeds/$T" -- \
  -max_total_time="$S" -timeout=20 -rss_limit_mb=2048 -print_final_stats=1 $DICT "$@"
