//! Persistent block volumes.
//!
//! A database in a pod's VM needs a disk that outlives the VM. A raw block device the platform made for it (an LVM logical volume, given to the
//! pod as a `volumeDevices` entry) is hot-plugged into the VM as a virtio-blk disk with direct I/O, and the guest agent mounts the ext4 on it.
//! The shim asks for it in the volumes of a request: `"block": {"id", "major", "minor", "fs", "format"}`, the numbers being those of the device
//! in the OCI spec (`linux.devices`). No path is trusted or used.
//!
//! - **Which device**: the numbers must name a device-mapper device (`/sys/dev/block/<major>:<minor>/dm/name`) whose name begins with one of
//!   `volume_block_prefixes`, and whose UUID is an LVM one (`LVM-...`). Anything else is refused: a pod cannot be given the host's own disks.
//! - **One VM at a time**: root takes an exclusive `flock` on a file named after the device for as long as the VM lives (`release`, from
//!   `shutdown_and_destroy`); a second VM that asks for the same device is refused. The lock is released by the kernel if the daemon dies.
//! - **How the Cloud Hypervisor gets it**: root makes a node of the device (`mknod`, 0600) in the VM's directory for disks
//!   (`emptydir::vm_dir`), gives it to the VM's user and hot-plugs `"path": <node>`. The directory is bound into the confined root (`confine.rs`)
//!   and, unlike every other bind, is not `nodev`: the VM's user cannot make a node or a name there, so what it can open is the node root made for it.
//!   The node goes with the directory when the VM does; the device never does.
//! - **The file system**: never made unless the volume says `"format": "if-blank"` and `volume_block_format_if_blank` is on, and then only on
//!   a device whose first MiB is zeros: an ext4 with a journal (`mke2fs`, by root, before the VM's user is given the node). Only ext4: the
//!   guest kernel has no other.
//! - **Volumes by name** (`volume_block_vg`, `lvm.rs`): the pod's annotation can ask for a volume by name and size instead of a device
//!   (`"volume": "data", "size_mib": 8192`). The daemon never runs LVM: its helper (`cloudhv-volume-helper`) makes the logical volume
//!   `pv-<namespace>_<volume>` of the volume group, zeroes it whole, and tags it `cloudhv-ns=<namespace>` and `cloudhv-ready`. The name comes from the
//!   pod's Kubernetes namespace (from containerd, through the shim) and the volume name, never from the annotation, so a pod cannot ask for another
//!   pod's volume. The daemon checks the answer again on its own: the device-mapper name must be exactly the one derived, and the tags must say
//!   this namespace and ready.
//! - **Whose volume it is**: with `volume_block_vg`, a device given as `volumeDevices` is looked up too: its `cloudhv-ns=` tag must be the pod's namespace
//!   and it must be ready. A volume without a tag is refused unless `volume_block_allow_untagged`, and a volume of another namespace never goes.
//! - What the guest writes is the guest's: a file system it corrupts hurts the guest, and the device is not touched by the host once plugged.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::config::DaemonConfig;

/// The most block volumes one request may ask for.
const MAX_BLOCKS: usize = 4;
/// The file systems a volume may have (the guest kernel's).
pub const FS_TYPES: &[&str] = &["ext4"];
/// How long making a file system may take (a large logical volume).
const MKFS_TIMEOUT: Duration = Duration::from_secs(600);
/// How much of the start of a device is looked at to call it blank.
const BLANK_BYTES: usize = 1024 * 1024;

/// A block volume a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRequest {
    /// 32 lower-case hex characters: the shim's name of the volume.
    pub id: String,
    pub major: u32,
    pub minor: u32,
    pub fs: String,
    /// `"format": "if-blank"`.
    pub format: bool,
    /// `"owner": "<uid>:<gid>"`: who owns the root of the file system when it is made (`root_owner` of `mke2fs`). `0:0` without it. A file system that
    /// exists keeps its owner: this is read only when the device is blank and is formatted.
    pub owner: (u32, u32),
    /// The Kubernetes namespace of the pod (containerd's, through the shim). Needed with `volume_block_vg`.
    pub namespace: Option<String>,
    /// A volume asked for by name: made by the helper, so `major` and `minor` are 0 until `plug` has the helper's answer.
    pub managed: Option<ManagedVolume>,
}

/// A volume asked for by name and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedVolume {
    pub volume: String,
    pub size_mib: u64,
}

/// `"<uid>:<gid>"`: two decimal numbers, nothing else (no sign, no space), neither the "no id" value of Linux (4294967295).
fn parse_owner(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("owner {s:?} is not <uid>:<gid>");
    let (u, g) = s.split_once(':').ok_or_else(bad)?;
    let id = |t: &str| -> Result<u32, String> {
        if t.is_empty() || t.len() > 10 || !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        match t.parse::<u32>() {
            Ok(n) if n != u32::MAX => Ok(n),
            _ => Err(bad()),
        }
    };
    Ok((id(u)?, id(g)?))
}

