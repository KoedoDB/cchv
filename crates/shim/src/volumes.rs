//! The volumes of a container.
//!
//! A pod's runtime (kubelet, through containerd) puts the pod's files into each container with bind mounts in the OCI
//! spec: `/etc/resolv.conf`, `/etc/hosts`, `/etc/hostname`, `/dev/termination-log`, the ConfigMaps, Secrets and the service
//! account token, the emptyDirs. The sources are paths on the host, and the VM cannot see the host's files. So the shim reads
//! each source here and sends its contents with the container to the guest agent, which writes them into the VM and binds
//! them from there.
//!
//! The contents are a copy taken when the container starts: a file the runtime updates later (a ConfigMap, a service account
//! token that the kubelet rotates) is not updated in the VM, and a file the container writes is not written back to the host.
//! An emptyDir starts empty, is shared by the containers of the pod (they are in one VM), and is lost with the VM.
//!
//! This runs as root on the host over directories a customer may write to (a persistent volume), so:
//! - only sources below the paths the runtime makes volumes in are read (`BUILTIN_ROOTS`, and the `volume_source_roots` of the
//!   shim's configuration); any other bind source (a hostPath that got through, `/etc`, the kubelet's PKI) is refused;
//! - the tree is walked with file descriptors, and every entry is opened by a single `openat2(RESOLVE_BENEATH |
//!   RESOLVE_NO_MAGICLINKS)` from the volume's own directory descriptor, so that a link swapped in between a check and the
//!   read cannot lead out of the volume: there is no check of a path followed by a read of the same path. (Where the kernel
//!   has no `openat2`, the same resolution is done component by component with `O_NOFOLLOW`.) Links that stay inside the volume
//!   are followed, as the kubelet's own `..data` links are;
//! - only regular files are read (opened through the descriptor, with `O_NONBLOCK`, never more than the limit): a FIFO, a socket
//!   or a device in the volume is left alone;
//! - a volume that is over the limits, or cannot be read, is an error and the container is not started: starting it without
//!   its data would make it write to the VM's own disk and lose the data. Only the files the runtime offers and a container
//!   can do without (`/dev/termination-log`) are skipped.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use log::warn;
use sha2::{Digest, Sha256};

/// Where in the guest the volumes are written. The agent accepts no other prefix.
const VOLUME_ROOT: &str = "/run/cloudhv-vols";

/// What one volume may hold. The agent's RPC message has a limit of 4 MiB in all.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 512;
const MAX_DEPTH: usize = 8;
/// The whole of reading the volumes of one container.
const DEADLINE: Duration = Duration::from_secs(20);

/// Where the runtime makes the sources of the bind mounts of a pod: the kubelet's directories of the pod (ConfigMaps, Secrets,
/// projected tokens, emptyDirs, the termination log, PVC mounts), and the directories of containerd's CRI plugin for a sandbox
/// (`hosts`, `resolv.conf`, `hostname`, `shm`), for containerd and for k3s's containerd.
pub const BUILTIN_ROOTS: &[&str] = &[
    "/var/lib/kubelet/pods",
    "/run/containerd/io.containerd.grpc.v1.cri/sandboxes",
    "/var/lib/containerd/io.containerd.grpc.v1.cri/sandboxes",
    "/run/k3s/containerd/io.containerd.grpc.v1.cri/sandboxes",
    "/var/lib/rancher/k3s/agent/containerd/io.containerd.grpc.v1.cri/sandboxes",
];

/// The roots of persistent volumes (the directories a local-path provisioner makes), unless the shim's configuration says otherwise.
pub fn default_pv_roots() -> Vec<String> {
    vec![
        "/var/openebs/local".to_string(),
        "/var/lib/rancher/k3s/storage".to_string(),
    ]
}

/// Bind mounts a container can do without: if the runtime's file is not there, the container starts without it.
const OPTIONAL_DESTINATIONS: &[&str] = &["/dev/termination-log"];

/// Where the kubelet makes the emptyDirs of a pod: `<root>/<pod uid>/volumes/kubernetes.io~empty-dir/<name>`.
const EMPTYDIR_PLUGIN: &str = "kubernetes.io~empty-dir";

/// The file systems that are memory: an emptyDir on one of these is `medium: Memory`, and stays in the guest's memory.
const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;

struct Policy {
    roots: Vec<PathBuf>,
    /// The kubelet's directory of pods, where an emptyDir is looked for (a persistent volume's directory cannot make a volume a disk).
    kubelet_roots: Vec<PathBuf>,
    /// The size (MiB) of the disk of an emptyDir that is not in memory; 0: such an emptyDir stays in the guest's memory.
    emptydir_mib: u64,
    deadline: Duration,
    /// Resolve without `openat2` (for tests; also what runs on a kernel that has none).
    force_fallback: bool,
}

/// The volumes (JSON, as the daemon's API takes them) for the bind mounts of the OCI spec. `pv_roots` are the persistent volume
/// roots of the configuration. An error means the container must not be started.
///
/// An emptyDir of the kubelet that is on a disk of the host (the default medium; not `medium: Memory`, which the kubelet makes a tmpfs)
/// is not read: it is passed as `"disk": {"id", "size_mb"}` (`emptydir_mib`, 0 to keep it in the guest's memory), and the daemon gives
/// the VM a disk for it. The host's directory is not touched; its contents (nothing, when the pod starts) are not copied.
pub fn from_spec(
    config_json: &[u8],
    pv_roots: &[String],
    emptydir_mib: u64,
) -> Result<Vec<serde_json::Value>, String> {
    let roots = BUILTIN_ROOTS
        .iter()
        .map(|r| PathBuf::from(*r))
        .chain(pv_roots.iter().map(PathBuf::from))
        .collect();
    from_spec_with(
        config_json,
        &Policy {
            roots,
            kubelet_roots: vec![PathBuf::from(BUILTIN_ROOTS[0])],
            emptydir_mib,
            deadline: DEADLINE,
            force_fallback: false,
        },
    )
}

