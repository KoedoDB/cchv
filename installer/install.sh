#!/usr/bin/env bash
set -euo pipefail

# CloudHV installer. Runs as the init container of the DaemonSet (privileged, host PID namespace, the host
# filesystem at /host) and exits when it is done; what stays on the pod afterwards is an unprivileged pause
# container. It copies the shim, daemon, Cloud Hypervisor and guest artifacts to the node, writes the configs and
# the daemon's unit, registers the runtime handler with containerd and starts the daemon.
#
# It is safe to run again: files are replaced atomically, containerd is restarted only when its config changed,
# and containerd's config is not touched (nor containerd restarted) unless everything it will point at is in place.

ARTIFACTS="${ARTIFACTS:-/opt/cloudhv}"
HOST="${HOST:-/host}"
# Overridable for tests (installer/smoke-test.sh).
CONTAINERD_CONFIG="${CONTAINERD_CONFIG:-$HOST/etc/containerd/config.toml}"

log() { echo "[cloudhv] $*"; }
die() { echo "[cloudhv] ERROR: $*" >&2; exit 1; }

# Run a command in the host's namespaces (mount, uts, ipc, pid). Not the network namespace: the pod is on the
# host network (hostNetwork), which is what the package managers below use.
host() { nsenter --target 1 --mount --uts --ipc --pid -- "$@"; }

HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    # The guest kernel of this repository has the virtio-console and no serial port: the console is hvc0 (the daemon reads it from a socket and logs it when serial_console is on).
    x86_64)  KERNEL_CONSOLE="console=hvc0" ;;
    aarch64) KERNEL_CONSOLE="console=ttyAMA0" ;;
    *) die "unsupported architecture: ${HOST_ARCH}" ;;
esac

# 0. Preflight: everything we are going to install is here, and the host is what we expect.
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon cloud-hypervisor vmlinux rootfs.erofs; do
  [ -s "$ARTIFACTS/$f" ] || die "artifact missing or empty: $ARTIFACTS/$f"
done
{ [ -d "$HOST/tmp" ] && [ -d "$HOST/etc/systemd/system" ] && [ -r "$HOST/etc/hostname" ]; } \
  || die "the host filesystem is not mounted at $HOST as expected"
[ -f "$CONTAINERD_CONFIG" ] \
  || die "$CONTAINERD_CONFIG not found: cannot register the runtime handler, so nothing is installed"

log "Installing on $(cat "$HOST/etc/hostname") (${HOST_ARCH})..."

# A private directory on the host for the scripts we run there (mktemp -d: created with a random name, mode 0700,
# so no one else on the node can plant a link or a file at a name we use).
WORK="$(mktemp -d "$HOST/tmp/cloudhv-install.XXXXXX")"
HOST_WORK="/tmp/$(basename "$WORK")"
NEW_CONFIG=""
cleanup() { rm -rf "$WORK"; if [ -n "$NEW_CONFIG" ]; then rm -f "$NEW_CONFIG"; fi; }
trap cleanup EXIT

cat > "$WORK/pkg.sh" << 'PKG'
#!/bin/sh
# usage: pkg.sh <apt packages> <tdnf/dnf packages>
set -e
if command -v tdnf >/dev/null 2>&1; then
  tdnf install -y $2
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y $2
elif command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $1
else
  echo "No supported package manager found" >&2
  exit 1
fi
PKG
chmod 700 "$WORK/pkg.sh"

# Install a host package for a command that is missing. Only what is needed, from the distribution's own
# (signed) repositories; the version is the distribution's.
need_command() { # command, apt packages, rpm packages
  if host sh -c "command -v $1" >/dev/null 2>&1; then
    return 0
  fi
  log "$1 not found, installing ($2)..."
  host sh "$HOST_WORK/pkg.sh" "$2" "$3" 2>&1 | tail -5 || die "$1 is required but could not be installed"
  host sh -c "command -v $1" >/dev/null 2>&1 || die "$1 is still missing after the install"
}

