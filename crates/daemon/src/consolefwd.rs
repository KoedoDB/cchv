//! The guest's console, forwarded to the daemon's log.
//!
//! With `serial_console` and `console_forward` on, each VM's console (the kernel's messages and the agent's log, the agent being the init) goes
//! through a Unix socket the Cloud Hypervisor makes in the VM's state directory (`console.sock`, `console` mode `Socket`). The daemon connects to it
//! and logs what it reads as `console[vm=<name>] <line>` (journald, when the daemon is a service), where a log collector (Vector) picks it up.
//!
//! There is no file: what the guest says takes no disk and no memory of the host, and nothing has to be rotated or cut. (Written to a file, the
//! console grew with whatever the guest wrote until the daemon dropped the forwarded part, and the Cloud Hypervisor does not open the file for
//! append, so it could not be rotated: tried and dropped, 2026-10-20.) The price: **while nobody reads the socket, the Cloud Hypervisor drops what
//! the guest writes** (the guest is never held up). The daemon connects before the guest boots (`attach`, between `vm.create` and `vm.boot`, or
//! after `vm.restore`), and again from a scan every `console_forward_interval_ms` (after the daemon restarted), so only what a VM says while the
//! daemon is down, and the few lines between a lost connection and the next scan, are lost.
//!
//! - **What the guest says is the guest's**: the lines are text from an untrusted guest. They are made printable (escape sequences and control
//!   characters taken out, a line cut at `console_forward_max_line_bytes`) and always carry the VM's name, which the daemon gives and the guest
//!   cannot choose. They are never read as the daemon's own.
//! - **A guest that writes without end cannot flood the log**: each VM has a budget of `console_forward_max_lines_per_sec` lines a second (a burst
//!   of five times that); the lines over it are counted and told as one line ("N lines dropped"). It cannot flood the daemon either: past
//!   `console_forward_max_bytes_per_sec` the daemon stops reading until the second is over, and the Cloud Hypervisor drops the rest.
//! - **The other end is checked**: the socket must be a socket (not a link), and the process that listens on it must have the user of the VM's
//!   Cloud Hypervisor (root, without `ch_rootless`). The daemon only reads; it writes nothing to the socket.
//! - **The last lines are kept**: a connection is read to its end, so what a VM said just before it died is in the log.

use std::collections::HashSet;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

use crate::config::DaemonConfig;

/// The burst a VM may have, in seconds of its rate.
const BURST_SECONDS: f64 = 5.0;
/// How long `attach` waits for the Cloud Hypervisor to make the socket.
const ATTACH_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct Settings {
    interval: Duration,
    max_lines_per_sec: u32,
    max_line_bytes: usize,
    max_bytes_per_sec: u64,
    /// The directories whose subdirectories (one for each VM) may hold a `console.sock`.
    roots: Vec<PathBuf>,
}

static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
/// The state directories whose console is being read.
static ACTIVE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn settings() -> Option<&'static Settings> {
    SETTINGS.get().and_then(|s| s.as_ref())
}

fn active() -> &'static Mutex<HashSet<PathBuf>> {
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// What is known of one console between two reads.
#[derive(Debug, Default)]
struct Tracker {
    /// The start of a line whose end was not written yet.
    pending: Vec<u8>,
    /// The budget of lines.
    tokens: f64,
    last_pass: Option<Instant>,
    dropped: u64,
}

/// Where the Cloud Hypervisor makes the console socket of the VM whose state directory this is.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("console.sock")
}

/// Read the configuration. Off unless the console is on (`serial_console`) and `console_forward` is on.
pub fn init(config: &DaemonConfig) {
    let s = config.console_enabled().then(|| Settings {
        interval: Duration::from_millis(config.console_forward_interval_ms.max(100)),
        max_lines_per_sec: config.console_forward_max_lines_per_sec.max(1),
        max_line_bytes: config.console_forward_max_line_bytes.max(64),
        max_bytes_per_sec: config.console_forward_max_bytes_per_sec.max(1024),
        roots: vec![PathBuf::from(&config.state_dir)],
    });
    if s.is_some() {
        info!("the guests' consoles are forwarded to the log (console[vm=...])");
    }
    let _ = SETTINGS.set(s);
}

