# Shared settings of the runtime comparison. Sourced by the other scripts. Everything the benchmark installs lives under RT_ROOT (and /opt/kata,
# /opt/cloudhv, which the Kata and cchv configuration files name by absolute path); nothing here touches the host's own Docker.
RT_ROOT=/opt/rtbench
RT_DATA=/var/lib/rtbench
RT_RUN=/run/rtbench
RT_SOCK=unix://$RT_RUN/docker.sock
RT_SUBNET=172.29.0.0/16          # the benchmark's own bridge: a range the host does not use (not Docker's 172.17/16)
RT_BRIDGE_IP=172.29.0.1/16
RT_DOCKER="sudo $RT_ROOT/docker/docker -H $RT_SOCK"

# Pinned versions (the checksums of what was downloaded are written to results/<date>/versions.txt by 10-install.sh)
DOCKER_VERSION=29.9.2
GVISOR_RELEASE=20261019.0
KATA_VERSION=4.3.1
CCHV_TAG=v0.3.1
POSTGRES_IMAGE=postgres:18.6-trixie
NGINX_IMAGE=nginx:1.29-alpine
WRK_IMAGE=alpine:3.24

# CPUs: the servers are limited by quota (--cpus); the load generators are pinned to the last two threads.
CLIENT_CPUS=6,7
