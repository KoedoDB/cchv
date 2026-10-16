//! Running under Docker (or nerdctl, `ctr`): a client that is not Kubernetes.
//!
//! Kubernetes makes the pod's network namespace first (the CNI plugin) and gives its path in the spec; containerd's CRI plugin marks
//! the sandbox and the containers with annotations, and the sandbox's limits are on the sandbox's spec. Docker does it the other way
//! round, as it does with runc:
//!
//! - the spec has a `network` namespace **without a path**, which means "make a new one";
//! - right after `Create`, dockerd takes `/proc/<pid>/ns/net` of the task's pid (containerd's `State`), bind-mounts it under
//!   `/var/run/docker/netns/` and sets the veth and the IP up in it, and only then calls `Start`;
//! - there are no CRI annotations: one container is the whole "pod", and its limits are in `linux.resources`.
//!
//! So for such a spec the shim makes the namespace itself, at `Create`: a tiny holder process that is the shim binary run in a mode of
//! its own (`CLOUDHV_NETNS_HOLDER`), does `unshare(CLONE_NEWNET)` and sleeps. Its pid is what the task shows (`Instance::pid_hint`), its
//! `/proc/<pid>/ns/net` is the namespace the daemon wires the VM's TAP to. Everything here applies only to a spec without CRI
//! annotations: the Kubernetes path does not use any of it.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::vmsize::PodLimits;

/// The environment variable that turns the shim binary into the namespace holder.
pub const HOLDER_ENV: &str = "CLOUDHV_NETNS_HOLDER";

/// Whether the spec comes from containerd's CRI plugin (it marks every sandbox and container with this annotation).
pub fn is_cri(spec: &serde_json::Value) -> bool {
    spec.pointer("/annotations/io.kubernetes.cri.container-type").is_some()
}

/// Whether the spec asks the runtime to create a new network namespace: a `network` entry without a path.
pub fn wants_new_netns(spec: &serde_json::Value) -> bool {
    spec.pointer("/linux/namespaces")
        .and_then(|n| n.as_array())
        .map(|ns| {
            ns.iter().any(|n| {
                n.get("type").and_then(|t| t.as_str()) == Some("network")
                    && n.get("path").and_then(|p| p.as_str()).unwrap_or("").is_empty()
            })
        })
        .unwrap_or(false)
}

/// The limits of a container from its own `linux.resources` (`docker run -m 512m --cpus 2`): the same fields as the CRI annotations of a
/// sandbox, so that `vm_size` and its caps apply unchanged. A limit that is absent, zero or negative (Docker writes -1 for none) is none.
pub fn limits_from_resources(spec: &serde_json::Value) -> PodLimits {
    let memory_bytes = spec
        .pointer("/linux/resources/memory/limit")
        .and_then(|v| v.as_i64())
        .filter(|&b| b > 0)
        .map(|b| b as u64);
    let cpu_quota = spec
        .pointer("/linux/resources/cpu/quota")
        .and_then(|v| v.as_i64())
        .filter(|&q| q > 0);
    let cpu_period = spec
        .pointer("/linux/resources/cpu/period")
        .and_then(|v| v.as_u64())
        .filter(|&p| p > 0);
    PodLimits {
        memory_bytes,
        cpu_quota,
        cpu_period,
    }
}

/// Where the container's root file system is. Kubernetes (containerd's CRI plugin) leaves it to the shim: the mounts come with `Create` and the
/// shim mounts them at `<bundle>/rootfs` (`root.path` is `rootfs`, relative). Docker mounts the file system itself and writes the absolute
/// path of its mount in `root.path` (`/var/lib/docker/rootfs/overlayfs/<id>`), leaving `<bundle>/rootfs` empty. Under Docker an absolute
/// `root.path` is the root; for anything else it is `<bundle>/rootfs`, as it always was.
pub fn rootfs_path(bundle: &std::path::Path, spec: &serde_json::Value, docker: bool) -> std::path::PathBuf {
    if docker {
        if let Some(p) = spec.pointer("/root/path").and_then(|v| v.as_str()) {
            let p = std::path::Path::new(p);
            // (no `..`: the path is only ever read, but a dotted one says that something is wrong)
            if p.is_absolute() && !p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return p.to_path_buf();
            }
        }
    }
    bundle.join("rootfs")
}

