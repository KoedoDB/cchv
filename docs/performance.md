# Performance

> **The figures in this file are the original author's, from runs on Azure Kubernetes Service with 128 MB guests. We have not reproduced them** (on our machines a VM
> costs more, see the README), and they cover small guests only. Our own measurements are in the [README](../README.md#numbers-measured-by-us).

## Daemon Pool Acquire (single pod)

With the sandbox daemon, VMs are pre-booted in a pool via CH v51 OnDemand
restore. Acquiring a VM is near-instant.

| Scenario | Latency |
|----------|--------:|
| Pool hit (pre-booted VM available) | **0ms** |
| Pool empty (synchronous OnDemand restore) | **~25ms** |

## Cold Path (shim inner, daemon mode)

Measured on AKS (3 × D8ds_v5, Kubernetes v1.33.7, containerd 2.0.0):

| Phase | Time |
|-------|-----:|
| erofs conversion (cached) | 0ms |
| erofs conversion (uncached, mkfs.erofs) | ~8ms |
| daemon.AcquireSandbox (pool hit) | ~63ms |
| Hot-plug + RunContainer RPC | ~11ms |
| **Cold path shim inner total** | **~74ms** |

## Warm Snapshot Path (daemon mode)

When a warm workload snapshot exists (created by a shadow VM in the background):

| Phase | Time |
|-------|-----:|
| daemon.AcquireSandbox (warm snapshot restore) | ~24ms |
| Agent connect | ~80ms |
| ConfigureNetwork RPC | ~50ms |
| Container adoption | ~14ms |
| **Warm path total** | **~168ms** |

The workload wakes up **already running** — no kernel boot, no agent init,
no application startup.

## Scale Performance

| Metric | Daemon (current) | v0.11.0 (warm restore) | v0.7.0 |
|--------|------------------:|----------------------:|-------:|
| 150 pods → all Ready | **11s** | 77s | 27s |
| Scale-down 150 → 0 | 12s | 12s | — |
| Infra | 3 × D8ds_v5 | 3 × D8ds_v5 | 3 × D8ds_v5 |

## Resource Overhead

### Per-VM Memory

| Component | Value |
|-----------|------:|
| VM RSS (128 MB allocated, OnDemand userfaultfd) | **~24 MB** |
| Pool VM idle RSS | ~6 MB |
| Shim process RSS | ~6 MB |
| Daemon RSS (total, not per-VM) | ~2.2 MB |

### Node Memory at 150 Pods (3 nodes, 50 VMs per node)

| Metric | Value |
|--------|------:|
| Per-node VM memory | ~3.2 GB |
| Per-node memory utilization | **~10%** of 32 GB |
| Total cluster memory (3 nodes) | ~9.6 GB |

### Cache Sizes (per node)

| Cache | Size |
|-------|-----:|
| erofs cache (per unique image) | ~8.6 MB |
| Base snapshot (daemon state) | ~513 MB |

## Rootfs Delivery Performance

The shim caches rootfs as erofs images at `/run/cloudhv/erofs-cache/<hash>.erofs`,
content-addressed by image digest. flock-serialized for concurrent builds.

| Metric | Without Cache | With Cache |
|--------|--------------|------------|
| 50 concurrent containers (same image) | 22,459ms | **655ms** |
| Burst failures | ~3-15% | **0%** |
| Per-container disk creation | ~460ms | **~3ms** |
| Speedup | — | **153×** |

## Comparison with Kata Containers

Measured on identical AKS infrastructure (3 × D8ds_v5, 96 GiB total RAM):

| | containerd-cloudhypervisor | Kata Containers |
|---|---|---|
| **Cold start (shim inner)** | ~74ms | ~500ms–1s |
| **Warm restore** | ~168ms | N/A |
| **150-pod scale** | 150/150 in **11s** | 130/150 (OOM at 130) |
| **Memory per pod** | **~24 MB** | ~330 MB |
| **Node memory at 150 pods** | **10%** | 43%+ (OOM) |
| **Shim binary** | ~4.6 MB | ~65 MB |
| **Guest rootfs** | 5.4 MB (erofs) | ~257 MB |
| **Hypervisors** | Cloud Hypervisor only | CH, QEMU, Firecracker |
| **Architecture** | Block-device-only (no FUSE) | virtio-fs + block |
| **Language** | Rust | Go |

## Default VM size: 320 MiB (2026-10-16)

The default VM is 320 MiB, 1 vCPU. Measured inside the guest (`free -m`, idle): a 288 MiB VM has 265 MiB total and **240 MiB available**
(the kernel's reserved memory takes about 23 MiB, the kernel and the agent use about 18 MiB); a 320 MiB VM has 297 MiB total and
**271 MiB available**, so a workload can have 256 MiB. A `tmpfs` in the guest (`/dev/shm`, an emptyDir with `medium: Memory`) is
capped at half of the VM's memory by default (148 MB here); a disk emptyDir is not.

Pods restored from the warm snapshot (`Copy`, `ab -n 40000 -c 16`, no CPU limit, one run of 5 starts and 3 loads each):

| | VM | start (median) | idle | max under load | req/s |
|---|---|---|---|---|---|
| nginx | 128 MiB | 0.68 s | 56 MiB | 57 MiB | 13,029 |
| nginx | 256 MiB | 0.61 s | 58 MiB | 59 MiB | 12,871 |
| nginx | 320 MiB | 0.69 s | 60 MiB | 62 MiB | 11,390 |
| node | 128 MiB | 0.91 s | 129 MiB | 130 MiB | 7,317 |
| node | 256 MiB | 0.88 s | 163 MiB | 180 MiB | 7,642 |
| node | 320 MiB | 0.93 s | 166 MiB | 183 MiB | 7,408 |

The size of the VM changes the idle memory of a pod little (the pooled VM's memory is shared memory that is mostly not touched). The
differences in start time and req/s between the sizes are within the spread of the runs; they were not repeated enough to say more.
