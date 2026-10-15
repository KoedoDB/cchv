//! Running Cloud Hypervisor as a user of its own ("rootless").
//!
//! Cloud Hypervisor is the process a guest attacks when it escapes the VM, so it should not be root. With `ch_rootless` the
//! daemon (which stays root: it creates TAP devices, enters network namespaces and sets up tc) starts each Cloud Hypervisor as
//! a user that exists only for that VM, in the group that may open `/dev/kvm`, with no capabilities and `no_new_privs`.
//!
//! What a VM's own user needs, and how it gets it:
//! - its state directory (`api.sock`, `vsock.sock`, `console.log`): owned by it, mode 0700;
//! - the TAP device: not opened by it. Enabling a TAP needs `CAP_NET_ADMIN`, which would let it change the host's network, so the
//!   daemon opens the device and hands the descriptor over the API socket (SCM_RIGHTS);
//! - the container's erofs image: read access for it alone, by an ACL (the images are the customers', and one VM must not read
//!   another's); the ACL is removed when the VM is destroyed;
//! - the snapshots (`base-snapshot`, `snapshot-cache`): written by the VM that takes them, then handed to the group (0640 root:gid)
//!   so that every VM can restore from them. They hold the memory of a freshly started guest and an image's warm-up, not a
//!   customer's secrets (those arrive after the restore);
//! - the kernel and the base rootfs: world-readable platform files.
//!
//! Userfaultfd restore (`CLOUDHV_MEMORY_RESTORE_MODE=OnDemand`) needs `vm.unprivileged_userfaultfd=1` for a process without
//! `CAP_SYS_PTRACE`; the default `Copy` mode does not.

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::config::DaemonConfig;

struct State {
    uid_base: u32,
    uid_count: u32,
    gid: u32,
    next: u32,
    in_use: HashSet<u32>,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

/// Read the settings once, at start. Fails if rootless is asked for and cannot work.
pub fn init(config: &DaemonConfig) -> Result<()> {
    if !config.ch_rootless {
        let _ = STATE.set(None);
        return Ok(());
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
    for (what, path) in [("kernel_path", &config.kernel_path), ("rootfs_path", &config.rootfs_path)] {
        let mode = std::fs::metadata(path)
            .with_context(|| format!("{what} {path}"))?
            .permissions()
            .mode();
        if mode & 0o004 == 0 {
            anyhow::bail!("{what} {path} must be readable by every user (mode {mode:o}) for rootless Cloud Hypervisor");
        }
    }
    log::info!(
        "Cloud Hypervisor runs rootless: users {}..{}, group {}",
        config.ch_uid_base,
        config.ch_uid_base + config.ch_uid_count,
        gid
    );
    let _ = STATE.set(Some(Mutex::new(State {
        uid_base: config.ch_uid_base,
        uid_count: config.ch_uid_count,
        gid,
        // A different place to start at each start of the daemon, so that a user left behind by a daemon that died is not the first reused.
        next: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
            .unwrap_or(0),
        in_use: HashSet::new(),
    })));
    Ok(())
}

fn state() -> Option<&'static Mutex<State>> {
    STATE.get().and_then(|s| s.as_ref())
}

pub fn enabled() -> bool {
    state().is_some()
}

/// A user for a new VM: (uid, gid). None when rootless is off.
pub fn alloc() -> Result<Option<(u32, u32)>> {
    let Some(m) = state() else { return Ok(None) };
    let mut s = m.lock().unwrap();
    for _ in 0..s.uid_count {
        let uid = s.uid_base + s.next % s.uid_count;
        s.next = s.next.wrapping_add(1);
        if s.in_use.insert(uid) {
            return Ok(Some((uid, s.gid)));
        }
    }
    anyhow::bail!("no free user for a VM: {} are in use", s.uid_count)
}

fn gid() -> u32 {
    state().map(|m| m.lock().unwrap().gid).unwrap_or(0)
}

/// The user of the VM whose state directory this is.
pub fn uid_of(state_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(state_dir.join("ch.uid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

pub fn record(state_dir: &Path, uid: u32) -> Result<()> {
    std::fs::write(state_dir.join("ch.uid"), uid.to_string()).context("write ch.uid")
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("chown {}", path.display()));
    }
    Ok(())
}

/// Hand a file or directory to the VM whose state directory is `state_dir` (no-op when rootless is off). A directory becomes 0700.
pub fn give(path: &Path, state_dir: &Path) -> Result<()> {
    let Some(uid) = uid_of(state_dir) else { return Ok(()) };
    chown(path, uid, gid())?;
    if path.is_dir() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// After a VM wrote a snapshot: every VM's group may read it, nobody else (no-op when rootless is off).
pub fn share_with_vms(dir: &Path) -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let gid = gid();
    chown(dir, 0, gid)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            share_with_vms(&p)?;
        } else {
            chown(&p, 0, gid)?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640))?;
        }
    }
    Ok(())
}

