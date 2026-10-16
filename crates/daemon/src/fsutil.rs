//! File-system helpers for a daemon that is root and works in directories other users can write to.
//!
//! A Cloud Hypervisor that runs as its own user owns its VM's state directory and the snapshot directory it writes. Whatever it
//! leaves there (a symbolic link, a hard link, a FIFO) is hostile once the guest has been compromised, and anything root does
//! with a *path* in such a directory (`chown`, `chmod`, `open` for writing) follows the link. So root works here on descriptors:
//! it opens with `O_NOFOLLOW` (or `openat2` with `RESOLVE_NO_SYMLINKS`), looks at what it got with `fstat`, and changes that.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

fn cstr(path: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    Ok(unsafe { OwnedFd::from_raw_fd(cvt(fd)?) })
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const SYS_OPENAT2: libc::c_long = 437;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

/// Open an absolute path without following a symbolic link in any of its components. `openat2` (Linux 5.6) does it in one step; on
/// an older kernel every component is looked at first and the last is opened with `O_NOFOLLOW` (a race is then possible, in a
/// directory someone else can write to, so a kernel with `openat2` is what to run).
pub fn open_nosymlinks(path: &Path, flags: i32) -> io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not absolute",
        ));
    }
    let c = cstr(path)?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    };
    let rc = unsafe {
        libc::syscall(
            SYS_OPENAT2,
            libc::AT_FDCWD,
            c.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if rc >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(rc as RawFd) });
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOSYS) {
        return Err(err);
    }
    for p in path.ancestors() {
        if p == Path::new("/") || p.as_os_str().is_empty() {
            continue;
        }
        if std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            return Err(io::Error::from_raw_os_error(libc::ELOOP));
        }
    }
    owned(unsafe { libc::open(c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) })
}

pub fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

pub fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    cvt(unsafe { libc::fchown(fd, uid, gid) }).map(|_| ())
}

pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    cvt(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(|_| ())
}

pub fn is_reg(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

pub fn openat(dir: RawFd, name: &str, flags: i32) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL"))?;
    owned(unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_CLOEXEC) })
}

/// `lstat` of `name` in the directory `dir` (a link is not followed).
pub fn fstatat_nofollow(dir: RawFd, name: &str) -> io::Result<libc::stat> {
    let c = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL"))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstatat(dir, c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

/// What `remove_entry` will go through: a tree a VM made can be as deep and as wide as the file system allows, and the daemon, as root,
/// must not use its stack or its time on it.
const REMOVE_MAX_DEPTH: usize = 64;
const REMOVE_MAX_ENTRIES: usize = 200_000;

/// Remove an entry of the directory `dir` whatever it is (a directory with everything below it too). Does not follow links. A tree
/// deeper than `REMOVE_MAX_DEPTH` or with more than `REMOVE_MAX_ENTRIES` entries is not removed (an error, and what could be removed is
/// removed): the caller must treat that as a failure and not reuse the place.
pub fn remove_entry(dir: RawFd, name: &str, st: &libc::stat) -> io::Result<()> {
    let mut budget = REMOVE_MAX_ENTRIES;
    remove_entry_bounded(dir, name, st, 0, &mut budget)
}

fn remove_entry_bounded(
    dir: RawFd,
    name: &str,
    st: &libc::stat,
    depth: usize,
    budget: &mut usize,
) -> io::Result<()> {
    let c = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL"))?;
    if is_dir(st) {
        if depth >= REMOVE_MAX_DEPTH {
            return Err(io::Error::other(
                "the directory tree is deeper than the limit",
            ));
        }
        // Empty it first (through a descriptor, so a link in it is removed, not followed), then the directory.
        let mut failed = None;
        if let Ok(sub) = openat(
            dir,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        ) {
            for child in list(sub.as_raw_fd())? {
                if *budget == 0 {
                    return Err(io::Error::other(
                        "the directory tree has more entries than the limit",
                    ));
                }
                *budget -= 1;
                if let Ok(cst) = fstatat_nofollow(sub.as_raw_fd(), &child) {
                    if let Err(e) =
                        remove_entry_bounded(sub.as_raw_fd(), &child, &cst, depth + 1, budget)
                    {
                        failed = Some(e);
                    }
                }
            }
        }
        if let Some(e) = failed {
            // A limit was hit below: the directory is not empty, and saying so is the point.
            if e.kind() == io::ErrorKind::Other {
                return Err(e);
            }
        }
        cvt(unsafe { libc::unlinkat(dir, c.as_ptr(), libc::AT_REMOVEDIR) }).map(|_| ())
    } else {
        cvt(unsafe { libc::unlinkat(dir, c.as_ptr(), 0) }).map(|_| ())
    }
}

/// The names in the directory `fd` (not `.` and `..`).
pub fn list(fd: RawFd) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/self/fd/{fd}"))? {
        names.push(entry?.file_name().to_string_lossy().to_string());
    }
    Ok(names)
}

