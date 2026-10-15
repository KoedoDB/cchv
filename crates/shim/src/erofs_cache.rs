//! The cache of erofs images of container root file systems.
//!
//! A container's rootfs is turned into an erofs image once and kept (`<key>.erofs`) so that the pods of one image share it. The
//! images are the customers' and are built in RAM-backed `/run`, so the cache has to be safe and bounded:
//!
//! - the directory is root's and not listable (0711: the daemon's rootless Cloud Hypervisors must be able to reach a file they have
//!   an ACL for, but cannot list the names); every file is 0600 from the moment it is created (the build runs with umask 077), and
//!   the lock files too;
//! - a size cap (`max_bytes`) with eviction of the least recently used image nobody is using. "Using" is a shared `flock` on
//!   `<key>.use`, taken by `prepare` and held in the returned [`Lease`] for as long as the sandbox runs (a shim process is one pod's);
//!   eviction takes the exclusive lock without waiting, so it can never remove an image a pod runs from. The `.use` and `.lock` files
//!   are never removed with the image (removing a lock file under a waiter would give two holders two different locks);
//! - one build per image at a time (`<key>.lock`), re-checked after the lock;
//! - the build is `mkfs.erofs` from an absolute path, with a timeout, and its temporary file is removed on every way out;
//! - the free space of the file system is checked before the build, with the size of the rootfs as an estimate.
//!
//! The daemon's warm snapshots refer to an image by path. An image that was evicted and is built again has a new modification time,
//! which the daemon takes as "the image changed" and builds its snapshot again.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use log::{info, warn};

pub struct CacheConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
    /// Where `mkfs.erofs` may be, in order; the first that exists is used.
    pub mkfs_candidates: Vec<PathBuf>,
    pub mkfs_timeout: Duration,
}

/// Proof that the image is in use: a shared lock on `<key>.use`. Dropping it (or the process ending) releases the image.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

/// Free space that must remain besides the image being built.
const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;
/// How long walking a rootfs to estimate its size may take.
const ESTIMATE_DEADLINE: Duration = Duration::from_secs(60);

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The image of `rootfs` under `key`, built if it is not in the cache, and a lease on it.
pub fn prepare(cfg: &CacheConfig, rootfs: &Path, key: &str) -> Result<(PathBuf, Lease), String> {
    if !valid_key(key) {
        return Err(format!("not a cache key: {key:?}"));
    }
    ensure_dir(&cfg.dir)?;
    remove_stale_temporaries(&cfg.dir);

    // The lease comes first: from here on the image cannot be evicted.
    let lease = Lease {
        _file: lock_file(&cfg.dir.join(format!("{key}.use")), libc::LOCK_SH)?,
    };
    let cache_path = cfg.dir.join(format!("{key}.erofs"));
    if cache_path.exists() {
        touch(&cache_path);
        return Ok((cache_path, lease));
    }

    // One build at a time per image; the next one finds the image there.
    let _build = lock_file(&cfg.dir.join(format!("{key}.lock")), libc::LOCK_EX)?;
    if cache_path.exists() {
        touch(&cache_path);
        return Ok((cache_path, lease));
    }

    let estimate = estimate_size(rootfs, cfg.max_bytes)?;
    make_room(cfg, estimate)?;
    check_free_space(&cfg.dir, estimate)?;

    let mkfs = cfg
        .mkfs_candidates
        .iter()
        .find(|p| p.is_absolute() && p.exists())
        .ok_or_else(|| format!("mkfs.erofs not found in {:?}", cfg.mkfs_candidates))?;
    let tmp = cfg.dir.join(format!("{key}.{}.tmp", std::process::id()));
    let guard = Temporary(tmp.clone());
    run_mkfs(mkfs, &tmp, rootfs, cfg.mkfs_timeout)?;
    // 0600 already (umask 077 in the build); say it again, then publish a finished file.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("chmod erofs: {e}"))?;
    std::fs::rename(&tmp, &cache_path).map_err(|e| format!("rename erofs: {e}"))?;
    guard.disarm();
    Ok((cache_path, lease))
}

/// A file that is removed when this is dropped, unless it was published.
struct Temporary(PathBuf);
impl Temporary {
    fn disarm(self) {
        std::mem::forget(self);
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn ensure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create erofs cache dir: {e}"))?;
    // Search but not list for others: a rootless Cloud Hypervisor reaches the one file it has an ACL for.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711)).map_err(|e| format!("chmod erofs cache dir: {e}"))
}

/// Open (create, 0600) a lock file and take the lock; waits.
fn lock_file(path: &Path, op: i32) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
        return Err(format!("flock {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(f)
}

/// Take the exclusive lock without waiting: true when nobody holds the image (the lock is released when the file is dropped).
fn try_exclusive(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .ok()?;
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

/// Mark the image as just used (the order of eviction).
fn touch(path: &Path) {
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        // NULL times: now.
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) };
    }
}