/// Let the VM read one image file, and nothing else read it more than before (a POSIX ACL entry for its user; no-op when rootless is
/// off). Remembered in the VM's state directory so that `release` can take it back.
pub fn allow_read(path: &Path, state_dir: &Path) -> Result<()> {
    let Some(uid) = uid_of(state_dir) else { return Ok(()) };
    // The images are the customers': no one but root and the VMs given an entry may read them.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    acl::add_reader(path, uid).with_context(|| format!("ACL on {}", path.display()))?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("acl.list"))?;
    writeln!(f, "{}", path.display())?;
    Ok(())
}

/// A VM was destroyed: take back its ACL entries and free its user. Read before the state directory is removed.
pub fn release(state_dir: &Path) {
    let Some(uid) = uid_of(state_dir) else { return };
    if let Ok(list) = std::fs::read_to_string(state_dir.join("acl.list")) {
        for path in list.lines().filter(|l| !l.is_empty()) {
            let _ = acl::remove_reader(Path::new(path), uid);
        }
    }
    if let Some(m) = state() {
        m.lock().unwrap().in_use.remove(&uid);
    }
}

/// Just enough of POSIX ACLs (the `system.posix_acl_access` attribute) to add and take back one reader. Done here so that the
/// host needs no `setfacl`.
mod acl {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use anyhow::{Context, Result};

    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const READ: u16 = 4;
    const NAME: &[u8] = b"system.posix_acl_access\0";

    type Entry = (u16, u16, u32); // tag, permissions, id

    fn read(path: &Path) -> Result<Vec<Entry>> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        let mut buf = vec![0u8; 4096];
        let n = unsafe { libc::getxattr(c.as_ptr(), NAME.as_ptr() as _, buf.as_mut_ptr() as _, buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENODATA) {
                // No ACL yet: the entries are the mode's.
                let m = std::fs::metadata(path)?.permissions().mode();
                return Ok(vec![
                    (USER_OBJ, ((m >> 6) & 7) as u16, u32::MAX),
                    (GROUP_OBJ, ((m >> 3) & 7) as u16, u32::MAX),
                    (OTHER, (m & 7) as u16, u32::MAX),
                ]);
            }
            return Err(e).context("getxattr (does the file system support ACLs?)");
        }
        let buf = &buf[..n as usize];
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

    fn write(path: &Path, entries: &mut Vec<Entry>) -> Result<()> {
        entries.sort_by_key(|e| (e.0, e.2));
        let mut buf = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries.iter() {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&perm.to_le_bytes());
            buf.extend_from_slice(&id.to_le_bytes());
        }
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        if unsafe { libc::setxattr(c.as_ptr(), NAME.as_ptr() as _, buf.as_ptr() as _, buf.len(), 0) } != 0 {
            return Err(std::io::Error::last_os_error()).context("setxattr");
        }
        Ok(())
    }

    pub fn add_reader(path: &Path, uid: u32) -> Result<()> {
        let mut e = read(path)?;
        e.retain(|x| !(x.0 == USER && x.2 == uid));
        e.push((USER, READ, uid));
        match e.iter_mut().find(|x| x.0 == MASK) {
            Some(m) => m.1 |= READ,
            None => {
                let g = e.iter().find(|x| x.0 == GROUP_OBJ).map(|x| x.1).unwrap_or(0);
                e.push((MASK, g | READ, u32::MAX));
            }
        }
        write(path, &mut e)
    }

    pub fn remove_reader(path: &Path, uid: u32) -> Result<()> {
        let mut e = read(path)?;
        e.retain(|x| !(x.0 == USER && x.2 == uid));
        if e.iter().any(|x| x.0 == USER || x.0 == GROUP) {
            return write(path, &mut e);
        }
        // The last named entry is gone: make the mask what the owning group has (the mode's group bits are the mask), and drop the ACL.
        let g = e.iter().find(|x| x.0 == GROUP_OBJ).map(|x| x.1).unwrap_or(0);
        e.retain(|x| x.0 != MASK);
        e.push((MASK, g, u32::MAX));
        write(path, &mut e)?;
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        if unsafe { libc::removexattr(c.as_ptr(), NAME.as_ptr() as _) } != 0 {
            return Err(std::io::Error::last_os_error()).context("removexattr");
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn add_and_take_back_a_reader() {
            let dir = std::env::temp_dir().join(format!("cloudhv-acl-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("img");
            std::fs::write(&f, b"x").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
            if add_reader(&f, 200_123).is_err() {
                // /tmp without ACL support: nothing to test here
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            let e = read(&f).unwrap();
            assert!(e.contains(&(USER, READ, 200_123)));
            remove_reader(&f, 200_123).unwrap();
            let m = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o600, "mode after taking the reader back");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
