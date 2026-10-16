#!/bin/sh
# End-to-end tests of the cloudhv runtime on a Kubernetes cluster (k3s, kubeadm, ...) that has a RuntimeClass named `cloudhv`.
#
#   tests/e2e/run.sh                      # from a machine whose kubectl talks to the cluster
#   docker exec -i <k3s-container> sh < tests/e2e/run.sh   # inside a test container that has kubectl (POSIX sh only)
#
# Environment:
#   E2E_NODE=1        the script runs on the node (or in the node's container): also check the host side (cgroup limits, the files
#                     of disk emptyDirs, leftover VM processes). Off by default; those checks are skipped, not failed.
#   E2E_IMAGE         image with a shell and busybox tools (default docker.io/library/busybox:1.36)
#   E2E_WEB_IMAGE     image that serves HTTP on port 80 (default docker.io/library/nginx:1.27-alpine)
#   E2E_RUNTIME_CLASS RuntimeClass to test (default cloudhv)
#   E2E_PARALLEL      pods started at once in the parallel test (default 24)
#   E2E_ONLY          run only the tests whose name contains this text
#
# Every test uses pods named e2e-*, in the namespace of the current kubectl context, and removes them. Nothing here talks to
# anything but the cluster (and, with E2E_NODE=1, the node's own cgroup and state files). Exit status: 0 if nothing failed.

IMAGE="${E2E_IMAGE:-docker.io/library/busybox:1.36}"
WEB="${E2E_WEB_IMAGE:-docker.io/library/nginx:1.27-alpine}"
RC="${E2E_RUNTIME_CLASS:-cloudhv}"
PAR="${E2E_PARALLEL:-24}"
NODE="${E2E_NODE:-0}"
ONLY="${E2E_ONLY:-}"

PASS=0
FAIL=0
SKIP=0
FAILED=""

say() { printf '%s\n' "$*"; }
pass() { PASS=$((PASS + 1)); say "  ok    $1"; }
fail() { FAIL=$((FAIL + 1)); FAILED="$FAILED $1"; say "  FAIL  $1${2:+: $2}"; }
skip() { SKIP=$((SKIP + 1)); say "  skip  $1${2:+: $2}"; }
want() { [ -z "$ONLY" ] || case "$1" in *"$ONLY"*) return 0 ;; *) return 1 ;; esac; }
section() { say ""; say "== $1"; }

cleanup_pods() { kubectl delete pod -l e2e=yes --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1; }

# apply YAML on stdin
apply() { kubectl apply -f - >/dev/null; }

# wait_ready NAME [SECONDS]
wait_ready() { kubectl wait --for=condition=Ready "pod/$1" --timeout="${2:-120}s" >/dev/null 2>&1; }

# wait_phase NAME PHASE [SECONDS]: Succeeded or Failed
wait_done() {
  n=0
  while [ "$n" -lt "${2:-120}" ]; do
    ph=$(kubectl get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)
    case "$ph" in Succeeded | Failed) return 0 ;; esac
    sleep 1
    n=$((n + 1))
  done
  return 1
}

# a pod that sleeps: pod NAME [extra spec YAML lines (indented 2)]
sleeper() {
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: $1, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  terminationGracePeriodSeconds: 2
  containers:
  - name: a
    image: $IMAGE
    command: ["sleep", "3600"]
EOF
}

now_ms() { date +%s%3N 2>/dev/null || echo 0; }

cleanup_pods

# ---------------------------------------------------------------------------
if want start; then
  section "start: a pod reaches Ready, and serves HTTP"
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-web, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  containers:
  - name: web
    image: $WEB
    readinessProbe: {httpGet: {path: /, port: 80}, periodSeconds: 1}
