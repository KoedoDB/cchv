//! Confining a rootless Cloud Hypervisor.
//!
//! A Cloud Hypervisor that has been taken over from its guest is hostile, and a user of its own with no capabilities still sees the
//! host: its network (localhost services, the management network), its processes, every world-readable file. So each one is started
//! by a launcher, a mode of this same binary (`cloudhv-sandbox-daemon --ch-launcher`), that puts it in:
//!
//! - a **mount namespace** whose root is a new, read-only tmpfs holding only what Cloud Hypervisor needs, at the paths it is given
//!   them at: its binary (and, if that is dynamic, the system libraries), the kernel and the base rootfs (read-only), the directory of
//!   container images (read-only; the files in it are readable only by the VM that was given them), its own state directory and its
//!   own outbox (read-write), `/dev/kvm`, `/dev/null`, `/dev/zero`, `/dev/urandom`, a new `/proc`, and a small `/tmp`;
//! - a **PID namespace**: it sees itself (as process 1) and its own threads, not the host's processes, and when it ends everything in
//!   the namespace ends with it;
//! - a **network namespace with no interface**: the TAP device is not opened by it but handed to it as a descriptor over its API
//!   socket (`vm.add-net` with `SCM_RIGHTS`), which names the device whatever namespace it is in, so it needs no network, and a
//!   compromised one cannot reach localhost, the management network or the pods';
//! - **IPC and UTS namespaces** of its own;
//! - a **cgroup** of its own (`cgroup.rs`) with a memory and a pids limit, which the launcher joins before it starts anything;
//! - **a user and a group of its own**, with only the group of `/dev/kvm` added (to open it, and to read the base snapshot), no
//!   capabilities (the bounding set is emptied), `no_new_privs`, no core files, a limit on its processes.
//!
//! The launcher runs as root until it has built all that. Its process stays as the supervisor (the process the daemon knows, signals,
//! and waits for): it drops to the VM's user, forwards the stop signals to Cloud Hypervisor, and exits with its status. Cloud
//! Hypervisor, the child, is process 1 of the new PID namespace, which is why a launcher (and not just `unshare` in `pre_exec`, whose
//! process stays outside the namespace it makes) is needed. Its own mount namespace is made by the child, not the supervisor, so that
//! the `pivot_root` does not change the supervisor's root.
//!
//! The launcher reads its instructions (a `LauncherSpec`, JSON) from its standard input and is given nothing else; its error messages
//! go to a log the daemon reads when a VM does not start.

use std::ffi::CString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A path made visible in the confined root, at the same path as on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    pub path: String,
    /// Writable (the VM's own directories). Everything else is read-only.
    #[serde(default)]
    pub rw: bool,
    /// A device node: the mount may carry devices. Every other mount is `nodev`.
    #[serde(default)]
    pub dev: bool,
    /// Programs may be run from it (Cloud Hypervisor itself, its libraries). Every other mount is `noexec`.
    #[serde(default)]
    pub exec: bool,
}

/// What the launcher is told, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherSpec {
    /// The program, an absolute path, and its arguments (after the program's name).
    pub binary: String,
    pub args: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    /// The supplementary groups: the group of `/dev/kvm`.
    pub groups: Vec<u32>,
    /// An existing directory (root's, private) the new root's tmpfs is mounted on. Inside the new mount namespace only.
    pub new_root: String,
    pub binds: Vec<Bind>,
    /// The cgroup directory to join first.
    pub cgroup: Option<String>,
    pub max_processes: u64,
    pub hostname: String,
    pub tmp_mib: u64,
}

// ── checking the instructions ───────────────────────────────────────────────

