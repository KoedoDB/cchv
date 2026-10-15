# The shim: what it limits, and how to set it

The shim (`containerd-shim-cloudhv-v1`) runs as root on the host, once per pod. It reads files the pod's runtime made and builds
images from the customer's root file system, so it limits what it will do. The limits are read from the same file as the daemon's
runtime config (`/opt/cloudhv/config.json`); every field below is optional.

| Field | Default | Meaning |
|---|---|---|
| `max_memory_mb` | 16384 | The largest VM (the pod's memory limit plus the guest's 64 MiB) the shim starts. A pod asking for more does not start. The pod's limit is its own to set, and the scheduler counts only requests. |
| `max_vcpus` | 0 (host CPUs) | The most vCPUs of a VM. A higher CPU limit gets this many (its cgroup quota limits the time anyway). Combined with `max_default_vcpus` (the smaller non-zero one). |
| `erofs_cache_max_mib` | 4096 | Size of the cache of erofs images in `/run/cloudhv/erofs-cache`. Images nobody uses are evicted, oldest first; one a running sandbox uses never is. If the cache is full of images in use, or the rootfs alone is bigger than the cache, the container does not start. |
| `mkfs_erofs_path` | `/usr/local/bin/mkfs.erofs`, then `/usr/sbin/mkfs.erofs` | Absolute path of `mkfs.erofs`. |
| `mkfs_erofs_timeout_secs` | 300 | `mkfs.erofs` (and what it started) is killed after this long. |
| `volume_source_roots` | `/var/openebs/local`, `/var/lib/rancher/k3s/storage` | Where persistent volumes are made. See below. |
| `rpc_timeout_secs` | 30 | A request to the daemon. (`WaitContainer` has none.) |
| `acquire_timeout_secs` | 120 | `AcquireSandbox` and `AddContainer`, which may boot a VM of its own size. |
| `require_pod_cgroup` | true | The pod's Cloud Hypervisor must be placed in the pod's cgroup, or the pod does not start (the VM would run outside the pod's limits). Turn off only where the pod's cgroup is not visible to the shim, such as a test cluster inside a container. |

## Volumes

A pod's bind mounts (`resolv.conf`, `hosts`, ConfigMaps, Secrets, the service account token, emptyDirs, PVCs) are read by the shim on
the host and copied into the VM when the container starts (`src/volumes.rs`).

- Only sources below the kubelet's `/var/lib/kubelet/pods`, containerd's CRI sandbox directories (containerd and k3s) and
  `volume_source_roots` are read. Any other bind source (a `hostPath` that got past admission, `/etc`, the kubelet's PKI) is refused
  and **the container does not start**.
- The tree is walked with file descriptors; each entry is opened with one `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` from the
  volume's own directory, so a link swapped in while it is read cannot lead out of the volume (a PVC is a directory the customer
  writes to). Links that stay inside, such as the kubelet's `..data`, work. Where the kernel has no `openat2`, the same is done by hand.
  Only regular files are read; FIFOs, sockets and devices are skipped.
- A volume over 512 files or 2 MiB, or one that cannot be read, **fails the container's start** instead of being skipped (starting
  without it would make the app write to the VM's own disk and lose the data). Only `/dev/termination-log` is optional.
- The contents are a copy taken at start. A ConfigMap updated later, and a service account token the kubelet rotates, are **not**
  updated in the VM; files the container writes are not written back.

## The erofs cache key

The key is made from what is mounted at the container's `rootfs` (`src/cachekey.rs`): for an overlay mount the list of its
`lowerdir`s in stacking order, for another mount the file system, device and directory, for a plain directory its device, inode and
times; plus the image's digest if containerd gives one. If none of that can be read the container does not start. Two pods of one image
share a key; a tag that was moved cannot put one image's files under another's key.

## Exec (`kubectl exec`, exec probes)

containerd's `Exec` task calls are served by the vendored shimkit (`third_party/containerd-shimkit`, see its `KOEDODB.md`): it keeps the
exec table of each task with a small state machine (created, starting, started, exited, deleting), checks the order of the calls, and
publishes `TaskExecAdded`, `TaskExecStarted` and `TaskExit` (with the exec id as `id`). It calls the optional `exec_*` methods of
the `Instance`; this shim implements them in `crates/shim/src/exec.rs` by asking the daemon, which asks the guest agent. The process
runs in the guest, in the container's own root file system, namespaces and cgroup.

**The daemon's JSON-line contract.** Every request has `vm_id`, `container_id` (the pod container the exec belongs to) and `exec_id`.
Ids are `[A-Za-z0-9_-]{1,64}` (the shimkit accepts `.` and up to 128 characters in an exec id; the daemon may be stricter).

| Method | Extra request fields | Answer |
|---|---|---|
| `ExecCreate` | `process_json` (base64 of the OCI `Process`), `terminal` | `{}` |
| `ExecStart` | | `{"pid": N}` |
| `ExecKill` | `signal` | `{}` |
| `ExecWait` | | blocks until the process exits: `{"exit_status": N, "exited_at_unix_nanos": N}` (no RPC timeout) |
| `ExecDelete` | | `{}` |
| `ExecWrite` | `data` (base64) | `{"written": N}` |
| `ExecCloseStdin` | | `{}` |
| `ExecRead` | `stdout_offset`, `stderr_offset`, `max` | `{"stdout", "stderr"` (base64)`, "stdout_offset", "stderr_offset", "eof", "dropped"}` |
| `ExecResize` | `width`, `height` | `{}` |

An error is `{"error": "..."}`. The offsets of `ExecRead` are cumulative byte positions and acknowledge what the caller has taken (as
for `ReadLogs`); with a terminal all output is on `stdout`.

**Streams.** After `ExecStart` the shim polls `ExecRead` (right away while data flows, then every 20 ms doubling up to 100 ms) and writes
to the stdout and stderr pipes containerd made, with the same timeouts as the log pump; a stream with no pipe is read and dropped. What
containerd's client writes to the stdin pipe is forwarded with `ExecWrite`; the end of input is `CloseIO`, which first forwards what
is in the pipe and then sends `ExecCloseStdin`. An exec whose stdin pipe has no writer (a probe) does not wait for input. When the
process exits, the shim reads until `eof`, and only then reports the exit to containerd (which takes it as the end of the streams).

**Limits.** At most 64 exec processes per task; process spec at most 1 MiB; each read or write at most 64 KiB; `Exec` is refused for a task
that is not running and for the sandbox (pause) container.

**Not done.** `ExecRead` is polled, not streamed, so an interactive terminal has up to 100 ms of latency when idle; `Pids` does not list
exec processes; the exec's resource limits are those of the container.

## Terminals (`tty: true`)

A container whose process has a terminal (`tty: true` in the pod; `kubectl run -it`, `kubectl attach`) is created with the task's
`terminal` flag; the shim sends it as the boolean `terminal` in `AcquireSandbox` and `AddContainer`, the daemon puts it in the
agent's `CreateContainerRequest.terminal`, and the agent forces `process.terminal` in the container's spec to it (the request's flag
decides, not the JSON's, as for an exec) and starts the container with `crun run --console-socket`. crun makes the pty and sends its
master over the socket (SCM_RIGHTS) before the process runs; the agent keeps the master.

- **Output**: read from the master into the container's log buffer, as the **stdout** stream (a terminal has no stderr), and carried to the
  stdout pipe containerd made by `pump_logs` as before: `kubectl logs` of a tty container works. The end of the output is reported after
  what was written to the terminal has been read (up to 3 s after the process exits).
- **Input**: if containerd made a stdin pipe (the pod has `stdin: true`), the shim forwards what is written to it with `ExecWrite`, and
  `CloseIO` with the daemon's `ExecCloseStdin` (a terminal has no end of input: accepted, nothing done; the client sends ^D). A pod with
  `tty: true` and no stdin (the common case) has no stdin pipe, and nothing is forwarded.
- **Size**: `ResizePty` of the task is `ExecResize`; the size is set with `TIOCSWINSZ` on the master. Refused for a task with no
  terminal or one that is not running.
- **The reserved exec id `init`** names the container's own process in `ExecWrite`, `ExecCloseStdin` and `ExecResize`, so that no new
  calls were added between shim, daemon and agent. `ExecCreate` refuses the id in the daemon and in the agent; `ExecStart`, `ExecKill`,
  `ExecWait`, `ExecDelete` and `ExecRead` mean nothing for it, and the hardening of exec processes is unchanged.

**Attach.** `kubectl attach` does not talk to the shim: containerd's CRI plugin attaches clients to the FIFOs it made at Create (it reads
the stdout FIFO for the log file and the attached clients, and writes the stdin FIFO for them). So it works exactly as far as the FIFOs do:
the container needs `stdin: true` for a client to send input, and `tty: true` for a terminal. Not tested with a real client in this tree.

