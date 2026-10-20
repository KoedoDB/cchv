# Trying the runtime with Docker

**This is an experiment.** The shim is written for Kubernetes (k3s with Cilium is what it is tested with). Docker can use any containerd
shim as a runtime, so `docker run --runtime=cloudhv …` starts the container in a Cloud Hypervisor VM, and this page shows how to try that on
a machine with KVM without changing it: everything runs in one throwaway container. It is meant for people who want to see the VM
life cycle, the confinement of Cloud Hypervisor and the snapshot pool work, not for running anything you care about.

## One command

You need Docker (29 or newer: the containerd image store is what Docker uses to hand a container to a shim) and `/dev/kvm` on an x86_64
Linux machine. Nothing else.

```
hacks/try-with-docker.sh --artifacts DIR --feature-test
```

It starts `docker:dind` (privileged, with `/dev/kvm`), configures the Docker inside with a runtime `cloudhv`, installs the shim, the
sandbox daemon, Cloud Hypervisor (the static v54.0 release, checked against the SHA-256 in `hacks/checksums.txt`) and the guest kernel and
root file system, starts the daemon (a pool of three ready VMs), and runs `docker run --runtime=cloudhv alpine uname -r` as a check: the
kernel it prints is the guest's. About 45 seconds the first time, 10 seconds when the container exists. `--feature-test` also runs the tour
of `docker` commands below. `hacks/try-with-docker.sh --clean` removes the container; `--help` lists the options.

`DIR` holds your own build of the two programs that have Docker support:

| file in `DIR` | what | how to build |
|---|---|---|
| `containerd-shim-cloudhv-v1` | the shim (static x86_64) | `bash hacks/build-static-rust.sh containerd-shim-cloudhv`; the binary is left in the repository root |
| `cloudhv-sandbox-daemon` | the daemon (static x86_64) | `cargo build --release -p cloudhv-sandbox-daemon --target x86_64-unknown-linux-musl` |
| `vmlinux`, `rootfs.erofs` (optional) | the guest kernel and root file system | taken from the release `--tag` (default `v0.15.0-koedodb.1`) if missing; to build them, see `hacks/build-guest.sh` |
| `cloud-hypervisor` (optional) | a static v54.0 binary | downloaded if missing |

Docker support was added after the release `v0.15.0-koedodb.1`: its shim and daemon do not have it (the check at the end of the script
fails with `bind-mount /proc/0/ns/net …`, and says so). Until a release has it, build the two as above.

Then, for example:

```
docker exec cchv-docker docker run --rm --runtime=cloudhv alpine:3.19 uname -a
docker exec -it cchv-docker docker run --rm -it --runtime=cloudhv alpine:3.19 sh
docker exec cchv-docker docker run -d --name web --runtime=cloudhv -p 8080:80 -m 512m nginx:1.27-alpine
docker exec cchv-docker tail -f /var/log/cloudhv-daemon.log     # the daemon (VM pool, networking, snapshots)
docker logs cchv-docker                                          # the inner containerd, with the shim's messages
```

A container started with `docker run -d --runtime=cloudhv … ` takes about half a second to start (a runc container: about 0.3 s).

## What works with Docker, and what does not

Checked with Docker 29.9 and the machine of the author (`hacks/docker-feature-test.sh` is the tour; the output differs in details):