fn clean_abs(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        && path != "/"
        && !path.contains('\0')
        && !path.contains('\n')
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// The launcher does what the spec says as root, so it checks first that the spec is something it can do safely.
pub fn validate_spec(spec: &LauncherSpec) -> Result<(), String> {
    if !clean_abs(&spec.binary) {
        return Err(format!(
            "binary {:?} is not a plain absolute path",
            spec.binary
        ));
    }
    if !clean_abs(&spec.new_root) {
        return Err(format!(
            "new_root {:?} is not a plain absolute path",
            spec.new_root
        ));
    }
    if spec.uid == 0 || spec.gid == 0 || spec.groups.contains(&0) {
        return Err("the user and groups of a confined VM must not be root".into());
    }
    if spec.args.iter().any(|a| a.contains('\0')) {
        return Err("an argument has a NUL".into());
    }
    if spec.binds.is_empty() {
        return Err("no binds".into());
    }
    let mut seen = std::collections::HashSet::new();
    for b in &spec.binds {
        if !clean_abs(&b.path) {
            return Err(format!("bind {:?} is not a plain absolute path", b.path));
        }
        // The new root has its own /proc, and the whole of /tmp is not shared (a place under /tmp may be: then there is no tmpfs of its own).
        if b.path == "/proc" || b.path.starts_with("/proc/") || b.path == "/tmp" {
            return Err(format!(
                "bind {:?} would hide /proc or share the whole /tmp",
                b.path
            ));
        }
        if !seen.insert(b.path.clone()) {
            return Err(format!("bind {:?} twice", b.path));
        }
    }
    if let Some(cg) = &spec.cgroup {
        if !clean_abs(cg) || !cg.starts_with("/sys/fs/cgroup/") {
            return Err(format!(
                "cgroup {cg:?} is not a directory of the cgroup file system"
            ));
        }
    }
    if spec.hostname.is_empty()
        || spec.hostname.len() > 63
        || !spec
            .hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("hostname is not a host name".into());
    }
    Ok(())
}

// ── the mount plan ──────────────────────────────────────────────────────────

/// One thing done in the new mount namespace, in order. Data, so that the plan can be tested without mounting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The new root: a tmpfs on `target`.
    Root { target: String, size_mib: u64 },
    /// `src` (a file, a directory or a device node) bound at `dst`.
    Bind {
        src: String,
        dst: String,
        rw: bool,
        dev: bool,
        exec: bool,
    },
    /// A fresh /proc (of the new PID namespace).
    Proc { dst: String },
    /// A small, private /tmp.
    Tmp { dst: String, size_mib: u64 },
}

/// The steps that build the new root under `spec.new_root`, then the pivot into it (done by `apply`).
pub fn plan(spec: &LauncherSpec) -> Vec<Step> {
    let root = spec.new_root.trim_end_matches('/').to_string();
    let mut steps = vec![Step::Root {
        target: root.clone(),
        size_mib: 4,
    }];
    // Shorter paths first, so that a directory is bound before what is bound inside it.
    let mut binds = spec.binds.clone();
    binds.sort_by_key(|b| (b.path.matches('/').count(), b.path.clone()));
    for b in binds {
        steps.push(Step::Bind {
            dst: format!("{root}{}", b.path),
            src: b.path,
            rw: b.rw,
            dev: b.dev,
            exec: b.exec,
        });
    }
    steps.push(Step::Proc {
        dst: format!("{root}/proc"),
    });
    // A small private /tmp, unless something is bound under /tmp (the places of a test, say): that is a /tmp already.
    if !spec.binds.iter().any(|b| b.path.starts_with("/tmp/")) {
        steps.push(Step::Tmp {
            dst: format!("{root}/tmp"),
            size_mib: spec.tmp_mib.max(1),
        });
    }
    steps
}

/// The mount flags of a bind mount after the remount that restricts it.
pub fn bind_flags(rw: bool, dev: bool, exec: bool) -> libc::c_ulong {
    let mut f = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID;
    if !rw {
        f |= libc::MS_RDONLY;
    }
    if !dev {
        f |= libc::MS_NODEV;
    }
    if !exec {
        f |= libc::MS_NOEXEC;
    }
    f
}

/// Whether an executable is dynamically linked (has a program interpreter, `PT_INTERP`), from its ELF header. Such a binary needs the
/// system libraries in the confined root. Anything that is not a 64-bit little-endian ELF file is taken as static.
pub fn is_dynamic_elf(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut hdr = [0u8; 64];
    if f.read_exact(&mut hdr).is_err() || &hdr[..4] != b"\x7fELF" || hdr[4] != 2 || hdr[5] != 1 {
        return false;
    }
    let phoff = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
    let phentsize = u16::from_le_bytes(hdr[54..56].try_into().unwrap()) as u64;
    let phnum = u16::from_le_bytes(hdr[56..58].try_into().unwrap()) as u64;
    if phentsize < 4 || phnum == 0 || phnum > 256 {
        return false;
    }
    use std::io::{Seek, SeekFrom};
    for i in 0..phnum {
        let mut t = [0u8; 4];
        if f.seek(SeekFrom::Start(phoff + i * phentsize)).is_err() || f.read_exact(&mut t).is_err()
        {
            return false;
        }
        if u32::from_le_bytes(t) == 3 {
            return true; // PT_INTERP
        }
    }
    false
}

