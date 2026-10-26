#!/usr/bin/env python3
"""The workloads of the runtime comparison. Run on the benchmark host, as a user that may sudo, after 10-install.sh, 12-prereqs.sh, 30-loopvg.sh and 20-start.sh.

  rtbench.py prepare                    pull and build the images, make the client containers
  rtbench.py pg RUNTIME [--quick]       PostgreSQL: start, pgbench (tpcb-like, select-only, commit latency), pg_test_fsync, memory
  rtbench.py nginx RUNTIME [--quick]    nginx: three static files at 50 and 200 connections with wrk, memory, CPU per 1000 requests
  rtbench.py density RUNTIME N          N idle PostgreSQL containers: the memory of the host that they cost
  rtbench.py start RUNTIME              the start-up times only (docker run -> first query / first 200), 5 times each

RUNTIME is one of: runc, runsc, kata-clh, cloudhv, runc-loop (the control: runc on the same loop-device logical volume that cchv gets).
Results go to $RT_RESULTS (default ~/rtbench-results/<date>), one JSON file per run and the raw output next to it.
"""
import argparse, json, os, re, statistics, subprocess, sys, time

ROOT = "/opt/rtbench"
VG = os.environ.get("RT_VG", "rtvg")         # the volume group of the database volumes, and the prefix of the logical volumes in it (see env.sh)
LVP = os.environ.get("RT_LVP", "")
CRUN = os.environ.get("RT_CRUN", "/run/cloudhv")
BINDS = os.environ.get("RT_BINDS", "")
DATA = os.environ.get("RT_DATA", "/var/lib/rtbench")
SOCK = "unix:///run/rtbench/docker.sock"
DOCKER = ["sudo", f"{ROOT}/docker/docker", "-H", SOCK]
RESULTS = os.environ.get("RT_RESULTS") or os.path.expanduser("~/rtbench-results/" + time.strftime("%Y%m%d"))
POSTGRES = os.environ.get("POSTGRES_IMAGE", "postgres:18.6-trixie")
NGINX = os.environ.get("NGINX_IMAGE", "nginx:1.29-alpine")
CLIENT_CPUS = os.environ.get("CLIENT_CPUS", "6,7")
BLOCK = '[{"device":"/dev/pgdata","mount":"/var/lib/postgresql","format":"if-blank","owner":"999:999"}]'
PG_ARGS = ["-c", "shared_buffers=512MB", "-c", "max_wal_size=4GB", "-c", "listen_addresses=*", "-c", "fsync=on", "-c", "synchronous_commit=on"]

RUNTIMES = {
    "runc": {"runtime": "runc", "vol": "dir"},
    "runsc": {"runtime": "runsc", "vol": "dir"},
    "kata-clh": {"runtime": "kata-clh", "vol": "dir"},
    "cloudhv": {"runtime": "cloudhv", "vol": "block"},
    "runc-loop": {"runtime": "runc", "vol": "loop"},
}


def sh(cmd, check=True, timeout=None, input=None):
    r = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True, timeout=timeout, input=input)
    if check and r.returncode != 0:
        raise RuntimeError(f"{cmd!r} failed ({r.returncode}): {(r.stderr or r.stdout).strip()[:400]}")
    return r


def dk(*args, check=True, timeout=None, input=None):
    return sh(DOCKER + list(args), check=check, timeout=timeout, input=input)


def save(name, obj, raw=None):
    os.makedirs(RESULTS, exist_ok=True)
    with open(os.path.join(RESULTS, name + ".json"), "w") as f:
        json.dump(obj, f, indent=1)
    if raw is not None:
        with open(os.path.join(RESULTS, name + ".txt"), "w") as f:
            f.write(raw)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, max(0, int(round(p / 100.0 * len(s) + 0.5)) - 1))]


def summ(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values), "n": len(values), "values": values}


# The Cloud Hypervisor binaries of the benchmark (cchv's: ROOT/bin; Kata's: /opt/kata/bin). A host that runs cchv for real has another one.
VMM_BINARIES = f"({ROOT}|/opt/kata)/bin/cloud-hypervisor"


def kill_stale_vms():
    """After a daemon that was stopped with VMs left to their pods: kill the VMMs of that run. Only the benchmark's own binaries (VMM_BINARIES): a host that runs cchv for real (stg-db-001) has VMs of its own."""
    sh(f"sudo pkill -f '^{VMM_BINARIES} '; sleep 1", check=False)


