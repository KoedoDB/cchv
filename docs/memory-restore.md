# Memory restore: `Copy` (default) and `OnDemand`

When the daemon restores a VM from a snapshot, Cloud Hypervisor (v52 and later) takes `memory_restore_mode`:

| Mode | What happens | Needs |
|---|---|---|
| `Copy` (**default**) | The snapshot is read into guest RAM, then the VM resumes. | Nothing from the host kernel. |
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
