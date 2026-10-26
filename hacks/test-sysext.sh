#!/bin/bash
#
# Test script for the sysext demo
#

set -euo pipefail

function read_until() {
  local fd="$1"
  shift
  local match_string="${@}"

  echo "Read until: looking for '${match_string}'"

  local line
  while read -ru ${fd} line; do \
    echo "VM :: $line"
    if [[ "$line" == *"${match_string}"* ]] ; then
      echo -e "\n MATCH: '${match_string}'"
      break
    fi
  done
}
# --

function child_procs() {
  local ppid="$(ps -o pid= --ppid $1 | tr -d '\n')"
  if [[ -n "$ppid" ]]; then
    echo -n "$ppid "
    child_procs "$ppid"
  fi
}
# --

if [[ ! -f containerd-cloudhypervisor.raw ]]; then
  echo "ERROR: system extension image 'containerd-cloudhypervisor.raw' not found."
  exit 1
fi

echo "Fetching bakery for Flatcar VM automation"
if [[ ! -d sysext-bakery ]]; then 
  # Pinned to a commit (versions.env), not the head of main: the bakery's script downloads and boots a Flatcar image.
  . "$(dirname "$0")/versions.env"
  git init --quiet sysext-bakery
  git -C sysext-bakery fetch --quiet --depth 1 https://github.com/flatcar/sysext-bakery.git "${SYSEXT_BAKERY_COMMIT}"
  git -C sysext-bakery checkout --quiet FETCH_HEAD
  test "$(git -C sysext-bakery rev-parse HEAD)" = "${SYSEXT_BAKERY_COMMIT}"
fi
cp containerd-cloudhypervisor.raw sysext-bakery
cd sysext-bakery

# The bakery boots the Flatcar *alpha* channel (lib/boot.sh). Alpha's "current" points at 4860.0.0, whose files are not on the download CDN
# (404, 2026-10-26), so the test never reached a VM. A release we test on is a stable one: the channel is replaced here, in the pinned copy.
# The check below fails the test if the bakery's line is not the one we expect (a new pin has to be looked at).
FLATCAR_CHANNEL="${FLATCAR_CHANNEL:-stable}"
grep -q 'https://alpha.release.flatcar-linux.net/${arch}-usr/current/' lib/boot.sh || { echo "ERROR: lib/boot.sh of the bakery has no alpha download line to replace"; exit 1; }
sed -i "s#https://alpha.release.flatcar-linux.net/#https://${FLATCAR_CHANNEL}.release.flatcar-linux.net/#" lib/boot.sh

echo "Running the test (includes Flatcar image download)"
coproc flatcar { ./bakery.sh boot containerd-cloudhypervisor.raw 2>&1; }
read_until "${flatcar[0]}" "localhost login:"
sleep 1
echo "sudo /usr/share/cloudhv/demo/demo.sh cleanup; rc=\"\$?\"; echo 'TEST_IS_DONE'; echo \"TEST_EXIT_CODE=\$rc\"" >&${flatcar[1]}
read_until "${flatcar[0]}" "TEST_IS_DONE" # input is echoed on the console
read_until "${flatcar[0]}" "TEST_IS_DONE"
read -ru "${flatcar[0]}" exit_code_line

exit_code="$(echo "${exit_code_line}" | sed 's/.*TEST_EXIT_CODE=\([0-9]*\).*/\1/')"
echo "Exit code is '$exit_code'"
kill -9 $(child_procs "$flatcar_PID")

exit $exit_code