/// What the daemon asks the launcher to give a VM's Cloud Hypervisor. `libs` are the library directories, used only if the binary is
/// dynamic. `kvm_gid` is the group of `/dev/kvm`.
#[allow(clippy::too_many_arguments)]
pub fn build_spec(
    binary: &str,
    ch_args: Vec<String>,
    ids: (u32, u32),
    kvm_gid: u32,
    new_root: &Path,
    state_dir: &Path,
    out_dir: &Path,
    kernel: &str,
    rootfs: &str,
    erofs_cache: &str,
    extra_ro: &[String],
    libs: &[&str],
    cgroup: Option<&Path>,
    max_processes: u64,
    vm_name: &str,
    disk_dir: Option<&Path>,
) -> LauncherSpec {
    let ro = |p: &str| Bind {
        path: p.to_string(),
        rw: false,
        dev: false,
        exec: false,
    };
    let mut binds = vec![
        Bind {
            path: binary.to_string(),
            rw: false,
            dev: false,
            exec: true,
        },
        ro(kernel),
        ro(rootfs),
        ro(erofs_cache),
        Bind {
            path: state_dir.display().to_string(),
            rw: true,
            dev: false,
            exec: false,
        },
        Bind {
            path: out_dir.display().to_string(),
            rw: true,
            dev: false,
            exec: false,
        },
    ];
    for dev in ["/dev/kvm", "/dev/null", "/dev/zero", "/dev/urandom"] {
        binds.push(Bind {
            path: dev.to_string(),
            rw: true,
            dev: true,
            exec: false,
        });
    }
    if is_dynamic_elf(Path::new(binary)) {
        for lib in libs {
            if Path::new(lib).exists() {
                binds.push(Bind {
                    path: lib.to_string(),
                    rw: false,
                    dev: false,
                    exec: true,
                });
            }
        }
    }
    for p in extra_ro {
        binds.push(ro(p));
    }
    // The directory of the VM's emptyDir disks: writable (the disk files are written), and not `exec`.
    if let Some(d) = disk_dir {
        binds.push(Bind {
            path: d.display().to_string(),
            rw: true,
            dev: false,
            exec: false,
        });
    }
    // A path asked for twice (the kernel may be inside a directory that is also given) is one bind.
    let mut seen = std::collections::HashSet::new();
    binds.retain(|b| seen.insert(b.path.clone()));
    LauncherSpec {
        binary: binary.to_string(),
        args: ch_args,
        uid: ids.0,
        gid: ids.1,
        groups: vec![kvm_gid],
        new_root: new_root.display().to_string(),
        binds,
        cgroup: cgroup.map(|c| c.display().to_string()),
        max_processes,
        hostname: format!(
            "ch-{}",
            vm_name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(40)
                .collect::<String>()
                .to_lowercase()
        ),
        tmp_mib: 16,
    }
}

// ── the launcher ────────────────────────────────────────────────────────────

/// Exit code of a launcher that could not build the confinement (not a code Cloud Hypervisor uses).
const EXIT_SETUP: i32 = 70;

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("{s:?} has a NUL"))
}

fn check(rc: libc::c_int, what: impl FnOnce() -> String) -> Result<(), String> {
    if rc != 0 {
        Err(format!("{}: {}", what(), last_error()))
    } else {
        Ok(())
    }
}

fn mount(
    src: Option<&str>,
    dst: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), String> {
    let s = src.map(cstr).transpose()?;
    let d = cstr(dst)?;
    let t = fstype.map(cstr).transpose()?;
    let da = data.map(cstr).transpose()?;
    let rc = unsafe {
        libc::mount(
            s.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            d.as_ptr(),
            t.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            da.as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    check(rc, || format!("mount {src:?} on {dst}"))
}

/// Make `dst` the same kind of thing as `src`: a directory, or an empty file to bind a file or device over.
fn make_target(src: &str, dst: &str) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{src}: {e}"))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| format!("create {dst}: {e}"))
    } else {
        if let Some(parent) = Path::new(dst).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dst)
            .map(|_| ())
            .map_err(|e| format!("create {dst}: {e}"))
    }
}

