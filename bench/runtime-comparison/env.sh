# Shared settings of the runtime comparison. Sourced by the other scripts. Everything the benchmark installs lives under RT_ROOT (and /opt/kata,
# /opt/cloudhv, which the Kata and cchv configuration files name by absolute path); nothing here touches the host's own Docker.
RT_ROOT=/opt/rtbench
export RT_DATA="${RT_DATA:-/var/lib/rtbench}"   # large: Docker's data root, the images, the erofs cache (up to 20 GiB); on a host with a small / put it on a big disk (stg-db-001: /data/rtbench)
RT_RUN=/run/rtbench
RT_SOCK=unix://$RT_RUN/docker.sock
RT_SUBNET=172.29.0.0/16          # the benchmark's own bridge: a range the host does not use (not Docker's 172.17/16)
RT_BRIDGE_IP=172.29.0.1/16
RT_DOCKER="sudo $RT_ROOT/docker/docker -H $RT_SOCK"

# Pinned versions (the checksums of what was downloaded are written to results/<date>/versions.txt by 10-install.sh)
DOCKER_VERSION=29.9.2
GVISOR_RELEASE=20261019.0
KATA_VERSION=4.3.1
CCHV_TAG=v0.4.3
POSTGRES_IMAGE=postgres:18.6-trixie
NGINX_IMAGE=nginx:1.29-alpine
WRK_IMAGE=alpine:3.24

# CPUs: the servers are limited by quota (--cpus); the load generators are pinned to the last two threads.
CLIENT_CPUS=6,7

# Where cchv's volumes and files live. The defaults are the benchmark host (a build host: a loop-device group `rtvg`, /opt/cloudhv, /run/cloudhv).
# On a host that already runs cchv for real (stg-db-001: the group `vg`, its own cloudhv-sandbox-daemon) set RT_VG=vg RT_LVP=rtbench- RT_CONF=/opt/cloudhv-rtbench RT_CRUN=/run/cloudhv-rtbench:
# the logical volumes are named rtbench-* in the real group, and the benchmark's containerd and cchv daemon see RT_CONF as /opt/cloudhv and RT_CRUN as /run/cloudhv (systemd BindPaths=),
# so nothing of the host's own cchv is touched. The shim's paths (/opt/cloudhv/config.json, /run/cloudhv/erofs-cache) are constants in its source, hence the bind mounts.
export RT_VG="${RT_VG:-rtvg}" RT_LVP="${RT_LVP:-}" RT_CONF="${RT_CONF:-/opt/cloudhv}" RT_CRUN="${RT_CRUN:-/run/cloudhv}"
RT_BINDS=""
if [ "$RT_CONF" != /opt/cloudhv ]; then
  RT_BINDS="-p BindPaths=$RT_CONF:/opt/cloudhv -p BindPaths=$RT_CRUN:/run/cloudhv -p BindPaths=$RT_DATA/erofs-cache:/run/cloudhv/erofs-cache"
fi
export RT_BINDS
# What run-all.sh runs: the runc-loop control only makes sense on a loop-device group; the density counts must fit the volume group.
RT_LOOP_CONTROL="${RT_LOOP_CONTROL:-1}"; RT_DENSITY_NS="${RT_DENSITY_NS:-1 8 16}"
