//! The open-files limit of the guest.
//!
//! The kernel gives PID 1 `RLIMIT_NOFILE` soft 1024 / hard 4096 (INR_OPEN_CUR, INR_OPEN_MAX), and every container the agent starts inherits it: a Kubernetes pod has no way to
//! ask for more, and containerd's CRI puts no `rlimits` in the spec, so `sanitize_rlimits` (spec.rs) has nothing to raise. A program that holds many sockets (Realtime's WebSockets) hit
//! the 4096 and the image's own `ulimit -Sn 65536` failed with "Invalid argument" (stg, 2026-11-16). The agent therefore raises its own limit once, as PID 1, before it starts anything.
//!
//! Soft 65536, hard 1048576 (the kernel's default `fs.nr_open`, the most a process may ask for):
//! - the soft limit is what a program gets without asking: 65536 is what Realtime, Node and most servers ask for. A soft limit of 1048576 would make every program that closes all its
//!   descriptors after a fork (close_fds in some runtimes) loop a million times, and PostgreSQL does not need it (`max_files_per_process` is 1000);
//! - the hard limit lets a program (an image's run.sh, `ulimit -Sn`, a container's spec) raise its own soft limit up to what the kernel allows.
//!
//! Each descriptor costs kernel memory only while it is open, so the limit itself costs nothing, and the memory limit of the pod (cgroup) still bounds what a container can use.

/// What a container may open without asking.
pub const NOFILE_SOFT: u64 = 65_536;
/// The most a container may raise its own limit to: the kernel's default `fs.nr_open`.
pub const NOFILE_HARD: u64 = 1_048_576;

/// The limits to set, given the kernel's `fs.nr_open` (the setrlimit of a hard limit above it fails with EPERM). `None`: it could not be read, and the default is assumed.
pub fn nofile_limits(nr_open: Option<u64>) -> (u64, u64) {
    let hard = NOFILE_HARD.min(nr_open.unwrap_or(NOFILE_HARD)).max(1);
    (NOFILE_SOFT.min(hard), hard)
}

#[cfg(target_os = "linux")]
fn read_nr_open() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Set `RLIMIT_NOFILE` of this process, soft then hard in one call.
#[cfg(target_os = "linux")]
pub fn set_nofile(soft: u64, hard: u64) -> std::io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: setrlimit reads the struct, which lives through the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The current `RLIMIT_NOFILE` as (soft, hard).
#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unnecessary_cast)] // rlim_t is u64 here, but not on every target
fn get_nofile() -> std::io::Result<(u64, u64)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the struct, which lives through the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((lim.rlim_cur as u64, lim.rlim_max as u64))
}

/// Raise the open-files limit of this process (the agent, PID 1) for every container it starts. A failure is logged, not fatal: the containers then keep the kernel's 1024/4096.
#[cfg(target_os = "linux")]
pub fn raise_nofile() {
    let (soft, hard) = nofile_limits(read_nr_open());
    if soft < NOFILE_SOFT {
        // A kernel whose fs.nr_open is below 65536: Realtime's `ulimit -Sn 65536` would fail. Said loudly, once, at boot.
        log::warn!("fs.nr_open allows only {hard} open files: below the {NOFILE_SOFT} the containers are meant to get");
    }
    match set_nofile(soft, hard) {
        Ok(()) => log::info!("RLIMIT_NOFILE soft={soft} hard={hard}"),
        Err(e) => log::warn!("RLIMIT_NOFILE soft={soft} hard={hard}: {e}"),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn raise_nofile() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_soft_65536_and_hard_1048576() {
        assert_eq!(nofile_limits(None), (65_536, 1_048_576));
        assert_eq!(nofile_limits(Some(1_048_576)), (65_536, 1_048_576));
    }

    #[test]
    fn a_kernel_with_a_lower_nr_open_lowers_the_hard_limit_and_the_soft_one_with_it() {
        assert_eq!(nofile_limits(Some(500_000)), (65_536, 500_000));
        assert_eq!(nofile_limits(Some(4_096)), (4_096, 4_096));
    }

    #[test]
    fn a_higher_nr_open_does_not_raise_the_hard_limit_above_what_is_decided() {
        assert_eq!(nofile_limits(Some(2_000_000)), (65_536, 1_048_576));
    }

    #[test]
    fn nr_open_zero_does_not_give_a_zero_limit() {
        assert_eq!(nofile_limits(Some(0)), (1, 1));
    }

    /// The helper really sets the limit (lowering the soft limit needs no privilege; the test restores it).
    #[cfg(target_os = "linux")]
    #[test]
    fn set_nofile_sets_and_get_nofile_reads_it_back() {
        let (soft0, hard0) = get_nofile().unwrap();
        let target = soft0.clamp(16, 1_000);
        set_nofile(target, hard0).unwrap();
        assert_eq!(get_nofile().unwrap(), (target, hard0));
        set_nofile(soft0, hard0).unwrap();
        assert_eq!(get_nofile().unwrap(), (soft0, hard0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_soft_limit_above_the_hard_limit_is_refused() {
        let (_, hard0) = get_nofile().unwrap();
        if hard0 < u64::MAX / 2 {
            assert!(set_nofile(hard0 + 1, hard0).is_err());
        }
    }
}
