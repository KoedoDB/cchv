#!/usr/bin/env python3
"""Where does a PostgreSQL container spend its time under select-only (pgbench -S, 16 clients)? Run on the benchmark host, in the directory of rtbench.py,
after `. ./env.sh` (RT_BINDS must be set: rtbench.py refuses to start without it).
For each mode it starts nothing new: one PostgreSQL (the benchmark's own start_pg), then
  net   pgbench from the client container over the bridge (what the comparison measures)
  unix  pgbench inside the server container over the Unix socket (no network: the CPU and memory cost alone)
and samples during each run: CPU per CPU (what /proc/stat shows inside the container: the guest's vCPUs for a VM, all host CPUs for runc), NET softirqs,
and, for a VM, the CPU time of every thread of the VMM on the host.
Usage: exp_net.py RUNTIME [MODES]    (RUNTIME: runc, cloudhv, kata-clh; MODES: net,unix)"""
import json, os, re, sys, threading, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rtbench as R

rtname = sys.argv[1] if len(sys.argv) > 1 else "cloudhv"
modes = (sys.argv[2] if len(sys.argv) > 2 else "net,unix").split(",")
SECS = int(os.environ.get("EXP_SECS", "60"))
TICK = os.sysconf("SC_CLK_TCK")


def find_vmm():
    for line in R.sh(f"pgrep -af '^{R.VMM_BINARIES} ' || true", check=False).stdout.splitlines():
        m = re.search(r"--api-socket (\S+)", line)
        if not m:
            continue
        sock = re.sub(r"^/run/cloudhv/", R.CRUN.rstrip("/") + "/", m.group(1))
        r = R.sh(f"sudo curl -s --max-time 5 --unix-socket {sock} http://localhost/api/v1/vm.info", check=False)
        try:
            cfg = json.loads(r.stdout)["config"]
        except (ValueError, KeyError):
            continue
        if cfg["cpus"]["boot_vcpus"] == 3 and cfg["memory"]["size"] // 1048576 == 2080:
            return int(line.split()[0]), cfg
    raise RuntimeError("no VMM with the 3 vCPU / 2080 MiB VM")


def threads(pid):
    out = {}
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            s = open(f"/proc/{pid}/task/{t}/stat").read()
        except OSError:
            continue
        name = s[s.index("(") + 1:s.rindex(")")]
        f = s[s.rindex(")") + 2:].split()
        out[t] = (name, int(f[11]) + int(f[12]))   # utime + stime
    return out


def guest():
    return R.dk("exec", "pgsrv", "sh", "-c", "grep '^cpu[0-9]' /proc/stat; echo ---SOFT; grep -E 'NET_RX|NET_TX' /proc/softirqs", check=False).stdout


def cpu_lines(txt):
    d = {}
    for l in txt.split("---SOFT")[0].splitlines():
        p = l.split()
        if p and p[0].startswith("cpu"):
            d[p[0]] = list(map(int, p[1:9]))
    return d


def soft(txt):
    d = {}
    for l in txt.split("---SOFT")[1].splitlines():
        p = l.split()
        if len(p) >= 2:
            d[p[0]] = list(map(int, p[1:]))
    return d


R.services(rtname)
R.rm("pgsrv")
time.sleep(3)
tag = "pgdata" if R.RUNTIMES[rtname]["vol"] == "block" else rtname
t0 = R.start_pg("pgsrv", rtname, tag)
first, ip = R.wait_query("pgsrv", t0)
print("== runtime", rtname, " limits", R.vm_limits(rtname, "pg"), " vm sizes:", R.check_vm(rtname, "pg"), " first query", round(first, 1), "s")
pid = None
if rtname in ("cloudhv", "kata-clh"):
    pid, cfg = find_vmm()
    print("VMM pid", pid, " net:", json.dumps([{k: v for k, v in n.items() if k in ("num_queues", "queue_size", "offload_csum", "offload_tso", "vhost_user")} for n in cfg.get("net", [])]), " memory thp/hugepages:", cfg["memory"].get("thp"), cfg["memory"].get("hugepages"))
R.dk("exec", "pgclient", "pgbench", "-h", ip, "-U", "postgres", "-i", "-s", "100", "--quiet", "postgres", timeout=7200)
print("init done")


def pgb(mode, secs):
    if mode == "net":
        cmd = ["exec", "pgclient", "pgbench", "-h", ip, "-U", "postgres", "-c", "16", "-j", "4", "-T", str(secs), "-S", "-r", "postgres"]
    else:
        cmd = ["exec", "pgsrv", "pgbench", "-h", "/var/run/postgresql", "-U", "postgres", "-c", "16", "-j", "4", "-T", str(secs), "-S", "-r", "postgres"]
    return R.dk(*cmd, check=False, timeout=secs + 300)


for mode in modes:
    pgb(mode, 20)   # warm up
    res = {}
    th = threading.Thread(target=lambda: res.setdefault("out", pgb(mode, SECS)))
    g0 = guest(); h0 = threads(pid) if pid else {}; w0 = time.time()
    th.start(); th.join()
    w1 = time.time(); g1 = guest(); h1 = threads(pid) if pid else {}
    el = w1 - w0
    out = res["out"].stdout
    print("\n=== %s / %s: %s | %s" % (rtname, mode, re.search(r"tps = [0-9.]+ \(without initial connection time\)", out).group(0), re.search(r"latency average = [0-9.]+ ms", out).group(0)))
    a, b = cpu_lines(g0), cpu_lines(g1)
    tot_busy = 0.0
    print("CPUs seen in the container (%% of time over %.0f s)" % el)
    for c in sorted(a, key=lambda x: int(x[3:])):
        d = [y - x for x, y in zip(a[c], b[c])]
        tot = sum(d) or 1
        user, nice, sysm, idle, iow, irq, sirq, steal = d
        busy = 100 * (tot - idle - iow) / tot
        tot_busy += busy
        print("  %-5s busy %5.1f  user %5.1f  system %5.1f  softirq %5.1f  steal %4.1f" % (c, busy, 100 * (user + nice) / tot, 100 * sysm / tot, 100 * sirq / tot, 100 * steal / tot))
    print("  sum of busy: %.0f%% of one CPU" % tot_busy)
    sa, sb = soft(g0), soft(g1)
    print("NET softirqs/s per CPU:", {k: [round((y - x) / el) for x, y in zip(sa[k], sb[k])] for k in sb})
    if pid:
        rows = sorted(((100.0 * (tk - h0[t][1]) / TICK / el, name) for t, (name, tk) in h1.items() if t in h0), reverse=True)
        print("VMM threads on the host (%% of one core):", ", ".join("%s %.0f%%" % (n, p) for p, n in rows[:6]), "| sum %.0f%%" % sum(p for p, n in rows))
R.rm("pgsrv")
print("done")
