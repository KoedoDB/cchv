#!/bin/sh
# Run several targets at the same time, one core each:  fuzz/run-batch.sh SECONDS TARGET...   (logs in fuzz/logs/)
set -eu
cd "$(dirname "$0")"
S=$1; shift
mkdir -p logs
for T in "$@"; do
  ./run.sh "$T" "$S" > "logs/$T.log" 2>&1 &
done
wait
for T in "$@"; do
  echo "== $T: $(grep -E '^(stat::number_of_executed_units|stat::peak_rss_mb|#[0-9]+.*DONE| *artifact_prefix)' "logs/$T.log" | tail -3 | tr '\n' ' ')"
  grep -E "SUMMARY|panicked at|Test unit written to" "logs/$T.log" | head -3
done