/// Temporaries of builds that died with their shim: the pid in the name is gone and the file is not new.
fn remove_stale_temporaries(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".tmp") else { continue };
        let Some(pid) = stem.rsplit('.').next().and_then(|p| p.parse::<u32>().ok()) else { continue };
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(60));
        if old && !Path::new(&format!("/proc/{pid}")).exists() {
            warn!("removing the leftover of a build that died: {name}");
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ── size ────────────────────────────────────────────────────────────────────

/// About how big the erofs image of `rootfs` will be: the files rounded up to blocks, and the inodes. Stops, with an error, as soon as
/// the sum is over `cap` (an image bigger than the whole cache is refused, and walking an enormous tree does not go on).
fn estimate_size(rootfs: &Path, cap: u64) -> Result<u64, String> {
    let started = Instant::now();
    let mut total: u64 = 0;
    let mut stack = vec![rootfs.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for e in entries {
            let e = e.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let m = e.metadata().map_err(|e| format!("stat {}: {e}", e_path(&dir)))?;
            total += 64 + if m.is_file() { m.len().div_ceil(4096) * 4096 } else { 0 };
            if m.is_dir() {
                stack.push(e.path());
            }
            if total > cap {
                return Err(format!(
                    "the image is larger than the whole erofs cache ({} MiB)",
                    cap / (1024 * 1024)
                ));
            }
            if started.elapsed() > ESTIMATE_DEADLINE {
                return Err("measuring the rootfs took too long".into());
            }
        }
    }
    Ok(total + total / 20)
}

fn e_path(p: &Path) -> String {
    p.display().to_string()
}

fn check_free_space(dir: &Path, need: u64) -> Result<(), String> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| "NUL in the path".to_string())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(format!("statvfs: {}", std::io::Error::last_os_error()));
    }
    let free = s.f_bavail as u64 * s.f_frsize as u64;
    if free < need + MIN_FREE_BYTES {
        return Err(format!(
            "not enough free space for the image: {} MiB free, about {} MiB needed",
            free / (1024 * 1024),
            (need + MIN_FREE_BYTES) / (1024 * 1024)
        ));
    }
    Ok(())
}

// ── eviction ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub size: u64,
    pub modified: SystemTime,
}

/// Which of `entries` to try to evict, in order: the least recently used first. Whether one is in use is only known when its lock is
/// tried, so this is an order, not a verdict.
pub fn eviction_order(entries: &[Entry]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by_key(|&i| entries[i].modified);
    idx
}

/// Whether `need` more bytes fit under `max` with `used` in the cache.
pub fn fits(used: u64, need: u64, max: u64) -> bool {
    used.checked_add(need).is_some_and(|t| t <= max)
}

fn list(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let key = name.strip_suffix(".erofs")?.to_string();
            let m = e.metadata().ok()?;
            Some(Entry {
                key,
                size: m.size(),
                modified: m.modified().ok()?,
            })
        })
        .collect()
}

/// Evict until `need` more bytes fit. An image in use is skipped; if too few can go, the build is refused (no space is made at the
/// cost of a running pod).
fn make_room(cfg: &CacheConfig, need: u64) -> Result<(), String> {
    let entries = list(&cfg.dir);
    let mut used: u64 = entries.iter().map(|e| e.size).sum();
    if fits(used, need, cfg.max_bytes) {
        return Ok(());
    }
    for i in eviction_order(&entries) {
        let e = &entries[i];
        // Held until the file is gone: nobody can take a lease on it in between.
        let Some(_lock) = try_exclusive(&cfg.dir.join(format!("{}.use", e.key))) else {
            continue;
        };
        if std::fs::remove_file(cfg.dir.join(format!("{}.erofs", e.key))).is_ok() {
            info!("erofs cache: evicted {} ({} MiB)", e.key, e.size / (1024 * 1024));
            used = used.saturating_sub(e.size);
        }
        if fits(used, need, cfg.max_bytes) {
            return Ok(());
        }
    }
    Err(format!(
        "the erofs cache is full ({} MiB of {} MiB, the rest in use) and the image needs about {} MiB more",
        used / (1024 * 1024),
        cfg.max_bytes / (1024 * 1024),
        need / (1024 * 1024)
    ))
}

// ── the build ───────────────────────────────────────────────────────────────

