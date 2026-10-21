# Sandbox Daemon Design

## Status: Implemented

The sandbox daemon is fully implemented and tested. All four implementation stages are complete. Figures that we measured are in the
[README](../README.md#numbers-measured-by-us); this document describes the design (the figures of the original project's runs are not repeated here: we did not reproduce them).

## Problem

Today, every pod start pays for VM boot + agent connect (~175ms with async
eager boot, 15s+ under 50-VM contention). The shim is ephemeral — spawned
per pod by containerd, destroyed when the pod dies. It cannot pre-warm VMs
because it doesn't exist before `RunPodSandbox`.

Previous optimizations (async eager boot in v0.12.0) overlapped ~90ms of
boot with containerd's internal processing, but could never eliminate the
cost entirely. Under burst scale (150 pods), all VMs competed for host CPU
during boot, inflating per-pod latency to seconds. The daemon architecture
eliminates this entirely by pre-booting VMs in a pool.

## Proposal: Sandbox Daemon

### Cloud Hypervisor Version Requirement

The daemon requires **Cloud Hypervisor >= v51.0** for the `OnDemand`
memory restore mode ([PR #7800](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7800)).
This enables userfaultfd-based lazy memory population during snapshot
restore, which reduces restore time from **277ms to 18ms** (15× faster)
and idle VM memory from **517 MB to 6 MB** per pool VM.

Without `OnDemand` mode (CH v44 and earlier), restore copies the entire
512 MB guest memory file into RAM synchronously. With `OnDemand`, memory
pages are loaded on demand via userfaultfd — only pages actually accessed
by the guest are read from the snapshot file.

| Metric | CH v44 (eager) | CH v51+ (OnDemand) |
|--------|---------------:|-------------------:|
| vm.restore | 277ms | **18ms** |
| Idle pool VM RSS | 517 MB | **6 MB** |
| Minor page faults | 131,512 | 546 |
| Pool of 10 VMs memory | ~5.2 GB | **~60 MB** |

A long-running systemd daemon on each node that:

1. **Pre-boots a pool of clean VMs** from a base snapshot (kernel + agent,
   no containers, no workload)
2. **Maintains warm workload snapshots** per container image
3. **Serves ready-to-use VMs to shims** via a Unix socket API

```
                    ┌─────────────────────────────────┐
                    │  cloudhv-sandbox-daemon          │
                    │  (systemd, long-running)         │
                    │                                  │
                    │  Base snapshot:                  │
                    │    kernel + agent (clean state)  │
                    │                                  │
                    │  Pool (restored from base):      │
                    │    VM-1: agent ready, clean ✓    │
                    │    VM-2: agent ready, clean ✓    │
                    │    VM-3: agent ready, clean ✓    │
                    │                                  │
                    │  Workload snapshots:             │
                    │    python-runtime: warm ✓        │
                    │    http-echo: warm ✓             │
                    │                                  │
                    │  /run/cloudhv/daemon.sock        │
                    └───────────┬─────────────────────┘
                                │
                    ┌───────────┴─────────────────────┐
                    │  containerd-shim-cloudhv-v1      │
                    │  (per-pod, ephemeral)             │
                    │                                  │
                    │  start_sandbox:                  │
                    │    TAP setup (0ms)               │
                    │    store TAP info                │
                    │    return (7ms)                  │
                    │                                  │
                    │  start_container:                │
                    │    daemon.AcquireSandbox()       │
                    │    receives pre-booted VM (<1ms) │
                    │    hot-plug rootfs (20ms)        │
                    │    RunContainer RPC (4ms)        │
                    │                                  │
                    │  delete:                         │
                    │    daemon.ReleaseSandbox()       │
                    │    VM destroyed, pool refilled   │
                    └─────────────────────────────────┘
```

## Architecture

### Daemon Responsibilities

The daemon owns:

- **CH process lifecycle** — spawn, boot, shutdown, destroy
- **Base snapshot** — created once at daemon startup from a clean VM boot
- **VM pool** — maintained by restoring from base snapshot (CoW memory)
- **Workload snapshots** — per-image warm snapshots (experimental)
- **Agent connections** — pre-connected, health-checked
- **Pool replenishment** — async restore after each acquire

The daemon does NOT own:

- **Networking** — TAP/tc setup stays in the shim (requires pod netns)
- **Container lifecycle** — RunContainer, logs, kill, wait stay in the shim
- **Rootfs conversion** — erofs cache stays in the shim
- **OCI spec handling** — the shim adapts specs and sends RPCs to the agent

### Shim Changes

The shim becomes a thin client for VM lifecycle:

#### Image Key Strategy

The image key identifies "same workload" for warm snapshot reuse. The shim
resolves the container image tag to a content-addressed digest using
`containerd-client` v0.8.0 (gRPC call to containerd's ImageService). The
digest is then hashed with FNV-1a 128-bit to produce a stable hex key.

```
busybox:latest → containerd gRPC → sha256:b3255e7d... → FNV-1a → e15434cbacd68a2d60dbb91d7f060194
```

This makes the key immune to tag mutation — if `latest` is repushed with
different content, the digest changes and a new snapshot is created. The
daemon's mtime check on the erofs file provides a secondary safety net.

Fallback chain:
1. **Content digest** (via containerd gRPC) — preferred, content-addressed
2. **Image tag hash** — if gRPC fails
3. **Rootfs path hash** — for non-CRI callers

```
start_sandbox:
  1. Set up TAP device in pod's network namespace
  2. Store TAP name, MAC, IP, gateway for later
  3. Return immediately (no VM interaction)

start_container (first container):
  1. Compute erofs image (cache hit or mkfs.erofs)
  2. Compute snapshot key from image identity
  3. Call daemon.AcquireSandbox(tap, mac, ip, gw, snapshot_key)
  4. Daemon returns: VM handle (api_socket, vsock_socket, cid, ch_pid, restored)
  5. Move CH process to pod cgroup
  6. If not restored: hot-plug rootfs disk, RunContainer RPC
  7. If restored: ConfigureNetwork RPC, skip RunContainer (workload alive)

delete:
  1. Kill container via agent RPC
  2. Call daemon.ReleaseSandbox(vm_id)
  3. Daemon destroys the VM
  4. Clean up TAP device
```

### Why AcquireSandbox in start_container, not start_sandbox

`start_sandbox` doesn't know the container image. The decision between
"warm workload restore" vs "clean pool VM" depends on the image identity,
which is only available in `start_container`. By deferring the acquire to
`start_container`, the daemon can make the optimal choice:

- **Workload snapshot exists** → restore from it (workload already running)
- **No snapshot** → hand out a clean pool VM (agent ready, hot-plug rootfs)

### Pool Management

```
Base VM Snapshot (created once at daemon startup):
  - Boot VM with guest rootfs only (/dev/vda)
  - Wait for agent to connect and respond to health check
  - Pause → snapshot → destroy
  - This is the "golden" clean state

Pool (maintained by daemon):
  - Target: pool_size VMs always ready (default: 3)
  - Each pool VM: restored from base snapshot (CoW, ~300ms)
  - VMs boot without networking (no TAP, no IP)
  - Agent already running and connected (from snapshot)

On AcquireSandbox:
  1. Pop a VM from the pool
  2. Configure its network (ConfigureNetwork RPC with pod IP)
  3. Return VM handle to shim
  4. Async: restore another VM from base snapshot to replenish pool

On ReleaseSandbox:
  1. Destroy the VM (shutdown + kill CH process)
  2. No recycling — clean VMs are cheap from snapshot

When the daemon stops (SIGTERM) — `keep_vms_on_stop`, on by default:
  1. The VMs waiting in the pool are stopped; the VMs handed to pods keep running (the unit has `KillMode=process`, so systemd does not
     end the Cloud Hypervisors with the daemon). Off: every VM is stopped, as before.
When the daemon starts (also after a SIGKILL, when systemd starts it again):
  1. The registry (`<state_dir>/.ctl/<vm_id>/`, root's) says which VMs were handed to pods (`active`, `cid`; `blocks.list` for the devices of
     persistent volumes). A VM is taken over if it is recorded, its process is that VM's Cloud Hypervisor (the command line names the VM's
     `--api-socket`; rootless, it runs as the VM's user) and it answers on its API socket. The counter, the CIDs, the VM's user, its cgroup,
     the disks of its emptyDirs and the locks of its devices are taken over (a lock lives as long as the file that holds it, so it is taken
     again); the connection to the guest's agent is made again before the API is opened.
  2. Every other VM left by the earlier daemon is stopped, as before, and so is what the start sweeps (cgroups, processes of the VMs' users, ACL
     entries, outboxes, emptyDir disks): except for the VMs taken over.
  3. A taken-over VM that no shim speaks about for `adopted_claim_secs` (600) is released: its pod is gone. The shims try for five minutes
     while the daemon is away, and ask about their VMs at once when it is back.
  A daemon that did not record this (an older one) stops every VM when it stops; the first replacement is from it.
  The host going down takes every VM with it (the registry is in `/run`).

Pool sizing:
  - pool_size: target idle VMs (default: 3)
  - max_pool_size: hard cap on idle VMs (default: 10)
  - If pool is empty: synchronous restore from base snapshot (blocking)
  - replenish_delay_ms: wait before spawning replacement (default: 100)
  - vm_idle_timeout_secs: destroy idle VMs after timeout (default: 300)
```

### Warm Workload Snapshots via Shadow VMs

Today (v0.11.0), warm snapshots are taken by pausing a **live production
VM** — the pod that's serving actual traffic. This causes:

- ~200-500ms of request downtime while paused
- Risk of TCP connection resets (peers time out)
- Kubelet may restart the container if liveness probe fires during pause

The daemon eliminates this by using **shadow VMs** — dedicated throwaway
instances that exist solely to produce warm snapshots, outside of
Kubernetes entirely.

#### Shadow VM Flow

```
First AcquireSandbox(image_key="python-runtime"):
  1. No workload snapshot exists for this image
  2. Daemon returns a clean pool VM → shim starts container normally
  3. Pod serves traffic immediately — never paused, never disrupted

Meanwhile (async, background):
  4. Daemon spawns a shadow VM:
     a. Restore from base snapshot (clean VM, agent ready)
     b. Hot-plug the container rootfs (same erofs image)
     c. RunContainer RPC (workload starts inside shadow)
     d. Wait warmup_duration (configurable, default 30s)
     e. Pause shadow VM → snapshot → destroy shadow
     f. Cache workload snapshot under image_key
  5. Shadow VM lifecycle: ~35s total, fully async

Next AcquireSandbox(image_key="python-runtime"):
  6. Workload snapshot exists → restore from it
  7. VM wakes up with Python already running, server listening
  8. ConfigureNetwork RPC (new IP) → traffic flows immediately
```

#### Shadow VM Properties

| Property | Value |
|----------|-------|
| Managed by containerd? | No — daemon-internal only |
| In a pod cgroup? | No — lives in daemon's cgroup |
| Has networking? | No — no TAP, no IP, no CNI |
| Serves traffic? | No — purely for snapshot creation |
| Lifetime | ~35s (boot + warmup + snapshot + destroy) |
| Impact on live pods | Zero — completely isolated |

#### Benefits Over Live-Pause Snapshots

- **Zero production disruption** — live pods are never paused
- **Longer warmup** — can wait 60s+ for full JIT/model loading since no
  user traffic is affected
- **Safer** — snapshot failure doesn't impact any running workload
- **Simpler error handling** — if shadow VM crashes, just retry later
- **Configurable warmup** — per-image warmup duration in daemon config

#### Shadow VM Networking

Shadow VMs boot without networking because:

1. The workload binds `0.0.0.0:<port>` — it listens on all interfaces
2. No interface exists during warmup, but the bind still succeeds on `0.0.0.0`
3. On restore, the shim configures the real network via ConfigureNetwork RPC
4. The kernel sees the new interface, workload accepts on it automatically

If a workload requires a network interface during startup (e.g. connects
to an external service on init), the daemon can create a dummy interface
with a non-routable IP for the shadow VM's warmup period.

#### Snapshot Cache Management

```json
{
  "snapshot_cache": {
    "max_total_size_gb": 10,
    "max_snapshots": 20,
    "eviction_policy": "lru",
    "warmup_duration_secs": 30,
    "per_image_overrides": {
      "python-runtime-sandbox": {
        "warmup_duration_secs": 60
      }
    }
  }
}
```

### Resource Accounting

#### Idle Pool VMs

Pool VMs live in the daemon's cgroup until acquired:

```yaml
# Daemon DaemonSet spec
resources:
  requests:
    memory: "2Gi"      # pool_size × default_memory_mb + daemon overhead
  limits:
    memory: "4Gi"      # headroom for replenishment
```

Kubelet sees pool VMs as daemon system overhead, not pod usage.

#### Acquired VMs

When a shim acquires a VM:

1. Shim receives the CH process PID from the daemon
2. Shim moves the CH PID to the pod's cgroup:
   `place_in_pod_cgroup(ch_pid, pod_cgroup_path)`
3. Kubelet now accounts the VM's memory/CPU to the pod
4. Daemon's cgroup usage drops by that VM's footprint

#### CoW Memory Sharing

All pool VMs share physical pages via CoW from the base snapshot.
The kernel's memory accounting correctly attributes:

- Shared pages: counted once (in the first VM's cgroup that faulted them)
- Private pages: counted per-VM (unique state after fork)

This is the same behavior as today's warm restore — no new accounting
challenges.

### Daemon API

```protobuf
service SandboxDaemon {
  // Acquire a pre-booted VM. The daemon selects the optimal source:
  // warm workload snapshot (if available) or clean pool VM.
  rpc AcquireSandbox(AcquireRequest) returns (AcquireResponse);

  // Release a VM back to the daemon for destruction.
  rpc ReleaseSandbox(ReleaseRequest) returns (ReleaseResponse);

  // Pool and snapshot status for monitoring.
  rpc Status(Empty) returns (StatusResponse);
}

message AcquireRequest {
  string tap_name = 1;
  string tap_mac = 2;
  string ip_cidr = 3;
  string gateway = 4;
  string netns = 5;
  string image_key = 6;     // erofs cache key for workload snapshot lookup
  uint32 vcpus = 7;         // 0 = use default
  uint64 memory_mb = 8;     // 0 = use default
}

message AcquireResponse {
  string vm_id = 1;
  string api_socket = 2;
  string vsock_socket = 3;
  uint64 cid = 4;
  uint32 ch_pid = 5;
  bool from_snapshot = 6;    // Was this a warm workload restore?
}

message ReleaseRequest {
  string vm_id = 1;
}

message ReleaseResponse {}

message StatusResponse {
  uint32 pool_ready = 1;
  uint32 pool_target = 2;
  uint32 active_vms = 3;
  uint32 shadow_vms_running = 4;   // Shadow VMs currently warming up
  repeated string snapshot_keys = 5;
}
```

### Daemon Configuration

```json
{
  "pool_size": 3,
  "max_pool_size": 10,
  "replenish_delay_ms": 100,
  "vm_idle_timeout_secs": 300,
  "default_vcpus": 1,
  "default_memory_mb": 320,
  "cloud_hypervisor_binary": "/usr/local/bin/cloud-hypervisor",
  "kernel_path": "/opt/cloudhv/vmlinux",
  "rootfs_path": "/opt/cloudhv/rootfs.erofs",
  "socket_path": "/run/cloudhv/daemon.sock"
}
```

### Installer Changes

The installer DaemonSet additionally:

1. Installs `cloudhv-sandbox-daemon` binary to `/usr/local/bin/`
2. Installs **Cloud Hypervisor >= v51.0** (built from source or from a
   future release). The current installer downloads CH v44 from GitHub
   releases. For the daemon, the installer must either:
   - Download a pre-built CH v51+ binary from the project's own GHCR
     (built in CI from CH main), or
   - Build CH from source during the installer image build (adds ~2 min
     to image build but guarantees version alignment)

   The recommended approach: **build CH from source in the installer
   Dockerfile** and pin to a specific CH commit SHA:
   ```dockerfile
   ARG CH_COMMIT=fc79d08  # CH main with OnDemand restore
   RUN git clone --depth 1 https://github.com/cloud-hypervisor/cloud-hypervisor.git /tmp/ch && \
       cd /tmp/ch && git fetch origin ${CH_COMMIT} && git checkout ${CH_COMMIT} && \
       cargo build --release && \
       cp target/release/cloud-hypervisor /opt/cloud-hypervisor && \
       rm -rf /tmp/ch
   ```

   Once CH cuts a release containing PR #7800, switch to downloading the
   release binary as before.

3. Creates a systemd unit:
   ```ini
   [Unit]
   Description=CloudHV Sandbox Daemon
   After=containerd.service
   Requires=containerd.service

   [Service]
   ExecStart=/usr/local/bin/cloudhv-sandbox-daemon \
     --config /opt/cloudhv/daemon.json
   Restart=always
   KillMode=process
   MemoryMax=4G

   [Install]
   WantedBy=multi-user.target
   ```
4. Writes `/opt/cloudhv/daemon.json` with pool configuration
5. The shim config adds: `"daemon_socket": "/run/cloudhv/daemon.sock"`
6. When `daemon_socket` is set, the shim uses the daemon API instead of
   spawning CH directly

### Measured Performance

See the [README](../README.md#numbers-measured-by-us) and [performance.md](performance.md): what a VM costs, the start times, and the tests of a database.
| Daemon RSS | N/A | **~2.2 MB** |
| Shim RSS | ~5.5 MiB | **~6 MB** |
| Base snapshot creation | N/A | **~1s** (cold boot + agent wait + snapshot) |
| Shadow VM lifecycle | N/A | **~33s** (30s warmup + 3s snapshot) |
| Pool restore (per VM) | N/A | **25ms** |

### Implementation Plan

All stages are complete. The daemon was built and validated independently
of the shim before integration.

#### Stage 1: Daemon Core ✅

Daemon binary with ttrpc API, base snapshot creation, VM pool management,
and `AcquireSandbox`/`ReleaseSandbox` RPCs. Tested via dedicated test client.

#### Stage 2: Shadow VM Snapshots ✅

Warm workload snapshots via shadow VMs. On first acquire for an unknown
image key, the daemon returns a pool VM and spawns a shadow VM in the
background to produce a warm snapshot. Subsequent acquires restore from it.

#### Stage 3: Shim Integration ✅

Shim modified to use daemon API when `daemon_socket` is configured.
`start_sandbox` stores TAP info; `start_container` calls `AcquireSandbox`;
`delete` calls `ReleaseSandbox`. Falls back to direct CH spawn when daemon
is unavailable.

#### Stage 4: AKS Benchmark and Production Hardening ✅

Installer updated with daemon binary, systemd unit, and daemon config.
The 150-pod benchmark of the original project is not repeated here. Remaining work:

- **CH v51 release dependency** — currently building from source (main branch)
  until CH cuts a release containing OnDemand restore
  ([PR #7800](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7800))
- **erofs-rs** — planned in-process erofs conversion to eliminate the
  `mkfs.erofs` subprocess dependency

### Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Daemon crash loses pool state | Systemd restarts daemon; pool rebuilds from base snapshot in seconds |
| Pool drain under burst | Fallback to synchronous restore (same latency as today) |
| Memory waste from idle pool | `vm_idle_timeout_secs` destroys unused VMs; DaemonSet `resources.limits` caps total |
| Stale base snapshot after upgrade | Daemon recreates base snapshot on startup; installer clears snapshot cache |
| Daemon socket unavailable | Shim falls back to direct CH spawn (v0.12.0 behavior) |

### Open Questions

1. **Pool sizing heuristics**: Static `pool_size` or dynamic based on
   recent pod creation rate?
2. **Multi-tenant isolation**: Should different RuntimeClasses get separate
   pools (e.g. different memory sizes)?
3. **Snapshot garbage collection**: LRU eviction? Max total cache size?
   Per-image TTL?
4. **Hot-standby for networking**: Can we pre-create TAP devices and
   pre-assign IPs to pool VMs for even faster acquire?
5. **Integration with Kubernetes scheduler**: Can we expose pool capacity
   as an extended resource so the scheduler avoids overcommitting nodes?

---

## Addendum: Test Plan

The daemon's ttrpc API provides a clean boundary for testing. A test client
can drive the full lifecycle without containerd, Kubernetes, or CRI — just
connect to the Unix socket and call RPCs.

### Unit Tests

Testable in isolation with mocked CH processes:

| Test | Validates |
|------|-----------|
| Pool initializes to `pool_size` VMs | Base snapshot creation + restore loop |
| AcquireSandbox returns a VM with valid api_socket, vsock_socket | Response fields populated correctly |
| AcquireSandbox decrements pool count | Pool bookkeeping |
| Pool replenishes after acquire (async) | Replenishment triggers and completes |
| AcquireSandbox blocks when pool is empty | Synchronous fallback restore |
| ReleaseSandbox destroys VM (CH process exits) | Cleanup path |
| ReleaseSandbox with invalid vm_id returns error | Error handling |
| Status RPC reports correct pool_ready / active_vms | Monitoring accuracy |
| Daemon startup with no existing base snapshot | Creates base snapshot from cold boot |
| Daemon startup with existing base snapshot | Reuses cached base snapshot |
| VM idle timeout destroys unused pool VMs | Resource reclaim |
| Pool respects max_pool_size cap | No unbounded growth |
| Config reload updates pool_size | Dynamic reconfiguration |

### Integration Tests (Daemon + CH + Agent)

These run against real Cloud Hypervisor with KVM. Require a host with
`/dev/kvm` access (hl-dev or CI runner with nested virt).

#### Lifecycle Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Acquire and use** | AcquireSandbox → hot-plug disk → RunContainer → verify workload runs → ReleaseSandbox | Full lifecycle |
| **Consecutive acquire/release** | Repeat acquire→release 10 times | No state leaks, pool replenishment |
| **Concurrent acquire** | 10 parallel AcquireSandbox calls | Pool drain + synchronous fallback |
| **Acquire with custom resources** | AcquireSandbox(vcpus=2, memory=1024) | Custom VM sizing |
| **Release during acquire** | AcquireSandbox (blocks, pool empty) → cancel | Graceful cancellation |
| **Daemon restart** | AcquireSandbox → daemon restarts → ReleaseSandbox | Orphan VM cleanup |
| **Daemon crash recovery** | Kill daemon process → restart → Status | Pool rebuilds from base snapshot |

#### Shadow VM Snapshot Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Shadow snapshot creation** | AcquireSandbox(image_key=X, no snapshot) → wait → verify snapshot exists | Shadow VM lifecycle |
| **Snapshot restore** | Create snapshot → AcquireSandbox(same image_key) → verify from_snapshot=true | Workload snapshot hit |
| **Shadow VM failure** | Shadow VM crashes during warmup → verify no snapshot cached, no impact on live VMs | Fault isolation |
| **Shadow VM networking** | Verify shadow VM has no TAP, workload still binds 0.0.0.0 | Networkless warmup |
| **Snapshot eviction** | Fill cache to max_snapshots → verify LRU eviction | Cache management |
| **Concurrent shadow VMs** | Multiple images trigger shadows simultaneously | No resource conflicts |
| **Per-image warmup duration** | Configure 60s for image A, 10s for image B → verify timing | Warmup configuration |

#### Resource Accounting Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **Cgroup migration** | AcquireSandbox → verify CH PID in daemon cgroup → shim moves to pod cgroup → verify | Accounting handoff |
| **Pool memory accounting** | Start daemon → verify pool VMs count against daemon cgroup | System overhead tracking |
| **CoW memory sharing** | Acquire 10 VMs → measure total RSS vs 10 × per-VM RSS | CoW deduplication |
| **Daemon resource limits** | Set MemoryMax=2G → fill pool → verify OOM kills daemon not host | Resource containment |

#### Network Configuration Tests

| Test | Steps | Validates |
|------|-------|-----------|
| **ConfigureNetwork on acquired VM** | AcquireSandbox → daemon calls ConfigureNetwork → verify guest has IP | Post-acquire networking |
| **IP conflict detection** | Acquire two VMs with same IP → verify error or correct isolation | Safety |
| **Network cleanup on release** | AcquireSandbox → configure → ReleaseSandbox → verify TAP cleaned | No network leaks |

### Performance Tests

Run on identical infrastructure to enable comparison with previous
benchmarks and Kata Containers.

#### Single-Pod Latency (hl-dev, D8s_v5, KVM)

Measure wall-clock time from AcquireSandbox call to workload serving HTTP:

| Scenario | Metric | Target | Comparison |
|----------|--------|--------|------------|
| Pool hit (base VM) | AcquireSandbox latency | <5ms | v0.12.0: 7ms sandbox + 26-110ms container |
| Pool hit + hot-plug + RPC | Container ready | <25ms | v0.12.0: 26-110ms |
| Pool empty (sync restore) | AcquireSandbox latency | <350ms | v0.12.0: 120-200ms total |
| Workload snapshot hit | Container ready (warm) | <10ms | v0.12.0 warm: 300ms |
| Pool replenish time | Time to refill one slot | <400ms | Background, non-blocking |

Run each scenario 20 times to reduce noise. Report p50, p95, p99.

#### Scale Benchmark (not run by us yet)

Scale 150 Pods to Ready, scale down to 0, and the memory of the nodes, on identical nodes; not measured by us on this version.

Protocol:
1. Deploy daemon with `pool_size: 10` on each node
2. Wait for pools to fill (Status RPC shows pool_ready = pool_size)
3. Scale deployment from 0 → 150
4. Record time to all pods Ready (readiness probe with startup probe, 1s period)
5. Capture node memory via `free -m` (VMSS run-command)
6. Scale 150 → 0, record time to all pods terminated
7. Repeat 3 times, report median

#### Agent Sandbox Benchmark (AKS, Python workload)

Compare Python sandbox startup with and without daemon:

| Test | Without Daemon | With Daemon (cold) | With Daemon (warm) |
|------|---------------|-------------------|-------------------|
| SDK create → sandbox ready | ~13s | Target: <5s | Target: <3s |
| Consecutive runs (10x) | 10/10 pass | 10/10 pass | 10/10 pass |
| VM uptime at first exec | 20s (cold boot) | ~0.5s (pool) | ~50s (snapshot clock) |

#### Contention Benchmark

The primary value of the daemon — pre-booted pools eliminate boot contention:

| Concurrent pods | v0.12.0 agent_connect | With Daemon |
|----------------|----------------------|-------------|
| 1 | 170ms | <5ms |
| 10 | ~500ms | <5ms |
| 50 | ~5s | <5ms (pool) / ~350ms (drain) |
| 150 | ~15s | <5ms (pool) / ~350ms (drain) |

Protocol:
1. Pre-fill pool to 50 VMs
2. Launch N pods simultaneously (kubectl scale)
3. Measure per-pod agent_connect time from TIMING logs
4. Compare with Kata's per-pod boot time at same concurrency

### Edge Cases

| Edge Case | Expected Behavior |
|-----------|-------------------|
| Daemon not running when shim starts | Shim falls back to direct CH spawn (v0.12.0 behavior) |
| Daemon socket exists but daemon is dead | Shim detects connection failure, falls back |
| Pool drains completely during burst | Synchronous restore (same latency as v0.12.0 warm restore) |
| Base snapshot corrupted | Daemon recreates from cold boot on startup |
| Workload snapshot restore fails | Daemon falls back to pool VM (cold boot path) |
| CH binary upgraded while daemon running | Daemon restart required (systemd handles) |
| Guest rootfs upgraded while pool VMs exist | Daemon invalidates pool and base snapshot on rootfs mtime change |
| Shadow VM OOM during warmup | Shadow destroyed, snapshot not cached, retry on next acquire |
| Node memory pressure | Daemon reduces pool_size via idle timeout; kubelet can evict daemon pod |
| Daemon and shim version mismatch | ttrpc API versioned; daemon rejects incompatible clients |
| Multiple images trigger shadow VMs simultaneously | Daemon caps concurrent shadows (e.g. max 2) to avoid resource exhaustion |
| AcquireSandbox with vcpus/memory that don't match pool VMs | Daemon bypasses pool, creates custom VM synchronously |

## Rootless Cloud Hypervisor

Cloud Hypervisor is what a guest attacks when it escapes the VM, so it need not be root. With `"ch_rootless": true` in the daemon's
config the daemon (still root: it makes TAP devices, enters network namespaces and sets up tc) starts every Cloud Hypervisor as a
user that exists for that VM alone (`ch_uid_base`, `ch_uid_count`; default 200000, 60000 of them), in the group of `/dev/kvm`
(`ch_gid` to say another), with no capabilities, `no_new_privs`, no core file, a limit on its processes (`ch_max_processes`), and
Cloud Hypervisor's own seccomp filter. Off by default.

**The rule behind all of it: once the guest is compromised, that Cloud Hypervisor is hostile and owns everything it can write.** It can
delete and re-create any file in its VM's state directory, so root never trusts, follows or acts on anything in such a directory.

- **Control data is out of reach.** The daemon's records of a VM (its Cloud Hypervisor's pid, its user, the files it was given read
  access to) are in `<state_dir>/.ctl/<vm_id>/` (0700 root; `ctl.rs`), not in the VM's own directory. A process is signalled only if
  it is verified to be that VM's Cloud Hypervisor (its command line has `--api-socket <state_dir>/<vm_id>/api.sock`; rootless, its user
  is the VM's user), through a pidfd, and never pid 0 or 1 or a number that is not a `pid_t`. The user of a VM is kept in the daemon's
  memory.
- **Root works on descriptors** (`fsutil.rs`): paths are opened with `openat2(RESOLVE_NO_SYMLINKS)` (or `O_NOFOLLOW`), looked at with
  `fstat`, and changed with `fchown`/`fchmod`/`fsetxattr` on the descriptor. The sockets of a VM (`api.sock`, `vsock.sock`) are
  checked to be sockets, not links, before the daemon connects. The console socket (`console.sock`) is the same, and its other end is checked (`SO_PEERCRED`).
- **A user is never given to a second VM while a process of the first is left.** Freeing a VM's user first kills every process that
  has that user (`/proc` is scanned until none is left); a user that cannot be cleared is *quarantined* (not used again until the daemon
  restarts). At start the daemon kills the processes of every user in the range and takes every ACL entry of a user of the range off
  the images and snapshots (those of an earlier daemon that was killed). The range must not contain a user of `/etc/passwd`.
- **The TAP device is not opened by Cloud Hypervisor**: enabling a TAP needs `CAP_NET_ADMIN`, which would let it change the host's
  network. The daemon opens the device (with the virtio-net header) and passes the descriptor over the API socket (SCM_RIGHTS).
  Without `ch_rootless` the device is opened by name, as before.
- **The container's erofs image is root-only** (0600, set by the shim); a VM is given read access with a POSIX ACL entry for its user
  and nobody else's (the daemon sets the ACL attribute itself; no `setfacl` is needed). One lock covers every read-modify-write of
  an ACL, the file is chmod-ed only when it has no ACL yet, and the grant is written to the VM's record before it is made, so that
  it can always be taken back. A grant is only made on a regular file under `erofs_cache_dir` or the snapshot cache, never on a path in
  a VM's own directory.
- **A snapshot is the work of a VM, and is not trusted until that VM is dead.** The VM writes it into a directory made for it; then
  the VM is destroyed (its process and its user's processes gone); only then is the directory handed over (`hand_over_snapshot`: on
  descriptors, links, devices, FIFOs, sockets, files with a second hard link and anything deeper than 4 levels are removed). The base
  snapshot (a fresh guest) goes to the VMs' group (0640 root:gid) and is marked complete (`.complete`) by the daemon; one without the
  mark is made again. A workload snapshot holds a customer's image warmed up in memory, so it is root's alone (0600) and a VM that
  restores from it is given read access to its two big files by ACL.
- **A snapshot's `config.json` is checked, not trusted** (`validate_snapshot_config`): only the keys of Cloud Hypervisor v54 are
  accepted; no `fs`, `pmem`, `devices`, `user_devices`, `vdpa`, `generic_vhost_user`, `tpm`, `numa`, `iommu`, `net`, `gdb`,
  `ivshmem`, `preserved_fds`, `landlock_rules`; the kernel is the daemon's; no initramfs, firmware, `fw_cfg`; memory has no zones;
  disks are read-only raw and only the platform's rootfs and the pod's own image (whose path is *replaced* by the daemon's, so the
  path the snapshot names is never used); consoles are off, a file or a socket (the daemon sets the path); `rng` is `/dev/urandom`. A snapshot that fails is dropped and the
  pod gets a VM from the pool.
- The kernel (`kernel_path`) and the base rootfs (`rootfs_path`) must be world-readable; the daemon refuses to start otherwise.

Restore with **`OnDemand`** (userfaultfd) needs `vm.unprivileged_userfaultfd=1` for a process without `CAP_SYS_PTRACE`; the default `Copy`
mode does not.

### Confinement (`ch_confine`, on by default with `ch_rootless`)

A user of its own with no capabilities still sees the host: its network (127.0.0.1 services, the management network), its processes,
every world-readable file, and the group of `/dev/kvm` shared by all VMs. So each Cloud Hypervisor is started by a **launcher**, a mode
of the daemon's own binary (`cloudhv-sandbox-daemon --ch-launcher`; `confine.rs`), which is given its instructions as JSON on standard
input (the program, the user, the paths to show it, the cgroup) and puts the Cloud Hypervisor in:

- **its own mount namespace, with a root of its own.** A new tmpfs, read-only after the `pivot_root`, holding only: the Cloud
  Hypervisor binary (and the system libraries `/lib`, `/lib64`, `/usr/lib` if it is dynamically linked), the kernel and the base rootfs
  (read-only), the directory of container images `erofs_cache_dir` (read-only; the files in it are 0600 and readable only by the VM
  given them), the VM's own state directory and its outbox (read-write), `/dev/kvm`, `/dev/null`, `/dev/zero`, `/dev/urandom`, a new
  `/proc`, and a 16 MiB `/tmp`. Paths are the same as on the host, because the VM's configuration names host paths. More read-only paths
  can be added with `ch_extra_ro_paths`. It cannot see the other VMs' directories, the registry (`.ctl`), the snapshot caches, the
  daemon's socket, `/home`, `/etc`;
- **its own PID namespace**: it is process 1 there and sees nothing else; when it ends, everything in the namespace ends;
- **its own network namespace with no interface**: the TAP descriptor is passed over the API socket, which names the device whatever
  namespace the opener is in (checked: `vm.add-net` with the descriptor works in an empty namespace, and the TAP stays where the daemon
  put it, then moves to the pod's namespace as before). 127.0.0.1 services and the management network are unreachable;
- **its own IPC and UTS namespaces**;
- **a cgroup of its own** (`cgroup.rs`), `<ch_cgroup_root>/<vm_id>`: `memory.max` = the guest's memory + `ch_memory_overhead_mib`
  (256), `memory.swap.max` 0, `pids.max` = `ch_max_processes`, and (`ch_cpu_limit`) `cpu.max` of the vCPUs plus one thread. The launcher
  joins it before it starts anything. Destroying a VM writes `cgroup.kill` (Linux 5.14; each pid in `cgroup.procs` otherwise) and removes
  it; the scan for the processes of the VM's user remains as the second way. If the cgroup cannot be made (no cgroup v2, or the memory
  and pids controllers cannot be turned on in a parent that has processes of its own) the daemon says so once at start and VMs run
  without limits, unless `ch_require_cgroup` is set;
- **a group of its own** (the number of its user), with the group of `/dev/kvm` as the only supplementary group: that opens `/dev/kvm`
  and reads the base snapshot (0640 root:kvm), the two things all VMs share. Everything else a VM makes is its own user's and group's;
- no capability (the bounding set is emptied), `no_new_privs`, `RLIMIT_CORE` 0, `RLIMIT_NPROC`, an empty environment, and Cloud
  Hypervisor's own seccomp filter.

**The supervisor.** The process the daemon starts, signals and waits for is the launcher, not Cloud Hypervisor: a process that makes a
PID namespace stays outside it, and its first child is process 1 inside. The launcher drops to the VM's user as soon as it has forked
Cloud Hypervisor and then only supervises, in a single thread (`signalfd` and `poll`; a process that has made a PID namespace cannot
make threads): it forwards `SIGTERM`/`SIGINT`/`SIGHUP`/`SIGQUIT` to Cloud Hypervisor, copies what Cloud Hypervisor writes on standard
error into `<state_dir>/.ctl/<vm_id>/launcher.log` (the first 64 KiB; the rest is discarded), and exits with its status. If the
launcher is killed, the kernel kills Cloud Hypervisor (parent-death signal, set after the change of user, which would clear it).
The launcher's command line carries `--api-socket <the VM's socket>`, which is how the daemon checks that a pid is a VM's before it
signals it. If a VM does not start, `wait_ch_ready` adds the log to the error.

**Snapshots through an outbox.** A confined VM sees no directory but its own, so it cannot write a snapshot to where it will live.
`vm.snapshot` goes to the VM's **outbox**, `<state_dir>/.out/<vm_id>/` (owned by the VM's user, bound read-write into its root).
After the VM is destroyed, the daemon moves the outbox to the snapshot's place (`collect_outbox`: a rename inside the state
directory), and only then hands it over and reads it, as before. An outbox that is not collected goes with the VM; at start all are removed.

Checked on the development machine with the real daemon (`scripts/test-ch-confinement.sh`, 68 checks): each Cloud Hypervisor has
its own mount, PID, network, IPC and UTS namespaces; a user and a group of its own and only the kvm group besides; `CapEff` and
`CapBnd` 0; `NoNewPrivs`; the root holds `dev proc tmp` and the bound paths only (no `/home`, `/etc/shadow`, other VMs, `.ctl`); the
root is read-only; it is process 1 and sees one process; the network namespace has only `lo` and 127.0.0.1 services are unreachable;
it is in a cgroup with `memory.max` and `pids.max`; SIGTERM to the launcher ends both; stopping the daemon (with `keep_vms_on_stop` off, as that check was made) leaves no launcher, no process
of the VMs' users, no cgroup, no VM directory, no outbox; the base snapshot is 0640. And by hand with a pod's network namespace: a
confined VM got its TAP as a descriptor in an empty network namespace, TC redirects were set in the pod's namespace, the guest was
configured and a container started; the shadow VM made a workload snapshot through its outbox (0600 root), and a second pod was
restored from it **warm, confined, with read access by ACL to that VM's user only**; after release the ACL entries were gone.

**Not done.** `/sys` is not shown (nothing needed it with Cloud Hypervisor v54.0; add what a newer version needs with
`ch_extra_ro_paths`); no user namespace (the launcher needs real root to build the root and the namespaces, and the VM's user is a real
user of the host, kept apart by the user ID range, the groups, the ACLs and the namespaces); no `landlock` (Cloud Hypervisor v54 can
take rules for itself; the mount namespace does the same job); the cgroup root is not delegated by systemd (the unit should have
`Delegate=yes` and the cgroup root should be below the unit's cgroup, or the controllers turned on in a parent without processes);

## Hardening of the API

The daemon is root and its socket is the one door:

- the socket is 0600 root (made with a umask that leaves no gap) and a connection whose peer is not root (`SO_PEERCRED`) is closed.
  The directory of the socket is not made 0700: the state directory is usually below it and a VM's user has to pass through it
  (the state directory is 0711: enter, not list, so a VM cannot read the names of the others);
- a request line is read up to `max_request_bytes` and no further; at most `max_connections` connections; a connection with no
  request for 5 minutes is closed; calls to Cloud Hypervisor time out (30 s, 120 s for snapshot and restore);
- `vm_id`, `container_id` and the keys of images are `[A-Za-z0-9_-]{1,64}`; `netns` is `/var/run/netns/<name>`, `/run/netns/<name>` or
  `/proc/<pid>/ns/net` and, opened, must be a network namespace on `nsfs` that is not the daemon's own; `erofs_path` must be a regular
  `<name>.erofs` file directly in `erofs_cache_dir`, reached without a link; memory and vCPUs are limited (`max_memory_mb`,
  `max_vcpus`), and so are the VMs handed to pods (`max_active_vms`) and the shadow VMs (`max_shadow_vms`);
- **the warm-snapshot key is made by the daemon** from the SHA-256 of the image file's content (`imagekey.rs`); the `image_key` the
  caller sends is ignored. Two pods share a warm snapshot only if their images are the same bytes;
- `ReleaseSandbox` only touches a VM that was handed out; for any other name it answers ok and does nothing.

### New configuration fields

| field | default | |
|---|---|---|
| `ch_rootless` | `false` | run each Cloud Hypervisor as a user of its own |
| `ch_uid_base`, `ch_uid_count` | `200000`, `60000` | the users of the VMs; must be 1000 or more, must not overflow, must not contain a user of `/etc/passwd` |
| `ch_gid` | group of `/dev/kvm` | the group of the Cloud Hypervisors |
| `ch_max_processes` | `1024` | `RLIMIT_NPROC` of a rootless Cloud Hypervisor's user (threads count) |
| `ch_confine` | `true` | with `ch_rootless`: also the namespaces, the cgroup, the group of its own and the root of its own described above. `false` returns to a user of its own and nothing more |
| `ch_cgroup_root` | `/sys/fs/cgroup/cloudhv` | the cgroup (v2) directory under which each VM gets one; must be below `/sys/fs/cgroup` |
| `ch_require_cgroup` | `false` | refuse to start when the cgroup limits cannot be had |
| `ch_memory_overhead_mib` | `256` | what a VM's cgroup allows beyond the guest's memory |
| `ch_cpu_limit` | `false` | limit a VM's CPU time to its vCPUs plus one thread |
| `ch_extra_ro_paths` | `[]` | paths shown read-only in every confined root |
| `serial_console` | `true` | make the guest's console (the virtio-console: the kernel's messages and the agent's log; the kernel has no serial port, so `console=hvc0`) a Unix socket in the VM's state directory (`console.sock`) |
| `console_forward` | `true` | with `serial_console` on, read the socket and log each line to the daemon's log (journald) as `console[vm=<VM name>] <line>`; the lines are made printable, with a budget for each VM. A log collector (Vector) reads journald. Off, the console is off |
| `console_forward_interval_ms` | `1000` | how often the daemon looks for a console it does not read (after it restarted) |
| `console_forward_max_lines_per_sec` | `200` | the most lines a VM may forward in a second (a burst of five times that); the rest are counted and told as one line ("N lines dropped") |
| `console_forward_max_line_bytes` | `2048` | the longest line forwarded; a longer one is cut |
| `console_forward_max_bytes_per_sec` | `2097152` | the most bytes the daemon reads from one VM's console in a second; the rest of the second it does not read, and the Cloud Hypervisor drops what the guest writes |
| `volume_block_prefixes` | `[]` (off) | persistent block volumes: the device-mapper names a Pod may be given as a disk (an LVM volume `pv-x` of the group `vg` is `vg-pv--x`); a device whose name begins with none of them is refused. Needs `emptydir_dir` |
| `volume_block_format_if_blank` | `false` | a volume that asks for `"format": "if-blank"` gets an ext4 made on it when its first MiB is zeros; off, the platform makes the file system before the Pod starts |
| `disk_direct` | `true` | open the hot-plugged disks (emptyDirs on disk, persistent volumes) with O_DIRECT: the guest has its own page cache and the host does not cache every block a second time (about 260 MiB less charged to a database Pod of 1 GiB, no loss of write speed in our test) |
| `vm_balloon` | `false` | give each VM a balloon with free page reporting, so its resident memory follows what the guest uses |
| `erofs_cache_dir` | `/run/cloudhv/erofs-cache` | where the shim keeps the images; must be the shim's `EROFS_CACHE_DIR` |
| `emptydir_dir` | `/var/lib/cloudhv/emptydir` | where the disks of emptyDirs are made (the host's disk, not `/run`); empty turns them off and a request for one is refused. See "emptyDir disks" |
| `emptydir_max_mib`, `emptydir_reserve_mib` | `16384`, `2048` | the largest disk a request may ask for; a disk is refused when the free space of `emptydir_dir` is below `size/4 + reserve` |
| `mke2fs_path` | `/usr/sbin/mke2fs` | makes the file system on the file (falls back to `/sbin`, `/usr/bin`, `/usr/local/bin`; it must be e2fsprogs', not BusyBox's, which makes ext2 only) |
| `max_memory_mb`, `max_vcpus` | `65536`, `64` | the most a request may ask for |
| `max_active_vms` | `512` | VMs handed to pods |
| `keep_vms_on_stop` | `true` | the VMs handed to pods keep running when the daemon stops, and the next daemon takes them over; `false` stops them (the unit needs `KillMode=process` for the first) |
| `adopted_claim_secs` | `600` | a VM taken over from an earlier daemon that no shim speaks about for this long is released (must be longer than the shim's five minutes of trying again) |
| `max_shadow_vms` | `2` | shadow VMs (warm-up) at once; a shadow that failed for an image is not retried for 5 minutes |
| `max_request_bytes` | `8388608` | the longest request line (volumes of a pod are in it; the agent's own limit is 4 MiB) |
| `max_connections` | `64` | |

### emptyDir disks

The request's `volumes` may carry `"disk": {"id": "<32 hex>", "size_mb": N}` (the shim sends it for an emptyDir on the host's disk; see `docs/shim.md`). `AcquireSandbox` and `AddContainer` check it first (`emptydir::parse`: the id is 32 lower-case hex characters, the size between 16 MiB and `emptydir_max_mib`, at most 16 disks, a repeated id is one disk, the feature must be on; anything else is `bad request (volumes)`), and then, before the container runs:

1. The VM's directory `<emptydir_dir>/<vm name>/` was made when its Cloud Hypervisor was spawned: root's, 0711, empty. A confined Cloud Hypervisor has *that directory only* bound into its root, writable (never the parent, so it sees no other VM's disks). It can write the file it is given and cannot create, rename or remove a name in it, so there is no race for a name between root and the VM.
2. For a disk the VM does not have yet (a file `<id>.img` in the directory), root checks the free space, creates the file (`O_EXCL | O_NOFOLLOW`, 0600) and sets its size (sparse), runs `mke2fs -t ext4 -O ^has_journal -m 0 -E lazy_itable_init=1,nodiscard` on it (60 s limit, empty environment), and only then gives it to the VM's user (`rootless::give`). The host never mounts it. The disk is `vm.add-disk` with `"serial": "ed" + the first 16 hex of the id` (virtio-blk serials are 20 bytes), `readonly: false`.
3. The `VolumeMount` goes to the agent as a `BLOCK` volume (`fs_type` `ext4`, the `serial`, `source` `/run/cloudhv-vols/<id>`). A disk already in the VM is not made again, whatever size a second container asks for (the first wins).

The directory is removed when the VM is destroyed (`shutdown_and_destroy`, and a failed spawn), and everything in `emptydir_dir` is removed when the daemon starts (the VMs of an earlier daemon are stopped by then). Limits to know: the space of disks already made is not reserved (a host whose disks are all written fills up; the guest then gets I/O errors on that disk, and the page cache of the file is charged to the VM's cgroup), the disk is not rate limited, and `emptydir_dir` should be on a disk of its own or at least not the one `/var/lib/kubelet` and the images are on if the pods are not trusted.

The daemon keeps its records in `<state_dir>/.ctl/` (0700 root), the outboxes of confined VMs in `<state_dir>/.out/` (0700 root; each VM's own is its user's), and the state directory is made 0711.

## The guests' consoles in the log (`consolefwd.rs`)

With `serial_console` and `console_forward` on, each VM's console (the kernel's messages and the agent's log; the agent is the init) goes through a Unix socket the Cloud
Hypervisor makes in the VM's state directory (`console.sock`; the `console` device in mode `Socket`). The daemon connects to it and logs what it reads as
`console[vm=<VM name>] <line>` (target `console`). There is no file: the console takes no disk and no memory of the host, and nothing is rotated.

(It was a file first, `console.log`. The Cloud Hypervisor does not open a file for append, so it could not be cut with `copytruncate`; the forwarded part was dropped with a
hole instead, and the file still grew with whatever the guest wrote in between. A socket has no such file.)

- **While nobody reads the socket, the Cloud Hypervisor drops what the guest writes** (measured with v54.0: a guest that wrote 8 MB with no reader was not held up; one
  with a slow reader lost about a sixth; one with a fast reader lost nothing). So the daemon connects before the guest runs: `attach` after `vm.create` (before `vm.boot`)
  and after `vm.restore` (before `vm.resume`), and a scan every `console_forward_interval_ms` finds the consoles it does not read (after the daemon restarted). What a VM says
  while the daemon is down is lost.
- The lines are text from an untrusted guest: made printable (escape sequences, control characters and carriage returns taken out, invalid UTF-8 replaced, a line cut at
  `console_forward_max_line_bytes`), and always under the VM's name, which the daemon gives.
- Each VM has a budget of `console_forward_max_lines_per_sec` lines a second (a burst of five times that); the lines over it are counted and told as one line. Past
  `console_forward_max_bytes_per_sec` the daemon does not read for the rest of the second (the Cloud Hypervisor drops the rest), so a guest cannot take the daemon's time.
- A connection is read to its end: what a VM said just before it died is in the log.
- The socket is looked at first (a link is not followed), and the process that listens on it must have the VM's user (`SO_PEERCRED`; root without `ch_rootless`). The daemon
  only reads.
- A snapshot taken with the console in a file or on a socket is restored with it on a socket in the new VM's directory; the path the snapshot names is never used.
- The first lines of the kernel (`Linux version …`, the memory map) are not in the log: the guest kernel gives the virtio-console only what it prints after the device is registered (about `virtio_blk virtio1`). The same with a file; it needs a serial port or `earlycon`, which the guest does not have.
- Checked on a host (2026-10-20): the VMs of the pool, restored from the snapshot, and the VMs booted for a Pod are read from the start of the boot or the restore; no `console.log` is left.

## Persistent block volumes (`blockvol.rs`)

A database in a Pod's VM needs a disk that outlives the VM. A raw block device the platform made (an LVM logical volume, given to the Pod as a `volumeDevices`
entry of a PersistentVolumeClaim with `volumeMode: Block`) is hot-plugged into the VM as a virtio-blk disk with direct I/O, and the guest agent mounts the ext4 on it.
The Pod says what the device is for in an annotation:

```yaml
metadata:
  annotations:
    cloudhv.koedodb.com/block-volumes: '[{"device":"/dev/pgdata","mount":"/var/lib/postgresql","format":"if-blank"}]'
spec:
  runtimeClassName: cloudhv
  containers:
  - name: pg
    image: postgres:18.6-trixie
    volumeDevices:
    - {name: data, devicePath: /dev/pgdata}
```

- **Which device**: the shim sends the device numbers from the OCI spec (`linux.devices`), no path. The daemon accepts a device-mapper device whose name begins with one of
  `volume_block_prefixes` and whose UUID is an LVM one; a Pod cannot be given the host's own disks.
- **One VM at a time**: root holds an exclusive `flock` on the device while its VM lives; a second VM that asks for it is refused. The kernel drops the lock if the daemon dies.
- **Node**: root makes a node of the device in the VM's directory (`mknod`, 0600), gives it to the VM's user and hot-plugs it. That directory is the one bind of the confined root
  that is not `nodev`; the VM's user cannot make a node there.
- **File system**: only ext4 (the guest kernel has no other). It is made only for `"format": "if-blank"` with `volume_block_format_if_blank` on, and only on a device whose
  first MiB is zeros. ext4 can be grown while mounted, not shrunk.
- **Set up on the host**: the containerd runtime needs `pod_annotations = ['cloudhv.koedodb.com/*']`, or the annotation does not reach the shim; the daemon's unit needs
  `CAP_MKNOD` and `DeviceAllow=block-device-mapper rwm`. **LVM does not zero a new logical volume**: whatever the disk held before is still there, so zero a new volume
  (`blkdiscard --zeroout`) before a Pod is given it.
- Tested on a host (Debian 13, a PostgreSQL 18 Pod in a VM of 1 GiB on a logical volume of 8 GiB): the data survives the Pod being made again; a second Pod on the same volume,
  and a volume that is not allowed, are refused; `kill -9` of the Cloud Hypervisor under `pgbench`, several times, with no broken invariant (`pg_amcheck` clean); a full disk gives
  `ERROR`, not `PANIC`; and after a hard reset of the host in the middle of writes every acknowledged insert was there (a software reset: it does not show what the drive's
  cache does in a power cut).
