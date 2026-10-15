//! What the guest passes on to crun from the OCI spec the host sent.
//!
//! The spec comes from the pod's runtime (containerd) and carries things the pod's own YAML decided: capabilities, a privileged flag,
//! annotations, hooks, device nodes, sysctls. The container's code is hostile, and so is anything a pod can make the spec say. So
//! nothing is passed on by default: this module builds a new spec from an allowlist of fields, and for the fields it keeps it
//! removes what the guest must not give (capabilities that reach the guest kernel, device nodes, hooks).
//!
//! The VM is the boundary between pods. Inside it, the containers of one pod share the VM's network, UTS and IPC namespaces (as
//! containers of a pod do) and get their own mount and PID namespaces. There is no user namespace: the erofs/overlay rootfs and the
//! shared volumes are owned by root and a mapping would break them; instead the capabilities are cut to a short list, `no_new_privs`
//! is forced, the device cgroup allows the standard devices only, and `user.max_user_namespaces` is 0 so that a container cannot make
//! one for itself.

use anyhow::Result;
use serde_json::{json, Map, Value};

/// The capabilities a container may have. containerd's default set, less `MKNOD` (device nodes: the way to /dev/vdX and /dev/mem) and
/// `NET_RAW` (raw sockets: ARP/IP spoofing on the pod's network; `ping` works through unprivileged ICMP sockets, see the sysctl set at
/// init). Everything else (SYS_ADMIN, SYS_MODULE, SYS_RAWIO, NET_ADMIN, SYS_PTRACE, SYS_TIME, SYSLOG, MAC_*, BPF, ...) is not in the
/// list, so a pod that asks for it, or a `privileged` pod, does not get it.
pub const ALLOWED_CAPS: &[&str] = &[
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_NET_BIND_SERVICE",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_CHROOT",
];

/// Paths every container has masked or read-only, whatever the spec says (a privileged pod's spec has none).
const MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];
const READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// The highest soft or hard limit on open files a container may ask for (the kernel's default `nr_open`).
const MAX_NOFILE: u64 = 1_048_576;

/// A valid `capabilities` set from the spec's list: only the names in ALLOWED_CAPS, once each, in a stable order.
pub fn filter_caps(list: Option<&Value>) -> Vec<Value> {
    let asked: Vec<&str> = list
        .and_then(|l| l.as_array())
        .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
        .unwrap_or_default();
    ALLOWED_CAPS
        .iter()
        .filter(|c| asked.contains(c))
        .map(|c| json!(c))
        .collect()
}

/// The five capability sets, each cut to ALLOWED_CAPS. A spec with no `capabilities` at all gets the full allowed list in every set
/// except ambient (without the field, a runtime leaves the process with the root's full set).
fn sanitize_capabilities(caps: Option<&Value>) -> Value {
    let mut out = Map::new();
    match caps.and_then(|c| c.as_object()) {
        Some(c) => {
            for set in ["bounding", "effective", "inheritable", "permitted", "ambient"] {
                out.insert(set.to_string(), Value::Array(filter_caps(c.get(set))));
            }
        }
        None => {
            let all: Vec<Value> = ALLOWED_CAPS.iter().map(|c| json!(c)).collect();
            for set in ["bounding", "effective", "permitted"] {
                out.insert(set.to_string(), Value::Array(all.clone()));
            }
            out.insert("inheritable".to_string(), json!([]));
            out.insert("ambient".to_string(), json!([]));
        }
    }
    Value::Object(out)
}

/// Resource limits: the `RLIMIT_*` the spec lists, with the open-files limit capped.
fn sanitize_rlimits(rlimits: Option<&Value>) -> Value {
    let mut out = Vec::new();
    for r in rlimits.and_then(|r| r.as_array()).into_iter().flatten() {
        let Some(kind) = r.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        if !kind.starts_with("RLIMIT_") || kind.len() > 24 {
            continue;
        }
        let (mut hard, mut soft) = (
            r.get("hard").and_then(|v| v.as_u64()).unwrap_or(0),
            r.get("soft").and_then(|v| v.as_u64()).unwrap_or(0),
        );
        if kind == "RLIMIT_NOFILE" {
            hard = hard.min(MAX_NOFILE);
            soft = soft.min(MAX_NOFILE);
        }
        out.push(json!({"type": kind, "hard": hard, "soft": soft}));
    }
    Value::Array(out)
}

