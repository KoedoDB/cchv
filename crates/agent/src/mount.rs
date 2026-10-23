#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use log::debug;
use log::info;
#[cfg(target_os = "linux")]
use log::warn;

/// Mount essential filesystems for a minimal init environment.
///
/// Called when the agent runs as PID 1 inside the VM.
/// Only available on Linux.
#[cfg(target_os = "linux")]
pub fn mount_essential_filesystems() -> Result<()> {
    use nix::mount::MsFlags;

    info!("mounting essential filesystems");

    mount_if_needed(
        "proc",
        "/proc",
        "proc",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    mount_if_needed(
        "sysfs",
        "/sys",
        "sysfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    mount_if_needed("devtmpfs", "/dev", "devtmpfs", MsFlags::MS_NOSUID)?;

    // Ensure standard device nodes exist with correct permissions.
    // devtmpfs should auto-create these, but if the kernel's devtmpfs
    // mount doesn't populate them, create them manually.
    create_dev_node("/dev/null", 1, 3, 0o666);
    create_dev_node("/dev/zero", 1, 5, 0o666);
    create_dev_node("/dev/full", 1, 7, 0o666);
    create_dev_node("/dev/random", 1, 8, 0o666);
    create_dev_node("/dev/urandom", 1, 9, 0o666);
    create_dev_node("/dev/tty", 5, 0, 0o666);

    std::fs::create_dir_all("/dev/pts").ok();
    mount_if_needed(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
    )?;

    std::fs::create_dir_all("/dev/shm").ok();
    mount_with_data(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some("size=64m,mode=1777"),
    )?;

    // Nothing the agent runs is in /tmp, and no container sees it (containers get the overlay rootfs), so it is as locked as /dev/shm.
    mount_with_data(
        "tmpfs",
        "/tmp",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some("size=64m,mode=1777"),
    )?;

    std::fs::create_dir_all("/run").ok();
    mount_if_needed(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
    )?;

    std::fs::create_dir_all("/sys/fs/cgroup").ok();
    // `nsdelegate` makes a cgroup namespace's root a delegation boundary (a container with a delegated cgroup, see cgdelegate.rs). It is
    // a property of the file system and can only be set here, in the first mount; a kernel that does not know it gets the plain mount.
    let cgroup_flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    if mount_with_data(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        cgroup_flags,
        Some("nsdelegate"),
    )
    .is_err()
    {
        warn!("cgroup2 without nsdelegate");
        mount_if_needed("cgroup2", "/sys/fs/cgroup", "cgroup2", cgroup_flags)?;
    }
    // The controllers a delegated cgroup hands on to its children. Without them at the root no cgroup has their files.
    if let Err(e) = std::fs::write(
        "/sys/fs/cgroup/cgroup.subtree_control",
        "+cpu +memory +pids",
    ) {
        warn!("could not enable cpu, memory and pids for child cgroups: {e}");
    }

    info!("essential filesystems mounted");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_essential_filesystems() -> Result<()> {
    info!("mount_essential_filesystems: no-op on non-Linux");
    Ok(())
}

/// Helper: mount a filesystem if the target is not already mounted.
#[cfg(target_os = "linux")]
fn mount_if_needed(
    source: &str,
    target: &str,
    fstype: &str,
    flags: nix::mount::MsFlags,
) -> Result<()> {
    mount_with_data(source, target, fstype, flags, None)
}

/// As mount_if_needed, with mount options for the file system (such as `size=64m`).
#[cfg(target_os = "linux")]
fn mount_with_data(
    source: &str,
    target: &str,
    fstype: &str,
    flags: nix::mount::MsFlags,
    data: Option<&str>,
) -> Result<()> {
    use nix::mount::mount;

    if is_mounted(target) {
        debug!("{} already mounted at {}", fstype, target);
        return Ok(());
    }

    std::fs::create_dir_all(target).ok();

    mount(Some(source), target, Some(fstype), flags, data)
        .with_context(|| format!("failed to mount {fstype} at {target}"))?;

    debug!("mounted {} at {}", fstype, target);
    Ok(())
}

/// Check if a path is a mount point by reading /proc/mounts.
#[cfg(target_os = "linux")]
fn is_mounted(target: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|mounts| {
            mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(target))
        })
        .unwrap_or(false)
}

/// Create a device node if it doesn't exist.
#[cfg(target_os = "linux")]
pub fn create_dev_node(path: &str, major: u64, minor: u64, mode: u32) {
    use std::path::Path;
    let dev = nix::sys::stat::makedev(major, minor);
    let cpath = std::ffi::CString::new(path).unwrap();

    // Save and clear umask so mknod creates nodes with exact permissions
    let old_umask = unsafe { libc::umask(0) };

    if Path::new(path).exists() {
        unsafe {
            libc::chmod(cpath.as_ptr(), mode);
        }
    } else {
        unsafe {
            libc::mknod(cpath.as_ptr(), libc::S_IFCHR | mode, dev);
        }
    }

    unsafe {
        libc::umask(old_umask);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn create_dev_node(_path: &str, _major: u64, _minor: u64, _mode: u32) {}

/// Kernel settings that close doors a container's code could use, written at boot before any container exists. A setting the kernel does
/// not have (the option is not built in) is skipped; the mounts above are required, these are hardening and must not stop a VM.
///
/// - `ping_group_range`: unprivileged ICMP sockets, so that `ping` works although containers do not have CAP_NET_RAW.
/// - `max_user_namespaces=0`: a container cannot make a user namespace (and so cannot get capabilities inside one).
#[cfg(target_os = "linux")]
const SYSCTLS: &[(&str, &str)] = &[
    ("kernel.dmesg_restrict", "1"),
    ("kernel.kptr_restrict", "2"),
    ("kernel.perf_event_paranoid", "3"),
    ("kernel.unprivileged_bpf_disabled", "2"),
    ("kernel.yama.ptrace_scope", "1"),
    ("user.max_user_namespaces", "0"),
    ("fs.protected_symlinks", "1"),
    ("fs.protected_hardlinks", "1"),
    ("net.ipv4.ping_group_range", "0 2147483647"),
];

/// The file of a sysctl key under /proc/sys (`kernel.yama.ptrace_scope` is `/proc/sys/kernel/yama/ptrace_scope`).
#[cfg(target_os = "linux")]
fn sysctl_path(key: &str) -> String {
    format!("/proc/sys/{}", key.replace('.', "/"))
}

#[cfg(target_os = "linux")]
pub fn apply_sysctls() {
    for (key, value) in SYSCTLS {
        match std::fs::write(sysctl_path(key), value) {
            Ok(()) => debug!("sysctl {key}={value}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("sysctl {key}: not in this kernel")
            }
            Err(e) => log::warn!("sysctl {key}={value}: {e}"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_sysctls() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn sysctl_keys_become_paths_under_proc_sys() {
        assert_eq!(
            sysctl_path("kernel.dmesg_restrict"),
            "/proc/sys/kernel/dmesg_restrict"
        );
        assert_eq!(
            sysctl_path("kernel.yama.ptrace_scope"),
            "/proc/sys/kernel/yama/ptrace_scope"
        );
        assert!(SYSCTLS
            .iter()
            .any(|(k, v)| *k == "user.max_user_namespaces" && *v == "0"));
    }
}