fn from_spec_with(config_json: &[u8], policy: &Policy) -> Result<Vec<serde_json::Value>, String> {
    let spec: serde_json::Value = match serde_json::from_slice(config_json) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let blocks = block_volumes(&spec)?;
    let mounts = match spec.get("mounts").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return Ok(blocks),
    };
    // The roots as they really are (the kubelet's directory may be a link to another disk).
    let roots: Vec<PathBuf> = policy
        .roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    let started = Instant::now();

    let mut volumes = Vec::new();
    for m in mounts {
        if m.get("type").and_then(|t| t.as_str()) != Some("bind") {
            continue;
        }
        let (Some(dest), Some(source)) = (
            m.get("destination").and_then(|d| d.as_str()),
            m.get("source").and_then(|s| s.as_str()),
        ) else {
            continue;
        };
        // The image declares a VOLUME where a block volume is mounted (postgres: /var/lib/postgresql): containerd, which does not know of the block
        // volume (it is not a mount of the pod), added an anonymous volume there. The block volume is the one the pod asked for.
        if blocks
            .iter()
            .any(|b| b.get("destination").and_then(|d| d.as_str()) == Some(dest))
            && is_implicit_image_volume(source)
        {
            continue;
        }
        let readonly = m
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| o.iter().any(|x| x.as_str() == Some("ro")))
            .unwrap_or(false);
        if policy.emptydir_mib > 0 && is_disk_emptydir(source, policy) {
            volumes.push(serde_json::json!({
                "destination": dest,
                "source": format!("{VOLUME_ROOT}/{}", name_of(source)),
                "readonly": readonly,
                "files": [],
                "disk": {"id": name_of(source), "size_mb": policy.emptydir_mib},
            }));
            continue;
        }
        match read_volume(source, dest, &roots, policy, started) {
            Ok(files) => {
                let files: Vec<serde_json::Value> = files
                    .into_iter()
                    .map(|(path, (content, mode))| {
                        serde_json::json!({
                            "path": path,
                            "content": base64::engine::general_purpose::STANDARD.encode(content),
                            "mode": mode,
                        })
                    })
                    .collect();
                volumes.push(serde_json::json!({
                    "destination": dest,
                    "source": format!("{VOLUME_ROOT}/{}", name_of(source)),
                    "readonly": readonly,
                    "files": files,
                }));
            }
            Err(e) if OPTIONAL_DESTINATIONS.contains(&dest) => {
                warn!("optional volume {dest} (from {source}) is not passed to the VM: {e}");
            }
            Err(e) => return Err(format!("volume {dest} (from {source}): {e}")),
        }
    }
    volumes.extend(blocks);
    Ok(volumes)
}

/// Whether `source` is the kubelet's emptyDir of a pod that is on the host's disk. It must really be below the kubelet's pods
/// directory, in the emptyDir plugin's directory (`<pod uid>/volumes/kubernetes.io~empty-dir/<name>`), a directory (not a link: it is
/// resolved from the root, as `read_volume` does) and not on a memory file system. Anything doubtful is `false`: the volume is then
/// read as before (and refused if it is not allowed).
fn is_disk_emptydir(source: &str, policy: &Policy) -> bool {
    let Ok(real) = std::fs::canonicalize(source) else {
        return false;
    };
    for root in policy
        .kubelet_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
    {
        let Ok(rel) = real.strip_prefix(&root) else {
            continue;
        };
        let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        if parts.len() != 4 || parts[1] != "volumes" || parts[2] != EMPTYDIR_PLUGIN {
            return false;
        }
        let Ok(root_c) = cstring(&root) else {
            return false;
        };
        let Ok(root_fd) = open_fd(&root_c, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        else {
            return false;
        };
        let Ok(fd) = resolve_beneath(&root_fd, &rel.to_string_lossy(), policy.force_fallback)
        else {
            return false;
        };
        let Ok(st) = fstat(&fd) else { return false };
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return false;
        }
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut sfs) } != 0 {
            return false;
        }
        let kind = sfs.f_type as i64;
        return kind != TMPFS_MAGIC && kind != RAMFS_MAGIC;
    }
    false
}

/// The name of a volume in the guest: the same host directory (an emptyDir mounted by two containers) is the same directory.
/// The annotation of a pod that says what its raw block devices (`volumeDevices`) are for: a JSON list of
/// `{"device": "<devicePath in the container>", "mount": "<where the guest mounts it>", "fs": "ext4", "format": "if-blank", "owner": "999:999"}` (`fs`,
/// `format` and `owner` optional; `owner` is the uid:gid of the root of the file system made on a blank device, so that a non-root user can write in it). The platform (the Regional Agent) writes it with the pod; a raw block device without it is not given to the VM.
///
/// An entry can instead ask for a volume by name, with no device and no claim: `{"volume": "data", "size_mib": 8192, "mount": "/var/lib/postgresql",
/// "format": "if-blank", "owner": "999:999"}`. The daemon's volume helper makes the logical volume for the pod's namespace (`doc/sandbox-daemon.md`); the
/// name of the volume is derived from the namespace and the volume's name, which is why the shim sends the pod's namespace (the one containerd writes in
/// `io.kubernetes.cri.sandbox-namespace`, which a pod cannot set: only `cloudhv.koedodb.com/*` annotations pass from a pod to the spec).
const BLOCK_VOLUMES_ANNOTATION: &str = "cloudhv.koedodb.com/block-volumes";
const SANDBOX_NAMESPACE: &str = "io.kubernetes.cri.sandbox-namespace";
const MAX_BLOCK_VOLUMES: usize = 4;

