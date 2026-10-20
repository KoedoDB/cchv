#!/usr/bin/env bash
# Runs inside the container of installer/smoke-test.sh.
set -euo pipefail

apt-get update -qq >/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends netcat-openbsd >/dev/null

PASS=0; FAIL=0
ok()  { echo "  ✅ $1"; PASS=$((PASS + 1)); }
bad() { echo "  ❌ $1"; FAIL=$((FAIL + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# --- the "node" and the stubs ---
STUBS=/stubs; mkdir -p "$STUBS"
cat > "$STUBS/nsenter" << 'S'
#!/bin/sh
while [ "$1" != "--" ]; do shift; done; shift
exec "$@"
S
cat > "$STUBS/systemctl" << 'S'
#!/bin/sh
echo "systemctl $*" >> /calls
case "$*" in
  *cloudhv-install-restart*) exit 3 ;;   # the transient unit is over
  *) exit 0 ;;
esac
S
cat > "$STUBS/systemd-run" << 'S'
#!/bin/sh
echo "systemd-run $*" >> /calls
exit 0
S
for c in modprobe tc mkfs.erofs mke2fs; do printf '#!/bin/sh\nexit 0\n' > "$STUBS/$c"; done
chmod +x "$STUBS"/*
export PATH="$STUBS:$PATH"

mkdir -p /run/cloudhv /etc/systemd/system /etc/containerd /usr/local/bin /opt/cloudhv /artifacts
nc -lU /run/cloudhv/daemon.sock >/dev/null 2>&1 &   # the daemon's socket, for the last check of the installer
sleep 1
cat > /etc/containerd/config.toml << 'T'
version = 3
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
T
for f in containerd-shim-cloudhv-v1 cloudhv-sandbox-daemon vmlinux rootfs.erofs; do echo "$f" > "/artifacts/$f"; done
printf '#!/bin/sh\necho cloud-hypervisor 54.0\n' > /artifacts/cloud-hypervisor

run_install() { HOST=/ ARTIFACTS=/artifacts bash /installer/install.sh; }

echo "=== Run 1: a fresh node ==="
: > /calls
run_install > /tmp/run1.log 2>&1 && ok "install succeeds" || { bad "install fails"; tail -20 /tmp/run1.log; }
check "shim installed, executable" '[ -x /usr/local/bin/containerd-shim-cloudhv-v1 ]'
check "daemon installed, executable" '[ -x /usr/local/bin/cloudhv-sandbox-daemon ]'
check "kernel is 0644" '[ "$(stat -c %a /opt/cloudhv/vmlinux)" = 644 ]'
check "no .new files left" '[ -z "$(find /usr/local/bin /opt/cloudhv /etc/systemd/system /etc/containerd -name "*.new")" ]'
check "no private work directory left in /tmp" '[ -z "$(ls -d /tmp/cloudhv-install.* 2>/dev/null)" ]'
check "runtime handler registered once" '[ "$(grep -c "runtimes.cloudhv" /etc/containerd/config.toml)" = 1 ]'
check "runc handler kept" 'grep -q "runtimes.runc" /etc/containerd/config.toml'
check "containerd's config backed up" 'ls /etc/containerd/config.toml.bak.* >/dev/null 2>&1'
check "unit has the hardening" 'grep -q "^CapabilityBoundingSet=" /etc/systemd/system/cloudhv-sandbox-daemon.service && grep -q "^NoNewPrivileges=yes" /etc/systemd/system/cloudhv-sandbox-daemon.service'
check "console argument filled in" 'grep -Eq "console=(hvc0|ttyAMA0)" /opt/cloudhv/daemon.json && ! grep -q "KERNEL_CONSOLE" /opt/cloudhv/daemon.json'
check "containerd restart scheduled (config changed)" 'grep "^systemd-run" /calls | grep -q "restart containerd"'
check "daemon stopped before the copy" 'grep -q "^systemctl stop cloudhv-sandbox-daemon" /calls'

echo "=== Run 2: again, nothing changed ==="
cp /etc/containerd/config.toml /tmp/config.after1
: > /calls
run_install > /tmp/run2.log 2>&1 && ok "install succeeds again" || { bad "second install fails"; tail -20 /tmp/run2.log; }
check "containerd's config identical" 'cmp -s /etc/containerd/config.toml /tmp/config.after1'
check "containerd is NOT restarted" '! grep "^systemd-run" /calls | grep -q "restart containerd"'
check "the daemon is restarted" 'grep "^systemd-run" /calls | grep -q "restart cloudhv-sandbox-daemon"'

echo "=== Run 3: an artifact is missing ==="
cp /etc/containerd/config.toml /tmp/config.before3
mv /artifacts/vmlinux /artifacts/vmlinux.gone
if run_install > /tmp/run3.log 2>&1; then bad "install should fail"; else ok "install fails"; fi
grep -q "artifact missing" /tmp/run3.log && ok "says which artifact" || bad "no message about the artifact"
check "containerd's config untouched" 'cmp -s /etc/containerd/config.toml /tmp/config.before3'
mv /artifacts/vmlinux.gone /artifacts/vmlinux

echo "=== Run 4: cloud-hypervisor does not run on the host ==="
printf '#!/bin/sh\nexit 1\n' > /artifacts/cloud-hypervisor
# make the config differ, to see that a failed check leaves it alone
cat > /etc/containerd/config.toml << 'T'
version = 3
T
cp /etc/containerd/config.toml /tmp/config.before4
: > /calls
if run_install > /tmp/run4.log 2>&1; then bad "install should fail"; else ok "install fails"; fi
grep -q "does not run on this host" /tmp/run4.log && ok "says why" || bad "no message about cloud-hypervisor"
check "containerd's config untouched" 'cmp -s /etc/containerd/config.toml /tmp/config.before4'
check "containerd not restarted" '! grep -q "restart containerd" /calls'

echo
echo "Passed: $PASS  Failed: $FAIL"
[ "$FAIL" = 0 ]
