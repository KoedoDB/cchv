//! What root reads and acts on about a VM lives here, never in the VM's own directory.
//!
//! A rootless Cloud Hypervisor owns its VM's state directory (`<state_dir>/<vm_id>`): it can delete and re-create any file in it, so
//! a `ch.pid` that root wrote there is whatever a compromised guest wants it to be, and `kill(<that>)` as root is a way to signal
//! any process of the host (the number 4294967295 is -1: every process). The daemon's own records are therefore kept in
//! `<state_dir>/.ctl/<vm_id>/`, which is root's (0700) and which a VM's user cannot even enter, and a process is signalled only
//! after it has been checked to be that VM's Cloud Hypervisor (its command line names the VM's API socket and, rootless, its
//! user is the VM's user), through a pidfd so that a recycled pid is not hit.
//!
//! Files in a VM's registry directory: `ch.pid`, `ch.uid` (rootless), `acl.list` (the files the VM was given read access to).

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The name of the registry directory in the state directory. Entries of the state directory that start with a dot are not VMs.
pub const CTL_DIR: &str = ".ctl";

/// The registry directory of the VM whose state directory this is: `<parent>/.ctl/<name>`.
pub fn dir_of(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join(CTL_DIR)
        .join(state_dir.file_name().unwrap_or_default())
}

fn mkdir_private(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
    }
    // Whatever umask or an earlier version left: root only.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Make the registry directory of a VM (and the registry itself).
pub fn create(state_dir: &Path) -> Result<PathBuf> {
    let dir = dir_of(state_dir);
    if let Some(root) = dir.parent() {
        mkdir_private(root)?;
    }
    mkdir_private(&dir)?;
    Ok(dir)
}

pub fn write(state_dir: &Path, name: &str, content: &str) -> Result<()> {
    let dir = create(state_dir)?;
    std::fs::write(dir.join(name), content).with_context(|| format!("write {name} of {}", state_dir.display()))
}

pub fn read(state_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir_of(state_dir).join(name)).ok()
}

pub fn append(state_dir: &Path, name: &str, line: &str) -> Result<()> {
    use std::io::Write;
    let dir = create(state_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Forget a VM (its registry directory goes).
pub fn remove(state_dir: &Path) {
    let _ = std::fs::remove_dir_all(dir_of(state_dir));
}

pub fn write_pid(state_dir: &Path, pid: u32) -> Result<()> {
    write(state_dir, "ch.pid", &pid.to_string())
}

/// The recorded Cloud Hypervisor pid; never 0 or 1 (nor anything that is not a pid).
pub fn read_pid(state_dir: &Path) -> Option<u32> {
    read(state_dir, "ch.pid")
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|p| valid_pid(*p))
}

/// A pid that may be signalled at all: not 0 (the group), 1 (init), and one that fits a `pid_t` (4294967295 is -1: everything).
pub fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Whether the command line (`/proc/<pid>/cmdline`: arguments separated by NUL) is a process started with `--api-socket <api_sock>`.
/// The socket is `<state_dir>/api.sock` of one VM, a path only this daemon makes, so a process that has it as that argument is that VM's
/// Cloud Hypervisor. (The name of the program is not looked at: the binary is the operator's to name.)
pub fn cmdline_is_ch_of(cmdline: &[u8], api_sock: &Path) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let sock = api_sock.as_os_str().as_encoded_bytes();
    !args.first().map(|a| a.is_empty()).unwrap_or(true)
        && args.windows(2).any(|w| w[0] == b"--api-socket" && w[1] == sock)
}

/// The real user of a process, from `/proc/<pid>/status` (the owner of `/proc/<pid>` is root for a process that is not dumpable).
fn uid_of_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_status_uid(&status)
}

fn parse_status_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|u| u.parse().ok())
}

/// Whether the process exists and is not a zombie.
pub fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `pid (comm) S ...`: the state is the first character after the last `)`.
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .map(|s| s != 'Z' && s != 'X')
            .unwrap_or(false),
        Err(_) => false,
    }
}

const SYS_PIDFD_OPEN: libc::c_long = 434;
const SYS_PIDFD_SEND_SIGNAL: libc::c_long = 424;

/// Send `sig` to `pid`, which must be the Cloud Hypervisor of the VM whose state directory this is (see the module comment). Returns
/// whether the signal was sent; a process that is not that VM's Cloud Hypervisor is left alone.
pub fn signal_ch(pid: u32, state_dir: &Path, sig: i32) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    // Hold the process first, then look at it: if the pid is reused after this, the pidfd still names the old process.
    let pidfd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0) };
    let have_pidfd = pidfd >= 0;
    if !have_pidfd && io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
        return false; // ESRCH: gone already
    }
    let verified = (|| {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        if !cmdline_is_ch_of(&cmdline, &state_dir.join("api.sock")) {
            return None;
        }
        if let Some(want) = crate::rootless::uid_of(state_dir) {
            if uid_of_pid(pid) != Some(want) {
                return None;
            }
        }
        Some(())
    })()
    .is_some();
    let sent = if !verified {
        false
    } else if have_pidfd {
        unsafe { libc::syscall(SYS_PIDFD_SEND_SIGNAL, pidfd as libc::c_int, sig, 0usize, 0u32) == 0 }
    } else {
        unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
    };
    if have_pidfd {
        unsafe { libc::close(pidfd as libc::c_int) };
    }
    if !verified {
        log::warn!(
            "not signalling pid {pid}: it is not the Cloud Hypervisor of {}",
            state_dir.display()
        );
    }
    sent
}