/// Whether the text of `/proc/<pid>/net/dev` lists an interface other than `lo`: that is, whether the client has put a network into the
/// namespace (`docker run --network none` leaves only `lo`).
pub fn has_interface(net_dev: &str) -> bool {
    net_dev
        .lines()
        .skip(2) // the two header lines
        .filter_map(|l| l.split(':').next())
        .map(|n| n.trim())
        .any(|n| !n.is_empty() && n != "lo")
}

/// The process that holds a network namespace for a task.
pub struct NetnsHolder {
    pid: u32,
    child: Mutex<Option<Child>>,
}

impl NetnsHolder {
    /// Start the holder and wait until its namespace exists. Until then `/proc/<pid>/ns/net` would still be the shim's own (the
    /// host's): a client that took it then would set its network up in the host's namespace. So the pid is not given out before the
    /// holder says it has made the namespace, and the namespace is checked to be another one.
    pub fn spawn() -> Result<NetnsHolder, String> {
        // `/proc/self/exe` also works when the binary on disk has been replaced since this process started.
        let mut child = Command::new("/proc/self/exe")
            .env(HOLDER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start the network namespace holder: {e}"))?;
        let pid = child.id();
        let fail = |child: &mut Child, why: String| -> Result<NetnsHolder, String> {
            let _ = child.kill();
            let _ = child.wait();
            Err(why)
        };
        let mut ready = [0u8; 1];
        let got = child.stdout.take().map(|mut out| out.read(&mut ready));
        match got {
            Some(Ok(1)) if ready[0] == b'k' => {}
            other => return fail(&mut child, format!("the network namespace holder did not start ({other:?})")),
        }
        match (ns_inode(&format!("/proc/{pid}/ns/net")), ns_inode("/proc/self/ns/net")) {
            (Some(theirs), Some(ours)) if theirs != ours => {}
            _ => return fail(&mut child, "the holder is not in a network namespace of its own".into()),
        }
        Ok(NetnsHolder {
            pid,
            child: Mutex::new(Some(child)),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The path of the namespace (a path the daemon accepts with `allow_proc_netns`).
    pub fn netns_path(&self) -> String {
        format!("/proc/{}/ns/net", self.pid)
    }

    /// Whether a network has been put into the namespace.
    pub fn has_interface(&self) -> bool {
        std::fs::read_to_string(format!("/proc/{}/net/dev", self.pid))
            .map(|t| has_interface(&t))
            .unwrap_or(false)
    }

    /// Stop the holder, which ends the namespace once nothing else keeps it (dockerd's bind mount does until it removes it).
    pub fn kill(&self) {
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            // (the shim's runtime may reap its children first: then there is nothing left to wait for)
            let _ = c.wait();
        }
    }
}

impl Drop for NetnsHolder {
    fn drop(&mut self) {
        self.kill();
    }
}

fn ns_inode(path: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}

/// The holder's own `main`: make a network namespace, say so, and wait until the shim that started it has gone.
pub fn holder_main() -> ! {
    // Close what the shim had open (it may have descriptors without close-on-exec, a listening socket for one): the holder lives as long
    // as the container and must not keep any of them alive. Standard output stays, for the one byte of the handshake.
    unsafe {
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
            for fd in 3..1024 {
                libc::close(fd);
            }
        }
        if libc::unshare(libc::CLONE_NEWNET) != 0 {
            libc::_exit(1);
        }
        let parent = libc::getppid();
        let ok = b"k";
        if libc::write(1, ok.as_ptr() as *const libc::c_void, 1) != 1 {
            libc::_exit(2);
        }
        libc::close(1);
        // A parent-death signal would fire when the *thread* that started this process exits, which in the shim's runtime can happen at any
        // time; so the parent is watched by polling.
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if libc::getppid() != parent {
                libc::_exit(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_cri_spec_is_told_by_its_annotation() {
        assert!(is_cri(&json!({"annotations": {"io.kubernetes.cri.container-type": "sandbox"}})));
        assert!(is_cri(&json!({"annotations": {"io.kubernetes.cri.container-type": "container"}})));
        assert!(!is_cri(&json!({"annotations": {"other": "x"}})));
        assert!(!is_cri(&json!({})));
    }

    #[test]
    fn a_network_entry_without_a_path_asks_for_a_new_namespace() {
        let docker = json!({"linux": {"namespaces": [{"type": "pid"}, {"type": "network"}, {"type": "ipc"}]}});
        assert!(wants_new_netns(&docker));
        assert!(wants_new_netns(&json!({"linux": {"namespaces": [{"type": "network", "path": ""}]}})));
        // Kubernetes (the CNI's namespace), and a container that joins another one: a path
        assert!(!wants_new_netns(&json!({"linux": {"namespaces": [{"type": "network", "path": "/var/run/netns/cni-1"}]}})));
        // host networking: no network entry
        assert!(!wants_new_netns(&json!({"linux": {"namespaces": [{"type": "pid"}]}})));
        assert!(!wants_new_netns(&json!({})));
    }

    #[test]
    fn limits_come_from_the_resources_of_the_container() {
        let l = limits_from_resources(&json!({"linux": {"resources": {
            "memory": {"limit": 536870912}, "cpu": {"quota": 200000, "period": 100000}}}}));
        assert_eq!(l.memory_bytes, Some(536870912));
        assert_eq!(l.cpu_quota, Some(200000));
        assert_eq!(l.cpu_period, Some(100000));
        // none, zero and the -1 Docker writes are no limit
        assert_eq!(limits_from_resources(&json!({"linux": {"resources": {"memory": {"limit": -1}, "cpu": {"quota": -1, "period": 100000}}}})).memory_bytes, None);
        assert_eq!(limits_from_resources(&json!({"linux": {"resources": {"memory": {"limit": 0}}}})).memory_bytes, None);
        assert_eq!(limits_from_resources(&json!({"linux": {"resources": {"cpu": {"quota": -1}}}})).cpu_quota, None);
        assert_eq!(limits_from_resources(&json!({})), PodLimits::default());
        // a hostile or odd type is no limit, not a panic
        assert_eq!(limits_from_resources(&json!({"linux": {"resources": {"memory": {"limit": "lots"}}}})).memory_bytes, None);
    }

    #[test]
    fn under_docker_an_absolute_root_path_is_the_root_file_system() {
        use std::path::Path;
        let bundle = Path::new("/run/containerd/b1");
        let docker = json!({"root": {"path": "/var/lib/docker/rootfs/overlayfs/abc"}});
        assert_eq!(rootfs_path(bundle, &docker, true), Path::new("/var/lib/docker/rootfs/overlayfs/abc"));
        // Kubernetes: the bundle's rootfs, whatever the spec says
        assert_eq!(rootfs_path(bundle, &docker, false), Path::new("/run/containerd/b1/rootfs"));
        assert_eq!(rootfs_path(bundle, &json!({"root": {"path": "rootfs"}}), true), Path::new("/run/containerd/b1/rootfs"));
        assert_eq!(rootfs_path(bundle, &json!({}), true), Path::new("/run/containerd/b1/rootfs"));
        assert_eq!(rootfs_path(bundle, &json!({"root": {"path": "/a/../etc"}}), true), Path::new("/run/containerd/b1/rootfs"));
    }

    #[test]
    fn an_interface_other_than_loopback_means_a_network() {
        let header = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";
        let only_lo = format!("{header}    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n");
        assert!(!has_interface(&only_lo));
        let with_eth = format!("{only_lo}  eth0:     100       1    0    0    0     0          0         0       50       1    0    0    0     0       0          0\n");
        assert!(has_interface(&with_eth));
        assert!(!has_interface(""));
        assert!(!has_interface(header));
    }
}
