//! Running Cloud Hypervisor as a user of its own ("rootless").
//!
//! Cloud Hypervisor is the process a guest attacks when it escapes the VM, so it should not be root. With `ch_rootless` the
//! daemon (which stays root: it creates TAP devices, enters network namespaces and sets up tc) starts each Cloud Hypervisor as
//! a user that exists only for that VM, in the group that may open `/dev/kvm`, with no capabilities and `no_new_privs`.
//!
//! Once the guest is compromised, that Cloud Hypervisor is hostile and owns everything it can write. Hence the rules below:
//! root never trusts or follows anything in a directory such a user can write (see `fsutil.rs`), and keeps its own records
//! out of reach (`ctl.rs`).
//!
//! What a VM's own user needs, and how it gets it:
//! - its state directory (`api.sock`, `vsock.sock`, `console.sock`): owned by it, mode 0700;
//! - the TAP device: not opened by it. Enabling a TAP needs `CAP_NET_ADMIN`, which would let it change the host's network, so the
//!   daemon opens the device and hands the descriptor over the API socket (SCM_RIGHTS);
//! - the container's erofs image: read access for it alone, by an ACL (the images are the customers', and one VM must not read
//!   another's); the ACL is removed when the VM is destroyed, and every ACL entry of a user of this range is swept at start;
//! - the snapshots: written by the VM that takes them. Only after that VM is dead and its user has no process left are they handed
//!   over: the base snapshot (a fresh guest) to the group (0640 root:gid) so that every VM can restore from it; a workload
//!   snapshot (`snapshot-cache`) holds a customer's image warmed up in memory, so it is root's alone (0600), and a VM that restores
//!   from it is given read access to its two big files by an ACL, like the rootfs. The hand-over removes whatever is not a plain
//!   file or directory (a link, a FIFO, a device, a file with a second hard link);
//! - the kernel and the base rootfs: world-readable platform files.
//!
//! A user is never given to a second VM while a process of the first is left: freeing a user kills every process that has it, and a
//! user that cannot be cleared is quarantined (not used again until the daemon restarts, where the start sweeps it).
//!
//! Userfaultfd restore (`CLOUDHV_MEMORY_RESTORE_MODE=OnDemand`) needs `vm.unprivileged_userfaultfd=1` for a process without
//! `CAP_SYS_PTRACE`; the default `Copy` mode does not.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use log::warn;

use crate::config::DaemonConfig;
use crate::fsutil;

/// The users of the range: which are taken, and which could not be cleared. Plain data, so that it can be tested.
struct UidPool {
    base: u32,
    count: u32,
    next: u32,
    in_use: HashSet<u32>,
    quarantined: HashSet<u32>,
}

impl UidPool {
    fn new(base: u32, count: u32, start: u32) -> Self {
        Self {
            base,
            count,
            next: start,
            in_use: HashSet::new(),
            quarantined: HashSet::new(),
        }
    }

    fn alloc(&mut self) -> Option<u32> {
        for _ in 0..self.count {
            let uid = self.base + self.next % self.count;
            self.next = self.next.wrapping_add(1);
            if self.in_use.insert(uid) {
                return Some(uid);
            }
        }
        None
    }

    /// A user whose VM is gone and who has no process left. A quarantined user is not freed.
    fn free(&mut self, uid: u32) {
        if !self.quarantined.contains(&uid) {
            self.in_use.remove(&uid);
        }
    }

    /// A user with a process that could not be killed: it stays taken.
    fn quarantine(&mut self, uid: u32) {
        self.in_use.insert(uid);
        self.quarantined.insert(uid);
    }
}