/// The `process` section: the fields a container's process needs, `no_new_privs` on, capabilities cut.
fn sanitize_process(process: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for key in ["terminal", "user", "args", "env", "cwd"] {
        if let Some(v) = process.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    out.insert(
        "capabilities".to_string(),
        sanitize_capabilities(process.get("capabilities")),
    );
    out.insert(
        "rlimits".to_string(),
        sanitize_rlimits(process.get("rlimits")),
    );
    // A setuid binary or file capability cannot raise a container's privileges.
    out.insert("noNewPrivileges".to_string(), json!(true));
    Value::Object(out)
}

/// The most arguments or environment entries an exec's process may have, and the most bytes in all of its strings.
const MAX_EXEC_STRINGS: usize = 4096;
const MAX_EXEC_BYTES: usize = 512 * 1024;
const MAX_EXEC_CWD: usize = 4096;

/// Intersect an exec's capability list with the container's: nothing the container does not have.
fn cap_intersection(asked: &[Value], container: Option<&Value>) -> Vec<Value> {
    match container.and_then(|c| c.as_array()) {
        Some(have) => asked.iter().filter(|c| have.contains(c)).cloned().collect(),
        None => asked.to_vec(),
    }
}

/// The OCI `Process` object for `crun exec`, from what a client sent (`process`, a JSON object) and the process section of the
/// container it runs in (`container`, from the container's bundle: already cut down by `sanitize_process`).
///
/// The same rules as a container's process, and never more than the container has:
/// - `args` (at least one), `env` (`NAME=value`), `cwd` (absolute) and `user` are kept after checking; strings have no NUL and the
///   whole is bounded;
/// - the five capability sets are cut to ALLOWED_CAPS and to the container's own set; without any, the container's sets;
/// - `rlimits` are the container's, each lowered (never raised) to what the exec asks;
/// - `noNewPrivileges` is on, `terminal` is `terminal` (the request's flag, not the JSON's);
/// - `apparmorProfile`, `selinuxLabel`, `oomScoreAdj` and everything else are dropped.
pub fn sanitize_exec_process(process: &Value, container: Option<&Value>, terminal: bool) -> Result<Value> {
    let p = process
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("the process is not an object"))?;
    let mut total = 0usize;
    let mut strings = |what: &str, v: Option<&Value>, need_eq: bool| -> Result<Vec<Value>> {
        let list = match v {
            None => return Ok(Vec::new()),
            Some(v) => v
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{what} is not a list"))?,
        };
        if list.len() > MAX_EXEC_STRINGS {
            anyhow::bail!("{what} has more than {MAX_EXEC_STRINGS} entries");
        }
        let mut out = Vec::with_capacity(list.len());
        for e in list {
            let s = e
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("an entry of {what} is not a string"))?;
            if s.contains('\0') {
                anyhow::bail!("an entry of {what} has a NUL");
            }
            if need_eq && !s.contains('=') {
                anyhow::bail!("an entry of {what} is not NAME=value");
            }
            total += s.len();
            if total > MAX_EXEC_BYTES {
                anyhow::bail!("the process is larger than {MAX_EXEC_BYTES} bytes");
            }
            out.push(e.clone());
        }
        Ok(out)
    };
    let args = strings("args", p.get("args"), false)?;
    if args.is_empty() {
        anyhow::bail!("the process has no args");
    }
    let env = strings("env", p.get("env"), true)?;

    let cwd = match p.get("cwd") {
        None => json!("/"),
        Some(c) => {
            let c = c
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("cwd is not a string"))?;
            if c.is_empty() {
                json!("/")
            } else if !c.starts_with('/') || c.len() > MAX_EXEC_CWD || c.contains('\0') {
                anyhow::bail!("cwd is not an absolute path");
            } else {
                json!(c)
            }
        }
    };

    let id = |v: Option<&Value>| -> Result<u32> {
        v.and_then(|x| x.as_u64())
            .and_then(|x| u32::try_from(x).ok())
            .ok_or_else(|| anyhow::anyhow!("user id is not a number"))
    };
    // The user: the exec's, or else the container's.
    let user = match p.get("user") {
        Some(u) => {
            let u = u
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("user is not an object"))?;
            let mut out = Map::new();
            out.insert("uid".to_string(), json!(id(u.get("uid"))?));
            out.insert("gid".to_string(), json!(id(u.get("gid"))?));
            if let Some(g) = u.get("additionalGids") {
                let g = g
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("additionalGids is not a list"))?;
                if g.len() > 64 {
                    anyhow::bail!("more than 64 additional groups");
                }
                let mut gids = Vec::new();
                for x in g {
                    gids.push(json!(id(Some(x))?));
                }
                out.insert("additionalGids".to_string(), Value::Array(gids));
            }
            if let Some(m) = u.get("umask").and_then(|m| m.as_u64()) {
                out.insert("umask".to_string(), json!(m & 0o777));
            }
            Value::Object(out)
        }
        None => container
            .and_then(|c| c.get("user"))
            .cloned()
            .unwrap_or_else(|| json!({"uid": 0, "gid": 0})),
    };

    // Capabilities: cut to the allowed set, and to the container's own.
    let container_caps = container.and_then(|c| c.get("capabilities")).and_then(|c| c.as_object());
    let asked_caps = p.get("capabilities").and_then(|c| c.as_object());
    let mut caps = Map::new();
    for set in ["bounding", "effective", "inheritable", "permitted", "ambient"] {
        // What the container has in this set (a container without any capabilities section has the allowed set, except for
        // inheritable and ambient, as `sanitize_capabilities` gives it).
        let have: Vec<Value> = match container_caps {
            Some(c) => filter_caps(c.get(set)),
            None if matches!(set, "inheritable" | "ambient") => Vec::new(),
            None => ALLOWED_CAPS.iter().map(|c| json!(c)).collect(),
        };
        let list = match asked_caps {
            Some(asked) => cap_intersection(&filter_caps(asked.get(set)), Some(&Value::Array(have))),
            None => have,
        };
        caps.insert(set.to_string(), Value::Array(list));
    }

    // Resource limits: the container's, lowered to what the exec asks for. An exec cannot raise a limit, and without any it keeps the
    // container's (not the agent's, which are root's).
    let mut rlimits = Vec::new();
    let container_limits = sanitize_rlimits(container.and_then(|c| c.get("rlimits")));
    let asked_limits = sanitize_rlimits(p.get("rlimits"));
    for l in container_limits.as_array().into_iter().flatten() {
        let kind = l.get("type").cloned().unwrap_or(Value::Null);
        let (mut hard, mut soft) = (
            l.get("hard").and_then(|v| v.as_u64()).unwrap_or(0),
            l.get("soft").and_then(|v| v.as_u64()).unwrap_or(0),
        );
        if let Some(a) = asked_limits
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a.get("type") == Some(&kind))
        {
            hard = hard.min(a.get("hard").and_then(|v| v.as_u64()).unwrap_or(hard));
            soft = soft.min(a.get("soft").and_then(|v| v.as_u64()).unwrap_or(soft));
        }
        rlimits.push(json!({"type": kind, "hard": hard, "soft": soft.min(hard)}));
    }

    let mut out = Map::new();
    out.insert("terminal".to_string(), json!(terminal));
    out.insert("user".to_string(), user);
    out.insert("args".to_string(), Value::Array(args));
    out.insert("env".to_string(), Value::Array(env));
    out.insert("cwd".to_string(), cwd);
    out.insert("capabilities".to_string(), Value::Object(caps));
    out.insert("rlimits".to_string(), Value::Array(rlimits));
    out.insert("noNewPrivileges".to_string(), json!(true));
    if terminal {
        if let Some(sz) = p.get("consoleSize").and_then(|c| c.as_object()) {
            let dim = |k: &str| sz.get(k).and_then(|v| v.as_u64()).map(|v| v.min(1000)).unwrap_or(0);
            out.insert(
                "consoleSize".to_string(),
                json!({"height": dim("height"), "width": dim("width")}),
            );
        }
    }
    Ok(Value::Object(out))
}

