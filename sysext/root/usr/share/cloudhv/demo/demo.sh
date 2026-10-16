#!/bin/bash
#
# containerd-cloudhypervisor "echo" demo
#

function announce() {
  echo -e "\n  * $@\n"
}
# --

function get_cont_id() {
  crictl ps --all -o json \
     | jq -r '.containers[] | select( .metadata.name == "echo-server") | "\(.id)"'
}
# --

function get_pod_id() {
  crictl pods -o json \
     | jq -r '.items[] | select( .metadata.uid == "echo-pod-uid") | "\(.id)"'
}
# --

function cleanup() {
  set +e
  local cont_id="$(get_cont_id)"
  local pod_id="$(get_pod_id)"

  if [[ -n "${cont_id}" ]] ; then
    echo "Cleanup: Stopping and removing container '${cont_id}'"
    crictl stop "${cont_id}"
    crictl rm "${cont_id}"
  fi

  if [[ -n "${pod_id}" ]] ; then
    echo "Cleanup: Stopping and removing pod '${pod_id}'"
    crictl stopp "${pod_id}"
    crictl rmp "${pod_id}"
  fi
}
# --

function demo() {

  set -euo pipefail

  local scriptdir cfg_box cfg_cont
  scriptdir="$(cd "$(dirname "$0")"; pwd)"
  cfg_box="${scriptdir}/demo-sandbox.json"
  cfg_cont="${scriptdir}/demo-container.json"

  # used by sandbox config for logging
  mkdir -p /tmp/echo-pod-logs

  if [[ "${@}" == *"cleanup"* ]]; then
    trap cleanup exit
  fi

  # The daemon opens its socket only after it has made the base snapshot and filled its pool of VMs, which takes a while on a
  # slow (emulated) machine: wait for it instead of failing on the first start.
  announce "Waiting for the sandbox daemon (up to 5 minutes)"
  local waited
  for waited in $(seq 1 150); do
    [[ -S /run/cloudhv/daemon.sock ]] && break
    sleep 2
  done
  if [[ ! -S /run/cloudhv/daemon.sock ]]; then
    echo "The sandbox daemon has no socket after 5 minutes; its log:"
    journalctl -u cloudhv-sandbox-daemon --no-pager | tail -40
    return 1
  fi

  announce "Pulling 'echo' container image"
  crictl pull hashicorp/http-echo:latest

  announce "Creating Sandbox VM pod"
  crictl runp --runtime=cloudhv "$cfg_box"
  local pod_id pod_ip
  pod_id="$(get_pod_id)"
  pod_ip="$(crictl inspectp "$pod_id" | jq -r '.status.network.ip')"
  echo "Pod has IP address '$pod_ip'"

  announce "Creating and starting 'echo' container"
  crictl create "$pod_id" "$cfg_cont" "$cfg_box"
  local cont_id
  cont_id="$(get_cont_id)"
  crictl start "$cont_id"

  sleep 2
  announce "Accessing echo container HTTP endpoint"
  local attempt
  for attempt in 1 2 3 4 5; do
    if curl -sSL --connect-timeout 2 http://"$pod_ip":5678; then
      break
    fi
    echo "  curl attempt $attempt failed, retrying..."
    sleep 1
  done

  announce "Printing container logs"
  for f in /tmp/echo-pod-logs/*; do
    echo " ### $f ###"
    cat "$f"
  done

  announce "All done. Bye bye."
}
# --

if [[ "$0" != "-bash" ]] && [[ "$(basename "$0")" = "demo.sh" ]] ; then
  demo "${@}"
fi