fn run_mkfs(mkfs: &Path, out: &Path, rootfs: &Path, timeout: Duration) -> Result<(), String> {
    let mut cmd = std::process::Command::new(mkfs);
    cmd.arg(out)
        .arg(rootfs)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // The file mkfs.erofs creates is the customer's image: 0600 from its first byte.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o077);
            // A process group of its own, so that a timeout kills what mkfs.erofs started too (which would hold the pipe open).
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("mkfs.erofs: {e}"))?;
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(64 * 1024).read_to_end(&mut text);
        }
        String::from_utf8_lossy(&text).to_string()
    });

    let deadline = Instant::now() + timeout;
    // None: the exit status was taken by someone else. The shim's runtime reaps the children of the process (it is a subreaper), and
    // wait() then fails with ECHILD. What mkfs.erofs did is then read from what it left: its messages and the image itself.
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => {
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("mkfs.erofs did not finish in {} s and was killed", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => break None,
            Err(e) => return Err(format!("mkfs.erofs: {e}")),
        }
    };
    let text = reader.join().unwrap_or_default();
    match status {
        Some(s) if !s.success() => Err(format!("mkfs.erofs failed ({s}): {}", text.trim())),
        Some(_) => Ok(()),
        None if text.trim().is_empty() && has_erofs_superblock(out) => Ok(()),
        None => Err(format!(
            "mkfs.erofs ended without a status that could be read, and left no valid image: {}",
            text.trim()
        )),
    }
}