/// The device cgroup rules: nothing is allowed except the standard devices, which are the ones crun puts in the container's /dev.
/// (A device node that is not allowed cannot be opened even if one is made; making one needs CAP_MKNOD, which is not granted.)
pub fn default_devices() -> Value {
    json!([
        {"allow": false, "access": "rwm"},
        {"allow": true, "type": "c", "major": 1, "minor": 3, "access": "rwm"},   // null
        {"allow": true, "type": "c", "major": 1, "minor": 5, "access": "rwm"},   // zero
        {"allow": true, "type": "c", "major": 1, "minor": 7, "access": "rwm"},   // full
        {"allow": true, "type": "c", "major": 1, "minor": 8, "access": "rwm"},   // random
        {"allow": true, "type": "c", "major": 1, "minor": 9, "access": "rwm"},   // urandom
        {"allow": true, "type": "c", "major": 5, "minor": 0, "access": "rwm"},   // tty
        {"allow": true, "type": "c", "major": 5, "minor": 2, "access": "rwm"},   // ptmx
        {"allow": true, "type": "c", "major": 136, "access": "rwm"}              // pts/*
    ])
}

fn union_paths(spec_list: Option<&Value>, defaults: &[&str]) -> Value {
    let mut out: Vec<String> = defaults.iter().map(|s| s.to_string()).collect();
    for p in spec_list.and_then(|l| l.as_array()).into_iter().flatten() {
        if let Some(p) = p.as_str() {
            if p.starts_with('/') && !p.contains("..") && !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    }
    json!(out)
}

/// The `linux` section: the namespaces the guest wants, a cgroup of the container's own with its limits and the device rules, the
/// masked and read-only paths, and the seccomp profile (the pod's with the guest's denials on top, or the default one). Device nodes, sysctls, ID mappings, and the rest of what the
/// spec could carry are dropped.
fn sanitize_linux(linux: &Map<String, Value>, cgroup_id: &str) -> Value {
    let mut out = Map::new();
    out.insert(
        "namespaces".to_string(),
        json!([{"type": "mount"}, {"type": "pid"}]),
    );

    // The container's own memory, CPU and pids limits are kept (see container::sanitize_resources); the cgroup is made even without
    // limits, because the device rules live in it.
    let mut resources = match crate::container::sanitize_resources(linux.get("resources")) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    resources.insert("devices".to_string(), default_devices());
    out.insert("resources".to_string(), Value::Object(resources));
    out.insert("cgroupsPath".to_string(), json!(format!("/{cgroup_id}")));

    out.insert(
        "maskedPaths".to_string(),
        union_paths(linux.get("maskedPaths"), MASKED_PATHS),
    );
    out.insert(
        "readonlyPaths".to_string(),
        union_paths(linux.get("readonlyPaths"), READONLY_PATHS),
    );
    out.insert("rootfsPropagation".to_string(), json!("rprivate"));

    // Every container has a seccomp profile: the spec's, with the guest's denials on top, or the default one (see seccomp.rs). The
    // listener fields of a profile are removed there: crun would connect, as root in the guest, to a socket the spec names.
    out.insert("seccomp".to_string(), crate::seccomp::enforce(linux.get("seccomp")));
    Value::Object(out)
}

/// The spec to give crun: only the sections below, rebuilt, with `mounts` as the guest's own list. Fails if the spec is not an object or
/// has no `process` or `linux` (a spec without `linux` would get no namespaces forced on it).
///
/// Dropped: `hostname` (the VM's is set by the agent), `hooks`, `annotations` (crun acts on some), `domainname`, and everything else.
pub fn sanitize_spec(spec: &Value, cgroup_id: &str, mounts: Vec<Value>) -> Result<Value> {
    let obj = spec
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("the OCI spec is not an object"))?;
    let linux = obj
        .get("linux")
        .and_then(|l| l.as_object())
        .ok_or_else(|| anyhow::anyhow!("the OCI spec has no linux section"))?;
    let process = obj
        .get("process")
        .and_then(|p| p.as_object())
        .ok_or_else(|| anyhow::anyhow!("the OCI spec has no process section"))?;

    let mut out = Map::new();
    out.insert(
        "ociVersion".to_string(),
        obj.get("ociVersion")
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or_else(|| json!("1.0.2")),
    );
    // The rootfs is the overlay the agent mounted at <bundle>/rootfs; a spec cannot point crun at any other directory.
    out.insert(
        "root".to_string(),
        json!({
            "path": "rootfs",
            "readonly": spec.pointer("/root/readonly").and_then(|r| r.as_bool()).unwrap_or(false),
        }),
    );
    out.insert("process".to_string(), sanitize_process(process));
    out.insert("mounts".to_string(), Value::Array(mounts));
    out.insert("linux".to_string(), sanitize_linux(linux, cgroup_id));
    Ok(Value::Object(out))
}

