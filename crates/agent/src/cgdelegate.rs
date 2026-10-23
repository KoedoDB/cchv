//! A delegated cgroup subtree for one container (Pod annotation `cloudhv.koedodb.com/cgroup-delegate`, daemon key
//! `allow_cgroup_delegate`).
//!
//! A container marked by name gets, inside the VM, a cgroup of its own that it may subdivide: it can make child cgroups and set
//! `memory.max`, `cpu.max` and `pids.max` on them for the processes it starts. Nothing else changes, and the container gets no capability
//! from this (it loses some, see [`STRIPPED_CAPS`]). What the guest does, following the kernel's rules for delegation
//! (Documentation/admin-guide/cgroup-v2.rst, "Delegation"):
//!
//! 1. Before `crun run`, the agent makes `/sys/fs/cgroup/<id>` (the cgroup crun then uses for the container: `cgroupsPath` is `/<id>`; the
//!    limits and the device rules stay on it and are applied by crun as root). The controllers are enabled at the root at boot, so the
//!    interface files exist. The directory and `cgroup.procs`, `cgroup.threads`, `cgroup.subtree_control` belong to the container's
//!    user; **every other file of it** (`memory.max`, `cpu.max`, `pids.max`, `memory.high`, `cgroup.kill`, ...) belongs to `nobody`, so
//!    the container cannot raise the limit that the platform put on it.
//! 2. The container is in a cgroup namespace whose root is that cgroup, and sees a cgroup2 file system that shows only it: no parent,
//!    no sibling, no way to name a cgroup outside. The guest mounts cgroup2 with `nsdelegate`, so the kernel also treats the namespace
//!    root as a delegation boundary (a process cannot be moved across it, from either side).
//! 3. `CAP_CHOWN`, `CAP_DAC_OVERRIDE` and `CAP_FOWNER` are removed from a delegated container: with any of them, root in the container
//!    could take or override the files of point 1 (a root container without them is only the owner of what is its own).
//!
//! The container's first process starts in the delegated cgroup itself. The kernel's "no internal processes" rule means it must move into
//! a leaf child (`mkdir leaf; echo $$ > leaf/cgroup.procs`) before it writes `+memory` to `cgroup.subtree_control`.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Where the guest mounts the cgroup2 file system.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The owner of the files the container must not write.
pub const NOBODY: u32 = 65534;
/// The annotation containerd sets on every container (a Pod's own annotations cannot override it) with the name from the Pod's spec.
const NAME_ANNOTATION: &str = "io.kubernetes.cri.container-name";
/// The files the container owns (the ones the kernel itself hands to the user of a delegated cgroup).
const DELEGATED_FILES: &[&str] = &["cgroup.procs", "cgroup.threads", "cgroup.subtree_control"];
/// Capabilities taken from a delegated container: they would let root change the owner or the permissions of the limit files, or
/// ignore them.
pub const STRIPPED_CAPS: &[&str] = &["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER"];

/// Whether this container is one the Pod's annotation names. The annotation holds container names, separated by commas. A container
/// without a name (a client that is not Kubernetes) is never delegated to, and neither is one the list does not name.
pub fn requested(spec: &Value) -> bool {
    let annotations = spec.get("annotations");
    let list = annotations
        .and_then(|a| a.get(cloudhv_common::CGROUP_DELEGATE_ANNOTATION))
        .and_then(|v| v.as_str());
    let name = annotations
        .and_then(|a| a.get(NAME_ANNOTATION))
        .and_then(|v| v.as_str());
    match (list, name) {
        (Some(list), Some(name)) if !name.is_empty() => {
            list.split(',').map(str::trim).any(|n| n == name)
        }
        _ => false,
    }
}

/// The user and group of the container's process (what the files of the delegated cgroup are given to), from the cleaned spec.
pub fn identity(clean: &Value) -> (u32, u32) {
    let id = |k: &str| {
        clean
            .pointer(&format!("/process/user/{k}"))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0)
    };
    (id("uid"), id("gid"))
}

/// Make the container's cgroup under `root` and hand out its files as described above. `chown` is a parameter so that the logic can be
/// tested on a directory that is not a cgroup file system. Returns the directory.
pub fn prepare_dir(
    root: &Path,
    id: &str,
    uid: u32,
    gid: u32,
    chown: &mut dyn FnMut(&Path, u32, u32) -> io::Result<()>,
) -> Result<PathBuf> {
    // The id is a plain word already (`valid_container_id`); this is the last line against a path that leaves `root`.
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        anyhow::bail!("bad container id for a cgroup");
    }
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("make cgroup {}", dir.display()))?;
    let result = hand_out(&dir, uid, gid, chown);
    if result.is_err() {
        let _ = std::fs::remove_dir(&dir);
    }
    result.map(|()| dir)
}

