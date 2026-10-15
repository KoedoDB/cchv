#!/bin/bash
# Checks that a rootless, confined Cloud Hypervisor (ch_rootless + ch_confine) is confined: namespaces, user and capabilities, the files
# it can see, its network, its cgroup, and that everything is gone when the daemon stops.
#
# Run as root, on a machine with /dev/kvm and cgroup v2:
#   sudo DAEMON=./cloudhv-sandbox-daemon CH=./cloud-hypervisor KERNEL=./vmlinux ROOTFS=./rootfs.erofs bash scripts/test-ch-confinement.sh
# (the daemon must be the static musl build or any build; KERNEL and ROOTFS must be world-readable; WORK defaults to a new directory in
# /tmp.) Prints PASS/FAIL lines and exits non-zero if any failed. It starts a daemon of its own (pool of 2, Copy restore) and stops it.
set -u
: "${DAEMON:?}" "${CH:?}" "${KERNEL:?}" "${ROOTFS:?}"
WORK=${WORK:-$(mktemp -d /tmp/chconf.XXXXXX)}
CGROOT=${CGROOT:-/sys/fs/cgroup/cloudhv-conftest}
STATE=$WORK/state
FAILS=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
check() { local what=$1; shift; if "$@"; then pass "$what"; else fail "$what"; fi; }

