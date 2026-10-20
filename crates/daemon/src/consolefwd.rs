//! The guest's console, forwarded to the daemon's log.
//!
//! With `serial_console` on, the Cloud Hypervisor writes each VM's console (the kernel's messages and the agent's log, the agent being the init)
//! to `console.log` in the VM's directory. That file goes with the VM, so what the guest said just before it died was lost, and it has no limit
//! on its size. This module reads what is new in each file every `console_forward_interval_ms`, and writes it to the daemon's log (journald, when
//! the daemon is a service) as `console[vm=<name>] <line>`, where a log collector (Vector) picks it up.
//!
//! - **What the guest says is the guest's**: the lines are text from an untrusted guest. They are made printable (escape sequences and control
//!   characters taken out, a line cut at `console_forward_max_line_bytes`) and always carry the VM's name, which the daemon gives and the guest
//!   cannot choose. They are never read as the daemon's own.
//! - **A guest that writes without end cannot flood the log**: each VM has a budget of `console_forward_max_lines_per_sec` lines a second (a burst
//!   of five times that); the lines over it are counted and told as one line ("N lines dropped").
//! - **The file does not grow in memory**: what was forwarded, except the last `console_keep_bytes`, is dropped from the file with a hole
//!   (`fallocate(PUNCH_HOLE | KEEP_SIZE)`). The Cloud Hypervisor does not open the file for append, so cutting the file (copytruncate) would leave
//!   a hole of the old size in front of the next write; a hole punched in the old part moves nothing, and nothing written is lost. The
//!   file's size, as `ls` shows it, only grows; the memory or disk it takes is the kept tail.
//! - **The last lines are kept**: `flush` forwards what is left when a VM is destroyed, before its directory goes.
//! - The files are opened by root with `O_NOFOLLOW`; a link in the place of the file is refused.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::DaemonConfig;

/// How much one pass reads of one file (a guest that writes faster is caught up with at the next pass, or dropped by the budget).
const READ_CHUNK: u64 = 256 * 1024;
/// The burst a VM may have, in seconds of its rate.
const BURST_SECONDS: f64 = 5.0;

#[derive(Debug, Clone)]
struct Settings {
    interval: Duration,
    max_lines_per_sec: u32,
    max_line_bytes: usize,
    keep_bytes: u64,
    min_punch_bytes: u64,
    /// The directories whose subdirectories (one for each VM) may hold a `console.log`.
    roots: Vec<PathBuf>,
}

static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
static TRACKERS: OnceLock<Mutex<HashMap<PathBuf, Tracker>>> = OnceLock::new();

fn settings() -> Option<&'static Settings> {
    SETTINGS.get().and_then(|s| s.as_ref())
}

fn trackers() -> &'static Mutex<HashMap<PathBuf, Tracker>> {
    TRACKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What is known of one file between two passes.
#[derive(Debug, Default)]
struct Tracker {
    /// How far the file was read.
    offset: u64,
    /// How far the file was dropped with a hole.
    punched: u64,
    /// The start of a line whose end was not written yet.
    pending: Vec<u8>,
    /// The budget of lines.
    tokens: f64,
    last_pass: Option<Instant>,
    dropped: u64,
}

/// Read the configuration. Off unless the console is written to a file (`serial_console`) and `console_forward` is on.
pub fn init(config: &DaemonConfig) {
    let mut roots = Vec::new();
    if !config.emptydir_dir.is_empty() {
        roots.push(PathBuf::from(&config.emptydir_dir));
    }
    roots.push(PathBuf::from(&config.state_dir));
    let s = (config.serial_console_enabled() && config.console_forward).then(|| Settings {
        interval: Duration::from_millis(config.console_forward_interval_ms.max(100)),
        max_lines_per_sec: config.console_forward_max_lines_per_sec.max(1),
        max_line_bytes: config.console_forward_max_line_bytes.max(64),
        keep_bytes: config.console_keep_bytes,
        min_punch_bytes: config.console_min_punch_bytes,
        roots,
    });
    if s.is_some() {
        info!("the guests' consoles are forwarded to the log (console[vm=...])");
    }
    let _ = SETTINGS.set(s);
}

/// Forward the consoles every interval, until the daemon stops. A no-op when the feature is off.
pub fn spawn() {
    let Some(s) = settings() else {
        return;
    };
    let interval = s.interval;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let _ = tokio::task::spawn_blocking(forward_all).await;
        }
    });
}

/// The name of the VM a console file belongs to: its directory's name, when that is a name the daemon gives.
fn vm_of(path: &Path) -> Option<String> {
    let name = path.parent()?.file_name()?.to_str()?;
    crate::validate::is_id(name).then(|| name.to_string())
}