fn apply(steps: &[Step]) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Root { target, size_mib } => {
                mount(
                    Some("tmpfs"),
                    target,
                    Some("tmpfs"),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    Some(&format!("mode=0755,size={size_mib}m")),
                )?;
            }
            Step::Bind {
                src,
                dst,
                rw,
                dev,
                exec,
            } => {
                make_target(src, dst)?;
                mount(Some(src), dst, None, libc::MS_BIND, None)?;
                mount(None, dst, None, bind_flags(*rw, *dev, *exec), None)?;
            }
            Step::Proc { dst } => {
                std::fs::create_dir_all(dst).map_err(|e| format!("create {dst}: {e}"))?;
                mount(
                    Some("proc"),
                    dst,
                    Some("proc"),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    None,
                )?;
            }
            Step::Tmp { dst, size_mib } => {
                std::fs::create_dir_all(dst).map_err(|e| format!("create {dst}: {e}"))?;
                mount(
                    Some("tmpfs"),
                    dst,
                    Some("tmpfs"),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    Some(&format!("mode=1777,size={size_mib}m")),
                )?;
            }
        }
    }
    Ok(())
}

/// Go into the new root and let the old one go: `pivot_root(".", ".")` with the old root then unmounted from under itself.
fn pivot(new_root: &str) -> Result<(), String> {
    let r = cstr(new_root)?;
    check(unsafe { libc::chdir(r.as_ptr()) }, || {
        format!("chdir {new_root}")
    })?;
    let dot = cstr(".")?;
    check(
        unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) as libc::c_int },
        || "pivot_root".to_string(),
    )?;
    check(
        unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) },
        || "umount the old root".to_string(),
    )?;
    let slash = cstr("/")?;
    check(unsafe { libc::chdir(slash.as_ptr()) }, || {
        "chdir /".to_string()
    })?;
    // The root itself is read-only (the VM's own directories and /tmp are separate mounts).
    mount(
        None,
        "/",
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )
}

/// No capability can be had again: the bounding set is emptied and the ambient set cleared.
fn drop_all_capabilities() {
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0,
            0,
            0,
        );
        for cap in 0..=63 {
            if libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) != 0
                && last_error().raw_os_error() == Some(libc::EINVAL)
            {
                break; // past the last capability this kernel has
            }
        }
    }
}

/// Become the VM's user: groups, group, user, in that order, so that each step is still allowed. Then `no_new_privs`.
fn drop_to(uid: u32, gid: u32, groups: &[u32]) -> Result<(), String> {
    let g: Vec<libc::gid_t> = groups.to_vec();
    check(unsafe { libc::setgroups(g.len(), g.as_ptr()) }, || {
        "setgroups".to_string()
    })?;
    check(unsafe { libc::setgid(gid) }, || "setgid".to_string())?;
    check(unsafe { libc::setuid(uid) }, || "setuid".to_string())?;
    check(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        || "no_new_privs".to_string(),
    )
}

fn set_rlimit(resource: u32, value: u64) -> Result<(), String> {
    let l = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    check(unsafe { libc::setrlimit(resource as _, &l) }, || {
        format!("setrlimit {resource}")
    })
}

fn fail(msg: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "ch-launcher: {msg}");
    std::process::exit(EXIT_SETUP);
}