/// The pod's hostname from the spec, if it is one the guest will set.
pub fn hostname_of(spec: &Value) -> Option<String> {
    let name = spec.get("hostname")?.as_str()?;
    valid_hostname(name).then(|| name.to_string())
}

/// 1 to 63 characters of `[A-Za-z0-9.-]`, not starting or ending with a dot or hyphen.
pub fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !name.starts_with(['.', '-'])
        && !name.ends_with(['.', '-'])
}

/// The guest's own mounts for every container: /proc, a tmpfs /dev with the standard nodes, /dev/pts, /dev/shm and a read-only /sys.
pub fn base_mounts() -> Vec<Value> {
    vec![
        json!({"destination": "/proc", "type": "proc", "source": "proc",
            "options": ["nosuid", "noexec", "nodev"]}),
        json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
            "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]}),
        json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts",
            "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]}),
        json!({"destination": "/dev/shm", "type": "tmpfs", "source": "shm",
            "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]}),
        json!({"destination": "/sys", "type": "sysfs", "source": "sysfs",
            "options": ["nosuid", "noexec", "nodev", "ro"]}),
    ]
}

/// A volume's destination in the container: absolute, no `..`, and not inside /proc or /sys (the guest's own mounts).
pub fn valid_destination(dest: &str) -> bool {
    if !dest.starts_with('/') || dest.contains('\0') {
        return false;
    }
    let mut parts = dest.split('/').filter(|p| !p.is_empty());
    match parts.next() {
        None => false, // "/"
        Some("proc") | Some("sys") => false,
        Some(first) => first != ".." && !parts.any(|p| p == ".."),
    }
}

/// Mount options for a volume: always `nosuid` and `nodev`; `ro` for a read-only volume; `noexec`, `noatime` and `relatime` only if
/// the host asked for them (a ConfigMap script or an emptyDir with a program in it must run unless the pod says not).
pub fn volume_options(readonly: bool, asked: &[String], extra: &[&str]) -> Vec<String> {
    let mut o: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    o.push("nosuid".to_string());
    o.push("nodev".to_string());
    if readonly {
        o.push("ro".to_string());
    }
    for a in asked {
        if matches!(a.as_str(), "noexec" | "noatime" | "relatime") && !o.contains(a) {
            o.push(a.clone());
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn containerd_spec() -> Value {
        json!({
            "ociVersion": "1.1.0",
            "hostname": "web-1",
            "annotations": {"run.oci.hooks.stdout": "/etc/shadow", "io.kubernetes.cri.sandbox-memory": "1"},
            "hooks": {"prestart": [{"path": "/bin/sh"}]},
            "root": {"path": "/some/host/path", "readonly": true},
            "process": {
                "terminal": false,
                "user": {"uid": 1000, "gid": 1000},
                "args": ["sleep", "1"],
                "env": ["A=b"],
                "cwd": "/",
                "apparmorProfile": "x",
                "oomScoreAdj": 1000,
                "noNewPrivileges": false,
                "capabilities": {
                    "bounding": ["CAP_CHOWN", "CAP_MKNOD", "CAP_NET_RAW", "CAP_SYS_ADMIN", "CAP_SYS_RAWIO", "CAP_KILL"],
                    "effective": ["CAP_CHOWN", "CAP_MKNOD", "CAP_SYS_MODULE"],
                    "permitted": ["CAP_CHOWN", "CAP_NET_ADMIN"],
                    "inheritable": ["CAP_SYS_PTRACE"],
                    "ambient": ["CAP_NET_BIND_SERVICE", "CAP_SYS_TIME"]
                },
                "rlimits": [
                    {"type": "RLIMIT_NOFILE", "hard": 18446744073709551615u64, "soft": 2000000},
                    {"type": "RLIMIT_NPROC", "hard": 4096, "soft": 4096},
                    {"type": "not-an-rlimit", "hard": 1, "soft": 1}
                ]
            },
            "linux": {
                "namespaces": [{"type": "network", "path": "/var/run/netns/cni-x"}, {"type": "user"}],
                "uidMappings": [{"hostID": 0, "containerID": 0, "size": 1}],
                "devices": [{"path": "/dev/vda", "type": "b", "major": 254, "minor": 0}],
                "sysctl": {"kernel.core_pattern": "|/bin/sh"},
                "cgroupsPath": "/kubepods/x",
                "rootfsPropagation": "shared",
                "maskedPaths": ["/proc/kcore", "/extra"],
                "readonlyPaths": [],
                "resources": {
                    "devices": [{"allow": true, "access": "rwm"}],
                    "memory": {"limit": 104857600},
                    "cpu": {"cpus": "0-3"}
                },
                "seccomp": {"defaultAction": "SCMP_ACT_ERRNO", "listenerPath": "/run/x.sock", "syscalls": []}
            }
        })
    }

    #[test]
    fn only_the_allowed_sections_and_fields_are_passed_on() {
        let out = sanitize_spec(&containerd_spec(), "abc", vec![json!({"destination": "/data"})]).unwrap();
        let top: Vec<&str> = out.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let mut top = top;
        top.sort();
        assert_eq!(top, ["linux", "mounts", "ociVersion", "process", "root"]);
        assert_eq!(out["ociVersion"], "1.1.0");
        // hooks, annotations and hostname are gone; the root cannot be pointed elsewhere
        assert_eq!(out["root"], json!({"path": "rootfs", "readonly": true}));
        assert!(out["process"].get("apparmorProfile").is_none());
        assert!(out["process"].get("oomScoreAdj").is_none());
        assert_eq!(out["mounts"], json!([{"destination": "/data"}]));
        let linux = out["linux"].as_object().unwrap();
        let mut keys: Vec<&str> = linux.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["cgroupsPath", "maskedPaths", "namespaces", "readonlyPaths", "resources", "rootfsPropagation", "seccomp"]
        );
        assert_eq!(linux["namespaces"], json!([{"type": "mount"}, {"type": "pid"}]));
        assert_eq!(linux["cgroupsPath"], "/abc");
        assert_eq!(linux["rootfsPropagation"], "rprivate");
    }

    #[test]
    fn a_spec_that_is_not_an_object_or_has_no_linux_or_process_is_refused() {
        assert!(sanitize_spec(&json!([1]), "a", vec![]).is_err());
        assert!(sanitize_spec(&json!({"process": {}}), "a", vec![]).is_err());
        assert!(sanitize_spec(&json!({"linux": {}}), "a", vec![]).is_err());
        assert!(sanitize_spec(&json!({"linux": 3, "process": {}}), "a", vec![]).is_err());
        assert!(sanitize_spec(&json!({"linux": {}, "process": {}}), "a", vec![]).is_ok());
    }

    #[test]
    fn capabilities_are_cut_to_the_allowlist_in_every_set() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        let caps = &out["process"]["capabilities"];
        assert_eq!(caps["bounding"], json!(["CAP_CHOWN", "CAP_KILL"]));
        assert_eq!(caps["effective"], json!(["CAP_CHOWN"]));
        assert_eq!(caps["permitted"], json!(["CAP_CHOWN"]));
        assert_eq!(caps["inheritable"], json!([]));
        assert_eq!(caps["ambient"], json!(["CAP_NET_BIND_SERVICE"]));
        let text = caps.to_string();
        for banned in ["MKNOD", "NET_RAW", "SYS_ADMIN", "SYS_RAWIO", "SYS_MODULE", "NET_ADMIN", "SYS_PTRACE", "SYS_TIME"] {
            assert!(!text.contains(banned), "{banned} must not survive");
        }
    }

    #[test]
    fn a_spec_without_capabilities_gets_the_allowed_set_not_the_roots_full_one() {
        let out = sanitize_spec(&json!({"linux": {}, "process": {}}), "a", vec![]).unwrap();
        let caps = &out["process"]["capabilities"];
        assert_eq!(caps["bounding"].as_array().unwrap().len(), ALLOWED_CAPS.len());
        assert_eq!(caps["ambient"], json!([]));
    }

    #[test]
    fn a_privileged_looking_spec_gets_nothing_extra() {
        // containerd's privileged pod: every capability, no masked paths, all devices.
        let all = ["CAP_SYS_ADMIN", "CAP_NET_ADMIN", "CAP_SYS_MODULE", "CAP_MKNOD", "CAP_CHOWN"];
        let spec = json!({
            "process": {"capabilities": {"bounding": all, "effective": all, "permitted": all, "inheritable": all, "ambient": all},
                "noNewPrivileges": false},
            "linux": {"maskedPaths": [], "readonlyPaths": [], "resources": {"devices": [{"allow": true, "access": "rwm"}]}}
        });
        let out = sanitize_spec(&spec, "p", vec![]).unwrap();
        assert_eq!(out["process"]["capabilities"]["effective"], json!(["CAP_CHOWN"]));
        assert_eq!(out["process"]["noNewPrivileges"], true);
        assert!(out["linux"]["maskedPaths"].as_array().unwrap().contains(&json!("/proc/kcore")));
        assert!(out["linux"]["readonlyPaths"].as_array().unwrap().contains(&json!("/proc/sys")));
        assert_eq!(out["linux"]["resources"]["devices"][0], json!({"allow": false, "access": "rwm"}));
    }

    #[test]
    fn no_new_privileges_is_forced_and_nothing_else_stray_is_left() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        assert_eq!(out["process"]["noNewPrivileges"], true);
        assert!(out["process"].get("noNewPrivs").is_none());
    }

    #[test]
    fn devices_are_denied_but_the_standard_ones_and_the_limits_are_kept() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        let res = &out["linux"]["resources"];
        assert_eq!(res["devices"], default_devices());
        assert_eq!(res["devices"][0], json!({"allow": false, "access": "rwm"}));
        assert_eq!(res["memory"], json!({"limit": 104857600}));
        // a container without limits still gets a cgroup (the device rules are in it)
        let bare = sanitize_spec(&json!({"linux": {}, "process": {}}), "z", vec![]).unwrap();
        assert_eq!(bare["linux"]["cgroupsPath"], "/z");
        assert_eq!(bare["linux"]["resources"], json!({"devices": default_devices()}));
        // no block device node and no spec device list gets through
        assert!(out["linux"].get("devices").is_none());
        assert!(!out["linux"]["resources"].to_string().contains("\"b\""));
    }

    #[test]
    fn masked_and_readonly_paths_are_the_defaults_plus_the_specs_valid_extras() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        let masked = out["linux"]["maskedPaths"].as_array().unwrap();
        assert!(masked.contains(&json!("/proc/kcore")));
        assert!(masked.contains(&json!("/extra")));
        assert_eq!(masked.iter().filter(|p| **p == json!("/proc/kcore")).count(), 1);
        let more = json!({"linux": {"maskedPaths": ["relative", "/a/../b", 3]}, "process": {}});
        let out = sanitize_spec(&more, "a", vec![]).unwrap();
        assert_eq!(out["linux"]["maskedPaths"].as_array().unwrap().len(), MASKED_PATHS.len());
    }

    #[test]
    fn rlimits_keep_only_rlimit_names_and_cap_open_files() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        assert_eq!(
            out["process"]["rlimits"],
            json!([
                {"type": "RLIMIT_NOFILE", "hard": 1048576, "soft": 1048576},
                {"type": "RLIMIT_NPROC", "hard": 4096, "soft": 4096}
            ])
        );
    }

    #[test]
    fn a_seccomp_listener_is_dropped_but_the_profile_is_kept() {
        let out = sanitize_spec(&containerd_spec(), "a", vec![]).unwrap();
        assert_eq!(out["linux"]["seccomp"]["defaultAction"], "SCMP_ACT_ERRNO");
        assert!(out["linux"]["seccomp"].get("listenerPath").is_none());
    }

    #[test]
    fn hostnames_are_letters_digits_dots_and_hyphens() {
        assert!(valid_hostname("web-1.default"));
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("a b"));
        assert!(!valid_hostname("a;rm"));
        assert!(!valid_hostname("-a"));
        assert!(!valid_hostname("a."));
        assert!(!valid_hostname(&"a".repeat(64)));
        assert_eq!(hostname_of(&json!({"hostname": "web-1"})), Some("web-1".to_string()));
        assert_eq!(hostname_of(&json!({"hostname": "web 1"})), None);
        assert_eq!(hostname_of(&json!({})), None);
    }

    #[test]
    fn destinations_must_be_absolute_and_stay_out_of_proc_and_sys() {
        assert!(valid_destination("/data"));
        assert!(valid_destination("/dev/termination-log"));
        assert!(valid_destination("/var/run/secrets/kubernetes.io/serviceaccount"));
        assert!(!valid_destination("data"));
        assert!(!valid_destination("/"));
        assert!(!valid_destination("/proc/sys"));
        assert!(!valid_destination("/sys/fs/cgroup"));
        assert!(!valid_destination("/data/../proc"));
        assert!(!valid_destination("/.."));
    }

    #[test]
    fn volume_options_are_nosuid_nodev_and_only_asked_extras() {
        let o = volume_options(false, &[], &["rbind"]);
        assert_eq!(o, ["rbind", "nosuid", "nodev"]);
        let o = volume_options(true, &["noexec".into(), "suid".into(), "rw".into(), "noexec".into()], &["rbind"]);
        assert_eq!(o, ["rbind", "nosuid", "nodev", "ro", "noexec"]);
    }

    #[test]
    fn the_base_mounts_have_no_suid_and_dev_shm_is_noexec() {
        let m = base_mounts();
        for mount in &m {
            let opts = mount["options"].to_string();
            assert!(opts.contains("nosuid"), "{mount}");
        }
        let shm = m.iter().find(|x| x["destination"] == "/dev/shm").unwrap();
        assert!(shm["options"].to_string().contains("noexec"));
    }

    fn container_process() -> Value {
        json!({
            "user": {"uid": 1000, "gid": 1000},
            "capabilities": {
                "bounding": ["CAP_CHOWN", "CAP_KILL", "CAP_SETUID"],
                "effective": ["CAP_CHOWN"],
                "inheritable": [],
                "permitted": ["CAP_CHOWN", "CAP_KILL"],
                "ambient": []
            },
            "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 512}, {"type": "RLIMIT_NPROC", "hard": 100, "soft": 100}]
        })
    }

    #[test]
    fn an_exec_gets_the_hardening_of_a_container_and_never_more_than_the_container_has() {
        let asked = json!({
            "args": ["sh", "-c", "id"],
            "env": ["A=b", "PATH=/usr/bin"],
            "cwd": "/tmp",
            "terminal": false,
            "apparmorProfile": "x", "selinuxLabel": "y", "oomScoreAdj": -1000,
            "noNewPrivileges": false,
            "capabilities": {
                "bounding": ["CAP_SYS_ADMIN", "CAP_CHOWN", "CAP_NET_RAW", "CAP_SETFCAP"],
                "effective": ["CAP_SYS_ADMIN", "CAP_CHOWN", "CAP_KILL"],
                "permitted": ["CAP_CHOWN"],
                "ambient": ["CAP_CHOWN"]
            },
            "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 4096, "soft": 4096}, {"type": "RLIMIT_NPROC", "hard": 10, "soft": 5},
                        {"type": "RLIMIT_CORE", "hard": 99, "soft": 99}]
        });
        let p = sanitize_exec_process(&asked, Some(&container_process()), false).unwrap();
        assert_eq!(p["args"], json!(["sh", "-c", "id"]));
        assert_eq!(p["env"], json!(["A=b", "PATH=/usr/bin"]));
        assert_eq!(p["cwd"], json!("/tmp"));
        assert_eq!(p["terminal"], json!(false));
        assert_eq!(p["noNewPrivileges"], json!(true), "forced on");
        assert!(p.get("apparmorProfile").is_none() && p.get("selinuxLabel").is_none() && p.get("oomScoreAdj").is_none());
        // The user is the container's when the exec names none.
        assert_eq!(p["user"], json!({"uid": 1000, "gid": 1000}));
        // Capabilities: in the allowed list AND in the container's set. SYS_ADMIN and NET_RAW never; SETFCAP and KILL are allowed in
        // general but this container's bounding set has no SETFCAP, and its effective set has no KILL.
        assert_eq!(p["capabilities"]["bounding"], json!(["CAP_CHOWN"]));
        assert_eq!(p["capabilities"]["effective"], json!(["CAP_CHOWN"]));
        assert_eq!(p["capabilities"]["permitted"], json!(["CAP_CHOWN"]));
        assert_eq!(p["capabilities"]["ambient"], json!([]), "the container has no ambient set");
        // Limits: the container's, lowered. NOFILE 4096 is above the container's 1024, so it stays 1024; NPROC is lowered; CORE is not
        // one the container has.
        let lim = p["rlimits"].as_array().unwrap();
        assert_eq!(lim.len(), 2);
        assert_eq!(lim[0], json!({"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 512}));
        assert_eq!(lim[1], json!({"type": "RLIMIT_NPROC", "hard": 10, "soft": 5}));
    }

    #[test]
    fn an_exec_without_capabilities_or_limits_of_its_own_has_the_containers() {
        let p = sanitize_exec_process(&json!({"args": ["ls"]}), Some(&container_process()), false).unwrap();
        assert_eq!(p["capabilities"]["bounding"], json!(["CAP_CHOWN", "CAP_KILL", "CAP_SETUID"]));
        assert_eq!(p["capabilities"]["permitted"], json!(["CAP_CHOWN", "CAP_KILL"]));
        assert_eq!(p["rlimits"].as_array().unwrap().len(), 2);
        assert_eq!(p["cwd"], json!("/"), "no cwd means /");
        // A container that has no capabilities section gets the allowed list, like a container does.
        let q = sanitize_exec_process(&json!({"args": ["ls"]}), None, false).unwrap();
        assert_eq!(q["capabilities"]["bounding"].as_array().unwrap().len(), ALLOWED_CAPS.len());
        assert_eq!(q["capabilities"]["ambient"], json!([]));
        assert_eq!(q["user"], json!({"uid": 0, "gid": 0}));
    }

    #[test]
    fn an_exec_with_a_user_of_its_own_and_a_terminal() {
        let asked = json!({"args": ["sh"], "user": {"uid": 5, "gid": 6, "additionalGids": [7, 8], "umask": 18},
                           "consoleSize": {"height": 50000, "width": 80}, "terminal": false});
        let p = sanitize_exec_process(&asked, Some(&container_process()), true).unwrap();
        assert_eq!(p["terminal"], json!(true), "the request's flag decides, not the JSON's");
        assert_eq!(p["user"], json!({"uid": 5, "gid": 6, "additionalGids": [7, 8], "umask": 18}));
        assert_eq!(p["consoleSize"], json!({"height": 1000, "width": 80}));
        let no_term = sanitize_exec_process(&asked, Some(&container_process()), false).unwrap();
        assert!(no_term.get("consoleSize").is_none());
    }

    #[test]
    fn a_bad_exec_process_is_refused() {
        let c = Some(container_process());
        let bad = |v: Value| sanitize_exec_process(&v, c.as_ref(), false).is_err();
        assert!(bad(json!("sh")), "not an object");
        assert!(bad(json!({})), "no args");
        assert!(bad(json!({"args": []})));
        assert!(bad(json!({"args": "sh"})));
        assert!(bad(json!({"args": [1]})));
        assert!(bad(json!({"args": ["a\u{0}b"]})), "NUL");
        assert!(bad(json!({"args": ["x"], "env": ["NOEQUALS"]})));
        assert!(bad(json!({"args": ["x"], "env": "A=b"})));
        assert!(bad(json!({"args": ["x"], "cwd": "relative"})));
        assert!(bad(json!({"args": ["x"], "cwd": 5})));
        assert!(bad(json!({"args": ["x"], "user": {"uid": -1, "gid": 0}})));
        assert!(bad(json!({"args": ["x"], "user": {"uid": 0}})));
        assert!(bad(json!({"args": ["x"], "user": {"uid": 0, "gid": 0, "additionalGids": (0..65).collect::<Vec<u32>>()}})));
        assert!(bad(json!({"args": (0..4097).map(|_| "a").collect::<Vec<_>>()})), "too many args");
        let big = "x".repeat(MAX_EXEC_BYTES + 1);
        assert!(bad(json!({"args": [big]})), "too large");
        // An empty cwd is the root.
        assert_eq!(
            sanitize_exec_process(&json!({"args": ["x"], "cwd": ""}), c.as_ref(), false).unwrap()["cwd"],
            json!("/")
        );
    }
}
