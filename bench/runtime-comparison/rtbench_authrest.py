#!/usr/bin/env python3
"""Auth (GoTrue) and PostgREST, the two small services of a KoedoDB Project, N pairs of them per runtime: what one VM costs when the program in it is small.

  rtbench_authrest.py prepare <dir>   pull the pinned images (koedodb-public/auth/IMAGE, postgrest/IMAGE), start the PostgreSQL they share (runc), prepare it; <dir> holds env files made by
                                      authrest-keys.sh (keys, tokens)
  rtbench_authrest.py run RUNTIME N   N Auth and N PostgREST on RUNTIME: idle memory, then under load (a sign-up loop against every Auth, GET /items against every PostgREST, at the same time)

The VM is the same for Kata and cchv: 2 vCPUs, 320 MiB. (A first run asked for 128 MiB: cchv does not make a VM smaller than its pool's default, 320 MiB, whatever the limit
(pod limit 64 MiB, which is what the Control Plane gives each of them, still got a 320 MiB VM), while Kata made 128 MiB. That run is kept as 20261027-authrest-128 for Kata and runc only.) runc gets 320 MiB.
The CPU limit is the VM's vCPU count minus one (see vm_limits), not the 100 millicores of production: the question here is memory, and whether 64 MiB is enough.
"""
import json, os, re, subprocess, sys, time
import rtbench as r

r.VM_SIZE["authrest"] = (2, 320)
r.PLAIN_LIMITS["authrest"] = ("2", 320)
HOME = os.path.expanduser("~")
HARDENED = ["--user", "10001:10001", "--read-only", "--tmpfs", "/tmp", "--cap-drop", "ALL", "--security-opt", "no-new-privileges"]
ROOT_PUBLIC = None   # the images are pulled by digest from the IMAGE lines the caller passes in the environment
AUTH_IMAGE = os.environ.get("AR_AUTH_IMAGE", "")
REST_IMAGE = os.environ.get("AR_REST_IMAGE", "")


def sql(db_ip, text):
    p = subprocess.run(r.DOCKER + ["exec", "-i", "arpg", "psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-At"], input=text, capture_output=True, text=True)
    if p.returncode:
        raise RuntimeError(p.stderr[:400])
    return p.stdout


def prepare(workdir):
    for img, tag in ((AUTH_IMAGE, "rtbench-auth"), (REST_IMAGE, "rtbench-rest")):
        r.dk("pull", "-q", img, timeout=900)
        r.dk("tag", img, tag)
    r.rm("arpg")
    r.dk("run", "-d", "--name", "arpg", "--runtime", "runc", "--cpus", "2", "-m", "2g", "-e", "POSTGRES_HOST_AUTH_METHOD=trust", r.POSTGRES, "-c", "max_connections=300", "-c", "fsync=off")
    for _ in range(60):
        if r.dk("exec", "arpg", "pg_isready", "-U", "postgres", check=False).returncode == 0:
            break
        time.sleep(1)
    time.sleep(3)
    sql(None, open(os.path.join(workdir, "init.sql")).read())
    print("database ready", r.container_ip("arpg"))


def common_args(name, rtname):
    rt = r.RUNTIMES[rtname]
    cpus, mem = r.vm_limits(rtname, "authrest")
    return ["run", "-d", "--name", name, "--runtime", rt["runtime"], "--cpus", cpus, "-m", mem, "--memory-swap", mem, "--pids-limit", "512"] + HARDENED


def start_pair(i, rtname, workdir, db_ip):
    auth, rest = f"ar-auth{i}", f"ar-rest{i}"
    env = open(os.path.join(workdir, "auth.env")).read().replace("@DB@", db_ip)
    envfile = os.path.join(workdir, f"auth-{i}.env"); open(envfile, "w").write(env + "GOTRUE_DB_MAX_POOL_SIZE=5\nGOTRUE_DB_MAX_IDLE_POOL_SIZE=5\n")
    r.dk(*common_args(auth, rtname), "--env-file", envfile, "rtbench-auth", timeout=300)
    renv = open(os.path.join(workdir, "rest.env")).read().replace("@DB@", db_ip)
    renvfile = os.path.join(workdir, f"rest-{i}.env"); open(renvfile, "w").write(renv + "PGRST_DB_POOL=5\n")
    r.dk(*common_args(rest, rtname), "--env-file", renvfile, "rtbench-rest", timeout=300)
    return auth, rest


def wait_ok(name, port, path, secs=180):
    ip = r.container_ip(name)
    end = time.time() + secs
    while time.time() < end:
        if r.dk("exec", "wrkc", "wget", "-q", "-O", "/dev/null", f"http://{ip}:{port}{path}", check=False, timeout=30).returncode == 0:
            return ip
        time.sleep(0.5)
    logs = r.dk("logs", "--tail", "15", name, check=False).stdout
    raise RuntimeError(f"{name} not healthy on {path}: {logs[-600:]}")