/// The erofs superblock sits at offset 1024 and starts with the magic 0xE0F5E1E2 (little endian).
fn has_erofs_superblock(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut magic = [0u8; 4];
    f.seek(SeekFrom::Start(1024)).is_ok()
        && f.read_exact(&mut magic).is_ok()
        && u32::from_le_bytes(magic) == 0xE0F5_E1E2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_magic_is_recognised() {
        let p = std::env::temp_dir().join(format!("cloudhv-sb-{}", std::process::id()));
        let mut img = vec![0u8; 2048];
        img[1024..1028].copy_from_slice(&0xE0F5_E1E2u32.to_le_bytes());
        std::fs::write(&p, &img).unwrap();
        assert!(has_erofs_superblock(&p));
        std::fs::write(&p, vec![0u8; 2048]).unwrap();
        assert!(!has_erofs_superblock(&p));
        assert!(!has_erofs_superblock(Path::new("/nonexistent/x")));
        let _ = std::fs::remove_file(&p);
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("cloudhv-erofs-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn rootfs(dir: &Path, bytes: usize) -> PathBuf {
        let r = dir.join("rootfs");
        std::fs::create_dir_all(r.join("etc")).unwrap();
        std::fs::write(r.join("etc/f"), vec![1u8; bytes]).unwrap();
        r
    }

    fn cfg(t: &Tmp, mkfs: PathBuf, max: u64) -> CacheConfig {
        CacheConfig {
            dir: t.0.join("cache"),
            max_bytes: max,
            mkfs_candidates: vec![PathBuf::from("/nonexistent/mkfs.erofs"), mkfs],
            mkfs_timeout: Duration::from_secs(5),
        }
    }

    const OK_MKFS: &str = r#"head -c 100000 /dev/zero > "$1""#;

    #[test]
    fn keys_are_lowercase_hex_only() {
        assert!(valid_key("0123abcdef"));
        for bad in ["", "../x", "a/b", "A1", "x y", &"a".repeat(65)] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_image_is_built_once_private_and_published_whole() {
        let t = Tmp::new("build");
        let mkfs = script(&t.0, "mkfs", OK_MKFS);
        let c = cfg(&t, mkfs, 10 << 20);
        let r = rootfs(&t.0, 1000);
        let (p, _lease) = prepare(&c, &r, "abc123").unwrap();
        assert_eq!(p, c.dir.join("abc123.erofs"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600, "the image is root's alone");
        assert_eq!(mode(&c.dir), 0o711, "the directory can be searched but not listed");
        assert_eq!(mode(&c.dir.join("abc123.use")), 0o600);
        assert_eq!(mode(&c.dir.join("abc123.lock")), 0o600);
        // The second time it is the same file, with no build (a failing mkfs would show).
        let c2 = cfg(&t, script(&t.0, "bad", "exit 1"), 10 << 20);
        assert_eq!(prepare(&c2, &r, "abc123").unwrap().0, p);
        // No temporary is left.
        assert!(std::fs::read_dir(&c.dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn a_failed_or_hung_build_leaves_no_temporary_and_a_hung_one_is_killed() {
        let t = Tmp::new("fail");
        let r = rootfs(&t.0, 10);
        let leftover = |c: &CacheConfig| {
            std::fs::read_dir(&c.dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp") || e.file_name().to_string_lossy().ends_with(".erofs"))
        };
        // Writes part of the file, then fails.
        let c = cfg(&t, script(&t.0, "half", r#"head -c 5000 /dev/zero > "$1"; echo boom >&2; exit 3"#), 10 << 20);
        let e = prepare(&c, &r, "aa").unwrap_err();
        assert!(e.contains("boom"), "{e}");
        assert!(!leftover(&c));
        // Hangs.
        let mut c = cfg(&t, script(&t.0, "hang", r#"head -c 5000 /dev/zero > "$1"; sleep 30"#), 10 << 20);
        c.mkfs_timeout = Duration::from_millis(300);
        let started = Instant::now();
        let e = prepare(&c, &r, "bb").unwrap_err();
        assert!(e.contains("killed"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!leftover(&c));
        // No mkfs at all.
        let mut c = cfg(&t, PathBuf::from("/nonexistent/x"), 10 << 20);
        c.mkfs_candidates = vec![PathBuf::from("relative/mkfs"), PathBuf::from("/nonexistent/y")];
        assert!(prepare(&c, &r, "cc").unwrap_err().contains("not found"));
    }

    #[test]
    fn an_image_bigger_than_the_cache_is_refused_before_the_build() {
        let t = Tmp::new("big");
        let c = cfg(&t, script(&t.0, "mkfs", OK_MKFS), 1 << 20);
        let r = rootfs(&t.0, 3 << 20);
        let e = prepare(&c, &r, "dd").unwrap_err();
        assert!(e.contains("larger than the whole erofs cache"), "{e}");
    }

    #[test]
    fn the_least_recently_used_unused_image_is_evicted_and_one_in_use_is_not() {
        let t = Tmp::new("evict");
        // Each image is 100 KB (the fake mkfs) and so is the estimate for the 100 000 byte rootfs; the cache holds 250 KB.
        let c = cfg(&t, script(&t.0, "mkfs", OK_MKFS), 250_000);
        let r = rootfs(&t.0, 100_000);
        let (a, lease_a) = prepare(&c, &r, "a1").unwrap(); // kept while its lease is held
        std::thread::sleep(Duration::from_millis(20));
        let (b, lease_b) = prepare(&c, &r, "b1").unwrap();
        drop(lease_b); // b is unused now
        std::thread::sleep(Duration::from_millis(20));
        // A third does not fit: b goes. a is older but in use.
        let (cpath, lease_c) = prepare(&c, &r, "c1").unwrap();
        assert!(a.exists(), "an image in use is never evicted");
        assert!(!b.exists(), "the unused one is evicted");
        assert!(cpath.exists());
        // With everything in use there is no room, and the build is refused without removing anything.
        let e = prepare(&c, &r, "d1").unwrap_err();
        assert!(e.contains("cache is full"), "{e}");
        assert!(a.exists() && cpath.exists());
        // a is unused now, and the oldest: it goes for the new one; c stays.
        drop(lease_a);
        let (d, _lease_d) = prepare(&c, &r, "d1").unwrap();
        assert!(!a.exists() && d.exists() && cpath.exists());
        drop(lease_c);
    }

    #[test]
    fn the_order_of_eviction_is_oldest_first() {
        let now = SystemTime::now();
        let e = |k: &str, ago: u64| Entry {
            key: k.into(),
            size: 1,
            modified: now - Duration::from_secs(ago),
        };
        let entries = vec![e("new", 1), e("old", 100), e("mid", 50)];
        let order: Vec<&str> = eviction_order(&entries).iter().map(|&i| entries[i].key.as_str()).collect();
        assert_eq!(order, vec!["old", "mid", "new"]);
        assert!(fits(10, 5, 15));
        assert!(!fits(10, 6, 15));
        assert!(!fits(u64::MAX, 1, u64::MAX));
    }

    #[test]
    fn leftovers_of_dead_builds_are_removed_but_a_live_ones_is_not() {
        let t = Tmp::new("stale");
        std::fs::create_dir_all(&t.0).unwrap();
        let dead = t.0.join("k.4000000000.tmp");
        let live = t.0.join(format!("k.{}.tmp", std::process::id()));
        for p in [&dead, &live] {
            std::fs::write(p, b"x").unwrap();
            let old = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            let times = [
                libc::timespec { tv_sec: 1_000_000, tv_nsec: 0 },
                libc::timespec { tv_sec: 1_000_000, tv_nsec: 0 },
            ];
            unsafe { libc::utimensat(libc::AT_FDCWD, old.as_ptr(), times.as_ptr(), 0) };
        }
        remove_stale_temporaries(&t.0);
        assert!(!dead.exists());
        assert!(live.exists());
    }
}
