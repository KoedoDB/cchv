use std::collections::HashSet;
use std::time::Duration;

use log::debug;
#[cfg(target_os = "linux")]
use log::{info, warn};

/// Set this process as a child subreaper.
#[cfg(target_os = "linux")]
pub fn set_child_subreaper() {
    unsafe {
        let ret = libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        if ret < 0 {
            warn!(
                "failed to set child subreaper: {}",
                std::io::Error::last_os_error()
            );
        } else {
            debug!("set as child subreaper");
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_child_subreaper() {
    debug!("set_child_subreaper: no-op on non-Linux");
}

// --- Zombies ---------------------------------------------------------------------------------------------------------------------
//
// The agent is PID 1 and a subreaper, so the processes a container's runtime leaves behind end up as its children, and a zombie stays
// until someone waits for it. A plain `waitpid(-1)` loop is not an option: crun forks twice and the agent waits for crun's own children
// (tokio's waits, the pidfd of `crun run`), and reaping those first makes crun hang or the agent lose an exit status. So a zombie is
// reaped only if it is none of those: it is not a process the agent started, and it was already a zombie, the same process (same start
// time), at the scan before this one. Anything the agent is waiting for is reaped by that wait within moments, long before.

/// How often the zombies are looked for.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(20);

/// A process as /proc/<pid>/stat shows it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Proc {
    pub pid: u32,
    pub state: char,
    pub ppid: u32,
    /// The start time in clock ticks since boot: with the PID, it tells one process from a later one with the same number.
    pub start: u64,
}

/// Read the pid, state, parent and start time out of a line of /proc/<pid>/stat. The command name is in parentheses and may contain
/// spaces and parentheses, so the fields are counted after the last `)`.
pub fn parse_stat(line: &str) -> Option<Proc> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let pid = line[..open].trim().parse().ok()?;
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    // After the name: state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5) flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10)
    // utime(11) stime(12) cutime(13) cstime(14) priority(15) nice(16) num_threads(17) itrealvalue(18) starttime(19)
    Some(Proc {
        pid,
        state: rest.first()?.chars().next()?,
        ppid: rest.get(1)?.parse().ok()?,
        start: rest.get(19)?.parse().ok()?,
    })
}

/// Which zombies to reap now: the children of `me` that are zombies, are not in `tracked`, and were zombies at the last scan too (in
/// `seen`, with the same start time). Returns those, and the zombies seen now for the next scan.
pub fn stale_zombies(
    procs: &[Proc],
    me: u32,
    tracked: &HashSet<u32>,
    seen: &HashSet<(u32, u64)>,
) -> (Vec<u32>, HashSet<(u32, u64)>) {
    let mut reap = Vec::new();
    let mut now = HashSet::new();
    for p in procs {
        if p.state != 'Z' || p.ppid != me || tracked.contains(&p.pid) {
            continue;
        }
        if seen.contains(&(p.pid, p.start)) {
            reap.push(p.pid);
        } else {
            now.insert((p.pid, p.start));
        }
    }
    (reap, now)
}

#[cfg(target_os = "linux")]
fn read_procs() -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter_map(|l| parse_stat(&l))
        .collect()
}

/// One scan: reap the zombies that were zombies at the last scan too and that the agent is not waiting for. Returns what to remember for
/// the next scan.
#[cfg(target_os = "linux")]
pub fn reap_stale_zombies(
    seen: &HashSet<(u32, u64)>,
    tracked: &HashSet<u32>,
) -> HashSet<(u32, u64)> {
    let me = std::process::id();
    let (reap, now) = stale_zombies(&read_procs(), me, tracked, seen);
    for pid in reap {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        if r == pid as i32 {
            info!("reaped orphaned zombie {pid}");
        } else {
            debug!("zombie {pid} was gone or not ours (waitpid returned {r})");
        }
    }
    now
}

#[cfg(not(target_os = "linux"))]
pub fn reap_stale_zombies(
    _seen: &HashSet<(u32, u64)>,
    _tracked: &HashSet<u32>,
) -> HashSet<(u32, u64)> {
    HashSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stat_line_is_read_even_when_the_name_has_spaces_and_parentheses() {
        let line = "123 (my (odd) name) Z 1 123 123 0 -1 4210692 0 0 0 0 0 0 0 0 20 0 1 0 98765 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(
            parse_stat(line),
            Some(Proc {
                pid: 123,
                state: 'Z',
                ppid: 1,
                start: 98765
            })
        );
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (a) S"), None);
    }

    #[test]
    fn only_zombies_seen_twice_that_are_ours_and_not_tracked_are_reaped() {
        let z = |pid, ppid, start| Proc {
            pid,
            state: 'Z',
            ppid,
            start,
        };
        let procs = vec![
            z(10, 1, 100),  // new: remember, do not reap yet
            z(11, 1, 100),  // seen before: reap
            z(12, 1, 100),  // seen before but tracked (the agent waits for it)
            z(13, 99, 100), // someone else's child
            z(14, 1, 200),  // same pid as before, but a different process
            Proc {
                pid: 15,
                state: 'S',
                ppid: 1,
                start: 100,
            }, // alive
        ];
        let tracked: HashSet<u32> = [12].into();
        let seen: HashSet<(u32, u64)> = [(11, 100), (12, 100), (13, 100), (14, 100)].into();
        let (reap, now) = stale_zombies(&procs, 1, &tracked, &seen);
        assert_eq!(reap, [11]);
        assert_eq!(now, [(10, 100), (14, 200)].into());
    }
}