/// Look for consoles nobody reads (after the daemon restarted) every interval, until the daemon stops. A no-op when the feature is off.
pub fn spawn() {
    let Some(s) = settings() else {
        return;
    };
    let interval = s.interval;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            scan().await;
        }
    });
}

/// The name of the VM a state directory belongs to: its name, when that is a name the daemon gives.
fn vm_of(state_dir: &Path) -> Option<String> {
    let name = state_dir.file_name()?.to_str()?;
    crate::validate::is_id(name).then(|| name.to_string())
}

async fn scan() {
    let Some(s) = settings() else {
        return;
    };
    for root in &s.roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let dir = e.path();
            if vm_of(&dir).is_none() || !is_socket(&socket_path(&dir)) {
                continue;
            }
            let known = active().lock().map(|a| a.contains(&dir)).unwrap_or(true);
            if !known {
                connect(s, &dir, false).await;
            }
        }
    }
}

/// Read the console of the VM whose state directory is `state_dir`: call it after the Cloud Hypervisor made the VM's devices (`vm.create`,
/// `vm.restore`) and before the guest runs, so that nothing it says is lost. Failing is logged, not an error: the scan tries again.
pub async fn attach(state_dir: &Path) {
    let Some(s) = settings() else {
        return;
    };
    connect(s, state_dir, true).await;
}