/// The persistent block volumes of a spec, as the daemon's API takes them: `"block": {"id", "major", "minor", "fs", "format"}`. The device is
/// named by its numbers in `linux.devices` (the runtime resolved the kubelet's path to them); the daemon checks that it is a logical volume of the
/// platform's, so nothing here is trusted. The host's file is not read or opened.
fn block_volumes(spec: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    let Some(raw) = spec
        .pointer("/annotations")
        .and_then(|a| a.get(BLOCK_VOLUMES_ANNOTATION))
        .and_then(|v| v.as_str())
    else {
        return Ok(Vec::new());
    };
    let entries: Vec<serde_json::Value> = serde_json::from_str(raw)
        .map_err(|e| format!("{BLOCK_VOLUMES_ANNOTATION} is not a JSON list: {e}"))?;
    if entries.len() > MAX_BLOCK_VOLUMES {
        return Err(format!(
            "{BLOCK_VOLUMES_ANNOTATION} has more than {MAX_BLOCK_VOLUMES} volumes"
        ));
    }
    let devices = spec
        .pointer("/linux/devices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let namespace = spec
        .pointer("/annotations")
        .and_then(|a| a.get(SANDBOX_NAMESPACE))
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty());
    let mut out = Vec::new();
    for e in &entries {
        let device = e.get("device").and_then(|d| d.as_str()).unwrap_or("");
        let mount = e.get("mount").and_then(|m| m.as_str()).unwrap_or("");
        if !mount.starts_with('/') || mount.split('/').any(|c| c == "..") {
            return Err(format!(
                "block volume of {device:?}: the mount {mount:?} is not an absolute path"
            ));
        }
        if let Some(name) = e.get("volume") {
            // A volume by name: no device. The daemon checks every field again; here they are only passed on, with the pod's namespace.
            let (Some(name), Some(ns)) = (name.as_str(), namespace) else {
                return Err(format!(
                    "block volume {name}: it needs a name and the pod's namespace ({SANDBOX_NAMESPACE})"
                ));
            };
            if !device.is_empty() {
                return Err(format!("block volume {name:?} names a volume and a device"));
            }
            let Some(size_mib) = e.get("size_mib").and_then(|n| n.as_u64()) else {
                return Err(format!("block volume {name:?} has no size_mib"));
            };
            let digest = Sha256::digest(format!("pv-by-name:{ns}:{name}").as_bytes());
            let id: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
            let mut block = serde_json::json!({"id": id, "namespace": ns, "volume": name, "size_mib": size_mib});
            // "thin": true, the last key of the annotation of a branch's database: passed on as it is, the daemon takes only a boolean.
            if let Some(t) = e.get("thin") {
                block["thin"] = t.clone();
            }
            for key in ["fs", "format", "owner"] {
                if let Some(v) = e.get(key).and_then(|v| v.as_str()) {
                    block[key] = serde_json::json!(v);
                }
            }
            out.push(serde_json::json!({
                "destination": mount,
                "source": format!("{VOLUME_ROOT}/{id}"),
                "readonly": false,
                "files": [],
                "block": block,
            }));
            continue;
        }
        let found = devices.iter().find(|d| {
            d.get("path").and_then(|p| p.as_str()) == Some(device)
                && d.get("type").and_then(|t| t.as_str()) == Some("b")
        });
        let (Some(major), Some(minor)) = (
            found.and_then(|d| d.get("major")).and_then(|n| n.as_u64()),
            found.and_then(|d| d.get("minor")).and_then(|n| n.as_u64()),
        ) else {
            return Err(format!(
                "block volume {device:?}: the container has no such block device (volumeDevices)"
            ));
        };
        let digest = Sha256::digest(format!("pv:{major}:{minor}").as_bytes());
        let id: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        let mut block = serde_json::json!({"id": id, "major": major, "minor": minor});
        if e.get("thin").is_some() {
            return Err(format!(
                "block volume {device:?}: a device cannot be thin, only a volume by name can"
            ));
        }
        if let Some(ns) = namespace {
            block["namespace"] = serde_json::json!(ns);
        }
        if let Some(fs) = e.get("fs").and_then(|f| f.as_str()) {
            block["fs"] = serde_json::json!(fs);
        }
        if let Some(format) = e.get("format").and_then(|f| f.as_str()) {
            block["format"] = serde_json::json!(format);
        }
        // "owner": "<uid>:<gid>" of the root of a file system made on a blank device (the daemon checks it).
        if let Some(owner) = e.get("owner").and_then(|o| o.as_str()) {
            block["owner"] = serde_json::json!(owner);
        }
        out.push(serde_json::json!({
            "destination": mount,
            "source": format!("{VOLUME_ROOT}/{id}"),
            "readonly": false,
            "files": [],
            "block": block,
        }));
    }
    Ok(out)
}

/// Whether `source` is one of the anonymous volumes containerd makes for a `VOLUME` of the image (`<root>/io.containerd.grpc.v1.cri/containers/<id>/volumes/<hash>`),
/// which it adds when no mount of the pod has the destination.
fn is_implicit_image_volume(source: &str) -> bool {
    let parts: Vec<&str> = source.split('/').filter(|p| !p.is_empty()).collect();
    parts
        .windows(5)
        .any(|w| w[0] == "io.containerd.grpc.v1.cri" && w[1] == "containers" && w[3] == "volumes")
        && parts
            .last()
            .map(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .unwrap_or(false)
}

fn name_of(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn read_volume(
    source: &str,
    dest: &str,
    roots: &[PathBuf],
    policy: &Policy,
    started: Instant,
) -> Result<BTreeMap<String, (Vec<u8>, u32)>, String> {
    // The source, as it really is, must be below a root: a link in the path cannot take it elsewhere.
    let real =
        std::fs::canonicalize(source).map_err(|e| format!("cannot resolve the source: {e}"))?;
    let root = roots
        .iter()
        .filter(|r| real.starts_with(r) && real != **r)
        .max_by_key(|r| r.as_os_str().len())
        .ok_or_else(|| {
            format!(
                "the source {} is not below a path the runtime makes volumes in",
                real.display()
            )
        })?;
    // That check is of a string, and the path could be changed after it (a directory of a persistent volume replaced by a link).
    // So the source is not opened by that path: the root (a path the runtime and the operator own) is opened, and the source is
    // resolved from it with nothing allowed to lead out of it, in one step.
    let root_fd = open_fd(
        &cstring(root)?,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )
    .map_err(|e| format!("cannot open the root {}: {e}", root.display()))?;
    let rel = real
        .strip_prefix(root)
        .map_err(|_| "the source is not below its root".to_string())?
        .to_string_lossy()
        .to_string();
    let fd = resolve_beneath(&root_fd, &rel, false).map_err(io)?;
    let st = fstat(&fd).map_err(io)?;
    let mut files = BTreeMap::new();
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => {
            let name = Path::new(dest)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("the destination has no file name")?;
            let content = read_regular(&fd, MAX_BYTES).map_err(io)?;
            files.insert(name.to_string(), (content, st.st_mode & 0o777));
        }
        libc::S_IFDIR => {
            let mut total = 0;
            walk(&fd, &fd, "", 0, &mut total, &mut files, policy, started)?;
        }
        _ => return Err("the source is neither a file nor a directory".into()),
    }
    Ok(files)
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &OwnedFd,
    dir: &OwnedFd,
    prefix: &str,
    depth: usize,
    total: &mut usize,
    files: &mut BTreeMap<String, (Vec<u8>, u32)>,
    policy: &Policy,
    started: Instant,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("the directory is deeper than the limit".into());
    }
    // Through the descriptor, not through a path: it lists this directory, whatever the path has become.
    let entries = std::fs::read_dir(proc_fd_path(dir)).map_err(|e| format!("list: {e}"))?;
    for entry in entries {
        if started.elapsed() > policy.deadline {
            return Err("reading the volume took too long".into());
        }
        let entry = entry.map_err(|e| format!("list: {e}"))?;
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        // The kubelet writes ConfigMaps and Secrets as `..data` -> `..<timestamp>/`, and each file is a link through `..data`.
        // The links are followed below, so the `..` entries are left out (they would be there twice).
        if name.starts_with("..") {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let fd = match resolve_beneath(root, &rel, policy.force_fallback) {
            Ok(fd) => fd,
            Err(e) => {
                // A dangling link, or a link that leaves the volume: not followed, and left out.
                warn!("volume entry {rel} is left out: {e}");
                continue;
            }
        };
        let st = fstat(&fd).map_err(io)?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => walk(root, &fd, &rel, depth + 1, total, files, policy, started)?,
            libc::S_IFREG => {
                let size = st.st_size.max(0) as usize;
                *total += size;
                if *total > MAX_BYTES || files.len() >= MAX_FILES {
                    return Err(format!("more than {MAX_FILES} files or {MAX_BYTES} bytes"));
                }
                let content = read_regular(&fd, size.max(1) + 1024).map_err(io)?;
                files.insert(rel, (content, st.st_mode & 0o777));
            }
            _ => {} // a FIFO, a socket, a device: not read
        }
    }
    Ok(())
}

