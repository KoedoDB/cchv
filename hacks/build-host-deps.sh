#!/bin/sh
# vim: et ts=2 sw=2 syn=sh
#
# Containerised build for static versions of all host dependencies.
# Currently "mkfs.erofs" (from erofs-utils)
#
# Usage:
#   hacks/build-host-deps.sh [--arch <arch>]
# arch can be x86-64 or arm64.

set -euo pipefail
scriptdir="$(cd "$(dirname "$0")"; pwd)"
source "${scriptdir}/util.inc"

# Runs inside the container
build() {
  host_user_group="$1"
  shift
  
  echo -e "\n############  Installing build prerequisites ############\n"
  apk add --no-cache build-base \
      autoconf automake libtool lz4-dev util-linux-dev \
      util-linux-static zlib-dev zlib-static zstd-static lz4-static xz-static \
      openssl-dev openssl-libs-static git

  mkdir /opt/build-host-deps
  cd /opt/build-host-deps
  # The tag, and then the commit it must point to (versions.env): a snapshot tarball is generated on request and has no stable hash.
  # git.kernel.org timed out in two releases, so the clone is from the GitHub repo of erofs-utils (the tag peels to the same commit, checked below); GitHub can fail too: 5 tries, 10/20/40/80 s apart. The commit is checked right below.
  try=1
  until git clone --quiet --depth 1 --branch "v${EROFS_UTILS_VERSION}" \
      https://github.com/erofs/erofs-utils.git "erofs-utils-${EROFS_UTILS_VERSION}"; do
    if [ "$try" -ge 5 ]; then echo "ERROR: git clone of erofs-utils failed after $try tries" >&2; exit 1; fi
    echo "git clone failed (try $try), retrying" >&2
    rm -rf "erofs-utils-${EROFS_UTILS_VERSION}"
    sleep $((10 * (1 << (try - 1))))
    try=$((try + 1))
  done
  cd "erofs-utils-${EROFS_UTILS_VERSION}"
  if [ "$(git rev-parse HEAD)" != "${EROFS_UTILS_COMMIT}" ] ; then
    echo "ERROR: erofs-utils v${EROFS_UTILS_VERSION} is $(git rev-parse HEAD), expected ${EROFS_UTILS_COMMIT} (versions.env)" >&2
    exit 1
  fi

  autoreconf -fiv
  CFLAGS='--static --static -Wl,-static' LDFLAGS="-static --static -Wl,-static" \
    ./configure --prefix=/usr/local/ --enable-static --disable-shared
  CFLAGS='--static --static -Wl,-static' LDFLAGS="-static --static -Wl,-static" \
    make -j
  make DESTDIR="$(pwd)/_install" install

  cp _install/usr/local/bin/mkfs.erofs /host
  strip /host/mkfs.erofs
  chown "$host_user_group" /host/mkfs.erofs
}
# --
  
if [ "${1:-}" = "build" ] ; then
  shift
  build ${@}
else
  docker_wrapper ${@}
fi