/// The volumes by name and the check of whose a volume is (`volume_block_vg`).
struct Managed {
    vg: String,
    max_mib: u64,
    allow_untagged: bool,
    client: crate::lvm::Client,
}

struct Settings {
    managed: Option<Managed>,
    prefixes: Vec<String>,
    format_if_blank: bool,
    locks: PathBuf,
    mke2fs: PathBuf,
    /// Whether the disks are opened with O_DIRECT (`disk_direct`).
    direct: bool,
}

static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();

fn settings() -> Option<&'static Settings> {
    SETTINGS.get().and_then(|s| s.as_ref())
}

/// The locks held by the VMs that are alive: the state directory of the VM, and a file for each device it has. Dropping a file releases its lock.
static HELD: OnceLock<Mutex<HashMap<PathBuf, Vec<std::fs::File>>>> = OnceLock::new();

fn held() -> &'static Mutex<HashMap<PathBuf, Vec<std::fs::File>>> {
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read the configuration and make the directory of the locks (root's, 0700). On an error the feature is off and the daemon goes on.
pub fn init(config: &DaemonConfig) {
    let s = if config.volume_block_prefixes.is_empty() {
        None
    } else if config.emptydir_dir.is_empty() {
        warn!(
            "persistent block volumes are off: they need emptydir_dir (the nodes are made in the VM's directory)"
        );
        None
    } else {
        let locks = Path::new(&config.state_dir).join("block-locks");
        match prepare_locks(&locks) {
            Ok(()) => Some(Settings {
                managed: (!config.volume_block_vg.is_empty()).then(|| Managed {
                    vg: config.volume_block_vg.clone(),
                    max_mib: config.volume_block_max_mib,
                    allow_untagged: config.volume_block_allow_untagged,
                    client: crate::lvm::Client {
                        socket: PathBuf::from(&config.volume_helper_socket),
                    },
                }),
                prefixes: config.volume_block_prefixes.clone(),
                format_if_blank: config.volume_block_format_if_blank,
                locks,
                mke2fs: crate::emptydir::find_mke2fs(&config.mke2fs_path),
                direct: config.disk_direct,
            }),
            Err(e) => {
                warn!("persistent block volumes are off: {e:#}");
                None
            }
        }
    };
    let _ = SETTINGS.set(s);
}

fn prepare_locks(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The virtio-blk serial of a volume (20 bytes at most): what the guest agent finds the disk by.
pub fn serial_of(id: &str) -> String {
    format!("pv{}", id.chars().take(16).collect::<String>())
}

fn is_volume_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a device-mapper name begins with one of the allowed prefixes.
fn name_allowed(name: &str, prefixes: &[String]) -> bool {
    !name.is_empty()
        && prefixes
            .iter()
            .any(|p| !p.is_empty() && name.starts_with(p))
}

/// The name of the device-mapper device `major:minor`, from `sysfs_root` (`/sys/dev/block`), when it is an LVM one.
fn lvm_name_in(sysfs_root: &Path, major: u32, minor: u32) -> Option<String> {
    let dm = sysfs_root.join(format!("{major}:{minor}")).join("dm");
    let uuid = std::fs::read_to_string(dm.join("uuid")).ok()?;
    if !uuid.trim().starts_with("LVM-") {
        return None;
    }
    let name = std::fs::read_to_string(dm.join("name")).ok()?;
    Some(name.trim().to_string())
}

fn check_device(s: &Settings, major: u32, minor: u32) -> Result<(), String> {
    check_device_in(Path::new("/sys/dev/block"), &s.prefixes, major, minor)
}

fn check_device_in(
    sysfs_root: &Path,
    prefixes: &[String],
    major: u32,
    minor: u32,
) -> Result<(), String> {
    match lvm_name_in(sysfs_root, major, minor) {
        Some(name) if name_allowed(&name, prefixes) => Ok(()),
        Some(_) => Err(format!(
            "the device {major}:{minor} is not one a pod may be given (volume_block_prefixes)"
        )),
        None => Err(format!(
            "the device {major}:{minor} is not an LVM logical volume"
        )),
    }
}

/// The block volumes of the `volumes` of a request. An error (a bad id or numbers, a device that is not allowed, too many, a file system
/// that is not ext4, a format that was not enabled, or the feature off) refuses the request. A volume asked for twice (one volume
/// mounted at two destinations) is one.
pub fn parse(request: &serde_json::Value) -> Result<Vec<BlockRequest>, String> {
    parse_with(
        request,
        settings().map(|s| (&s.prefixes[..], s.format_if_blank)),
        settings()
            .and_then(|s| s.managed.as_ref())
            .map(|m| m.max_mib),
        |maj, min| {
            settings()
                .map(|s| check_device(s, maj, min))
                .unwrap_or(Err("persistent block volumes are off".into()))
        },
    )
}

fn parse_with(
    request: &serde_json::Value,
    on: Option<(&[String], bool)>,
    managed_max_mib: Option<u64>,
    check: impl Fn(u32, u32) -> Result<(), String>,
) -> Result<Vec<BlockRequest>, String> {
    let mut out: Vec<BlockRequest> = Vec::new();
    for v in request
        .get("volumes")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(b) = v.get("block").filter(|b| !b.is_null()) else {
            continue;
        };
        let (_, format_ok) =
            on.ok_or("persistent block volumes are off (volume_block_prefixes)")?;
        let id = b.get("id").and_then(|i| i.as_str()).unwrap_or("");
        if !is_volume_id(id) {
            return Err("a block volume id is not 32 hex characters".into());
        }
        let namespace = match b.get("namespace").and_then(|n| n.as_str()) {
            None | Some("") => None,
            Some(n) if crate::lvm::valid_namespace(n) => Some(n.to_string()),
            Some(n) => return Err(format!("{n:?} is not a namespace name")),
        };
        if managed_max_mib.is_some() && namespace.is_none() {
            return Err("a block volume has no namespace (volume_block_vg is on)".into());
        }
        let num = |k: &str| {
            b.get(k)
                .and_then(|n| n.as_u64())
                .and_then(|n| u32::try_from(n).ok())
        };
        // A volume by name has no device numbers: the helper makes the device. A device has no name.
        let by_name = b.get("volume").filter(|v| !v.is_null());
        let (major, minor, managed) = if let Some(v) = by_name {
            let max = managed_max_mib.ok_or("volumes by name are off (volume_block_vg)")?;
            if b.get("major").is_some() || b.get("minor").is_some() {
                return Err("a block volume names a volume and a device".into());
            }
            let volume = v.as_str().filter(|v| crate::lvm::valid_volume(v));
            let Some(volume) = volume else {
                return Err("a block volume's name is not a volume name".into());
            };
            let Some(size_mib) = b
                .get("size_mib")
                .and_then(|n| n.as_u64())
                .filter(|n| *n > 0)
            else {
                return Err("a block volume by name has no size_mib".into());
            };
            if size_mib > max {
                return Err(format!(
                    "a block volume of {size_mib} MiB is more than volume_block_max_mib ({max})"
                ));
            }
            (
                0,
                0,
                Some(ManagedVolume {
                    volume: volume.to_string(),
                    size_mib,
                }),
            )
        } else {
            let (Some(major), Some(minor)) = (num("major"), num("minor")) else {
                return Err("a block volume has no device numbers".into());
            };
            (major, minor, None)
        };
        let fs = b.get("fs").and_then(|f| f.as_str()).unwrap_or("ext4");
        if !FS_TYPES.contains(&fs) {
            return Err(format!(
                "a block volume of file system {fs:?} is not allowed"
            ));
        }
        let format = match b.get("format").and_then(|f| f.as_str()) {
            None | Some("") => false,
            Some("if-blank") => true,
            Some(other) => return Err(format!("format {other:?} is not known")),
        };
        let owner = match b.get("owner").and_then(|o| o.as_str()) {
            None | Some("") => (0, 0),
            Some(o) => parse_owner(o)?,
        };
        if format && !format_ok {
            return Err(
                "a block volume asks to be formatted: volume_block_format_if_blank is off".into(),
            );
        }
        if v.get("destination").and_then(|d| d.as_str()).is_none() {
            return Err("a block volume has no destination".into());
        }
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        if managed.is_none()
            && out
                .iter()
                .any(|o| o.managed.is_none() && o.major == major && o.minor == minor)
        {
            return Err("two block volumes are the same device".into());
        }
        if managed.is_some()
            && out.iter().any(|o| {
                o.managed.as_ref().map(|m| &m.volume) == managed.as_ref().map(|m| &m.volume)
            })
        {
            return Err("two block volumes are the same volume".into());
        }
        if out.len() >= MAX_BLOCKS {
            return Err(format!("more than {MAX_BLOCKS} block volumes"));
        }
        // A device the helper has not made yet is checked when it has (`plug`).
        if managed.is_none() {
            check(major, minor)?;
        }
        out.push(BlockRequest {
            id: id.to_string(),
            major,
            minor,
            fs: fs.to_string(),
            format,
            owner,
            namespace,
            managed,
        });
    }
    Ok(out)
}

/// Take the exclusive lock of a device for the VM whose state directory is `state_dir`. A device another VM holds is refused.
fn take_lock(locks: &Path, state_dir: &Path, major: u32, minor: u32) -> Result<()> {
    lock_device(locks, state_dir, major, minor)?;
    // In the registry (root's), so that a daemon that takes the VM over can take the lock again (`adopt`).
    crate::ctl::append(state_dir, "blocks.list", &format!("{major}:{minor}"))
        .context("record the device of the VM")
}

fn lock_device(locks: &Path, state_dir: &Path, major: u32, minor: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(locks.join(format!("{major}:{minor}")))
        .context("open the lock of the device")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            anyhow::bail!("the block device {major}:{minor} is in use by another VM");
        }
        return Err(e).context("lock the device");
    }
    held()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(state_dir.to_path_buf())
        .or_default()
        .push(file);
    Ok(())
}