/// The process of Cloud Hypervisor, in the new PID namespace (this is process 1 there). Never returns.
fn child(spec: &LauncherSpec, log_fd: libc::c_int, err_fd: libc::c_int) -> ! {
    let steps = plan(spec);
    let result = (|| -> Result<(), String> {
        check(
            unsafe {
                libc::unshare(
                    libc::CLONE_NEWNS
                        | libc::CLONE_NEWNET
                        | libc::CLONE_NEWIPC
                        | libc::CLONE_NEWUTS,
                )
            },
            || "unshare".to_string(),
        )?;
        // What happens to the mounts here must not reach the host's: the whole tree is private to this namespace.
        mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
        unsafe { libc::umask(0o022) };
        let h = cstr(&spec.hostname)?;
        check(
            unsafe { libc::sethostname(h.as_ptr(), spec.hostname.len()) },
            || "sethostname".to_string(),
        )?;
        apply(&steps)?;
        pivot(&spec.new_root)?;
        drop_all_capabilities();
        set_rlimit(libc::RLIMIT_CORE as u32, 0)?;
        set_rlimit(libc::RLIMIT_NPROC as u32, spec.max_processes)?;
        drop_to(spec.uid, spec.gid, &spec.groups)?;
        // Set only now: a change of user clears the parent-death signal. If the supervisor goes (it is killed), so does Cloud Hypervisor.
        check(
            unsafe {
                libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                )
            },
            || "pdeathsig".to_string(),
        )?;
        // Files made from here on are the VM's own.
        unsafe { libc::umask(0o077) };
        Ok(())
    })();
    if let Err(e) = result {
        fail(&e);
    }

    // Input and output are /dev/null; what Cloud Hypervisor says on standard error goes to the supervisor (a pipe), which keeps the first
    // bit of it in the log. The log descriptor stays (close-on-exec) to report if the exec fails.
    unsafe {
        let null = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            if null > 2 {
                libc::close(null);
            }
        }
        libc::dup2(err_fd, 2);
        let flags = libc::fcntl(log_fd, libc::F_GETFD);
        libc::fcntl(log_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    let bin = match cstr(&spec.binary) {
        Ok(b) => b,
        Err(e) => fail(&e),
    };
    let mut argv: Vec<CString> = vec![bin.clone()];
    for a in &spec.args {
        match cstr(a) {
            Ok(c) => argv.push(c),
            Err(e) => fail(&e),
        }
    }
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // A clean environment: Cloud Hypervisor needs none.
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    unsafe { libc::execve(bin.as_ptr(), ptrs.as_ptr(), envp.as_ptr()) };
    let msg = format!("ch-launcher: exec {}: {}\n", spec.binary, last_error());
    unsafe { libc::write(log_fd, msg.as_ptr() as *const libc::c_void, msg.len()) };
    unsafe { libc::_exit(EXIT_SETUP) }
}

/// The most of Cloud Hypervisor's standard error that is kept in the log.
const LOG_LIMIT: usize = 64 * 1024;

/// What the supervisor does until Cloud Hypervisor ends, in one thread (a process that has made a PID namespace cannot make threads):
/// the stop signals it is sent are forwarded to Cloud Hypervisor; what Cloud Hypervisor writes on standard error is copied into the log,
/// up to `LOG_LIMIT` bytes, and the rest is read and thrown away (a compromised one must not fill the disk or memory through it, nor
/// block on a full pipe); and when Cloud Hypervisor ends, so does the supervisor, with its status. Signals are taken with a signalfd,
/// so there is no signal handler.
fn supervise(child: libc::pid_t, err_rd: libc::c_int, log_fd: libc::c_int) -> ! {
    let forwarded = [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGCHLD,
    ];
    let sfd = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in forwarded {
            libc::sigaddset(&mut set, sig);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
    };
    if sfd < 0 {
        unsafe { libc::kill(child, libc::SIGKILL) };
        fail(&format!("signalfd: {}", last_error()));
    }
    unsafe {
        let fl = libc::fcntl(err_rd, libc::F_GETFL);
        libc::fcntl(err_rd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut pipe_open = true;
    let mut kept = 0usize;
    let mut buf = [0u8; 4096];
    let mut status: Option<libc::c_int> = None;
    let mut drain = |pipe_open: &mut bool, kept: &mut usize| loop {
        let n = unsafe { libc::read(err_rd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n == 0 {
            *pipe_open = false;
            return;
        }
        if n < 0 {
            if last_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return; // EAGAIN: nothing more now
        }
        let take = (n as usize).min(LOG_LIMIT.saturating_sub(*kept));
        if take > 0 {
            unsafe { libc::write(log_fd, buf.as_ptr() as *const libc::c_void, take) };
            *kept += take;
        }
    };
    loop {
        let mut fds = [
            libc::pollfd {
                fd: sfd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if pipe_open { err_rd } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A timeout, because the end of Cloud Hypervisor is also looked for without waiting for the signal about it.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 500) };
        if pipe_open {
            drain(&mut pipe_open, &mut kept);
        }
        // The signals: a stop signal goes on to Cloud Hypervisor; SIGCHLD is only a prompt to look.
        loop {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::read(
                    sfd,
                    &mut info as *mut _ as *mut libc::c_void,
                    std::mem::size_of::<libc::signalfd_siginfo>(),
                )
            };
            if n != std::mem::size_of::<libc::signalfd_siginfo>() as isize {
                break;
            }
            if info.ssi_signo as i32 != libc::SIGCHLD {
                unsafe { libc::kill(child, info.ssi_signo as i32) };
            }
        }
        let mut st: libc::c_int = 0;
        if unsafe { libc::waitpid(child, &mut st, libc::WNOHANG) } == child {
            status = Some(st);
        }
        if let Some(st) = status {
            if pipe_open {
                drain(&mut pipe_open, &mut kept);
            }
            if libc::WIFEXITED(st) {
                std::process::exit(libc::WEXITSTATUS(st));
            }
            // Ended by a signal: the same, as a shell reports it.
            std::process::exit(128 + libc::WTERMSIG(st));
        }
    }
}

/// The entry point of `cloudhv-sandbox-daemon --ch-launcher`. Never returns.
pub fn launcher_main() -> ! {
    // The instructions, from standard input (the daemon's pipe), at most 1 MiB.
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin()
        .lock()
        .take(1 << 20)
        .read_to_end(&mut input)
    {
        fail(&format!("read the instructions: {e}"));
    }
    let spec: LauncherSpec = match serde_json::from_slice(&input) {
        Ok(s) => s,
        Err(e) => fail(&format!("the instructions are not valid: {e}")),
    };
    if let Err(e) = validate_spec(&spec) {
        fail(&format!("refusing the instructions: {e}"));
    }
    // Standard input is no longer the pipe; standard output goes nowhere. The log stays on standard error until the exec.
    let log_fd = unsafe { libc::dup(2) };
    unsafe {
        let null = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            if null > 2 {
                libc::close(null);
            }
        }
    }

    // The cgroup first, as root: this process and everything it makes is in it.
    if let Some(cg) = &spec.cgroup {
        if let Err(e) = std::fs::write(Path::new(cg).join("cgroup.procs"), "0") {
            fail(&format!("join the cgroup {cg}: {e}"));
        }
    }
    if let Err(e) = check(unsafe { libc::unshare(libc::CLONE_NEWPID) }, || {
        "unshare the PID namespace".to_string()
    }) {
        fail(&e);
    }
    let mut pipe_fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        fail(&format!("pipe: {}", last_error()));
    }
    match unsafe { libc::fork() } {
        -1 => fail(&format!("fork: {}", last_error())),
        0 => child(&spec, log_fd, pipe_fds[1]),
        pid => {
            // The supervisor. It has nothing left to do as root.
            unsafe { libc::close(pipe_fds[1]) };
            drop_all_capabilities();
            if let Err(e) = drop_to(spec.uid, spec.gid, &spec.groups) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                fail(&e);
            }
            supervise(pid, pipe_fds[0], log_fd)
        }
    }
}

