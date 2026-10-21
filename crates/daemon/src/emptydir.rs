//! Disk-backed emptyDirs.
//!
//! An emptyDir that is not `medium: Memory` is a disk of the pod's VM, backed by a sparse file on the host's disk: not memory (the guest
//! would pay for every byte it writes out of its own RAM), and not a copy of anything. The shim asks for it in the volumes of a request
//! (`"disk": {"id", "size_mb"}`); this module makes the file, puts an ext4 file system in it, and hot-plugs it into the VM with a serial
//! number the guest agent finds it by. The agent mounts it once per VM.
//!
//! - The files live in a directory of each VM, `<emptydir_dir>/<vm name>/`, which is root's (0711). A confined Cloud Hypervisor has that
//!   directory bound into its root, writable, so it can write the file it is given and nothing else: it cannot create, rename or remove
//!   a name in the directory, so root and the VM never race for a name.
//! - The file is made by root (`O_EXCL | O_NOFOLLOW`, 0600), the file system is made on it by `mke2fs` before the VM's user is given the file
//!   (`rootless::give`), and the host never mounts it. What the guest writes into it is the guest's; a file system the guest corrupts
//!   hurts the guest.
//! - The disk is as large as the request says (at most `emptydir_max_mib`) and takes the host's space as the guest writes: a creation is
//!   refused when the host's disk would be left with less than a quarter of the size plus `emptydir_reserve_mib`. The space of disks
//!   already made is not reserved: see `docs/sandbox-daemon.md`.
//! - The directory goes with the VM (`remove_vm_dir`, from `shutdown_and_destroy`), and what an earlier daemon left is removed at start
//!   (`sweep`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::config::DaemonConfig;

/// The most disks one request may ask for, and the smallest (MiB).
const MAX_DISKS: usize = 16;
const MIN_MIB: u64 = 16;
/// How long making the file system may take.
const MKFS_TIMEOUT: Duration = Duration::from_secs(60);

struct Settings {
    root: PathBuf,
    max_mib: u64,
    reserve_mib: u64,
    mke2fs: PathBuf,
    /// Whether the disks are opened with O_DIRECT (`disk_direct`).
    direct: bool,
}

static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();

fn settings() -> Option<&'static Settings> {
    SETTINGS.get().and_then(|s| s.as_ref())
}

/// Read the configuration, make the root directory (0711) and remove what an earlier daemon left in it. The VMs of an earlier daemon are
/// stopped by now. On an error the feature is off and the daemon goes on.
pub fn init(config: &DaemonConfig, keep: &std::collections::HashSet<String>) {
    let s = if config.emptydir_dir.is_empty() {
        None
    } else {
        let root = PathBuf::from(&config.emptydir_dir);
        match prepare_root(&root, keep) {
            Ok(()) => Some(Settings {
                root,
                max_mib: config.emptydir_max_mib,
                reserve_mib: config.emptydir_reserve_mib,
                direct: config.disk_direct,
                mke2fs: find_mke2fs(&config.mke2fs_path),
            }),
            Err(e) => {
                warn!("disk-backed emptyDirs are off: {e:#}");
                None
            }
        }
    };
    if let Some(s) = &s {
        if !s.mke2fs.exists() {
            warn!("{} is not there: a pod with an emptyDir will not start until mke2fs is installed (e2fsprogs)", s.mke2fs.display());
        }
    }
    let _ = SETTINGS.set(s);
}

pub(crate) fn find_mke2fs(configured: &str) -> PathBuf {
    let c = PathBuf::from(configured);
    if c.exists() {
        return c;
    }
    [
        "/usr/sbin/mke2fs",
        "/sbin/mke2fs",
        "/usr/bin/mke2fs",
        "/usr/local/bin/mke2fs",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .unwrap_or(c)
}

fn prepare_root(root: &Path, keep: &std::collections::HashSet<String>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let meta = std::fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711))?;
    let swept = sweep_dir(root, keep);
    if swept > 0 {
        info!("removed the emptyDir disks of {swept} VMs of an earlier daemon");
    }
    Ok(())
}

