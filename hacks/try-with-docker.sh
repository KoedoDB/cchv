#!/bin/sh
# Try the cloudhv runtime with Docker, in a throwaway container: nothing on the host is changed, and nothing but Docker (with /dev/kvm)
# is needed on it. See docs/try-with-docker.md first: this is an experiment.
#
#   hacks/try-with-docker.sh [--artifacts DIR] [--tag TAG] [--name NAME] [--feature-test]
#   hacks/try-with-docker.sh --clean [--name NAME]
#
# It starts a docker:dind container (privileged, /dev/kvm) in which Docker runs with the containerd image store and a runtime named
# `cloudhv`, installs the shim, the sandbox daemon, Cloud Hypervisor, a guest kernel and the guest root file system there, starts the daemon,
# waits until its pool of VMs is ready, and runs one VM container as a check. Run it again to restart the daemon with a fresh state.
#
#   --artifacts DIR   a directory with your own build of: containerd-shim-cloudhv-v1, cloudhv-sandbox-daemon (static x86_64 binaries),
#                     and optionally cloud-hypervisor (a static v54.0 binary), vmlinux, rootfs.erofs. Anything missing is downloaded
#                     (see --tag). Docker support is in the shim and the daemon of a build from after 2026-10-16: the binaries of the
#                     release v0.15.0-koedodb.1 do not have it, so build the two yourself (docs/try-with-docker.md says how).
#   --tag TAG         the release of KoedoDB/cchv to take missing files from (default: v0.15.0-koedodb.1)
#   --name NAME       the name of the container (default: cchv-docker)
#   --feature-test    also run hacks/docker-feature-test.sh in it and print its output
#   --clean           remove the container and stop
#
# Tested with Docker 29 on Linux x86_64 with KVM. Needs Internet access (images, and the downloads above).

set -eu

NAME="cchv-docker"
ARTIFACTS=""
TAG="v0.15.0-koedodb.1"
FEATURE_TEST=0
CLEAN=0
CH_VERSION="v54.0"
# sha256 of cloud-hypervisor-static of v54.0 (the same as hacks/checksums.txt)
CH_SHA256="4f4f98a3d661a40f3e5272627cc895a1b2e525f2f6bedf5f895f9cd21ba5c65b"
REPO="KoedoDB/cchv"

while [ $# -gt 0 ]; do
  case "$1" in
    --artifacts) ARTIFACTS="$2"; shift 2 ;;
    --tag) TAG="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --feature-test) FEATURE_TEST=1; shift ;;
    --clean) CLEAN=1; shift ;;
    -h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is not installed (or not in PATH)" >&2; exit 1
fi

if [ "$CLEAN" = 1 ]; then
  docker rm -f "$NAME" >/dev/null 2>&1 && echo "removed $NAME" || echo "no container named $NAME"
  exit 0
fi

if [ ! -e /dev/kvm ]; then
  echo "/dev/kvm is not there: this needs a machine with KVM (and, in a VM, nested virtualization)" >&2; exit 1
fi
if [ "$(uname -m)" != "x86_64" ]; then
  echo "this recipe is for x86_64 (the release also has arm64 files; adapt the names)" >&2; exit 1
fi

here="$(cd "$(dirname "$0")" && pwd)"

say() { echo "==> $*"; }
inside() { docker exec "$NAME" sh -c "$1"; }

# ── the container ───────────────────────────────────────────────────────────

# The configuration of the Docker inside is written by the entrypoint of the container before Docker starts: no file is made on the host
# (and nothing has to be readable by a Docker that is confined, like the snap's).
DOCKERD_JSON='{"features":{"containerd-snapshotter":true},"runtimes":{"cloudhv":{"runtimeType":"io.containerd.cloudhv.v1"}}}'

if ! docker inspect "$NAME" >/dev/null 2>&1; then
  say "creating the container $NAME (docker:dind, privileged, /dev/kvm)"
  docker create --name "$NAME" --privileged --device /dev/kvm -e DOCKER_TLS_CERTDIR= -e DOCKERD_JSON="$DOCKERD_JSON" \
    --entrypoint sh docker:dind -c 'mkdir -p /etc/docker && printf "%s" "$DOCKERD_JSON" > /etc/docker/daemon.json && exec dockerd-entrypoint.sh' >/dev/null
