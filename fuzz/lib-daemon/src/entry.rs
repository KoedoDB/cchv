//! Entry points of the daemon's fuzz targets (see ../../lib-agent/src/entry.rs for the idea).

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Once;

use arbitrary::Arbitrary;
use serde_json::Value;

use crate::config::DaemonConfig;
use crate::validate::{self, SnapshotSize};
use crate::{api, ctl, fsutil, rootless};

fn config() -> DaemonConfig {
    serde_json::from_str(r#"{"kernel_path":"/k","rootfs_path":"/r"}"#).unwrap()
}

// ── the checks of a request's fields ───────────────────────────────────────────────────────────────────────────────────────

const SIZE: SnapshotSize = SnapshotSize { memory_bytes: 128 << 20, vcpus: 1 };
const CACHE: &str = "/run/cloudhv/erofs-cache";

fn components_clean(p: &str) -> bool {
    !p.split('/').any(|c| c == ".." || c == ".")
}

/// Text → the validators of identifiers and paths.
pub fn validate(data: &[u8]) {
    let s = String::from_utf8_lossy(data);
    let s = s.as_ref();

    if validate::is_id(s) {
        assert!((1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'), "id {s:?}");
    }
    assert_eq!(validate::is_id(s), validate::check_id("x", s).is_ok(), "check_id and is_id differ on {s:?}");

    // (The kernel reads `//run/netns/x` and `/run//netns/x` as `/run/netns/x`; so does `Path`.)
    // (`Path` also drops a "." in the middle: `/var/run/./netns/x` is the same file.)
    let collapsed = {
        let mut out = String::new();
        for c in s.chars() {
            if !(c == '/' && out.ends_with('/')) {
                out.push(c);
            }
        }
        while out.contains("/./") {
            out = out.replace("/./", "/");
        }
        // (and a trailing "/" or "/.": `x.erofs/.` is `x.erofs`; opening it as a file fails)
        while out.len() > 1 && (out.ends_with('/') || out.ends_with("/.")) {
            out.truncate(out.len() - if out.ends_with("/.") { 2 } else { 1 });
        }
        out
    };
    for allow_proc in [false, true] {
        let allowed = validate::netns_path_allowed_with(s, allow_proc);
        let s = collapsed.as_str();
        if allowed {
            assert!(!s.contains('\n') && !s.contains('\0') && s.starts_with('/') && components_clean(s), "netns {s:?}");
            let var = s.strip_prefix("/var/run/netns/").or_else(|| s.strip_prefix("/run/netns/"));
            match var {
                // (A trailing "/" is not a part of the path: `/var/run/netns/x/` is `/var/run/netns/x`, and opening it as a file fails.)
                Some(name) => {
                    let name = name.trim_end_matches('/');
                    assert!(!name.is_empty() && !name.contains('/'), "netns name in {s:?}")
                }
                None => {
                    assert!(allow_proc, "a /proc path allowed with allow_proc_netns off: {s:?}");
                    let rest = s.strip_prefix("/proc/").expect("a /proc path").trim_end_matches('/');
                    let parts: Vec<&str> = rest.split('/').collect();
                    assert!(
                        parts.len() == 3 && !parts[0].is_empty() && parts[0].bytes().all(|b| b.is_ascii_digit()) && parts[1] == "ns" && parts[2] == "net",
                        "proc netns {s:?}"
                    );
                }
            }
        }
    }

    if validate::erofs_path_allowed(s, CACHE) {
        let rest = collapsed.strip_prefix(&format!("{CACHE}/")).unwrap_or_else(|| panic!("erofs path {s:?} is not in the cache"));
        assert!(!rest.contains('/') && rest.ends_with(".erofs") && rest.len() > ".erofs".len(), "erofs file {rest:?}");
        assert!(!s.contains('\n') && !s.contains('\0') && components_clean(s));
    }

    // Sizes.
    let n = data.iter().take(16).fold(0u128, |a, b| a << 8 | *b as u128);
    let c = config();
    let (mem, cpus) = ((n >> 64) as u64, n as u64);
    if validate::check_size(&c, mem, cpus).is_ok() {
        assert!(mem <= c.max_memory_mb && cpus <= c.max_vcpus as u64);
        assert!(mem.checked_mul(1 << 20).is_some(), "the byte count of {mem} MiB overflows");
    }

    // A snapshot's configuration.
    if let Ok(v) = serde_json::from_slice::<Value>(data) {
        if validate::validate_snapshot_config(&v, "/k", "/r", Some("/cache/x.erofs"), SIZE).is_ok() {
            assert_snapshot_config_safe(&v);
        }
        let _ = validate::validate_snapshot_config(&v, "/k", "/r", None, SIZE);
    }
}

fn assert_snapshot_config_safe(v: &Value) {
    for k in ["fs", "pmem", "devices", "user_devices", "vdpa", "generic_vhost_user", "net", "gdb", "ivshmem", "numa", "iommu"] {
        let x = v.get(k);
        let unset = match x {
            None | Some(Value::Null) | Some(Value::Bool(false)) => true,
            Some(Value::Array(a)) => a.is_empty(),
            Some(Value::Object(o)) => o.is_empty(),
            _ => false,
        };
        assert!(unset, "an accepted configuration has {k}: {x:?}");
    }
    assert_eq!(v["cpus"]["boot_vcpus"].as_u64(), Some(1), "vcpus");
    assert_eq!(v["cpus"]["max_vcpus"].as_u64().unwrap_or(1), 1, "max vcpus");
    assert_eq!(v["memory"]["size"].as_u64(), Some(128 << 20), "memory");
    assert_eq!(v["payload"]["kernel"].as_str(), Some("/k"));
    for d in v["disks"].as_array().into_iter().flatten() {
        let p = d["path"].as_str().unwrap_or("");
        assert!(p == "/r" || p == "/cache/x.erofs", "disk {p:?}");
        assert_eq!(d["readonly"], Value::Bool(true), "disk {p:?} is writable");
    }
    for key in ["serial", "console"] {
        if let Some(c) = v.get(key).filter(|c| !c.is_null()) {
            let mode = c["mode"].as_str().unwrap_or("Off");
            assert!(matches!(mode, "Off" | "Null" | "File"), "{key}.mode {mode}");
            if key == "console" {
                assert!(matches!(mode, "Off" | "Null"));
            }
        }
    }
    if let Some(d) = v.get("debug_console").filter(|c| !c.is_null()) {
        assert_eq!(d["mode"].as_str().unwrap_or("Off"), "Off");
    }
}

// ── a request line and the helpers of the registry ────────────────────────────────────────────────────────────────────────

/// A JSON request → what the API does with its fields before anything else: the id fields and the volumes.
pub fn request(data: &[u8]) {
    let Ok(v) = serde_json::from_slice::<Value>(data) else { return };
    for key in ["vm_id", "container_id", "exec_id", "image_key"] {
        if let Ok(id) = api::required_id(&v, key) {
            assert!(validate::is_id(id), "{key} {id:?} passed required_id but is not an id");
        }
    }
    let vols = api::volumes_from_request(&v);
    assert!(vols.len() <= 1024);
    for m in &vols {
        for f in &m.files {
            assert!(f.mode <= 0o777, "mode {:o}", f.mode);
        }
    }
}

/// Bytes → the parsers of /proc text and command lines used before a signal is sent to a process.
pub fn ctl(data: &[u8]) {
    let n = data.iter().take(4).fold(0u32, |a, b| a << 8 | *b as u32);
    if ctl::valid_pid(n) {
        assert!(n > 1 && n <= i32::MAX as u32);
    }
    let text = String::from_utf8_lossy(data);
    if ctl::parse_status_uid(&text).is_some() {
        assert!(text.contains("Uid:"));
    }
    if ctl::status_has_uid(&text, n) {
        // A zombie is never reported.
        let zombie = text
            .lines()
            .find_map(|l| l.strip_prefix("State:"))
            .map(|s| s.trim_start().starts_with('Z') || s.trim_start().starts_with('X'))
            .unwrap_or(false);
        assert!(!zombie, "a zombie counted as a process of the user");
    }
    // The command line: it must have `--api-socket <exact socket>` as two arguments, and a first argument.
    let sock = Path::new("/run/cloudhv/d/pool-1-4/api.sock");
    if ctl::cmdline_is_ch_of(data, sock) {
        let args: Vec<&[u8]> = data.split(|b| *b == 0).collect();
        assert!(!args[0].is_empty());
        assert!(args.windows(2).any(|w| w[0] == b"--api-socket" && w[1] == sock.as_os_str().as_encoded_bytes()));
    }
    let p = PathBuf::from(String::from_utf8_lossy(data).to_string());
    let dir = ctl::dir_of(&p);
    // The registry directory of a state directory is its sibling `.ctl/<name>`, never the state directory itself.
    assert_ne!(dir, p);
}

// ── trees a VM made ────────────────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Arbitrary, Debug, Clone)]
pub enum Kind {
    File(Vec<u8>),
    Dir,
    /// A link to the outside file, the outside directory, itself, its parent, a path inside, or a made-up one.
    LinkOutsideFile { relative: bool },
    LinkOutsideDir { relative: bool },
    LinkToItself,
    LinkToParent,
    LinkInside(Vec<u8>),
    LinkRaw(String),
    HardLinkOutside,
    HardLinkInside(u8),
    Fifo,
    Socket,
}

#[derive(Arbitrary, Debug, Clone)]
pub struct Entry {
    /// Path components, each one of a few names so that entries collide and nest.
    pub path: Vec<u8>,
    pub kind: Kind,
}

#[derive(Arbitrary, Debug)]
pub struct TreeCase {
    pub entries: Vec<Entry>,
    pub shared: bool,
    /// Also a chain of directories this deep (a tree a VM made to be as deep as it can).
    pub deep: u16,
    pub then_remove: bool,
}

const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];

pub fn plant(root: &Path, outside: &Path, e: &Entry, made: &mut Vec<PathBuf>) {
    let comps: Vec<&str> = e.path.iter().take(8).map(|b| NAMES[*b as usize % NAMES.len()]).collect();
    if comps.is_empty() {
        return;
    }
    // Walk down without following a link: the test's own set-up must not write outside by accident.
    let mut cur = root.to_path_buf();
    for c in &comps[..comps.len() - 1] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return,
            Err(_) => {
                if std::fs::create_dir(&cur).is_err() {
                    return;
                }
            }
        }
    }
    let path = cur.join(comps[comps.len() - 1]);
    if std::fs::symlink_metadata(&path).is_ok() {
        return;
    }
    let secret = outside.join("secret");
    let ok = match &e.kind {
        Kind::File(c) => std::fs::write(&path, &c[..c.len().min(256)]).is_ok(),
        Kind::Dir => std::fs::create_dir(&path).is_ok(),
        Kind::LinkOutsideFile { relative } => {
            symlink(if *relative { PathBuf::from("../../../outside/secret") } else { secret }, &path).is_ok()
        }
        Kind::LinkOutsideDir { relative } => {
            symlink(if *relative { PathBuf::from("../../../outside") } else { outside.to_path_buf() }, &path).is_ok()
        }
        Kind::LinkToItself => symlink(&path, &path).is_ok(),
        Kind::LinkToParent => symlink("..", &path).is_ok(),
        Kind::LinkInside(to) => {
            let t: PathBuf = to.iter().take(4).map(|b| NAMES[*b as usize % NAMES.len()]).collect();
            symlink(root.join(t), &path).is_ok()
        }
        Kind::LinkRaw(t) => !t.contains('\0') && symlink(t, &path).is_ok(),
        Kind::HardLinkOutside => std::fs::hard_link(&secret, &path).is_ok(),
        Kind::HardLinkInside(i) => made
            .get(*i as usize % made.len().max(1))
            .map(|p| std::fs::hard_link(p, &path).is_ok())
            .unwrap_or(false),
        Kind::Fifo => {
            let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            unsafe { libc::mkfifo(c.as_ptr(), 0o644) == 0 }
        }
        Kind::Socket => std::os::unix::net::UnixListener::bind(&path).is_ok(),
    };
    if ok {
        made.push(path);
    }
}