struct State {
    uids: UidPool,
    /// The group of `/dev/kvm` (`ch_gid`): the group of the base snapshot, and the one group a confined VM has besides its own.
    gid: u32,
    /// Whether each VM is also confined (`ch_confine`): then it has a group of its own (the same number as its user) and a cgroup, a
    /// mount namespace, and the rest (see `confine.rs`).
    confine: bool,
    /// The VMs whose outbox (where a confined VM wrote a snapshot) is kept when the VM is destroyed, until it is collected.
    kept_outboxes: HashSet<String>,
    /// The user of each VM, by the name of its state directory. The truth is here, in the daemon's memory; the registry file
    /// (`ch.uid`) is for the sweep after a restart.
    by_vm: HashMap<String, u32>,
    /// Where the images are, and where the cached snapshots are: the only places `allow_read` gives access in. A grant is made on the
    /// file there, never on a path in a VM's own directory, which the VM can change; so it can be taken back from the same file.
    erofs_root: PathBuf,
    snapshot_root: PathBuf,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

/// A range of users that cannot work: empty, wrapping around, or in the range of ordinary users.
fn check_range(base: u32, count: u32) -> Result<()> {
    if count == 0 {
        anyhow::bail!("ch_uid_count must not be 0");
    }
    if base < 1000 {
        anyhow::bail!(
            "ch_uid_base {base} is in the range of system and ordinary users (below 1000)"
        );
    }
    if base.checked_add(count).is_none() {
        anyhow::bail!("ch_uid_base {base} + ch_uid_count {count} does not fit a user ID");
    }
    Ok(())
}

/// Whether a user of /etc/passwd is inside [lo, hi): a range must belong to the daemon alone.
fn passwd_overlaps(passwd: &str, lo: u32, hi: u32) -> Option<String> {
    for line in passwd.lines() {
        let mut f = line.split(':');
        let name = f.next().unwrap_or("");
        let uid: Option<u32> = f.nth(1).and_then(|u| u.parse().ok());
        if let Some(uid) = uid {
            if uid >= lo && uid < hi {
                return Some(format!("{name} ({uid})"));
            }
        }
    }
    None
}

/// Read the settings once, at start. Fails if rootless is asked for and cannot work.
pub fn init(config: &DaemonConfig) -> Result<()> {
    if !config.ch_rootless {
        let _ = STATE.set(None);
        return Ok(());
    }
    check_range(config.ch_uid_base, config.ch_uid_count)?;
    let hi = config.ch_uid_base + config.ch_uid_count;
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        if let Some(who) = passwd_overlaps(&passwd, config.ch_uid_base, hi) {
            anyhow::bail!(
                "the user {who} of /etc/passwd is in the range {}..{hi} of the VMs' users",
                config.ch_uid_base
            );
        }
    }
    let gid = match config.ch_gid {
        Some(g) => g,
        None => std::fs::metadata("/dev/kvm")
            .context("/dev/kvm (needed to find the group Cloud Hypervisor runs as)")?
            .gid(),
    };
    if gid == 0 {
        anyhow::bail!("/dev/kvm belongs to group root; set ch_gid to the group that may open it");
    }
    for (what, path) in [
        ("kernel_path", &config.kernel_path),
        ("rootfs_path", &config.rootfs_path),
    ] {
        let mode = std::fs::metadata(path)
            .with_context(|| format!("{what} {path}"))?
            .mode();
        if mode & 0o004 == 0 {
            anyhow::bail!("{what} {path} must be readable by every user (mode {mode:o}) for rootless Cloud Hypervisor");
        }
    }
    log::info!(
        "Cloud Hypervisor runs rootless: users {}..{}, group {}",
        config.ch_uid_base,
        hi,
        gid
    );
    // A different place to start at each start of the daemon, so that a user left behind by a daemon that died is not the first reused.
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    let _ = STATE.set(Some(Mutex::new(State {
        uids: UidPool::new(config.ch_uid_base, config.ch_uid_count, start),
        gid,
        confine: config.ch_confine,
        kept_outboxes: HashSet::new(),
        by_vm: HashMap::new(),
        erofs_root: PathBuf::from(&config.erofs_cache_dir),
        snapshot_root: config.snapshot_cache_dir(),
    })));
    Ok(())
}

fn state() -> Option<&'static Mutex<State>> {
    STATE.get().and_then(|s| s.as_ref())
}

pub fn enabled() -> bool {
    state().is_some()
}

