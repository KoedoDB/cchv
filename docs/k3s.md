# Running on k3s

This is how the runtime is used on **k3s**, the way KoedoDB runs it. It was verified in a k3s test cluster (k3s `v1.36.4+k3s1`, Cilium as the CNI, containerd 2.x, cgroup v2, a 12-core x86_64 machine with KVM, the hardened 6.18.57 guest kernel of this repository, Cloud Hypervisor v54.0): every test of `tests/e2e/run.sh` passes there. **The test cluster ran inside a container on a development machine; a k3s installed directly on a server was not tried, and neither was the systemd unit of the daemon on a k3s node.** What is verified and what is not is marked below.

Not for Azure Kubernetes Service: `example/aks` was written for AKS and has **not been verified** (it needs nested virtualization on the node VM, which most AKS node sizes do not offer).

## What the node needs

- x86_64 or aarch64 with **KVM** (`/dev/kvm`), a cgroup v2 host, and a Linux kernel recent enough for Cloud Hypervisor (6.x is fine). Bare metal, or a VM with nested virtualization.
- k3s with containerd 2.x (config version 3). Cilium was used as the CNI; the runtime does not depend on it (it gets a network namespace from the CNI like any other runtime), but only Cilium was tested.
- `mke2fs` (package `e2fsprogs`) on the node, for disk-backed `emptyDir` volumes. Without it a pod with an `emptyDir` does not start (`medium: Memory` is fine).
- `mkfs.erofs` (package `erofs-utils`) and the `erofs` kernel module on the node, for the container images, and `tc` (package `iproute2`) for the VM's network. `installer/install.sh` checks for these.

## 1. Put the files on the node

From a release of this repository (`docs/releasing.md`; check the signature, `docs/supply-chain.md`):

| File | Where |
|---|---|
| `containerd-shim-cloudhv-v1` | `/usr/local/bin/` (on containerd's `PATH`) |
| `cloudhv-sandbox-daemon` | `/usr/local/bin/` |
| `cloud-hypervisor` | `/usr/local/bin/` |
| `vmlinux`, `rootfs.erofs` | `/opt/cloudhv/` (world-readable: the daemon refuses to start otherwise) |

`installer/install.sh` does this as a DaemonSet, but it patches containerd's config in the **containerd 1.x layout** (`io.containerd.grpc.v1.cri`, `/etc/containerd/config.toml`). **It does not fit k3s**, whose containerd config is generated and has a different layout: do step 2 by hand.

## 2. Register the runtime with k3s's containerd

k3s writes its containerd config itself; to add a runtime, give it a template. For containerd 2.x (config version 3) create `/var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.tmpl`:

```toml
{{ template "base" . }}

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.'cloudhv']
  runtime_type = 'io.containerd.cloudhv.v1'
```

and restart k3s (`systemctl restart k3s`; on an agent node `k3s-agent`). The runtime type `io.containerd.cloudhv.v1` makes containerd run `containerd-shim-cloudhv-v1`.

Then the RuntimeClass:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: cloudhv
handler: cloudhv
```

## 3. The configuration of the shim and the daemon

`/opt/cloudhv/config.json` (the shim) and `/opt/cloudhv/daemon.json` (the daemon); `configuration.md` lists many of the fields (not all: `require_pod_cgroup`, `volume_source_roots`, `emptydir_default_mib` and the others added in this fork are in `crates/shim/src/config.rs` and `crates/daemon/src/config.rs`). The values the test cluster used:

```json
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "max_containers_per_vm": 5,
  "daemon_socket": "/run/cloudhv/daemon.sock"
}
```

```json
{
  "pool_size": 3,
  "max_pool_size": 10,
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0",
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "/run/cloudhv/daemon",
  "ch_rootless": true,
  "mke2fs_path": "/sbin/mke2fs"
}
```

- `ch_rootless` runs every Cloud Hypervisor as a user of its own, confined (own mount, PID, network, IPC and UTS namespaces, an own cgroup). It is on in the test cluster. Leave it on for anything that runs customer code.
- The shim requires the pod's cgroup (`require_pod_cgroup`, on by default): under k3s the kubelet makes it. Turn it off only where there is no kubelet (the demo of the sysext does that).
- The memory restore mode is `Copy` by default; see `memory-restore.md` (the lazy modes need `userfaultfd`, which a rootless Cloud Hypervisor does not get on a normal host).
- The default VM is 320 MiB, 1 vCPU: 256 MiB are available to the workload (`performance.md`).

## 4. The daemon

The daemon must run before the first pod: `cloudhv-sandbox-daemon /opt/cloudhv/daemon.json`, as root, as a service that starts after k3s's containerd. `installer/install.sh` writes a systemd unit with the hardening that the repository uses (`sysext/root/usr/lib/systemd/system/cloudhv-sandbox-daemon.service` is the same unit for the Flatcar extension). **The test cluster started the daemon by hand in a container; the unit was not tried under systemd on a k3s node.**

It makes the base snapshot at its first start (about half a minute) and fills its pool; it opens its socket only after that.

## 5. A first pod

```yaml
apiVersion: v1
kind: Pod
metadata: {name: hello}
spec:
  runtimeClassName: cloudhv
  containers:
  - name: web
    image: docker.io/library/nginx:1.27-alpine
    resources:
      limits: {cpu: 500m, memory: 512Mi}
