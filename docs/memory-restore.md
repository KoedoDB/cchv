# Memory restore: `Copy` (default) and `OnDemand`

When the daemon restores a VM from a snapshot, Cloud Hypervisor (v52 and later) takes `memory_restore_mode`:

| Mode | What happens | Needs |
|---|---|---|
| `Copy` (**default**) | The snapshot is read into guest RAM, then the VM resumes. | Nothing from the host kernel. |
| `CopyOnWrite` | The snapshot memory file is mapped copy-on-write; pages come from the page cache when touched (no userfaultfd). Opt-in; **no gain measured in the daemon** (below). | Nothing from the host kernel. |
| `OnDemand` | Guest memory is not read first; a page is read from the snapshot when the guest touches it (userfaultfd). | userfaultfd for the Cloud Hypervisor process. |

## Why `Copy` is the default

- It works on any host kernel. The CI VM (Flatcar) has no `userfaultfd` syscall at all (`Failed to create userfaultfd (os error 38)`).
- It was the smaller one when measured (Cloud Hypervisor v53.0, 2026-10-15): a restored VM at about 40 MiB resident with `Copy`, about
  137 MiB of a 128 MiB guest with `OnDemand`. We have not found out why lazy loading does not help; **not measured again with v54.0**.
- The benchmark numbers in `performance.md` are taken with `Copy`.

## Using `OnDemand`

Set `CLOUDHV_MEMORY_RESTORE_MODE=OnDemand` in the daemon's environment. Then:

- The kernel must have the `userfaultfd` syscall. If it does not, the daemon logs a warning once and uses `Copy`.
- The Cloud Hypervisor runs as a user of its own (`ch_rootless`) without `CAP_SYS_PTRACE`, so the host needs
  `vm.unprivileged_userfaultfd=1`. With `0` the restore fails: `Failed to create userfaultfd, Operation not permitted (os error 1)`
  (seen 2026-10-16 on a 7.0 kernel). A sysctl file for it is in `docs/90-cloudhv-userfaultfd.conf`; **it is not installed by the
  sysext**.
- The security cost: with `1`, every unprivileged process on the host can use userfaultfd, which has been a helper in kernel race
  exploits. The guest's own seccomp profile denies it (the guest cannot reach it), but the host sees the change. Do not turn it on
  for a host that runs customer code without a reason.

## `CopyOnWrite`: measured, and no gain in the daemon

Cloud Hypervisor v54.0 accepts `memory_restore_mode: "CopyOnWrite"` (`CLOUDHV_MEMORY_RESTORE_MODE=CopyOnWrite`), measured 2026-10-16:

- Cloud Hypervisor alone, private (non-shared) guest memory, no hotplug: restore about 11 ms and about 12 MiB resident, against about
  220 ms and 128 MiB of anonymous memory with `Copy`.
- The same with a memory hotplug region (`hotplug_size`, as `vm.resize` of the memory needs): it loads everything again (about
  110-140 ms, 128 MiB anonymous). `max_vcpus` alone does not do that.
- In the daemon there is no difference. Pods of 256 MiB / 1 vCPU, nginx: `Copy` start 0.61 s, 58 MiB idle; `CopyOnWrite` 0.66 s,
  58 MiB. node: 0.88 s / 163 MiB and 0.91 s / 163 MiB. The restore step is 35-37 ms with both. The daemon's snapshot memory is
  shared memory (memfd), which is why even `Copy` leaves a pooled VM at about 41 MiB resident; the single-VM figures above are for
  private memory. The default stays `Copy`.
