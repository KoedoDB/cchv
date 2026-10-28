#!/usr/bin/env python3
"""Prints the results of a run as Markdown tables. Usage: summarize.py RESULTS_DIR [KATA_SAME_VM_DIR]
KATA_SAME_VM_DIR (optional): the results of run-kata-samevm.sh. Its Kata rows are printed next to the Kata rows of RESULTS_DIR, so that Kata is shown both with the
VM one vCPU larger (what Kata makes of the same limit) and with the VM the same size as cchv's."""
import json, os, sys

R = sys.argv[1]
R2 = sys.argv[2] if len(sys.argv) > 2 else None
ORDER = ["runc", "runc-loop", "runsc", "kata-qemu", "kata-clh", "kata-clh-blk-direct", "kata-clh-blk", "cloudhv"]
LABEL = {"runc": "runc", "runc-loop": "runc (volume on the loop device)", "runsc": "gVisor", "kata-qemu": "Kata + QEMU (Kata's default VMM; virtio-fs)", "kata-clh": "Kata + Cloud Hypervisor (virtio-fs volume, Kata's default)", "kata-clh-blk-direct": "Kata + Cloud Hypervisor (virtio-blk volume, O_DIRECT)", "kata-clh-blk": "Kata + Cloud Hypervisor (virtio-blk volume, Kata's default cache mode)", "cloudhv": "cchv + Cloud Hypervisor"}


def load(name):
    p = os.path.join(R, name + ".json")
    return json.load(open(p)) if os.path.exists(p) else None


def load2(name):
    p = os.path.join(R2, name + ".json") if R2 else None
    return json.load(open(p)) if p and os.path.exists(p) else None


def variants(kind):
    """(runtime, label, data) of every row of a table: one per runtime; for Kata a second row from R2, when given."""
    for rt in ORDER:
        d = load(f"{kind}-{rt}")
        if d:
            yield rt, (LABEL[rt] + (", VM one vCPU larger" if rt == "kata-clh" and R2 else "")), d
        if rt == "kata-clh" and R2:
            d2 = load2(f"{kind}-{rt}")
            if d2:
                yield rt, LABEL[rt] + ", VM the same size as cchv's", d2


def f(v, d=0):
    return "-" if v is None else (f"{v:,.{d}f}")


def med(x, d=0):
    return "-" if not x else f"{x['median']:,.{d}f} ({x['min']:,.{d}f}–{x['max']:,.{d}f})"


print("## PostgreSQL 18.6 (16 clients, 4 jobs, scale 100; median of 3 runs, min–max in brackets)\n")
print("**Same CPUs to use in all four** (PostgreSQL 3, nginx 2: `--cpus` for runc, gVisor, Kata, cchv). **Kata's VM has one vCPU more than cchv's** (PostgreSQL 4 vs 3, nginx 3 vs 2; same memory): Kata always adds one vCPU to the container's limit and cannot be told otherwise. The extra vCPU is outside the container's limit and may be used by the guest kernel (network, I/O); it can favour Kata.\n")
print("| runtime | tpcb-like tps | tpcb p99 ms | select-only tps | select p99 ms | commit (1 client) tps | commit p99 ms | pgbench -i s | first query s | restart s |")
print("|---|---|---|---|---|---|---|---|---|---|")
for rt, label, d in variants("pg"):
    if not d:
        continue
    t, s, c = d.get("tpcb"), d.get("select"), d.get("commit")
    print(f"| {label} | {med(t and t['tps'])} | {med(t and t['p99_ms'], 1)} | {med(s and s['tps'])} | {med(s and s['p99_ms'], 2)} | {med(c and c['tps'])} | {med(c and c['p99_ms'], 2)} | "
          f"{f(d.get('pgbench_init_s'), 1)} | {f(d.get('first_query_s'), 2)} | {', '.join(str(x) for x in d.get('restart_first_query_s', []))} |")

print("\n## pg_test_fsync (µs per operation, 8 kB writes, first of the runs)\n")
print("| runtime | open_datasync | fdatasync | fsync | open_sync |")
print("|---|---|---|---|---|")
for rt, label, d in variants("pg"):
    if not d or not d.get("pg_test_fsync"):
        continue
    fs = d["pg_test_fsync"]
    print(f"| {label} | " + " | ".join(str(fs.get(k, [{}])[0].get("us_per_op", "-")) for k in ("open_datasync", "fdatasync", "fsync", "open_sync")) + " |")

print("\n## Memory of the host that one PostgreSQL instance costs (PSS of the runtime's processes, MiB; the difference to the same host without the container)\n")
print("| runtime | idle (after initdb, 1 min) | under tpcb load | cgroup memory.current idle / load (container) |")
print("|---|---|---|---|")
for rt, label, d in variants("pg"):
    if not d or not d.get("mem_baseline"):
        continue
    b = d["mem_baseline"]["pss_mib"]
    idle = d["mem_idle_after_initdb"]["pss_mib"] - b if d.get("mem_idle_after_initdb") else None
    load_ = d["mem_under_load"]["pss_mib"] - b if d.get("mem_under_load") else None
    print(f"| {label} | {f(idle, 0)} | {f(load_, 0)} | {f(d.get('cgroup_idle_mib'), 0)} / {f(d.get('cgroup_under_load_mib'), 0)} |")

