#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Checksum-checked downloads. Meant to be sourced (works in sh, bash and busybox ash):
#
#   HACKS_DIR="$(cd "$(dirname "$0")/../../hacks" && pwd)"   # path to this directory
#   . "$HACKS_DIR/verify-download.sh"
#   download_verified "$URL" "linux-6.18.57.tar.xz" "linux-6.18.57.tar.xz"
#
# The expected SHA-256 comes from checksums.txt (next to this file, or $CHECKSUMS_FILE), looked up by NAME.
# A name that is not listed, or a file that does not match, fails (return 1) and the download is removed.

if [ -z "${CHECKSUMS_FILE:-}" ]; then
  if [ -n "${HACKS_DIR:-}" ]; then
    CHECKSUMS_FILE="${HACKS_DIR}/checksums.txt"
  elif [ -n "${scriptdir:-}" ]; then
    CHECKSUMS_FILE="${scriptdir}/checksums.txt"
  else
    echo "verify-download.sh: set HACKS_DIR (or CHECKSUMS_FILE) before sourcing it" >&2
    return 1 2>/dev/null || exit 1
  fi
fi

# expected_sha256 NAME -> prints the SHA-256 listed for NAME (nothing if not listed)
expected_sha256() {
  awk -v n="$1" '!/^#/ && $2 == n { print $1; exit }' "$CHECKSUMS_FILE"
}

# verify_sha256 FILE NAME
verify_sha256() {
  _vd_want="$(expected_sha256 "$2")"
  if [ -z "$_vd_want" ]; then
    echo "ERROR: no checksum for '$2' in $CHECKSUMS_FILE (add one after checking it against upstream: docs/supply-chain.md)" >&2
    return 1
  fi
  _vd_got="$(sha256sum "$1" | awk '{ print $1 }')"
  if [ "$_vd_got" != "$_vd_want" ]; then
    echo "ERROR: checksum mismatch for '$2': expected $_vd_want, got $_vd_got" >&2
    return 1
  fi
}

# download_verified URL DEST NAME: downloads to a temporary file, checks it, and only then moves it to DEST.
download_verified() {
  _vd_tmp="$(mktemp "$2.XXXXXX")"
  if ! wget -q -O "$_vd_tmp" "$1"; then
    rm -f "$_vd_tmp"
    echo "ERROR: download failed: $1" >&2
    return 1
  fi
  if ! verify_sha256 "$_vd_tmp" "$3"; then
    rm -f "$_vd_tmp"
    return 1
  fi
  mv "$_vd_tmp" "$2"
}