/// A VM of an earlier daemon is taken over: lock again the devices it has. The locks went with the earlier daemon (a lock lives as long as the
/// file that holds it), and without them the volume could be given to a second VM while the first still writes to it. A device that cannot be
/// locked (another VM holds it: it was given away in between) is reported; the caller stops the VM then. Returns whether all were locked.
pub fn adopt(state_dir: &Path) -> bool {
    let Some(s) = settings() else {
        return crate::ctl::read(state_dir, "blocks.list").is_none();
    };
    let Some(list) = crate::ctl::read(state_dir, "blocks.list") else {
        return true;
    };
    let mut all = true;
    for line in list.lines().filter(|l| !l.is_empty()) {
        let dev = line
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)));
        let Some((major, minor)) = dev else {
            warn!(
                "{}: a device of the VM is not understood: {line:?}",
                state_dir.display()
            );
            all = false;
            continue;
        };
        if let Err(e) = lock_device(&s.locks, state_dir, major, minor) {
            warn!(
                "{}: block device {major}:{minor} cannot be locked again: {e:#}",
                state_dir.display()
            );
            all = false;
        }
    }
    all
}

/// Let go of the devices of the VM whose state directory is `state_dir` (a VM that had none is not an error). The nodes go with the VM's
/// directory (`emptydir::remove_vm_dir`); the devices are never touched.
pub fn release(state_dir: &Path) {
    let gone = held()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(state_dir);
    if let Some(files) = gone {
        info!(
            "released {} block device(s) of {}",
            files.len(),
            state_dir.display()
        );
    }
}