/// Remove everything in `root` (not `root`): links are removed, never followed. Returns how many entries went.
fn sweep_dir(root: &Path, keep: &std::collections::HashSet<String>) -> usize {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if keep.contains(e.file_name().to_string_lossy().as_ref()) {
                continue; // the disks of a VM just taken over
            }
            let gone = match e.file_type() {
                Ok(t) if t.is_dir() => std::fs::remove_dir_all(&p),
                _ => std::fs::remove_file(&p),
            };
            match gone {
                Ok(()) => n += 1,
                Err(err) => warn!("could not remove {}: {err}", p.display()),
            }
        }
    }
    n
}

/// A VM's directory for disks, named like its state directory.
fn vm_dir_in(root: &Path, state_dir: &Path) -> Option<PathBuf> {
    let name = state_dir.file_name()?.to_str()?;
    crate::validate::is_id(name).then(|| root.join(name))
}

/// The directory of this VM's disks (made by `prepare_vm_dir`), when the feature is on.
pub fn vm_dir(state_dir: &Path) -> Option<PathBuf> {
    vm_dir_in(&settings()?.root, state_dir)
}

/// Make the VM's directory, root's, 0711. Called when its Cloud Hypervisor is spawned (the directory is bound into the confined root), so it may be there
/// already: an empty directory is kept, anything else in a directory of the same name is of an earlier VM and goes. `None` when the feature is off.
pub fn prepare_vm_dir(state_dir: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = vm_dir(state_dir) else {
        return Ok(None);
    };
    if std::fs::symlink_metadata(&dir).is_ok() {
        if !is_empty_dir(&dir) {
            remove_dir(&dir);
        }
    }
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711))?;
    Ok(Some(dir))
}

/// Whether `dir` is a real directory (not a link) with nothing in it.
fn is_empty_dir(dir: &Path) -> bool {
    let is_dir = std::fs::symlink_metadata(dir)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    is_dir
        && std::fs::read_dir(dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false)
}

/// Remove the VM's directory and the disks in it (a VM that never had one is not an error).
pub fn remove_vm_dir(state_dir: &Path) {
    if let Some(dir) = vm_dir(state_dir) {
        remove_dir(&dir);
    }
}

fn remove_dir(dir: &Path) {
    // `remove_dir_all` removes a link, it does not follow it.
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                warn!("could not remove {}: {e}", dir.display());
            }
        }
        Ok(_) => {
            let _ = std::fs::remove_file(dir);
        }
        Err(_) => {}
    }
}

// ── the request ─────────────────────────────────────────────────────────────

/// A disk a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskRequest {
    /// 32 lower-case hex characters: the shim's name of the volume (the same for every container and destination that mounts it).
    pub id: String,
    pub size_mb: u64,
}

/// The virtio-blk serial of a disk (20 bytes at most): what the guest agent finds the disk by.
pub fn serial_of(id: &str) -> String {
    format!("ed{}", id.chars().take(16).collect::<String>())
}

fn is_disk_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The disks of the `volumes` of a request. An error (a bad id or size, too many, or the feature off) refuses the request. A disk asked
/// for twice (one volume mounted at two destinations) is one.
pub fn parse(request: &serde_json::Value) -> Result<Vec<DiskRequest>, String> {
    parse_with(request, settings().map(|s| s.max_mib))
}

fn parse_with(
    request: &serde_json::Value,
    max_mib: Option<u64>,
) -> Result<Vec<DiskRequest>, String> {
    let mut out: Vec<DiskRequest> = Vec::new();
    for v in request
        .get("volumes")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(d) = v.get("disk").filter(|d| !d.is_null()) else {
            continue;
        };
        let max_mib = max_mib.ok_or("disk-backed emptyDirs are off (emptydir_dir)")?;
        let id = d.get("id").and_then(|i| i.as_str()).unwrap_or("");
        if !is_disk_id(id) {
            return Err("a disk id is not 32 hex characters".into());
        }
        let size_mb = d.get("size_mb").and_then(|s| s.as_u64()).unwrap_or(0);
        if !(MIN_MIB..=max_mib).contains(&size_mb) {
            return Err(format!(
                "a disk of {size_mb} MiB is not between {MIN_MIB} and {max_mib}"
            ));
        }
        if v.get("destination").and_then(|d| d.as_str()).is_none() {
            return Err("a disk volume has no destination".into());
        }
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        if out.len() >= MAX_DISKS {
            return Err(format!("more than {MAX_DISKS} disks"));
        }
        out.push(DiskRequest {
            id: id.to_string(),
            size_mb,
        });
    }
    Ok(out)
}

