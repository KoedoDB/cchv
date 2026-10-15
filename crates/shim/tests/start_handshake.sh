#!/bin/sh
# Regression test of containerd's `start` handshake with the real shim binary (containerd-shim 0.9+ protocol).
#
# `start` spawns the server with its stdout a pipe and reads the pipe until it ends; the server closes its end when it is ready. Any
# other process that holds the pipe (the zygote, once) keeps it from ending, and `start` hangs until containerd kills it
# ("start failed: signal: killed"). This runs `start` the way containerd does, in a root container that has the directories containerd
# provides, and checks that it returns, prints the socket address, and that no process but the server is left holding its stdout.
#
# usage: crates/shim/tests/start_handshake.sh [path to containerd-shim-cloudhv-v1]   (needs docker; the directory is made under $HOME
# because a snap docker cannot mount /tmp)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-$here/../../../containerd-shim-cloudhv-v1}
dir=$HOME/.cache/cchv-start-handshake
mkdir -p "$dir"
cp "$bin" "$dir/shim"
cat > "$dir/inner.sh" <<'INNER'
#!/bin/sh
set -u
apk add --no-cache python3 procps >/dev/null 2>&1
mkdir -p /run/containerd/s /b/rootfs && cd /b
python3 -c '
import json
json.dump({"ociVersion":"1.0.2","process":{"terminal":False,"user":{"uid":0,"gid":0},"args":["x"],"cwd":"/"},"root":{"path":"rootfs"},"hostname":"t","linux":{},"annotations":{"io.kubernetes.cri.sandbox-id":"test"}},open("config.json","w"))'
mkfifo log; exec 9<>log      # containerd reads the shim's log FIFO
python3 -c 'import socket,time; s=socket.socket(socket.AF_UNIX); s.bind("/t.sock"); s.listen(5); time.sleep(60)' &
sleep 1
export TTRPC_ADDRESS=/t.sock
start=$(date +%s)
out=$(timeout 20 /w/shim -namespace k8s.io -id test -address /c.sock start 2>/dev/null) && rc=0 || rc=$?
took=$(( $(date +%s) - start ))
fail=0
[ "$rc" = 0 ] || { echo "FAIL: start exited $rc (124 = it hung until killed)"; fail=1; }
[ "$took" -le 5 ] || { echo "FAIL: start took $took s"; fail=1; }
case "$out" in unix:///run/containerd/s/*) ;; *) echo "FAIL: start printed '$out'"; fail=1;; esac
# every shim process of the server must have its stdout on /dev/null (none on a pipe)
for p in $(ps -eo pid,args | grep '[/]w/shim' | awk '{print $1}'); do
  fd1=$(readlink /proc/$p/fd/1 || true)
  case "$fd1" in pipe:*) echo "FAIL: pid $p still holds a pipe as stdout: $fd1"; fail=1;; esac
done
[ "$fail" = 0 ] && echo "ok: start returned in ${took}s with $out"
exit $fail
INNER
docker run --rm -v "$dir":/w alpine sh /w/inner.sh