print("\n## Density: N idle PostgreSQL containers (3 CPUs, 2 GiB limit each), host memory per instance (PSS, MiB)\n")
print("| runtime | N=1 | N=8 | N=16 | all N=16 ready after s |")
print("|---|---|---|---|---|")
for rt in ORDER:
    cells, ready = [], "-"
    for n in (1, 8, 16):
        d = load(f"density-{rt}-{n}")
        cells.append(f(d["per_instance_pss_mib"], 0) if d else "-")
        if d and n == 16:
            ready = f(d["all_ready_s"], 0)
    if any(c != "-" for c in cells):
        print(f"| {LABEL[rt]} | " + " | ".join(cells) + f" | {ready} |")

print("\n## Density: N idle nginx containers (2 CPUs, 256 MiB limit each), host memory per instance (PSS, MiB) - the fixed cost of a runtime\n")
print("| runtime | N=1 | N=8 | N=16 |")
print("|---|---|---|---|")
for rt in ORDER:
    cells = []
    for n in (1, 8, 16):
        d = load(f"ndensity-{rt}-{n}")
        cells.append(f(d["per_instance_pss_mib"], 0) if d else "-")
    if any(c != "-" for c in cells):
        print(f"| {LABEL[rt]} | " + " | ".join(cells) + " |")

print("\n## nginx under load: host memory per instance (PSS, MiB), idle -> while wrk loads all N instances (total requests per second in brackets)\n")
print("| runtime | N=1 | N=8 | N=16 |")
print("|---|---|---|---|")
for rt in ORDER:
    cells = []
    for n in (1, 8, 16):
        d = load(f"ndensity-{rt}-{n}")
        cells.append(f"{f(d['per_instance_pss_mib'], 0)} -> {f(d['per_instance_pss_under_load_mib'], 0)} ({f(d.get('load_rps_total'), 0)} rps)" if d and d.get("per_instance_pss_under_load_mib") is not None else "-")
    if any(c != "-" for c in cells):
        print(f"| {LABEL[rt]} | " + " | ".join(cells) + " |")

print("\n## nginx (2 CPUs, 256 MiB, worker_processes 1, files in the image; wrk 2 threads, 60 s, median of 3 runs)\n")
print("**Same CPUs to use in all four** (PostgreSQL 3, nginx 2: `--cpus` for runc, gVisor, Kata, cchv). **Kata's VM has one vCPU more than cchv's** (PostgreSQL 4 vs 3, nginx 3 vs 2; same memory): Kata always adds one vCPU to the container's limit and cannot be told otherwise. The extra vCPU is outside the container's limit and may be used by the guest kernel (network, I/O); it can favour Kata.\n")
print("| runtime | first 200 s | 1 KB c50 rps | 1 KB c200 rps | 100 KB c50 rps | 100 KB c200 rps | 10 MB c50 rps | 10 MB c200 rps |")
print("|---|---|---|---|---|---|---|---|")
for rt, label, d in variants("nginx"):
    if not d:
        continue
    cells = []
    for k in ("1k.html_c50", "1k.html_c200", "100k.bin_c50", "100k.bin_c200", "10m.bin_c50", "10m.bin_c200"):
        cells.append(med(d.get(k, {}).get("rps")))
    print(f"| {label} | {f(d.get('first_200_s'), 2)} | " + " | ".join(cells) + " |")
print("\n### nginx p99 latency (ms, median) and CPU of the runtime's processes per 1000 requests (ms of CPU)\n")
print("| runtime | 1 KB c50 | 1 KB c200 | 100 KB c50 | 100 KB c200 | 10 MB c50 | 10 MB c200 |")
print("|---|---|---|---|---|---|---|")
for rt, label, d in variants("nginx"):
    if not d:
        continue
    cells = []
    for k in ("1k.html_c50", "1k.html_c200", "100k.bin_c50", "100k.bin_c200", "10m.bin_c50", "10m.bin_c200"):
        e = d.get(k)
        if not e:
            cells.append("-")
            continue
        cpu = [r["cpu_ms_per_1000_requests"] for r in e["runs"] if r.get("cpu_ms_per_1000_requests") and r["cpu_ms_per_1000_requests"] > 0]   # a negative value is a measurement error (processes of the runtime came and went during the run)
        cells.append(f"{f(e['p99_ms']['median'] if e.get('p99_ms') else None, 1)} / {f(sorted(cpu)[len(cpu)//2] if cpu else None, 0)}")
    print(f"| {label} | " + " | ".join(cells) + " |")

print("\n## Start-up (docker run to the first answer; 5 runs, seconds)\n")
print("| runtime | PostgreSQL first query | nginx first 200 |")
print("|---|---|---|")
for rt in ORDER:
    d = load(f"start-{rt}")
    if d:
        print(f"| {LABEL[rt]} | {', '.join(str(x) for x in d['pg_first_query_s'])} | {', '.join(str(x) for x in d['nginx_first_200_s'])} |")

print("\n## Is a file written without fsync still there when the container has exited? (3 tries)\n")
print("| runtime | without fsync | with fsync |")
print("|---|---|---|")
for rt in ORDER:
    d = load(f"stopcheck-{rt}")
    if d:
        print(f"| {LABEL[rt]} | {d['no_sync_file_survives_container_exit']} | {d['fsync_file_survives_container_exit']} |")