/// Whether the start of a device is zeros.
fn is_blank(path: &Path) -> Result<bool> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; BLANK_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).context("read the device")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(buf[..filled].iter().all(|b| *b == 0))
}

/// The arguments of `mke2fs` for a volume: ext4 with a journal (the data is meant to stay), no blocks reserved for root, the inode tables made
/// as they are used. `lazy_journal_init` is not used: the device is not known to be zeros beyond its first MiB. The node last.
fn mke2fs_args(node: &Path, owner: (u32, u32)) -> Vec<std::ffi::OsString> {
    let extended = format!(
        "lazy_itable_init=1,nodiscard,root_owner={}:{}",
        owner.0, owner.1
    );
    let mut a: Vec<std::ffi::OsString> = [
        "-q",
        "-t",
        "ext4",
        "-b",
        "4096",
        "-m",
        "0",
        "-O",
        "has_journal",
        "-E",
        extended.as_str(),
        "-L",
        "pv",
    ]
    .iter()
    .map(Into::into)
    .collect();
    a.push(node.as_os_str().to_os_string());
    a
}

async fn format(s: &Settings, node: &Path, owner: (u32, u32)) -> Result<()> {
    let mut cmd = tokio::process::Command::new(&s.mke2fs);
    cmd.args(mke2fs_args(node, owner))
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(MKFS_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("mke2fs took longer than {}s", MKFS_TIMEOUT.as_secs()))?
        .with_context(|| format!("run {}", s.mke2fs.display()))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "mke2fs failed ({}): {}",
            out.status,
            text.chars().take(300).collect::<String>().trim()
        );
    }
    Ok(())
}

/// Make the node of a device (a block special file) at `path`, 0600, root's. Fails if the name exists.
fn make_node(path: &Path, major: u32, minor: u32) -> Result<()> {
    let c =
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).context("NUL in the path")?;
    let dev = libc::makedev(major, minor);
    if unsafe { libc::mknod(c.as_ptr(), libc::S_IFBLK | 0o600, dev) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("make the node of {major}:{minor}"));
    }
    Ok(())
}

/// The body of `vm.add-disk` for a node: raw, writable, with direct I/O (the host's page cache is not in the way of the guest's `fsync`).
fn disk_body(node: &Path, b: &BlockRequest, direct: bool) -> serde_json::Value {
    serde_json::json!({
        "path": node,
        "readonly": false,
        "direct": direct,
        "image_type": "Raw",
        "id": format!("pv-{}", b.id.chars().take(16).collect::<String>()),
        "serial": serial_of(&b.id),
    })
}

/// The volumes of a request are in the VM: for each one not there yet, check the device again, lock it, make its node in the VM's directory,
/// make the file system if asked (and the device is blank), give the node to the VM's user and hot-plug it. A volume the VM has already (a
/// second container of the pod asks for it) is left as it is. On an error the node and the lock of the volume that failed are gone.
pub async fn attach(api_socket: &Path, blocks: &[BlockRequest]) -> Result<()> {
    if blocks.is_empty() {
        return Ok(());
    }
    let s = settings().context("persistent block volumes are off")?;
    let state_dir = api_socket.parent().context("the VM's state directory")?;
    let dir = crate::emptydir::vm_dir(state_dir)
        .context("the VM has no directory for disks (emptydir_dir)")?;
    // Two containers of one pod may ask for the same volume together.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _held = LOCK.lock().await;
    for b in blocks {
        let node = dir.join(format!("pv-{}", b.id.chars().take(16).collect::<String>()));
        if std::fs::symlink_metadata(&node).is_ok() {
            continue;
        }
        if let Err(e) = plug(s, api_socket, state_dir, &node, b).await {
            let _ = std::fs::remove_file(&node);
            return Err(e.context(format!("block volume {}", b.id)));
        }
        info!("block volume {} attached to {}", b.id, state_dir.display());
    }
    Ok(())
}

