#!/bin/sh
# Run every target, four at a time (one core each), SECONDS each:  fuzz/run-all.sh [SECONDS]
set -eu
cd "$(dirname "$0")"
S=${1:-600}
./run-batch.sh "$S" agent_spec daemon_tree shim_volumes shimkit_task
./run-batch.sh "$S" agent_volumes daemon_validate agent_small daemon_request
./run-batch.sh "$S" agent_proto agent_logs daemon_ctl shim_small