# 1. tc (traffic control) for the VM TAP networking; 2. mkfs.erofs and the erofs kernel module
need_command tc iproute2 iproute-tc
host modprobe erofs 2>/dev/null || true
need_command mkfs.erofs erofs-utils erofs-utils
# mke2fs makes the file system of an emptyDir's disk (on the file, never mounted on the host)
need_command mke2fs e2fsprogs e2fsprogs

# 3. Stop the daemon (if running) before clearing its state
host systemctl stop cloudhv-sandbox-daemon 2>/dev/null || true

# 4. Clear stale caches from previous installs
log "Clearing caches from previous install..."
host rm -rf /run/cloudhv/erofs-cache /run/cloudhv/daemon
host mkdir -p -m 0711 /run/cloudhv/daemon

# 5. Copy binaries and guest artifacts. Each is copied beside its final name, compared, then renamed over it (atomic:
# a running shim keeps the old file, the next one starts the new).
put() { # source, destination (under $HOST), mode
  install -D -m "$3" "$1" "$2.new"
  cmp -s "$1" "$2.new" || { rm -f "$2.new"; die "the copy of $1 to $2 is not identical"; }
  mv -f "$2.new" "$2"
}
log "Copying binaries and guest artifacts..."
put "$ARTIFACTS/containerd-shim-cloudhv-v1" "$HOST/usr/local/bin/containerd-shim-cloudhv-v1" 755
put "$ARTIFACTS/cloudhv-sandbox-daemon"     "$HOST/usr/local/bin/cloudhv-sandbox-daemon"     755
put "$ARTIFACTS/cloud-hypervisor"           "$HOST/usr/local/bin/cloud-hypervisor"           755
put "$ARTIFACTS/vmlinux"                    "$HOST/opt/cloudhv/vmlinux"                      644
put "$ARTIFACTS/rootfs.erofs"               "$HOST/opt/cloudhv/rootfs.erofs"                 644

# Everything the runtime handler and the unit point at exists and runs, seen from the host.
for f in /usr/local/bin/containerd-shim-cloudhv-v1 /usr/local/bin/cloudhv-sandbox-daemon /usr/local/bin/cloud-hypervisor; do
  host test -x "$f" || die "$f is not executable on the host"
done
host /usr/local/bin/cloud-hypervisor --version >/dev/null 2>&1 \
  || die "cloud-hypervisor does not run on this host (wrong architecture or a missing library?)"
for f in /opt/cloudhv/vmlinux /opt/cloudhv/rootfs.erofs; do
  host test -s "$f" || die "$f is missing on the host"
done

# 6. Shim runtime config (with daemon_socket)
log "Writing runtime config..."
cat > "$HOST/opt/cloudhv/config.json.new" << CONFIG
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "${KERNEL_CONSOLE} root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "max_default_vcpus": 0,
  "default_memory_mb": 320,
  "max_containers_per_vm": 5,
  "hotplug_memory_mb": 0,
  "hotplug_method": "acpi",
  "tpm_enabled": false,
  "daemon_socket": "/run/cloudhv/daemon.sock"
}
CONFIG
mv -f "$HOST/opt/cloudhv/config.json.new" "$HOST/opt/cloudhv/config.json"

# 7. Daemon config
log "Writing daemon config..."
cat > "$HOST/opt/cloudhv/daemon.json.new" << DAEMON
{
  "pool_size": 3,
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "${KERNEL_CONSOLE} root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "/run/cloudhv/daemon"
}
DAEMON
mv -f "$HOST/opt/cloudhv/daemon.json.new" "$HOST/opt/cloudhv/daemon.json"

# 8. systemd unit for the sandbox daemon. It runs as root (TAP devices, network namespaces, tc, and starting the
# Cloud Hypervisor processes), so it gets only the capabilities that needs; Cloud Hypervisor, its child, inherits
# the same limits. The notes on each directive are in sysext/.../cloudhv-sandbox-daemon.service.
log "Installing sandbox daemon service..."
cat > "$HOST/etc/systemd/system/cloudhv-sandbox-daemon.service.new" << 'UNIT'
[Unit]
Description=CloudHV Sandbox Daemon
After=containerd.service
Requires=containerd.service

