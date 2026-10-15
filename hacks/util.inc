#!/bin/sh
# vim: et ts=2 sw=2 syn=sh

# Pinned versions (container image digest, CH, CNI, kernel, ...) and checksum-checked downloads.
# The sourcing script sets $scriptdir to this directory.
. "${scriptdir}/versions.env"
. "${scriptdir}/verify-download.sh"

translate_arch() {
  local arch="${1:-$(uname -m)}"
  case "$arch" in
    x86_64)  echo "x86-64";;
    x86-64)  echo "x86-64";;
    aarch64) echo "arm64";;
    arm64)   echo "arm64";;
    *) echo "ERROR: unknown arch '$1'"; exit 1;;
  esac
}
# --

docker_arch() {
  local i=1
  local arch=""
  arch="$(translate_arch)"

  while [ "$#" -gt 0 ]; do
    if [ "${1}" = "--arch" ]; then
      shift
      arch="$(translate_arch "$1")"
    fi
    shift
  done

  echo "$arch"
}
# --

docker_platform() {
  local arch="${1:-$(translate_arch)}"

  case "$arch" in
    x86-64)  echo "--platform linux/x86-64";;
    arm64)   echo "--platform linux/arm64/v8";;
  esac
}
# --

docker_wrapper() {
  local uid gid scriptname platform arch git_mask
  uid="$(id -u)"
  gid="$(id -g)"
  scriptname="$(basename "$0")"

  arch="$(docker_arch "$@")"
  if [ "${1:-}" == "--arch" ] ; then
    shift; shift
  fi
  platform="$(docker_platform "$arch")"

  # The build runs code we did not write (every build.rs of every crate, the kernel's build system) with the checkout mounted
  # read-write, because the artifacts are written back into it. Keep the git directory (credentials in .git/config, hooks) out of
  # its reach: an empty tmpfs over it. A worktree's .git is a file: mask that with /dev/null.
  git_mask=""
  if [ -d .git ] ; then
    git_mask="--mount type=tmpfs,destination=/host/.git"
  elif [ -e .git ] ; then
    git_mask="-v /dev/null:/host/.git:ro"
  fi

  # The container is root inside (apk add and the chown of the artifacts need it) but gets only the capabilities those need,
  # cannot gain more (no-new-privileges), and runs the image pinned in versions.env. It keeps the network (crates, apk).
  # shellcheck disable=SC2086
  docker run --rm -i -v "$(pwd):/host" \
      $git_mask \
      --security-opt no-new-privileges \
      --cap-drop ALL \
      --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER --cap-add FSETID --cap-add SETUID --cap-add SETGID \
      $platform \
      "${ALPINE_IMAGE}" \
      "/host/hacks/${scriptname}" "build" "$uid:$gid" ${@}
}
# --

build_if_missing() {
  local host_user_group="$1"
  local build_command="$2"
  shift; shift

  local build_opts=""
  while [ "$1" != "--" ] ; do
    build_opts="$build_opts $1"
    shift
  done
  shift # "--"

  local f build="false"
  for f in ${@}; do
    if [ ! -f "/host/${f}" ] ; then
      echo -e "\n --- '${f}' not found, running '${build_command} ${build_opts}' to build it ---\n"
      $build_command build "$host_user_group" ${build_opts}
      build="true"
      break
    fi
  done

  if [ "$build" = "false" ] ; then
    echo -e "\n --- Skipping '${build_command}', all artifacts found: ${@} ---\n"
  fi
}
# --