/// Create a file that must not exist yet, without following a link at its name (mode 0600), and return it.
pub fn create_new(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// `fgetxattr` / `fsetxattr` / `fremovexattr` on a descriptor.
pub fn fgetxattr(fd: RawFd, name: &[u8], buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::fgetxattr(fd, name.as_ptr() as _, buf.as_mut_ptr() as _, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn fsetxattr(fd: RawFd, name: &[u8], value: &[u8]) -> io::Result<()> {
    cvt(unsafe { libc::fsetxattr(fd, name.as_ptr() as _, value.as_ptr() as _, value.len(), 0) })
        .map(|_| ())
}

pub fn fremovexattr(fd: RawFd, name: &[u8]) -> io::Result<()> {
    cvt(unsafe { libc::fremovexattr(fd, name.as_ptr() as _) }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-fsutil-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_link_in_any_component_is_refused() {
        let d = tmp("nosym");
        std::fs::create_dir_all(d.join("real")).unwrap();
        std::fs::write(d.join("real/f"), b"x").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        std::os::unix::fs::symlink(d.join("real/f"), d.join("real/flink")).unwrap();
        assert!(open_nosymlinks(&d.join("real/f"), libc::O_RDONLY).is_ok());
        assert!(
            open_nosymlinks(&d.join("link/f"), libc::O_RDONLY).is_err(),
            "link as a directory"
        );
        assert!(
            open_nosymlinks(&d.join("real/flink"), libc::O_RDONLY).is_err(),
            "link as the file"
        );
        assert!(open_nosymlinks(Path::new("relative"), libc::O_RDONLY).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn remove_entry_gives_up_on_a_tree_that_is_too_deep_or_too_wide() {
        // Deeper than the limit: built with relative opens, so the path length does not matter. It must fail, not recurse to the end.
        let d = tmp("deep");
        let mut cur = open_nosymlinks(&d, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        for _ in 0..(REMOVE_MAX_DEPTH + 20) {
            let c = std::ffi::CString::new("d").unwrap();
            assert_eq!(
                unsafe { libc::mkdirat(cur.as_raw_fd(), c.as_ptr(), 0o755) },
                0
            );
            cur = openat(cur.as_raw_fd(), "d", libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        }
        let top = open_nosymlinks(&d, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        let st = fstatat_nofollow(top.as_raw_fd(), "d").unwrap();
        assert!(remove_entry(top.as_raw_fd(), "d", &st).is_err(), "too deep");
        let _ = std::fs::remove_dir_all(&d);

        // An ordinary tree is removed.
        let e = tmp("shallow");
        std::fs::create_dir_all(e.join("a/b/c")).unwrap();
        std::fs::write(e.join("a/b/c/f"), b"x").unwrap();
        let top = open_nosymlinks(&e, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        let st = fstatat_nofollow(top.as_raw_fd(), "a").unwrap();
        remove_entry(top.as_raw_fd(), "a", &st).unwrap();
        assert!(!e.join("a").exists());
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn remove_entry_does_not_follow_links() {
        let d = tmp("rm");
        let victim = tmp("rm-victim");
        std::fs::write(victim.join("keep"), b"x").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::os::unix::fs::symlink(&victim, d.join("sub/escape")).unwrap();
        let dir = open_nosymlinks(&d, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        let st = fstatat_nofollow(dir.as_raw_fd(), "sub").unwrap();
        remove_entry(dir.as_raw_fd(), "sub", &st).unwrap();
        assert!(!d.join("sub").exists());
        assert!(
            victim.join("keep").exists(),
            "what the link pointed at is still there"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&victim);
    }
}