/// The pids of the processes that have `uid` as their real, effective, saved or file-system user (not zombies).
pub fn pids_of_uid(uid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        if status_has_uid(&status, uid) {
            out.push(pid);
        }
    }
    out
}

fn status_has_uid(status: &str, uid: u32) -> bool {
    let zombie = status
        .lines()
        .find_map(|l| l.strip_prefix("State:"))
        .map(|s| s.trim_start().starts_with('Z') || s.trim_start().starts_with('X'))
        .unwrap_or(false);
    if zombie {
        return false;
    }
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .map(|l| l.split_whitespace().filter_map(|u| u.parse::<u32>().ok()).any(|u| u == uid))
        .unwrap_or(false)
}

/// The processes (pid, user) whose real user is in [lo, hi) (not zombies).
pub fn procs_in_uid_range(lo: u32, hi: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        if let Some(uid) = parse_status_uid(&status) {
            if uid >= lo && uid < hi && status_has_uid(&status, uid) {
                out.push((pid, uid));
            }
        }
    }
    out
}

/// SIGKILL a process only if it still belongs to `uid` (the pid may have been recycled since it was listed).
pub fn kill_if_uid(pid: u32, uid: u32) {
    if !valid_pid(pid) {
        return;
    }
    let pidfd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0) };
    if pidfd < 0 {
        return;
    }
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        if status_has_uid(&status, uid) {
            unsafe { libc::syscall(SYS_PIDFD_SEND_SIGNAL, pidfd as libc::c_int, libc::SIGKILL, 0usize, 0u32) };
        }
    }
    unsafe { libc::close(pidfd as libc::c_int) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_pid_may_be_signalled() {
        assert!(!valid_pid(0));
        assert!(!valid_pid(1));
        assert!(!valid_pid(u32::MAX), "4294967295 is -1 as a pid_t");
        assert!(!valid_pid(i32::MAX as u32 + 1));
        assert!(valid_pid(2));
        assert!(valid_pid(4_194_304));
    }

    #[test]
    fn the_command_line_must_name_the_vms_own_api_socket() {
        let sock = Path::new("/run/cloudhv/daemon/pool-1-4/api.sock");
        let ours = b"/usr/local/bin/cloud-hypervisor\0--api-socket\0/run/cloudhv/daemon/pool-1-4/api.sock\0";
        assert!(cmdline_is_ch_of(ours, sock));
        let other_vm = b"/usr/local/bin/cloud-hypervisor\0--api-socket\0/run/cloudhv/daemon/pool-1-5/api.sock\0";
        assert!(!cmdline_is_ch_of(other_vm, sock));
        let renamed = b"/opt/ch\0--api-socket\0/run/cloudhv/daemon/pool-1-4/api.sock\0";
        assert!(cmdline_is_ch_of(renamed, sock), "the program's name does not matter");
        let sshd = b"/usr/sbin/sshd\0-D\0";
        assert!(!cmdline_is_ch_of(sshd, sock));
        let no_flag = b"/usr/local/bin/cloud-hypervisor\0/run/cloudhv/daemon/pool-1-4/api.sock\0";
        assert!(!cmdline_is_ch_of(no_flag, sock));
        assert!(!cmdline_is_ch_of(b"", sock));
    }

    #[test]
    fn the_status_of_a_process_tells_its_users_and_zombies() {
        let st = "Name:\tx\nState:\tS (sleeping)\nUid:\t200001\t200001\t200001\t200001\n";
        assert_eq!(parse_status_uid(st), Some(200001));
        assert!(status_has_uid(st, 200001));
        assert!(!status_has_uid(st, 5));
        let zombie = "Name:\tx\nState:\tZ (zombie)\nUid:\t200001\t200001\t200001\t200001\n";
        assert!(!status_has_uid(zombie, 200001));
        let setuid = "Name:\tx\nState:\tS (sleeping)\nUid:\t1\t200001\t1\t1\n";
        assert!(status_has_uid(setuid, 200001), "the effective user counts");
    }

    #[test]
    fn this_process_is_alive_and_a_far_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(4_194_300));
        assert!(pids_of_uid(unsafe { libc::getuid() }).contains(&std::process::id()));
    }

    #[test]
    fn the_registry_is_next_to_the_vm_directories_and_apart_from_them() {
        let d = dir_of(Path::new("/run/cloudhv/daemon/warm-abc-3"));
        assert_eq!(d, Path::new("/run/cloudhv/daemon/.ctl/warm-abc-3"));
    }
}