fn vm_name(state_dir: &Path) -> String {
    state_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// A user for the VM whose state directory this is: (uid, gid); the group is the user's own number when VMs are confined, else the group
/// of `/dev/kvm`. None when rootless is off. Recorded in the daemon's memory and in the
/// VM's registry directory.
pub fn alloc_for(state_dir: &Path) -> Result<Option<(u32, u32)>> {
    let Some(m) = state() else { return Ok(None) };
    let (uid, gid) = {
        let mut s = m.lock().unwrap();
        let uid = s.uids.alloc().ok_or_else(|| {
            anyhow::anyhow!("no free user for a VM: all {} are in use", s.uids.count)
        })?;
        s.by_vm.insert(vm_name(state_dir), uid);
        (uid, if s.confine { uid } else { s.gid })
    };
    if let Err(e) = crate::ctl::write(state_dir, "ch.uid", &uid.to_string()) {
        let mut s = m.lock().unwrap();
        s.by_vm.remove(&vm_name(state_dir));
        s.uids.free(uid);
        return Err(e);
    }
    Ok(Some((uid, gid)))
}

/// A VM of an earlier daemon is taken over (`Pool::adopt`): its user is taken again, so that it is not given to a second VM, and the
/// daemon's memory knows it again. The user comes from the registry (`ch.uid`, root's). Rootless off: nothing.
pub fn adopt(state_dir: &Path) {
    let Some(m) = state() else { return };
    let Some(uid) =
        crate::ctl::read(state_dir, "ch.uid").and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    let mut s = m.lock().unwrap();
    if uid >= s.uids.base && uid - s.uids.base < s.uids.count {
        s.uids.in_use.insert(uid);
    }
    s.by_vm.insert(vm_name(state_dir), uid);
}

/// The group of `/dev/kvm`.
pub fn gid() -> u32 {
    state().map(|m| m.lock().unwrap().gid).unwrap_or(0)
}

/// Whether VMs are confined (`ch_rootless` and `ch_confine`).
pub fn confined() -> bool {
    state().map(|m| m.lock().unwrap().confine).unwrap_or(false)
}

/// The group a VM of this user runs as.
fn group_of(uid: u32) -> u32 {
    match state() {
        Some(m) => {
            let s = m.lock().unwrap();
            if s.confine {
                uid
            } else {
                s.gid
            }
        }
        None => 0,
    }
}

/// The user of the VM whose state directory this is: from the daemon's memory, else from its registry directory (after a restart).
pub fn uid_of(state_dir: &Path) -> Option<u32> {
    if let Some(m) = state() {
        if let Some(uid) = m.lock().unwrap().by_vm.get(&vm_name(state_dir)) {
            return Some(*uid);
        }
    }
    crate::ctl::read(state_dir, "ch.uid").and_then(|s| s.trim().parse().ok())
}

/// Hand a file or directory to the VM whose state directory is `state_dir` (no-op when rootless is off). A directory becomes 0700.
/// Done on a descriptor opened without following links.
pub fn give(path: &Path, state_dir: &Path) -> Result<()> {
    let Some(uid) = uid_of(state_dir) else {
        return Ok(());
    };
    let fd = fsutil::open_nosymlinks(path, libc::O_RDONLY | libc::O_NONBLOCK)
        .with_context(|| format!("open {}", path.display()))?;
    let st = fsutil::fstat(fd.as_raw_fd())?;
    fsutil::fchown(fd.as_raw_fd(), uid, group_of(uid))?;
    if fsutil::is_dir(&st) {
        fsutil::fchmod(fd.as_raw_fd(), 0o700)?;
    }
    Ok(())
}

/// The outbox of a confined VM: a directory of its own, outside its state directory, where it writes a snapshot. A confined VM sees no
/// directory but its own, so a snapshot cannot be written to where it will live; root moves it there once the VM is dead
/// (`collect_outbox`), and only then looks at it.
pub fn make_outbox(state_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let out = crate::confine::out_dir_of(state_dir);
    if let Some(parent) = out.parent() {
        match std::fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("create {}", parent.display())),
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // Nothing of an earlier VM of the same name.
    remove_outbox_dir(&out);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&out)
        .with_context(|| format!("create {}", out.display()))?;
    give(&out, state_dir)?;
    Ok(out)
}

fn remove_outbox_dir(out: &Path) {
    let (Some(parent), Some(name)) = (out.parent(), out.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let Ok(dir) = fsutil::open_nosymlinks(
        parent,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
    ) else {
        return;
    };
    if let Ok(st) = fsutil::fstatat_nofollow(dir.as_raw_fd(), name) {
        if let Err(e) = fsutil::remove_entry(dir.as_raw_fd(), name, &st) {
            warn!("could not remove {}: {e}", out.display());
        }
    }
}

/// The VM took a snapshot into its outbox: keep the outbox when the VM is destroyed, until it is collected.
pub fn keep_outbox(state_dir: &Path) {
    if let Some(m) = state() {
        m.lock().unwrap().kept_outboxes.insert(vm_name(state_dir));
    }
}

/// The snapshot in the outbox is not wanted (the steps that made it failed): it goes.
pub fn discard_outbox(state_dir: &Path) {
    if let Some(m) = state() {
        m.lock().unwrap().kept_outboxes.remove(&vm_name(state_dir));
    }
    remove_outbox_dir(&crate::confine::out_dir_of(state_dir));
}

/// The VM is destroyed: its outbox goes with it, unless it holds a snapshot that is about to be collected.
pub fn drop_outbox(state_dir: &Path) {
    let Some(m) = state() else { return };
    if m.lock()
        .unwrap()
        .kept_outboxes
        .contains(&vm_name(state_dir))
    {
        return;
    }
    remove_outbox_dir(&crate::confine::out_dir_of(state_dir));
}

/// After the VM that took a snapshot is destroyed: its outbox becomes `dest` (which is removed first; it is an empty directory made for
/// the snapshot). The caller then hands the directory over (`hand_over_snapshot`) before it reads anything in it. A VM that is not
/// confined wrote into `dest` itself, and nothing is done.
pub fn collect_outbox(state_dir: &Path, dest: &Path) -> Result<()> {
    let Some(m) = state() else { return Ok(()) };
    if !m.lock().unwrap().kept_outboxes.remove(&vm_name(state_dir)) {
        return Ok(());
    }
    let out = crate::confine::out_dir_of(state_dir);
    match std::fs::remove_dir(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            remove_outbox_dir(&out);
            return Err(e).with_context(|| format!("{} is not an empty directory", dest.display()));
        }
    }
    let r = std::fs::rename(&out, dest)
        .with_context(|| format!("move {} to {}", out.display(), dest.display()));
    if r.is_err() {
        remove_outbox_dir(&out);
    }
    r
}