// ── descriptors ─────────────────────────────────────────────────────────────

fn io(e: std::io::Error) -> String {
    e.to_string()
}

fn cstring(p: &Path) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| "the path has a NUL".to_string())
}

fn proc_fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

fn open_fd(path: &std::ffi::CStr, flags: i32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

/// Read a regular file through its descriptor (a descriptor from `O_PATH` is reopened from `/proc/self/fd`, which is the same
/// file whatever happened to its path), never more than `max` bytes.
fn read_regular(fd: &OwnedFd, max: usize) -> std::io::Result<Vec<u8>> {
    let c = std::ffi::CString::new(proc_fd_path(fd).to_string_lossy().as_bytes()).unwrap();
    let f = open_fd(&c, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK)?;
    let mut f = std::fs::File::from(f);
    let mut buf = Vec::new();
    (&mut f).take(max as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max {
        return Err(std::io::Error::other(format!(
            "the file is larger than {max} bytes"
        )));
    }
    Ok(buf)
}

// ── resolving beneath a directory ───────────────────────────────────────────

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;
/// The numbers of `openat2` are the same on every architecture Linux has it on.
const SYS_OPENAT2: libc::c_long = 437;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Whether the kernel has `openat2` (learned at the first call).
static NO_OPENAT2: AtomicBool = AtomicBool::new(false);

/// An `O_PATH` descriptor for `rel`, below `root`: every link on the way is followed, and none may lead out of `root`.
fn resolve_beneath(root: &OwnedFd, rel: &str, force_fallback: bool) -> std::io::Result<OwnedFd> {
    if !force_fallback && !NO_OPENAT2.load(Ordering::Relaxed) {
        let c = std::ffi::CString::new(rel)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                SYS_OPENAT2,
                root.as_raw_fd(),
                c.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            // No openat2 (an old kernel), or a filter that refuses it: resolve by hand from now on.
            Some(libc::ENOSYS) | Some(libc::EPERM) => NO_OPENAT2.store(true, Ordering::Relaxed),
            _ => return Err(e),
        }
    }
    resolve_beneath_by_hand(root, rel)
}

/// The same as `openat2(RESOLVE_BENEATH)`, a component at a time: no component is followed by the kernel (`O_NOFOLLOW`); a link is
/// read and its target is resolved from the directory the link is in; an absolute target, or a `..` above `root`, is refused.
fn resolve_beneath_by_hand(root: &OwnedFd, rel: &str) -> std::io::Result<OwnedFd> {
    let exdev = || std::io::Error::from_raw_os_error(libc::EXDEV);
    let dup = |fd: &OwnedFd| -> std::io::Result<OwnedFd> {
        let d = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if d < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(d) })
    };
    let mut components: VecDeque<String> = rel.split('/').map(String::from).collect();
    let mut stack: Vec<OwnedFd> = vec![dup(root)?];
    let mut hops = 0;
    let mut last: Option<OwnedFd> = None;
    while let Some(c) = components.pop_front() {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            if stack.len() <= 1 {
                return Err(exdev());
            }
            stack.pop();
            last = None;
            continue;
        }
        let cc = std::ffi::CString::new(c)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let dir = stack.last().expect("the stack holds the root");
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                cc.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let st = fstat(&fd)?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                hops += 1;
                if hops > 40 {
                    return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
                }
                let mut buf = vec![0u8; 4096];
                let n = unsafe {
                    libc::readlinkat(
                        fd.as_raw_fd(),
                        c"".as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_char,
                        buf.len(),
                    )
                };
                if n < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let target = String::from_utf8_lossy(&buf[..n as usize]).to_string();
                if target.starts_with('/') {
                    return Err(exdev());
                }
                for part in target.split('/').rev() {
                    components.push_front(part.to_string());
                }
                last = None;
            }
            libc::S_IFDIR => {
                stack.push(dup(&fd)?);
                last = Some(fd);
            }
            _ => {
                if !components.iter().all(|c| c.is_empty() || c == ".") {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                last = Some(fd);
            }
        }
    }
    match last {
        Some(fd) => Ok(fd),
        // `rel` was empty (or ended in `..`): the directory the stack ends in.
        None => dup(stack.last().expect("the stack holds the root")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("cloudhv-vol-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(std::fs::canonicalize(&p).unwrap())
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn policy(root: &Path, fallback: bool) -> Policy {
        Policy {
            roots: vec![root.to_path_buf()],
            kubelet_roots: vec![root.to_path_buf()],
            emptydir_mib: 0,
            deadline: Duration::from_secs(5),
            force_fallback: fallback,
        }
    }

    fn spec(mounts: serde_json::Value) -> Vec<u8> {
        serde_json::json!({ "mounts": mounts })
            .to_string()
            .into_bytes()
    }

    fn block_spec(annotation: serde_json::Value, devices: serde_json::Value) -> Vec<u8> {
        block_spec_in(annotation.to_string(), devices, None)
    }

    /// A spec with the namespace containerd writes for the pod (`None`: none).
    fn block_spec_in(annotation: String, devices: serde_json::Value, ns: Option<&str>) -> Vec<u8> {
        let mut annotations = serde_json::json!({ BLOCK_VOLUMES_ANNOTATION: annotation });
        if let Some(n) = ns {
            annotations[SANDBOX_NAMESPACE] = serde_json::json!(n);
        }
        serde_json::json!({
            "mounts": [],
            "annotations": annotations,
            "linux": { "devices": devices },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_volume_by_name_carries_the_pods_namespace_and_no_device() {
        let root = Tmp::new("blk-named");
        let entries = serde_json::json!([{"volume": "data", "size_mib": 8192, "mount": "/var/lib/postgresql", "format": "if-blank", "owner": "999:999"}])
            .to_string();
        let none = serde_json::json!([]);
        let spec = block_spec_in(entries.clone(), none.clone(), Some("p-abc-db"));
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got.len(), 1);
        let b = &got[0]["block"];
        assert_eq!(got[0]["destination"], "/var/lib/postgresql");
        assert_eq!(b["namespace"], "p-abc-db");
        assert_eq!(b["volume"], "data");
        assert_eq!(b["size_mib"], 8192);
        assert_eq!(b["format"], "if-blank");
        assert_eq!(b["owner"], "999:999");
        assert!(b.get("major").is_none() && b.get("minor").is_none());
        // The id depends on the namespace and the name: two namespaces' "data" are two volumes.
        let other = block_spec_in(entries.clone(), none.clone(), Some("p-zzz-db"));
        let got2 = from_spec_with(&other, &policy(&root.0, false)).unwrap();
        assert_ne!(got[0]["block"]["id"], got2[0]["block"]["id"]);
        // Without the namespace of the pod there is no volume.
        let bare = block_spec_in(entries, none.clone(), None);
        assert!(from_spec_with(&bare, &policy(&root.0, false)).is_err());
        // Without a size, or with a device as well.
        let no_size = block_spec_in(
            r#"[{"volume":"data","mount":"/x"}]"#.into(),
            none.clone(),
            Some("p-abc-db"),
        );
        assert!(from_spec_with(&no_size, &policy(&root.0, false)).is_err());
        let both = block_spec_in(
            r#"[{"volume":"data","size_mib":1,"device":"/dev/x","mount":"/x"}]"#.into(),
            none,
            Some("p-abc-db"),
        );
        assert!(from_spec_with(&both, &policy(&root.0, false)).is_err());
    }

    #[test]
    fn a_thin_volume_by_name_is_passed_on_and_a_device_cannot_be_thin() {
        let root = Tmp::new("blk-thin");
        let none = serde_json::json!([]);
        let entries = serde_json::json!([{"volume": "data", "size_mib": 2048, "mount": "/var/lib/postgresql", "format": "if-blank", "owner": "999:999", "thin": true}])
            .to_string();
        let spec = block_spec_in(entries, none.clone(), Some("p-abc-db"));
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got[0]["block"]["thin"], true);
        assert_eq!(got[0]["block"]["size_mib"], 2048);
        // Without the key there is none (the daemon's default is thick).
        let plain =
            serde_json::json!([{"volume": "data", "size_mib": 2048, "mount": "/x"}]).to_string();
        let spec = block_spec_in(plain, none, Some("p-abc-db"));
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert!(got[0]["block"].get("thin").is_none());
        // A device with a thin key is refused here already.
        let spec = block_spec(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql", "thin": true}]),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7, "fileMode": 432}]),
        );
        assert!(from_spec_with(&spec, &policy(&root.0, false)).is_err());
    }

    #[test]
    fn a_device_volume_carries_the_namespace_too() {
        let root = Tmp::new("blk-dev-ns");
        let spec = block_spec_in(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql"}])
                .to_string(),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7, "fileMode": 432}]),
            Some("p-abc-db"),
        );
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got[0]["block"]["namespace"], "p-abc-db");
    }

    #[test]
    fn the_owner_of_a_block_volume_is_passed_on_for_the_daemon_to_check() {
        let root = Tmp::new("blk-owner");
        let spec = block_spec(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql", "format": "if-blank", "owner": "999:999"}]),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7, "fileMode": 432}]),
        );
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got[0]["block"]["owner"], "999:999");
        // Without it the key is absent (the daemon's default is root).
        let spec = block_spec(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql", "format": "if-blank"}]),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7, "fileMode": 432}]),
        );
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert!(got[0]["block"].get("owner").is_none());
    }

    #[test]
    fn a_raw_block_device_named_by_the_annotation_becomes_a_block_volume() {
        let root = Tmp::new("blk");
        let spec = block_spec(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql", "format": "if-blank"}]),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7, "fileMode": 432}]),
        );
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["destination"], "/var/lib/postgresql");
        assert_eq!(got[0]["block"]["major"], 252);
        assert_eq!(got[0]["block"]["minor"], 7);
        assert_eq!(got[0]["block"]["format"], "if-blank");
        assert!(
            got[0]["block"].get("fs").is_none(),
            "the daemon's default is ext4"
        );
        let id = got[0]["block"]["id"].as_str().unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(got[0]["source"], format!("{VOLUME_ROOT}/{id}"));
        // The same device is the same volume; another device is another.
        let again = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(again[0]["block"]["id"], id);
        let other = block_spec(
            serde_json::json!([{"device": "/dev/pgdata", "mount": "/data"}]),
            serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 8}]),
        );
        let other = from_spec_with(&other, &policy(&root.0, false)).unwrap();
        assert_ne!(other[0]["block"]["id"], id);
    }

    #[test]
    fn the_anonymous_volume_of_the_image_where_a_block_volume_goes_is_left_out() {
        let root = Tmp::new("blk-implicit");
        let hash = "18d0d8e5897208925b08f4449259bf592acb37cab15b35f6129efed4aa6b4e9f";
        let implicit = format!(
            "/var/lib/rancher/k3s/agent/containerd/io.containerd.grpc.v1.cri/containers/pg/volumes/{hash}"
        );
        let spec = serde_json::json!({
            "mounts": [
                {"type": "bind", "destination": "/var/lib/postgresql", "source": implicit, "options": ["rbind"]},
            ],
            "annotations": { BLOCK_VOLUMES_ANNOTATION: serde_json::json!([{"device": "/dev/pgdata", "mount": "/var/lib/postgresql"}]).to_string() },
            "linux": { "devices": [{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7}] },
        })
        .to_string()
        .into_bytes();
        let got = from_spec_with(&spec, &policy(&root.0, false)).unwrap();
        assert_eq!(got.len(), 1, "the block volume, and not the anonymous one");
        assert!(got[0].get("block").is_some());
        // Without a block volume at that destination the anonymous volume is still refused: it is not below a root the runtime makes volumes in.
        let no_block = serde_json::json!({
            "mounts": [{"type": "bind", "destination": "/var/lib/postgresql", "source": implicit, "options": ["rbind"]}],
        })
        .to_string()
        .into_bytes();
        assert!(from_spec_with(&no_block, &policy(&root.0, false)).is_err());
        // A source that only looks like it is not left out.
        assert!(!is_implicit_image_volume(
            "/var/lib/rancher/k3s/storage/pvc-x"
        ));
        assert!(!is_implicit_image_volume(&format!(
            "/containers/pg/volumes/{hash}"
        )));
        assert!(!is_implicit_image_volume(
            "/x/io.containerd.grpc.v1.cri/containers/pg/volumes/short"
        ));
    }

    #[test]
    fn a_block_device_without_the_annotation_is_not_given_to_the_vm() {
        let root = Tmp::new("blk-none");
        let spec = serde_json::json!({
            "mounts": [],
            "linux": { "devices": [{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7}] },
        })
        .to_string()
        .into_bytes();
        assert!(from_spec_with(&spec, &policy(&root.0, false))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_block_volume_the_container_does_not_have_is_an_error() {
        let root = Tmp::new("blk-bad");
        for (annotation, devices) in [
            // the device is not in the container
            (
                serde_json::json!([{"device": "/dev/pgdata", "mount": "/data"}]),
                serde_json::json!([]),
            ),
            // a character device is not a block device
            (
                serde_json::json!([{"device": "/dev/pgdata", "mount": "/data"}]),
                serde_json::json!([{"path": "/dev/pgdata", "type": "c", "major": 1, "minor": 3}]),
            ),
            // the mount is not an absolute path, or leaves it
            (
                serde_json::json!([{"device": "/dev/pgdata", "mount": "data"}]),
                serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7}]),
            ),
            (
                serde_json::json!([{"device": "/dev/pgdata", "mount": "/a/../etc"}]),
                serde_json::json!([{"path": "/dev/pgdata", "type": "b", "major": 252, "minor": 7}]),
            ),
            // too many
            (
                serde_json::json!((0..5).map(|i| serde_json::json!({"device": format!("/dev/d{i}"), "mount": format!("/m{i}")})).collect::<Vec<_>>()),
                serde_json::json!([]),
            ),
        ] {
            let spec = block_spec(annotation, devices);
            assert!(from_spec_with(&spec, &policy(&root.0, false)).is_err());
        }
        // an annotation that is not a list
        let spec = serde_json::json!({"annotations": {BLOCK_VOLUMES_ANNOTATION: "{}"}})
            .to_string()
            .into_bytes();
        assert!(from_spec_with(&spec, &policy(&root.0, false)).is_err());
    }

    fn names(v: &serde_json::Value) -> Vec<String> {
        v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// A kubelet-style secret directory (`..data` link, files as links through it) and a file next to it.
    fn kubelet_layout(base: &Path) -> (PathBuf, PathBuf) {
        let dir = base.join("secret");
        std::fs::create_dir_all(dir.join("..2026_01_01")).unwrap();
        std::fs::write(dir.join("..2026_01_01/token"), b"secret").unwrap();
        symlink("..2026_01_01", dir.join("..data")).unwrap();
        symlink("..data/token", dir.join("token")).unwrap();
        let resolv = base.join("resolv.conf");
        std::fs::write(&resolv, b"nameserver 10.45.0.10\n").unwrap();
        (dir, resolv)
    }

    #[test]
    fn file_and_directory_sources_in_both_ways_of_resolving() {
        for fallback in [false, true] {
            let t = Tmp::new(&format!("a{fallback}"));
            let (dir, resolv) = kubelet_layout(&t.0);
            let mounts = serde_json::json!([
                {"destination": "/etc/resolv.conf", "type": "bind", "source": resolv, "options": ["rbind", "ro"]},
                {"destination": "/var/run/secrets/x", "type": "bind", "source": dir, "options": ["rbind"]},
                {"destination": "/proc", "type": "proc", "source": "proc"},
            ]);
            let v = from_spec_with(&spec(mounts), &policy(&t.0, fallback)).unwrap();
            assert_eq!(v.len(), 2, "fallback={fallback}");
            assert_eq!(v[0]["readonly"], true);
            assert_eq!(names(&v[0]), vec!["resolv.conf"]);
            assert!(v[0]["source"].as_str().unwrap().starts_with(VOLUME_ROOT));
            // The kubelet's own links work, and its `..` entries are not there twice.
            let secret = names(&v[1]);
            assert!(
                secret.contains(&"token".to_string()),
                "fallback={fallback}: {secret:?}"
            );
            assert!(!secret.iter().any(|n| n.starts_with("..")), "{secret:?}");
            let content = v[1]["files"][0]["content"].as_str().unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(content)
                    .unwrap(),
                b"secret"
            );
        }
    }

    #[test]
    fn a_link_that_leaves_the_volume_is_left_out() {
        for fallback in [false, true] {
            let t = Tmp::new(&format!("b{fallback}"));
            let outside = t.0.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("host-secret"), b"HOST").unwrap();
            let vol = t.0.join("vol");
            std::fs::create_dir_all(vol.join("sub")).unwrap();
            std::fs::write(vol.join("ok"), b"ok").unwrap();
            symlink(outside.join("host-secret"), vol.join("abs")).unwrap(); // absolute
            symlink("../outside/host-secret", vol.join("rel")).unwrap(); // relative, out
            symlink("../../outside/host-secret", vol.join("sub/deep")).unwrap();
            symlink("../outside", vol.join("dirlink")).unwrap(); // a directory out
            symlink("sub", vol.join("inside")).unwrap(); // a link that stays in
            let p = Policy {
                roots: vec![t.0.join("vol")],
                ..policy(&t.0, fallback)
            };
            // The volume is below the root: use the parent as root so that `vol` is strictly below it.
            let p = Policy {
                roots: vec![t.0.clone()],
                ..p
            };
            let mounts = serde_json::json!([{"destination": "/v", "type": "bind", "source": vol}]);
            let v = from_spec_with(&spec(mounts), &p).unwrap();
            let got = names(&v[0]);
            assert_eq!(got, vec!["ok"], "fallback={fallback}: {got:?}");
        }
    }

    #[test]
    fn a_source_outside_the_roots_is_refused_and_the_container_does_not_start() {
        let t = Tmp::new("c");
        let (dir, _) = kubelet_layout(&t.0);
        let other = Tmp::new("c-other");
        let mounts = serde_json::json!([{"destination": "/v", "type": "bind", "source": dir}]);
        let e = from_spec_with(&spec(mounts), &policy(&other.0, false)).unwrap_err();
        assert!(
            e.contains("not below a path the runtime makes volumes in"),
            "{e}"
        );
        // The root itself is not a volume.
        let mounts = serde_json::json!([{"destination": "/v", "type": "bind", "source": t.0}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false)).is_err());
        // A hostPath such as /etc.
        let mounts =
            serde_json::json!([{"destination": "/host", "type": "bind", "source": "/etc"}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false)).is_err());
    }

    #[test]
    fn a_link_as_the_source_does_not_get_around_the_roots() {
        let t = Tmp::new("d");
        let other = Tmp::new("d-other");
        std::fs::write(other.0.join("f"), b"x").unwrap();
        let l = t.0.join("link");
        symlink(other.0.join("f"), &l).unwrap();
        let mounts = serde_json::json!([{"destination": "/f", "type": "bind", "source": l}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false)).is_err());
    }

    #[test]
    fn a_volume_over_the_limits_or_missing_is_an_error_but_the_termination_log_is_optional() {
        let t = Tmp::new("e");
        let big = t.0.join("big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("f"), vec![0u8; MAX_BYTES + 1]).unwrap();
        let mounts = serde_json::json!([{"destination": "/data", "type": "bind", "source": big}]);
        let e = from_spec_with(&spec(mounts), &policy(&t.0, false)).unwrap_err();
        assert!(e.contains("bytes"), "{e}");

        let many = t.0.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..=MAX_FILES {
            std::fs::write(many.join(format!("f{i}")), b"x").unwrap();
        }
        let mounts = serde_json::json!([{"destination": "/data", "type": "bind", "source": many}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false)).is_err());

        let gone = t.0.join("gone");
        let mounts = serde_json::json!([{"destination": "/data", "type": "bind", "source": gone}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false)).is_err());
        let mounts = serde_json::json!([{"destination": "/dev/termination-log", "type": "bind", "source": gone}]);
        assert!(from_spec_with(&spec(mounts), &policy(&t.0, false))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn special_files_are_not_read() {
        let t = Tmp::new("f");
        let vol = t.0.join("vol");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(vol.join("regular"), b"r").unwrap();
        let c = std::ffi::CString::new(vol.join("fifo").to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let mounts = serde_json::json!([{"destination": "/v", "type": "bind", "source": vol}]);
        let v = from_spec_with(&spec(mounts), &policy(&t.0, false)).unwrap();
        assert_eq!(names(&v[0]), vec!["regular"]);
    }

    #[test]
    fn the_resolver_by_hand_refuses_what_openat2_refuses() {
        let t = Tmp::new("g");
        let vol = t.0.join("vol");
        std::fs::create_dir_all(vol.join("a/b")).unwrap();
        std::fs::write(vol.join("a/b/f"), b"x").unwrap();
        std::fs::write(t.0.join("out"), b"o").unwrap();
        symlink("b/f", vol.join("a/l")).unwrap();
        symlink("../../out", vol.join("a/b/up")).unwrap();
        symlink("/etc/passwd", vol.join("abs")).unwrap();
        symlink("loop", vol.join("loop")).unwrap();
        let root = open_fd(
            &cstring(&vol).unwrap(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
        .unwrap();
        for fallback in [false, true] {
            assert!(
                resolve_beneath(&root, "a/l", fallback).is_ok(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "a/../a/b/f", fallback).is_ok(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "a/b/up", fallback).is_err(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "../out", fallback).is_err(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "abs", fallback).is_err(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "loop", fallback).is_err(),
                "fallback={fallback}"
            );
            assert!(
                resolve_beneath(&root, "a/b/f/x", fallback).is_err(),
                "fallback={fallback}"
            );
        }
    }

    /// While the volume is read, a customer's process swaps a file between a regular file and a link to a file outside the volume,
    /// as fast as it can. What is read must never be the outside file.
    #[test]
    fn swapping_a_link_in_while_it_is_read_does_not_leak_the_outside_file() {
        for fallback in [false, true] {
            let t = Tmp::new(&format!("h{fallback}"));
            let vol = t.0.join("vol");
            std::fs::create_dir_all(vol.join("d")).unwrap();
            std::fs::write(vol.join("f"), b"inside").unwrap();
            std::fs::write(t.0.join("host-secret"), b"HOST-SECRET").unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let swapper = {
                let (vol, out, stop) = (vol.clone(), t.0.join("host-secret"), stop.clone());
                std::thread::spawn(move || {
                    let mut flip = false;
                    while !stop.load(Ordering::Relaxed) {
                        let tmp = vol.join("f.tmp");
                        let _ = std::fs::remove_file(&tmp);
                        if flip {
                            let _ = symlink(&out, &tmp);
                        } else {
                            let _ = std::fs::write(&tmp, b"inside");
                        }
                        let _ = std::fs::rename(&tmp, vol.join("f"));
                        // the directory too: d -> / and back
                        let dtmp = vol.join("d.tmp");
                        let _ = std::fs::remove_file(&dtmp);
                        let _ = std::fs::remove_dir_all(&dtmp);
                        if flip {
                            let _ = symlink("/", &dtmp);
                            let _ = std::fs::rename(&dtmp, vol.join("d"));
                        }
                        flip = !flip;
                    }
                })
            };
            let mounts = serde_json::json!([{"destination": "/v", "type": "bind", "source": vol}]);
            let s = spec(mounts);
            for _ in 0..300 {
                if let Ok(v) = from_spec_with(
                    &s,
                    &Policy {
                        roots: vec![t.0.clone()],
                        ..policy(&t.0, fallback)
                    },
                ) {
                    let text = serde_json::to_string(&v).unwrap();
                    let host = base64::engine::general_purpose::STANDARD.encode(b"HOST-SECRET");
                    assert!(
                        !text.contains(&host),
                        "fallback={fallback}: the outside file was read"
                    );
                    assert!(
                        !text.contains("root:"),
                        "fallback={fallback}: /etc/passwd of the host was read"
                    );
                }
            }
            stop.store(true, Ordering::Relaxed);
            swapper.join().unwrap();
        }
    }

    #[test]
    fn a_volume_has_the_same_name_in_every_container_of_the_pod() {
        assert_eq!(
            name_of("/var/lib/kubelet/pods/a/volumes/x"),
            name_of("/var/lib/kubelet/pods/a/volumes/x")
        );
        assert_ne!(
            name_of("/var/lib/kubelet/pods/a/volumes/x"),
            name_of("/var/lib/kubelet/pods/a/volumes/y")
        );
        assert_eq!(name_of("x").len(), 32);
    }

    /// A directory on the disk, not in /tmp (which is a tmpfs on many hosts).
    fn disk_tmp(tag: &str) -> Tmp {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/emptydir-tests")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(std::fs::canonicalize(&p).unwrap())
    }

    fn emptydir_policy(root: &Path, mib: u64) -> Policy {
        Policy {
            emptydir_mib: mib,
            ..policy(root, false)
        }
    }

    fn make_emptydir(base: &Path) -> PathBuf {
        let d = base
            .join("pod-uid")
            .join("volumes")
            .join(EMPTYDIR_PLUGIN)
            .join("cache");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_emptydir_on_the_hosts_disk_becomes_a_disk_and_is_not_read() {
        let t = disk_tmp("disk");
        let dir = make_emptydir(&t.0);
        // Something in it (a leftover): it is not copied.
        std::fs::write(dir.join("old"), b"data").unwrap();
        let mounts = serde_json::json!([
            {"destination": "/cache", "type": "bind", "source": dir, "options": ["rbind"]},
            {"destination": "/other", "type": "bind", "source": dir, "options": ["rbind", "ro"]},
        ]);
        let v = from_spec_with(&spec(mounts), &emptydir_policy(&t.0, 1024)).unwrap();
        assert_eq!(v.len(), 2);
        let id = name_of(dir.to_str().unwrap());
        for (i, ro) in [(0, false), (1, true)] {
            assert_eq!(v[i]["disk"]["id"], id.as_str());
            assert_eq!(v[i]["disk"]["size_mb"], 1024);
            assert_eq!(v[i]["source"], format!("{VOLUME_ROOT}/{id}"));
            assert_eq!(v[i]["readonly"], ro);
            assert!(v[i]["files"].as_array().unwrap().is_empty());
        }
        // The same directory is the same disk at both destinations.
        assert_eq!(v[0]["disk"]["id"], v[1]["disk"]["id"]);
    }

    #[test]
    fn an_emptydir_in_memory_or_with_the_feature_off_is_read_as_before() {
        // medium: Memory is a tmpfs
        let t = Tmp::new("emptydir-mem"); // /tmp
        if std::fs::read_to_string("/proc/mounts")
            .map(|m| {
                m.lines()
                    .any(|l| l.split(' ').nth(1) == Some("/tmp") && l.contains(" tmpfs "))
            })
            .unwrap_or(false)
        {
            let dir = make_emptydir(&t.0);
            let mounts =
                serde_json::json!([{"destination": "/cache", "type": "bind", "source": dir}]);
            let v = from_spec_with(&spec(mounts), &emptydir_policy(&t.0, 1024)).unwrap();
            assert!(
                v[0].get("disk").is_none(),
                "a tmpfs emptyDir stays in the guest's memory"
            );
        }
        // size 0: off
        let d = disk_tmp("off");
        let dir = make_emptydir(&d.0);
        let mounts = serde_json::json!([{"destination": "/cache", "type": "bind", "source": dir}]);
        let v = from_spec_with(&spec(mounts), &emptydir_policy(&d.0, 0)).unwrap();
        assert!(v[0].get("disk").is_none());
    }

    #[test]
    fn only_the_kubelets_emptydir_directories_are_disks() {
        let t = disk_tmp("notdisk");
        let p = emptydir_policy(&t.0, 1024);
        // a ConfigMap, a persistent volume, the pod's own files: files, as before
        let other =
            t.0.join("pod-uid")
                .join("volumes")
                .join("kubernetes.io~configmap")
                .join("cfg");
        std::fs::create_dir_all(&other).unwrap();
        assert!(!is_disk_emptydir(other.to_str().unwrap(), &p));
        let shallow = t.0.join("pod-uid").join("volumes").join(EMPTYDIR_PLUGIN);
        assert!(
            !is_disk_emptydir(shallow.to_str().unwrap(), &p),
            "the plugin directory itself"
        );
        let deep = make_emptydir(&t.0).join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(
            !is_disk_emptydir(deep.to_str().unwrap(), &p),
            "a directory inside an emptyDir"
        );
        // a file with the right name
        let file =
            t.0.join("pod-uid")
                .join("volumes")
                .join(EMPTYDIR_PLUGIN)
                .join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_disk_emptydir(file.to_str().unwrap(), &p));
        // outside the kubelet's directory, even with the right shape (a persistent volume's customer-made directory)
        let elsewhere = disk_tmp("elsewhere");
        let fake = make_emptydir(&elsewhere.0);
        assert!(!is_disk_emptydir(fake.to_str().unwrap(), &p));
        // a link to a real emptyDir resolves to it (the kubelet's own directory may be a link); a link out does not
        let real = make_emptydir(&t.0);
        assert!(is_disk_emptydir(real.to_str().unwrap(), &p));
        let l =
            t.0.join("pod-uid")
                .join("volumes")
                .join(EMPTYDIR_PLUGIN)
                .join("l");
        symlink(&fake, &l).unwrap();
        assert!(!is_disk_emptydir(l.to_str().unwrap(), &p));
        // does not exist
        assert!(!is_disk_emptydir("/nonexistent/x", &p));
    }
}