fn forward_all() {
    let Some(s) = settings() else {
        return;
    };
    for root in &s.roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let log = e.path().join("console.log");
            if vm_of(&log).is_some() && std::fs::symlink_metadata(&log).is_ok() {
                forward_file(s, &log, false);
            }
        }
    }
}

/// Forward what is left of the console of the VM whose state directory is `state_dir`, before the directory goes. A VM with no console is not
/// an error.
pub fn flush(state_dir: &Path) {
    let Some(s) = settings() else {
        return;
    };
    let log = crate::vm_lifecycle::console_log_path(state_dir);
    if std::fs::symlink_metadata(&log).is_ok() {
        forward_file(s, &log, true);
    }
    if let Ok(mut t) = trackers().lock() {
        t.remove(&log);
    }
}

fn forward_file(s: &Settings, path: &Path, last: bool) {
    let Some(vm) = vm_of(path) else {
        return;
    };
    let mut map = trackers().lock().unwrap_or_else(|p| p.into_inner());
    let tracker = map.entry(path.to_path_buf()).or_default();
    let mut emit = |line: &str| info!(target: "console", "console[vm={vm}] {line}");
    if let Err(e) = forward_from(s, path, tracker, Instant::now(), last, &mut emit) {
        warn!("console of VM {vm}: {e}");
    }
}

/// One pass over one file. `emit` gets each line to forward. `last` is the final pass of a VM: the line that has no end yet is forwarded too and
/// the budget is full.
fn forward_from(
    s: &Settings,
    path: &Path,
    t: &mut Tracker,
    now: Instant,
    last: bool,
    emit: &mut dyn FnMut(&str),
) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(());
    }
    let len = meta.len();
    if len < t.offset {
        // The file was cut: begin again.
        *t = Tracker::default();
    }
    // The budget: it grows with the time since the last pass, up to the burst.
    let cap = s.max_lines_per_sec as f64 * BURST_SECONDS;
    let elapsed = t
        .last_pass
        .map(|p| now.saturating_duration_since(p).as_secs_f64())
        .unwrap_or(BURST_SECONDS);
    t.tokens = if last {
        cap
    } else {
        (t.tokens + elapsed * s.max_lines_per_sec as f64).min(cap)
    };
    t.last_pass = Some(now);

    let mut taken = 0;
    while t.offset < len && taken < READ_CHUNK {
        // Skip a hole (what was dropped): there is nothing to read in it.
        let data =
            unsafe { libc::lseek(file.as_raw_fd(), t.offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            // No data after the offset: the rest is a hole.
            t.offset = len;
            break;
        }
        t.offset = data as u64;
        let want = (len - t.offset).min(READ_CHUNK - taken) as usize;
        let mut buf = vec![0u8; want];
        let n = file.read_at(&mut buf, t.offset)?;
        if n == 0 {
            break;
        }
        t.offset += n as u64;
        taken += n as u64;
        lines_of(t, &buf[..n], s.max_line_bytes, emit);
    }
    if last && !t.pending.is_empty() {
        let line = std::mem::take(&mut t.pending);
        take_line(t, &line, s.max_line_bytes, emit);
    }
    if t.dropped > 0 {
        emit(&format!(
            "{} lines dropped (more than {} a second)",
            t.dropped, s.max_lines_per_sec
        ));
        t.dropped = 0;
    }
    // Drop what was forwarded, except the tail.
    let target = t.offset.saturating_sub(s.keep_bytes);
    if target > t.punched && target - t.punched >= s.min_punch_bytes {
        if punch(path, t.punched, target - t.punched) {
            t.punched = target;
        }
    }
    Ok(())
}

/// Split `bytes` (the pending start of a line, then these) into lines and hand each to the budget; the end that has no newline stays pending.
fn lines_of(t: &mut Tracker, bytes: &[u8], max: usize, emit: &mut dyn FnMut(&str)) {
    let mut rest = bytes;
    while let Some(i) = rest.iter().position(|b| *b == b'\n') {
        let mut line = std::mem::take(&mut t.pending);
        line.extend_from_slice(&rest[..i]);
        take_line(t, &line, max, emit);
        rest = &rest[i + 1..];
    }
    t.pending.extend_from_slice(rest);
    // A line that never ends must not grow without end: forward it cut.
    if t.pending.len() > max * 4 {
        let line = std::mem::take(&mut t.pending);
        take_line(t, &line, max, emit);
    }
}

fn take_line(t: &mut Tracker, line: &[u8], max: usize, emit: &mut dyn FnMut(&str)) {
    let text = printable(line, max);
    if text.is_empty() {
        return;
    }
    if t.tokens >= 1.0 {
        t.tokens -= 1.0;
        emit(&text);
    } else {
        t.dropped += 1;
    }
}