/// Whether the volume `info` (what the helper says of a device) may be given to a pod of `namespace`.
fn check_owner(
    info: Option<&crate::lvm::LvInfo>,
    namespace: &str,
    allow_untagged: bool,
) -> Result<(), String> {
    let Some(i) = info else {
        return Err("the device is not a logical volume of volume_block_vg".into());
    };
    match i.namespaces().as_slice() {
        [] if allow_untagged => Ok(()),
        [] => Err(format!(
            "the logical volume {}/{} has no {} tag (volume_block_allow_untagged is off)",
            i.vg,
            i.lv,
            crate::lvm::NS_TAG_KEY
        )),
        [n] if *n == namespace => {
            if i.has_tag(crate::lvm::READY_TAG) {
                Ok(())
            } else {
                Err(format!("the logical volume {}/{} is not ready", i.vg, i.lv))
            }
        }
        _ => Err(format!(
            "the logical volume {}/{} belongs to another namespace",
            i.vg, i.lv
        )),
    }
}

/// The device numbers of a volume to give a pod. A volume by name is made (or found) by the helper, and what it answers is checked again here: the
/// device-mapper name from sysfs must be exactly the derived one, the tags must be this namespace's and ready. A device is looked up, and its owner tag
/// checked. Without `volume_block_vg` a device is as it was: the numbers of the request.
async fn resolve(s: &Settings, b: &BlockRequest) -> Result<(u32, u32)> {
    let Some(m) = &s.managed else {
        return Ok((b.major, b.minor));
    };
    let namespace = b.namespace.clone().context("the volume has no namespace")?;
    let client = m.client.clone();
    if let Some(v) = &b.managed {
        let (ns, vol, size) = (namespace.clone(), v.volume.clone(), v.size_mib);
        let got = tokio::task::spawn_blocking(move || client.ensure(&ns, &vol, size))
            .await
            .context("the call to the volume helper")??;
        let lv = crate::lvm::lv_name(&namespace, &v.volume).map_err(|e| anyhow::anyhow!(e))?;
        let want = crate::lvm::dm_name(&m.vg, &lv).map_err(|e| anyhow::anyhow!(e))?;
        let (major, minor) = (got.info.major, got.info.minor);
        if lvm_name_in(Path::new("/sys/dev/block"), major, minor).as_deref() != Some(&want) {
            anyhow::bail!("the device {major}:{minor} the helper named is not {want}");
        }
        check_owner(Some(&got.info), &namespace, false).map_err(|e| anyhow::anyhow!(e))?;
        if !got.info.has_tag(crate::lvm::READY_TAG) {
            anyhow::bail!("the helper's volume is not ready");
        }
        if got.created || got.extended {
            info!(
                "the helper made the volume {want} ({} MiB; created {}, extended {})",
                got.info.size_mib, got.created, got.extended
            );
        }
        Ok((major, minor))
    } else {
        let (major, minor) = (b.major, b.minor);
        let info = tokio::task::spawn_blocking(move || client.tags(major, minor))
            .await
            .context("the call to the volume helper")??;
        check_owner(info.as_ref(), &namespace, m.allow_untagged).map_err(|e| anyhow::anyhow!(e))?;
        Ok((major, minor))
    }
}

