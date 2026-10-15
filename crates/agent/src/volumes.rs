//! Writing a pod's volumes (ConfigMaps, Secrets, tokens, emptyDirs) into the guest.
//!
//! The files come from the host over the RPC and are written as root in the guest, outside any container's mount namespace, while
//! containers of the same pod may already be running with the same volume mounted read-write. A container can plant a symbolic link
//! in a volume it has; this code must not follow it. So:
//!
//! - a volume lives in one directory directly under a root (`/run/cloudhv-vols/<name>`); the name is a single safe component;
//! - every step is relative to a directory descriptor and uses `O_NOFOLLOW` (a link in the way is an error, not followed);
//! - a file is created with `O_EXCL` and with its final mode (less the umask, so never wider), then given its exact mode;
//! - a volume that a container is using is left as it is (the first writer wins), and a volume nobody uses is rewritten: a file in the
//!   way is unlinked (the link itself, never its target) and created again.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where in the guest the host's volumes are written; the same prefix as the shim's.
pub const VOLUME_ROOT: &str = "/run/cloudhv-vols";

/// The files of a volume as the RPC carries them.
pub struct VolumeFile<'a> {
    pub path: &'a str,
    pub content: &'a [u8],
    pub mode: u32,
}

/// One safe path component: 1 to 128 characters of `[A-Za-z0-9._-]`, not `.`, `..` or starting with a dot.
pub fn safe_component(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 128
        && !c.starts_with('.')
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// The name of a volume directory under VOLUME_ROOT, from the `source` the RPC carries: exactly `/run/cloudhv-vols/<name>`.
pub fn shared_name(source: &str) -> Option<&str> {
    let rest = source.strip_prefix(VOLUME_ROOT)?.strip_prefix('/')?;
    safe_component(rest).then_some(rest)
}

/// The name of a volume directory in the legacy per-container place, from the last component of an arbitrary `source`.
pub fn legacy_name(source: &str) -> Option<&str> {
    let last = source.trim_end_matches('/').rsplit('/').next()?;
    safe_component(last).then_some(last)
}

/// Where a volume's directory is: `(root, name)`. A `source` under VOLUME_ROOT is the shared place (the containers of a pod that mount
/// the same host directory share it) and must be exactly `/run/cloudhv-vols/<name>`; any other `source` can only name a directory in
/// this container's own `volumes` directory, by its last component. The path is never used as it came.
pub fn location(source: &str, bundle: &Path) -> Result<(PathBuf, String)> {
    if source.starts_with(VOLUME_ROOT) {
        let name = shared_name(source).with_context(|| format!("volume source {source:?} is not allowed"))?;
        return Ok((PathBuf::from(VOLUME_ROOT), name.to_string()));
    }
    let name = legacy_name(source).with_context(|| format!("volume source {source:?} is not allowed"))?;
    Ok((bundle.join("volumes"), name.to_string()))
}

/// A relative path inside a volume: at most 16 components, each a plain file name (no `/` or NUL, not `.`, and nothing starting with
/// `..`, which is where the kubelet keeps its own links), no leading slash.
pub fn safe_relative(path: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').collect();
    (parts.len() <= 16 && parts.iter().all(|c| file_component(c))).then_some(parts)
}

fn file_component(c: &str) -> bool {
    !c.is_empty() && c.len() <= 255 && c != "." && !c.starts_with("..") && !c.contains('\0')
}

/// The mode of a written file: the permission bits only (no setuid, setgid or sticky), and 0600 if the host gave none.
pub fn file_mode(mode: u32) -> u32 {
    match mode & 0o777 {
        0 => 0o600,
        m => m,
    }
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow::anyhow!("NUL in a path component"))
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// Open a directory below `dirfd` without following a link.
fn open_dir_at(dirfd: RawFd, name: &str) -> Result<OwnedFd> {
    let c = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            dirfd,
            c.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY,
        )
    };
    if fd < 0 {
        return Err(errno()).with_context(|| format!("open directory {name} (a link or a file in the way?)"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Make a directory below `dirfd` if it is not there, then open it without following a link.
fn ensure_dir_at(dirfd: RawFd, name: &str, mode: u32) -> Result<OwnedFd> {
    let c = cstr(name)?;
    if unsafe { libc::mkdirat(dirfd, c.as_ptr(), mode) } != 0 {
        let e = errno();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e).with_context(|| format!("mkdir {name}"));
        }
    }
    open_dir_at(dirfd, name)
}

/// Open the volume directory `root/name` (made if needed), as a descriptor. `root` is made too, and must not be a link.
fn open_volume(root: &Path, name: &str) -> Result<OwnedFd> {
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let root_c = cstr(&root.to_string_lossy())?;
    let rootfd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY,
        )
    };
    if rootfd < 0 {
        return Err(errno()).with_context(|| format!("open {}", root.display()));
    }
    let rootfd = unsafe { OwnedFd::from_raw_fd(rootfd) };
    ensure_dir_at(rootfd.as_raw_fd(), name, 0o755)
}