/// At start: the outboxes of an earlier daemon's VMs go.
pub fn sweep_outboxes(state_root: &Path, keep: &HashSet<String>) {
    let dir = state_root.join(crate::confine::OUT_DIR);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if keep.contains(e.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            remove_outbox_dir(&e.path());
        }
    }
}

/// How deep a snapshot directory may go: it is a directory of files (`config.json`, `state.json`, `memory-ranges`).
const MAX_SNAPSHOT_DEPTH: usize = 4;

/// After the VM that wrote a snapshot is dead and its user is cleared (no-op when rootless is off). The base snapshot (`shared`: a
/// fresh guest, the same for every customer) may be read by the group of all VMs. A workload snapshot holds a customer's image
/// warmed up in memory, so it is root's alone, and a VM is given read access to the one it restores from (`allow_read`).
pub fn hand_over_snapshot(dir: &Path, shared: bool) -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    if shared {
        hand_over_dir(dir, (0, gid()), 0o750, 0o640)
    } else {
        hand_over_dir(dir, (0, 0), 0o700, 0o600)
    }
}

/// Give a directory tree to `owner` with the given modes, on descriptors, without following a link, and remove what is not a plain
/// file or directory: a symbolic link, a device, a FIFO, a socket, a file with a second hard link (it could be another file's), and
/// anything deeper than `MAX_SNAPSHOT_DEPTH`. The VM that wrote it must be dead: a live one could still write through a descriptor.
fn hand_over_dir(dir: &Path, owner: (u32, u32), dir_mode: u32, file_mode: u32) -> Result<()> {
    let fd = fsutil::open_nosymlinks(dir, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK)
        .with_context(|| format!("open {}", dir.display()))?;
    hand_over_fd(fd.as_raw_fd(), 0, owner, dir_mode, file_mode)
        .with_context(|| format!("hand over {}", dir.display()))
}

fn hand_over_fd(
    fd: i32,
    depth: usize,
    owner: (u32, u32),
    dir_mode: u32,
    file_mode: u32,
) -> Result<()> {
    fsutil::fchown(fd, owner.0, owner.1)?;
    fsutil::fchmod(fd, dir_mode)?;
    for name in fsutil::list(fd)? {
        let Ok(st) = fsutil::fstatat_nofollow(fd, &name) else {
            continue;
        };
        let kept = if fsutil::is_reg(&st) && st.st_nlink == 1 {
            match fsutil::openat(
                fd,
                &name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            ) {
                Ok(f) => match fsutil::fstat(f.as_raw_fd()) {
                    // The same file that was looked at (it could have been replaced in between).
                    Ok(st2)
                        if fsutil::is_reg(&st2)
                            && st2.st_nlink == 1
                            && st2.st_ino == st.st_ino
                            && st2.st_dev == st.st_dev =>
                    {
                        fsutil::fchown(f.as_raw_fd(), owner.0, owner.1)?;
                        fsutil::fchmod(f.as_raw_fd(), file_mode)?;
                        true
                    }
                    _ => false,
                },
                Err(_) => false,
            }
        } else if fsutil::is_dir(&st) && depth < MAX_SNAPSHOT_DEPTH {
            match fsutil::openat(
                fd,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            ) {
                Ok(sub) => match fsutil::fstat(sub.as_raw_fd()) {
                    Ok(st2)
                        if fsutil::is_dir(&st2)
                            && st2.st_ino == st.st_ino
                            && st2.st_dev == st.st_dev =>
                    {
                        hand_over_fd(sub.as_raw_fd(), depth + 1, owner, dir_mode, file_mode)?;
                        true
                    }
                    _ => false,
                },
                Err(_) => false,
            }
        } else {
            false
        };
        if !kept {
            warn!("snapshot hand-over: removing {name:?}, which is not a plain file or directory (or is too deep)");
            let st = fsutil::fstatat_nofollow(fd, &name).unwrap_or(st);
            fsutil::remove_entry(fd, &name, &st).with_context(|| format!("remove {name:?}"))?;
        }
    }
    Ok(())
}