fn init_rootless() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let cfg: DaemonConfig = serde_json::from_str(
            r#"{"kernel_path":"/etc/passwd","rootfs_path":"/etc/passwd","ch_rootless":true,"ch_gid":1234}"#,
        )
        .unwrap();
        rootless::init(&cfg).expect("rootless init");
    });
}

struct Outside {
    secret: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    dir_mode: u32,
}

fn snapshot_outside(outside: &Path) -> Outside {
    let m = std::fs::metadata(outside.join("secret")).unwrap();
    let d = std::fs::metadata(outside).unwrap();
    Outside { secret: outside.join("secret"), mode: m.mode(), uid: m.uid(), gid: m.gid(), nlink: m.nlink(), dir_mode: d.mode() }
}

fn assert_outside_untouched(outside: &Path, before: &Outside, tree_is_done_with: bool) {
    let m = std::fs::metadata(&before.secret).expect("the outside file is gone");
    assert_eq!(std::fs::read(&before.secret).unwrap(), b"SECRET", "the outside file was written");
    assert_eq!((m.mode(), m.uid(), m.gid()), (before.mode, before.uid, before.gid), "the outside file's owner or mode changed");
    // A link of the file planted in the tree is gone only once the tree was handed over (a plain file with a second link is removed) or
    // removed; a hand-over that failed leaves the tree for its caller to remove.
    if tree_is_done_with {
        assert_eq!(m.nlink(), before.nlink, "the outside file's link count is not what it was (a link of it was left in the tree)");
    }
    let d = std::fs::metadata(outside).unwrap();
    assert_eq!(d.mode(), before.dir_mode, "the outside directory's mode changed");
    let mut names: Vec<_> = std::fs::read_dir(outside).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, vec!["inner", "secret"], "the outside directory changed");
}