| | |
|---|---|
| `docker run --runtime=cloudhv IMAGE CMD`, exit status of the command, stdout and stderr kept apart | works |
| `-m 512m`, `--cpus 2`: the VM is sized from them (`MemTotal` 561 MB, 2 CPUs), up to `max_memory_mb` and the host's CPUs | works |
| `-e`, `-w`, `-u`, `--read-only`, restart policies (`--restart=on-failure:2` restarted twice) | works |
| `-t` and `-it`: a terminal; input, window size (30×100, then 50×120 after a resize), Ctrl-C | works |
| `docker logs`, `docker ps`, `docker inspect` (the task's pid is a small process that holds the container's network namespace) | works |
| `docker exec` (also `-it`) | works |
| `docker stop` (SIGTERM goes to the container, exit status 0), `docker kill` (exit status 137), `docker rm` | works; the VM, its user, its files and its network device are removed |
| default bridge network: the container has its IP (the VM's `eth0`), DNS and Internet through the host's resolvers | works |
| `-p 8080:80`: docker-proxy connects to the container's IP and reaches the VM | works |
| `--network none`: no interface but `lo` | works |
| named volumes (`-v vol:/data`), and files of the container written under them | **start-up copy only**: see below |
| `-i` without `-t` (`echo x \| docker run -i …`) | **not supported**: the container's standard input is empty (the shim forwards the input of a terminal only) |
| bind mounts of host directories (`-v /host/dir:/data`) | **refused** unless the directory is below one of `volume_source_roots` of the shim's `/opt/cloudhv/config.json` (the recipe lists `/var/lib/docker/containers`, where Docker keeps `/etc/hosts` and `/etc/resolv.conf` of a container, and `/var/lib/docker/volumes`); copied at start, 2 MiB and 512 files at most |
| `docker cp`, `docker diff`, `docker commit`, `docker export` | **do not see what the container wrote**: the writable layer lives in the VM's memory, not in the host's layer |
| user-defined networks: names between containers (`http://other/`) | **not supported**: Docker's embedded DNS listens on `127.0.0.11` in the container's namespace, which is not the VM's loopback. By IP it works. |
| `--network host` | **not supported**: the VM has no network (only `lo`) |
| `--privileged`, `--cap-add`, `--device` | accepted, but the guest agent gives the container its fixed set of capabilities (no `NET_ADMIN`, `SYS_ADMIN`, `MKNOD`, `NET_RAW`) and no devices beyond the standard ones |
| `docker top` | **not supported** (`Pids` is not implemented: the processes are in the VM) |
| `docker stats` | runs, but shows zeros for memory and PIDs (the VM's numbers are not reported) |
| `docker pause`, `unpause` | **not supported** |
| `docker stop` of a container that ignores SIGTERM | Docker kills it after its timeout, as usual |

**Files and volumes.** The container's root file system is a read-only image (erofs) built from the layers, with a writable layer in the VM's
memory. Volumes are copied into the VM when the container starts. What the container writes stays in the VM and is gone when it stops, in
a volume as well. This is by design (the platform this was made for keeps nothing of a customer's VM on the host), and it is the biggest
difference from runc.

**Why some of this does not work, and what may change.** Most of the "not supported" rows come from one fact: the container is in a VM, with a network namespace
of its own and no view of the host's files except what is copied in or attached as a disk.

- **Names between containers (`docker compose`).** Docker's embedded DNS listens on `127.0.0.11` in the *container's network namespace* on the host; the VM has its
  own loopback, so nothing answers there. Containers on a user-defined network can reach each other **by IP** (the veth is wired to the VM), so a Compose file that
  uses addresses runs; one that uses service names does not. **Likely to be fixed, and small:** a DNS relay in the guest on `127.0.0.11:53` that sends each query
  over vsock to the shim, which enters the container's network namespace and asks the real `127.0.0.11`. Not built yet.
- **Bind mounts of host directories (`-v /host/dir:/data`), live.** Host and VM would have to see the same files at the same time. That takes a shared file
  system (virtiofs or 9p), which means one more process on the host that parses requests from the guest. **This fork does not do that, on purpose**, and a bind
  mount of a host directory stays out of scope: the directory is copied in at start (below `volume_source_roots`) and what the container writes is not
  written back. Do not use this fork to develop against a source tree on the host.
- **Named volumes (`-v vol:/data`) that keep what is written.** A disk does not need to be shared: a disk image kept on the host and attached to the VM as a
  virtio-blk disk would hold the data between runs (the host cannot read it while the VM runs). **Possible, not built.** The disk-backed `emptyDir` of Kubernetes
  is the same mechanism without keeping the file; keeping it means giving the confined Cloud Hypervisor a path that is known only after the VM exists
  (`docs/persistent-block-volumes.md` on the branch `persistent-block-volumes` is a design note on it).
- **`docker cp`, `diff`, `commit`, `export`** do not see what the container wrote, because the writable layer is in the VM's memory. Stays as it is.
- **`-i` without `-t`** (standard input from a pipe): the shim forwards the input of a terminal only. Possible to add.
- **`--network host`, `docker pause`, `docker top`, `docker stats`** (numbers): a VM has no host network and no process of its own on the host. Stays as it is.
- **`kubectl port-forward`** (Kubernetes, not Docker): containerd connects to `localhost` inside the pod's network namespace, where the VM's network is not.
  `docker run -p` is the Docker way and it works.

## How it fits Docker

Docker and Kubernetes hand a container to the runtime differently, and the shim notices which it is by the annotations of the OCI spec
(`io.kubernetes.cri.container-type` is there for Kubernetes; Docker has none). Under Docker, and only then:

- **The network namespace.** The spec asks the runtime to create one (a `network` namespace without a path). dockerd takes
  `/proc/<pid>/ns/net` of the pid that containerd's `State` shows right after `Create`, and sets the veth and the IP up in it *before*
  `Start`. A VM has no process of its own on the host, so at `Create` the shim starts a holder: the shim binary itself, run in a mode of its
  own (`CLOUDHV_NETNS_HOLDER`), which unshares a network namespace, tells its parent that it has, and sleeps until its parent is gone. Its pid
  is what `State` shows (shimkit's `Instance::pid_hint`). At `Start` the daemon wires the VM's TAP to that namespace (the veth is
  there already: no waiting), which needs `allow_proc_netns` in the daemon's `daemon.json` (off by default: with Kubernetes the namespace's path
  is `/var/run/netns/<name>`, which is always accepted). The holder is stopped at `delete`. With `--network none` the namespace has no
  interface and the VM gets no network.
- **The root file system.** Docker mounts it itself and puts the absolute path of the mount in `root.path`
  (`/var/lib/docker/rootfs/overlayfs/<id>`); `<bundle>/rootfs` stays empty. The shim builds the erofs image from `root.path`.
- **The VM's size** comes from `linux.resources` (`-m`, `--cpus`) when the spec has no sandbox annotations, with the same limits.
- **One container is the whole "pod"**: its VM is acquired and released with it, and a signal goes to the container (the VM is not
  released by `docker stop`'s SIGTERM before the container has exited).

None of this applies to a spec from Kubernetes.

## Security posture

The Cloud Hypervisor of each VM runs as its own non-root user, in its own mount, PID, network, IPC and UTS namespaces on a minimal root, in
its own cgroup, with no capabilities and a seccomp filter (`docs/sandbox-daemon.md`). This needs `ch_rootless` (the recipe turns it on) and, for
the namespace above, `allow_proc_netns` (the recipe turns it on too: **it lets the daemon enter `/proc/<pid>/ns/net` of any pid that a client
names**; the daemon's socket is root-only, and it still checks that the namespace is a network namespace and not the host's). The
container in the VM gets a capability allow-list, a default seccomp profile and no devices, whatever Docker
asks for. The throwaway container itself is **privileged** (Docker inside Docker needs it): do not run this on a machine whose Docker
you share with things you care about, and remove it with `--clean`.

## Troubleshooting

- `bind-mount /proc/0/ns/net … no such file or directory`: the shim is older than Docker support (see above).
- `executable file … not found in $PATH`: the shim built the image from an empty `<bundle>/rootfs` (an older shim again).
- `the source … is not below a path the runtime makes volumes in`: a bind mount of a host directory; add its parent to
  `volume_source_roots` in `/opt/cloudhv/config.json` inside the container, or use a named volume.
- The pool does not come up: `docker exec cchv-docker tail -30 /var/log/cloudhv-daemon.log`. Typical: no `/dev/kvm` in the container,
  or `kernel_path`/`rootfs_path` not readable by every user (they must be world-readable for the rootless Cloud Hypervisor).
- Slow or stuck start: `docker logs cchv-docker | grep -E "start_container|acquire"` shows the shim's timings.

## Report problems

Issues: <https://github.com/KoedoDB/cchv/issues>. A vulnerability: please use GitHub's private security advisory of
that repository, not an issue. Questions about Cloud Hypervisor itself belong in its own community
(<https://github.com/cloud-hypervisor/cloud-hypervisor>).