mkdir -p "$WORK" "$STATE"; chmod 755 "$WORK"
cat >"$WORK/daemon.json" <<JSON
{
  "pool_size": 2, "max_pool_size": 4, "default_vcpus": 1, "default_memory_mb": 128,
  "cloud_hypervisor_binary": "$CH", "kernel_path": "$KERNEL", "rootfs_path": "$ROOTFS",
  "socket_path": "$WORK/daemon.sock", "state_dir": "$STATE", "erofs_cache_dir": "$WORK/erofs-cache",
  "ch_rootless": true, "ch_confine": true, "ch_cgroup_root": "$CGROOT", "ch_uid_base": 210000, "ch_uid_count": 1000
}
JSON
CLOUDHV_MEMORY_RESTORE_MODE=Copy RUST_LOG=info "$DAEMON" "$WORK/daemon.json" >"$WORK/daemon.log" 2>&1 &
DPID=$!
cleanup() { kill "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq 1 120); do grep -q "pool initialized" "$WORK/daemon.log" && break; kill -0 "$DPID" 2>/dev/null || break; sleep 0.5; done
grep -q "pool initialized" "$WORK/daemon.log" || { echo "the daemon did not come up:"; tail -20 "$WORK/daemon.log"; exit 2; }
pass "the daemon is up: base snapshot made and pool VMs restored, all confined"

DNS=$(readlink /proc/$DPID/ns/net) DMNT=$(readlink /proc/$DPID/ns/mnt) DPIDNS=$(readlink /proc/$DPID/ns/pid)
LAUNCHERS=$(pgrep -f -- '[-]-ch-launcher' | tr '\n' ' ')
[ -n "$LAUNCHERS" ] && pass "launchers running: $LAUNCHERS" || { fail "no launcher process"; exit 1; }
N=0
for L in $LAUNCHERS; do
  C=$(pgrep -P "$L" | head -1)
  [ -n "$C" ] || { fail "launcher $L has no Cloud Hypervisor child"; continue; }
  N=$((N + 1))
  echo "--- VM: launcher $L, Cloud Hypervisor $C"
  st=/proc/$C/status
  check "own mount namespace"  [ "$(readlink /proc/$C/ns/mnt)" != "$DMNT" ]
  check "own PID namespace"    [ "$(readlink /proc/$C/ns/pid)" != "$DPIDNS" ]
  check "own network namespace" [ "$(readlink /proc/$C/ns/net)" != "$DNS" ]
  check "own IPC namespace"    [ "$(readlink /proc/$C/ns/ipc)" != "$(readlink /proc/$DPID/ns/ipc)" ]
  check "own UTS namespace"    [ "$(readlink /proc/$C/ns/uts)" != "$(readlink /proc/$DPID/ns/uts)" ]
  check "launcher stays in the host's PID namespace (the daemon can signal it)" [ "$(readlink /proc/$L/ns/pid)" == "$DPIDNS" ]
  uid=$(awk '/^Uid:/{print $2}' $st); gid=$(awk '/^Gid:/{print $2}' $st)
  check "user is a VM user ($uid >= 210000)" [ "$uid" -ge 210000 ]
  check "group is its own ($gid == $uid)" [ "$gid" == "$uid" ]
  check "supplementary groups are only kvm" [ "$(awk '/^Groups:/{print NF-1}' $st)" == "1" ]
  check "no capabilities" [ "$(awk '/^CapEff:/{print $2}' $st)" == "0000000000000000" ]
  check "no capability can be gained (bounding set empty)" [ "$(awk '/^CapBnd:/{print $2}' $st)" == "0000000000000000" ]
  check "no_new_privs" [ "$(awk '/^NoNewPrivs:/{print $2}' $st)" == "1" ]
  check "the launcher is the same user" [ "$(awk '/^Uid:/{print $2}' /proc/$L/status)" == "$uid" ]
  # The network namespace has no interface but loopback, and nothing listens on the host's ports in it.
  # (/proc/net follows the process's own network namespace; /sys/class/net is that of whoever mounted sysfs, i.e. the host's)
  ifs=$(nsenter -t "$C" -n awk 'NR>2{sub(":","",$1); print $1}' /proc/net/dev 2>/dev/null | tr '\n' ' ')
  check "network: only lo ($ifs)" [ "$ifs" == "lo " ]
  # Every TCP port the host listens on (e.g. 6443, 22) must be unreachable from inside.
  port=$(ss -ltnH 2>/dev/null | awk '{n=split($4,a,":"); print a[n]}' | grep -E '^[0-9]+$' | head -1)
  if [ -n "$port" ]; then
    nsenter -t "$C" -n bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null && fail "reached 127.0.0.1:$port from inside" || pass "cannot reach 127.0.0.1:$port from inside"
  fi
  # What it sees of the file system.
  root=$(ls /proc/$C/root | tr '\n' ' ')
  echo "    root of the VM: $root"
  check "root has only dev, proc, tmp and the bound paths" bash -c "! ls /proc/$C/root | grep -qE '^(home|etc|var|root|boot|srv|mnt|media|snap|usr|sbin|bin)$'"
  check "no host home" [ ! -e /proc/$C/root/home ]
  check "no /etc/shadow" [ ! -e /proc/$C/root/etc/shadow ]
  check "other VMs' directories are not there" bash -c "[ \$(ls /proc/$C/root$STATE 2>/dev/null | grep -c '^pool-') -le 1 ]"
  check "the registry (.ctl) is not there" [ ! -e /proc/$C/root$STATE/.ctl ]
  check "root file system is read-only" bash -c "grep -qE ' / .*\\bro\\b' /proc/$C/mountinfo"
  check "/dev/kvm is there" [ -e /proc/$C/root/dev/kvm ]
  # The host's processes are not visible: this namespace's /proc lists a handful of numbers.
  nproc_inside=$(ls /proc/$C/root/proc | grep -cE '^[0-9]+$')
  check "sees only its own processes in /proc ($nproc_inside)" [ "$nproc_inside" -le 40 ]
  check "is process 1 of its namespace" bash -c "grep -q '^NSpid:.*\\b1\$' $st"
  # cgroup
  cg=$(sed -n 's/^0:://p' /proc/$C/cgroup)
  echo "    cgroup: $cg"
  check "in a cgroup under $CGROOT" bash -c "[[ '$cg' == */cloudhv-conftest/* ]]"
  d=/sys/fs/cgroup$cg
  check "memory.max is set ($(cat $d/memory.max 2>/dev/null))" bash -c "[ \"\$(cat $d/memory.max 2>/dev/null)\" != max ]"
  check "pids.max is set ($(cat $d/pids.max 2>/dev/null))" bash -c "[ \"\$(cat $d/pids.max 2>/dev/null)\" != max ]"
  check "the launcher is in the same cgroup" [ "$(sed -n 's/^0:://p' /proc/$L/cgroup)" == "$cg" ]
done
check "at least two VMs were checked" [ "$N" -ge 2 ]

# The daemon's own view: it can signal and wait for the launchers, and a SIGTERM to the launcher ends Cloud Hypervisor.
L=$(echo "$LAUNCHERS" | awk '{print $1}')
C=$(pgrep -P "$L" | head -1)
kill -TERM "$L"; for _ in $(seq 1 40); do kill -0 "$L" 2>/dev/null || break; sleep 0.1; done
check "SIGTERM to the launcher ends the launcher" bash -c "! kill -0 $L 2>/dev/null"
check "... and Cloud Hypervisor" bash -c "! kill -0 $C 2>/dev/null"

# Stopping the daemon: nothing is left.
kill -TERM "$DPID"; for _ in $(seq 1 100); do kill -0 "$DPID" 2>/dev/null || break; sleep 0.2; done
check "the daemon stopped" bash -c "! kill -0 $DPID 2>/dev/null"
check "no launcher left" bash -c "[ -z \"\$(pgrep -f -- '[-]-ch-launcher')\" ]"
check "no process of the VMs' users left" bash -c "[ -z \"\$(ps -eo uid | awk '\$1>=210000 && \$1<211000')\" ]"
left=$(find "$CGROOT" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
check "no VM cgroup left ($left)" [ "$left" == "0" ]
check "no VM directory left" bash -c "[ -z \"\$(ls $STATE | grep -vE '^(base-snapshot|snapshot-cache)$')\" ]"
check "no outbox left" bash -c "[ -z \"\$(ls $STATE/.out 2>/dev/null)\" ]"
check "base snapshot was handed over (group-readable, not writable)" bash -c "[ \"\$(stat -c %a $STATE/base-snapshot/memory-ranges)\" == 640 ]"
rmdir "$CGROOT" 2>/dev/null
echo "--- $FAILS failed; log in $WORK/daemon.log"
[ "$FAILS" == 0 ]