/// The library directories added to the confined root when Cloud Hypervisor is a dynamic binary.
pub const SYSTEM_LIB_DIRS: [&str; 3] = ["/lib", "/lib64", "/usr/lib"];

/// A path the instructions and `out_dir_of` make: `<state root>/.out/<vm>`.
pub fn out_dir_of(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join(OUT_DIR)
        .join(state_dir.file_name().unwrap_or_default())
}

/// The name of the directory in the state directory that holds each VM's outbox (where a confined VM writes a snapshot).
pub const OUT_DIR: &str = ".out";

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LauncherSpec {
        build_spec(
            "/usr/local/bin/cloud-hypervisor",
            vec![
                "--api-socket".into(),
                "/run/cloudhv/d/pool-1-4/api.sock".into(),
            ],
            (200_001, 200_001),
            993,
            Path::new("/run/cloudhv/d/.ctl/pool-1-4/root"),
            Path::new("/run/cloudhv/d/pool-1-4"),
            Path::new("/run/cloudhv/d/.out/pool-1-4"),
            "/opt/cloudhv/vmlinux",
            "/opt/cloudhv/rootfs.erofs",
            "/run/cloudhv/erofs-cache",
            &["/opt/cloudhv/extra".to_string()],
            &SYSTEM_LIB_DIRS,
            Some(Path::new("/sys/fs/cgroup/cloudhv/pool-1-4")),
            1024,
            "pool-1-4",
            Some(Path::new("/var/lib/cloudhv/emptydir/pool-1-4")),
        )
    }

    #[test]
    fn the_spec_a_vm_gets_is_valid_and_has_what_it_needs() {
        let s = spec();
        validate_spec(&s).unwrap();
        let paths: Vec<&str> = s.binds.iter().map(|b| b.path.as_str()).collect();
        for needed in [
            "/usr/local/bin/cloud-hypervisor",
            "/opt/cloudhv/vmlinux",
            "/opt/cloudhv/rootfs.erofs",
            "/run/cloudhv/erofs-cache",
            "/run/cloudhv/d/pool-1-4",
            "/run/cloudhv/d/.out/pool-1-4",
            "/dev/kvm",
            "/dev/null",
            "/dev/urandom",
            "/opt/cloudhv/extra",
            "/var/lib/cloudhv/emptydir/pool-1-4",
        ] {
            assert!(paths.contains(&needed), "{needed} is bound");
        }
        // Only the VM's own directory of disks, not the directory of the others'.
        assert!(!paths.contains(&"/var/lib/cloudhv/emptydir"));
        // Nothing else of the host: not the state root (other VMs' directories), the registry, the daemon's socket, the snapshots.
        assert!(!paths.contains(&"/run/cloudhv/d"));
        assert!(!paths.iter().any(|p| p.contains(".ctl")));
        assert!(!paths.iter().any(|p| p.contains("snapshot")));
        assert_eq!(s.groups, vec![993]);
        assert_eq!(s.hostname, "ch-pool14");
    }

    #[test]
    fn only_a_vms_own_directories_are_writable_and_only_device_nodes_carry_devices() {
        let s = spec();
        for b in &s.binds {
            let own = b.path == "/run/cloudhv/d/pool-1-4"
                || b.path == "/run/cloudhv/d/.out/pool-1-4"
                || b.path == "/var/lib/cloudhv/emptydir/pool-1-4";
            let dev = b.path.starts_with("/dev/");
            assert_eq!(b.rw, own || dev, "{} writable", b.path);
            assert_eq!(b.dev, dev, "{} carries devices", b.path);
            assert_eq!(
                b.exec,
                b.path == "/usr/local/bin/cloud-hypervisor",
                "{} runs programs",
                b.path
            );
        }
    }

    #[test]
    fn a_vm_without_emptydir_disks_has_no_directory_for_them() {
        let s = build_spec(
            "/usr/local/bin/cloud-hypervisor",
            vec![],
            (200_001, 200_001),
            993,
            Path::new("/r/root"),
            Path::new("/run/cloudhv/d/pool-1-4"),
            Path::new("/run/cloudhv/d/.out/pool-1-4"),
            "/k",
            "/rootfs",
            "/cache",
            &[],
            &SYSTEM_LIB_DIRS,
            None,
            1024,
            "pool-1-4",
            None,
        );
        assert!(!s.binds.iter().any(|b| b.path.contains("emptydir")));
    }

    #[test]
    fn the_plan_builds_the_root_first_then_binds_parents_before_children_then_proc_and_tmp() {
        let mut s = spec();
        s.binds.push(Bind {
            path: "/opt/cloudhv".into(),
            rw: false,
            dev: false,
            exec: false,
        });
        let steps = plan(&s);
        assert!(
            matches!(&steps[0], Step::Root { target, .. } if target == "/run/cloudhv/d/.ctl/pool-1-4/root")
        );
        let dsts: Vec<String> = steps
            .iter()
            .filter_map(|st| match st {
                Step::Bind { dst, .. } => Some(dst.clone()),
                _ => None,
            })
            .collect();
        let root = "/run/cloudhv/d/.ctl/pool-1-4/root";
        assert!(
            dsts.iter().all(|d| d.starts_with(root)),
            "everything is bound inside the new root"
        );
        let a = dsts
            .iter()
            .position(|d| d == &format!("{root}/opt/cloudhv"))
            .unwrap();
        let b = dsts
            .iter()
            .position(|d| d == &format!("{root}/opt/cloudhv/vmlinux"))
            .unwrap();
        assert!(a < b, "the directory before what is in it");
        assert!(matches!(steps[steps.len() - 2], Step::Proc { .. }));
        assert!(matches!(steps[steps.len() - 1], Step::Tmp { .. }));
    }

    #[test]
    fn the_restricting_flags() {
        let ro = bind_flags(false, false, false);
        assert!(
            ro & libc::MS_RDONLY != 0
                && ro & libc::MS_NODEV != 0
                && ro & libc::MS_NOEXEC != 0
                && ro & libc::MS_NOSUID != 0
        );
        let kvm = bind_flags(true, true, false);
        assert!(
            kvm & libc::MS_RDONLY == 0 && kvm & libc::MS_NODEV == 0 && kvm & libc::MS_NOEXEC != 0
        );
        let exe = bind_flags(false, false, true);
        assert!(exe & libc::MS_NOEXEC == 0 && exe & libc::MS_RDONLY != 0);
        assert!(exe & libc::MS_REMOUNT != 0 && exe & libc::MS_BIND != 0);
    }

    #[test]
    fn instructions_that_could_hurt_the_host_are_refused() {
        let ok = spec();
        let mut s = ok.clone();
        s.uid = 0;
        assert!(validate_spec(&s).is_err(), "root");
        let mut s = ok.clone();
        s.groups = vec![0];
        assert!(validate_spec(&s).is_err(), "root group");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "/run/../etc".into(),
            rw: true,
            dev: false,
            exec: false,
        });
        assert!(validate_spec(&s).is_err(), "..");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "relative".into(),
            rw: false,
            dev: false,
            exec: false,
        });
        assert!(validate_spec(&s).is_err(), "relative");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "/proc/sys".into(),
            rw: true,
            dev: false,
            exec: false,
        });
        assert!(validate_spec(&s).is_err(), "over /proc");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "/tmp".into(),
            rw: true,
            dev: false,
            exec: false,
        });
        assert!(validate_spec(&s).is_err(), "the whole /tmp");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "/tmp/x".into(),
            rw: false,
            dev: false,
            exec: false,
        });
        validate_spec(&s).unwrap();
        assert!(
            !plan(&s).iter().any(|st| matches!(st, Step::Tmp { .. })),
            "no tmpfs over what is bound under /tmp"
        );
        let mut s = ok.clone();
        s.binds.push(s.binds[0].clone());
        assert!(validate_spec(&s).is_err(), "twice");
        let mut s = ok.clone();
        s.binds.push(Bind {
            path: "/".into(),
            rw: true,
            dev: false,
            exec: false,
        });
        assert!(validate_spec(&s).is_err(), "the whole root");
        let mut s = ok.clone();
        s.cgroup = Some("/etc/cgroup".into());
        assert!(
            validate_spec(&s).is_err(),
            "a cgroup outside the cgroup file system"
        );
        let mut s = ok.clone();
        s.cgroup = Some("/sys/fs/cgroup/../../etc".into());
        assert!(validate_spec(&s).is_err(), "a cgroup with ..");
        let mut s = ok.clone();
        s.binary = "cloud-hypervisor".into();
        assert!(
            validate_spec(&s).is_err(),
            "a program that is not an absolute path"
        );
        let mut s = ok.clone();
        s.hostname = "a b".into();
        assert!(validate_spec(&s).is_err(), "host name");
        let mut s = ok;
        s.binds.clear();
        assert!(validate_spec(&s).is_err(), "no binds");
    }

    #[test]
    fn the_spec_survives_json() {
        let s = spec();
        let back: LauncherSpec = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn a_dynamic_binary_is_told_from_a_static_one() {
        // This test binary is a dynamic ELF on glibc and a static one on musl; both are ELF. A text file and a missing one are neither.
        let me = std::env::current_exe().unwrap();
        let _ = is_dynamic_elf(&me); // must not panic
        assert!(!is_dynamic_elf(Path::new("/nonexistent/x")));
        let dir = std::env::temp_dir().join(format!("cloudhv-elf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("text");
        std::fs::write(&text, b"#!/bin/sh\necho\n").unwrap();
        assert!(!is_dynamic_elf(&text));
        // a minimal ELF64 header with two program headers: PT_LOAD, then PT_INTERP
        let mut elf = vec![0u8; 64 + 2 * 56];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&2u16.to_le_bytes());
        elf[64..68].copy_from_slice(&1u32.to_le_bytes());
        elf[64 + 56..64 + 56 + 4].copy_from_slice(&3u32.to_le_bytes());
        let dynamic = dir.join("dyn");
        std::fs::write(&dynamic, &elf).unwrap();
        assert!(is_dynamic_elf(&dynamic));
        elf[64 + 56..64 + 56 + 4].copy_from_slice(&1u32.to_le_bytes());
        let stat = dir.join("static");
        std::fs::write(&stat, &elf).unwrap();
        assert!(!is_dynamic_elf(&stat));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_outbox_is_next_to_the_vm_directories() {
        assert_eq!(
            out_dir_of(Path::new("/run/cloudhv/d/pool-1-4")),
            Path::new("/run/cloudhv/d/.out/pool-1-4")
        );
    }
}