```

A pod with limits gets a VM of that size (the limit plus room for the guest's own memory: 512Mi gives a guest of about 548 MiB); a pod without limits gets the default VM, restored from a warm snapshot (a Pod is Ready in about 0.7 s on the test machine, `performance.md`). Pod start and `kubectl exec`, `logs`, `cp`, probes, `emptyDir`, ConfigMap and Secret volumes, DNS and the CPU and memory limits are covered by `tests/e2e/run.sh`.

## 6. Check it

```
E2E_NODE=1 sh tests/e2e/run.sh
```

on the node (or `docker exec` into the container that is the node) runs the tests; for the terminal check it needs a terminal (`script -qec "sh tests/e2e/run.sh" /dev/null`). See the header of the script.

## What does not work, or differs

- **`kubectl port-forward` does not work** (containerd connects to `localhost` inside the pod's network namespace, where the VM's network is not).
- **A ConfigMap or Secret that changes is not updated in a running pod.** The files are copied into the VM when the container starts. There is no virtiofs (on purpose: its daemon would be another process with access to the host's files).
- **`hostPath` volumes are not mounted**, and volume sources are limited to the kubelet's pod directories and an allow list (`volume_source_roots`); the shim reads them with `openat2` and `RESOLVE_BENEATH`.
- `emptyDir` is a disk image on the host (1 GiB by default, `emptydir_default_mib`), `medium: Memory` is a `tmpfs` in the guest capped at half of the VM's memory. A kubelet `sizeLimit` is not passed on to the runtime, so every disk `emptyDir` has the default size.
- Privileged containers, host namespaces and devices: **not tested**. The VM is the isolation, and the guest's runtime spec is rebuilt from an allow list (no devices, 12 capabilities, `noNewPrivileges`), so a pod that asks for more does not get it.
- The restore of a pod's size: a pod with limits is a **cold boot** of a VM of its size (about 0.85 to 1 s on the test machine, 0.84 s for 512Mi / 2 CPUs); only pods of the default size come from a warm snapshot.
- **A VM that dies** (killed, out of memory, the daemon restarted) ends the pod's sandbox; the kubelet makes the pod again (about 3 s with the test machine). The data in an `emptyDir` goes with the VM.

## Where it was tested

A 24-hour test with pods starting and stopping, faults (a VM killed, a pod force-deleted, a full `emptyDir`, a daemon restart) and a sample of the host's memory, file descriptors, processes, directories and cgroups every minute is described in `docs/performance.md` when it has finished.
