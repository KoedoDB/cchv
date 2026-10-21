# cchv — run Kubernetes Pods in Cloud Hypervisor microVMs (a containerd shim)

**cchv** is a [containerd](https://containerd.io/) shim (runtime v2) that runs every [Kubernetes](https://kubernetes.io/) Pod in its own lightweight
[Cloud Hypervisor](https://www.cloudhypervisor.org/) virtual machine. You use it like any other runtime: a `RuntimeClass` and `runtimeClassName: cloudhv`
in the Pod. Containers stay ordinary OCI images; the isolation is a KVM virtual machine with its own guest kernel instead of a shared host kernel.
Rust, source-available (PolyForm Noncommercial, see License), for Linux hosts with `/dev/kvm`. A pool of warm VMs restored from snapshots keeps the start of a Pod short and the fixed memory
cost of a VM small.

It began as a fork of [devigned/containerd-cloudhypervisor](https://github.com/devigned/containerd-cloudhypervisor) by David Justice (MIT) and has been an
independent repository since 2026-10-20 (the original has had no commits since March 2026; GitHub forwards the old address). The idea and the first
implementation are his; we are grateful for them. [How it differs](#history-and-what-changed-from-the-original).

## What you get

- **A VM for each Pod**, on a stock Kubernetes + containerd node (we run it on k3s): `kubectl logs`, `exec`, `attach`, `cp`, probes and `tty: true` work.
  `port-forward` does not.
- **Cloud Hypervisor, unprivileged and confined**: its own user, no capabilities, its own mount, PID, network, IPC and UTS namespaces and a cgroup.
- **Warm VM pool**: VMs wait in a pool, restored from a snapshot; a Pod gets one in about 60 ms.
- **Persistent block volumes**: an LVM logical volume (a Pod's `volumeDevices`) is hot-plugged into the VM with direct I/O, for a database that has to outlive it.
- **A hardened guest**: container specs rebuilt from an allow list, a default seccomp profile, a hardened guest kernel configuration.
- **Everything the daemon takes as input is checked** and fuzzed; a supply chain that is pinned and signed ([docs/supply-chain.md](docs/supply-chain.md)).

## Use it

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello
spec:
  runtimeClassName: cloudhv
  containers:
  - name: app
    image: alpine:3.22
    command: ["sh", "-c", "echo hello from a microVM; sleep 3600"]
    resources:
      limits: {cpu: "1", memory: 256Mi}   # the VM is sized from these
```

Install: [docs/k3s.md](docs/k3s.md) (k3s, the setup we run), the [Quick Start](#quick-start) below, and [docs/configuration.md](docs/configuration.md).

## Numbers (measured by us)

On our machines (a 12-core AMD development machine, and a bare-metal Xeon E3-1270 v6 server with Debian 13 and k3s v1.36), with Cloud Hypervisor v54.0.
They are not benchmarks of your hardware. The load is `pgbench` run inside the Pod, or an HTTP server in the Pod; memory is the resident size of the
Cloud Hypervisor process (and of the Pod's cgroup, where a table says so).

**What a VM costs** (Cloud Hypervisor process, resident; guest of 128 MiB):

| | |
|---|---|
| A VM waiting in the pool | **34 MiB** |
| A Pod restored from a warm snapshot (an idle Alpine) | **47–50 MiB** |
| A Pod sized by its limits (384 MiB; the VM is booted for it), idle Alpine | **63 MiB** |
| The same, running a Node.js HTTP server | 171 MiB (the workload is most of it) |

Start: about **60 ms** to restore a pooled VM, **220–350 ms** to boot a VM of another size; a Pod is `Running` in about 2 seconds on the server above.
For comparison (other runtimes, other machines; **we have not yet measured cchv with these loads, so the table has no cchv row**):

| Runtime | A Next.js server (SSR), idle | after load | start until it answers (polled every 0.5 s) | measured on |
|---|---|---|---|---|
| runc | 49 MiB | 163–192 MiB | 0.9–1.7 s | a bare-metal server, Docker, 1 GiB |
| gVisor | 70 MiB | 153–180 MiB | 2.1–6.1 s | the same |
| Kata + Cloud Hypervisor | 284 MiB | 379–467 MiB | 2.5–3.4 s | the same |

(An empty Alpine Pod on Kata + Cloud Hypervisor, VM of 1 GB: 177 MiB; the shim and the VMM's processes, as proportional set size. The start time depends on the CPU limit, 0.5 to 2 CPUs.)
On a development machine, a small Deno HTTP server took 48 MiB with runc, 110–170 MiB with gVisor and 289 MiB with Kata + Cloud Hypervisor (the processes' memory, outside Kubernetes).

**A database in a VM** — PostgreSQL 18, Pod of 2 CPU / 1 GiB, `pgbench`:

| | cchv (VM) | runc |
|---|---|---|
| `tpcb-like`, 8 clients, 30 s (scale 10, emptyDir) | 3,590 tps (2.23 ms) | 3,723 tps (2.15 ms) |
| `pg_test_fsync` | 222–241 µs | 192–217 µs |

Writes are about 0.96 times those of runc; `fsync` about 1.1 times slower. With a persistent volume and a database larger than the VM's memory (scale 100, 1.5 GB;
VM of 1 GiB), direct I/O on, off and on again:

| | direct I/O on | off | on again |
|---|---|---|---|
| `tpcb-like` (5 min, 8 clients) | 3,032 tps | 3,070 tps | 2,965 tps |
| `select-only` (3 min) | 19,334 tps | 18,038 tps | 19,302 tps |
| page cache charged to the Pod's cgroup | 0 MiB | 259–264 MiB | 0 MiB |

**Does it hold up** (persistent volume, PostgreSQL under `pgbench`):
- `kill -9` of the VM's Cloud Hypervisor in the middle of writes, several times: the Pod came back, the totals of the `pgbench` tables agreed and `pg_amcheck` was clean.
- A full disk: PostgreSQL returned `ERROR: could not extend file … No space left on device`, with no `PANIC` and no restart; writes resumed within a second of freeing space.
- Eight database Pods at once on one host of 8 threads (`pgbench`, 1 CPU each, data on an emptyDir with direct I/O): no failures, each got 562–581 tps (within 3% of each other) against 2,044 for one alone; the disk was not the limit (`iowait` 0.0%), the CPUs were.
- A hard reset of the host in the middle of writes (a software reset, not a power cut): all 11,416 acknowledged inserts were there.
- Replacing the daemon (`systemctl restart`, and `kill -9`) with nine Pods running (PostgreSQL on a persistent volume, one on an emptyDir, a web server, six idle ones): the next daemon took over all nine VMs (same Cloud Hypervisor processes), no Pod restarted, the writes to the persistent volume did not stop for a moment, no acknowledged insert was lost and `pg_amcheck` was clean. The shim of each Pod waits for the daemon to come back (up to five minutes) instead of giving up. A VM nobody asks about for ten minutes after the takeover (its Pod was deleted while the daemon was away) is released. The first replacement, from a daemon that does not leave the records for a takeover, still stops every VM.
- Restarting k3s (containerd) with Pods running: they keep running. (containerd asks every shim for the process ids of its task when it starts, and deletes a shim that answers "not supported" as a leaked one; the shim answers now.)
- With the memory balloon on, memory the guest freed went back to the host within seconds (350 MB of anonymous memory in a Pod booted for its size; 120 MB of tmpfs in a Pod restored from the pool, whose guest memory is shared memory: the host's `Shmem` fell back within 5 seconds); page cache that is still in use does not.

What we have **not** measured or done: other hardware, a power cut, a hard reset with the Pods of this version (every VM is lost with the host; the Pods start again, a persistent volume keeps its data), many Pods at once, GPU or live migration (not supported), a run of days on this version.

## History and what changed from the original

We run a small hosting service ([KoedoDB](https://koedodb.com)) and were looking for a way to run customers' code that **starts
fast, uses little memory, performs well, and is safe**: microVMs light enough to keep one per project. This project is exactly
that idea, done in a way we liked: a thin shim, a guest agent, no big VMM stack. So we tried it.

Trying it, we ran into a few things (for instance the problems reported as issues #97 and #98: pods that did not start from a warm
restore, and memory that stayed resident), and the original had not seen new commits for a while. We needed fixes, and some of what
we needed (hardening of what runs in the guest, running Cloud Hypervisor itself unprivileged and confined, `exec` and terminals,
checks on everything the daemon and shim take as input) may not be what the original wants to carry. So we keep a copy that we maintain
for our own use, and we keep it in the open in case it is useful to someone else.

This is not meant as criticism: the original is a good starting point, and finding and fixing the rough edges is what a first user
does. If the original becomes active again, we would be glad to send our changes there (or to merge back); patches are welcome in
both directions. We are not affiliated with the Cloud Hypervisor, containerd or Kata projects.


### What changed

- **Fixed**: pods start from warm snapshots (#97), the VM is sized from the Pod's limits, per-container limits are enforced in the guest,
  logs reach `kubectl logs`, children of the daemon are stopped and cleaned up when it stops, volumes (ConfigMaps, Secrets,
  emptyDirs, `resolv.conf`) reach the VM (they did not), Cloud Hypervisor v54.
- **Hardening of the guest**: container specs are rebuilt from an allow list (capabilities cut, no device nodes, `noNewPrivileges`),
  a default seccomp profile for every container (including no `AF_VSOCK`), safe volume and disk handling, a hardened guest kernel
  configuration.
- **Cloud Hypervisor runs unprivileged and confined** (`ch_rootless`): its own user, no capabilities, its own mount, PID, network,
  IPC and UTS namespaces and a cgroup; the TAP device is handed over as a file descriptor; container images are readable only by the
  VM that runs them. See [docs/sandbox-daemon.md](docs/sandbox-daemon.md).
- **`kubectl exec`, `attach`, `cp`, exec probes and `tty: true`** work (the original refuses them). `kubectl port-forward` does not
  (see [docs/shim.md](docs/shim.md)).
- **Input is checked**: the daemon validates every request (ids, paths, namespaces, sizes, who is connecting), the shim reads volume
  sources beneath an opened directory, and the parsers are fuzzed (`fuzz/`).
- **Our own shimkit**: the task service library is [KoedoDB/containerd-shimkit](https://github.com/KoedoDB/containerd-shimkit), a
  cut-down and extended copy of the one in [containerd/runwasi](https://github.com/containerd/runwasi).
- **Supply chain**: pinned actions and base images, checked downloads, signed installer image, `cargo audit` clean. See
  [docs/supply-chain.md](docs/supply-chain.md).
- **Persistent block volumes**: a raw block device the platform made (an LVM logical volume, a Pod's `volumeDevices`) is hot-plugged into the VM with direct I/O and
  mounted as ext4, for a database that has to outlive the VM. Only a device the daemon is told to allow (by name and LVM uuid), one VM at a time. See
  [docs/sandbox-daemon.md](docs/sandbox-daemon.md#persistent-block-volumes-blockvolrs).
- **Disk and memory options**: `disk_direct` (on by default: the guest has its own page cache and the host does not keep a second copy) and `vm_balloon` (off by default:
  a VM's resident memory follows what its guest uses).
- **The guest's console reaches the log**: the kernel's messages and the agent's log are read from a Unix socket (not a file, so nothing grows on the host's disk) and logged
  as `console[vm=<name>] <line>`, cleaned and with a budget for each VM.
- **More fixes**: the loopback interface of the guest was never brought up (a netlink reply was read one behind), and a base snapshot was reused after the kernel or the
  root file system had changed (it is now checked).
- **Windows** code is gone (Cloud Hypervisor runs on Linux hosts), and so are the Wasm-oriented parts of shimkit.

## Status

**Experimental.** We use it in our own tests; we do not run other people's code on it yet. It has been reviewed by us (and by
AI-assisted reviews) but **not by anyone outside the project**, and the guest kernel's hardening options are in the configuration but
have not been exercised on a built kernel. Please read the code before you rely on it, and tell us what you find: see
[SECURITY.md](SECURITY.md) for vulnerabilities (privately), and use issues or discussions for everything else.

## When to Use

Choose this shim when you are building a **platform** where you control the stack and want VM isolation for a Pod with a smaller fixed
cost than a full Kata stack: serverless-style workloads, agent sandboxes, hosting for untrusted code. It is not a drop-in replacement
for Kata: no GPU passthrough, no live migration, no CPU or memory hot-plug, no virtiofs (volumes are copied in when the container
starts), no `port-forward`, and a single hypervisor.

## Quick Start

### System extension for Flatcar and similar container OSes

A self-contained system extension image is shipped with each [release](releases/); there's a Butane snippet included with the release notes for provisioning the extension.
The general pattern is
```
variant: flatcar
version: 1.0.0

storage:
  files:
  - path: /etc/extensions/containerd-cloudhypervisor.raw
    mode: 0644
    contents:
      source: https://github.com/KoedoDB/cchv/releases/download/<release-version>/containerd-cloudhypervisor-<release-version>-x86-64.raw
```

The sysext includes a brief demo to verify if the system is working. Run
```shell
root@flatcar $ /usr/share/cloudhv/demo/demo.sh
```
to verify.

#### Test your builds locally in a Flatcar VM

Sysext integration makes it easy to build the repository and run it locally in a Flatcar VM.

First, build the sysext.
This build is containerised and has no host dependencies (except Docker).
```
bash hacks/build-sysext.sh
```

For local testing, we'll leverage the [`boot` feature](https://github.com/flatcar/sysext-bakery?tab=readme-ov-file#interactively-test-extension-images-in-a-local-vm)
of Flatcar's [sysext bakery](https://github.com/flatcar/sysext-bakery).

1. Check out the bakery repo into a separate directory:
   ```
   git clone --depth 1 https://github.com/flatcar/sysext-bakery.git
   ```
2. Copy `containerd-cloudhypervisor.raw` into the bakery repo root; change into the bakery repo root.
3. Run
   ```
   ./bakery.sh boot containerd-cloudhypervisor.raw
   ```

This will download the latest Flatcar Alpha release for qemu, then start a Flatcar VM in ephemeral mode (no changes will be persisted in the Flatcar OS image).
`bakery.sh boot` will also launch a local Python webserver and generate transient Ignition configuration to provision `containerd-cloudhypervisor.raw` at boot time.

After the VM boot finished, you'll end up on the VM's serial port.
Run the demo included with the extension image to verify:
```bash
sudo /usr/share/cloudhv/demo/demo.sh
```

You can also connect to the local VM via ssh, using the `core` user:
```bash
ssh -p 2222 core@localhost
```

### Manual installation

```bash
# Build
cargo build --release -p containerd-shim-cloudhv
cargo build --release -p cloudhv-sandbox-daemon
cargo build --release -p cloudhv-agent --target x86_64-unknown-linux-musl
cd guest/kernel && bash build-kernel.sh && cd ../..
cd guest/rootfs && sudo bash build-rootfs.sh ../../target/x86_64-unknown-linux-musl/release/cloudhv-agent && cd ../..

# Install binaries
sudo install -m 755 target/release/containerd-shim-cloudhv-v1 /usr/local/bin/
sudo install -m 755 target/release/cloudhv-sandbox-daemon /usr/local/bin/
sudo mkdir -p /opt/cloudhv /run/cloudhv/erofs-cache /run/cloudhv/daemon
sudo cp guest/kernel/vmlinux guest/rootfs/rootfs.erofs /opt/cloudhv/

# Shim config (see docs/configuration.md for full reference)
sudo tee /opt/cloudhv/config.json > /dev/null <<EOF
{
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0",
  "default_vcpus": 1,
  "max_default_vcpus": 0,
  "default_memory_mb": 320,
  "max_containers_per_vm": 5,
  "daemon_socket": "/run/cloudhv/daemon.sock"
}
EOF

# Daemon config
sudo tee /opt/cloudhv/daemon.json > /dev/null <<EOF
{
  "pool_size": 3,
  "max_pool_size": 10,
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "kernel_args": "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0",
  "socket_path": "/run/cloudhv/daemon.sock",
  "state_dir": "/run/cloudhv/daemon",
  "warmup_duration_secs": 30,
  "max_snapshots": 100
}
EOF

# Systemd unit for the daemon
sudo tee /etc/systemd/system/cloudhv-sandbox-daemon.service > /dev/null <<EOF
[Unit]
Description=CloudHV Sandbox Daemon
After=containerd.service
Requires=containerd.service

[Service]
Type=simple
ExecStartPre=/bin/mkdir -p /run/cloudhv/daemon
ExecStart=/usr/local/bin/cloudhv-sandbox-daemon /opt/cloudhv/daemon.json
Restart=always
RestartSec=5
# Only the daemon is stopped, not the Cloud Hypervisors it started: the VMs handed to pods run on while the daemon is replaced, and the next one takes
# them over (keep_vms_on_stop in daemon.json). With the default (control-group) systemd would end every process of the unit, VMs included.
KillMode=process
Environment=RUST_LOG=info
MemoryMax=4G

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now cloudhv-sandbox-daemon
```

## Documentation

See the **[docs/](docs/)** folder for detailed documentation:

- **[Architecture](docs/architecture.md)** — system design, daemon + shim + agent components, networking
- **[Sandbox Daemon](docs/sandbox-daemon.md)** — daemon design, VM pool, shadow snapshots, benchmark results
- **[Configuration](docs/configuration.md)** — shim and daemon config reference, pod annotations
- **[Performance](docs/performance.md)** — benchmarks, latency breakdown, density, comparison with Kata
- **[Development](docs/development.md)** — building, testing, contributing, code quality standards
- **[Releasing](docs/releasing.md)** — how we cut a release, published artifacts, verifying and installing them
- **[Supply chain](docs/supply-chain.md)** — what is pinned and checked, how to update it
- **[Security policy](SECURITY.md)** — how to report a vulnerability

## Examples

- **[Bare Linux with crictl](example/crictl/)** — run containers with crictl, no Kubernetes required (from the original; not re-run by us)
- **[Azure Kubernetes Service](example/aks/)** — deploy on AKS with the DaemonSet installer. **This is the original's procedure and we
  have not run it**: our changes (unprivileged Cloud Hypervisor, ACLs, cgroup layout, the volume source allow list) are untested on AKS
  nodes, and the code assumes `/dev/kvm` (not `/dev/mshv`).
- **k3s** — the setup we test on: [docs/k3s.md](docs/k3s.md). containerd's config comes from a template (`config-v3.toml.tmpl`) that k3s regenerates on start, so the
  installer, which edits `/etc/containerd/config.toml`, does not fit k3s as it is.
- **Docker** — trying it with `docker run --runtime=cloudhv` (Docker 29 with the containerd image store) is being worked on; see
  [docs/try-with-docker.md](docs/try-with-docker.md) if it is there yet.

## License

This is **not open source**: the source is public so that it can be read and checked.

- The **original work** (devigned/containerd-cloudhypervisor by David Justice, up to its commit `be79d285c0f8`) stays under the **MIT** licence: [LICENSE](LICENSE).
  You can take it from upstream under those terms.
- **Our changes and new files** (everything after that commit: the daemon, the shim and agent changes, the installer, the guest, the docs) are copyright
  (c) 2026 RadarWorks LLC (KoedoDB) and under the **PolyForm Noncommercial License 1.0.0**: [LICENSE-POLYFORM-NC.md](LICENSE-POLYFORM-NC.md).
  Personal, research, hobby and similar noncommercial use is allowed; commercial use needs a licence from us (license@koedodb.com).
  Running it as part of a service you sell or offer (a VPS or hosting provider, a PaaS, a BaaS, a cloud) is commercial use, and so is use inside a company for
  its business: it needs a licence from us.
- Where a file has both, the combined file is to be used under the terms of both, so for practical purposes the fork as a whole is noncommercial.
- Releases up to and including `v0.17.0-koedodb.1` were published under MIT, and stay under it for those who got them. The new terms apply from the next release.

A release also carries software of others (Cloud Hypervisor, crun, the Linux
kernel, the Rust crates): [THIRD-PARTY.md](THIRD-PARTY.md) lists it with the licenses and the source of the GPL parts, and each release
attaches the license texts and CycloneDX SBOMs.