def services(rtname):
    """The cchv daemon (and its pool of ready VMs) runs only for the cchv runs: the other runtimes are measured without it."""
    active = sh("systemctl is-active rtbench-cchv-daemon", check=False).stdout.strip() == "active"
    if rtname == "cloudhv" and not active:
        kill_stale_vms()
        sh(f"sudo find {CRUN}/state -mindepth 1 -delete", check=False)
        sh("sudo systemctl reset-failed rtbench-cchv-daemon 2>/dev/null; sudo systemd-run --unit=rtbench-cchv-daemon --property=Restart=no {BINDS} -E CLOUDHV_MEMORY_RESTORE_MODE=Copy -E RUST_LOG=info "
           f"{ROOT}/bin/cloudhv-sandbox-daemon /opt/cloudhv/daemon.json", check=False)
        for _ in range(60):
            if "pool initialized" in sh("sudo journalctl -u rtbench-cchv-daemon -o cat --no-pager", check=False).stdout:
                break
            time.sleep(1)
        time.sleep(5)
    elif rtname != "cloudhv" and active:
        dk("rm", "-f", "pgsrv", "nginxsrv", check=False, timeout=120)
        sh("sudo systemctl stop rtbench-cchv-daemon", check=False)
        kill_stale_vms()
        time.sleep(3)


def host_state():
    load = open("/proc/loadavg").read().split()[0]
    others = sh("sudo docker ps -q 2>/dev/null | wc -l", check=False).stdout.strip()
    return {"loadavg1": float(load), "host_docker_running_containers": others}


# ── memory and CPU of the runtime's processes, seen from the host ───────────────────────────────────────────────────────────────────────────────────────────────

VM_COMMS = ("cloud-hyperviso", "virtiofsd", "cloudhv-sandbox", "qemu-system", "nydusd")