async fn plug(
    s: &Settings,
    api_socket: &Path,
    state_dir: &Path,
    node: &Path,
    b: &BlockRequest,
) -> Result<()> {
    let (major, minor) = resolve(s, b).await?;
    check_device(s, major, minor).map_err(|e| anyhow::anyhow!(e))?;
    take_lock(&s.locks, state_dir, major, minor)?;
    make_node(node, major, minor)?;
    if b.format {
        if is_blank(node)? {
            format(s, node, b.owner)
                .await
                .context("make the file system")?;
            info!("made an ext4 on the blank device {major}:{minor}");
        } else {
            info!("the device {major}:{minor} is not blank: no file system made");
        }
    }
    // Only now the VM's user owns the node: root made the file system, and the VM could not have opened the node before.
    crate::rootless::give(node, state_dir).context("give the node to the VM")?;
    crate::vm_lifecycle::api_request_with_body(
        api_socket,
        "PUT",
        "/api/v1/vm.add-disk",
        &disk_body(node, b, s.direct).to_string(),
    )
    .await
    .context("hot-plug the device")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const ID2: &str = "fedcba9876543210fedcba9876543210";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-blockvol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn vol(id: &str, major: u64, minor: u64) -> serde_json::Value {
        json!({"destination": "/var/lib/postgresql", "block": {"id": id, "major": major, "minor": minor}})
    }

    fn on() -> Option<(&'static [String], bool)> {
        Some((&[], false))
    }

    fn allow(_: u32, _: u32) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn a_serial_fits_the_twenty_bytes_of_virtio_blk() {
        let s = serial_of(ID);
        assert_eq!(s, "pv0123456789abcdef");
        assert!(s.len() <= 20);
        assert_ne!(s, crate::emptydir::serial_of(ID));
    }

    #[test]
    fn the_volumes_of_a_request_are_checked_and_one_volume_is_one_device() {
        let two_destinations = json!({"volumes": [vol(ID, 252, 3), vol(ID, 252, 3)]});
        let got = parse_with(&two_destinations, on(), None, allow).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0],
            BlockRequest {
                id: ID.into(),
                major: 252,
                minor: 3,
                fs: "ext4".into(),
                format: false,
                owner: (0, 0),
                namespace: None,
                managed: None,
            }
        );
        // A volume without a block is not one of these.
        let none = json!({"volumes": [{"destination": "/x", "source": "/y"}]});
        assert!(parse_with(&none, None, None, allow).unwrap().is_empty());
        assert!(parse_with(&json!({}), None, None, allow)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_request_the_feature_does_not_allow_is_refused() {
        let r = json!({"volumes": [vol(ID, 252, 3)]});
        assert!(
            parse_with(&r, None, None, allow).is_err(),
            "the feature is off"
        );
        for bad in [
            vol("not-an-id", 252, 3),
            json!({"destination": "/x", "block": {"id": ID, "minor": 3}}),
            json!({"destination": "/x", "block": {"id": ID, "major": 252, "minor": 4294967296u64}}),
            json!({"block": {"id": ID, "major": 252, "minor": 3}}),
            json!({"destination": "/x", "block": {"id": ID, "major": 252, "minor": 3, "fs": "xfs"}}),
            json!({"destination": "/x", "block": {"id": ID, "major": 252, "minor": 3, "format": "always"}}),
        ] {
            let r = json!({"volumes": [bad]});
            assert!(parse_with(&r, on(), None, allow).is_err(), "{r}");
        }
        let two = json!({"volumes": [vol(ID, 252, 3), vol(ID2, 252, 3)]});
        assert!(
            parse_with(&two, on(), None, allow).is_err(),
            "two names for one device"
        );
        let many: Vec<serde_json::Value> = (0..5u64)
            .map(|i| vol(&format!("{i:032x}"), 252, 10 + i))
            .collect();
        assert!(parse_with(&json!({ "volumes": many }), on(), None, allow).is_err());
    }

    #[test]
    fn a_format_is_refused_unless_the_daemon_allows_it() {
        let r = json!({"volumes": [{"destination": "/x", "block": {"id": ID, "major": 252, "minor": 3, "format": "if-blank"}}]});
        assert!(parse_with(&r, Some((&[], false)), None, allow).is_err());
        let got = parse_with(&r, Some((&[], true)), None, allow).unwrap();
        assert!(got[0].format);
    }

    #[test]
    fn the_device_is_checked_for_every_volume() {
        let r = json!({"volumes": [vol(ID, 8, 0)]});
        let refuse = |_: u32, _: u32| Err("not allowed".to_string());
        assert_eq!(
            parse_with(&r, on(), None, refuse).unwrap_err(),
            "not allowed"
        );
    }

    #[test]
    fn only_lvm_devices_with_an_allowed_name_may_be_given() {
        let root = tmp("sysfs");
        let make = |maj: u32, min: u32, name: &str, uuid: &str| {
            let dm = root.join(format!("{maj}:{min}")).join("dm");
            std::fs::create_dir_all(&dm).unwrap();
            std::fs::write(dm.join("name"), format!("{name}\n")).unwrap();
            std::fs::write(dm.join("uuid"), format!("{uuid}\n")).unwrap();
        };
        make(252, 3, "vg-pv--abc", "LVM-xyz");
        make(252, 4, "vg-root", "LVM-xyz");
        make(252, 5, "vg-pv--evil", "CRYPT-LUKS2-xyz");
        std::fs::create_dir_all(root.join("8:0")).unwrap(); // a disk: no dm directory
        let prefixes = vec!["vg-pv--".to_string()];
        assert!(check_device_in(&root, &prefixes, 252, 3).is_ok());
        assert!(
            check_device_in(&root, &prefixes, 252, 4).is_err(),
            "an LVM volume of the host"
        );
        assert!(
            check_device_in(&root, &prefixes, 252, 5).is_err(),
            "not an LVM uuid"
        );
        assert!(
            check_device_in(&root, &prefixes, 8, 0).is_err(),
            "a disk, not a logical volume"
        );
        assert!(
            check_device_in(&root, &prefixes, 9, 9).is_err(),
            "no such device"
        );
        assert!(
            !name_allowed("vg-pv--abc", &[String::new()]),
            "an empty prefix allows nothing"
        );
        assert!(!name_allowed("", &prefixes));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_device_is_held_by_one_vm_until_it_is_released() {
        let locks = tmp("locks");
        // The registry (where the devices of a VM are recorded) is next to the VMs' directories.
        let state = tmp("locks-state");
        let a_buf = state.join("sized-1-0");
        let b_buf = state.join("sized-1-1");
        let (a, b) = (a_buf.as_path(), b_buf.as_path());
        take_lock(&locks, a, 252, 3).unwrap();
        assert!(
            take_lock(&locks, b, 252, 3).is_err(),
            "a second VM asks for the same device"
        );
        take_lock(&locks, b, 252, 4).unwrap();
        release(a);
        take_lock(&locks, b, 252, 3).unwrap();
        release(b);
        release(b); // a VM that has none is not an error
        let _ = std::fs::remove_dir_all(locks);
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn a_device_is_blank_when_its_start_is_zeros() {
        let d = tmp("blank");
        let f = d.join("dev");
        std::fs::write(&f, vec![0u8; 3 * 1024 * 1024]).unwrap();
        assert!(is_blank(&f).unwrap());
        let mut data = vec![0u8; 3 * 1024 * 1024];
        data[1024 + 56] = 0x53; // the ext4 magic's place, as a file system would have it
        std::fs::write(&f, &data).unwrap();
        assert!(!is_blank(&f).unwrap());
        let mut late = vec![0u8; 3 * 1024 * 1024];
        late[BLANK_BYTES + 5] = 1; // after the first MiB: not looked at
        std::fs::write(&f, &late).unwrap();
        assert!(is_blank(&f).unwrap());
        std::fs::write(&f, b"").unwrap();
        assert!(is_blank(&f).unwrap(), "an empty device");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn mke2fs_gets_the_node_last_and_a_journal_that_is_written() {
        let a = mke2fs_args(Path::new("/x/pv-1"), (0, 0));
        assert_eq!(a.last().unwrap(), "/x/pv-1");
        assert!(a.iter().any(|x| x == "has_journal"));
        assert!(!a.iter().any(|x| x == "^has_journal"));
        assert!(
            !a.iter()
                .any(|x| x.to_string_lossy().contains("lazy_journal_init")),
            "the device is not known to be zeros"
        );
        assert!(
            !a.iter().any(|x| x == "-F"),
            "a device, and never forced over a file system"
        );
    }

    #[test]
    fn the_owner_of_the_new_file_system_is_a_uid_and_a_gid() {
        assert_eq!(parse_owner("999:999"), Ok((999, 999)));
        assert_eq!(parse_owner("0:0"), Ok((0, 0)));
        assert_eq!(parse_owner("1000:0"), Ok((1000, 0)));
        for bad in [
            "",
            "999",
            "999:",
            ":999",
            "a:b",
            "-1:0",
            "+1:0",
            "1:2:3",
            " 1:2",
            "1: 2",
            "4294967295:0",
            "0:4294967295",
            "99999999999:0",
            "1.5:0",
        ] {
            assert!(parse_owner(bad).is_err(), "{bad:?} must be refused");
        }
        let a = mke2fs_args(Path::new("/x/pv-1"), (999, 998));
        assert!(
            a.iter()
                .any(|x| x == "lazy_itable_init=1,nodiscard,root_owner=999:998"),
            "mke2fs gets the owner: {a:?}"
        );
        let a = mke2fs_args(Path::new("/x/pv-1"), (0, 0));
        assert!(a
            .iter()
            .any(|x| x.to_string_lossy().ends_with("root_owner=0:0")));
    }

    #[test]
    fn a_volume_may_name_its_owner_and_a_bad_owner_refuses_the_request() {
        let with = |o: &str| json!({"volumes": [{"destination": "/x", "block": {"id": ID, "major": 252, "minor": 3, "format": "if-blank", "owner": o}}]});
        let formatting: Option<(&[String], bool)> = Some((&[], true));
        let got = parse_with(&with("999:999"), formatting, None, allow).unwrap();
        assert_eq!(got[0].owner, (999, 999));
        assert!(parse_with(&with("postgres"), formatting, None, allow).is_err());
        let without = json!({"volumes": [vol(ID, 252, 3)]});
        assert_eq!(
            parse_with(&without, on(), None, allow).unwrap()[0].owner,
            (0, 0)
        );
    }

    #[test]
    fn the_disk_is_raw_writable_and_opened_with_direct_io() {
        let b = BlockRequest {
            id: ID.into(),
            major: 252,
            minor: 3,
            fs: "ext4".into(),
            format: false,
            owner: (0, 0),
            namespace: None,
            managed: None,
        };
        let body = disk_body(
            Path::new("/var/lib/cloudhv/emptydir/vm/pv-0123456789abcdef"),
            &b,
            true,
        );
        assert_eq!(body["image_type"], "Raw");
        assert_eq!(body["direct"], true);
        assert_eq!(body["readonly"], false);
        assert_eq!(body["serial"], "pv0123456789abcdef");
        assert_eq!(body["id"], "pv-0123456789abcdef");
        // `disk_direct: false` leaves the host's page cache in between.
        let cached = disk_body(Path::new("/x/pv-0123456789abcdef"), &b, false);
        assert_eq!(cached["direct"], false);
    }

    // ---- volumes by name, and whose a volume is ----

    fn named(
        volume: serde_json::Value,
        size: serde_json::Value,
        ns: Option<&str>,
    ) -> serde_json::Value {
        let mut b = json!({"id": ID, "volume": volume, "size_mib": size, "format": "if-blank", "owner": "999:999"});
        if let Some(n) = ns {
            b["namespace"] = json!(n);
        }
        json!({"volumes": [{"destination": "/var/lib/postgresql", "block": b}]})
    }

    #[test]
    fn a_volume_by_name_needs_the_feature_a_namespace_and_a_size_under_the_cap() {
        let formatting: Option<(&[String], bool)> = Some((&[], true));
        let ok = named(json!("data"), json!(8192), Some("p-abc-db"));
        let got = parse_with(&ok, formatting, Some(10_000), allow).unwrap();
        assert_eq!(got[0].namespace.as_deref(), Some("p-abc-db"));
        assert_eq!(
            got[0].managed,
            Some(ManagedVolume {
                volume: "data".into(),
                size_mib: 8192
            })
        );
        assert_eq!((got[0].major, got[0].minor), (0, 0));
        // Not allowed at all without volume_block_vg.
        assert!(parse_with(&ok, formatting, None, allow).is_err());
        // The device check is not run for a device that does not exist yet.
        let never = |_: u32, _: u32| -> Result<(), String> { Err("must not be called".into()) };
        assert!(parse_with(&ok, formatting, Some(10_000), never).is_ok());
        for bad in [
            named(json!("data"), json!(8192), None), // no namespace
            named(json!("data"), json!(8192), Some("P_x")), // not a namespace
            named(json!("Data"), json!(8192), Some("p-abc-db")), // not a volume name
            named(json!("../x"), json!(8192), Some("p-abc-db")),
            named(json!(""), json!(8192), Some("p-abc-db")),
            named(json!("data"), json!(0), Some("p-abc-db")), // no size
            named(json!("data"), json!("8192"), Some("p-abc-db")), // a string
            named(json!("data"), json!(10_001), Some("p-abc-db")), // over the cap
            named(json!("data"), json!(-5), Some("p-abc-db")),
        ] {
            assert!(
                parse_with(&bad, formatting, Some(10_000), allow).is_err(),
                "{bad}"
            );
        }
        // A volume by name with device numbers is both, and neither.
        let mut both = named(json!("data"), json!(8192), Some("p-abc-db"));
        both["volumes"][0]["block"]["major"] = json!(252);
        assert!(parse_with(&both, formatting, Some(10_000), allow).is_err());
        // Two volumes of one name are one volume, asked for twice with two ids.
        let mut two = named(json!("data"), json!(8192), Some("p-abc-db"));
        let mut second = two["volumes"][0].clone();
        second["block"]["id"] = json!(ID2);
        two["volumes"].as_array_mut().unwrap().push(second);
        assert!(parse_with(&two, formatting, Some(10_000), allow).is_err());
    }

    #[test]
    fn a_device_needs_a_namespace_when_whose_it_is_is_checked() {
        let r = json!({"volumes": [vol(ID, 252, 3)]});
        assert!(parse_with(&r, on(), Some(10_000), allow).is_err());
        let mut r = r;
        r["volumes"][0]["block"]["namespace"] = json!("p-abc-db");
        let got = parse_with(&r, on(), Some(10_000), allow).unwrap();
        assert_eq!(got[0].namespace.as_deref(), Some("p-abc-db"));
        r["volumes"][0]["block"]["namespace"] = json!("P-abc");
        assert!(parse_with(&r, on(), Some(10_000), allow).is_err());
    }

    fn lv(tags: &[&str]) -> crate::lvm::LvInfo {
        crate::lvm::LvInfo {
            vg: "vg".into(),
            lv: "pv-p-abc-db_data".into(),
            size_mib: 100,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            major: 252,
            minor: 3,
        }
    }

    #[test]
    fn a_volume_goes_only_to_the_namespace_its_tag_names_and_only_when_ready() {
        let ready = lv(&["cloudhv-ns=p-abc-db", "cloudhv-ready"]);
        assert!(check_owner(Some(&ready), "p-abc-db", false).is_ok());
        let e = check_owner(Some(&ready), "p-zzz-db", false).unwrap_err();
        assert!(e.contains("another namespace"), "{e}");
        assert!(
            check_owner(Some(&ready), "p-zzz-db", true).is_err(),
            "untagged allowed does not allow another's"
        );
        // Not finished.
        assert!(check_owner(Some(&lv(&["cloudhv-ns=p-abc-db"])), "p-abc-db", false).is_err());
        // Two owners.
        let two = lv(&[
            "cloudhv-ns=p-abc-db",
            "cloudhv-ns=p-zzz-db",
            "cloudhv-ready",
        ]);
        assert!(check_owner(Some(&two), "p-abc-db", false).is_err());
        // No tag: only when it is allowed.
        let bare = lv(&[]);
        assert!(check_owner(Some(&bare), "p-abc-db", false).is_err());
        assert!(check_owner(Some(&bare), "p-abc-db", true).is_ok());
        // Not a volume of the group.
        assert!(check_owner(None, "p-abc-db", true).is_err());
    }
}
