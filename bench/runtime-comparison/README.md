# Runtime comparison: runc, gVisor, Kata Containers + Cloud Hypervisor, cchv + Cloud Hypervisor

The same PostgreSQL and the same nginx, started by four container runtimes on one machine, with the same limits and the same load. The scripts here
install the runtimes, run the workloads and print the tables. They were written for one measurement (October 2026) and are published so that the numbers
can be repeated and checked, not as a general benchmark suite.

**Read the numbers with the limits below in mind.** Where a runtime could not run a test the way the others did, the tables say so.

## What is compared

| name | what it is | how the benchmark starts it |
|---|---|---|
| `runc` | Docker's runc (the baseline) | `docker run --runtime runc` |
| `runsc` | gVisor (release 20261019.0), default platform, default settings | `docker run --runtime runsc` |
| `kata-qemu` | Kata Containers 4.3.1 (the Rust runtime) with **its default configuration, which uses QEMU** (virtio-fs). The first run of the benchmark used this by mistake while it was labelled Cloud Hypervisor; the results are kept as a row of their own | `docker run --runtime kata-clh`, with `runtime-rs/configuration.toml` pointing at the QEMU configuration |
| `kata-clh` | Kata Containers 4.3.1 (the Rust runtime), its Cloud Hypervisor configuration (its own Cloud Hypervisor v54.0), **virtio-fs** for the root file system and for the data directory (Kata's default storage path) | `docker run --runtime kata-clh`, with `runtime-rs/configuration.toml` pointing at `configuration-clh-runtime-rs.toml` |
| `kata-clh-blk-direct`, `kata-clh-blk` | the same Kata + Cloud Hypervisor, with the data on a **raw block device** given with `--device` (hot-plugged as virtio-blk, `/dev/vdb` in the guest); the entrypoint makes an ext4 on it if it has none and mounts it inside the container (the container gets `CAP_SYS_ADMIN` and no seccomp filter: the VM is the boundary). `-direct`: Cloud Hypervisor opens the disk with `direct=on` (a copy of the shipped configuration with `block_device_cache_direct = true`: the runtime-rs shim ignores the annotation); without it, Kata's default (`direct=off`). The image has `e2fsprogs` added (the stock image has no `mkfs.ext4`); that is the only difference | `rtbench_blk.py` |
| `cloudhv` | cchv v0.3.1: one Cloud Hypervisor microVM per container, the root file system as a read-only erofs image, the data volume as a raw block device (direct I/O) | `docker run --runtime cloudhv` |
| `runc-loop` | the control: runc, with the data directory on the same loop-device logical volume that cchv gets | `docker run --runtime runc` |

Everything runs in a **second Docker** (29.9.2, its own containerd, data root, socket and bridge `rtbench0`), started as systemd units next to the machine's
own Docker, which is not touched. `22-reset.sh` gives a clean state before each runtime.

## The machine

One bare-metal server: Xeon E3-1270 v6 (4 cores, 8 threads, 3.8 GHz), 31 GB, two NVMe SSDs in a RAID 1, ext4. CPU governor `performance` during the runs. The host's
mitigations (SMT is on) are in `environment.txt` of the results. The machine is a build host: nothing else ran during the runs (`host_state` in every result file records the
load and any other container).

## Workloads

PostgreSQL 18.6 (image pinned by digest), the container limited to 2 CPUs and 2 GiB, `shared_buffers=512MB`, `fsync=on`, `synchronous_commit=on`, `max_wal_size=4GB`, the
same settings for all runtimes. pgbench scale 100 (about 1.5 GB), 16 clients, 4 jobs, from a client container (runc, pinned to two threads that the servers do not use) over the bridge:

- tpcb-like read/write, 5 minutes, 3 runs; select-only, 3 minutes, 3 runs; a single client that inserts and commits (one WAL flush per transaction), 1 minute, 3 runs.
  Per run: tps and the latency percentiles from pgbench's per-transaction log (p99 is computed from every transaction, not from an average).
- `pg_test_fsync` in the container, 5 seconds per test.
- The time from `docker run` to the first query that works (the first start includes `initdb`), and after `docker restart`.
- The memory of the host that the instance costs, idle and under load: the PSS of every process of the runtime (daemons, shims, VMMs, sentry and gofers, virtiofsd, the container's
  own processes) with the container minus the same sum without it. Also the container's cgroup `memory.current` where there is one.
- Density: N = 1, 8 and 16 idle PostgreSQL containers at the same time and what the host spends per instance.

nginx (image pinned by digest, one worker, no access log), the container limited to 1 CPU and 256 MiB, three static files baked into the image (1 KB, 100 KB, 10 MB), `wrk` (2 threads) at 50
and 200 connections for 60 seconds, 3 runs each: requests per second, p50 and p99, the CPU time of the runtime's processes per 1000 requests, the idle memory and the time to the first answer.

The nginx files: baked into the image (served from the container's root file system) for runc, gVisor, Kata (virtio-fs) and cchv (its erofs image); for `kata-clh-blk-direct` the same three files (copied out
of the image, checked with sha256 after the copy) on an ext4 on a block device, mounted read-only at `/usr/share/nginx/html` by the entrypoint. The page cache is warm in every run, so the storage path
matters little once a file is in memory, except where each request still has to ask the file system (virtio-fs, see the 1 KB rows).

The data volume: for runc, gVisor and Kata a directory of the host's file system bind-mounted into the container (for Kata it is shared by virtio-fs); for cchv a raw block device (a logical
volume that the VM formats, with direct I/O); for `runc-loop` a directory on an ext4 that lives on the same logical volume kind. The volume group is on a loop device with direct I/O over a
sparse file on the host's ext4, so **cchv's volume has a loop device and the host's file system under it**: the `runc-loop` row shows what that layer costs for a runtime that does not matter.

