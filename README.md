# cchv — a containerd shim for Cloud Hypervisor

A [containerd](https://containerd.io/) shim for [Cloud Hypervisor](https://www.cloudhypervisor.org/) that runs container workloads
inside lightweight microVMs: one VM per Pod, a small purpose-built shim instead of a full Kata stack, and a pool of warm VMs
restored from snapshots.

> **This began as a fork** of [devigned/containerd-cloudhypervisor](https://github.com/devigned/containerd-cloudhypervisor) by David Justice
> (MIT). The idea and the first implementation are his; we are grateful for them. The repository is independent of the original since 2026-10-20
> (the original has had no commits since March 2026); the history is kept, and so is the licence. It was called `containerd-cloudhypervisor`; GitHub forwards the old address.

## Why we forked

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

## What is different from the original

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

### What we measured

On one 12-core development machine (KVM, k3s with Cilium, a guest with 128 MiB; numbers are not benchmarks of your hardware):

| | Cloud Hypervisor process, resident |
|---|---|
| A VM waiting in the pool | **34 MiB** |
| A Pod restored from a warm snapshot (an idle Alpine) | **47–50 MiB** |
| A Pod sized by its limits (384 MiB; the VM is booted for it), idle Alpine | **63 MiB** |
| The same, running a Node.js HTTP server | **171 MiB** (the workload is most of it) |

VM boot: about 60 ms to restore a pooled VM, 220–350 ms to boot a VM of another size. (A Pod used to take 11–14 seconds to start with
the unprivileged Cloud Hypervisor: the daemon waited for an answer that Cloud Hypervisor never closes, until a 10 second timeout. That is fixed;
the figures below and in [docs/performance.md](docs/performance.md) are from after the fix.) **The figures in the original README (about 6 MB idle and
24 MB per VM, 150 pods in 11 s) came from other hardware and settings; we could not reproduce them**, and the fixed cost of a VM here is
the table above. Kata Containers with Cloud Hypervisor, in our setup, took 177 MiB for an empty Pod. A comparison of runc, gVisor,
Kata and this shim under the same load is in [docs/performance.md](docs/performance.md) when we have it. The restore mode (`Copy` by default, `OnDemand` opt-in) is in [docs/memory-restore.md](docs/memory-restore.md).

A database Pod (PostgreSQL 18, a VM of 1 GiB, a persistent volume on an LVM logical volume, `pgbench` scale 100, 1.5 GB: more than the VM's memory), the same test three times,
with direct I/O on, off and on again (2026-10-20):

| | direct I/O on | off | on again |
|---|---|---|---|
| `tpcb-like` (5 min, 8 clients) | 3,032 tps | 3,070 tps | 2,965 tps |
| `select-only` (3 min) | 19,334 tps | 18,038 tps | 19,302 tps |
| page cache charged to the Pod's cgroup | 0 MiB | 259–264 MiB | 0 MiB |

After a hard reset of the host in the middle of writes (a software reset, not a power cut), all 11,416 acknowledged inserts were there and `pg_amcheck` was clean.

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
- The security review and its results: [doc/LAMBDA-SECURITY.md](https://github.com/banil246/koedodb/blob/main/doc/LAMBDA-SECURITY.md) in the KoedoDB repository

## Examples

- **[Bare Linux with crictl](example/crictl/)** — run containers with crictl, no Kubernetes required (from the original; not re-run by us)
- **[Azure Kubernetes Service](example/aks/)** — deploy on AKS with the DaemonSet installer. **This is the original's procedure and we
  have not run it**: our changes (unprivileged Cloud Hypervisor, ACLs, cgroup layout, the volume source allow list) are untested on AKS
  nodes, and the code assumes `/dev/kvm` (not `/dev/mshv`).
- **k3s** — the setup we test on: containerd's config comes from a template (`config-v3.toml.tmpl`) that k3s regenerates on start, so the
  installer, which edits `/etc/containerd/config.toml`, does not fit k3s as it is. A written-down k3s example is still to come.
- **Docker** — trying it with `docker run --runtime=cloudhv` (Docker 29 with the containerd image store) is being worked on; see
  [docs/try-with-docker.md](docs/try-with-docker.md) if it is there yet.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 David Justice for the original work; the modifications in this fork are copyright
(c) 2026 RadarWorks LLC (KoedoDB) and under the same license. A release also carries software of others (Cloud Hypervisor, crun, the Linux
kernel, the Rust crates): [THIRD-PARTY.md](THIRD-PARTY.md) lists it with the licenses and the source of the GPL parts, and each release
attaches the license texts and CycloneDX SBOMs.
