# Performance

Only figures we measured ourselves are in this file and in the [README](../README.md#numbers-measured-by-us), on our machines (a 12-core AMD development machine and a
bare-metal Xeon E3-1270 v6 server), with Cloud Hypervisor v54.0 (the section on the default VM size: v53.0). They are not benchmarks of your hardware. Figures of the original
project (a 150-Pod run on Azure Kubernetes Service, a comparison with Kata, the time of the phases of a start) are not here: we did not reproduce them.

For what a VM costs, the start times, PostgreSQL against runc, direct I/O and what survives a `kill -9`, a full disk or a hard reset, see the README. Restoring a
snapshot (`Copy` and `OnDemand`): [memory-restore.md](memory-restore.md).

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