fi
docker start "$NAME" >/dev/null

say "waiting for Docker inside"
i=0
until docker exec "$NAME" docker info >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" != "true" ] || [ "$i" -gt 60 ]; then
    echo "the Docker inside did not start; the end of its log:" >&2
    docker logs --tail 8 "$NAME" >&2 || true
    exit 1
  fi
  sleep 1
done

# ── the files ───────────────────────────────────────────────────────────────

inside 'mkdir -p /opt/cloudhv /run/cloudhv'

# Copy a file of the host into the container through a tar stream on standard input: the Docker CLI then does not read the file itself,
# which a confined Docker (the snap's) is not allowed to do outside the user's home.
copy_in() { # local-file dest-in-container
  src_dir="$(dirname "$1")"; src_name="$(basename "$1")"
  inside 'mkdir -p /opt/cloudhv-in'
  tar -C "$src_dir" -cf - "$src_name" | docker cp - "$NAME:/opt/cloudhv-in/"
  inside "mv -f '/opt/cloudhv-in/$src_name' '$2'"
}

# put FILE (a name in --artifacts, or a release asset) at DEST in the container
install_file() { # name-in-artifacts release-asset dest mode
  if [ -n "$ARTIFACTS" ] && [ -f "$ARTIFACTS/$1" ]; then
    say "using $ARTIFACTS/$1"
    copy_in "$ARTIFACTS/$1" "$3.part"
  else
    say "downloading $2 of $TAG"
    inside "wget -q -O '$3.part' 'https://github.com/$REPO/releases/download/$TAG/$2'" \
      || { echo "cannot download $2 of $TAG: give your own with --artifacts DIR (file name: $1)" >&2; exit 1; }
  fi
  inside "chmod $4 '$3.part' && mv -f '$3.part' '$3'"
}

# A running binary cannot be overwritten (but can be replaced): stop the daemon first.
inside 'for p in $(ps -o pid,args | grep "[s]andbox-daemon /opt" | awk "{print \$1}"); do kill $p; done; sleep 2' || true

install_file containerd-shim-cloudhv-v1 containerd-shim-cloudhv-v1-linux-amd64 /usr/local/bin/containerd-shim-cloudhv-v1 755
install_file cloudhv-sandbox-daemon cloudhv-sandbox-daemon-linux-amd64 /usr/local/bin/cloudhv-sandbox-daemon 755
install_file vmlinux vmlinux-amd64 /opt/cloudhv/vmlinux 644
install_file rootfs.erofs rootfs-amd64.erofs /opt/cloudhv/rootfs.erofs 644

if [ -n "$ARTIFACTS" ] && [ -f "$ARTIFACTS/cloud-hypervisor" ]; then
  say "using $ARTIFACTS/cloud-hypervisor"
  copy_in "$ARTIFACTS/cloud-hypervisor" /usr/local/bin/cloud-hypervisor.part
else
  say "downloading Cloud Hypervisor $CH_VERSION (static)"
  inside "wget -q -O /usr/local/bin/cloud-hypervisor.part https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH_VERSION/cloud-hypervisor-static"
  inside "echo '$CH_SHA256  /usr/local/bin/cloud-hypervisor.part' | sha256sum -c - >/dev/null" \
    || { echo "the checksum of the Cloud Hypervisor download is wrong" >&2; exit 1; }
fi
inside 'chmod 755 /usr/local/bin/cloud-hypervisor.part && mv -f /usr/local/bin/cloud-hypervisor.part /usr/local/bin/cloud-hypervisor'

# mkfs.erofs builds the image of a container's root file system; Alpine has it.
say "installing erofs-utils (mkfs.erofs) in the container"
inside 'apk add --no-cache erofs-utils >/dev/null'

# ── the configuration ───────────────────────────────────────────────────────

