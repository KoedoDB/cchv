#!/usr/bin/env bash
# vim: et ts=2 sw=2
#
# Re-checks the entries of hacks/checksums.txt against what the upstream projects publish, and the
# commit pins in hacks/versions.env against the upstream tags. Run it when you change a version, and
# now and then (a pinned file that no longer matches upstream is a sign of tampering or a re-upload).
#
# Needs: curl, jq, git, gpg, sha256sum. Network access. Set GITHUB_TOKEN to avoid GitHub's anonymous API limit. Changes nothing in the repository.
#
# Usage: bash hacks/check-upstream.sh

set -euo pipefail
HACKS_DIR="$(cd "$(dirname "$0")"; pwd)"
# shellcheck source=versions.env
. "${HACKS_DIR}/versions.env"
# shellcheck disable=SC2034  # read by verify-download.sh
CHECKSUMS_FILE="${HACKS_DIR}/checksums.txt"
# shellcheck source=verify-download.sh
. "${HACKS_DIR}/verify-download.sh"

fail=0
ok()  { echo "  ok    $*"; }
bad() { echo "  FAIL  $*"; fail=1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
export GNUPGHOME="${work}/gnupg"; mkdir -m 700 "$GNUPGHOME"

# GitHub's API allows few anonymous requests; use GITHUB_TOKEN (any token, no scopes needed) when it is set.
ghapi() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$1"
  else
    curl -fsSL "$1"
  fi
}

# listed NAME EXPECTED: the pinned entry NAME equals EXPECTED
listed() {
  local have; have="$(expected_sha256 "$1")"
  if [ "$have" = "$2" ]; then ok "$1"; else bad "$1: checksums.txt has '${have}', upstream says '$2'"; fi
}

echo "Cloud Hypervisor ${CH_VERSION}: GitHub's digest of the release assets"
for sfx in "" "-aarch64"; do
  d="$(ghapi "https://api.github.com/repos/cloud-hypervisor/cloud-hypervisor/releases/tags/${CH_VERSION}" \
        | jq -r --arg n "cloud-hypervisor-static${sfx}" '.assets[] | select(.name == $n) | .digest' | sed 's/^sha256://')"
  listed "cloud-hypervisor-${CH_VERSION}-static${sfx}" "$d"
done
c="$(git ls-remote https://github.com/cloud-hypervisor/cloud-hypervisor.git "refs/tags/${CH_VERSION}^{}" | cut -f1)"
[ "$c" = "$CH_COMMIT" ] && ok "CH_COMMIT is the commit of ${CH_VERSION}" || bad "CH_COMMIT is ${CH_COMMIT}, the tag points to ${c}"

echo "CNI plugins ${CNI_VERSION}: the .sha256 files of the release"
for a in amd64 arm64; do
  n="cni-plugins-linux-${a}-${CNI_VERSION}.tgz"
  d="$(curl -fsSL "https://github.com/containernetworking/plugins/releases/download/${CNI_VERSION}/${n}.sha256" | awk '{ print $1 }')"
  listed "$n" "$d"
done

echo "Kernel ${KERNEL_VERSION}: kernel.org sha256sums.asc, checked against its signature"
major="${KERNEL_VERSION%%.*}"
curl -fsSL -o "${work}/sums.asc" "https://cdn.kernel.org/pub/linux/kernel/v${major}.x/sha256sums.asc"
gpg --quiet --auto-key-locate wkd --locate-keys autosigner@kernel.org >/dev/null 2>&1
if gpg --verify "${work}/sums.asc" >/dev/null 2>&1; then ok "signature of sha256sums.asc (kernel.org autosigner)"; else bad "signature of sha256sums.asc"; fi
d="$(awk -v n="linux-${KERNEL_VERSION}.tar.xz" '$2 == n { print $1 }' "${work}/sums.asc")"
listed "linux-${KERNEL_VERSION}.tar.xz" "$d"

echo "crun ${CRUN_VERSION}: detached signatures of the release assets"
gpg --quiet --keyserver hkps://keys.openpgp.org --recv-keys AF60FCA3CDAA6DEAD157EA3A67E38F7A8BA21772 >/dev/null 2>&1 || true
for a in amd64 arm64; do
  n="crun-${CRUN_VERSION}-linux-${a}-disable-systemd"
  curl -fsSL -o "${work}/${n}" "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/${n}"
  curl -fsSL -o "${work}/${n}.asc" "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/${n}.asc"
  if gpg --verify "${work}/${n}.asc" "${work}/${n}" 2>&1 | grep -q "Good signature"; then ok "signature of ${n}"; else bad "signature of ${n}"; fi
  listed "$n" "$(sha256sum "${work}/${n}" | awk '{ print $1 }')"
done

echo "erofs-utils ${EROFS_UTILS_VERSION}: the commit of the tag"
c="$(git ls-remote https://git.kernel.org/pub/scm/linux/kernel/git/xiang/erofs-utils.git "refs/tags/v${EROFS_UTILS_VERSION}^{}" | cut -f1)"
[ "$c" = "$EROFS_UTILS_COMMIT" ] && ok "EROFS_UTILS_COMMIT is the commit of v${EROFS_UTILS_VERSION}" || bad "EROFS_UTILS_COMMIT is ${EROFS_UTILS_COMMIT}, the tag points to ${c}"

echo
if [ "$fail" = 0 ]; then echo "All upstream checks passed."; else echo "Some checks FAILED."; exit 1; fi
