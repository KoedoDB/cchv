# Results of the runtime comparison (October 2026)

One machine (Xeon E3-1270 v6, 4 cores / 8 threads, 31 GB, two NVMe SSDs in a RAID 1; `environment.txt` in each directory has the rest). How it was run: `../README.md`.

| directory | what is in it |
|---|---|
| `20261025-db001/` | runc, gVisor (20261019.0) and Kata 4.3.1 (QEMU row, Cloud Hypervisor with virtio-fs, and the two virtio-blk variants). Kata's guest kernel is **6.18.56**. **The cchv files of this run are not here**: that run used cchv v0.3.1, and its cchv measurements failed (the VM size was read from the wrong path, see `vm_sizes()` in `rtbench.py`), so they were removed instead of kept next to numbers that can be read. `run.log` still shows those failed steps. |
| `20261026-db001-cchv/` | cchv **v0.4.1** (guest kernel **6.18.58**, files checked against the release's checksums): PostgreSQL, nginx, density with 1 and 8 instances. Run with `run-cchv.sh`. |
| `SUMMARY.md` | `summarize.py` over the two directories together (the cchv rows from the second, the others from the first). |

The VM is the same size for Kata and cchv: PostgreSQL 3 vCPUs and 2080 MiB, nginx 2 vCPUs and 320 MiB. `rtbench.py` reads it from the VMM's command line after the start and stops otherwise; the sizes are in `vm_sizes` of every result file.

Not measured: density with 16 instances (the volume group had about 6 GiB free). Known problems of the tables: the "Start-up" table of `summarize.py` comes out empty (the first-query and restart times are in the PostgreSQL table); the nginx idle memory of cchv comes out negative (the difference of two PSS sums that are close), do not use it.