[Service]
Type=simple
ExecStartPre=/bin/mkdir -p -m 0711 /run/cloudhv/daemon
ExecStart=/usr/local/bin/cloudhv-sandbox-daemon /opt/cloudhv/daemon.json
Restart=always
RestartSec=5
# Only the daemon is stopped, not the Cloud Hypervisors it started: the VMs handed to pods run on while the daemon is replaced, and the next one takes
# them over (keep_vms_on_stop in daemon.json). With the default (control-group) systemd would end every process of the unit, VMs included.
KillMode=process
Environment=RUST_LOG=info
MemoryMax=4G

NoNewPrivileges=yes
CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_SETUID CAP_SETGID CAP_KILL CAP_SYS_RESOURCE CAP_SYS_PTRACE CAP_IPC_LOCK
ProtectSystem=strict
ReadWritePaths=/run/cloudhv
ProtectHome=yes
PrivateTmp=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
LockPersonality=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
RestrictAddressFamilies=AF_UNIX AF_NETLINK AF_VSOCK AF_INET
SystemCallArchitectures=native
DevicePolicy=closed
DeviceAllow=/dev/kvm rw
DeviceAllow=/dev/net/tun rw
DeviceAllow=/dev/userfaultfd rw
RuntimeDirectory=cloudhv
RuntimeDirectoryMode=0711
RuntimeDirectoryPreserve=yes

[Install]
WantedBy=multi-user.target
UNIT
mv -f "$HOST/etc/systemd/system/cloudhv-sandbox-daemon.service.new" "$HOST/etc/systemd/system/cloudhv-sandbox-daemon.service"

