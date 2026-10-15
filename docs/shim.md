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