state="/run/cloudhv/state-$(date +%s)"
docker exec -i "$NAME" sh -c 'cat > /opt/cloudhv/daemon.json' <<EOF
{
  "pool_size": 3,
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "$state",
  "ch_rootless": true,
  "allow_proc_netns": true
}
EOF
# volume_source_roots: where the shim may read bind mounts from. Docker keeps /etc/hosts, /etc/resolv.conf and /etc/hostname of a
# container under /var/lib/docker/containers, and named volumes under /var/lib/docker/volumes. A bind mount of any other host
# directory (-v /some/dir:/data) is refused until its parent is listed here.
docker exec -i "$NAME" sh -c 'cat > /opt/cloudhv/config.json' <<'EOF'
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "daemon_socket": "/run/cloudhv/daemon.sock",
  "mkfs_erofs_path": "/usr/bin/mkfs.erofs",
  "require_pod_cgroup": false,
  "volume_source_roots": ["/var/lib/docker/containers", "/var/lib/docker/volumes"]
}
EOF
inside "mkdir -p '$state'"

# ── the daemon ──────────────────────────────────────────────────────────────

say "starting the sandbox daemon (state in $state)"
# Copy restore mode: the userfaultfd mode needs vm.unprivileged_userfaultfd=1 on the host for a Cloud Hypervisor that is not root.
docker exec -d "$NAME" sh -c 'CLOUDHV_MEMORY_RESTORE_MODE=Copy RUST_LOG=info cloudhv-sandbox-daemon /opt/cloudhv/daemon.json > /var/log/cloudhv-daemon.log 2>&1'

i=0
until inside 'grep -q "pool initialized" /var/log/cloudhv-daemon.log 2>/dev/null'; do
  i=$((i + 1))
  if [ "$i" -gt 120 ]; then
    echo "the daemon's pool did not come up; its log:" >&2
    inside 'tail -20 /var/log/cloudhv-daemon.log' >&2 || true
    exit 1
  fi
  sleep 1
done
say "the daemon is ready ($(inside 'grep "pool initialized" /var/log/cloudhv-daemon.log | tail -1 | sed "s/.*pool initialized: //"'))"

# ── a check ─────────────────────────────────────────────────────────────────

say "checking: docker run --runtime=cloudhv alpine (the first run pulls the image)"
if out="$(inside 'timeout 240 docker run --rm --runtime=cloudhv alpine:3.19 sh -c "echo hello from \$(hostname); uname -r" 2>&1')"; then
  echo "$out" | tail -2 | sed 's/^/    /'
  echo "    (the kernel is the guest's: the container runs in a VM)"
else
  echo "the check failed:" >&2
  echo "$out" | tail -5 >&2
  echo "the daemon's log: docker exec $NAME tail -30 /var/log/cloudhv-daemon.log" >&2
  echo "(if the shim and daemon are the ones of the release $TAG, they predate Docker support: build your own, see docs/try-with-docker.md)" >&2
  exit 1
fi

if [ "$FEATURE_TEST" = 1 ]; then
  say "running hacks/docker-feature-test.sh"
  copy_in "$here/docker-feature-test.sh" /opt/docker-feature-test.sh
  docker exec "$NAME" sh -c 'chmod +x /opt/docker-feature-test.sh; /opt/docker-feature-test.sh' 2>&1 | grep -v "Pulling\|Download\|Pull complete\|Digest\|Status:\|Unable to find"
fi

cat <<EOF

Done. Try it (the commands run Docker inside the container $NAME):

  docker exec $NAME docker run --rm --runtime=cloudhv alpine:3.19 uname -a
  docker exec -it $NAME docker run --rm -it --runtime=cloudhv alpine:3.19 sh
  docker exec $NAME docker run -d --name web --runtime=cloudhv -p 8080:80 -m 512m nginx:1.27-alpine

  daemon log:     docker exec $NAME tail -f /var/log/cloudhv-daemon.log
  shim and VMs:   docker logs $NAME           (the inner containerd's messages, including the shim's)
  feature tour:   hacks/try-with-docker.sh --feature-test
  remove it all:  hacks/try-with-docker.sh --clean
EOF
