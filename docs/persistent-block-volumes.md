# Persistent block volumes (design note, not built)

Status: a design note on the branch `persistent-block-volumes`. Nothing here is implemented. Written 2026-10-19, while the 24-hour test of v0.16 was running, so
that the work can start from the facts below once it has ended.

## What is wanted

A database in a pod's VM (PostgreSQL, for KoedoDB) needs a disk that **outlives the VM**, is **writable** and is **limited in size**. Today a VM's volumes are:

| Volume | How it gets in | Persistent |
|---|---|---|
| container image layers (erofs) | `vm.add-disk`, read-only, mounted by the guest agent | no (made again) |
| `emptyDir` (the default medium) | a sparse ext4 file in the VM's own directory, `vm.add-disk`, mounted by the guest agent (`crates/daemon/src/emptydir.rs`) | no: it goes with the VM |
| `emptyDir` with `medium: Memory` | a tmpfs in the guest | no |
| ConfigMap, Secret, a filesystem PVC | read by the shim, sent in the `CreateContainer` RPC, written into the guest | no, and read-only to the host |
| `hostPath` | refused | — |

The pieces that already exist and would be reused: `vm.add-disk` (the daemon's hot-plug), the serial number the guest agent finds a disk by (`serial_of`),
and the guest agent's mount of a block volume with a file system the host made (`Resolved::Block`, `BLOCK_FS_TYPES = ext4, xfs`).
`docs/architecture.md` (Volumes) lists "Block PVC (raw)" as persistent; the code has no path for it. That row is a plan, not a feature.

## The difficulty

Cloud Hypervisor is the process a guest attacks, so it runs rootless and confined (`crates/daemon/src/rootless.rs`): as a user that exists only for that VM, with no
capabilities, and with only the paths it needs in its root. The `emptyDir` disks work because they live in the VM's own directory, which is bound into that root
when the VM is spawned. A persistent volume is somewhere else (a directory under the PV root, or a block device), and **which volume a pod uses is known only after the
VM exists**: a VM comes out of the pool first and is given its containers afterwards. So a persistent disk has to be made reachable by a Cloud Hypervisor that is
already running, without opening the host to it.

## Options

**A. A device node in the VM's directory.** For a host block device (an LVM logical volume), the daemon (root) makes a block device node with the same major and minor
in the VM's directory, gives it to the VM's user, and hot-plugs it with `"path": "<that node>"`. The VM's user opens a node it was given; the directory is already bound
in. Open questions, to be tried on a real VM: whether the confined root (a user namespace, how its mounts are made) allows opening a device node (no `nodev` on the
bind, no device cgroup rule that refuses it); how the node is removed with the VM; that one device is never given to two VMs (a lock taken by the daemon).

**B. The PV root, bound into every VM's root.** The directory of the persistent volumes (`pv_roots`) is bound into every Cloud Hypervisor's root, and a volume's image
file (`koedodb-disk.img`) is given to one VM by an ACL, as the erofs images are. Cheaper to build, but it puts the **names** of every customer's volume in front of every
Cloud Hypervisor; mode 0711 on the directories and 0600 on the files keep the contents away, not the names. Not liked.

**C. An unconfined Cloud Hypervisor for such a pod.** Rejected: it lowers the wall for exactly the pods that hold the most.

## What a build would need (for A)

1. The shim: take a block device that kubelet gives a pod (`volumeDevices` appear as `linux.devices` of type `b` in the OCI spec; the host path is under the kubelet's
   plugin directory) or a local PV's device path from an allow list (`volume_block_roots`, nothing else), check it is a block device (not a link out of the roots,
   `openat2` with `RESOLVE_BENEATH`), and pass it as `"disk": {"device": ..., "fs": "xfs"|"ext4"}` in the volumes.
2. The daemon: a persistent disk request next to the `emptyDir` one: a device (not a size), a lock per device (`flock` on a file in a directory of root's), the node
   in the VM's directory, `vm.add-disk` with `direct` I/O, and the node removed (never the device) with the VM.
3. A file system on a device with none: **never made by default**. The platform makes it (a Job, before the pod), or a flag on the volume says `format: if-blank` and the
   daemon formats only a device that is blank (no signature at all) and is in the allow list.
4. The guest agent: the existing block volume mount, with `fs_type` and the options (`noatime`), `fsync` honest (the disk is not cached by the host: `direct=on`).
5. The spec filter (`crates/agent/src/spec.rs`) stays as it is: no device node reaches the container; the container sees a mounted directory.

## What to try first (on a real VM, after the endurance test)

- A throw-away LV (or a loop device) as the block device: make a node in the VM's directory, `vm.add-disk` with that path, see whether Cloud Hypervisor opens it,
  whether the guest sees `/dev/vdb`, and whether a kill of the VM leaves the device usable by the next one.
- `pgbench` and `fio` (direct, sync) in the guest against the same device on the host, to see the cost of the virtio-blk path with `direct=on`.
- Kill the VM during a write and mount the file system again (xfs log recovery) in a second VM.
- A device that two requests ask for together: the second is refused.

## What this note does not decide

Whether a database goes in a VM at all (`TODO.md` in the KoedoDB repository: it waits for these results and for the memory cost of a VM per Project), and how the
platform names and sizes the logical volumes (a CSI such as LVM-localpv, or the Regional Agent itself).
