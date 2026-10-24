#!/bin/bash
# Stops what 20-start.sh started (the containers first).
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; . "$here/env.sh"
$RT_DOCKER ps -aq 2>/dev/null | xargs -r $RT_DOCKER rm -f >/dev/null 2>&1
sudo systemctl stop rtbench-dockerd 2>/dev/null; sudo systemctl stop rtbench-containerd 2>/dev/null; sudo systemctl stop rtbench-cchv-daemon 2>/dev/null
sudo umount /run/cloudhv/erofs-cache 2>/dev/null
sudo iptables -D DOCKER-USER -i rtbench0 -o rtbench0 -j ACCEPT 2>/dev/null
sudo ip link del rtbench0 2>/dev/null
echo stopped