def procset():
    """pid -> (comm, rss_kb, pss_kb, cpu_ticks) of every process of the benchmark's runtimes: those in a cgroup whose path has 'rtbench' (the two daemons, the shims, the
    containers, gVisor's sentry and gofers, Kata's VMM) and the VMMs and daemon of cchv and Kata by name (cchv's Cloud Hypervisor runs in a cgroup of its own)."""
    out = {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            comm = open(f"/proc/{pid}/comm").read().strip()
            cg = open(f"/proc/{pid}/cgroup").read()
            if "rtbench" not in cg and not comm.startswith(VM_COMMS):
                continue
            rss = pss = 0
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Rss:"):
                    rss = int(line.split()[1])
                elif line.startswith("Pss:"):
                    pss = int(line.split()[1])
            st = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            out[int(pid)] = (comm, rss, pss, int(st[11]) + int(st[12]))
        except (FileNotFoundError, ProcessLookupError, PermissionError, IndexError):
            continue
    return out


def mem_total():
    ps = procset()
    return {"rss_mib": sum(v[1] for v in ps.values()) / 1024.0, "pss_mib": sum(v[2] for v in ps.values()) / 1024.0, "cpu_ticks": sum(v[3] for v in ps.values()),
            "processes": len(ps), "by_comm": _by_comm(ps)}


def _by_comm(ps):
    d = {}
    for comm, rss, pss, _ in ps.values():
        e = d.setdefault(comm, [0, 0.0])
        e[0] += 1
        e[1] += pss / 1024.0
    return {k: [v[0], round(v[1], 1)] for k, v in sorted(d.items())}


def run_as_root_python(func_name):
    """procset() reads /proc/<pid>/smaps_rollup, which needs root: the whole script is started with sudo for the memory commands, so call it directly when root."""
    return globals()[func_name]()


def cgroup_current(cid):
    full = dk("inspect", "-f", "{{.Id}}", cid).stdout.strip()
    r = sh(f"sudo find /sys/fs/cgroup -maxdepth 4 -type d -name '*{full}*' | head -1", check=False).stdout.strip()
    if not r:
        return None
    try:
        return int(open(os.path.join(r, "memory.current")).read()) / 1048576.0
    except (OSError, ValueError):
        return None


def memsnap_remote():
    """memory of the runtime's processes (needs root: it is read through sudo python of this same file)."""
    r = sh(["sudo", sys.executable, os.path.abspath(__file__), "memjson"])
    return json.loads(r.stdout)


# ── images and containers ────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

NGINX_CONF = """worker_processes 1;
events { worker_connections 4096; }
http {
  access_log off; sendfile on; tcp_nopush on; keepalive_timeout 65;
  include /etc/nginx/mime.types;
  server { listen 80; root /usr/share/nginx/html; location / { try_files $uri =404; } }
}
"""


def prepare():
    for img in (POSTGRES, NGINX, "alpine:3.24"):
        dk("pull", "-q", img, timeout=900)
    ctx = os.path.join(DATA, "build-nginx")
    sh(f"sudo mkdir -p {ctx} && sudo chown $(id -u) {ctx}")
    open(os.path.join(ctx, "nginx.conf"), "w").write(NGINX_CONF)
    open(os.path.join(ctx, "Dockerfile"), "w").write(
        f"FROM {NGINX}\nCOPY nginx.conf /etc/nginx/nginx.conf\n"
        "RUN head -c 750 /dev/urandom | base64 -w0 | head -c 1024 > /usr/share/nginx/html/1k.html \\\n"
        " && head -c 100000 /dev/urandom > /usr/share/nginx/html/100k.bin \\\n"
        " && head -c 10485760 /dev/urandom > /usr/share/nginx/html/10m.bin\n")
    dk("build", "-q", "-t", "rtbench-nginx", ctx, timeout=600)
    open(os.path.join(ctx, "Dockerfile.wrk"), "w").write("FROM alpine:3.24\nRUN apk add --no-cache wrk\nENTRYPOINT [\"sleep\", \"infinity\"]\n")
    dk("build", "-q", "--network", "host", "-t", "rtbench-wrk", "-f", os.path.join(ctx, "Dockerfile.wrk"), ctx, timeout=600)
    for name in ("pgclient", "wrkc"):
        dk("rm", "-f", name, check=False)
    dk("run", "-d", "--name", "pgclient", "--runtime", "runc", "--cpuset-cpus", CLIENT_CPUS, "-m", "2g", "--pids-limit", "512", "--entrypoint", "sleep", POSTGRES, "infinity")
    dk("run", "-d", "--name", "wrkc", "--runtime", "runc", "--cpuset-cpus", CLIENT_CPUS, "-m", "1g", "--pids-limit", "512", "rtbench-wrk")
    log("images and client containers ready")


def container_ip(name):
    return dk("inspect", "-f", '{{(index .NetworkSettings.Networks "bridge").IPAddress}}', name).stdout.strip()


def volume_args(kind, rt, tag):
    """The data volume of a PostgreSQL container for each runtime."""
    if rt["vol"] == "block":
        dev = f"/dev/{VG}/{LVP}{tag}"
        sh(f"sudo blkdiscard -f --zeroout {dev}")
        return ["--device", f"{dev}:/dev/pgdata", "--annotation", "cloudhv.koedodb.com/block-volumes=" + BLOCK]
    if rt["vol"] == "loop":
        path = f"{DATA}/mnt-ctl"
    else:
        path = f"{DATA}/pgdata-{tag}"
    sh(f"sudo mkdir -p {path}; sudo find {path} -mindepth 1 -delete; sudo chown 999:999 {path}", check=False)
    return ["-v", f"{path}:/var/lib/postgresql"]


# The VM of a Kata or a cchv container is the same size in every test: PostgreSQL 3 vCPUs and 2080 MiB, nginx 2 vCPUs and 320 MiB.
# Kata makes its VM from the container's limits (+1 vCPU, +32 MiB) and cannot be told otherwise; cchv makes it from the limits (+64 MiB) and
# never gives it fewer vCPUs than its pool's default (RT_CCHV_VCPUS in 20-start.sh: 3 for PostgreSQL, 2 for nginx). So the limits are chosen
# per runtime so that both end in the same VM; runc and gVisor get the VM's own numbers as their limits (--cpus and -m), so that all four have the same CPUs and memory to use.
# (Kata also sets a cgroup of its own limits inside the guest: 2 CPUs for PostgreSQL, 1 for nginx; cchv sets none. Seen with `cat /sys/fs/cgroup/cpu.max` in the guest.)
VM_SIZE = {"pg": (3, 2080), "nginx": (2, 320)}
PLAIN_LIMITS = {"pg": (str(VM_SIZE["pg"][0]), VM_SIZE["pg"][1]), "nginx": (str(VM_SIZE["nginx"][0]), VM_SIZE["nginx"][1])}


def vm_limits(rtname, kind):
    cpus, mem = PLAIN_LIMITS[kind]
    runtime = RUNTIMES[rtname]["runtime"]
    if runtime == "kata-clh":
        cpus, mem = str(VM_SIZE[kind][0] - 1), VM_SIZE[kind][1] - 32   # Kata adds 1 vCPU and 32 MiB
    elif runtime == "cloudhv":
        cpus, mem = str(VM_SIZE[kind][0] - 1), VM_SIZE[kind][1] - 64   # cchv adds 64 MiB; the vCPUs come from the pool's default_vcpus (>= the limit)
    return cpus, f"{mem}m"


def vm_sizes():
    """The VMs of the benchmark that are running now, as [vCPUs, memory MiB]: asked of every one of its Cloud Hypervisor processes through its API socket (vm.info; the size is not on the command line)."""
    out = []
    for line in sh(f"pgrep -af '^{VMM_BINARIES} ' || true", check=False).stdout.splitlines():
        m = re.search(r"--api-socket (\S+)", line)
        if not m:
            continue
        # The daemon sees RT_CRUN as /run/cloudhv (BindPaths=, see env.sh), so that is the path on the command line; from here the socket is under RT_CRUN.
        sock = re.sub(r"^/run/cloudhv/", CRUN.rstrip("/") + "/", m.group(1))
        r = sh(f"sudo curl -s --max-time 5 --unix-socket {sock} http://localhost/api/v1/vm.info", check=False)
        try:
            cfg = json.loads(r.stdout)["config"]
            out.append([cfg["cpus"]["boot_vcpus"], cfg["memory"]["size"] // 1048576])
        except (ValueError, KeyError):
            continue   # a VMM that has no VM yet
    return sorted(out)


def check_vm(rtname, kind):
    """The VM that was just started must be the size VM_SIZE says (Kata and cchv; the pool's idle VMs of cchv are in the list as well). Raises when it is not: a result with VMs of different sizes is not a result."""
    sizes = vm_sizes()
    if RUNTIMES[rtname]["runtime"] in ("kata-clh", "cloudhv") and list(VM_SIZE[kind]) not in sizes:
        raise RuntimeError(f"{rtname}: no VM of {VM_SIZE[kind]} (vCPUs, MiB) is running; the VMs are {sizes}")
    return sizes


def start_pg(name, rtname, tag, limits=None):
    rt = RUNTIMES[rtname]
    limits = limits or vm_limits(rtname, "pg")
    args = ["run", "-d", "--name", name, "--runtime", rt["runtime"], "--cpus", limits[0], "-m", limits[1], "--memory-swap", limits[1], "--pids-limit", "2048",
            "--shm-size", "256m", "-e", "POSTGRES_HOST_AUTH_METHOD=trust"] + volume_args("pg", rt, tag) + [POSTGRES] + PG_ARGS
    t0 = time.time()
    dk(*args, timeout=300)
    return t0


def wait_query(name, t0, timeout=300):
    ip = container_ip(name)
    deadline = time.time() + timeout
    while time.time() < deadline:
        r = dk("exec", "pgclient", "psql", "-h", ip, "-U", "postgres", "-tAc", "select 1", check=False, timeout=60)
        if r.returncode == 0 and r.stdout.strip() == "1":
            return time.time() - t0, ip
        if "no pg_hba.conf entry" in (r.stderr or ""):
            raise RuntimeError(f"{name}: the database refuses the client (pg_hba.conf has no trust line: not a fresh data volume?)")
        time.sleep(0.05)
    raise RuntimeError(f"{name}: no query answered in {timeout}s")


def rm(name):
    dk("rm", "-f", name, check=False, timeout=120)


def pgbench(ip, extra, seconds, tag, clients=16, jobs=4):
    """One pgbench run in the client container with a per-transaction log; returns tps, latency average and percentiles (ms)."""
    prefix = f"/tmp/pgb_{tag}"
    dk("exec", "pgclient", "sh", "-c", f"rm -f {prefix}*")
    cmd = ["exec", "pgclient", "pgbench", "-h", ip, "-U", "postgres", "-c", str(clients), "-j", str(jobs), "-T", str(seconds), "-l", "--log-prefix", prefix, "-r", "-P", "30"] + extra + ["postgres"]
    r = dk(*cmd, check=False, timeout=seconds + 600)
    out = r.stdout + r.stderr
    tps = re.search(r"tps = ([0-9.]+) \(without initial connection time\)", out)
    lat = re.search(r"latency average = ([0-9.]+) ms", out)
    fail = re.search(r"number of failed transactions: (\d+)", out)
    lg = dk("exec", "pgclient", "sh", "-c", f"cat {prefix}*", check=False, timeout=600).stdout
    lats = []
    for line in lg.splitlines():
        p = line.split()
        if len(p) >= 3:
            try:
                lats.append(int(p[2]) / 1000.0)
            except ValueError:
                pass
    res = {"tps": float(tps.group(1)) if tps else None, "latency_avg_ms": float(lat.group(1)) if lat else None, "n_transactions": len(lats),
           "p50_ms": pct(lats, 50), "p99_ms": pct(lats, 99), "p999_ms": pct(lats, 99.9), "failed": int(fail.group(1)) if fail else 0, "rc": r.returncode}
    dk("exec", "pgclient", "sh", "-c", f"rm -f {prefix}*", check=False)
    return res, out


def pg(rtname, quick=False):
    assert rtname in RUNTIMES
    services(rtname)
    tag = "pgdata" if RUNTIMES[rtname]["vol"] == "block" else rtname
    scale, t_tpcb, t_sel, t_com, reps = (100, 300, 180, 60, 3) if not quick else (10, 20, 15, 10, 1)
    result = {"runtime": rtname, "settings": {"scale": scale, "clients": 16, "jobs": 4, "tpcb_s": t_tpcb, "select_only_s": t_sel, "commit_s": t_com, "reps": reps,
                                              "pg_args": PG_ARGS, "limits": " / ".join(vm_limits(rtname, "pg")) + " (--cpus / -m); the VM of Kata and cchv is %d vCPUs, %d MiB" % VM_SIZE["pg"]}, "host": host_state()}
    rm("pgsrv")
    time.sleep(3)
    base = memsnap_remote()
    result["mem_baseline"] = base
    log(rtname, "start")
    t0 = start_pg("pgsrv", rtname, tag)
    first, ip = wait_query("pgsrv", t0)
    result["first_query_s"] = round(first, 2)
    result["vm_sizes"] = check_vm(rtname, "pg")
    log(rtname, "first query after", round(first, 2), "s")
    time.sleep(10 if quick else 60)
    result["mem_idle_after_initdb"] = memsnap_remote()
    cid = "pgsrv"
    result["cgroup_idle_mib"] = cgroup_current(cid)
    # restart (a warm database that needs no initdb)
    rs = []
    for _ in range(3 if not quick else 1):
        # A `sync` first: under cchv a normal container stop tears the VM down without flushing what the container wrote and did not fsync (see stopcheck): the entrypoint's
        # edit of pg_hba.conf is such a write, so the restart would find a database that refuses the client. The restart times are those of an application that syncs.
        dk("exec", "pgsrv", "sync", check=False)
        t1 = time.time()
        dk("restart", "-t", "30", "pgsrv", timeout=200)
        rs.append(round(wait_query("pgsrv", t1)[0], 2))
    result["restart_first_query_s"] = rs
    ip = container_ip("pgsrv")
    log(rtname, "pgbench init scale", scale)
    t1 = time.time()
    r = dk("exec", "pgclient", "pgbench", "-h", ip, "-U", "postgres", "-i", "-s", str(scale), "--quiet", "postgres", check=False, timeout=7200)
    result["pgbench_init_s"] = round(time.time() - t1, 1)
    if r.returncode != 0:
        result["pgbench_init_error"] = (r.stderr or r.stdout)[-400:]
        save(f"pg-{rtname}", result)
        rm("pgsrv")
        return result
    dk("exec", "pgclient", "psql", "-h", ip, "-U", "postgres", "-c", "CHECKPOINT", check=False)
    dk("exec", "pgclient", "psql", "-h", ip, "-U", "postgres", "-c", "CREATE TABLE IF NOT EXISTS bench_commit(id bigserial PRIMARY KEY, pad text)", check=False)
    os.makedirs(RESULTS, exist_ok=True)
    cs = os.path.join(RESULTS, "commit.sql")
    open(cs, "w").write("INSERT INTO bench_commit(pad) VALUES (md5(random()::text));\n")
    dk("cp", cs, "pgclient:/tmp/commit.sql")
    raws = []
    for kind, extra, secs, clients, jobs in (("tpcb", [], t_tpcb, 16, 4), ("select", ["-S"], t_sel, 16, 4), ("commit", ["-f", "/tmp/commit.sql"], t_com, 1, 1)):
        runs = []
        for i in range(reps):
            log(rtname, kind, "rep", i + 1)
            if kind == "tpcb" and i == 0:
                # the memory under load: measured a third into the first read/write run, from another thread of control (the benchmark runs in the foreground)
                import threading
                holder = {}
                def sample():
                    time.sleep(max(5, secs // 3))
                    holder["m"] = memsnap_remote()
                    holder["cg"] = cgroup_current("pgsrv")
                th = threading.Thread(target=sample); th.start()
            res, out = pgbench(ip, extra, secs, f"{kind}{i}", clients, jobs)
            if kind == "tpcb" and i == 0:
                th.join()
                result["mem_under_load"] = holder.get("m")
                result["cgroup_under_load_mib"] = holder.get("cg")
            runs.append(res)
            raws.append(f"== {kind} rep {i + 1}\n{out}\n")
        result[kind] = {"runs": runs, "tps": summ([r["tps"] for r in runs if r["tps"]]) if any(r["tps"] for r in runs) else None,
                        "p99_ms": summ([r["p99_ms"] for r in runs if r["p99_ms"]]) if any(r["p99_ms"] for r in runs) else None,
                        "latency_avg_ms": summ([r["latency_avg_ms"] for r in runs if r["latency_avg_ms"]]) if any(r["latency_avg_ms"] for r in runs) else None}
    log(rtname, "pg_test_fsync")
    f = dk("exec", "pgsrv", "sh", "-c", "/usr/lib/postgresql/18/bin/pg_test_fsync -f /var/lib/postgresql/pg_test_fsync.tmp -s 5; rm -f /var/lib/postgresql/pg_test_fsync.tmp", check=False, timeout=600)
    result["pg_test_fsync_raw"] = f.stdout[-3000:]
    fs = {}
    for line in f.stdout.splitlines():
        m = re.match(r"\s*(open_datasync|fdatasync|fsync|fsync_writethrough|open_sync)\b.*?([0-9.]+) ops/sec\s+([0-9]+) usecs/op", line)
        if m:
            fs.setdefault(m.group(1), []).append({"ops_s": float(m.group(2)), "us_per_op": int(m.group(3))})
    result["pg_test_fsync"] = fs
    result["host_after"] = host_state()
    save(f"pg-{rtname}", result, "".join(raws))
    rm("pgsrv")
    return result


# ── nginx ────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

NGINX_KATA_MEM = "288m"   # = vm_limits("kata-clh", "nginx")


def start_nginx(name, rtname):
    rt = RUNTIMES[rtname]
    t0 = time.time()
    cpus, mem = vm_limits(rtname, "nginx")   # the same VM for Kata and cchv: 2 vCPUs, 320 MiB
    dk("run", "-d", "--name", name, "--runtime", rt["runtime"], "--cpus", cpus, "-m", mem, "--memory-swap", mem, "--pids-limit", "512", "rtbench-nginx", timeout=300)
    ip = container_ip(name)
    deadline = time.time() + 120
    while time.time() < deadline:
        r = dk("exec", "wrkc", "wget", "-q", "-O", "/dev/null", f"http://{ip}/1k.html", check=False, timeout=30)
        if r.returncode == 0:
            return time.time() - t0, ip
        time.sleep(0.02)
    raise RuntimeError("nginx did not answer")


def wrk(ip, path, conns, secs):
    r = dk("exec", "wrkc", "wrk", "-t2", f"-c{conns}", f"-d{secs}s", "--latency", f"http://{ip}/{path}", check=False, timeout=secs + 120)
    out = r.stdout
    rps = re.search(r"Requests/sec:\s+([0-9.]+)", out)
    n = re.search(r"([0-9]+) requests in", out)
    p50 = re.search(r"50%\s+([0-9.]+)(us|ms|s)", out)
    p99 = re.search(r"99%\s+([0-9.]+)(us|ms|s)", out)
    def ms(m):
        if not m:
            return None
        v = float(m.group(1))
        return v / 1000.0 if m.group(2) == "us" else (v * 1000.0 if m.group(2) == "s" else v)
    errs = re.search(r"Non-2xx or 3xx responses: (\d+)", out)
    sock = re.search(r"Socket errors: (.*)", out)
    return {"rps": float(rps.group(1)) if rps else None, "requests": int(n.group(1)) if n else None, "p50_ms": ms(p50), "p99_ms": ms(p99),
            "non2xx": int(errs.group(1)) if errs else 0, "socket_errors": sock.group(1) if sock else None}, out


def nginx(rtname, quick=False):
    services(rtname)
    secs, reps = (60, 3) if not quick else (8, 1)
    result = {"runtime": rtname, "settings": {"seconds": secs, "reps": reps, "limits": " / ".join(vm_limits(rtname, "nginx")) + " (--cpus / -m); the VM of Kata and cchv is %d vCPUs, %d MiB" % VM_SIZE["nginx"], "files": ["1k.html", "100k.bin", "10m.bin"], "connections": [50, 200]}, "host": host_state()}
    rm("nginxsrv")
    time.sleep(3)
    result["mem_baseline"] = memsnap_remote()
    first, ip = start_nginx("nginxsrv", rtname)
    result["first_200_s"] = round(first, 2)
    result["vm_sizes"] = check_vm(rtname, "nginx")
    time.sleep(5 if quick else 30)
    result["mem_idle"] = memsnap_remote()
    raws = []
    for path in ("1k.html", "100k.bin", "10m.bin"):
        wrk(ip, path, 50, 5)  # warm: page cache, connections
        for conns in (50, 200):
            runs = []
            for i in range(reps):
                m0 = memsnap_remote()
                res, out = wrk(ip, path, conns, secs)
                m1 = memsnap_remote()
                ticks = m1["cpu_ticks"] - m0["cpu_ticks"]
                res["cpu_ms_per_1000_requests"] = round(ticks * 10.0 / (res["requests"] / 1000.0), 1) if res["requests"] else None
                res["mem_pss_mib_after"] = round(m1["pss_mib"], 1)
                runs.append(res)
                raws.append(f"== {path} c={conns} rep {i + 1}\n{out}\n")
                log(rtname, "nginx", path, conns, i + 1, res["rps"])
            key = f"{path}_c{conns}"
            vals = [r["rps"] for r in runs if r["rps"] is not None]
            result[key] = {"runs": runs, "rps": summ(vals) if vals else None,
                           "p99_ms": summ([r["p99_ms"] for r in runs if r["p99_ms"]]) if any(r["p99_ms"] for r in runs) else None}
    result["mem_under_load_last"] = memsnap_remote()
    result["host_after"] = host_state()
    save(f"nginx-{rtname}", result, "".join(raws))
    rm("nginxsrv")
    return result


# ── density and start-up ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

def density(rtname, n):
    rt = RUNTIMES[rtname]
    services(rtname)
    names = [f"dens{i}" for i in range(n)]
    for nm in names:
        rm(nm)
    time.sleep(3)
    base = memsnap_remote()
    t0 = time.time()
    for i, nm in enumerate(names):
        if rt["vol"] == "block":
            lv = f"dens{i}"
            sh(f"sudo lvs {VG}/{LVP}{lv} >/dev/null 2>&1 || sudo lvcreate -y -L 1G -n {LVP}{lv} {VG} >/dev/null")
            sh(f"sudo blkdiscard -f --zeroout /dev/{VG}/{LVP}{lv}")
            args = ["--device", f"/dev/{VG}/{LVP}{lv}:/dev/pgdata", "--annotation", "cloudhv.koedodb.com/block-volumes=" + BLOCK]
        else:
            path = f"{DATA}/pgdens-{rtname}-{i}"
            sh(f"sudo rm -rf {path}; sudo mkdir -p {path}; sudo chown 999:999 {path}")
            args = ["-v", f"{path}:/var/lib/postgresql"]
        cpus, mem = vm_limits(rtname, "pg")
        dk("run", "-d", "--name", nm, "--runtime", rt["runtime"], "--cpus", cpus, "-m", mem, "--memory-swap", mem, "--pids-limit", "2048", "--shm-size", "256m",
           "-e", "POSTGRES_HOST_AUTH_METHOD=trust", *args, POSTGRES, *PG_ARGS, timeout=300)
    ready = []
    for nm in names:
        wait_query(nm, time.time(), timeout=300)
        ready.append(round(time.time() - t0, 1))
    time.sleep(60)
    vms = check_vm(rtname, "pg")
    snap = memsnap_remote()
    with open("/proc/meminfo") as f:
        mi = {l.split(":")[0]: int(l.split()[1]) for l in f}
    res = {"runtime": rtname, "n": n, "all_ready_s": ready[-1], "vm_sizes": vms, "mem_baseline": base, "mem_after": snap,
           "per_instance_pss_mib": round((snap["pss_mib"] - base["pss_mib"]) / n, 1), "per_instance_rss_mib": round((snap["rss_mib"] - base["rss_mib"]) / n, 1),
           "host_mem_available_mib": round(mi["MemAvailable"] / 1024.0, 0)}
    save(f"density-{rtname}-{n}", res)
    log(rtname, "density", n, res["per_instance_pss_mib"], "MiB PSS per instance")
    for nm in names:
        rm(nm)
    return res


def nginx_density(rtname, n):
    """N idle nginx containers (limits: vm_limits(rtname, "nginx")): what the host spends per instance. The fixed cost of a container, without a database that takes all the memory it is given."""
    services(rtname)
    names = [f"ndens{i}" for i in range(n)]
    for nm in names:
        rm(nm)
    time.sleep(3)
    base = memsnap_remote()
    for nm in names:
        start_nginx(nm, rtname)
    time.sleep(60)
    vms = check_vm(rtname, "nginx")
    snap = memsnap_remote()
    res = {"runtime": rtname, "n": n, "vm_sizes": vms, "mem_baseline": base, "mem_after": snap,
           "per_instance_pss_mib": round((snap["pss_mib"] - base["pss_mib"]) / n, 1), "per_instance_rss_mib": round((snap["rss_mib"] - base["rss_mib"]) / n, 1)}
    save(f"ndensity-{rtname}-{n}", res)
    log(rtname, "nginx density", n, res["per_instance_pss_mib"], "MiB PSS per instance")
    for nm in names:
        rm(nm)
    return res


def stopcheck(rtname):
    """Is a file that the container wrote WITHOUT fsync still there when the container has exited and another one reads the volume?"""
    services(rtname)
    rt = RUNTIMES[rtname]
    res = {"runtime": rtname}
    for variant, cmd in (("no_sync", "echo hello > /var/lib/postgresql/f.txt"), ("fsync", "echo hello > /var/lib/postgresql/f.txt; sync")):
        out = []
        for _ in range(3):
            args = volume_args("x", rt, "pgdata" if rt["vol"] == "block" else rtname + "-stopcheck")
            r1 = dk("run", "--rm", "--runtime", rt["runtime"], *args, "alpine:3.24", "sh", "-c", cmd, check=False, timeout=300)
            r2 = dk("run", "--rm", "--runtime", rt["runtime"], *[a for a in args], "alpine:3.24", "sh", "-c", "cat /var/lib/postgresql/f.txt 2>&1; true", check=False, timeout=300)
            out.append("hello" in r2.stdout)
        res[variant + "_file_survives_container_exit"] = out
    save(f"stopcheck-{rtname}", res)
    return res


def start_times(rtname):
    services(rtname)
    res = {"runtime": rtname, "pg_first_query_s": [], "nginx_first_200_s": []}
    tag = "pgdata" if RUNTIMES[rtname]["vol"] == "block" else rtname
    phase = os.environ.get("RT_PHASE", "")   # "pg" or "nginx": the cchv daemon's pool is sized for one of them at a time (20-start.sh)
    for _ in range(5 if phase != "nginx" else 0):
        rm("pgsrv"); time.sleep(2)
        t0 = start_pg("pgsrv", rtname, tag)
        res["pg_first_query_s"].append(round(wait_query("pgsrv", t0)[0], 2))
        rm("pgsrv")
    for _ in range(5 if phase != "pg" else 0):
        rm("nginxsrv"); time.sleep(2)
        res["nginx_first_200_s"].append(round(start_nginx("nginxsrv", rtname)[0], 2))
        rm("nginxsrv")
    save(f"start-{rtname}" + (f"-{phase}" if phase else ""), res)
    return res


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "memjson":   # run as root by memsnap_remote()
        print(json.dumps(mem_total()))
        sys.exit(0)
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("prepare")
    for c in ("pg", "nginx"):
        p = sub.add_parser(c); p.add_argument("runtime", choices=RUNTIMES); p.add_argument("--quick", action="store_true")
    p = sub.add_parser("density"); p.add_argument("runtime", choices=RUNTIMES); p.add_argument("n", type=int)
    p = sub.add_parser("ndensity"); p.add_argument("runtime", choices=RUNTIMES); p.add_argument("n", type=int)
    p = sub.add_parser("start"); p.add_argument("runtime", choices=RUNTIMES)
    p = sub.add_parser("stopcheck"); p.add_argument("runtime", choices=RUNTIMES)
    a = ap.parse_args()
    try:
        if a.cmd == "prepare":
            prepare()
        elif a.cmd == "pg":
            pg(a.runtime, a.quick)
        elif a.cmd == "nginx":
            nginx(a.runtime, a.quick)
        elif a.cmd == "density":
            density(a.runtime, a.n)
        elif a.cmd == "start":
            start_times(a.runtime)
        elif a.cmd == "stopcheck":
            stopcheck(a.runtime)
        elif a.cmd == "ndensity":
            nginx_density(a.runtime, a.n)
    finally:
        if a.cmd != "prepare":
            dk("rm", "-f", "pgsrv", "nginxsrv", *[f"dens{i}" for i in range(64)], check=False, timeout=300)