/// A line the guest wrote, made safe to put in a log: UTF-8 (what is not is replaced), no escape sequences (colours of the agent's log), no
/// control characters (a tab stays), no carriage return, and cut at `max` bytes.
fn printable(line: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(line);
    let mut out = String::with_capacity(text.len().min(max + 4));
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC [ parameters final byte (CSI), or ESC and one more character.
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if c == '\t' || !(c.is_control()) {
            out.push(c);
        }
    }
    let out = out.trim_end().to_string();
    if out.len() <= max {
        return out;
    }
    let mut end = max;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &out[..end])
}

/// Drop `len` bytes of the file from `offset` with a hole, keeping its size. `false` if the file system cannot (the file then keeps its size).
fn punch(path: &Path, offset: u64, len: u64) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(path)
    else {
        return false;
    };
    unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            len as libc::off_t,
        ) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    fn settings_for(root: &Path) -> Settings {
        Settings {
            interval: Duration::from_secs(1),
            max_lines_per_sec: 100,
            max_line_bytes: 200,
            keep_bytes: 1024,
            min_punch_bytes: 0,
            roots: vec![root.to_path_buf()],
        }
    }

    fn vm_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-console-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let vm = d.join("sized-1-0");
        std::fs::create_dir_all(&vm).unwrap();
        vm
    }

    fn pass(s: &Settings, path: &Path, t: &mut Tracker, now: Instant, last: bool) -> Vec<String> {
        let mut out = Vec::new();
        forward_from(s, path, t, now, last, &mut |l| out.push(l.to_string())).unwrap();
        out
    }

    #[test]
    fn a_line_is_made_printable_and_cut() {
        assert_eq!(printable(b"plain text\r", 100), "plain text");
        assert_eq!(
            printable(
                b"\x1b[90m[\x1b[0m2026-10-20T07:29:24Z \x1b[32mINFO \x1b[0m agent] started",
                200
            ),
            "[2026-10-20T07:29:24Z INFO  agent] started"
        );
        assert_eq!(
            printable(b"a\x07b\x00c\x7fd\te", 100),
            "abcd\te",
            "control characters go, a tab stays"
        );
        assert_eq!(
            printable(b"bad \xff\xfe bytes", 100),
            "bad \u{fffd}\u{fffd} bytes"
        );
        assert_eq!(printable(b"   ", 100), "", "a line with nothing is nothing");
        let long = "x".repeat(500);
        let cut = printable(long.as_bytes(), 100);
        assert!(cut.starts_with(&"x".repeat(100)) && cut.ends_with('…') && cut.len() <= 104);
        // A cut inside a multi-byte character goes back to its start.
        let jp = "あ".repeat(50);
        assert!(printable(jp.as_bytes(), 10).is_char_boundary(0));
        // An escape that never ends does not hang or panic.
        assert_eq!(printable(b"x\x1b[31", 100), "x");
        assert_eq!(printable(b"y\x1b", 100), "y");
    }

    #[test]
    fn only_whole_lines_are_forwarded_and_the_start_of_a_line_waits() {
        let vm = vm_dir("lines");
        let log = vm.join("console.log");
        let s = settings_for(vm.parent().unwrap());
        let mut t = Tracker::default();
        let now = Instant::now();
        std::fs::write(&log, "first\nsecond\nthi").unwrap();
        assert_eq!(pass(&s, &log, &mut t, now, false), ["first", "second"]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"rd\nfourth").unwrap();
        assert_eq!(pass(&s, &log, &mut t, now, false), ["third"]);
        assert!(pass(&s, &log, &mut t, now, false).is_empty(), "nothing new");
        // The last pass of a VM forwards the line that has no end.
        assert_eq!(pass(&s, &log, &mut t, now, true), ["fourth"]);
        let _ = std::fs::remove_dir_all(vm.parent().unwrap());
    }

    #[test]
    fn a_guest_that_writes_without_end_gets_a_budget_and_a_count_of_what_was_dropped() {
        let vm = vm_dir("budget");
        let log = vm.join("console.log");
        let s = settings_for(vm.parent().unwrap()); // 100 lines a second, a burst of 500
        let mut t = Tracker::default();
        let now = Instant::now();
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&log, text).unwrap();
        let out = pass(&s, &log, &mut t, now, false);
        let lines = out.iter().filter(|l| l.starts_with("line ")).count();
        assert_eq!(lines, 500, "the burst");
        assert!(
            out.last().unwrap().contains("1500 lines dropped"),
            "{:?}",
            out.last()
        );
        // Later the budget has grown a little: one second, 100 lines.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(("more\n".repeat(1000)).as_bytes()).unwrap();
        let out = pass(&s, &log, &mut t, now + Duration::from_secs(1), false);
        assert_eq!(out.iter().filter(|l| *l == "more").count(), 100);
        let _ = std::fs::remove_dir_all(vm.parent().unwrap());
    }

    #[test]
    fn what_was_forwarded_is_dropped_with_a_hole_and_nothing_is_forwarded_twice() {
        let vm = vm_dir("punch");
        let log = vm.join("console.log");
        let mut s = settings_for(vm.parent().unwrap());
        s.max_lines_per_sec = 1_000_000;
        s.keep_bytes = 4096;
        let mut t = Tracker::default();
        let now = Instant::now();
        let text: String = (0..20000)
            .map(|i| format!("a line of the console, number {i}\n"))
            .collect();
        std::fs::write(&log, &text).unwrap();
        let len = std::fs::metadata(&log).unwrap().len();
        // One pass reads READ_CHUNK; go on until it is all read.
        let mut all = Vec::new();
        while t.offset < len {
            all.extend(pass(&s, &log, &mut t, now, false));
        }
        assert_eq!(all.len(), 20000);
        assert_eq!(all[19999], "a line of the console, number 19999");
        let meta = std::fs::metadata(&log).unwrap();
        assert_eq!(
            meta.len(),
            len,
            "the size is kept, so that nothing the guest writes next lands in the wrong place"
        );
        if t.punched == 0 {
            eprintln!("this file system cannot punch holes: the rest of the test is skipped");
            let _ = std::fs::remove_dir_all(vm.parent().unwrap());
            return;
        }
        assert!(
            meta.blocks() * 512 < len / 2,
            "the old part took its blocks back: {} of {}",
            meta.blocks() * 512,
            len
        );
        // The guest writes more, at the end: it is forwarded, and what was forwarded is not again.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"after the hole\n").unwrap();
        assert_eq!(pass(&s, &log, &mut t, now, false), ["after the hole"]);
        // A reader that starts over (a new daemon) finds only the kept tail, with no zeros.
        let mut fresh = Tracker::default();
        let out = pass(&s, &log, &mut fresh, now, false);
        assert!(!out.is_empty() && out.len() < 200, "{}", out.len());
        // (the first line may be a part of one: the hole ends inside a line)
        assert!(
            out.iter()
                .skip(1)
                .all(|l| l.starts_with("a line of the console") || l == "after the hole"),
            "{:?}",
            &out[..3]
        );
        let _ = std::fs::remove_dir_all(vm.parent().unwrap());
    }

    #[test]
    fn a_file_that_was_cut_is_read_from_the_start_again() {
        let vm = vm_dir("cut");
        let log = vm.join("console.log");
        let mut s = settings_for(vm.parent().unwrap());
        s.keep_bytes = u64::MAX; // no hole in this test
        let mut t = Tracker::default();
        let now = Instant::now();
        std::fs::write(&log, "one\ntwo\n").unwrap();
        assert_eq!(pass(&s, &log, &mut t, now, false), ["one", "two"]);
        std::fs::write(&log, "three\n").unwrap();
        assert_eq!(pass(&s, &log, &mut t, now, false), ["three"]);
        let _ = std::fs::remove_dir_all(vm.parent().unwrap());
    }

    #[test]
    fn a_link_in_the_place_of_the_file_is_refused() {
        let vm = vm_dir("link");
        let secret = vm.parent().unwrap().join("secret");
        std::fs::write(&secret, "do not forward this\n").unwrap();
        let log = vm.join("console.log");
        std::os::unix::fs::symlink(&secret, &log).unwrap();
        let s = settings_for(vm.parent().unwrap());
        let mut t = Tracker::default();
        let mut out = Vec::new();
        let r = forward_from(&s, &log, &mut t, Instant::now(), false, &mut |l| {
            out.push(l.to_string())
        });
        assert!(r.is_err() && out.is_empty());
        let _ = std::fs::remove_dir_all(vm.parent().unwrap());
    }

    #[test]
    fn the_vm_is_named_by_its_directory_and_only_when_the_daemon_could_have_named_it() {
        assert_eq!(
            vm_of(Path::new("/data/emptydir/sized-12-3/console.log")).as_deref(),
            Some("sized-12-3")
        );
        assert_eq!(
            vm_of(Path::new("/data/emptydir/pool-9-1/console.log")).as_deref(),
            Some("pool-9-1")
        );
        assert_eq!(
            vm_of(Path::new("/data/emptydir/not a vm/console.log")),
            None
        );
        assert_eq!(vm_of(Path::new("/data/emptydir/../console.log")), None);
    }
}