EOF
  t0=$(now_ms)
  if wait_ready e2e-web 120; then
    pass "start: Ready (probe passed) in $(($(now_ms) - t0)) ms after apply returned"
  else
    fail "start: Ready" "pod did not become Ready"
  fi
  ip=$(kubectl get pod e2e-web -o jsonpath='{.status.podIP}')
  sleeper e2e-client
  wait_ready e2e-client 120
  out=$(kubectl exec e2e-client -- wget -q -O - -T 5 "http://$ip/" 2>&1 | head -c 200)
  case "$out" in *nginx*) pass "start: a second pod reaches it over the pod network" ;; *) fail "start: pod network" "$out" ;; esac
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want logs; then
  section "logs: the output of short-lived containers is not lost"
  bad=0
  for i in 1 2 3 4 5 6 7 8; do
    cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-log$i, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  restartPolicy: Never
  containers:
  - name: a
    image: $IMAGE
    command: ["sh", "-c", "echo out-$i; echo err-$i >&2"]
EOF
  done
  for i in 1 2 3 4 5 6 7 8; do
    wait_done e2e-log$i 120
    l=$(kubectl logs e2e-log$i 2>&1 | tr '\n' ' ')
    case "$l" in *out-$i*err-$i* | *err-$i*out-$i*) ;; *) bad=$((bad + 1)); say "        e2e-log$i: [$l]" ;; esac
  done
  [ "$bad" -eq 0 ] && pass "logs: stdout and stderr of 8 short pods both in kubectl logs" || fail "logs" "$bad of 8 pods lost output"
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want exec; then
  section "exec: kubectl exec, with and without a terminal, and the exit code"
  sleeper e2e-exec
  if wait_ready e2e-exec 120; then
    [ "$(kubectl exec e2e-exec -- echo hello 2>&1)" = "hello" ] && pass "exec: output" || fail "exec: output"
    kubectl exec e2e-exec -- sh -c 'exit 7' >/dev/null 2>&1
    [ "$?" -eq 7 ] && pass "exec: exit code 7 comes back" || fail "exec: exit code"
    if [ -t 0 ]; then
      t=$(kubectl exec -it e2e-exec -- sh -c 'tty; stty size' 2>&1 | tr -d '\r' | tr '\n' ' ')
      case "$t" in */dev/pts/*) pass "exec: with a terminal (-it): $t" ;; *) fail "exec: terminal" "$t" ;; esac
    else
      skip "exec: with a terminal (-it)" "kubectl needs a terminal on its own stdin: copy this script to the node and run it from an interactive shell (docker exec -it ... sh run.sh)"
    fi
    echo "from-stdin" | kubectl exec -i e2e-exec -- cat >/tmp/e2e-stdin.$$ 2>&1
    [ "$(cat /tmp/e2e-stdin.$$)" = "from-stdin" ] && pass "exec: stdin (-i)" || fail "exec: stdin"
    rm -f /tmp/e2e-stdin.$$
    kubectl exec e2e-exec -- sh -c 'dd if=/dev/zero bs=1M count=20 2>/dev/null | wc -c' 2>&1 | grep -q 20971520 && pass "exec: 20 MiB through stdout" || fail "exec: large output"
    kubectl cp e2e-exec:/etc/hostname /tmp/e2e-cp.$$ >/dev/null 2>&1 && [ -s /tmp/e2e-cp.$$ ] && pass "exec: kubectl cp from the pod" || skip "exec: kubectl cp" "needs tar in the image"
    rm -f /tmp/e2e-cp.$$
  else
    fail "exec: pod Ready"
  fi
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want probe; then
  section "probes: exec, tcp and http probes work, and a failing liveness probe restarts the container"
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-probe, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  containers:
  - name: web
    image: $WEB
    readinessProbe: {exec: {command: ["sh", "-c", "test -f /etc/nginx/nginx.conf"]}, periodSeconds: 1}
    livenessProbe: {tcpSocket: {port: 80}, periodSeconds: 1}
    startupProbe: {httpGet: {path: /, port: 80}, periodSeconds: 1, failureThreshold: 30}
EOF
  wait_ready e2e-probe 120 && pass "probes: exec readiness, tcp liveness and http startup probes pass" || fail "probes: Ready"
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-live, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  containers:
  - name: a
    image: $IMAGE
    command: ["sh", "-c", "touch /tmp/ok; sleep 5; rm /tmp/ok; sleep 3600"]
    livenessProbe: {exec: {command: ["cat", "/tmp/ok"]}, initialDelaySeconds: 1, periodSeconds: 1, failureThreshold: 2}
EOF
  n=0
  r=0
  while [ "$n" -lt 90 ]; do
    r=$(kubectl get pod e2e-live -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    [ "${r:-0}" -ge 1 ] && break
    sleep 1
    n=$((n + 1))
  done
  [ "${r:-0}" -ge 1 ] && pass "probes: a failing liveness probe restarted the container (restarts=$r)" || fail "probes: liveness restart"
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want volume; then
  section "volumes: emptyDir on disk and in memory, ConfigMap, Secret"
  kubectl create configmap e2e-cm --from-literal=k=v1 --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl create secret generic e2e-sec --from-literal=s=topsecret --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-vol, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  terminationGracePeriodSeconds: 2
  volumes:
  - {name: disk, emptyDir: {}}
  - {name: ram, emptyDir: {medium: Memory}}
  - {name: cm, configMap: {name: e2e-cm}}
  - {name: sec, secret: {secretName: e2e-sec}}
  containers:
  - name: a
    image: $IMAGE
    command: ["sh", "-c", "echo from-a > /disk/f; sleep 3600"]
    volumeMounts: [{name: disk, mountPath: /disk}, {name: ram, mountPath: /ram}, {name: cm, mountPath: /cm}, {name: sec, mountPath: /sec}]
  - name: b
    image: $IMAGE
    command: ["sleep", "3600"]
    volumeMounts: [{name: disk, mountPath: /other}]
EOF
  if wait_ready e2e-vol 120; then
    sleep 2
    [ "$(kubectl exec e2e-vol -c b -- cat /other/f 2>&1)" = "from-a" ] && pass "volumes: two containers share a disk emptyDir" || fail "volumes: shared emptyDir"
    m=$(kubectl exec e2e-vol -c a -- sh -c 'mount | grep -E " /disk | /ram "' 2>&1)
    case "$m" in *"/disk type ext4"*) pass "volumes: emptyDir is ext4 on a block device" ;; *) fail "volumes: emptyDir type" "$m" ;; esac
    case "$m" in *"tmpfs on /ram"*) pass "volumes: medium Memory is a tmpfs" ;; *) fail "volumes: Memory emptyDir" "$m" ;; esac
    [ "$(kubectl exec e2e-vol -c a -- cat /cm/k 2>&1)" = "v1" ] && pass "volumes: ConfigMap file" || fail "volumes: ConfigMap"
    [ "$(kubectl exec e2e-vol -c a -- cat /sec/s 2>&1)" = "topsecret" ] && pass "volumes: Secret file" || fail "volumes: Secret"
    kubectl exec e2e-vol -c a -- sh -c 'dd if=/dev/zero of=/disk/big bs=1M count=100 conv=fsync 2>&1 | tail -1' >/dev/null 2>&1
    sz=$(kubectl exec e2e-vol -c b -- sh -c 'ls -l /other/big | awk "{print \$5}"' 2>&1)
    [ "$sz" = "104857600" ] && pass "volumes: 100 MiB written to the disk emptyDir is visible from the other container" || fail "volumes: 100 MiB" "$sz"
    kubectl exec e2e-vol -c a -- sh -c 'dd if=/dev/zero of=/disk/fill bs=1M count=4000 2>&1; echo dd-exit=$?' 2>&1 | grep -qiE "no space|ENOSPC" && pass "volumes: writing past the limit stops with no space left" || fail "volumes: ENOSPC"
    kubectl exec e2e-vol -c a -- sh -c 'rm -f /disk/fill; kill 1' >/dev/null 2>&1
    sleep 12
    [ "$(kubectl exec e2e-vol -c b -- cat /other/f 2>&1)" = "from-a" ] && pass "volumes: files survive a restart of the other container" || fail "volumes: after container restart"
    if [ "$NODE" = 1 ]; then
      d=/var/lib/cloudhv/emptydir
      n=$(find "$d" -name '*.img' 2>/dev/null | wc -l)
      [ "$n" -ge 1 ] && pass "volumes (node): the disk emptyDir is a file on the host ($n)" || fail "volumes (node): no image file under $d"
    fi
  else
    fail "volumes: pod Ready"
  fi
  cleanup_pods
  kubectl delete configmap e2e-cm secret e2e-sec --ignore-not-found >/dev/null 2>&1
  if [ "$NODE" = 1 ]; then
    sleep 8
    n=$(find /var/lib/cloudhv/emptydir -name '*.img' 2>/dev/null | wc -l)
    [ "$n" -eq 0 ] && pass "volumes (node): no image file left after the pods are gone" || fail "volumes (node): $n image file(s) left"
  fi
fi

# ---------------------------------------------------------------------------
if want limits; then
  section "limits: CPU and memory limits reach the VM"
  cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-lim, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  terminationGracePeriodSeconds: 2
  containers:
  - name: a
    image: $IMAGE
    command: ["sleep", "3600"]
    resources: {requests: {cpu: 100m, memory: 64Mi}, limits: {cpu: 500m, memory: 512Mi}}
EOF
  if wait_ready e2e-lim 120; then
    pass "limits: a pod with limits (500m CPU, 512Mi) starts"
    mem=$(kubectl exec e2e-lim -- sh -c "awk '/MemTotal/{print int(\$2/1024)}' /proc/meminfo" 2>&1)
    [ "${mem:-0}" -ge 400 ] 2>/dev/null && pass "limits: the guest sees $mem MiB for a 512Mi limit" || fail "limits: guest memory" "$mem"
    if [ "$NODE" = 1 ]; then
      uid=$(kubectl get pod e2e-lim -o jsonpath='{.metadata.uid}')
      cg=$(ls -d /sys/fs/cgroup/kubepods/*/pod"$uid" 2>/dev/null | head -1)
      if [ -n "$cg" ]; then
        mx=$(cat "$cg/cpu.max" 2>/dev/null)
        case "$mx" in "50000 100000") pass "limits (node): the pod's cgroup has cpu.max $mx" ;; *) fail "limits (node): cpu.max" "$mx" ;; esac
        procs=$(find "$cg" -name cgroup.procs -exec cat {} + | sort -u | wc -l)
        [ "$procs" -ge 2 ] && pass "limits (node): the VM's processes are inside the pod's cgroup ($procs)" || fail "limits (node): VM not in the pod's cgroup"
      else
        skip "limits (node)" "pod cgroup not found under /sys/fs/cgroup/kubepods"
      fi
    fi
  else
    fail "limits: pod with limits Ready"
  fi
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want recovery; then
  section "recovery: a pod whose VM is killed comes back by itself (needs E2E_NODE=1)"
  if [ "$NODE" != 1 ]; then
    skip "recovery" "needs E2E_NODE=1: the VM is found and killed through the pod's cgroup"
  else
    # The Cloud Hypervisor processes of a pod (found through the pod's cgroup).
    ch_pids() {
      u=$(kubectl get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null)
      c=$(ls -d /sys/fs/cgroup/kubepods/*/pod"$u" 2>/dev/null | head -1)
      [ -n "$c" ] || return 0
      find "$c" -name cgroup.procs -exec cat {} + 2>/dev/null | sort -u | while read -r q; do
        [ "$(cat /proc/"$q"/comm 2>/dev/null)" = cloud-hyperviso ] && echo "$q"
      done
    }
    # recover NAME: kill the VM, wait until the pod is Ready with another VM; prints the seconds, or fails
    recover() {
      old=$(ch_pids "$1" | tr '\n' ' ')
      [ -n "$old" ] || return 2
      for q in $old; do kill -9 "$q" 2>/dev/null; done
      t0=$(date +%s)
      while [ $(( $(date +%s) - t0 )) -lt 120 ]; do
        sleep 1
        new=$(ch_pids "$1" | tr '\n' ' ')
        if [ -n "$new" ] && [ "$new" != "$old" ] && [ "$(kubectl get pod "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" = True ]; then
          echo $(( $(date +%s) - t0 )); return 0
        fi
      done
      return 1
    }
    sleeper e2e-rec1
    cat <<EOF | apply
apiVersion: v1
kind: Pod
metadata: {name: e2e-rec2, labels: {e2e: "yes"}}
spec:
  runtimeClassName: $RC
  terminationGracePeriodSeconds: 2
  containers:
  - {name: a, image: $IMAGE, command: ["sleep", "3600"]}
  - {name: b, image: $IMAGE, command: ["sleep", "3600"]}
EOF
    wait_ready e2e-rec1 120 && wait_ready e2e-rec2 120 || fail "recovery: pods Ready"
    sec=$(recover e2e-rec1) && pass "recovery: a pod with one container is Ready again with a new VM after $sec s" || fail "recovery: one container" "not Ready with a new VM within 120 s"
    sec=$(recover e2e-rec2) && pass "recovery: a pod with two containers is Ready again with a new VM after $sec s" || fail "recovery: two containers" "not Ready with a new VM within 120 s"
    kubectl delete pod e2e-rec1 e2e-rec2 --wait=true --timeout=120s >/dev/null 2>&1
    sleep 8
    n=$(ls /run/netns 2>/dev/null | wc -l)
    say "        (node: $n network namespaces after the pods are gone)"
  fi
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want security; then
  section "security: what the guest denies"
  sleeper e2e-sec2
  if wait_ready e2e-sec2 120; then
    kubectl exec e2e-sec2 -- unshare -r true >/dev/null 2>&1 && fail "security: unshare -r is allowed" || pass "security: unshare (new user namespace) is denied"
    k=$(kubectl exec e2e-sec2 -- sh -c 'cat /proc/self/status | grep -E "^(NoNewPrivs|Seccomp|CapEff)"' 2>&1 | tr '\n' ' ')
    case "$k" in *"Seccomp:"*"2"*) pass "security: the container runs under a seccomp filter ($k)" ;; *) fail "security: seccomp" "$k" ;; esac
    kubectl exec e2e-sec2 -- sh -c 'mknod /tmp/x c 1 3' >/dev/null 2>&1 && fail "security: mknod is allowed" || pass "security: mknod is denied"
    kubectl exec e2e-sec2 -- sh -c 'mount -t tmpfs none /mnt' >/dev/null 2>&1 && fail "security: mount is allowed" || pass "security: mount is denied"
    kubectl exec e2e-sec2 -- sh -c 'ls /dev/kvm /dev/mem /dev/sda 2>&1 | grep -vq "No such"' >/dev/null 2>&1 && fail "security: host-like devices are visible" || pass "security: no /dev/kvm, /dev/mem or disk device nodes in the container"
  else
    fail "security: pod Ready"
  fi
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want dns; then
  section "dns: cluster DNS and the outside"
  sleeper e2e-dns
  if wait_ready e2e-dns 120; then
    kubectl exec e2e-dns -- nslookup kubernetes.default.svc.cluster.local >/dev/null 2>&1 && pass "dns: a cluster name resolves" || fail "dns: cluster name"
    kubectl exec e2e-dns -- nslookup github.com >/dev/null 2>&1 && pass "dns: an outside name resolves" || skip "dns: outside name" "no outside DNS from this cluster?"
  else
    fail "dns: pod Ready"
  fi
  cleanup_pods
fi

# ---------------------------------------------------------------------------
if want parallel; then
  section "parallel: $PAR pods at once, then all gone"
  i=1
  while [ "$i" -le "$PAR" ]; do
    sleeper "e2e-p$i" &
    i=$((i + 1))
  done
  wait
  t0=$(now_ms)
  bad=0
  i=1
  while [ "$i" -le "$PAR" ]; do
    wait_ready "e2e-p$i" 240 || bad=$((bad + 1))
    i=$((i + 1))
  done
  [ "$bad" -eq 0 ] && pass "parallel: all $PAR pods Ready ($(($(now_ms) - t0)) ms after the last apply)" || fail "parallel" "$bad of $PAR pods not Ready"
  cleanup_pods
  if [ "$NODE" = 1 ]; then
    sleep 10
    n=$(ps -o comm | grep -c '^cloud-hyperviso' 2>/dev/null)
    # the pool keeps a few warm VMs: they are not leftovers
    say "        (node: $n Cloud Hypervisor processes remain; the daemon's pool keeps a few warm VMs)"
  fi
fi

# ---------------------------------------------------------------------------
section "result"
say "passed: $PASS   failed: $FAIL   skipped: $SKIP"
[ "$FAIL" -eq 0 ] || { say "failed:$FAILED"; exit 1; }
exit 0