# The volume helper (docs/sandbox-daemon.md, "Persistent block volumes"): LVM for volumes asked for by name. The unit is installed and NOT enabled: a host
# whose daemon.json has volume_block_vg enables it (systemctl enable --now cloudhv-volume-helper). The daemon never runs LVM itself.
cat > "$HOST/etc/systemd/system/cloudhv-volume-helper.service.new" << 'HELPERUNIT'
[Unit]
Description=CloudHV volume helper (LVM for the daemon's persistent block volumes)
Documentation=https://github.com/KoedoDB/cchv/blob/main/docs/sandbox-daemon.md
# Not enabled by default: it is for a host whose daemon.json has volume_block_vg (volumes by name). The daemon itself never runs LVM.
After=local-fs.target

[Service]
Type=simple
ExecStart=/usr/local/bin/cloudhv-sandbox-daemon --volume-helper /opt/cloudhv/daemon.json
Restart=always
RestartSec=5
Environment=RUST_LOG=info
MemoryMax=256M
TasksMax=64

# What it does: lvs, lvcreate, lvchange, lvextend and blkdiscard, on the volume group of daemon.json, for requests on a Unix socket (0600, root's) in /run/cloudhv.
# CAP_SYS_ADMIN: the device-mapper ioctls. CAP_MKNOD: a node for a volume that udev has not made (to zero it). Nothing else: no network, no cgroups, no processes of others.
NoNewPrivileges=yes
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_MKNOD
ProtectSystem=strict
# /etc/lvm: LVM keeps a backup and an archive of every change of the volume group's metadata there. /run/lvm and /run/lock/lvm: its locks and caches.
ReadWritePaths=/run/cloudhv /etc/lvm /run/lvm /run/lock/lvm
ProtectHome=yes
PrivateTmp=yes
ProtectKernelModules=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes
ProtectClock=yes
LockPersonality=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
# AF_NETLINK: libudev (lvm waits for udev to make the nodes).
RestrictAddressFamilies=AF_UNIX AF_NETLINK
SystemCallArchitectures=native
DevicePolicy=closed
DeviceAllow=/dev/mapper/control rw
DeviceAllow=block-device-mapper rwm
# The disks the volume group is made of (add the kind of yours if it is not here).
DeviceAllow=block-sd rwm
DeviceAllow=block-nvme rwm
DeviceAllow=block-virtblk rwm
DeviceAllow=block-md rwm
# /run/cloudhv is the daemon's too (the same settings as its unit): the socket is in it.
RuntimeDirectory=cloudhv
RuntimeDirectoryMode=0711
RuntimeDirectoryPreserve=yes

[Install]
WantedBy=multi-user.target
HELPERUNIT
mv -f "$HOST/etc/systemd/system/cloudhv-volume-helper.service.new" "$HOST/etc/systemd/system/cloudhv-volume-helper.service"

host systemctl daemon-reload
host systemctl enable cloudhv-sandbox-daemon

# 9. Register the runtime handler with containerd. Only now, when everything it points at is in place and checked.
log "Patching containerd config..."
ORIG_MODE="$(stat -c '%a' "$CONTAINERD_CONFIG" 2>/dev/null || echo 644)"
NEW_CONFIG="${CONTAINERD_CONFIG}.new"

# Remove an existing cloudhv runtime section (and the comment line we put above it) and trailing blank lines,
# so that running again gives the same file.
awk '
  /^# Cloud Hypervisor VM-isolated runtime$/ { next }
  /^\[.*\.runtimes\.cloudhv/ { skip=1; next }
  skip && /^\[/ && !/cloudhv/ { skip=0 }
  skip { next }
  /^[[:space:]]*$/ { blanks++; next }
  { while (blanks > 0) { print ""; blanks-- } print }
' "$CONTAINERD_CONFIG" > "$NEW_CONFIG"

# Add the cloudhv runtime handler (uses containerd's default snapshotter; the shim converts the rootfs to erofs itself)
{
  echo ""
  echo "# Cloud Hypervisor VM-isolated runtime"
  echo '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]'
  echo '  runtime_type = "io.containerd.cloudhv.v1"'
} >> "$NEW_CONFIG"

grep -q 'runtimes\.cloudhv' "$NEW_CONFIG" || die "the new containerd config has no cloudhv runtime; left the old one"
[ "$(wc -l < "$NEW_CONFIG")" -ge 3 ] || die "the new containerd config looks truncated; left the old one"

CONFIG_CHANGED=true
if cmp -s "$CONTAINERD_CONFIG" "$NEW_CONFIG"; then
  CONFIG_CHANGED=false
  rm -f "$NEW_CONFIG"
else
  cp "$CONTAINERD_CONFIG" "${CONTAINERD_CONFIG}.bak.$(date +%s)"
  chmod "$ORIG_MODE" "$NEW_CONFIG"
  mv -f "$NEW_CONFIG" "$CONTAINERD_CONFIG"
fi
NEW_CONFIG=""

# 10. Restart containerd (only if its config changed) and (re)start the daemon. This runs as a transient systemd unit,
# so that it survives this container, and the restart of containerd does not take it down. Without systemd-run it is
# left to a detached shell.
if [ "$CONFIG_CHANGED" = true ]; then
  log "containerd's config changed: scheduling its restart and the daemon's start..."
  RESTART='sleep 5; systemctl restart containerd; sleep 3; systemctl restart cloudhv-sandbox-daemon'
else
  log "containerd's config is unchanged: restarting only the daemon..."
  RESTART='sleep 1; systemctl restart cloudhv-sandbox-daemon'
fi
RESTART_UNIT="cloudhv-install-restart-$(date +%s)"
if host systemd-run --no-block --collect --unit="$RESTART_UNIT" sh -c "$RESTART" >/dev/null 2>&1; then
  # wait for the restart itself to be over, then check the result below
  for _ in $(seq 1 60); do
    host systemctl is-active --quiet "$RESTART_UNIT.service" || break
    sleep 1
  done
else
  host sh -c "nohup sh -c '$RESTART' >/dev/null 2>&1 &"
  sleep 15
fi

# Wait for them (up to 2 minutes); failing here shows in the pod (the init container is retried).
for _ in $(seq 1 60); do
  if host systemctl is-active --quiet containerd \
     && host systemctl is-active --quiet cloudhv-sandbox-daemon \
     && host test -S /run/cloudhv/daemon.sock; then
    log "containerd and the sandbox daemon are running"
    log "Installation complete on $(cat "$HOST/etc/hostname")"
    log "Daemon socket: /run/cloudhv/daemon.sock"
    exit 0
  fi
  sleep 2
done
die "containerd or the sandbox daemon is not running after 2 minutes (see: journalctl -u cloudhv-sandbox-daemon)"