// ── making a disk ───────────────────────────────────────────────────────────

/// Whether the free space (bytes) of the host's disk allows a disk of `size_mb`.
fn enough_space(free_bytes: u64, size_mb: u64, reserve_mib: u64) -> bool {
    let mib = 1024 * 1024;
    free_bytes / mib >= size_mb / 4 + reserve_mib
}

fn free_bytes(dir: &Path) -> Result<u64> {
    let c =
        std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).context("NUL in the path")?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs");
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// The arguments of `mke2fs`: ext4 without a journal (the disk is thrown away with the VM, and a journal is writes), no blocks reserved
/// for root, the inode tables and journal left to be made by the guest's kernel as it uses them (fast, and the file stays sparse).
fn mke2fs_args(image: &Path) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = [
        "-q",
        "-F",
        "-t",
        "ext4",
        "-b",
        "4096",
        "-m",
        "0",
        "-O",
        "^has_journal",
        "-E",
        "lazy_itable_init=1,nodiscard,root_owner=0:0",
        "-L",
        "emptydir",
    ]
    .iter()
    .map(Into::into)
    .collect();
    a.push(image.as_os_str().to_os_string());
    a
}

/// Make the sparse file and the file system in it. Blocking.
async fn make_image(s: &Settings, path: &Path, size_mb: u64) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let free = free_bytes(&s.root)?;
    if !enough_space(free, size_mb, s.reserve_mib) {
        anyhow::bail!(
            "the host's disk has {} MiB free, not enough for an emptyDir of {size_mb} MiB (it keeps {} MiB and a quarter of the size)",
            free / (1024 * 1024),
            s.reserve_mib
        );
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.set_len(size_mb * 1024 * 1024)
        .context("size the disk file")?;
    drop(file);

    let mut cmd = tokio::process::Command::new(&s.mke2fs);
    cmd.args(mke2fs_args(path))
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

/// The disks of a request are in the VM: for each one not there yet, make its file, give it to the VM's user and hot-plug it. A disk
/// the VM has already (a second container of the pod asks for the same volume) is left as it is, whatever size it now asks for. On an
/// error the file of the disk that failed is removed.
pub async fn attach(api_socket: &Path, disks: &[DiskRequest]) -> Result<()> {
    if disks.is_empty() {
        return Ok(());
    }
    let s = settings().context("disk-backed emptyDirs are off")?;
    let state_dir = api_socket.parent().context("the VM's state directory")?;
    let dir = vm_dir_in(&s.root, state_dir).context("the VM has no name for disks")?;
    // Two containers of one pod may ask for the same disk together.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _held = LOCK.lock().await;
    for d in disks {
        if !is_disk_id(&d.id) {
            anyhow::bail!("a disk id is not 32 hex characters");
        }
        let path = dir.join(format!("{}.img", d.id));
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Err(e) = make_and_plug(s, api_socket, state_dir, &path, d).await {
            let _ = std::fs::remove_file(&path);
            return Err(e.context(format!("emptyDir disk of {} MiB", d.size_mb)));
        }
        info!(
            "emptyDir disk {} of {} MiB attached to {}",
            d.id,
            d.size_mb,
            state_dir.display()
        );
    }
    Ok(())
}

/// The body of the `vm.add-disk` request of an emptyDir's disk.
fn disk_body(path: &Path, d: &DiskRequest, direct: bool) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "readonly": false,
        "image_type": "Raw",
        // O_DIRECT: the guest has its own page cache; without this the host caches every block a second time, and that is charged to the
        // pod's cgroup (a database pod of 1 GiB had 320 MB of the host's cache of its emptyDir in it).
        "direct": direct,
        "id": format!("ed-{}", d.id.chars().take(16).collect::<String>()),
        "serial": serial_of(&d.id),
    })
}