SIGNUP_LUA = '''local counter = 0
local idx = "0"
init = function(args) idx = args[1] or "0" end
request = function()
  counter = counter + 1
  local body = string.format('{"email":"u%s_%d@example.com","password":"correct-horse-battery-%d"}', idx, counter, counter)
  return wrk.format("POST", "/signup", {["Content-Type"]="application/json"}, body)
end
'''


def run(rtname, n, workdir):
    r.services(rtname)
    if rtname == "cloudhv":
        time.sleep(60)   # the daemon fills its pool of ready VMs after it starts: they are not the cost of a pod, so they must be in the baseline
    db_ip = r.container_ip("arpg")
    names = [f"ar-{k}{i}" for i in range(n) for k in ("auth", "rest")]
    for nm in names:
        r.rm(nm)
    time.sleep(3)
    base = r.memsnap_remote()
    t0 = time.time()
    pairs = []
    first = start_pair(0, rtname, workdir, db_ip)
    wait_ok(first[0], 9999, "/health")   # Auth migrates its schema at the first start: the others wait for it
    pairs.append(first)
    for i in range(1, n):
        pairs.append(start_pair(i, rtname, workdir, db_ip))
    auth_ips = [wait_ok(a, 9999, "/health") for a, _ in pairs]
    rest_ips = [wait_ok(b, 3001, "/ready") for _, b in pairs]
    all_up_s = round(time.time() - t0, 1)
    vms = r.check_vm(rtname, "authrest")
    time.sleep(60)
    idle = r.memsnap_remote()
    # the load: every Auth a sign-up loop, every PostgREST a GET with a token, all at the same time
    r.dk("exec", "-i", "wrkc", "sh", "-c", "cat > /tmp/signup.lua", input=SIGNUP_LUA)
    token = open(os.path.join(workdir, "anon.jwt")).read().strip()
    procs = [subprocess.Popen(r.DOCKER + ["exec", "wrkc", "wrk", "-t1", "-c4", "-d40s", "-s", "/tmp/signup.lua", f"http://{ip}:9999", "--", str(k)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) for k, ip in enumerate(auth_ips)]
    procs += [subprocess.Popen(r.DOCKER + ["exec", "wrkc", "wrk", "-t1", "-c8", "-d40s", "-H", f"Authorization: Bearer {token}", f"http://{ip}:3000/items?limit=20"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) for ip in rest_ips]
    time.sleep(25)
    load = r.memsnap_remote()
    outs = [p.communicate(timeout=180)[0] for p in procs]
    def stats(o):
        m = re.search(r"Requests/sec:\s+([0-9.]+)", o); e = re.search(r"Non-2xx or 3xx responses: (\d+)", o); t = re.search(r"([0-9]+) requests in", o)
        return {"rps": float(m.group(1)) if m else None, "requests": int(t.group(1)) if t else None, "non2xx": int(e.group(1)) if e else 0}
    auth_stats, rest_stats = [stats(o) for o in outs[:n]], [stats(o) for o in outs[n:]]
    states = {nm: r.dk("inspect", "-f", "{{.State.Status}} oom={{.State.OOMKilled}} restarts={{.RestartCount}}", nm, check=False).stdout.strip() for nm in names}
    res = {"runtime": rtname, "n_pairs": n, "vms": 2 * n, "vm_sizes": vms, "limits": list(r.vm_limits(rtname, "authrest")), "all_up_s": all_up_s,
           "mem_baseline": base, "mem_idle": idle, "mem_load": load,
           "per_vm_pss_idle_mib": round((idle["pss_mib"] - base["pss_mib"]) / (2 * n), 1), "per_vm_pss_load_mib": round((load["pss_mib"] - base["pss_mib"]) / (2 * n), 1),
           "per_vm_rss_idle_mib": round((idle["rss_mib"] - base["rss_mib"]) / (2 * n), 1), "per_vm_rss_load_mib": round((load["rss_mib"] - base["rss_mib"]) / (2 * n), 1),
           "auth": auth_stats, "rest": rest_stats, "states": states}
    r.save(f"authrest-{rtname}-{n}", res, "\n".join(outs))
    r.log(rtname, "auth+rest", n, "pairs:", res["per_vm_pss_idle_mib"], "MiB idle,", res["per_vm_pss_load_mib"], "MiB under load per VM; states", sorted(set(states.values())))
    for nm in names:
        r.rm(nm)
    return res


if __name__ == "__main__":
    if sys.argv[1] == "prepare":
        prepare(sys.argv[2])
    elif sys.argv[1] == "run":
        run(sys.argv[2], int(sys.argv[3]), sys.argv[4])