fn is_socket(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// Connect to the console socket of a VM and read it in a task. `wait`: the socket may not be there yet.
async fn connect(s: &'static Settings, state_dir: &Path, wait: bool) {
    let Some(vm) = vm_of(state_dir) else {
        return;
    };
    {
        let Ok(mut a) = active().lock() else {
            return;
        };
        if !a.insert(state_dir.to_path_buf()) {
            return; // read already
        }
    }
    let path = socket_path(state_dir);
    let started = Instant::now();
    let stream = loop {
        // A link is not followed by anything the daemon does here: it is looked at first (what the user of the VM can swap between this and
        // the connect is caught by the check of the other end).
        let attempt = if is_socket(&path) {
            UnixStream::connect(&path).await
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        match attempt {
            Ok(st) => break Some(st),
            Err(e) if wait && started.elapsed() < ATTACH_WAIT => {
                debug!("console of VM {vm}: not yet ({e})");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                if wait {
                    warn!("console of VM {vm}: cannot connect: {e}");
                }
                break None;
            }
        }
    };
    let Some(stream) = stream else {
        forget(state_dir);
        return;
    };
    let want = crate::rootless::uid_of(state_dir).unwrap_or(0);
    if let Err(e) = check_peer(&stream, want) {
        warn!("console of VM {vm}: {e}");
        forget(state_dir);
        return;
    }
    let dir = state_dir.to_path_buf();
    tokio::spawn(async move {
        let emit_vm = vm.clone();
        let mut emit = move |line: &str| info!(target: "console", "console[vm={emit_vm}] {line}");
        if let Err(e) = read_console(stream, s, &mut emit).await {
            debug!("console of VM {vm}: {e}");
        }
        forget(&dir);
    });
}

fn forget(state_dir: &Path) {
    if let Ok(mut a) = active().lock() {
        a.remove(state_dir);
    }
}

/// The process that listens on the socket has the user `want`: the VM's Cloud Hypervisor, not something else the VM's user put in its place.
fn check_peer(stream: &UnixStream, want: u32) -> Result<(), String> {
    match stream.peer_cred() {
        Ok(c) if c.uid() == want => Ok(()),
        Ok(c) => Err(format!(
            "the socket is listened on by user {}, not {want}: not read",
            c.uid()
        )),
        Err(e) => Err(format!("the other end of the socket is unknown: {e}")),
    }
}

/// Read a console to its end. `emit` gets each line to forward.
async fn read_console(
    mut stream: UnixStream,
    s: &Settings,
    emit: &mut (dyn FnMut(&str) + Send),
) -> std::io::Result<()> {
    let mut t = Tracker::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut window = Instant::now();
    let mut window_bytes: u64 = 0;
    let mut told = Instant::now();
    loop {
        let n = stream.read(&mut buf).await?;
        let now = Instant::now();
        if n == 0 {
            // The end: the line that has no end is forwarded too, and the count of what was dropped.
            t.tokens = s.max_lines_per_sec as f64 * BURST_SECONDS;
            if !t.pending.is_empty() {
                let line = std::mem::take(&mut t.pending);
                take_line(&mut t, &line, s.max_line_bytes, emit);
            }
            tell_dropped(&mut t, s, emit);
            return Ok(());
        }
        feed(&mut t, s, &buf[..n], now, emit);
        if t.dropped > 0 && now.saturating_duration_since(told) >= Duration::from_secs(1) {
            tell_dropped(&mut t, s, emit);
            told = now;
        }
        // A guest that writes faster than the daemon is willing to read is not read for the rest of the second (the Cloud Hypervisor drops it).
        if now.saturating_duration_since(window) >= Duration::from_secs(1) {
            window = now;
            window_bytes = 0;
        }
        window_bytes += n as u64;
        if window_bytes > s.max_bytes_per_sec {
            let left = Duration::from_secs(1).saturating_sub(now.saturating_duration_since(window));
            tokio::time::sleep(left).await;
            window = Instant::now();
            window_bytes = 0;
        }
    }
}

fn tell_dropped(t: &mut Tracker, s: &Settings, emit: &mut dyn FnMut(&str)) {
    if t.dropped > 0 {
        emit(&format!(
            "{} lines dropped (more than {} a second)",
            t.dropped, s.max_lines_per_sec
        ));
        t.dropped = 0;
    }
}

/// Take in what was read: the budget grows with the time since the last read (up to the burst), then each whole line is handed to it.
fn feed(t: &mut Tracker, s: &Settings, bytes: &[u8], now: Instant, emit: &mut dyn FnMut(&str)) {
    let cap = s.max_lines_per_sec as f64 * BURST_SECONDS;
    let elapsed = t
        .last_pass
        .map(|p| now.saturating_duration_since(p).as_secs_f64())
        .unwrap_or(BURST_SECONDS);
    t.tokens = (t.tokens + elapsed * s.max_lines_per_sec as f64).min(cap);
    t.last_pass = Some(now);
    lines_of(t, bytes, s.max_line_bytes, emit);
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
    if t.tokens < 1.0 {
        // Over the budget: not even made printable.
        t.dropped += 1;
        return;
    }
    let text = printable(line, max);
    if text.is_empty() {
        return;
    }
    t.tokens -= 1.0;
    emit(&text);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdListener;

    fn settings() -> Settings {
        Settings {
            interval: Duration::from_secs(1),
            max_lines_per_sec: 100,
            max_line_bytes: 200,
            max_bytes_per_sec: 1 << 20,
            roots: vec![],
        }
    }

    fn feed_all(s: &Settings, t: &mut Tracker, bytes: &[u8], now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        feed(t, s, bytes, now, &mut |l| out.push(l.to_string()));
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
        let s = settings();
        let mut t = Tracker::default();
        let now = Instant::now();
        assert_eq!(
            feed_all(&s, &mut t, b"first\nsecond\nthi", now),
            ["first", "second"]
        );
        assert_eq!(feed_all(&s, &mut t, b"rd\nfourth", now), ["third"]);
        assert!(feed_all(&s, &mut t, b"", now).is_empty(), "nothing new");
        assert_eq!(t.pending, b"fourth");
    }

    #[test]
    fn a_line_that_never_ends_is_cut_and_does_not_grow_without_end() {
        let s = settings(); // lines of 200 bytes at most: 800 are held
        let mut t = Tracker::default();
        let out = feed_all(&s, &mut t, &vec![b'x'; 5000], Instant::now());
        assert_eq!(out.len(), 1);
        assert!(t.pending.len() <= 800);
    }

    #[test]
    fn a_guest_that_writes_without_end_gets_a_budget_and_a_count_of_what_was_dropped() {
        let s = settings(); // 100 lines a second, a burst of 500
        let mut t = Tracker::default();
        let now = Instant::now();
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let out = feed_all(&s, &mut t, text.as_bytes(), now);
        assert_eq!(out.len(), 500, "the burst");
        assert_eq!(t.dropped, 1500);
        let mut told = Vec::new();
        tell_dropped(&mut t, &s, &mut |l| told.push(l.to_string()));
        assert!(told[0].contains("1500 lines dropped"), "{told:?}");
        assert_eq!(t.dropped, 0);
        // A second later the budget has grown by 100 lines.
        let out = feed_all(
            &s,
            &mut t,
            "more\n".repeat(1000).as_bytes(),
            now + Duration::from_secs(1),
        );
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn the_vm_is_named_by_its_directory_and_only_when_the_daemon_could_have_named_it() {
        assert_eq!(
            vm_of(Path::new("/run/cloudhv/daemon/sized-12-3")).as_deref(),
            Some("sized-12-3")
        );
        assert_eq!(
            vm_of(Path::new("/run/cloudhv/daemon/pool-9-1")).as_deref(),
            Some("pool-9-1")
        );
        assert_eq!(vm_of(Path::new("/run/cloudhv/daemon/not a vm")), None);
        assert_eq!(vm_of(Path::new("/run/cloudhv/daemon/..")), None);
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cloudhv-console-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn a_console_is_read_to_its_end_with_the_last_line_that_has_no_end() {
        let d = dir("read");
        let path = d.join("console.sock");
        let l = StdListener::bind(&path).unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"kernel: booting\nagent: started\n\x1b[31mred\x1b[0m\nno end")
                .unwrap();
        });
        let st = UnixStream::connect(&path).await.unwrap();
        assert!(check_peer(&st, unsafe { libc::geteuid() }).is_ok());
        assert!(
            check_peer(&st, unsafe { libc::geteuid() } + 1).is_err(),
            "another user"
        );
        let mut out = Vec::new();
        read_console(st, &settings(), &mut |l| out.push(l.to_string()))
            .await
            .unwrap();
        writer.join().unwrap();
        assert_eq!(out, ["kernel: booting", "agent: started", "red", "no end"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_guest_that_writes_faster_than_the_daemon_reads_is_not_read_for_the_rest_of_the_second(
    ) {
        let d = dir("rate");
        let path = d.join("console.sock");
        let l = StdListener::bind(&path).unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let (mut c, _) = l.accept().unwrap();
            // 3 chunks of 40 KiB with a limit of 50 KiB a second: the third has to wait for the next second.
            for _ in 0..3 {
                c.write_all(&vec![b'a'; 40 * 1024]).unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let mut s = settings();
        s.max_bytes_per_sec = 50 * 1024;
        let st = UnixStream::connect(&path).await.unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        read_console(st, &s, &mut |l| out.push(l.to_string()))
            .await
            .unwrap();
        writer.join().unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(800),
            "{:?}",
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_link_is_not_a_socket() {
        let d = dir("link");
        let sock = d.join("real.sock");
        let _l = StdListener::bind(&sock).unwrap();
        let link = d.join("console.sock");
        std::os::unix::fs::symlink(&sock, &link).unwrap();
        assert!(is_socket(&sock));
        assert!(!is_socket(&link), "a link is looked at, not followed");
        assert!(!is_socket(&d.join("none")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