/// Whether `path` is one `allow_read` may open: an absolute path with no `.` or `..` and no newline (the records are lines), in the
/// images' directory or in the cache of snapshots.
fn path_allowed(path: &Path, erofs_root: &Path, snapshot_root: &Path) -> bool {
    let s = path.as_os_str().as_encoded_bytes();
    if s.contains(&b'\n') || s.contains(&0) || !path.is_absolute() {
        return false;
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    path.starts_with(erofs_root) || path.starts_with(snapshot_root)
}

/// The places `allow_read` may give access in.
fn roots() -> (PathBuf, PathBuf) {
    state()
        .map(|m| {
            let s = m.lock().unwrap();
            (s.erofs_root.clone(), s.snapshot_root.clone())
        })
        .unwrap_or_default()
}

/// Let the VM read one file, and nothing else read it more than before (a POSIX ACL entry for its user; no-op when rootless is off).
/// The file is opened without following links, and must be a regular file in a place `path_allowed` names. The grant is written to
/// the VM's registry first, so that `release` can take it back even if the entry is only half made.
pub fn allow_read(path: &Path, state_dir: &Path) -> Result<()> {
    let Some(uid) = uid_of(state_dir) else {
        return Ok(());
    };
    let (erofs_root, snapshot_root) = roots();
    if !path_allowed(path, &erofs_root, &snapshot_root) {
        anyhow::bail!("{} is not a file a VM may be given", path.display());
    }
    crate::ctl::append(state_dir, "acl.list", &path.display().to_string())?;
    let fd = fsutil::open_nosymlinks(path, libc::O_RDONLY | libc::O_NONBLOCK)
        .with_context(|| format!("open {}", path.display()))?;
    if !fsutil::is_reg(&fsutil::fstat(fd.as_raw_fd())?) {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    acl::add_reader_fd(fd.as_raw_fd(), uid).with_context(|| format!("ACL on {}", path.display()))
}

/// SIGKILL every process of `uid` until none is left (for up to two seconds). Returns whether none is.
async fn kill_all_of(uid: u32) -> bool {
    for _ in 0..100 {
        let pids = crate::ctl::pids_of_uid(uid);
        if pids.is_empty() {
            return true;
        }
        for pid in pids {
            crate::ctl::kill_if_uid(pid, uid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    crate::ctl::pids_of_uid(uid).is_empty()
}

/// A VM was destroyed (its Cloud Hypervisor is gone): kill what else its user left, take back its ACL entries and free the user. A
/// user that still has a process is quarantined and not given out again. Call before the registry directory is removed.
pub async fn release(state_dir: &Path) {
    let Some(uid) = uid_of(state_dir) else { return };
    // A confined VM has a cgroup: everything in it ends there, whatever it made and wherever (this is the sure way); the scan for the
    // user's processes that follows is for the VM that has none, and for what the cgroup did not hold.
    crate::cgroup::kill_and_remove(&vm_name(state_dir)).await;
    let clear = kill_all_of(uid).await;
    if let Some(list) = crate::ctl::read(state_dir, "acl.list") {
        let (erofs_root, snapshot_root) = roots();
        for line in list.lines().filter(|l| !l.is_empty()) {
            let p = Path::new(line);
            if !path_allowed(p, &erofs_root, &snapshot_root) {
                continue;
            }
            if let Ok(fd) = fsutil::open_nosymlinks(p, libc::O_RDONLY | libc::O_NONBLOCK) {
                let _ = acl::remove_range_fd(fd.as_raw_fd(), uid, uid + 1);
            }
        }
    }
    if let Some(m) = state() {
        let mut s = m.lock().unwrap();
        s.by_vm.remove(&vm_name(state_dir));
        if clear {
            s.uids.free(uid);
        } else {
            warn!("user {uid} still has a process; it is not used again until the daemon restarts");
            s.uids.quarantine(uid);
        }
    }
}

/// At start: kill every process that belongs to a user of this daemon's range. They are the processes of VMs of an earlier daemon (a
/// Cloud Hypervisor, or what it started); the range is the daemon's alone (checked against /etc/passwd at start). A user that still has
/// one afterwards is quarantined. Only when rootless is on. The users in `keep` are those of the VMs just taken over: they stay.
pub async fn sweep_processes(config: &DaemonConfig, keep: &HashSet<u32>) {
    if !config.ch_rootless {
        return;
    }
    let Some(hi) = config.ch_uid_base.checked_add(config.ch_uid_count) else {
        return;
    };
    let lo = config.ch_uid_base;
    let mut killed = 0;
    for _ in 0..100 {
        let procs: Vec<(u32, u32)> = crate::ctl::procs_in_uid_range(lo, hi)
            .into_iter()
            .filter(|(_, uid)| !keep.contains(uid))
            .collect();
        if procs.is_empty() {
            break;
        }
        for (pid, uid) in procs {
            crate::ctl::kill_if_uid(pid, uid);
            killed += 1;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    if killed > 0 {
        log::info!("killed {killed} processes of the VMs' users left by an earlier daemon");
    }
    if let Some(m) = state() {
        let mut s = m.lock().unwrap();
        for (pid, uid) in crate::ctl::procs_in_uid_range(lo, hi) {
            if keep.contains(&uid) {
                continue;
            }
            warn!("process {pid} of user {uid} could not be killed; the user is not used");
            s.uids.quarantine(uid);
        }
    }
}

/// At start: take every ACL entry of a user of this daemon's range off the images and the cached snapshots. The entries of the VMs of
/// an earlier daemon (killed, or the machine restarted with the images on disk) would otherwise stay for ever, and a VM given one of
/// those users later could read an image that was not its own. The users in `keep` (VMs just taken over) keep theirs.
pub fn sweep_acls(config: &DaemonConfig, keep: &HashSet<u32>) {
    if config.ch_uid_count == 0 {
        return;
    }
    let Some(hi) = config.ch_uid_base.checked_add(config.ch_uid_count) else {
        return;
    };
    let lo = config.ch_uid_base;
    let mut swept = 0;
    let roots = [
        PathBuf::from(&config.erofs_cache_dir),
        config.snapshot_cache_dir(),
    ];
    for root in roots {
        let mut stack = vec![(root, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() && depth < MAX_SNAPSHOT_DEPTH {
                    stack.push((p, depth + 1));
                } else if ft.is_file() {
                    if let Ok(fd) = fsutil::open_nosymlinks(&p, libc::O_RDONLY | libc::O_NONBLOCK) {
                        // The range minus the users just taken over: each stretch between them.
                        let mut removed = false;
                        for (a, b) in ranges_without(lo, hi, keep) {
                            if let Ok(true) = acl::remove_range_fd(fd.as_raw_fd(), a, b) {
                                removed = true;
                            }
                        }
                        if removed {
                            swept += 1;
                        }
                    }
                }
            }
        }
    }
    if swept > 0 {
        log::info!(
            "took the ACL entries of the VMs' users off {swept} files left by an earlier daemon"
        );
    }
}

/// `[lo, hi)` without the users in `keep`, as the stretches that remain, in order.
fn ranges_without(lo: u32, hi: u32, keep: &HashSet<u32>) -> Vec<(u32, u32)> {
    let mut kept: Vec<u32> = keep
        .iter()
        .copied()
        .filter(|u| *u >= lo && *u < hi)
        .collect();
    kept.sort_unstable();
    let mut out = Vec::new();
    let mut from = lo;
    for u in kept {
        if u > from {
            out.push((from, u));
        }
        from = u + 1;
    }
    if from < hi {
        out.push((from, hi));
    }
    out
}

/// Just enough of POSIX ACLs (the `system.posix_acl_access` attribute) to add and take back readers. Done here so that the host
/// needs no `setfacl`. Everything is on a descriptor, and one lock covers every read-modify-write: two VMs starting from the same
/// image would otherwise both read, then write, and one entry would be lost.
mod acl {
    use std::io;
    use std::os::unix::io::RawFd;
    use std::sync::Mutex;

    use anyhow::{Context, Result};

    use crate::fsutil;

    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const READ: u16 = 4;
    const NAME: &[u8] = b"system.posix_acl_access\0";
    const NO_ID: u32 = u32::MAX;

    pub type Entry = (u16, u16, u32); // tag, permissions, id

    static LOCK: Mutex<()> = Mutex::new(());

    pub fn decode(buf: &[u8]) -> Result<Vec<Entry>> {
        if buf.len() < 4 || u32::from_le_bytes(buf[..4].try_into()?) != 2 {
            anyhow::bail!("unknown ACL version");
        }
        Ok(buf[4..]
            .chunks_exact(8)
            .map(|e| {
                (
                    u16::from_le_bytes([e[0], e[1]]),
                    u16::from_le_bytes([e[2], e[3]]),
                    u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
                )
            })
            .collect())
    }

    pub fn encode(entries: &[Entry]) -> Vec<u8> {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|e| (e.0, e.2));
        let mut buf = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in sorted {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&perm.to_le_bytes());
            buf.extend_from_slice(&id.to_le_bytes());
        }
        buf
    }

    /// The entries a mode stands for.
    pub fn from_mode(mode: u32) -> Vec<Entry> {
        vec![
            (USER_OBJ, ((mode >> 6) & 7) as u16, NO_ID),
            (GROUP_OBJ, ((mode >> 3) & 7) as u16, NO_ID),
            (OTHER, (mode & 7) as u16, NO_ID),
        ]
    }

    /// The entries with `uid` as a reader (the mask opened as far as reading).
    pub fn with_reader(mut e: Vec<Entry>, uid: u32) -> Vec<Entry> {
        e.retain(|x| !(x.0 == USER && x.2 == uid));
        e.push((USER, READ, uid));
        match e.iter_mut().find(|x| x.0 == MASK) {
            Some(m) => m.1 |= READ,
            None => {
                let g = e
                    .iter()
                    .find(|x| x.0 == GROUP_OBJ)
                    .map(|x| x.1)
                    .unwrap_or(0);
                e.push((MASK, g | READ, NO_ID));
            }
        }
        e
    }

    /// The entries without the named users in [lo, hi), and whether any named user or group is left.
    pub fn without_range(mut e: Vec<Entry>, lo: u32, hi: u32) -> (Vec<Entry>, bool) {
        e.retain(|x| !(x.0 == USER && x.2 >= lo && x.2 < hi));
        let named_left = e.iter().any(|x| x.0 == USER || x.0 == GROUP);
        (e, named_left)
    }

    /// The ACL of the file, None if it has none (its mode is all there is).
    fn get(fd: RawFd) -> io::Result<Option<Vec<Entry>>> {
        let mut buf = vec![0u8; 4096];
        match fsutil::fgetxattr(fd, NAME, &mut buf) {
            Ok(n) => decode(&buf[..n])
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn add_reader_fd(fd: RawFd, uid: u32) -> Result<()> {
        let _lock = LOCK.lock().unwrap();
        let entries = match get(fd).context("read the ACL (does the file system support ACLs?)")? {
            Some(e) => e,
            None => {
                // No ACL yet: the file becomes root's alone (the images are the customers'), and the ACL is built on that.
                fsutil::fchmod(fd, 0o600).context("chmod 0600")?;
                from_mode(0o600)
            }
        };
        fsutil::fsetxattr(fd, NAME, &encode(&with_reader(entries, uid))).context("set the ACL")?;
        Ok(())
    }

    /// Take the named users in [lo, hi) off the file. Returns whether it had any.
    pub fn remove_range_fd(fd: RawFd, lo: u32, hi: u32) -> Result<bool> {
        let _lock = LOCK.lock().unwrap();
        let Some(entries) = get(fd).context("read the ACL")? else {
            return Ok(false);
        };
        if !entries.iter().any(|x| x.0 == USER && x.2 >= lo && x.2 < hi) {
            return Ok(false);
        }
        let (mut rest, named_left) = without_range(entries, lo, hi);
        if named_left {
            fsutil::fsetxattr(fd, NAME, &encode(&rest)).context("set the ACL")?;
            return Ok(true);
        }
        // The last named entry is gone: make the mask what the owning group has (the mode's group bits are the mask), and drop the ACL.
        let g = rest
            .iter()
            .find(|x| x.0 == GROUP_OBJ)
            .map(|x| x.1)
            .unwrap_or(0);
        rest.retain(|x| x.0 != MASK);
        rest.push((MASK, g, NO_ID));
        fsutil::fsetxattr(fd, NAME, &encode(&rest)).context("set the ACL")?;
        fsutil::fremovexattr(fd, NAME).context("remove the ACL")?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        #[test]
        fn the_encoding_round_trips_and_is_sorted() {
            let e = with_reader(from_mode(0o600), 200_123);
            let e = with_reader(e, 200_050);
            let back = decode(&encode(&e)).unwrap();
            let tags: Vec<u16> = back.iter().map(|x| x.0).collect();
            assert_eq!(tags, vec![USER_OBJ, USER, USER, GROUP_OBJ, MASK, OTHER]);
            assert_eq!(back[1].2, 200_050, "named users in order of ID");
            assert!(back.contains(&(MASK, READ, NO_ID)));
        }

        #[test]
        fn a_range_is_taken_off_and_the_rest_stays() {
            let e = with_reader(with_reader(from_mode(0o600), 200_001), 7);
            let (rest, named_left) = without_range(e, 200_000, 260_000);
            assert!(named_left, "user 7 is outside the range");
            assert!(!rest.iter().any(|x| x.0 == USER && x.2 == 200_001));
            let (rest, named_left) = without_range(rest, 0, 100);
            assert!(!named_left);
            assert!(rest.iter().all(|x| x.0 != USER));
        }

        #[test]
        fn add_and_take_back_a_reader_on_a_real_file() {
            let dir = std::env::temp_dir().join(format!("cloudhv-acl-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("img");
            std::fs::write(&f, b"x").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            let file = std::fs::File::open(&f).unwrap();
            let fd = file.as_raw_fd();
            if add_reader_fd(fd, 200_123).is_err() {
                // a file system without ACLs: nothing to test here
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            assert!(get(fd).unwrap().unwrap().contains(&(USER, READ, 200_123)));
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o640,
                "owner rw, mask r"
            );
            assert!(remove_range_fd(fd, 200_000, 260_000).unwrap());
            assert!(
                !remove_range_fd(fd, 200_000, 260_000).unwrap(),
                "nothing the second time"
            );
            assert!(get(fd).unwrap().is_none());
            let m = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o600, "mode after taking the reader back");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn users_are_not_given_twice_and_a_quarantined_one_stays_taken() {
        let mut p = UidPool::new(200_000, 3, 0);
        let a = p.alloc().unwrap();
        let b = p.alloc().unwrap();
        let c = p.alloc().unwrap();
        assert_eq!(HashSet::from([a, b, c]).len(), 3);
        assert!(p.alloc().is_none(), "all taken");
        p.free(a);
        assert_eq!(p.alloc(), Some(a));

        let mut q = UidPool::new(200_000, 2, 0);
        let x = q.alloc().unwrap();
        q.quarantine(x);
        q.free(x); // freeing a quarantined user does nothing
        let y = q.alloc().unwrap();
        assert_ne!(x, y);
        assert!(q.alloc().is_none(), "the quarantined user is not given out");
        q.free(y);
        assert_eq!(q.alloc(), Some(y), "only the free one comes back");
    }

    #[test]
    fn a_range_must_be_non_empty_and_clear_of_ordinary_users() {
        assert!(check_range(200_000, 60_000).is_ok());
        assert!(check_range(200_000, 0).is_err());
        assert!(check_range(500, 10).is_err());
        assert!(
            check_range(u32::MAX - 5, 100).is_err(),
            "base + count overflows"
        );
        let passwd = "root:x:0:0::/root:/bin/sh\nnobody:x:65534:65534::/:/bin/false\nvmuser:x:200100:200100::/:/bin/false\n";
        assert!(passwd_overlaps(passwd, 200_000, 260_000)
            .unwrap()
            .contains("vmuser"));
        assert!(passwd_overlaps(passwd, 300_000, 360_000).is_none());
    }

    #[test]
    fn only_the_images_and_the_cached_snapshots_may_be_opened_for_a_vm() {
        let root = Path::new("/run/cloudhv/erofs-cache");
        let snaps = Path::new("/run/cloudhv/daemon/snapshot-cache");
        assert!(path_allowed(
            Path::new("/run/cloudhv/erofs-cache/abc.erofs"),
            root,
            snaps
        ));
        assert!(path_allowed(
            Path::new("/run/cloudhv/daemon/snapshot-cache/0123abcd/memory-ranges"),
            root,
            snaps
        ));
        assert!(!path_allowed(Path::new("/etc/shadow"), root, snaps));
        assert!(!path_allowed(
            Path::new("/run/cloudhv/erofs-cache/../../../etc/shadow"),
            root,
            snaps
        ));
        assert!(
            !path_allowed(
                Path::new("/run/cloudhv/daemon/pool-1-3/snapshot/memory-ranges"),
                root,
                snaps
            ),
            "a path in a VM's own directory, which the VM can change, is not one"
        );
        assert!(
            !path_allowed(
                Path::new("/run/cloudhv/erofs-cache/a\nb.erofs"),
                root,
                snaps
            ),
            "a newline breaks the records"
        );
        assert!(!path_allowed(Path::new("relative/path"), root, snaps));
        assert!(
            !path_allowed(Path::new("/run/cloudhv/erofs-cache2/x"), root, snaps),
            "a sibling is not inside"
        );
    }

    #[test]
    fn the_hand_over_removes_links_fifos_hard_links_and_depth_and_keeps_plain_files() {
        let d = std::env::temp_dir().join(format!("cloudhv-handover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let outside =
            std::env::temp_dir().join(format!("cloudhv-handover-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(d.join("a/b/c/d/e")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("precious"), b"keep").unwrap();
        std::fs::set_permissions(
            outside.join("precious"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(d.join("memory-ranges"), b"mem").unwrap();
        std::fs::write(d.join("state.json"), b"{}").unwrap();
        std::fs::write(d.join("a/ok.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(outside.join("precious"), d.join("filelink")).unwrap();
        std::os::unix::fs::symlink(".", d.join("loop")).unwrap();
        std::fs::hard_link(outside.join("precious"), d.join("hardlinked")).unwrap();
        let fifo = std::ffi::CString::new(d.join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o666) }, 0);
        std::fs::write(d.join("a/b/c/d/e/too-deep"), b"x").unwrap();

        // Own the tree as ourselves (a test does not run as root); the modes are what is checked.
        let me = (unsafe { libc::getuid() }, unsafe { libc::getgid() });
        hand_over_dir(&d, me, 0o700, 0o600).unwrap();

        for gone in ["dirlink", "filelink", "loop", "hardlinked", "fifo"] {
            assert!(
                std::fs::symlink_metadata(d.join(gone)).is_err(),
                "{gone} is removed"
            );
        }
        assert!(
            !d.join("a/b/c/d/e").exists(),
            "deeper than the limit is removed"
        );
        assert_eq!(
            std::fs::read(outside.join("precious")).unwrap(),
            b"keep",
            "what the links pointed at is untouched"
        );
        let mode = |p: &str| std::fs::metadata(d.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("memory-ranges"), 0o600);
        assert_eq!(mode("state.json"), 0o600);
        assert_eq!(mode("a/ok.json"), 0o600);
        assert_eq!(mode("a"), 0o700);
        assert_eq!(
            std::fs::metadata(outside.join("precious"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "the mode of the file a link or hard link pointed at is not changed"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }
}

#[cfg(test)]
mod adopt_tests {
    use super::*;

    #[test]
    fn the_users_taken_over_are_left_out_of_the_range() {
        let none = HashSet::new();
        assert_eq!(
            ranges_without(200_000, 200_010, &none),
            vec![(200_000, 200_010)]
        );
        let keep: HashSet<u32> = [200_003, 200_004, 200_009].into();
        assert_eq!(
            ranges_without(200_000, 200_010, &keep),
            vec![(200_000, 200_003), (200_005, 200_009)]
        );
        let ends: HashSet<u32> = [200_000, 200_009].into();
        assert_eq!(
            ranges_without(200_000, 200_010, &ends),
            vec![(200_001, 200_009)]
        );
        let outside: HashSet<u32> = [5, 999_999].into();
        assert_eq!(
            ranges_without(200_000, 200_010, &outside),
            vec![(200_000, 200_010)]
        );
    }
}