/// Create (or replace) one file below `dir`. A link in the way of a directory component is an error; a file or link at the name itself
/// is unlinked (the entry, never what it points to) and created again, with the final mode from the start.
fn write_file_at(dir: &OwnedFd, rel: &[&str], content: &[u8], mode: u32) -> Result<()> {
    let (name, parents) = rel.split_last().context("empty file path")?;
    let mut owned: Option<OwnedFd> = None;
    for part in parents {
        let cur = owned.as_ref().unwrap_or(dir).as_raw_fd();
        owned = Some(ensure_dir_at(cur, part, 0o755)?);
    }
    let cur = owned.as_ref().unwrap_or(dir).as_raw_fd();
    let c = cstr(name)?;

    if unsafe { libc::unlinkat(cur, c.as_ptr(), 0) } != 0 {
        let e = errno();
        if e.raw_os_error() != Some(libc::ENOENT) {
            return Err(e).with_context(|| format!("replace {name} (a directory in the way?)"));
        }
    }
    let fd = unsafe {
        libc::openat(
            cur,
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            file_mode(mode),
        )
    };
    if fd < 0 {
        return Err(errno()).with_context(|| format!("create {name}"));
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    // The umask can only narrow the mode the file was created with; this sets exactly the one asked for.
    if unsafe { libc::fchmod(file.as_raw_fd(), file_mode(mode)) } != 0 {
        return Err(errno()).context("fchmod");
    }
    file.write_all(content)
        .with_context(|| format!("write {name}"))?;
    Ok(())
}

/// Make the volume `root/name` ready for a container and return the path to bind. `files` is empty for an emptyDir (a directory every
/// user can write to, kept as it is across container restarts). With files (a ConfigMap, Secret, token), they are written unless a
/// container is using the volume already: then they are left as they are. A single file named like the destination is bound as the
/// file (crun cannot mount a directory on a file).
pub fn materialize(
    root: &Path,
    name: &str,
    files: &[VolumeFile<'_>],
    destination: &str,
    in_use: bool,
) -> Result<PathBuf> {
    let dir = open_volume(root, name)?;
    let path = root.join(name);

    if files.is_empty() {
        // An emptyDir: like Kubernetes, writable by every user.
        if unsafe { libc::fchmod(dir.as_raw_fd(), 0o777) } != 0 {
            return Err(errno()).context("fchmod emptyDir");
        }
        return Ok(path);
    }

    if in_use {
        log::info!(
            "volume {name} is in use by a running container; its files are not rewritten"
        );
    } else {
        for f in files {
            let rel = safe_relative(f.path)
                .with_context(|| format!("unsafe file path in volume {name}: {:?}", f.path))?;
            write_file_at(&dir, &rel, f.content, f.mode)?;
        }
    }

    if files.len() == 1 {
        let dest_base = Path::new(destination).file_name().and_then(|n| n.to_str());
        let file_base = Path::new(files[0].path).file_name().and_then(|n| n.to_str());
        if dest_base.is_some() && dest_base == file_base {
            return Ok(path.join(files[0].path));
        }
    }
    Ok(path)
}

/// Remove a volume directory (a Secret or ConfigMap copy nobody uses any more). `remove_dir_all` does not follow links.
pub fn remove(root: &Path, name: &str) {
    if safe_component(name) {
        let _ = std::fs::remove_dir_all(root.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-agent-vol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn f<'a>(path: &'a str, content: &'a [u8], mode: u32) -> VolumeFile<'a> {
        VolumeFile { path, content, mode }
    }

    #[test]
    fn sources_must_be_one_safe_name_under_the_volume_root() {
        assert_eq!(shared_name("/run/cloudhv-vols/00ab12"), Some("00ab12"));
        assert_eq!(shared_name("/run/cloudhv-vols/"), None);
        assert_eq!(shared_name("/run/cloudhv-vols"), None);
        assert_eq!(shared_name("/run/cloudhv-vols/.."), None);
        assert_eq!(shared_name("/run/cloudhv-vols/a/b"), None);
        assert_eq!(shared_name("/run/cloudhv-vols/../x"), None);
        assert_eq!(shared_name("/run/cloudhv-vols/a b"), None);
        assert_eq!(shared_name("/run/cloudhv-volsX/a"), None);
        assert_eq!(shared_name("/"), None);
        assert_eq!(legacy_name("/etc/passwd"), Some("passwd"));
        assert_eq!(legacy_name(".."), None);
        assert_eq!(legacy_name("/a/.."), None);
        assert_eq!(legacy_name("/"), None);
    }

    #[test]
    fn a_volume_is_located_by_its_name_only() {
        let b = Path::new("/run/containers/c1");
        assert_eq!(location("/run/cloudhv-vols/ab12", b).unwrap(), (PathBuf::from(VOLUME_ROOT), "ab12".to_string()));
        assert!(location("/run/cloudhv-vols/../../etc", b).is_err());
        assert!(location("/run/cloudhv-volsX/a", b).is_err());
        assert!(location("/run/cloudhv-vols/a/b", b).is_err());
        // not under the shared root: only the last component names a directory of the container's own
        assert_eq!(location("/some/where/data", b).unwrap(), (b.join("volumes"), "data".to_string()));
        assert!(location("/", b).is_err());
        assert!(location("/a/..", b).is_err());
    }

    #[test]
    fn relative_paths_have_safe_components_only() {
        assert_eq!(safe_relative("app.conf"), Some(vec!["app.conf"]));
        assert_eq!(safe_relative("a/b/c.txt"), Some(vec!["a", "b", "c.txt"]));
        assert_eq!(safe_relative(".env"), Some(vec![".env"]));
        assert_eq!(safe_relative("my file (1).txt"), Some(vec!["my file (1).txt"]));
        assert_eq!(safe_relative("/abs"), None);
        assert_eq!(safe_relative("a/../b"), None);
        assert_eq!(safe_relative("a//b"), None);
        assert_eq!(safe_relative(".."), None);
        assert_eq!(safe_relative("..data/x"), None);
        assert_eq!(safe_relative(""), None);
    }

    #[test]
    fn modes_lose_setuid_setgid_sticky_and_a_missing_mode_is_private() {
        assert_eq!(file_mode(0o4755), 0o755);
        assert_eq!(file_mode(0o2644), 0o644);
        assert_eq!(file_mode(0o1777), 0o777);
        assert_eq!(file_mode(0), 0o600);
        assert_eq!(file_mode(0o7000), 0o600);
    }

    #[test]
    fn files_are_written_with_their_mode_and_nested_directories() {
        let root = tmp("write");
        let p = materialize(
            &root,
            "v1",
            &[f("app.conf", b"x=1", 0o4644), f("sub/dir/t", b"tok", 0o400)],
            "/etc/cfg",
            false,
        )
        .unwrap();
        assert_eq!(p, root.join("v1"));
        assert_eq!(std::fs::read(root.join("v1/app.conf")).unwrap(), b"x=1");
        assert_eq!(std::fs::metadata(root.join("v1/app.conf")).unwrap().mode() & 0o7777, 0o644);
        assert_eq!(std::fs::metadata(root.join("v1/sub/dir/t")).unwrap().mode() & 0o7777, 0o400);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_single_file_named_like_its_destination_is_bound_as_the_file() {
        let root = tmp("single");
        let p = materialize(&root, "v2", &[f("resolv.conf", b"nameserver 1.1.1.1", 0o644)], "/etc/resolv.conf", false).unwrap();
        assert_eq!(p, root.join("v2/resolv.conf"));
        let q = materialize(&root, "v3", &[f("a", b"1", 0o644)], "/etc/other", false).unwrap();
        assert_eq!(q, root.join("v3"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_planted_link_to_another_file_is_replaced_not_followed() {
        let root = tmp("link-file");
        let victim = root.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::fs::create_dir_all(root.join("v")).unwrap();
        symlink(&victim, root.join("v/token")).unwrap();
        materialize(&root, "v", &[f("token", b"new", 0o600)], "/t", false).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep", "the target of the link is untouched");
        assert_eq!(std::fs::read(root.join("v/token")).unwrap(), b"new");
        assert!(!std::fs::symlink_metadata(root.join("v/token")).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_planted_link_as_a_directory_component_is_an_error() {
        let root = tmp("link-dir");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(root.join("v")).unwrap();
        symlink(&outside, root.join("v/sub")).unwrap();
        let r = materialize(&root, "v", &[f("sub/evil", b"x", 0o644)], "/t", false);
        assert!(r.is_err());
        assert!(!outside.join("evil").exists(), "nothing was written through the link");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_volume_that_is_itself_a_link_is_not_opened() {
        let root = tmp("link-vol");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("v")).unwrap();
        assert!(materialize(&root, "v", &[f("a", b"x", 0o644)], "/t", false).is_err());
        assert!(!outside.join("a").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_volume_in_use_is_left_as_it_is_and_a_free_one_is_rewritten() {
        let root = tmp("use");
        materialize(&root, "v", &[f("a", b"one", 0o644)], "/t", false).unwrap();
        materialize(&root, "v", &[f("a", b"two", 0o644)], "/t", true).unwrap();
        assert_eq!(std::fs::read(root.join("v/a")).unwrap(), b"one", "first writer wins while a container uses it");
        materialize(&root, "v", &[f("a", b"two", 0o644)], "/t", false).unwrap();
        assert_eq!(std::fs::read(root.join("v/a")).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_dir_is_world_writable_and_keeps_what_is_in_it() {
        let root = tmp("empty");
        let p = materialize(&root, "e", &[], "/data", false).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o777);
        std::fs::write(p.join("kept"), b"x").unwrap();
        materialize(&root, "e", &[], "/data", false).unwrap();
        assert!(p.join("kept").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unsafe_path_in_the_files_is_refused() {
        let root = tmp("unsafe");
        assert!(materialize(&root, "v", &[f("../escape", b"x", 0o644)], "/t", false).is_err());
        assert!(materialize(&root, "v", &[f("/abs", b"x", 0o644)], "/t", false).is_err());
        assert!(!root.join("escape").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