async fn make_and_plug(
    s: &Settings,
    api_socket: &Path,
    state_dir: &Path,
    path: &Path,
    d: &DiskRequest,
) -> Result<()> {
    make_image(s, path, d.size_mb).await?;
    // Only now the VM's user owns it: the file system was made by root, and the VM could not have touched the file before.
    crate::rootless::give(path, state_dir).context("give the disk to the VM")?;
    let body = disk_body(path, d, s.direct);
    crate::vm_lifecycle::api_request_with_body(
        api_socket,
        "PUT",
        "/api/v1/vm.add-disk",
        &body.to_string(),
    )
    .await
    .context("hot-plug the disk")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_disk_of_an_emptydir_is_read_and_written_around_the_hosts_page_cache() {
        let d = DiskRequest {
            id: "0123456789abcdef0123456789abcdef".into(),
            size_mb: 64,
        };
        let b = disk_body(Path::new("/data/x.img"), &d, true);
        assert_eq!(b["direct"], json!(true));
        // `disk_direct: false` leaves the host's page cache in between.
        assert_eq!(
            disk_body(Path::new("/data/x.img"), &d, false)["direct"],
            json!(false)
        );
        assert_eq!(b["readonly"], json!(false));
        assert_eq!(b["image_type"], json!("Raw"));
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-emptydir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_serial_fits_the_twenty_bytes_of_virtio_blk() {
        let s = serial_of(ID);
        assert_eq!(s, "ed0123456789abcdef");
        assert!(s.len() <= 20);
    }

    #[test]
    fn the_disks_of_a_request_are_checked_and_one_volume_is_one_disk() {
        let two_destinations = json!({"volumes": [
            {"destination": "/a", "disk": {"id": ID, "size_mb": 1024}},
            {"destination": "/b", "disk": {"id": ID, "size_mb": 1024}},
            {"destination": "/etc/hosts", "source": "/x", "files": []},
        ]});
        assert_eq!(
            parse_with(&two_destinations, Some(16384)).unwrap(),
            vec![DiskRequest {
                id: ID.into(),
                size_mb: 1024
            }]
        );
        assert_eq!(
            parse_with(&json!({}), None).unwrap(),
            vec![],
            "no disks needs nothing"
        );

        let bad = |disk: serde_json::Value| {
            parse_with(
                &json!({"volumes": [{"destination": "/a", "disk": disk}]}),
                Some(16384),
            )
        };
        assert!(bad(json!({"id": "../../etc/passwd", "size_mb": 100})).is_err());
        assert!(bad(json!({"id": ID.to_uppercase(), "size_mb": 100})).is_err());
        assert!(bad(json!({"id": &ID[..31], "size_mb": 100})).is_err());
        assert!(bad(json!({"id": ID, "size_mb": 0})).is_err());
        assert!(bad(json!({"id": ID, "size_mb": 15})).is_err());
        assert!(
            bad(json!({"id": ID, "size_mb": 16385})).is_err(),
            "above emptydir_max_mib"
        );
        assert!(bad(json!({"id": ID})).is_err());
        assert!(bad(json!({"size_mb": 100})).is_err());
        assert!(bad(json!({"id": ID, "size_mb": 16})).is_ok());
        assert!(bad(json!({"id": ID, "size_mb": 16384})).is_ok());
        // the feature off: refused, not quietly made of memory
        assert!(parse_with(&two_destinations, None).is_err());
    }

    #[test]
    fn at_most_sixteen_disks_a_request() {
        let vols: Vec<_> = (0..17)
            .map(|i| json!({"destination": format!("/d{i}"), "disk": {"id": format!("{i:032x}"), "size_mb": 64}}))
            .collect();
        assert!(parse_with(&json!({"volumes": vols[..16]}), Some(1024)).is_ok());
        assert!(parse_with(&json!({"volumes": vols}), Some(1024)).is_err());
    }

    #[test]
    fn a_disk_is_refused_when_the_host_would_be_left_short() {
        let gib = 1024 * 1024 * 1024u64;
        // a 1 GiB disk needs 256 MiB + the reserve (2 GiB)
        assert!(enough_space(3 * gib, 1024, 2048));
        assert!(!enough_space(2 * gib + 200 * 1024 * 1024, 1024, 2048));
        assert!(!enough_space(0, 16, 2048));
        assert!(enough_space(10 * gib, 16384, 2048) == (10 * 1024 >= 16384 / 4 + 2048));
    }

    #[test]
    fn a_vm_directory_is_named_like_the_vm_and_nothing_else() {
        let root = Path::new("/var/lib/cloudhv/emptydir");
        assert_eq!(
            vm_dir_in(root, Path::new("/run/cloudhv/daemon/pool-1-4")),
            Some(root.join("pool-1-4"))
        );
        assert_eq!(vm_dir_in(root, Path::new("/run/cloudhv/daemon/..")), None);
        assert_eq!(vm_dir_in(root, Path::new("/run/cloudhv/daemon/a.b")), None);
        assert_eq!(vm_dir_in(root, Path::new("/")), None);
    }

    #[test]
    fn mke2fs_gets_the_file_last_and_no_journal() {
        let a = mke2fs_args(Path::new("/x/y.img"));
        assert_eq!(a.last().unwrap(), "/x/y.img");
        assert!(a.iter().any(|x| x == "^has_journal"));
        assert!(
            a.iter().any(|x| x == "-F"),
            "the file is not a block device"
        );
    }

    #[test]
    fn sweeping_removes_the_vm_directories_and_links_without_following_them() {
        let root = tmp("sweep");
        let outside = tmp("sweep-outside");
        std::fs::write(outside.join("keep"), b"x").unwrap();
        std::fs::create_dir_all(root.join("pool-1-1")).unwrap();
        std::fs::write(root.join("pool-1-1").join(format!("{ID}.img")), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("pool-1-1").join("inner")).unwrap();
        assert_eq!(sweep_dir(&root, &std::collections::HashSet::new()), 2);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(
            outside.join("keep").exists(),
            "a link is removed, what it points to is not"
        );
    }

    #[test]
    fn sweeping_leaves_the_disks_of_the_vms_taken_over() {
        let root = tmp("sweep-keep");
        std::fs::create_dir_all(root.join("pool-1-1")).unwrap();
        std::fs::create_dir_all(root.join("pool-1-2")).unwrap();
        std::fs::write(root.join("pool-1-2").join("a.img"), b"x").unwrap();
        let keep: std::collections::HashSet<String> = ["pool-1-2".to_string()].into();
        assert_eq!(sweep_dir(&root, &keep), 1);
        assert!(!root.join("pool-1-1").exists());
        assert!(root.join("pool-1-2").join("a.img").exists());
    }

    #[tokio::test]
    async fn an_image_is_a_sparse_file_with_an_ext4_file_system() {
        let mke2fs = find_mke2fs("/usr/sbin/mke2fs");
        if !mke2fs.exists() {
            eprintln!("no mke2fs here: skipped");
            return;
        }
        let root = tmp("image");
        let s = Settings {
            root: root.clone(),
            max_mib: 1024,
            reserve_mib: 0,
            mke2fs,
            direct: true,
        };
        let path = root.join(format!("{ID}.img"));
        make_image(&s, &path, 64).await.unwrap();
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&path).unwrap();
        assert_eq!(m.len(), 64 * 1024 * 1024);
        assert!(
            m.blocks() * 512 < 16 * 1024 * 1024,
            "the file is sparse: {} bytes on disk",
            m.blocks() * 512
        );
        assert_eq!(m.mode() & 0o777, 0o600);
        // the ext4 superblock magic is 0xEF53 at 1024 + 56
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[1024 + 56..1024 + 58], &[0x53, 0xEF]);
        // never over a file that is there (nor through a link)
        assert!(make_image(&s, &path, 64).await.is_err());
        let link = root.join("link.img");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        assert!(make_image(&s, &link, 64).await.is_err());
        assert!(std::fs::read_to_string("/etc/passwd")
            .unwrap()
            .contains("root"));
    }
}