fn tree_depth_and_kinds(dir: &Path, depth: usize, owner: (u32, u32), shared: bool) {
    assert!(depth <= 5, "a tree deeper than the limit was kept");
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let m = std::fs::symlink_metadata(e.path()).unwrap();
        assert_eq!((m.uid(), m.gid()), owner, "{:?} was not handed over", e.path());
        if m.is_dir() {
            assert_eq!(m.mode() & 0o7777, if shared { 0o750 } else { 0o700 }, "dir mode of {:?}", e.path());
            tree_depth_and_kinds(&e.path(), depth + 1, owner, shared);
        } else if m.is_file() {
            assert_eq!(m.nlink(), 1, "{:?} has a second link and was kept", e.path());
            assert_eq!(m.mode() & 0o7777, if shared { 0o640 } else { 0o600 }, "file mode of {:?}", e.path());
        } else {
            panic!("{:?} is neither a plain file nor a directory and was kept", e.path());
        }
    }
}

/// A tree a compromised VM made → the hand-over of a snapshot, and the removal of a tree.
pub fn tree(case: TreeCase) {
    use std::os::fd::AsRawFd;
    init_rootless();
    let base = tempfile::tempdir().expect("tempdir");
    let outside = base.path().join("outside");
    let root = base.path().join("work").join("snap");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(outside.join("secret"), b"SECRET").unwrap();
    std::fs::write(outside.join("inner"), b"inner").unwrap();
    std::fs::set_permissions(outside.join("secret"), std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = snapshot_outside(&outside);

    let mut made = Vec::new();
    for e in case.entries.iter().take(40) {
        plant(&root, &outside, e, &mut made);
    }
    if case.deep > 0 {
        // A chain of directories, as deep as the path length allows.
        let mut p = root.join("deep");
        for _ in 0..(case.deep % 1800) {
            p.push("d");
        }
        let _ = std::fs::create_dir_all(&p);
    }

    let gid = 1234;
    let owner = if case.shared { (0, gid) } else { (0, 0) };
    let started = std::time::Instant::now();
    let res = rootless::hand_over_snapshot(&root, case.shared);
    assert!(started.elapsed().as_secs() < 10, "the hand-over took {:?}", started.elapsed());
    assert_outside_untouched(&outside, &before, res.is_ok());
    if res.is_ok() {
        let m = std::fs::metadata(&root).unwrap();
        assert_eq!((m.uid(), m.gid()), owner, "the root of the tree was not handed over");
        tree_depth_and_kinds(&root, 0, owner, case.shared);
    }

    if case.then_remove {
        let parent = root.parent().unwrap();
        let dirfd = fsutil::open_nosymlinks(parent, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        let st = fsutil::fstatat_nofollow(dirfd.as_raw_fd(), "snap").unwrap();
        let started = std::time::Instant::now();
        let r = fsutil::remove_entry(dirfd.as_raw_fd(), "snap", &st);
        assert!(started.elapsed().as_secs() < 10, "the removal took {:?}", started.elapsed());
        if r.is_ok() {
            assert!(!root.exists(), "remove_entry said Ok and the tree is still there");
        }
        assert_outside_untouched(&outside, &before, r.is_ok());
    }
}
