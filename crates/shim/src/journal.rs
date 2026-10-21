//! The shim's log, written to journald.
//!
//! containerd's shim library sends the log to a FIFO that containerd reads and puts into its own log. On a k3s node that log is not kept
//! anywhere that is collected, so what the shim did (for instance whether `delete` was called, and where it stopped) was lost with the
//! process. The daemon already logs to journald; this puts the shim's lines in the same journal, where they are read next to the daemon's
//! and collected the same way.
//!
//! It speaks journald's native protocol (a datagram to `/run/systemd/journal/socket`), so there is no dependency to add. Fields:
//! `SYSLOG_IDENTIFIER=cloudhv-shim`, `PRIORITY`, `CODE_MODULE`, `CLOUDHV_SHIM_ID` (the `-id` flag containerd gives the shim: the
//! sandbox's id) and `MESSAGE`. If the socket cannot be used, the line goes to standard error: the log never stops the shim.

use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

const SOCKET: &str = "/run/systemd/journal/socket";
/// A message is cut here (a datagram has a limit, and a shim line has no use for more).
const MAX_MESSAGE: usize = 8 * 1024;

struct JournalLogger {
    socket: Mutex<Option<UnixDatagram>>,
    shim_id: String,
    level: LevelFilter,
}

/// Install the logger. `shim_id` is `-id` of the command line (empty if there is none). The level is `info`, or `debug` with `-debug`.
pub fn init(shim_id: &str, debug: bool) {
    let level = if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let socket = UnixDatagram::unbound().ok().filter(|s| s.connect(SOCKET).is_ok());
    let logger = JournalLogger {
        socket: Mutex::new(socket),
        shim_id: shim_id.chars().filter(|c| !c.is_control()).take(128).collect(),
        level,
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}

/// The value of a flag (`-id x` or `--id x`) in the arguments, or "".
pub fn flag_value(args: &[String], name: &str) -> String {
    let (short, long) = (format!("-{name}"), format!("--{name}"));
    args.iter()
        .position(|a| *a == short || *a == long)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default()
}

fn priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

/// One field of the native protocol. A value with a newline is written as `NAME\n<length, 8 bytes little endian><value>\n`, which
/// can hold anything; a plain one as `NAME=value\n`.
fn push_field(buf: &mut Vec<u8>, name: &str, value: &[u8]) {
    if value.contains(&b'\n') {
        buf.extend_from_slice(name.as_bytes());
        buf.push(b'\n');
        buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
        buf.extend_from_slice(value);
    } else {
        buf.extend_from_slice(name.as_bytes());
        buf.push(b'=');
        buf.extend_from_slice(value);
    }
    buf.push(b'\n');
}

fn truncate(msg: &str) -> &str {
    if msg.len() <= MAX_MESSAGE {
        return msg;
    }
    let mut end = MAX_MESSAGE;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    &msg[..end]
}

fn encode(shim_id: &str, record: &Record) -> Vec<u8> {
    let message = record.args().to_string();
    let mut buf = Vec::with_capacity(message.len().min(MAX_MESSAGE) + 160);
    push_field(&mut buf, "SYSLOG_IDENTIFIER", b"cloudhv-shim");
    push_field(&mut buf, "PRIORITY", priority(record.level()).to_string().as_bytes());
    push_field(&mut buf, "CODE_MODULE", record.target().as_bytes());
    if !shim_id.is_empty() {
        push_field(&mut buf, "CLOUDHV_SHIM_ID", shim_id.as_bytes());
    }
    push_field(&mut buf, "MESSAGE", truncate(&message).as_bytes());
    buf
}

impl Log for JournalLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let buf = encode(&self.shim_id, record);
        let sent = match self.socket.lock() {
            Ok(g) => g.as_ref().is_some_and(|s| s.send(&buf).is_ok()),
            Err(_) => false,
        };
        if !sent {
            let _ = writeln!(
                std::io::stderr(),
                "cloudhv-shim[{}] {}: {}",
                self.shim_id,
                record.level(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(msg: &str) -> Vec<u8> {
        encode(
            "sb1",
            &Record::builder()
                .args(format_args!("{msg}"))
                .level(Level::Warn)
                .target("cloudhv_shim::instance")
                .build(),
        )
    }

    #[test]
    fn a_plain_line_is_name_equals_value() {
        let b = String::from_utf8(rec("hello")).unwrap();
        assert!(b.contains("SYSLOG_IDENTIFIER=cloudhv-shim\n"));
        assert!(b.contains("PRIORITY=4\n"));
        assert!(b.contains("CLOUDHV_SHIM_ID=sb1\n"));
        assert!(b.ends_with("MESSAGE=hello\n"));
    }

    #[test]
    fn a_line_with_a_newline_is_length_prefixed() {
        let b = rec("a\nb");
        let tail = b"MESSAGE\n\x03\0\0\0\0\0\0\0a\nb\n";
        assert!(b.ends_with(tail));
    }

    #[test]
    fn a_long_message_is_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_MESSAGE);
        let b = rec(&long);
        assert!(b.len() < MAX_MESSAGE + 300);
        assert!(std::str::from_utf8(&b).is_ok());
    }

    #[test]
    fn the_flag_is_found_in_either_form() {
        let a: Vec<String> = ["x", "-namespace", "k8s.io", "-id", "abc"].map(String::from).to_vec();
        assert_eq!(flag_value(&a, "id"), "abc");
        assert_eq!(flag_value(&a, "namespace"), "k8s.io");
        assert_eq!(flag_value(&a, "missing"), "");
    }
}