## Limits and things to know

- **One machine, one kind of disk**, an older CPU generation with SMT on. Other hardware will give other numbers.
- Under Docker, cchv builds **one erofs image per container** (under Kubernetes the key is the image's and pods of one image share it). This is in the start-up times of cchv and in the disk it
  uses, not in the memory counted here. The shim's cache was moved to the disk (`20-start.sh`) because `/run` is a small tmpfs.
- cchv's network device offers **no checksum or segmentation offload** to the guest (the virtio feature bits of `eth0` show none), so with one vCPU a large response is limited by the guest's
  CPU: this shows in the 100 KB and 10 MB nginx rows and not in PostgreSQL. It is a property of cchv at this version, not of the method.
- **A file written without `fsync` is not there after a normal container stop under cchv** (the VM is released without a `sync` of the volume); runc, gVisor and Kata keep it because they write
  through to the host's file system. See `stopcheck` below. A database that fsyncs is not affected. The restart times of cchv are taken after a `sync` for that reason.
- Kata and cchv do not get the same VM from the same limits. Kata adds its defaults to the container's limits: for 2 CPUs and 2 GiB it makes a VM of 3 vCPUs and 2080 MiB, for 1 CPU and 256 MiB a VM of
  2 vCPUs and 288 MiB (Cloud Hypervisor's own `vm.info` says so). cchv makes 2 vCPUs and 2112 MiB for the first, and for the second it hands out a ready VM of 1 vCPU and 320 MiB from its pool. The numbers of
  CPU-bound tests (nginx with small files) are affected by that: Kata has one vCPU more.
- Kata with Cloud Hypervisor selects its configuration by the file `runtime-rs/configuration.toml` (a symlink); the handler name and `KATA_CONF_FILE` did not select it (the second is refused unless the
  file is one that Kata ships). `12-prereqs.sh` and `rtbench_blk.py` set the symlink. **Check which VMM runs** (`pgrep -a qemu-system` / `cloud-hypervisor`) before trusting a "Cloud Hypervisor" label.
- gVisor is not given a block device either: its data directory is a bind mount.
- Kata's virtual machine defaults (2048 MiB, 1 vCPU, resized from the container's limits) and gVisor's defaults were left as they are: nothing was tuned for one runtime and not for another.
- Memory is measured as PSS of processes. It does not count the host's page cache, which differs between a bind mount, virtio-fs and a direct-I/O block device; the cgroup number includes the
  page cache of a container where the runtime has a cgroup.
- A short run (`--quick`) exists for trying the scripts and is not used for numbers.

## How to repeat it

On a bare-metal host with `/dev/kvm`, Debian-like, as a user that may sudo:

```
./05-copy-cchv.sh HOST DIR SHA_FILE   # from the machine that has the cchv release files and their verified checksums
./10-install.sh                       # Docker, gVisor, Kata (downloads, checks the checksums that exist, writes versions.txt)
./12-prereqs.sh                       # erofs-utils, lvm2, the shims on the PATH of the benchmark's containerd
./30-loopvg.sh 48G                    # the loop-device volume group
./20-start.sh                         # the second Docker, its containerd, the cchv daemon
python3 rtbench.py prepare            # images, the client containers
./run-all.sh                          # everything, about 5 hours; or: python3 rtbench.py pg runsc | nginx runsc | density runsc 8 | start runsc | stopcheck runsc
python3 summarize.py RESULTS_DIR      # the tables
./21-stop.sh; ./90-cleanup.sh         # take it all away (keeps the results)
```

Results: one JSON file per run (with the raw per-run values) and the raw pgbench and wrk output next to it, `environment.txt` (versions, digests, configuration, mitigations), `run.log`.
