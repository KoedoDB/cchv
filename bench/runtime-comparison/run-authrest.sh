#!/bin/bash
# Auth + PostgREST, N pairs on each runtime (rtbench_authrest.py). Usage: run-authrest.sh RESULTS_DIR WORKDIR KOEDODB_PUBLIC_IMAGE_AUTH KOEDODB_PUBLIC_IMAGE_REST
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
export RT_RESULTS="${1:?results dir}"; work="${2:?workdir}"; export AR_AUTH_IMAGE="${3:?auth image}" AR_REST_IMAGE="${4:?rest image}"
mkdir -p "$RT_RESULTS"; exec > >(tee -a "$RT_RESULTS/run.log") 2>&1
"$here/40-governor.sh"; "$here/50-environment.sh" "$RT_RESULTS"
cd "$here"
for rt in runc kata-clh cloudhv; do
  RT_CCHV_VCPUS=2 "$here/22-reset.sh" >/dev/null 2>&1; python3 rtbench.py prepare >/dev/null 2>&1
  echo "=== $(date +%T) $rt"
  python3 rtbench_authrest.py prepare "$work" || { echo "!!! prepare failed"; continue; }
  python3 rtbench_authrest.py run $rt 8 "$work" || echo "!!! failed: $rt"
done
"$here/50-environment.sh" "$RT_RESULTS" >/dev/null
echo "=== $(date +%T) done"