fn hand_out(
    dir: &Path,
    uid: u32,
    gid: u32,
    chown: &mut dyn FnMut(&Path, u32, u32) -> io::Result<()>,
) -> Result<()> {
    // Every file first (to nobody), then the three that are the container's, and the directory last.
    for entry in std::fs::read_dir(dir).context("read the cgroup")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let (u, g) = if DELEGATED_FILES.iter().any(|d| name == *d) {
            (uid, gid)
        } else {
            (NOBODY, NOBODY)
        };
        chown(&entry.path(), u, g).with_context(|| format!("chown {}", entry.path().display()))?;
    }
    chown(dir, uid, gid).context("chown the cgroup directory")?;
    Ok(())
}

/// The real thing: the cgroup under [`CGROUP_ROOT`], files handed out with chown(2).
pub fn prepare(id: &str, uid: u32, gid: u32) -> Result<PathBuf> {
    prepare_dir(Path::new(CGROUP_ROOT), id, uid, gid, &mut |p, u, g| {
        std::os::unix::fs::chown(p, Some(u), Some(g))
    })
}

/// Change the cleaned spec of a delegated container: a cgroup namespace (its root is the container's cgroup, which `crun` has already
/// entered), a cgroup2 mount that shows that root, and no `CAP_CHOWN`, `CAP_DAC_OVERRIDE`, `CAP_FOWNER`.
pub fn apply(clean: &mut Value) {
    if let Some(ns) = clean
        .pointer_mut("/linux/namespaces")
        .and_then(|n| n.as_array_mut())
    {
        if !ns.iter().any(|n| n["type"] == "cgroup") {
            ns.push(json!({"type": "cgroup"}));
        }
    }
    if let Some(m) = clean.get_mut("mounts").and_then(|m| m.as_array_mut()) {
        m.push(
            json!({"destination": CGROUP_ROOT, "type": "cgroup2", "source": "cgroup2",
            "options": ["nosuid", "noexec", "nodev"]}),
        );
    }
    if let Some(caps) = clean
        .pointer_mut("/process/capabilities")
        .and_then(|c| c.as_object_mut())
    {
        for set in caps.values_mut() {
            if let Some(list) = set.as_array_mut() {
                list.retain(|c| !STRIPPED_CAPS.iter().any(|s| c == s));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(list: Option<&str>, name: Option<&str>) -> Value {
        let mut a = serde_json::Map::new();
        if let Some(l) = list {
            a.insert(cloudhv_common::CGROUP_DELEGATE_ANNOTATION.into(), json!(l));
        }
        if let Some(n) = name {
            a.insert(NAME_ANNOTATION.into(), json!(n));
        }
        json!({ "annotations": a })
    }

    #[test]
    fn only_the_named_container_is_marked() {
        assert!(requested(&spec(Some("spawner"), Some("spawner"))));
        assert!(requested(&spec(Some("a, spawner ,b"), Some("spawner"))));
        // Another container of the Pod, no name, no annotation, an empty list, a name that only contains the word.
        assert!(!requested(&spec(Some("spawner"), Some("sidecar"))));
        assert!(!requested(&spec(Some("spawner"), None)));
        assert!(!requested(&spec(Some("spawner"), Some(""))));
        assert!(!requested(&spec(None, Some("spawner"))));
        assert!(!requested(&spec(Some(""), Some("spawner"))));
        assert!(!requested(&spec(Some("spawner2"), Some("spawner"))));
        assert!(!requested(&json!({})));
    }

    /// A fake cgroup root: a directory with the files a new cgroup has, made by the test when the "kernel" is asked to mkdir.
    fn fake_root(files: &[&str]) -> tempdir::Dir {
        let root = tempdir::Dir::new();
        let d = root.path().join("c1");
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            std::fs::write(d.join(f), "").unwrap();
        }
        std::fs::create_dir(d.join("child")).unwrap();
        root
    }

    const FILES: &[&str] = &[
        "cgroup.procs",
        "cgroup.threads",
        "cgroup.subtree_control",
        "cgroup.type",
        "cgroup.kill",
        "cgroup.freeze",
        "cgroup.max.depth",
        "memory.max",
        "memory.high",
        "memory.swap.max",
        "memory.min",
        "memory.low",
        "cpu.max",
        "cpu.weight",
        "pids.max",
    ];

    #[test]
    fn the_container_owns_three_files_and_the_directory_and_nothing_else() {
        let root = fake_root(FILES);
        let mut calls: Vec<(String, u32, u32)> = Vec::new();
        let dir = prepare_dir(root.path(), "c1", 1000, 1001, &mut |p, u, g| {
            calls.push((
                p.strip_prefix(root.path()).unwrap().display().to_string(),
                u,
                g,
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(dir, root.path().join("c1"));
        let owner = |p: &str| {
            let hit: Vec<_> = calls.iter().filter(|c| c.0 == p).collect();
            assert_eq!(hit.len(), 1, "{p} is changed exactly once");
            (hit[0].1, hit[0].2)
        };
        assert_eq!(owner("c1"), (1000, 1001));
        for f in ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control"] {
            assert_eq!(owner(&format!("c1/{f}")), (1000, 1001), "{f}");
        }
        // The limit files, the ones that kill or freeze, and the type are not the container's. A root container (uid 0) is the owner
        // of anything root-owned, so none of them may stay with root either.
        for f in FILES.iter().filter(|f| !f.starts_with("cgroup.procs")) {
            if DELEGATED_FILES.contains(f) {
                continue;
            }
            assert_eq!(owner(&format!("c1/{f}")), (NOBODY, NOBODY), "{f}");
        }
        // The child directory is left alone, and the directory is the last thing changed.
        assert!(!calls.iter().any(|c| c.0 == "c1/child"));
        assert_eq!(calls.last().unwrap().0, "c1");
    }

    #[test]
    fn a_root_container_still_gets_nobody_for_the_limit_files() {
        let root = fake_root(FILES);
        let mut seen = Vec::new();
        prepare_dir(root.path(), "c1", 0, 0, &mut |p, u, _| {
            seen.push((p.file_name().unwrap().to_string_lossy().to_string(), u));
            Ok(())
        })
        .unwrap();
        for f in ["memory.max", "cpu.max", "pids.max", "memory.high"] {
            assert!(seen.contains(&(f.to_string(), NOBODY)), "{f}");
        }
    }

    #[test]
    fn a_failed_chown_removes_the_cgroup_and_reports() {
        let root = tempdir::Dir::new();
        let r = prepare_dir(root.path(), "c2", 0, 0, &mut |_, _, _| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        // No files in the new directory, so only the directory chown fails: the directory is removed again.
        assert!(r.is_err());
        assert!(!root.path().join("c2").exists());
    }

    #[test]
    fn an_id_cannot_leave_the_cgroup_root() {
        let root = tempdir::Dir::new();
        for bad in ["", "../x", "a/b", ".hidden"] {
            assert!(
                prepare_dir(root.path(), bad, 0, 0, &mut |_, _, _| Ok(())).is_err(),
                "{bad:?}"
            );
        }
    }

    fn base_clean() -> Value {
        json!({
            "process": {"user": {"uid": 0, "gid": 0}, "capabilities": {
                "bounding": ["CAP_SETUID", "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_KILL"],
                "effective": ["CAP_SETUID", "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER"],
                "permitted": ["CAP_SETUID", "CAP_FOWNER"],
                "inheritable": [], "ambient": []}},
            "linux": {"namespaces": [{"type": "mount"}, {"type": "pid"}]},
            "mounts": [{"destination": "/sys", "type": "sysfs"}],
        })
    }

    #[test]
    fn the_spec_gets_a_cgroup_namespace_a_cgroup_mount_and_fewer_capabilities() {
        let mut s = base_clean();
        apply(&mut s);
        apply(&mut s); // twice changes nothing more than once
        let ns: Vec<_> = s["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ns.iter().filter(|n| *n == "cgroup").count(), 1);
        let mount = s["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["destination"] == CGROUP_ROOT)
            .unwrap();
        assert_eq!(mount["type"], "cgroup2");
        let opts = mount["options"].to_string();
        assert!(!opts.contains("\"ro\"") && opts.contains("nosuid") && opts.contains("noexec"));
        // Nothing is added, and the three are gone from every set.
        let caps = s["process"]["capabilities"].to_string();
        for c in STRIPPED_CAPS {
            assert!(!caps.contains(c), "{c}");
        }
        assert!(caps.contains("CAP_SETUID") && caps.contains("CAP_KILL"));
        assert_eq!(identity(&s), (0, 0));
    }

    /// Minimal temporary directory (no new dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "cgdelegate-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
