## PostgreSQL 18.6 (16 clients, 4 jobs, scale 100; median of 3 runs, min–max in brackets)

| runtime | tpcb-like tps | tpcb p99 ms | select-only tps | select p99 ms | commit (1 client) tps | commit p99 ms | pgbench -i s | first query s | restart s |
|---|---|---|---|---|---|---|---|---|---|
| runc | 5,084 (5,081–5,085) | 38.1 (37.8–38.1) | 32,988 (31,578–33,044) | 0.75 (0.74–2.08) | 5,023 (5,001–5,121) | 0.89 (0.84–0.92) | 15.8 | 1.28 | 0.54, 0.53, 0.55 |
| gVisor | 2,036 (1,985–2,039) | 37.2 (36.8–37.7) | 12,686 (12,310–12,735) | 34.02 (28.03–34.27) | 3,337 (3,271–3,448) | 0.59 (0.48–0.69) | 21.1 | 3.10 | 0.83, 0.82, 0.81 |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 2,893 (2,864–2,953) | 35.8 (35.5–35.8) | 19,463 (17,178–19,799) | 2.60 (2.50–3.77) | 4,040 (4,008–4,051) | 0.44 (0.43–0.46) | 22.1 | 4.41 | 1.56, 1.53, 1.56 |
| Kata + Cloud Hypervisor (virtio-blk volume, O_DIRECT) | 3,319 (3,305–3,364) | 33.5 (33.0–33.7) | 19,605 (19,002–19,705) | 2.50 (2.31–3.49) | 3,733 (3,369–3,815) | 0.49 (0.42–0.94) | 16.1 | 2.73 | 1.58, 1.51, 1.55 |
| Kata + Cloud Hypervisor (virtio-blk volume, Kata's default cache mode) | 3,192 (3,186–3,224) | 35.4 (35.4–35.6) | 20,018 (19,456–20,126) | 2.20 (2.11–2.68) | 3,866 (3,767–3,876) | 0.43 (0.41–0.59) | 17.2 | 2.92 | 1.5, 1.49, 1.51 |
| cchv + Cloud Hypervisor | 3,167 (3,139–3,207) | 33.2 (32.7–33.3) | 19,543 (19,065–19,686) | 2.89 (2.88–3.78) | 3,795 (3,611–3,820) | 0.41 (0.40–0.80) | 15.4 | 4.81 | 3.11, 2.94, 2.99 |

## pg_test_fsync (µs per operation, 8 kB writes, first of the runs)

| runtime | open_datasync | fdatasync | fsync | open_sync |
|---|---|---|---|---|
| runc | 208 | 213 | 222 | 237 |
| gVisor | 245 | 249 | 267 | 271 |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 201 | 195 | 229 | 221 |
| Kata + Cloud Hypervisor (virtio-blk volume, O_DIRECT) | 269 | 281 | 270 | 269 |
| Kata + Cloud Hypervisor (virtio-blk volume, Kata's default cache mode) | 216 | 221 | 207 | 208 |
| cchv + Cloud Hypervisor | 200 | 195 | 196 | 196 |

## Memory of the host that one PostgreSQL instance costs (PSS of the runtime's processes, MiB; the difference to the same host without the container)

| runtime | idle (after initdb, 1 min) | under tpcb load | cgroup memory.current idle / load (container) |
|---|---|---|---|
| runc | 73 | 646 | 86 / 2,080 |
| gVisor | 166 | 1,305 | 136 / 2,080 |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 332 | 2,150 | 347 / 5,002 |
| Kata + Cloud Hypervisor (virtio-blk volume, O_DIRECT) | 399 | 2,168 | 360 / 2,098 |
| Kata + Cloud Hypervisor (virtio-blk volume, Kata's default cache mode) | 362 | 2,156 | 379 / 5,368 |
| cchv + Cloud Hypervisor | 320 | 2,135 | 197 / 1,983 |

## Density: N idle PostgreSQL containers (2 CPUs, 2 GiB limit each), host memory per instance (PSS, MiB)

| runtime | N=1 | N=8 | N=16 | all N=16 ready after s |
|---|---|---|---|---|
| runc | 63 | 50 | - | - |
| gVisor | 160 | 92 | - | - |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 342 | 295 | - | - |
| cchv + Cloud Hypervisor | 376 | 317 | - | - |

## Density: N idle nginx containers (1 CPU, 256 MiB limit each), host memory per instance (PSS, MiB) - the fixed cost of a runtime

| runtime | N=1 | N=8 | N=16 |
|---|---|---|---|
| runc | 17 | 11 | - |
| gVisor | 85 | 44 | - |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 182 | 154 | - |
| cchv + Cloud Hypervisor | 80 | 72 | - |

## nginx (1 CPU, 256 MiB, worker_processes 1, files in the image; wrk 2 threads, 60 s, median of 3 runs)

| runtime | first 200 s | idle MiB | under load MiB (after the last 10 MB run) | 1 KB c50 rps | 1 KB c200 rps | 100 KB c50 rps | 100 KB c200 rps | 10 MB c50 rps | 10 MB c200 rps |
|---|---|---|---|---|---|---|---|---|---|
| runc | 0.29 | 18 | 6 | 34,136 (33,955–34,485) | 34,006 (33,807–34,376) | 21,279 (21,248–21,314) | 21,180 (21,143–21,263) | 419 (401–421) | 98 (98–98) |
| gVisor | 0.49 | 89 | -1 | 832 (832–832) | 3,328 (3,326–3,328) | 5,413 (5,409–5,424) | 5,267 (5,232–5,282) | 37 (37–37) | - |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 0.96 | 183 | 226 | 7,496 (7,358–7,691) | 7,526 (7,304–7,749) | 1,783 (1,782–1,804) | 1,619 (1,610–1,635) | 17 (16–17) | 14 (13–14) |
| cchv + Cloud Hypervisor | 0.91 | -22 | -17 | 36,604 (36,590–36,728) | 36,621 (35,651–37,118) | 22,609 (22,242–22,619) | 19,444 (17,881–21,176) | 233 (228–237) | 131 (114–231) |

### nginx p99 latency (ms, median) and CPU of the runtime's processes per 1000 requests (ms of CPU)

| runtime | 1 KB c50 | 1 KB c200 | 100 KB c50 | 100 KB c200 | 10 MB c50 | 10 MB c200 |
|---|---|---|---|---|---|---|
| runc | 1.6 / 29 | 6.8 / 29 | 2.6 / 47 | 12.6 / 47 | 208.9 / 669 | 820.2 / - |
| gVisor | 13.5 / 207 | 33.8 / 194 | 15.1 / 369 | 413.4 / 378 | 645.5 / - | - / - |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | 8.5 / 365 | 236.2 / 364 | 87.5 / 1,330 | 408.0 / 1,469 | 1,970.0 / 119,970 | - / 169,756 |
| cchv + Cloud Hypervisor | 2.2 / 79 | 8.8 / 79 | 3.5 / 106 | 165.0 / 124 | 281.2 / 8,112 | 1,950.0 / 16,674 |

## Start-up (docker run to the first answer; 5 runs, seconds)

| runtime | PostgreSQL first query | nginx first 200 |
|---|---|---|

## Is a file written without fsync still there when the container has exited? (3 tries)

| runtime | without fsync | with fsync |
|---|---|---|
| runc | [True, True, True] | [True, True, True] |
| gVisor | [True, True, True] | [True, True, True] |
| Kata + Cloud Hypervisor (virtio-fs volume, Kata's default) | [False, False, False] | [False, False, False] |
| cchv + Cloud Hypervisor | [True, True, True] | [True, True, True] |
